//! Durable, bounded, byte-preserving capture for non-interactive commands.
//! One detached request worker owns the process and drains both nonblocking
//! pipes. Receipts and delivery cursors commit in the same SQLite transaction.
use crate::{
    engine::Engine,
    journal::{Journal, Request, TERMINAL},
    util::*,
};
use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::{fd::AsRawFd, unix::process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const STREAM_CAP: u64 = 1024 * 1024;
const STORE_CAP: u64 = 4 * 1024 * 1024 * 1024;
const RETENTION: f64 = 7.0 * 86400.0;
const MAX_RUNNING: usize = 512;
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Serialize, Deserialize)]
struct Job {
    session_id: String,
    started_request_id: String,
    cmd: String,
    workdir: PathBuf,
    worker_pid: u32,
    worker_fingerprint: String,
    started_at: f64,
    finished: bool,
    exit_code: Option<i32>,
    signal: Option<i32>,
    duration_ms: u64,
    total: [u64; 2],
    stored: [u64; 2],
    capture_error: Option<String>,
    cancelled: bool,
    timed_out: bool,
    logs_expired: bool,
}

impl Job {
    fn progress_changed(&self, saved: &Self) -> bool {
        self.total != saved.total
            || self.stored != saved.stored
            || self.capture_error != saved.capture_error
            || self.finished != saved.finished
            || self.exit_code != saved.exit_code
            || self.signal != saved.signal
            || self.cancelled != saved.cancelled
            || self.timed_out != saved.timed_out
    }
}

pub fn schema(j: &Journal) -> Result<()> {
    j.db.execute_batch(
        "CREATE TABLE IF NOT EXISTS pipe_sessions (
        session_id TEXT PRIMARY KEY, data TEXT NOT NULL,
        cancel INTEGER NOT NULL DEFAULT 0,
        stdout_offset INTEGER NOT NULL DEFAULT 0, stderr_offset INTEGER NOT NULL DEFAULT 0);
        CREATE INDEX IF NOT EXISTS requests_generation ON requests(generation,updated);",
    )?;
    Ok(())
}

