use crate::{
    VERSION,
    engine::Engine,
    journal::{Journal, TERMINAL, validate_id},
    util::*,
};
use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn collect(state: &Path, id: &str, wait: u64) -> Result<Value> {
    let journal = Journal::new(state)?;
    let deadline = Instant::now() + Duration::from_secs(wait);
    loop {
        let row = journal.reconcile(journal.get(id)?)?;
        if TERMINAL.contains(&row.status.as_str()) || Instant::now() >= deadline {
            return row.receipt();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn submit(
    state: &Path,
    tool: &str,
    arguments: &Value,
    id: Option<&str>,
    wait: u64,
) -> Result<Value> {
    let journal = Journal::new(state)?;
    let id = id
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let row = journal.prepare(&id, tool, arguments)?;
    if row.status == "queued" {
        let log = append_log(&state.join("workers.log"))?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--state")
            .arg(state)
            .arg("_worker")
            .arg(&id)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // Worker locks and rechecks the row before any dispatch. Concurrent
        // retriers may create workers, but only one can execute this request.
        detached(&mut command)
            .spawn()
            .context("Start durable worker")?;
    }
    collect(state, &id, wait)
}

pub fn run_worker(state: &Path, id: &str) -> Result<()> {
    validate_id(id)?;
    let journal = Journal::new(state)?;
    let locks = state.join("request-locks");
    private_dir(&locks)?;
    let _lock = lock(&locks.join(format!("{id}.lock")))?;
    let row = journal.reconcile(journal.get(id)?)?;
    if row.status != "queued" {
        return Ok(());
    }
    journal.running(id)?;
    let mut dispatched = false;
    let outcome = (|| -> Result<Value> {
        let arguments: Value = serde_json::from_str(&row.arguments)?;
        if row.tool == "apply_patch" {
            return crate::files::apply(state, &arguments, &mut dispatched)
                .map(|body| json!({"structuredContent":body,"isError":false}));
        }
        let engine = Engine::new(state)?;
        let identity = engine.ensure()?;
        journal.generation(id, &identity.generation)?;
        journal.check_session(&arguments, &identity.generation)?;
        let wait = arguments["wait_seconds"].as_u64().unwrap_or(0).min(100);
        let yield_ms = arguments["yield_time_ms"].as_u64().unwrap_or(0).min(120000);
        let timeout = 50.max(wait + 15).max(yield_ms.div_ceil(1000) + 15);
        dispatched = true;
        engine.rpc(
            "tools/call",
            json!({"name":row.tool,"arguments":arguments}),
            timeout,
        )
    })();
    match outcome {
        Ok(value) => journal.finish(id, if value["isError"] == true { "error" } else { "done" }, &value),
        Err(error) => journal.finish(id, if dispatched { "unknown" } else { "error" }, &json!({
            "isError":true,"error":format!("{error:#}"),"execution_may_have_started":dispatched,
            "retry":if dispatched {"Inspect this receipt and side effects; never repeat an uncertain mutation with a new ID."}
                    else {"Resolve the error, then submit a new request ID."}
        })),
    }
}

pub fn result_body(result: &Value) -> Value {
    if result["structuredContent"].is_object() {
        return result["structuredContent"].clone();
    }
    if let Some(content) = result["content"].as_array() {
        for part in content {
            if part["type"] == "text"
                && let Some(text) = part["text"].as_str()
                && let Ok(value) = serde_json::from_str::<Value>(text)
                && value.is_object()
            {
                return value;
            }
        }
    }
    json!({})
}

pub fn sessions(state: &Path) -> Result<Value> {
    let engine = Engine::new(state)?;
    let generation = engine.identity().map(|i| i.generation);
    let journal = Journal::new(state)?;
    let mut sessions = BTreeMap::<(Option<String>, String), Value>::new();
    let mut rows = journal.rows(i64::MAX as usize)?;
    // Concurrent calls can complete in a different order from submission.
    // Reconstruct observations by receipt time; creation time and request ID
    // provide deterministic tie breakers when timestamps are equal.
    rows.sort_by(|a, b| {
        a.updated
            .total_cmp(&b.updated)
            .then_with(|| a.created.total_cmp(&b.created))
            .then_with(|| a.id.cmp(&b.id))
    });
    for row in rows {
        if !TERMINAL.contains(&row.status.as_str()) {
            continue;
        }
        let args: Value = serde_json::from_str(&row.arguments)?;
        let result = row
            .result
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            .unwrap_or(json!({}));
        let body = result_body(&result);
        let exit_code = body.get("exit_code").and_then(Value::as_i64);
        let Some(sid) = body["session_id"].as_str().or(args["session_id"].as_str()) else {
            continue;
        };
        // A failed stale-session operation belongs to its original generation.
        // Do not invent a current-generation session from an error receipt.
        if !body["session_id"].is_string()
            && exit_code.is_none()
            && !sessions.contains_key(&(row.generation.clone(), sid.to_owned()))
        {
            continue;
        }
        let item = sessions
            .entry((row.generation.clone(), sid.into()))
            .or_insert_with(|| json!({"session_id":sid,"generation":row.generation}));
        item["last_request_id"] = json!(row.id);
        item["last_seen_at"] = json!(row.updated);
        if let Some(exit_code) = exit_code {
            item["status"] = json!("exited");
            item["exit_code"] = json!(exit_code);
        } else if item["status"] == "exited" {
            // A delayed running receipt or subsequent error cannot resurrect
            // a process whose exit has already been observed.
            continue;
        } else if row.generation != generation {
            item["status"] = json!("engine_restarted_or_stopped");
        } else if result["isError"] == true || row.status == "unknown" {
            item["status"] = json!("unknown");
        } else if body.get("session_id").is_some() {
            item["status"] = json!("running_at_last_observation");
        }
    }
    // Refresh generation markers even if no calls occurred after restart.
    for ((session_generation, _), item) in &mut sessions {
        if session_generation != &generation && item["status"] != "exited" {
            item["status"] = json!("engine_restarted_or_stopped");
        }
    }
    Ok(
        json!({"sessions":sessions.into_values().collect::<Vec<_>>(),
        "note":"Observations, not a process liveness guarantee; poll or wait a current-generation session to refresh."}),
    )
}

pub fn read_log(state: &Path, path: &Path, offset: u64, limit: u64) -> Result<Value> {
    ensure!(
        (1..=1048576).contains(&limit),
        "Log limit must be between 1 and 1048576"
    );
    let config: Value = serde_json::from_slice(&fs::read(state.join("engine.json"))?)?;
    let root = Path::new(
        config["output_store_dir"]
            .as_str()
            .context("Missing output_store_dir")?,
    )
    .canonicalize()?;
    let path = path.canonicalize()?;
    ensure!(
        path != root && path.starts_with(&root),
        "log only accepts output_ref files in the engine output store; use Coder file tools for other files"
    );
    let mut file = File::open(&path)?;
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(offset))?;
    let mut data = Vec::new();
    file.take(limit).read_to_end(&mut data)?;
    let next = offset
        .checked_add(data.len() as u64)
        .context("Log offset overflow")?;
    let mut value = json!({"path":path,"offset":offset,"next_offset":next,"size_bytes":size,"eof":next>=size,"text":String::from_utf8_lossy(&data)});
    if std::str::from_utf8(&data).is_err() {
        value["base64"] = json!(base64::engine::general_purpose::STANDARD.encode(&data));
        value["encoding_loss"] = json!(true);
    }
    Ok(value)
}

pub fn find_executable(name: &str, path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|p| p.join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

pub fn doctor(state: &Path) -> Result<Value> {
    let engine = Engine::new(state)?;
    let settings = engine.settings()?;
    let path = settings
        .environment
        .get("PATH")
        .context("Missing configured PATH")?;
    let sudo_ok = find_executable("sudo", path).is_some_and(|sudo| {
        Command::new(sudo)
            .args(["-n", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
            .and_then(|mut child| wait_child(&mut child, 5).ok())
            .is_some_and(|s| s.success())
    });
    let tools: BTreeMap<_, _> = [
        "git", "gh", "python3", "node", "npm", "cargo", "docker", "tmux",
    ]
    .iter()
    .map(|name| (*name, find_executable(name, path)))
    .collect();
    Ok(
        json!({"version":VERSION,"runtime":"rust","state":state,"workspace":settings.workspace,
        "engine_ready":engine.ready(),"daemon":engine.identity(),"sudo_without_password":sudo_ok,"tools":tools,
        "transport":"owner-only Unix socket; authenticated Coder MCP is the remote entrypoint",
        "permissions":"No command allowlist, path sandbox, approval prompt or execution deadline added by lkjyolo",
        "persistence":"Requests/logs survive reconnection; processes survive client disconnects, not engine/workspace restart"}),
    )
}
