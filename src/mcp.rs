//! A deliberately small development interface, shared by HTTP and CLI transports.
use crate::{VERSION, files, journal::Journal, runtime};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

pub const VERSIONS: &[&str] = &["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26"];
pub const INSTRUCTIONS: &str = "Use lkjyolo for coding, debugging, tests, builds, installations and Linux work when no other environment is specified. The default workspace is already selected. Execute and verify requested work autonomously using the workspace's OS permissions, sudo, Docker, network and existing GitHub authentication. No command approval prompts or allowlists are added by this server. Use a unique request_id for every exec, input and patch. After disconnect, call get_result with that same ID; never repeat an unknown mutation with a new ID. queued/running means retrieve the receipt; a session_id means use write_stdin. done means a saved response, not command success: inspect exit_code. Read output_ref.path using read_file for truncated logs. Processes survive disconnects but not workspace/engine restarts. Respect the user's chosen environment and scope; do not reveal credentials. Explanation-only requests need no execution.";

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
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,"idempotentHint":true,"openWorldHint":true}})
}
pub fn tools() -> Value {
    let id = string(
        "Unique operation ID, e.g. project-build-<random suffix>. Reuse this exact ID and arguments after a transport failure; do not invent another ID to retry uncertain work.",
    );
    json!([
        tool(
            "exec_command",
            "Run any shell command in your persistent Linux development workspace. Use for coding, tests, builds, dependency installation, Git, sudo and Docker. Long-running processes return a session_id and keep running after disconnect. No command allowlist or approval prompt.",
            json!({"request_id":id,"cmd":string("Shell command. Use /bin/bash with tty=true for a persistent interactive shell."),"workdir":string("Absolute working directory. Default: configured workspace root."),"tty":{"type":"boolean","default":false},"yield_time_ms":integer(1000,10,10000),"max_output_tokens":integer(4096,1,32768)}),
            &["request_id", "cmd"],
            false
        ),
        tool(
            "write_stdin",
            "Continue a running session: send input, read new output, wait for completion, or send Ctrl-C (chars=\\u0003). Use a fresh request_id for each distinct input or poll. Set wait_seconds to 15..35 with empty chars for a longer wait.",
            json!({"request_id":id,"session_id":string("Session ID returned by exec_command"),"chars":{"type":"string","default":""},"wait_seconds":integer(0,0,35),"max_output_tokens":integer(4096,1,32768)}),
            &["request_id", "session_id"],
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
            "Create, replace or delete text files with stale-write protection. Read first and supply before_sha256; use absent for a new file, content=null for deletion. Parents must exist. All hashes are preflighted; individual replacements are atomic, but a batch is not a transaction. Symlink targets are rejected. Use exec_command for other file operations.",
            json!({"request_id":id,"changes":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"object","properties":{"path":string("Absolute file path"),"before_sha256":string("SHA-256 from read_file, or absent"),"content":{"type":["string","null"],"description":"Complete replacement text, or null to delete"}},"required":["path","before_sha256","content"],"additionalProperties":false}}}),
            &["request_id", "changes"],
            false
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
    request_id: String,
    cmd: String,
    workdir: Option<String>,
    #[serde(default)]
    tty: bool,
    #[serde(default = "yield_ms")]
    yield_time_ms: u64,
    #[serde(default = "tokens")]
    max_output_tokens: u64,
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
    request_id: String,
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
            crate::journal::validate_id(&a.request_id)?;
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
            if let Some(path) = a.workdir {
                ensure!(Path::new(&path).is_absolute(), "workdir must be absolute");
            }
        }
        "write_stdin" => {
            let a: InputArgs = serde_json::from_value(args.clone())?;
            crate::journal::validate_id(&a.request_id)?;
            ensure!(
                a.session_id.len() <= 120
                    && a.session_id.split('-').count() == 2
                    && a.session_id
                        .split('-')
                        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase())),
                "Invalid session_id"
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
                "Invalid path, offset or limit"
            );
        }
        "apply_patch" => {
            let mut value = args.clone();
            let id = value
                .as_object_mut()
                .unwrap()
                .remove("request_id")
                .context("request_id required")?;
            crate::journal::validate_id(id.as_str().context("request_id must be a string")?)?;
            // Option<String> otherwise treats an omitted field as a deletion.
            ensure!(
                value["changes"]
                    .as_array()
                    .is_some_and(|cs| cs.iter().all(|c| c.get("content").is_some())),
                "Each change must explicitly include content"
            );
            files::validate_patch(&serde_json::from_value(value)?)?;
        }
        "get_result" => {
            let a: ResultArgs = serde_json::from_value(args.clone())?;
            ensure!((1..=100).contains(&a.limit), "limit must be 1..100");
            if let Some(id) = a.request_id {
                crate::journal::validate_id(&id)?;
            }
        }
        _ => bail!("Unknown tool: {name}"),
    }
    Ok(())
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
            runtime::submit(state, name, &body, Some(&a.request_id), 3)?
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
            runtime::submit(state, tool, &body, Some(&a.request_id), 3)?
        }
        "apply_patch" => {
            let mut body = args;
            let id = body.as_object_mut().unwrap().remove("request_id").unwrap();
            runtime::submit(state, name, &body, id.as_str(), 3)?
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