pub fn handles(tool: &str, args: &Value) -> bool {
    (tool == "exec_command" && args["tty"] != true)
        || args["session_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("pipe-"))
}

fn path(state: &Path, sid: &str, stream: usize) -> PathBuf {
    state
        .join("outputs")
        .join(format!("{sid}.{}", ["stdout", "stderr"][stream]))
}

fn get(j: &Journal, sid: &str) -> Result<Job> {
    let raw: String =
        j.db.query_row(
            "SELECT data FROM pipe_sessions WHERE session_id=?",
            [sid],
            |r| r.get(0),
        )
        .optional()?
        .context("Unknown native session_id")?;
    Ok(serde_json::from_str(&raw)?)
}

fn save(j: &Journal, job: &Job) -> Result<()> {
    j.db.execute(
        "UPDATE pipe_sessions SET data=? WHERE session_id=?",
        params![serde_json::to_string(job)?, job.session_id],
    )?;
    Ok(())
}

fn alive(job: &Job) -> bool {
    fingerprint(job.worker_pid).as_deref() == Some(&job.worker_fingerprint)
}

fn jobs(j: &Journal) -> Result<Vec<Job>> {
    schema(j)?;
    let mut stmt = j.db.prepare("SELECT data FROM pipe_sessions")?;
    let values = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    values
        .iter()
        .map(|v| Ok(serde_json::from_str(v)?))
        .collect()
}

/// Reserve two full stream caps per retained job, including active jobs. Only
/// completed/dead jobs may be evicted. This bounds disk without a global lock
/// on every captured chunk; retention is enforced on the next admission.
fn admit(state: &Path, j: &Journal) -> Result<()> {
    let mut all = jobs(j)?;
    ensure!(
        all.iter().filter(|job| !job.finished && alive(job)).count() < MAX_RUNNING,
        "Native running command quota reached (512); poll or cancel an existing session"
    );
    all.sort_by(|a, b| a.started_at.total_cmp(&b.started_at));
    let mut reserved = all.iter().filter(|s| !s.logs_expired).count() as u64 * STREAM_CAP * 2;
    for mut job in all {
        if !job.logs_expired
            && (job.finished || !alive(&job))
            && (now() - job.started_at > RETENTION || reserved + STREAM_CAP * 2 > STORE_CAP)
        {
            for stream in 0..2 {
                match fs::remove_file(path(state, &job.session_id, stream)) {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
            }
            job.logs_expired = true;
            save(j, &job)?;
            reserved -= STREAM_CAP * 2;
        }
    }
    ensure!(
        reserved + STREAM_CAP * 2 <= STORE_CAP,
        "Native output store is full"
    );
    Ok(())
}

/// Kill only this worker's own process group on errors; never orphan a running
/// command if capture or durable bookkeeping fails after spawn.
struct Process(Child, bool);
impl Process {
    fn group_running(&self) -> bool {
        if unsafe { libc::kill(-(self.0.id() as i32), 0) == -1 }
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return false;
        }
        // kill(0) also succeeds for zombie-only groups. Linux can retain
        // orphaned zombies indefinitely when a container's PID 1 does not reap.
        // Inspection failures remain conservative, just as EPERM does above.
        #[cfg(target_os = "linux")]
        {
            group_has_running_members(Path::new("/proc"), self.0.id()).unwrap_or(true)
        }
        #[cfg(not(target_os = "linux"))]
        true
    }
}

#[cfg(target_os = "linux")]
fn group_has_running_members(proc: &Path, pgid: u32) -> Result<bool> {
    let pgid = pgid.to_string();
    for entry in fs::read_dir(proc)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
        match group_member_running(&entry.path().join("stat"), &pgid)? {
            Some(true) => return Ok(true),
            Some(false) => {
                // A zombie leader can still have executable threads. /proc
                // enumerates process leaders, so inspect its remaining tasks.
                let tasks = match fs::read_dir(entry.path().join("task")) {
                    Ok(tasks) => tasks,
                    Err(e) if process_disappeared(&e) => continue,
                    Err(e) => return Err(e.into()),
                };
                for task in tasks {
                    if group_member_running(&task?.path().join("stat"), &pgid)? == Some(true) {
                        return Ok(true);
                    }
                }
            }
            None => (),
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn process_disappeared(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
}

#[cfg(target_os = "linux")]
fn group_member_running(stat: &Path, pgid: &str) -> Result<Option<bool>> {
    let raw = match fs::read(stat) {
        Ok(raw) => raw,
        // Processes and threads may disappear during inspection.
        Err(e) if process_disappeared(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    // comm may contain whitespace, ')' and non-UTF-8 bytes. Only the
    // fields after its final ')' describe the state, parent and group.
    let raw = String::from_utf8_lossy(&raw);
    let mut fields = raw
        .rsplit_once(')')
        .context("Invalid process stat")?
        .1
        .split_whitespace();
    let state = fields.next().context("Missing process state")?;
    let group = fields.nth(1).context("Missing process group")?;
    Ok((group == pgid).then_some(!matches!(state, "Z" | "X")))
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.1 {
            return;
        }
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

fn nonblocking(fd: i32) -> Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        ensure!(
            flags != -1 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) != -1,
            "Cannot configure capture pipe: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Consume at most one chunk per turn per stream so neither a busy writer nor
/// a full pipe can starve the other stream, cancellation or timeout checks.
fn drain(reader: &mut impl Read, file: &mut File, job: &mut Job, stream: usize) -> bool {
    let mut buf = [0u8; 32768];
    match reader.read(&mut buf) {
        Ok(0) => false,
        Ok(n) => {
            job.total[stream] = job.total[stream].saturating_add(n as u64);
            let keep = (STREAM_CAP - job.stored[stream]).min(n as u64) as usize;
            if keep > 0 && job.capture_error.is_none() {
                match file.write_all(&buf[..keep]) {
                    Ok(()) => job.stored[stream] += keep as u64,
                    Err(e) => job.capture_error = Some(format!("Output store write failed: {e}")),
                }
            }
            true
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            true
        }
        Err(e) => {
            job.capture_error = Some(format!("Output pipe read failed: {e}"));
            false
        }
    }
}

pub fn run(
    state: &Path,
    j: &Journal,
    row: &Request,
    args: &Value,
    dispatched: &mut bool,
) -> Result<()> {
    schema(j)?;
    if let Some(sid) = args["session_id"].as_str() {
        return deliver(state, j, &row.id, sid, args, dispatched);
    }
    crate::mcp::validate("exec_command", args)?;
    let settings = Engine::new(state)?.settings()?;
    let workdir = args["workdir"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or(settings.workspace);
    ensure!(
        workdir.is_absolute() && workdir.is_dir(),
        "workdir must be an existing absolute directory"
    );
    let cmd = args["cmd"].as_str().context("cmd is required")?;
    let sid = format!(
        "pipe-{}",
        uuid::Uuid::new_v4()
            .as_bytes()
            .iter()
            .flat_map(|b| [char::from(b'a' + (b >> 4)), char::from(b'a' + (b & 15))])
            .collect::<String>()
    );
    let mut job = Job {
        session_id: sid.clone(),
        started_request_id: row.id.clone(),
        cmd: cmd.chars().take(200).collect(),
        workdir,
        worker_pid: std::process::id(),
        worker_fingerprint: fingerprint(std::process::id())
            .context("Cannot identify native worker")?,
        started_at: now(),
        finished: false,
        exit_code: None,
        signal: None,
        duration_ms: 0,
        total: [0; 2],
        stored: [0; 2],
        capture_error: None,
        cancelled: false,
        timed_out: false,
        logs_expired: false,
    };
    let admission = lock(&state.join("pipe-admission.lock"))?;
    admit(state, j)?;
    private_dir(&state.join("outputs"))?;
    let mut files = [
        append_log(&path(state, &sid, 0))?,
        append_log(&path(state, &sid, 1))?,
    ];
    let mut command = Command::new("/bin/bash");
    command
        .args(["-c", cmd])
        .current_dir(&job.workdir)
        .env_clear()
        .envs(&settings.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    job.started_at = now();
    *dispatched = true;
    let mut child = Process(
        detached(&mut command)
            .spawn()
            .context("Spawn native command")?,
        true,
    );
    let start = Instant::now();
    let mut stdout = child.0.stdout.take().context("Missing stdout pipe")?;
    let mut stderr = child.0.stderr.take().context("Missing stderr pipe")?;
    nonblocking(stdout.as_raw_fd())?;
    nonblocking(stderr.as_raw_fd())?;
    j.db.execute(
        "INSERT INTO pipe_sessions(session_id,data) VALUES(?,?)",
        params![sid, serde_json::to_string(&job)?],
    )?;
    j.generation(&row.id, &sid)?;
    drop(admission);
    let yield_ms = args["yield_time_ms"].as_u64().unwrap_or(1000);
    let timeout_ms = args["timeout_ms"].as_u64().filter(|v| *v > 0);
    let mut open = [true; 2];
    let mut status = None;
    let mut interrupted = None;
    let mut kill_sent = false;
    let mut receipt_saved = false;
    let mut saved = job.clone();
    let mut last_save = Instant::now();
    loop {
        if open[0] {
            open[0] = drain(&mut stdout, &mut files[0], &mut job, 0);
        }
        if open[1] {
            open[1] = drain(&mut stderr, &mut files[1], &mut job, 1);
        }
        if status.is_none() {
            status = child.0.try_wait()?;
        }
        job.duration_ms = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let captured = status.is_some() && !open[0] && !open[1];
        let cancel: bool = j.db.query_row(
            "SELECT cancel FROM pipe_sessions WHERE session_id=?",
            [&sid],
            |r| r.get(0),
        )?;
        // EOF and shell exit do not cover descendants that redirected their
        // output. An explicit deadline keeps their process group supervised.
        let group_survives = (timeout_ms.is_some() || interrupted.is_some())
            && !kill_sent
            && (status.is_none() || child.group_running());
        if (!captured || group_survives)
            && interrupted.is_none()
            && (cancel || timeout_ms.is_some_and(|ms| job.duration_ms >= ms))
        {
            job.cancelled = cancel;
            job.timed_out = !cancel;
            unsafe {
                libc::kill(
                    -(child.0.id() as i32),
                    if cancel { libc::SIGINT } else { libc::SIGTERM },
                );
            }
            interrupted = Some(Instant::now());
        }
        // A shell can exit and close both pipes while a redirected descendant
        // still ignores TERM/INT. Keep supervising that group through KILL.
        if group_survives
            && interrupted.is_some_and(|at| at.elapsed() >= Duration::from_millis(250))
        {
            unsafe {
                libc::kill(-(child.0.id() as i32), libc::SIGKILL);
            }
            kill_sent = true;
        }
        // KILL may leave adopted zombies until their reaper runs. They cannot
        // execute, so do not wait for kill(0) to stop reporting those members.
        job.finished = captured && (!group_survives || kill_sent);
        if job.finished {
            let status = status.unwrap();
            job.exit_code = status.code();
            job.signal = status.signal();
        }
        let receipt_due = !receipt_saved && (job.finished || job.duration_ms >= yield_ms);
        if job.progress_changed(&saved) || last_save.elapsed() >= PROGRESS_INTERVAL || receipt_due {
            save(j, &job)?;
            saved = job.clone();
            last_save = Instant::now();
        }
        if receipt_due {
            deliver(state, j, &row.id, &sid, args, dispatched)?;
            receipt_saved = true;
        }
        if job.finished {
            break;
        }
        let mut fds = [
            libc::pollfd {
                fd: if open[0] { stdout.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if open[1] { stderr.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        unsafe {
            libc::poll(fds.as_mut_ptr(), 2, 20);
        }
    }
    // The group may already have disappeared: avoid signalling a recycled ID
    // from Process::drop once the process and all capture pipes have finished.
    child.1 = false;
    Ok(())
}

fn snapshot(state: &Path, job: &Job, offsets: [u64; 2], tokens: u64) -> Result<Value> {
    let lost = !job.finished && !alive(job);
    let mut body = json!({"stream_mode":"pipes","stdout":"","stderr":"",
        "exit_code":job.exit_code,"signal":job.signal,"duration_ms":job.duration_ms,
        "finished":job.finished,"cancelled":job.cancelled,"timed_out":job.timed_out,
        "workdir":job.workdir,"truncated":false,
        "capture_status":if lost {"supervisor_lost"} else if job.logs_expired {"expired"} else if job.capture_error.is_some() {"error"} else if job.total != job.stored {"truncated"} else if !job.finished {"capturing"} else {"complete"}});
    if !job.finished {
        body["session_id"] = json!(job.session_id);
    }
    if lost {
        body["capture_error"] = json!(
            "Native supervisor disappeared; process outcome is unknown. Inspect side effects before replacement work."
        );
    }
    if let Some(e) = &job.capture_error {
        body["capture_error"] = json!(e);
    }
    let budget = tokens.clamp(1, 32768) * 4 / 2;
    for (i, name) in ["stdout", "stderr"].iter().enumerate() {
        let offset = offsets[i].min(job.stored[i]);
        let mut data = Vec::new();
        if !job.logs_expired {
            let mut file = File::open(path(state, &job.session_id, i))?;
            file.seek(SeekFrom::Start(offset))?;
            file.take(budget.min(job.stored[i] - offset))
                .read_to_end(&mut data)?;
        }
        let next = offset + data.len() as u64;
        let capture_truncated = job.total[i] > job.stored[i];
        let truncated = next < job.stored[i] || capture_truncated || job.logs_expired;
        body[*name] = json!(String::from_utf8_lossy(&data));
        if std::str::from_utf8(&data).is_err() {
            body[format!("{name}_base64")] =
                json!(base64::engine::general_purpose::STANDARD.encode(&data));
            body[format!("{name}_encoding_loss")] = json!(true);
        }
        body[format!("{name}_truncated")] = json!(truncated);
        body[format!("{name}_capture_truncated")] = json!(capture_truncated);
        body[format!("{name}_ref")] = json!({"path":path(state, &job.session_id, i),"offset":next,
            "end_offset":job.stored[i],"total_bytes":job.total[i],"available":!job.logs_expired});
        if truncated {
            body["truncated"] = json!(true);
        }
    }
    Ok(body)
}

fn deliver(
    state: &Path,
    j: &Journal,
    id: &str,
    sid: &str,
    args: &Value,
    dispatched: &mut bool,
) -> Result<()> {
    let mut job = get(j, sid)?;
    let origin = &job.started_request_id;
    // A lost worker may have saved capture progress without its first receipt.
    // Reconcile that request, but keep its terminal outcome and delivery cursors
    // intact. A live supervisor must retain priority for the initial delivery.
    let origin_ready = if origin == id {
        true
    } else {
        let origin = j.reconcile(j.get(origin)?)?;
        origin.status == "done" || (TERMINAL.contains(&origin.status.as_str()) && !alive(&job))
    };
    ensure!(
        origin_ready,
        "Initial command response is not saved yet; call get_result with its request_id before polling this session"
    );
    let chars = args["chars"].as_str().unwrap_or("");
    ensure!(
        chars.is_empty() || chars == "\u{3}",
        "Native pipes have closed stdin; use tty=true for interactive input. Poll with empty chars or cancel with Ctrl-C."
    );
    // Cancellation must reach the supervisor even while another caller holds
    // the delivery lock in a long wait. Output consumption stays serialized.
    if chars == "\u{3}" && !job.finished && alive(&job) {
        *dispatched = true;
        j.db.execute(
            "UPDATE pipe_sessions SET cancel=1 WHERE session_id=?",
            [sid],
        )?;
    }
    let dir = state.join("session-locks");
    private_dir(&dir)?;
    let _guard = lock(&dir.join(format!("{}.lock", sha256(sid.as_bytes()))))?;
    job = get(j, sid)?;
    let wait = if chars == "\u{3}" {
        1
    } else {
        args["wait_seconds"].as_u64().unwrap_or(0).min(35)
    };
    let deadline = Instant::now() + Duration::from_secs(wait);
    while !job.finished && alive(&job) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        job = get(j, sid)?;
    }
    // Prevent log eviction between snapshot and durable receipt commit.
    let _admission = lock(&state.join("pipe-admission.lock"))?;
    job = get(j, sid)?;
    let offsets = j.db.query_row(
        "SELECT stdout_offset,stderr_offset FROM pipe_sessions WHERE session_id=?",
        [sid],
        |r| Ok([r.get(0)?, r.get(1)?]),
    )?;
    let mut body = snapshot(
        state,
        &job,
        offsets,
        args["max_output_tokens"].as_u64().unwrap_or(4096),
    )?;
    if let Some(cmd) = args["cmd"].as_str() {
        let notes = crate::mcp::advisories(cmd);
        if !notes.is_empty() {
            body["warnings"] = json!(notes);
        }
    }
    let value = json!({"structuredContent":body,"isError":false});
    let tx = j.db.unchecked_transaction()?;
    tx.execute(
        "UPDATE pipe_sessions SET stdout_offset=?,stderr_offset=? WHERE session_id=?",
        params![job.stored[0], job.stored[1], sid],
    )?;
    tx.execute(
        "UPDATE requests SET status='done',result=?,generation=?,updated=? WHERE id=?",
        params![value.to_string(), sid, now(), id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn sessions(j: &Journal) -> Result<Vec<Value>> {
    jobs(j)?.into_iter().map(|job| {
        let status = if job.finished { "exited" } else if alive(&job) { "running_at_last_observation" } else { "unknown" };
        let last: Option<String> = j.db.query_row("SELECT id FROM requests WHERE generation=? AND status='done' ORDER BY updated DESC,id DESC LIMIT 1", [&job.session_id], |r| r.get(0)).optional()?;
        Ok(json!({"session_id":job.session_id,"stream_mode":"pipes","cmd":job.cmd,"workdir":job.workdir,
            "started_request_id":job.started_request_id,"last_seen_at":job.started_at + job.duration_ms as f64 / 1000.0,
            "last_request_id":last,
            "status":status,"exit_code":job.exit_code,"signal":job.signal,"duration_ms":job.duration_ms}))
    }).collect()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::engine::Settings;
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        write_json(
            &dir.path().join("settings.json"),
            &Settings {
                version: 1,
                engine: "/no-engine-needed".into(),
                engine_sha256: String::new(),
                workspace: dir.path().to_owned(),
                environment: BTreeMap::from([
                    ("PATH".into(), "/usr/bin:/bin".into()),
                    ("LANG".into(), "C.UTF-8".into()),
                ]),
            },
        )
        .unwrap();
        dir
    }

    fn execute(dir: &Path, args: Value) -> (Value, Journal) {
        let j = Journal::new(dir).unwrap();
        let row = j.prepare("test", "exec_command", &args).unwrap();
        j.running("test").unwrap();
        run(dir, &j, &row, &args, &mut false).unwrap();
        let receipt = j.get("test").unwrap().receipt().unwrap();
        (crate::runtime::result_body(&receipt["result"]), j)
    }

    #[test]
    fn separate_streams_exit_and_workdir() {
        let dir = fixture();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let (body, _) = execute(
            dir.path(),
            json!({"cmd":"printf out; printf err >&2; pwd >&2; exit 7", "workdir":sub,"yield_time_ms":10000}),
        );
        assert_eq!(body["stdout"], "out");
        assert_eq!(body["stderr"], format!("err{}\n", sub.display()));
        assert_eq!(body["exit_code"], 7);
        assert_eq!(body["signal"], Value::Null);
        assert_eq!(body["finished"], true);
        assert_eq!(body["truncated"], false);
        assert!(body["duration_ms"].is_u64());
    }

    #[test]
    fn large_both_streams_drain_without_deadlock_and_references_recover_bytes() {
        let dir = fixture();
        let (body, _) = execute(
            dir.path(),
            json!({"cmd":"head -c 2097152 /dev/zero & head -c 2097152 /dev/zero >&2 & wait", "yield_time_ms":10000,"max_output_tokens":10}),
        );
        assert_eq!(body["exit_code"], 0);
        assert_eq!(body["truncated"], true);
        for name in ["stdout", "stderr"] {
            assert_eq!(body[name].as_str().unwrap().len(), 20);
            assert_eq!(body[format!("{name}_capture_truncated")], true);
            let r = &body[format!("{name}_ref")];
            assert_eq!(r["offset"], 20);
            assert_eq!(r["total_bytes"], 2097152);
            let bytes = fs::read(r["path"].as_str().unwrap()).unwrap();
            assert_eq!(bytes, vec![0; STREAM_CAP as usize]);
        }
    }

    #[test]
    fn binary_bytes_are_preserved_with_base64_and_raw_refs() {
        let dir = fixture();
        let (body, _) = execute(
            dir.path(),
            json!({"cmd":"printf '\\000\\377a'; printf '\\376b' >&2", "yield_time_ms":10000}),
        );
        assert_eq!(body["stdout_base64"], "AP9h");
        assert_eq!(body["stderr_base64"], "/mI=");
        assert_eq!(
            fs::read(body["stdout_ref"]["path"].as_str().unwrap()).unwrap(),
            b"\x00\xffa"
        );
    }

    #[test]
    fn signals_are_not_shell_exit_codes() {
        let dir = fixture();
        let (body, _) = execute(
            dir.path(),
            json!({"cmd":"kill -TERM $$", "yield_time_ms":10000}),
        );
        assert_eq!(body["exit_code"], Value::Null);
        assert_eq!(body["signal"], libc::SIGTERM);
        assert_eq!(body["finished"], true);
    }

    #[test]
    fn timeout_kills_group_even_when_term_ignored() {
        let dir = fixture();
        let before = Instant::now();
        let (body, _) = execute(
            dir.path(),
            json!({"cmd":"trap '' TERM; sleep 60 & wait", "timeout_ms":80,"yield_time_ms":10000}),
        );
        assert!(before.elapsed() < Duration::from_secs(5));
        assert_eq!(body["timed_out"], true);
        assert_eq!(body["signal"], libc::SIGKILL);
        assert_eq!(body["exit_code"], Value::Null);
    }

    #[test]
    fn timeout_supervises_redirected_background_children_after_capture_finishes() {
        for (ignore_term, yield_ms) in [(false, 10000), (true, 10000), (true, 10)] {
            let dir = fixture();
            let cmd = if ignore_term {
                "bash -c 'trap \"\" TERM; echo $$ > child.pid; while :; do sleep 1; done' >/dev/null 2>&1 & while [ ! -s child.pid ]; do sleep .01; done"
            } else {
                "sleep 60 >/dev/null 2>&1 & echo $! > child.pid"
            };
            let before = Instant::now();
            let (first, j) = execute(
                dir.path(),
                json!({"cmd":cmd,"timeout_ms":150,"yield_time_ms":yield_ms}),
            );
            let pid: u32 = fs::read_to_string(dir.path().join("child.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            // Only this test's child is eligible for cleanup on assertion failure.
            struct Cleanup(u32);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    unsafe {
                        libc::kill(self.0 as i32, libc::SIGKILL);
                    }
                }
            }
            let _cleanup = Cleanup(pid);
            let job = jobs(&j).unwrap().remove(0);
            let body = snapshot(dir.path(), &job, [0, 0], 100).unwrap();
            if yield_ms == 10 {
                assert_eq!(first["finished"], false);
                assert_eq!(first["timed_out"], false);
                assert_eq!(first["session_id"], job.session_id);
            }
            assert!(before.elapsed() < Duration::from_secs(5));
            assert_eq!(body["timed_out"], true);
            assert_eq!(body["cancelled"], false);
            assert_eq!(body["finished"], true);
            // The shell exited successfully before its child's deadline.
            assert_eq!(body["exit_code"], 0);
            assert_eq!(body["signal"], Value::Null);
            assert!(body["duration_ms"].as_u64().unwrap() >= 150);
            let deadline = Instant::now() + Duration::from_secs(1);
            while fingerprint(pid).is_some() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                fingerprint(pid).is_none(),
                "redirected child survived deadline"
            );
            assert!(job.finished);
        }
    }

    #[test]
    fn deadline_does_not_delay_a_command_whose_process_group_has_exited() {
        for cmd in ["printf done", "sleep .05 >/dev/null 2>&1 & printf done"] {
            let dir = fixture();
            let before = Instant::now();
            let (body, _) = execute(
                dir.path(),
                json!({"cmd":cmd, "timeout_ms":1000,"yield_time_ms":10000}),
            );
            assert!(before.elapsed() < Duration::from_secs(2));
            assert_eq!(body["finished"], true);
            assert_eq!(body["timed_out"], false);
            assert_eq!(body["stdout"], "done");
        }
    }

    #[test]
    fn zombie_leaders_with_live_threads_keep_the_group_supervised() {
        let proc = tempfile::tempdir().unwrap();
        let leader = proc.path().join("101");
        let thread = leader.join("task/102");
        fs::create_dir_all(&thread).unwrap();
        // Include a comm that cannot be parsed as whitespace-delimited UTF-8.
        fs::write(leader.join("stat"), b"101 (worker ) \xff) Z 1 101 0").unwrap();
        for state in ["R", "S", "D", "T", "Z", "X"] {
            fs::write(thread.join("stat"), format!("102 (thread) {state} 1 101 0")).unwrap();
            assert_eq!(
                group_has_running_members(proc.path(), 101).unwrap(),
                !matches!(state, "Z" | "X"),
                "thread state {state}"
            );
            assert!(!group_has_running_members(proc.path(), 999).unwrap());
        }
        // Exits during enumeration are harmless, but unreadable or malformed
        // state must reach the caller's conservative liveness fallback.
        fs::remove_file(thread.join("stat")).unwrap();
        assert!(!group_has_running_members(proc.path(), 101).unwrap());
        fs::write(thread.join("stat"), "invalid").unwrap();
        assert!(group_has_running_members(proc.path(), 101).is_err());
        assert!(group_has_running_members(&proc.path().join("missing"), 101).is_err());
    }

    #[test]
    fn deadline_ignores_zombie_only_process_groups() {
        const ISOLATED: &str = "LKJYOLO_TEST_ZOMBIE_SUBREAPER";
        if std::env::var_os(ISOLATED).is_none() {
            // Isolate adoption from parallel tests, and retain exited orphans
            // deterministically even on hosts whose PID 1 reaps promptly.
            let mut test = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pipes::tests::deadline_ignores_zombie_only_process_groups",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(ISOLATED, "1")
                .spawn()
                .unwrap();
            assert!(wait_child(&mut test, 15).unwrap().success());
            return;
        }
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);

        struct Reap(Vec<i32>);
        impl Drop for Reap {
            fn drop(&mut self) {
                for &pid in &self.0 {
                    unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
                }
            }
        }

        for yield_ms in [10000, 10] {
            for mixed in [false, true] {
                let dir = fixture();
                let cmd = if mixed {
                    "sleep .05 >/dev/null 2>&1 & echo $! > children; sleep .25 >/dev/null 2>&1 & echo $! >> children; printf done"
                } else {
                    "sleep .05 >/dev/null 2>&1 & echo $! > children; printf done"
                };
                let (first, j) = execute(
                    dir.path(),
                    json!({"cmd":cmd,"timeout_ms":2000,"yield_time_ms":yield_ms}),
                );
                let children = Reap(
                    fs::read_to_string(dir.path().join("children"))
                        .unwrap()
                        .split_whitespace()
                        .map(|pid| pid.parse().unwrap())
                        .collect(),
                );
                // Prove the regression actually retained zombies, rather than
                // relying only on a fast command or a fortunate host reaper.
                for pid in &children.0 {
                    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
                    assert_eq!(
                        stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
                        Some("Z")
                    );
                }
                let job = jobs(&j).unwrap().remove(0);
                let body = snapshot(dir.path(), &job, [0, 0], 100).unwrap();
                if yield_ms == 10 {
                    assert_eq!(first["finished"], false);
                    assert_eq!(first["timed_out"], false);
                }
                assert_eq!(body["finished"], true);
                assert_eq!(body["timed_out"], false);
                assert_eq!(body["cancelled"], false);
                assert_eq!(body["exit_code"], 0);
                assert_eq!(body["signal"], Value::Null);
                assert_eq!(body["stdout"], "done");
                assert!(job.duration_ms < 1500, "zombies delayed completion");
                assert!(job.duration_ms >= if mixed { 200 } else { 40 });
            }
        }
    }

    #[test]
    fn timeout_and_cancel_kill_redirected_descendants_after_shell_exit() {
        for cancel in [false, true] {
            let dir = fixture();
            let state = dir.path().to_owned();
            let handle = std::thread::spawn(move || {
                execute(
                    &state,
                    json!({
                        "cmd":"bash -c 'trap \"\" TERM INT; echo $$ > child.pid; while :; do sleep 1; done' >/dev/null 2>&1 & wait",
                        "timeout_ms":if cancel { 10000 } else { 500 },
                        "yield_time_ms":10
                    }),
                )
            });
            let j = Journal::new(dir.path()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let sid = loop {
                if let Ok(row) = j.get("test")
                    && row.status == "done"
                    && dir.path().join("child.pid").exists()
                {
                    break row.receipt().unwrap()["result"]["structuredContent"]["session_id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            };
            let pid: u32 = fs::read_to_string(dir.path().join("child.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            // Clean up the test's descendant even if this regression returns.
            struct Cleanup(u32);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    unsafe {
                        libc::kill(self.0 as i32, libc::SIGKILL);
                    }
                }
            }
            let _cleanup = Cleanup(pid);
            assert!(fingerprint(pid).is_some());
            if cancel {
                let args = json!({"session_id":sid,"chars":"\u{3}"});
                j.prepare("cancel", "write_stdin", &args).unwrap();
                deliver(dir.path(), &j, "cancel", &sid, &args, &mut false).unwrap();
            }
            handle.join().unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            while fingerprint(pid).is_some() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(fingerprint(pid).is_none(), "redirected descendant survived");
            let job = get(&j, &sid).unwrap();
            assert!(job.finished);
            assert_eq!(job.cancelled, cancel);
            assert_eq!(job.timed_out, !cancel);
            // The shell died on the first signal; KILL was for its descendant.
            assert_eq!(
                job.signal,
                Some(if cancel { libc::SIGINT } else { libc::SIGTERM })
            );
        }
    }

    #[test]
    fn idle_progress_is_throttled_but_output_receipts_and_exit_are_saved() {
        let dir = fixture();
        let j = Journal::new(dir.path()).unwrap();
        schema(&j).unwrap();
        j.db.execute_batch(
            "CREATE TABLE snapshots(data TEXT NOT NULL);
             CREATE TRIGGER count_snapshots AFTER UPDATE OF data ON pipe_sessions
             BEGIN INSERT INTO snapshots(data) VALUES(NEW.data); END;",
        )
        .unwrap();
        let (first, _) = execute(
            dir.path(),
            json!({
                "cmd":"printf early; sleep 2.2; printf late >&2", "yield_time_ms":30
            }),
        );
        assert_eq!(first["stdout"], "early");
        assert_eq!(first["finished"], false);
        let snapshots: Vec<Job> =
            j.db.prepare("SELECT data FROM snapshots")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|raw| serde_json::from_str(&raw.unwrap()).unwrap())
                .collect();
        assert!(
            (4..=8).contains(&snapshots.len()),
            "{} writes for idle command",
            snapshots.len()
        );
        assert!(
            snapshots
                .iter()
                .any(|job| !job.finished && job.duration_ms >= 1000)
        );
        let last = snapshots.last().unwrap();
        assert!(last.finished);
        assert_eq!(last.exit_code, Some(0));
        assert_eq!(last.stored, [5, 4]);
        assert_eq!(
            get(&j, first["session_id"].as_str().unwrap())
                .unwrap()
                .stored,
            [5, 4]
        );
    }

    #[test]
    fn cancellation_and_first_receipt_remains_immutable() {
        let dir = fixture();
        let state = dir.path().to_owned();
        let handle = std::thread::spawn(move || {
            execute(&state, json!({"cmd":"sleep 60", "yield_time_ms":10})).0
        });
        let j = Journal::new(dir.path()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let first = loop {
            if let Ok(row) = j.get("test")
                && row.status == "done"
            {
                break row.receipt().unwrap();
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        };
        let sid = first["result"]["structuredContent"]["session_id"]
            .as_str()
            .unwrap();
        let args = json!({"session_id":sid,"chars":"\u{3}"});
        j.prepare("cancel", "write_stdin", &args).unwrap();
        deliver(dir.path(), &j, "cancel", sid, &args, &mut false).unwrap();
        handle.join().unwrap();
        let cancelled = j.get("cancel").unwrap().receipt().unwrap();
        let end = &cancelled["result"]["structuredContent"];
        assert_eq!(end["cancelled"], true);
        assert_eq!(end["signal"], libc::SIGINT);
        assert_eq!(end["finished"], true);
        assert_eq!(j.get("test").unwrap().receipt().unwrap(), first);
    }

    #[test]
    fn invalid_workdir_does_not_dispatch() {
        let dir = fixture();
        let j = Journal::new(dir.path()).unwrap();
        let args = json!({"cmd":"true", "workdir":"relative"});
        let row = j.prepare("invalid", "exec_command", &args).unwrap();
        let mut dispatched = false;
        assert!(run(dir.path(), &j, &row, &args, &mut dispatched).is_err());
        assert!(!dispatched);
    }

    #[test]
    fn expired_logs_report_unavailability_without_changing_first_receipt() {
        let dir = fixture();
        let (first, j) = execute(
            dir.path(),
            json!({"cmd":"printf retained", "yield_time_ms":10000}),
        );
        let mut job = jobs(&j).unwrap().remove(0);
        job.started_at = now() - RETENTION - 1.0;
        save(&j, &job).unwrap();
        admit(dir.path(), &j).unwrap();
        let job = get(&j, &job.session_id).unwrap();
        let expired = snapshot(dir.path(), &job, [0, 0], 100).unwrap();
        assert_eq!(expired["stdout_ref"]["available"], false);
        assert_eq!(expired["capture_status"], "expired");
        assert!(!path(dir.path(), &job.session_id, 0).exists());
        let saved = j.get("test").unwrap().receipt().unwrap();
        assert_eq!(crate::runtime::result_body(&saved["result"]), first);
    }

    #[test]
    fn disappeared_supervisor_has_unknown_status_and_preserves_captured_bytes() {
        let dir = fixture();
        let (_, j) = execute(
            dir.path(),
            json!({"cmd":"printf partial", "yield_time_ms":10000}),
        );
        let mut job = jobs(&j).unwrap().remove(0);
        job.worker_fingerprint = "another-process".into();
        job.finished = false;
        job.exit_code = None;
        save(&j, &job).unwrap();
        assert_eq!(sessions(&j).unwrap()[0]["status"], "unknown");
        let body = snapshot(dir.path(), &job, [0, 0], 100).unwrap();
        assert_eq!(body["stdout"], "partial");
        assert_eq!(body["capture_status"], "supervisor_lost");
        let flat =
            crate::mcp::flatten(json!({"status":"done","result":{"structuredContent":body}}));
        assert_eq!(flat["status"], "unknown");
    }

    #[test]
    fn poll_recovers_output_when_supervisor_disappears_before_initial_receipt() {
        for collect_first in [false, true] {
            let dir = fixture();
            let command = json!({"cmd":"printf partial; printf error >&2", "yield_time_ms":10000});
            let (_, j) = execute(dir.path(), command.clone());
            let mut job = jobs(&j).unwrap().remove(0);
            job.worker_fingerprint = "disappeared-supervisor".into();
            job.finished = false;
            job.exit_code = None;
            save(&j, &job).unwrap();
            j.db.execute(
                "UPDATE requests SET status='running',result=NULL,worker_fingerprint=? WHERE id='test'",
                [&job.worker_fingerprint],
            ).unwrap();
            j.db.execute(
                "UPDATE pipe_sessions SET stdout_offset=0,stderr_offset=0",
                [],
            )
            .unwrap();
            assert_eq!(sessions(&j).unwrap()[0]["status"], "unknown");
            if collect_first {
                assert_eq!(
                    crate::runtime::collect(dir.path(), "test", 0).unwrap()["status"],
                    "unknown"
                );
            }
            let args = json!({"session_id":job.session_id,"max_output_tokens":1});
            let row = j.prepare("recover", "write_stdin", &args).unwrap();
            run(dir.path(), &j, &row, &args, &mut false).unwrap();
            let receipt = j.get("recover").unwrap().receipt().unwrap();
            let flat = crate::mcp::flatten(receipt.clone());
            assert_eq!(flat["status"], "unknown");
            assert_eq!(flat["capture_status"], "supervisor_lost");
            assert_eq!(flat["finished"], false);
            assert_eq!(flat["exit_code"], Value::Null);
            assert_eq!(flat["stdout"], "pa");
            assert_eq!(flat["stderr"], "er");
            for (name, expected) in [
                ("stdout", b"partial".as_slice()),
                ("stderr", b"error".as_slice()),
            ] {
                let reference = &flat[format!("{name}_ref")];
                assert_eq!(reference["available"], true);
                assert_eq!(reference["offset"], 2);
                assert_eq!(reference["end_offset"], expected.len());
                assert_eq!(
                    fs::read(reference["path"].as_str().unwrap()).unwrap(),
                    expected
                );
            }
            let origin = crate::runtime::collect(dir.path(), "test", 0).unwrap();
            assert_eq!(origin["status"], "unknown");
            assert_eq!(
                j.prepare("test", "exec_command", &command)
                    .unwrap()
                    .receipt()
                    .unwrap(),
                origin
            );
            // Replays preserve the recovery receipt; subsequent polls consume no bytes twice.
            assert_eq!(
                j.prepare("recover", "write_stdin", &args)
                    .unwrap()
                    .receipt()
                    .unwrap(),
                receipt
            );
            let row = j.prepare("again", "write_stdin", &args).unwrap();
            run(dir.path(), &j, &row, &args, &mut false).unwrap();
            let again = j.get("again").unwrap().receipt().unwrap();
            assert_eq!(again["result"]["structuredContent"]["stdout"], "");
            assert_eq!(again["result"]["structuredContent"]["stderr"], "");
            assert_eq!(j.get("test").unwrap().receipt().unwrap(), origin);
        }
    }

    #[test]
    fn polling_requires_initial_receipt_or_terminal_origin_and_dead_supervisor() {
        for live in [false, true] {
            for status in ["queued", "running", "done", "error", "unknown"] {
                let dir = fixture();
                let (_, j) = execute(dir.path(), json!({"cmd":"printf retained"}));
                let mut job = jobs(&j).unwrap().remove(0);
                if !live {
                    job.worker_fingerprint = "disappeared-supervisor".into();
                }
                job.finished = false;
                save(&j, &job).unwrap();
                j.db.execute("UPDATE requests SET status=? WHERE id='test'", [status])
                    .unwrap();
                j.db.execute(
                    "UPDATE pipe_sessions SET stdout_offset=0,stderr_offset=0",
                    [],
                )
                .unwrap();
                let args = json!({"session_id":job.session_id});
                j.prepare("poll", "write_stdin", &args).unwrap();
                let result = deliver(dir.path(), &j, "poll", &job.session_id, &args, &mut false);
                let allowed = status == "done" || (!live && matches!(status, "error" | "unknown"));
                assert_eq!(
                    result.is_ok(),
                    allowed,
                    "live={live}, status={status}: {result:?}"
                );
                let offsets: [u64; 2] =
                    j.db.query_row(
                        "SELECT stdout_offset,stderr_offset FROM pipe_sessions",
                        [],
                        |r| Ok([r.get(0)?, r.get(1)?]),
                    )
                    .unwrap();
                assert_eq!(offsets, if allowed { job.stored } else { [0, 0] });
            }
        }
    }
}
