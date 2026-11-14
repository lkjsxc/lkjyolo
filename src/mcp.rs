//! A deliberately small development interface, shared by HTTP and CLI transports.
use crate::{VERSION, files, journal::Journal, runtime};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

pub const VERSIONS: &[&str] = &["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26"];
pub const INSTRUCTIONS: &str = "lkjyolo is a remote shell and file API for one persistent Linux workspace, meant for clients that have no shell of their own. An agent that already runs inside this workspace (for example Codex) has no need for it. Commands run as the workspace user with its OS permissions, sudo, Docker, network and existing GitHub login; the server adds no allowlist or approval prompt. Before editing a repository, check its git status and AGENTS.md. Do not restart or reconfigure running services, and do not print or copy credential files, unless the user asked for that. request_id is optional and server-assigned when omitted; it is returned in every result. After a disconnect call get_result with the ID (or resend the same ID and arguments); never repeat an unknown mutation under a new ID. Result fields: status=done means a response was saved, so check exit_code for success; a session_id means the process is still running (continue with write_stdin; list_sessions finds a forgotten one); queued/running means poll get_result; Default commands return separate stdout/stderr, exit_code (null on signal), signal, duration_ms, truncated flags and stdout_ref/stderr_ref byte ranges for read_file; cross-stream interleaving order is not guaranteed. tty=true uses merged terminal output in output/output_ref instead; its signal and total duration are unavailable (null). Processes survive disconnects. Workspace/container restarts lose processes; an external engine restart loses only terminal sessions. Native supervisor loss is an unknown outcome and may leave children alive. Explanation-only requests need no execution.";

