//! Recover continuation quota without restarting the engine or discarding output.
use crate::{
    engine::Engine,
    journal::{Journal, Request},
    runtime::result_body,
    util::*,
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

struct Session {
    handle: String,
    engine_id: String,
    generation: String,
    terminal: bool,
    pending: Option<String>,
    final_result: Option<String>,
}

pub struct Delivery {
    pub value: Value,
    // Keep input/collection serialization through the durable receipt commit.
    _lock: File,
    session: Option<Session>,
    new_origin: Option<(String, String)>,
    consume_pending: bool,
}

fn schema(journal: &Journal) -> Result<()> {
    journal.db.execute_batch("CREATE TABLE IF NOT EXISTS managed_sessions (
        handle TEXT PRIMARY KEY, engine_id TEXT NOT NULL, generation TEXT NOT NULL,
        origin_request TEXT NOT NULL UNIQUE, origin_tool TEXT NOT NULL,
        terminal INTEGER NOT NULL DEFAULT 0, pending TEXT, final_result TEXT,
        uncertain INTEGER NOT NULL DEFAULT 0);
        CREATE INDEX IF NOT EXISTS managed_sessions_active ON managed_sessions(generation,origin_tool,terminal);")?;
    Ok(())
}

fn get(journal: &Journal, handle: &str) -> Result<Option<Session>> {
    Ok(journal.db.query_row("SELECT handle,engine_id,generation,terminal,pending,final_result FROM managed_sessions WHERE handle=?", [handle], |r| Ok(Session {
        handle:r.get(0)?,engine_id:r.get(1)?,generation:r.get(2)?,terminal:r.get(3)?,pending:r.get(4)?,final_result:r.get(5)?
    })).optional()?)
}

fn session_lock(state: &Path, handle: &str, nonblocking: bool) -> Result<Option<File>> {
    let dir = state.join("session-locks");
    private_dir(&dir)?;
    let path = dir.join(format!("{}.lock", sha256(handle.as_bytes())));
    if !nonblocking {
        return Ok(Some(lock(&path)?));
    }
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn response(body: Value) -> Value {
    json!({"content":[{"type":"text","text":body.to_string()}],"structuredContent":body,"isError":false})
}

fn terminal(value: &Value) -> bool {
    let b = result_body(value);
    value["isError"] != true
        && !b["session_id"].is_string()
        && (b["exit_code"].is_number() || b["capture_error"].is_string())
}

fn public_response(value: Value, handle: &str) -> Value {
    let mut b = result_body(&value);
    if value["isError"] != true && b["session_id"].is_string() {
        b["session_id"] = json!(handle);
        response(b)
    } else {
        value
    }
}

/// Only this pinned-engine rejection proves that no process was spawned.
fn quota_rejection(value: &Value, tool: &str) -> bool {
    let prefix = match tool {
        "exec_command" => "exec continuation session quota reached (",
        "start_session" => "explicit session quota reached (",
        _ => return false,
    };
    let Some(items) = value["content"].as_array() else {
        return false;
    };
    value["isError"] == true
        && items.len() == 1
        && items[0]["type"] == "text"
        && items[0]["text"]
            .as_str()
            .and_then(|s| s.strip_prefix(prefix))
            .and_then(|s| s.strip_suffix(')'))
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

pub fn dispatch(
    engine: &Engine,
    journal: &Journal,
    row: &Request,
    generation: &str,
    arguments: &Value,
    timeout: u64,
    dispatched: &mut bool,
) -> Result<Delivery> {
    let state = &engine.state;
    schema(journal)?;
    if let Some(handle) = arguments["session_id"].as_str() {
        if !handle.starts_with("lkj-") && get(journal, handle)?.is_none() {
            // Register legacy inputs before they can consume a final response.
            // Otherwise a later import could mistake a recycled engine ID for
            // this already-delivered session.
            let _admission = lock(&state.join("session-admission.lock"))?;
            import_legacy(journal, generation)?;
        }
        let guard = session_lock(state, handle, false)?.unwrap();
        let mut session = get(journal, handle)?;
        let mut args = arguments.clone();
        if let Some(s) = &mut session {
            ensure!(
                s.generation == generation,
                "Session belongs to a stopped/restarted engine"
            );
            if s.pending.is_some() || s.terminal {
                ensure!(
                    arguments["chars"].as_str().unwrap_or("").is_empty(),
                    "Session output was collected automatically; poll with empty chars to retrieve it. Input was not sent."
                );
                let value = if let Some(pending) = &s.pending {
                    let mut values: Vec<Value> = serde_json::from_str(pending)?;
                    ensure!(!values.is_empty(), "Empty pending output queue");
                    let mut first = values.remove(0);
                    let mut body = result_body(&first);
                    let budget = arguments["max_output_tokens"]
                        .as_u64()
                        .unwrap_or(4000)
                        .saturating_mul(4)
                        .min(1048576) as usize;
                    ensure!(budget > 0, "max_output_tokens must be positive");
                    if let Some(output) = body["output"].as_str().filter(|s| s.len() > budget) {
                        let mut split = budget;
                        while !output.is_char_boundary(split) {
                            split -= 1;
                        }
                        let head = output[..split].to_owned();
                        let tail = output[split..].to_owned();
                        let mut remainder = body.clone();
                        remainder["output"] = json!(tail);
                        values.insert(0, response(remainder));
                        body["output"] = json!(head);
                        body.as_object_mut().unwrap().remove("exit_code");
                        body["session_id"] = json!(handle);
                        first = response(body);
                    }
                    s.pending = if values.is_empty() {
                        None
                    } else {
                        Some(serde_json::to_string(&values)?)
                    };
                    first
                } else {
                    let mut b = result_body(&serde_json::from_str(
                        s.final_result
                            .as_deref()
                            .context("Missing terminal receipt")?,
                    )?);
                    b["output"] = json!("");
                    b["output_truncated"] = json!(false);
                    response(b)
                };
                return Ok(Delivery {
                    value,
                    _lock: guard,
                    session,
                    new_origin: None,
                    consume_pending: true,
                });
            }
            args["session_id"] = json!(s.engine_id);
        }
        *dispatched = true;
        let value = public_response(
            engine.rpc(
                "tools/call",
                json!({"name":row.tool,"arguments":args}),
                timeout,
            )?,
            handle,
        );
        return Ok(Delivery {
            value,
            _lock: guard,
            session,
            new_origin: None,
            consume_pending: false,
        });
    }

    // Admission and recovery share a lock so an engine ID cannot be reused while
    // a collector is archiving its previous incarnation. Input has separate locks.
    let guard = lock(&state.join("session-admission.lock"))?;
    *dispatched = true;
    let mut value = engine.rpc(
        "tools/call",
        json!({"name":row.tool,"arguments":arguments}),
        timeout,
    )?;
    if quota_rejection(&value, &row.tool) {
        *dispatched = false;
        if recover(state, engine, journal, generation, &row.tool)? > 0 {
            *dispatched = true;
            value = engine.rpc(
                "tools/call",
                json!({"name":row.tool,"arguments":arguments}),
                timeout,
            )?;
        }
    }
    let b = result_body(&value);
    let session = b["session_id"]
        .as_str()
        .filter(|_| value["isError"] != true)
        .map(|id| Session {
            // Keep the syntax accepted by deployed gateways while retaining
            // all UUID bits in a..p encoding.
            handle: format!(
                "lkj-{}",
                uuid::Uuid::new_v4()
                    .as_bytes()
                    .iter()
                    .flat_map(|b| [char::from(b'a' + (b >> 4)), char::from(b'a' + (b & 15))])
                    .collect::<String>()
            ),
            engine_id: id.into(),
            generation: generation.into(),
            terminal: false,
            pending: None,
            final_result: None,
        });
    let new_origin = session.as_ref().map(|_| (row.id.clone(), row.tool.clone()));
    if let Some(s) = &session {
        value = public_response(value, &s.handle);
    }
    Ok(Delivery {
        value,
        _lock: guard,
        session,
        new_origin,
        consume_pending: false,
    })
}

impl Delivery {
    pub fn commit(self, journal: &Journal, id: &str) -> Result<()> {
        let tx = journal.db.unchecked_transaction()?;
        if let Some(s) = &self.session {
            if let Some((origin, tool)) = &self.new_origin {
                tx.execute("INSERT INTO managed_sessions(handle,engine_id,generation,origin_request,origin_tool) VALUES(?,?,?,?,?)",
                    params![s.handle,s.engine_id,s.generation,origin,tool])?;
            }
            if self.consume_pending {
                tx.execute(
                    "UPDATE managed_sessions SET pending=? WHERE handle=?",
                    params![s.pending, s.handle],
                )?;
            }
            if terminal(&self.value) {
                tx.execute(
                    "UPDATE managed_sessions SET terminal=1,final_result=? WHERE handle=?",
                    params![self.value.to_string(), s.handle],
                )?;
            }
            if self.value["isError"] != true {
                tx.execute(
                    "UPDATE managed_sessions SET uncertain=0 WHERE handle=?",
                    [&s.handle],
                )?;
            }
        }
        journal.finish(
            id,
            if self.value["isError"] == true {
                "error"
            } else {
                "done"
            },
            &self.value,
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Import still-open pre-upgrade sessions; never overwrite an archived mapping.
fn import_legacy(journal: &Journal, generation: &str) -> Result<()> {
    let mut rows = journal.rows(i64::MAX as usize)?;
    rows.reverse();
    let mut active = BTreeMap::new();
    for row in rows {
        if row.generation.as_deref() != Some(generation) {
            continue;
        }
        let args: Value = serde_json::from_str(&row.arguments)?;
        let result: Value = serde_json::from_str(row.result.as_deref().unwrap_or("{}"))?;
        let b = result_body(&result);
        if let Some(sid) = b["session_id"].as_str().filter(|s| !s.starts_with("lkj-"))
            && matches!(row.tool.as_str(), "exec_command" | "start_session")
        {
            active.insert(sid.to_owned(), (row.id, row.tool));
        } else if terminal(&result)
            && let Some(sid) = args["session_id"].as_str()
        {
            active.remove(sid);
        }
    }
    for (sid, (origin, tool)) in active {
        journal.db.execute("INSERT OR IGNORE INTO managed_sessions(handle,engine_id,generation,origin_request,origin_tool) VALUES(?,?,?,?,?)",params![sid,sid,generation,origin,tool])?;
    }
    Ok(())
}

/// This is only a conservative skip hint, never proof of completion. A poll's
/// actual result decides completion, and even a nonterminal result is buffered.
fn live_ids() -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|c| c.is_ascii_digit())
        {
            continue;
        }
        let bytes = match fs::read(entry.path().join("environ")) {
            Ok(b) => b,
            // This includes zombies, vanished processes and protected services.
            // Missing metadata never proves completion; poll results are saved
            // even when the process turns out to still be alive.
            Err(_) => continue,
        };
        for e in bytes.split(|b| *b == 0) {
            if let Some(sid) = e.strip_prefix(b"CHATGPT_EXEC_SESSION=") {
                ids.insert(String::from_utf8_lossy(sid).into_owned());
            }
        }
    }
    Ok(ids)
}

fn recover(
    state: &Path,
    engine: &Engine,
    journal: &Journal,
    generation: &str,
    tool: &str,
) -> Result<usize> {
    import_legacy(journal, generation)?;
    let live = live_ids()?;
    let handles = journal.db.prepare("SELECT handle FROM managed_sessions WHERE generation=? AND origin_tool=? AND terminal=0 AND uncertain=0 ORDER BY rowid")?
        .query_map(params![generation,tool],|r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut released = 0;
    for handle in handles {
        let Some(_guard) = session_lock(state, &handle, true)? else {
            continue;
        };
        let s = get(journal, &handle)?.context("Session disappeared")?;
        if s.terminal || live.contains(&s.engine_id) {
            continue;
        }
        ensure!(
            engine
                .identity()
                .is_some_and(|i| i.generation == generation),
            "Engine changed during quota recovery"
        );
        let id = format!("auto-collect-{}", uuid::Uuid::new_v4().simple());
        let args = json!({"session_id":handle,"chars":"","max_output_tokens":4000});
        journal.prepare(&id, "write_stdin", &args)?;
        journal.running(&id)?;
        journal.generation(&id, generation)?;
        // Mark before RPC. A killed collector or ambiguous transport is never
        // automatically polled again, and its original receipt remains auditable.
        journal.db.execute(
            "UPDATE managed_sessions SET uncertain=1 WHERE handle=?",
            [&handle],
        )?;
        let result = engine.rpc("tools/call",json!({"name":"write_stdin","arguments":{"session_id":s.engine_id,"chars":"","max_output_tokens":4000}}),50);
        match result {
            Ok(v) => {
                let value = public_response(v, &handle);
                let done = terminal(&value);
                let tx = journal.db.unchecked_transaction()?;
                journal.finish(
                    &id,
                    if value["isError"] == true {
                        "error"
                    } else {
                        "done"
                    },
                    &value,
                )?;
                if value["isError"] != true {
                    let mut pending: Vec<Value> =
                        serde_json::from_str(s.pending.as_deref().unwrap_or("[]"))?;
                    pending.push(value.clone());
                    tx.execute("UPDATE managed_sessions SET pending=?,terminal=?,final_result=?,uncertain=0 WHERE handle=?",
                        params![serde_json::to_string(&pending)?,done,if done {Some(value.to_string())} else {None},handle])?;
                }
                tx.commit()?;
                released += usize::from(done);
            }
            Err(e) => {
                journal.finish(&id,"unknown",&json!({"isError":true,"error":e.to_string(),"execution_may_have_started":true}))?;
            }
        }
    }
    Ok(released)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_exact_quota_errors_allow_retry() {
        let error = json!({"isError":true,"content":[{"type":"text","text":"exec continuation session quota reached (128)"}]});
        assert!(quota_rejection(&error, "exec_command"));
        assert!(!quota_rejection(&error, "start_session"));
        let mut success = error.clone();
        success["isError"] = json!(false);
        assert!(!quota_rejection(&success, "exec_command"));
        let ambiguous = json!({"isError":true,"error":"connection lost"});
        assert!(!quota_rejection(&ambiguous, "exec_command"));
    }
}
