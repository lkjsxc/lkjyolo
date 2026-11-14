//! Durable, bounded, byte-preserving capture for non-interactive commands.
//! One detached request worker owns the process and drains both nonblocking
//! pipes. Receipts and delivery cursors commit in the same SQLite transaction.
use crate::{
    engine::Engine,
    journal::{Journal, Request},
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
    let mut receipt_saved = false;
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
        job.finished = status.is_some() && !open[0] && !open[1];
        if job.finished {
            let status = status.unwrap();
            job.exit_code = status.code();
            job.signal = status.signal();
        }
        let cancel: bool = j.db.query_row(
            "SELECT cancel FROM pipe_sessions WHERE session_id=?",
            [&sid],
            |r| r.get(0),
        )?;
        if !job.finished
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
        if !job.finished && interrupted.is_some_and(|at| at.elapsed() >= Duration::from_millis(250))
        {
            unsafe {
                libc::kill(-(child.0.id() as i32), libc::SIGKILL);
            }
        }
        save(j, &job)?;
        if !receipt_saved && (job.finished || job.duration_ms >= yield_ms) {
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
    ensure!(
        origin == id || j.get(origin)?.status == "done",
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

#[cfg(test)]
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
}