fn string(description: &str) -> Value {
    json!({"type":"string","description":description})
}
fn integer(default: u64, minimum: u64, maximum: u64) -> Value {
    json!({"type":"integer","default":default,"minimum":minimum,"maximum":maximum})
}
fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,"idempotentHint":read_only,"openWorldHint":true}})
}
pub fn tools() -> Value {
    let id = string(
        "Optional operation ID (letters, digits, . _ -; up to 120). Omit it and the server assigns one and returns it. Set your own only when you may need to recover this call: after a transport failure resend the same ID and arguments, or call get_result with it. Never retry uncertain work under a different ID.",
    );
    json!([
        tool(
            "exec_command",
            "Run /bin/bash -c (non-login) in a fresh shell. Default tty=false uses native pipes with closed stdin: separate stdout and stderr (cross-stream interleaving order is not guaranteed), real exit_code (null on signal), signal, duration_ms, finished, cancelled/timed_out, and truncated/stdout_truncated/stderr_truncated flags. max_output_tokens is an approximate total byte budget (4 bytes/token, half per stream). stdout_ref/stderr_ref give raw log paths, offset after displayed bytes, and end_offset for read_file continuation; polls return newly captured bytes, so read refs for omitted bytes. Capture retains the first 1 MiB per stream; *_capture_truncated reports discarded bytes. Invalid UTF-8 includes *_base64 and *_encoding_loss. Waits up to yield_time_ms, then returns session_id while the durable worker continues; list_sessions rediscovers jobs and write_stdin polls/cancels. Optional timeout_ms terminates the process group. For terminal input, REPLs or persistent shell state use tty=true: the pinned engine returns merged output/output_ref, stream_mode=engine_merged, null stdout/stderr/signal/duration_ms and its shell-style exit_code. PTYs may echo input, add CRLF/ANSI and change buffering. workdir defaults to the configured workspace; cd/exports do not persist across execs. Check finished plus exit_code/signal, not status=done. Without request_id, repeating a call starts a new execution. Pass a stable request_id to return the first saved response without re-execution.",
            json!({"request_id":id,"cmd":string("Shell command. Use /bin/bash with tty=true for a persistent interactive shell."),"workdir":string("Absolute working directory. Default: configured workspace root."),"tty":{"type":"boolean","default":false,"description":"Use the PTY engine for terminal input; streams are merged."},"timeout_ms":{"type":"integer","minimum":1,"maximum":604800000,"description":"Optional native command deadline, including background descendants; omitted means no deadline. tty=false only."},"yield_time_ms":integer(1000,10,10000),"max_output_tokens":integer(4096,1,32768)}),
            &["cmd"],
            false
        ),
        tool(
            "write_stdin",
            "Continue a session: poll newly captured stdout/stderr with empty chars, wait with wait_seconds 15..35 (empty chars only), or cancel the command process group with chars=\\u0003 (SIGINT, then SIGKILL after 250 ms if needed). Native pipes have closed stdin; other input requires tty=true and returns merged terminal output. Cross-stream interleaving order is not guaranteed; use stdout_ref/stderr_ref and read_file to recover omitted raw bytes. Every distinct input or poll needs a fresh request_id if you supply one. Native final polls return finished=true, exit_code (null on signal), signal and duration_ms. Native running capacity is freed at exit; engine sessions need a final poll. Not idempotent: input sent twice is delivered twice; pass a stable request_id and a retry returns the first result instead of sending again.",
            json!({"request_id":id,"session_id":string("Session ID returned by exec_command"),"chars":{"type":"string","default":""},"wait_seconds":integer(0,0,35),"max_output_tokens":integer(4096,1,32768)}),
            &["session_id"],
            false
        ),
        tool(
            "read_file",
            "Read a UTF-8 file, binary file (base64), directory or saved command log by absolute path. Files return byte offsets and SHA-256 for guarded edits (up to 16 MiB). Directories return up to 500 entries; their offsets count entries.",
            json!({"path":string("Absolute file or directory path"),"offset":integer(0,0,i64::MAX as u64),"limit":integer(32768,1,262144)}),
            &["path"],
            true
        ),
        tool(
            "apply_patch",
            "Create, replace or delete text files with stale-write protection. Read first and supply before_sha256; use absent for a new file, content=null for deletion. Parents must exist. All hashes are preflighted; individual replacements are atomic, but a batch is not a transaction. Symlink targets are rejected. Use exec_command for other file operations. Not idempotent without a request_id: pass a stable request_id and a retry returns the first result instead of applying again (the hash check also rejects a stale re-apply).",
            json!({"request_id":id,"changes":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"object","properties":{"path":string("Absolute file path"),"before_sha256":string("SHA-256 from read_file, or absent"),"content":{"type":["string","null"],"description":"Complete replacement text, or null to delete"}},"required":["path","before_sha256","content"],"additionalProperties":false}}}),
            &["changes"],
            false
        ),
        tool(
            "list_sessions",
            "List sessions started by exec_command: running ones first, then the most recent finished ones. Each item has session_id, the starting command (truncated), workdir, status (running_at_last_observation, exited with exit_code or signal, engine_restarted_or_stopped, unknown) and last request_id. Bounded: default limit 20; when more_available is true pass next_cursor as cursor for the next page. Use status=running to see only live jobs. Use it to find a background job after losing its session_id, then poll with write_stdin (empty chars) or cancel with chars=\\u0003. Statuses are observations, not liveness guarantees.",
            json!({"status":{"type":"string","enum":["all","running","exited","lost"],"default":"all","description":"running = running_at_last_observation or unknown; exited = finished with exit_code or signal; lost = engine_restarted_or_stopped"},"limit":integer(20,1,100),"cursor":string("next_cursor from a previous page")}),
            &[],
            true
        ),
        tool(
            "get_result",
            "Recover a saved operation after disconnect without repeating it. With no request_id, list recent request summaries. queued/running: poll again; unknown: inspect effects before starting any replacement operation.",
            json!({"request_id":string("Previously submitted operation ID"),"limit":integer(20,1,100)}),
            &[],
            true
        )
    ])
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecArgs {
    request_id: Option<String>,
    cmd: String,
    workdir: Option<String>,
    #[serde(default)]
    tty: bool,
    #[serde(default = "yield_ms")]
    yield_time_ms: u64,
    #[serde(default = "tokens")]
    max_output_tokens: u64,
    timeout_ms: Option<u64>,
}
fn yield_ms() -> u64 {
    1000
}
fn tokens() -> u64 {
    4096
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputArgs {
    request_id: Option<String>,
    session_id: String,
    #[serde(default)]
    chars: String,
    #[serde(default)]
    wait_seconds: u64,
    #[serde(default = "tokens")]
    max_output_tokens: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsArgs {
    #[serde(default = "all_status")]
    pub status: String,
    #[serde(default = "recent_limit")]
    pub limit: usize,
    pub cursor: Option<String>,
}
fn all_status() -> String {
    "all".into()
}
impl Default for SessionsArgs {
    fn default() -> Self {
        Self {
            status: all_status(),
            limit: recent_limit(),
            cursor: None,
        }
    }
}

/// Running sessions first, then finished ones by recency; bounded page with a cursor.
/// Keeps the response far below the 2 MiB Coder transport limit however long the history.
pub fn page_sessions(all: &Value, args: &SessionsArgs) -> Value {
    let rank = |s: &Value| match s["status"].as_str() {
        Some("running_at_last_observation" | "unknown") => 0,
        Some("exited") => 1,
        _ => 2,
    };
    let mut items: Vec<&Value> = all["sessions"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    items.retain(|s| match args.status.as_str() {
        "running" => rank(s) == 0,
        "exited" => rank(s) == 1,
        "lost" => rank(s) == 2,
        _ => true,
    });
    items.sort_by(|a, b| {
        rank(a).cmp(&rank(b)).then_with(|| {
            b["last_seen_at"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&a["last_seen_at"].as_f64().unwrap_or(0.0))
        })
    });
    let total = items.len();
    let start = args
        .cursor
        .as_deref()
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0)
        .min(total);
    let end = (start + args.limit).min(total);
    let page: Vec<Value> = items[start..end]
        .iter()
        .map(|s| {
            let mut v = (*s).clone();
            if let Some(cmd) = v["cmd"].as_str()
                && cmd.chars().count() > 120
            {
                v["cmd"] = json!(format!("{}...", cmd.chars().take(120).collect::<String>()));
            }
            v
        })
        .collect();
    let mut out = json!({"sessions":page,"total_matching":total,"more_available":end<total,
        "note":"Observations, not a process liveness guarantee; poll a current-generation session to refresh."});
    if end < total {
        out["next_cursor"] = json!(end.to_string());
    }
    out
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultArgs {
    request_id: Option<String>,
    #[serde(default = "recent_limit")]
    limit: usize,
}
fn recent_limit() -> usize {
    20
}

pub fn flatten(receipt: Value) -> Value {
    let result = &receipt["result"];
    let mut body = runtime::result_body(result);
    for key in ["request_id", "status", "tool", "generation"] {
        if let Some(value) = receipt.get(key) {
            body[key] = value.clone();
        }
    }
    if result["isError"] == true {
        body["isError"] = json!(true);
        body["error"] = result
            .get("error")
            .cloned()
            .unwrap_or_else(|| result["content"].clone());
    }
    if body["capture_status"] == "supervisor_lost" {
        body["status"] = json!("unknown");
    }
    if matches!(receipt["status"].as_str(), Some("queued" | "running")) {
        body["next"] = json!("Call get_result with this request_id.");
    }
    body
}

/// Validate even in gateway mode, before forwarding anything to Coder.
pub fn validate(name: &str, args: &Value) -> Result<()> {
    ensure!(args.is_object(), "arguments must be an object");
    ensure!(
        serde_json::to_vec(args)?.len() <= 65536,
        "Tool arguments exceed 64 KiB; split large edits into separate operations"
    );
    match name {
        "exec_command" => {
            let a: ExecArgs = serde_json::from_value(args.clone())?;
            if let Some(id) = &a.request_id {
                crate::journal::validate_id(id)?;
            }
            ensure!(
                !a.cmd.trim().is_empty() && a.cmd.len() <= 512 * 1024,
                "cmd must be nonempty and at most 512 KiB"
            );
            ensure!(
                (10..=10000).contains(&a.yield_time_ms),
                "yield_time_ms must be 10..10000"
            );
            ensure!(
                (1..=32768).contains(&a.max_output_tokens),
                "max_output_tokens must be 1..32768"
            );
            ensure!(
                a.timeout_ms.is_none_or(|ms| ms > 0 && ms <= 604800000),
                "timeout_ms must be 1..604800000"
            );
            ensure!(
                !a.tty || a.timeout_ms.is_none(),
                "timeout_ms is supported only with tty=false"
            );
            if let Some(path) = a.workdir {
                ensure!(
                    Path::new(&path).is_absolute(),
                    "workdir must be an absolute path such as /home/coder/workspace; relative paths are not resolved"
                );
            }
        }
        "write_stdin" => {
            let a: InputArgs = serde_json::from_value(args.clone())?;
            if let Some(id) = &a.request_id {
                crate::journal::validate_id(id)?;
            }
            ensure!(
                a.session_id.len() <= 120
                    && a.session_id.split('-').count() == 2
                    && a.session_id
                        .split('-')
                        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase())),
                "Invalid session_id: pass the session_id exactly as exec_command returned it (two lowercase letter groups joined by a hyphen). If the process already finished, call get_result with the original exec_command request_id"
            );
            ensure!(a.chars.len() <= 512 * 1024, "Input exceeds 512 KiB");
            ensure!(
                (1..=32768).contains(&a.max_output_tokens),
                "max_output_tokens must be 1..32768"
            );
            ensure!(
                a.wait_seconds == 0 || ((15..=35).contains(&a.wait_seconds) && a.chars.is_empty()),
                "wait_seconds must be 0, or 15..35 with empty chars"
            );
        }
        "read_file" => {
            let a: files::ReadArgs = serde_json::from_value(args.clone())?;
            ensure!(
                a.path.is_absolute()
                    && a.offset <= i64::MAX as u64
                    && (1..=262144).contains(&a.limit),
                "read_file needs an absolute path, offset >= 0 and limit 1..262144 bytes (or entries for a directory)"
            );
        }
        "apply_patch" => {
            let mut value = args.clone();
            if let Some(id) = value.as_object_mut().unwrap().remove("request_id") {
                crate::journal::validate_id(id.as_str().context("request_id must be a string")?)?;
            }
            // Option<String> otherwise treats an omitted field as a deletion.
            ensure!(
                value["changes"]
                    .as_array()
                    .is_some_and(|cs| cs.iter().all(|c| c.get("content").is_some())),
                "Each change must explicitly include content"
            );
            files::validate_patch(&serde_json::from_value(value)?)?;
        }
        "list_sessions" => {
            let a: SessionsArgs = serde_json::from_value(args.clone())?;
            ensure!((1..=100).contains(&a.limit), "limit must be 1..100");
            ensure!(
                ["all", "running", "exited", "lost"].contains(&a.status.as_str()),
                "status must be all, running, exited or lost"
            );
            if let Some(c) = &a.cursor {
                c.parse::<usize>().map_err(|_| {
                    anyhow::anyhow!(
                        "cursor must be a next_cursor value from a previous list_sessions page"
                    )
                })?;
            }
        }
        "get_result" => {
            let a: ResultArgs = serde_json::from_value(args.clone())?;
            ensure!((1..=100).contains(&a.limit), "limit must be 1..100");
            if let Some(id) = a.request_id {
                crate::journal::validate_id(&id)?;
            }
        }
        _ => bail!(
            "Unknown tool: {name}. Available tools: exec_command, write_stdin, list_sessions, read_file, apply_patch, get_result"
        ),
    }
    Ok(())
}

/// Advisory notes for risky command shapes. They never block: the server keeps
/// its no-allowlist contract, but the model gets a reminder with the result.
pub fn advisories(cmd: &str) -> Vec<&'static str> {
    let c = cmd.to_ascii_lowercase();
    let has = |needle: &str| c.contains(needle);
    let mut out = Vec::new();
    if (has("systemctl") || has("service "))
        && ["restart", "stop", "disable", "mask", "kill"]
            .iter()
            .any(|v| has(v))
    {
        out.push("Service control: running services may serve real users. Do this only if the user asked for it.");
    }
    if has("tofu apply") || has("terraform apply") || has("setup-live") {
        out.push("Deployment-style command: confirm the user asked for a live change; tests should use fresh ports and stores.");
    }
    if has("push --force") || has("push -f") || has("reset --hard") || has("clean -fd") {
        out.push(
            "Destructive git operation: confirm the target and that no unmerged work is lost.",
        );
    }
    let secret = [
        ".env",
        "secrets",
        "tunnel.token",
        "bootstrap.secret",
        "id_rsa",
        "id_ed25519",
        "auth.json",
    ];
    let reads = [
        "cat ", "less ", "head ", "tail ", "cp ", "base64", "printenv", "env |", "grep ",
    ];
    if secret.iter().any(|n| has(n)) && reads.iter().any(|v| has(v)) {
        out.push("Command touches a credential-like file. Do not print or copy secret values into output, chat or Git.");
    }
    out
}

pub fn call(state: &Path, name: &str, args: Value) -> Result<Value> {
    validate(name, &args)?;
    let receipt = match name {
        "exec_command" => {
            let a: ExecArgs = serde_json::from_value(args)?;
            let mut body = json!({"cmd":a.cmd,"tty":a.tty,"yield_time_ms":a.yield_time_ms,"max_output_tokens":a.max_output_tokens});
            if let Some(wd) = a.workdir {
                body["workdir"] = json!(wd);
            }
            if let Some(ms) = a.timeout_ms {
                body["timeout_ms"] = json!(ms);
            }
            let notes = advisories(&a.cmd);
            let mut flat = flatten(runtime::submit(
                state,
                name,
                &body,
                a.request_id.as_deref(),
                3,
            )?);
            if !notes.is_empty() {
                flat["warnings"] = json!(notes);
            }
            return Ok(flat);
        }
        "write_stdin" => {
            let a: InputArgs = serde_json::from_value(args)?;
            let mut body =
                json!({"session_id":a.session_id,"max_output_tokens":a.max_output_tokens});
            let tool = if a.wait_seconds > 0 {
                body["wait_seconds"] = json!(a.wait_seconds);
                "wait_for_exit"
            } else {
                body["chars"] = json!(a.chars);
                "write_stdin"
            };
            runtime::submit(state, tool, &body, a.request_id.as_deref(), 3)?
        }
        "apply_patch" => {
            let mut body = args;
            let id = body.as_object_mut().unwrap().remove("request_id");
            runtime::submit(state, name, &body, id.as_ref().and_then(Value::as_str), 3)?
        }
        "list_sessions" => {
            let a: SessionsArgs = serde_json::from_value(args)?;
            return Ok(page_sessions(&runtime::sessions(state)?, &a));
        }
        "read_file" => return files::read(serde_json::from_value(args)?),
        "get_result" => {
            let a: ResultArgs = serde_json::from_value(args)?;
            if let Some(id) = a.request_id {
                runtime::collect(state, &id, 0)?
            } else {
                let journal = Journal::new(state)?;
                let rows = journal
                    .rows(a.limit)?
                    .into_iter()
                    .map(|r| {
                        let mut v = journal.reconcile(r)?.receipt()?;
                        v.as_object_mut().unwrap().remove("result");
                        Ok(v)
                    })
                    .collect::<Result<Vec<_>>>()?;
                return Ok(json!({"requests":rows}));
            }
        }
        _ => unreachable!(),
    };
    Ok(flatten(receipt))
}

pub fn wrapped(value: Value) -> Value {
    json!({"resultType":"complete","isError":value["isError"]==true || matches!(value["status"].as_str(),Some("error"|"unknown")),"content":[{"type":"text","text":value.to_string()}],"structuredContent":value})
}
pub fn info() -> Value {
    json!({"name":"lkjyolo","version":VERSION,"title":"lkjyolo"})
}
