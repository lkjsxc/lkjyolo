#![cfg(target_os = "linux")]
//! Explicit opt-in real engine suite: cargo test --test engine -- --ignored.
use base64::Engine as _;
use lkjyolo::{
    engine::Engine,
    journal::Journal,
    runtime::result_body,
    util::{fingerprint, wait_child},
};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    dir: tempfile::TempDir,
    engine: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let engine = std::env::var_os("LKJYOLO_TEST_ENGINE")
            .expect("Set LKJYOLO_TEST_ENGINE to pinned 0.2.1 executable")
            .into();
        let value = Self {
            dir: tempfile::Builder::new()
                .prefix("lkjyolo-rust-test-")
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir_in("/tmp")
                .unwrap(),
            engine,
        };
        value.run(&[
            "setup",
            "--engine",
            value.engine.to_str().unwrap(),
            "--workspace",
            value.path().to_str().unwrap(),
        ]);
        value
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lkjyolo"));
        command
            .arg("--state")
            .arg(self.path())
            .args(args)
            .current_dir("/tmp");
        command
    }
    fn execute(&self, args: &[&str], success: bool) -> Value {
        // A file avoids pipe deadlock when testing recovered large output.
        let output = tempfile::NamedTempFile::new_in(self.path()).unwrap();
        let err = tempfile::NamedTempFile::new_in(self.path()).unwrap();
        let mut child = self
            .command(args)
            .stdout(output.reopen().unwrap())
            .stderr(err.reopen().unwrap())
            .spawn()
            .unwrap();
        let status = wait_child(&mut child, 55).unwrap();
        let stdout = fs::read_to_string(output.path()).unwrap();
        let stderr = fs::read_to_string(err.path()).unwrap();
        assert_eq!(status.success(), success, "{args:?}\n{stdout}\n{stderr}");
        serde_json::from_str(&stdout).unwrap()
    }
    fn run(&self, args: &[&str]) -> Value {
        self.execute(args, true)
    }
    fn body(&self, args: &[&str]) -> Value {
        let result = self.run(args);
        assert_eq!(result["status"], "done", "{result}");
        result_body(&result["result"])
    }
    fn stop(&self) {
        if let Some(identity) = Engine::new(self.path()).unwrap().identity() {
            unsafe {
                libc::kill(identity.pid as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while fingerprint(identity.pid).is_some() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if fingerprint(identity.pid).is_some() {
                unsafe {
                    libc::kill(identity.pid as i32, libc::SIGKILL);
                }
            }
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn quota_fixture(limit: usize) -> Fixture {
    let f = Fixture::new();
    let path = f.path().join("engine.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["max_exec_continuations"] = json!(limit);
    fs::write(path, config.to_string()).unwrap();
    f
}

fn await_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists());
    // The marker precedes shell exit; let pipe capture finish as well.
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
#[ignore = "requires Linux execution engine"]
fn quota_recovery_preserves_unread_output_and_replays_without_duplicate_execution() {
    let f = quota_fixture(1);
    let command = "sleep .2; printf UNREAD; touch finished";
    let start = f.body(&[
        "exec",
        "--request-id",
        "forgotten",
        "--yield-ms",
        "10",
        command,
    ]);
    let sid = start["session_id"].as_str().unwrap();
    assert!(sid.starts_with("lkj-"));
    await_file(&f.path().join("finished"));
    let before = f.run(&["doctor"])["daemon"].clone();
    let args = [
        "exec",
        "--request-id",
        "after-quota",
        "printf X >> counter; printf RECOVERED",
    ];
    assert_eq!(f.body(&args)["output"], "RECOVERED");
    f.run(&args);
    assert_eq!(fs::read_to_string(f.path().join("counter")).unwrap(), "X");
    assert_eq!(f.run(&["doctor"])["daemon"], before);
    let end = f.body(&["poll", "--request-id", "late-poll", sid]);
    assert_eq!(end["exit_code"], 0);
    assert_eq!(end["output"], "UNREAD");
    assert_eq!(f.body(&["poll", "--request-id", "late-poll", sid]), end);
    assert_eq!(f.body(&["poll", sid])["output"], "");
    let rejected = f.execute(&["input", sid, "touch should-not-exist\n"], false);
    assert_eq!(rejected["result"]["execution_may_have_started"], false);
    assert!(!f.path().join("should-not-exist").exists());
}

#[test]
#[ignore = "requires Linux execution engine"]
fn concurrent_admission_recovers_once_and_archived_handle_cannot_target_reused_id() {
    let f = quota_fixture(1);
    let start = f.body(&[
        "exec",
        "--yield-ms",
        "10",
        "sleep .2; printf OLD; touch finished",
    ]);
    let sid = start["session_id"].as_str().unwrap();
    await_file(&f.path().join("finished"));
    std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..4)
            .map(|i| {
                let f = &f;
                scope.spawn(move || {
                    f.body(&[
                        "exec",
                        "--request-id",
                        &format!("parallel-{i}"),
                        &format!("printf X >> counter-{i}"),
                    ])
                })
            })
            .collect();
        for job in jobs {
            assert_eq!(job.join().unwrap()["exit_code"], 0);
        }
    });
    for i in 0..4 {
        assert_eq!(
            fs::read_to_string(f.path().join(format!("counter-{i}"))).unwrap(),
            "X"
        );
    }
    let current = f.body(&["exec", "--tty", "/bin/bash"]);
    let current_id = current["session_id"].as_str().unwrap();
    let journal = lkjyolo::journal::Journal::new(f.path()).unwrap();
    // Model upstream's permitted reuse of the old human-readable ID. The
    // archived logical handle must not send input or polling to this live shell.
    journal.db.execute("UPDATE managed_sessions SET engine_id=(SELECT engine_id FROM managed_sessions WHERE handle=?) WHERE handle=?", rusqlite::params![current_id,sid]).unwrap();
    assert_eq!(f.body(&["poll", sid])["output"], "OLD");
    f.execute(&["input", sid, "touch wrong-session\n"], false);
    assert!(!f.path().join("wrong-session").exists());
    assert!(
        f.body(&["input", current_id, "printf CURRENT\n"])["output"]
            .as_str()
            .unwrap()
            .contains("CURRENT")
    );
    f.body(&["input", current_id, "exit\n"]);
}

#[test]
#[ignore = "requires Linux execution engine"]
fn quota_recovery_keeps_live_shell_and_full_truncated_log() {
    let f = quota_fixture(2);
    let shell = f.body(&["exec", "--tty", "/bin/bash"]);
    let shell_id = shell["session_id"].as_str().unwrap();
    let start = f.body(&[
        "exec",
        "--yield-ms",
        "10",
        "sleep .2; awk 'BEGIN {for(i=0;i<100000;i++) printf \"z\"}'; touch finished",
    ]);
    let sid = start["session_id"].as_str().unwrap();
    await_file(&f.path().join("finished"));
    assert_eq!(f.body(&["exec", "printf NEW"])["output"], "NEW");
    let end = f.body(&["poll", sid]);
    assert_eq!(end["exit_code"], 0);
    assert_eq!(end["output_truncated"], true);
    let raw = f.run(&[
        "log",
        end["output_ref"]["path"].as_str().unwrap(),
        "--limit",
        "200000",
    ]);
    assert_eq!(raw["text"], "z".repeat(100000));
    let output = f.body(&["input", shell_id, "printf STILL_ALIVE\n"]);
    assert!(output["output"].as_str().unwrap().contains("STILL_ALIVE"));
    assert_eq!(f.body(&["input", shell_id, "exit\n"])["exit_code"], 0);
}

#[test]
#[ignore = "requires Linux execution engine"]
fn archived_output_respects_later_smaller_budget_without_losing_utf8() {
    let f = quota_fixture(1);
    let start = f.body(&[
        "exec",
        "--yield-ms",
        "10",
        "sleep .2; printf 'あいうえお'; touch finished",
    ]);
    let sid = start["session_id"].as_str().unwrap();
    await_file(&f.path().join("finished"));
    f.body(&["exec", "true"]);
    let mut output = String::new();
    for _ in 0..10 {
        let part = f.body(&["poll", "--max-output-tokens", "1", sid]);
        let text = part["output"].as_str().unwrap();
        assert!(text.len() <= 4);
        output.push_str(text);
        if part["exit_code"].is_number() {
            assert_eq!(part["exit_code"], 0);
            assert_eq!(output, "あいうえお");
            return;
        }
        assert_eq!(part["session_id"], sid);
    }
    panic!("buffered output did not finish");
}

#[test]
#[ignore = "requires Linux execution engine"]
fn all_live_quota_is_reported_without_killing_or_replaying_commands() {
    let f = quota_fixture(1);
    let start = f.body(&["exec", "--tty", "/bin/bash"]);
    let failed = f.execute(&["exec", "touch unexpected"], false);
    assert!(failed.to_string().contains("quota reached"));
    assert!(!f.path().join("unexpected").exists());
    let sid = start["session_id"].as_str().unwrap();
    let output = f.body(&["input", sid, "printf ALIVE\n"]);
    assert!(output["output"].as_str().unwrap().contains("ALIVE"));
    f.body(&["input", sid, "exit\n"]);
}

#[test]
#[ignore = "requires Linux execution engine"]
fn quota_recovery_imports_legacy_sessions_without_engine_restart() {
    let f = quota_fixture(1);
    let engine = Engine::new(f.path()).unwrap();
    let identity = engine.ensure().unwrap();
    let journal = lkjyolo::journal::Journal::new(f.path()).unwrap();
    let args = json!({"cmd":"sleep .2; printf LEGACY; touch finished","yield_time_ms":10});
    journal
        .prepare("pre-upgrade", "exec_command", &args)
        .unwrap();
    journal
        .generation("pre-upgrade", &identity.generation)
        .unwrap();
    let value = engine
        .rpc(
            "tools/call",
            json!({"name":"exec_command","arguments":args}),
            10,
        )
        .unwrap();
    journal.finish("pre-upgrade", "done", &value).unwrap();
    let body = result_body(&value);
    let sid = body["session_id"].as_str().unwrap();
    await_file(&f.path().join("finished"));
    assert_eq!(f.body(&["exec", "printf UPGRADED"])["output"], "UPGRADED");
    assert_eq!(f.body(&["poll", sid])["output"], "LEGACY");
    assert_eq!(engine.identity().unwrap().generation, identity.generation);
}

#[test]
#[ignore = "requires Linux execution engine"]
fn uncertain_liveness_buffers_output_without_interrupting_process() {
    let f = quota_fixture(1);
    let start = f.body(&["exec", "--yield-ms", "10", "exec env -u CHATGPT_EXEC_SESSION sh -c 'sleep .2; printf BUFFERED; touch ready; while [ ! -e finish ]; do sleep .1; done; printf FINAL'"]);
    let sid = start["session_id"].as_str().unwrap();
    await_file(&f.path().join("ready"));
    let failed = f.execute(&["exec", "touch unexpected"], false);
    assert!(failed.to_string().contains("quota reached"));
    let journal = lkjyolo::journal::Journal::new(f.path()).unwrap();
    let pending: Option<String> = journal
        .db
        .query_row(
            "SELECT pending FROM managed_sessions WHERE handle=?",
            [sid],
            |r| r.get(0),
        )
        .unwrap();
    assert!(pending.unwrap().contains("BUFFERED"));
    f.execute(&["input", sid, "touch wrong-input\n"], false);
    fs::write(f.path().join("finish"), "").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(f.body(&["exec", "printf RETRIED"])["output"], "RETRIED");
    let pending = f.body(&["poll", sid]);
    assert_eq!(pending["output"], "BUFFERED");
    assert_eq!(pending["session_id"], sid);
    let end = f.body(&["wait", "--seconds", "15", sid]);
    assert_eq!(end["exit_code"], 0);
    assert_eq!(end["output"], "FINAL");
}

#[test]
#[ignore = "requires Linux execution engine"]
fn direct_mcp_exec_file_edit_replay_and_recovery() {
    let f = Fixture::new();
    let invoke = |name: &str, args: Value| {
        let payload = base64::engine::general_purpose::STANDARD
            .encode(json!({"name":name,"arguments":args}).to_string());
        f.run(&["mcp-call", "--json-base64", &payload])
    };
    let path = f.path().join("mcp-file");
    let patch = json!({"request_id":"mcp-patch","changes":[{"path":path,"before_sha256":"absent","content":"hello\n"}]});
    let first = invoke("apply_patch", patch.clone());
    assert_eq!(first["status"], "done", "{first}");
    assert_eq!(invoke("apply_patch", patch), first);
    let read = invoke("read_file", json!({"path":path}));
    assert_eq!(read["text"], "hello\n");
    assert_eq!(read["sha256"], lkjyolo::util::sha256(b"hello\n"));
    let args = json!({"request_id":"mcp-exec","cmd":"printf DIRECT; sleep 1; printf DONE","yield_time_ms":10});
    let result = invoke("exec_command", args.clone());
    assert_eq!(result["status"], "done", "{result}");
    assert_eq!(invoke("exec_command", args), result);
    assert_eq!(
        invoke("get_result", json!({"request_id":"mcp-exec"})),
        result
    );
    let sid = result["session_id"].as_str().unwrap();
    let wait = invoke(
        "write_stdin",
        json!({"request_id":"mcp-wait","session_id":sid,"wait_seconds":15}),
    );
    let wait = if wait["status"] == "running" {
        lkjyolo::mcp::flatten(f.run(&["result", "mcp-wait", "--wait", "20"]))
    } else {
        wait
    };
    assert_eq!(wait["exit_code"], 0, "{wait}");
    assert!(wait["output"].as_str().unwrap().contains("DONE"));
    let recent = invoke("get_result", json!({}));
    assert!(
        recent["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["request_id"] == "mcp-patch")
    );
}

#[test]
#[ignore = "requires Linux execution engine"]
fn command_exit_environment_and_arbitrary_workdir() {
    let f = Fixture::new();
    let result = f.body(&[
        "exec",
        "--cwd",
        "/tmp",
        "printf '%s\\n' \"$HOME\"; pwd; exit 7",
    ]);
    assert_eq!(result["exit_code"], 7);
    assert!(result["output"].as_str().unwrap().contains("/tmp"));
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains(&std::env::var("HOME").unwrap())
    );
}

#[test]
#[ignore = "requires Linux execution engine"]
fn disconnected_client_replays_without_duplicate_execution() {
    let f = Fixture::new();
    let counter = f.path().join("counter");
    let command = format!(
        "printf X >> {}; sleep 1; printf COMPLETE",
        counter.display()
    );
    let args = [
        "exec",
        "--request-id",
        "disconnected",
        "--yield-ms",
        "4000",
        &command,
    ];
    let mut client = f
        .command(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !counter.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(counter.exists());
    client.kill().unwrap();
    client.wait().unwrap();
    let receipt = f.run(&args);
    assert_eq!(receipt["result"]["structuredContent"]["output"], "COMPLETE");
    assert_eq!(fs::read_to_string(counter).unwrap(), "X");
    assert_eq!(f.run(&args), receipt);
}

#[test]
#[ignore = "requires Linux execution engine"]
fn concurrent_retries_only_dispatch_once() {
    let f = Fixture::new();
    let counter = f.path().join("counter");
    let command = format!("printf X >> {}; sleep .3; printf READY", counter.display());
    let args = ["exec", "--request-id", "concurrent", &command];
    let values = std::thread::scope(|scope| {
        let mut jobs = Vec::new();
        for _ in 0..6 {
            jobs.push(scope.spawn(|| f.run(&args)));
        }
        jobs.into_iter()
            .map(|j| j.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(values.iter().all(|v| *v == values[0]));
    assert_eq!(fs::read_to_string(counter).unwrap(), "X");
    assert_eq!(
        f.execute(&["exec", "--request-id", "concurrent", "false"], false)["isError"],
        true
    );
}

#[test]
#[ignore = "requires Linux execution engine"]
fn interactive_shell_retains_state_and_accepts_stdin() {
    let f = Fixture::new();
    let start = f.body(&["start"]);
    let sid = start["session_id"].as_str().unwrap();
    f.body(&["input", sid, "export LKJYOLO_CHECK=retained; cd /tmp\n"]);
    let result = f.body(&[
        "input",
        sid,
        "printf 'STATE=%s\\n' \"$LKJYOLO_CHECK\"; pwd\n",
    ]);
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("STATE=retained")
    );
    assert!(result["output"].as_str().unwrap().contains("/tmp"));
    assert_eq!(f.body(&["input", sid, "exit\n"])["exit_code"], 0);
}

#[test]
#[ignore = "requires Linux execution engine"]
fn background_wait_and_history() {
    let f = Fixture::new();
    let start = f.body(&["exec", "--yield-ms", "10", "sleep .3; printf FINISHED"]);
    let end = f.body(&[
        "wait",
        "--seconds",
        "15",
        start["session_id"].as_str().unwrap(),
    ]);
    assert_eq!(end["exit_code"], 0);
    assert_eq!(end["output"], "FINISHED");
    assert_eq!(f.run(&["sessions"])["sessions"][0]["status"], "exited");
    let recent = f.run(&["requests"]);
    assert_eq!(recent["requests"].as_array().unwrap().len(), 2);
    assert!(!recent.to_string().contains("cmd"));
}

#[test]
#[ignore = "requires Linux execution engine"]
fn ctrl_c_interrupts_process() {
    let f = Fixture::new();
    let start = f.body(&["exec", "--yield-ms", "10", "sleep 60"]);
    let sid = start["session_id"].as_str().unwrap();
    let mut end = f.body(&["interrupt", sid]);
    if end.get("exit_code").is_none() {
        end = f.body(&["wait", "--seconds", "15", sid]);
    }
    assert_ne!(end["exit_code"], 0);
    assert!(end["exit_code"].is_number());
}

#[test]
#[ignore = "requires Linux execution engine"]
fn long_output_is_recovered_exactly_without_python() {
    let f = Fixture::new();
    let result = f.body(&[
        "exec",
        "--max-output-tokens",
        "40",
        "awk 'BEGIN { for (i=0;i<10000;i++) printf \"0123456789\"; printf \"\\n\" }'",
    ]);
    assert_eq!(result["output_truncated"], true);
    let log = f.run(&[
        "log",
        result["output_ref"]["path"].as_str().unwrap(),
        "--limit",
        "200000",
    ]);
    assert_eq!(log["text"], format!("{}\n", "0123456789".repeat(10000)));
    assert_eq!(log["eof"], true);
}

#[test]
#[ignore = "requires Linux execution engine"]
fn invalid_tool_arguments_return_error() {
    let f = Fixture::new();
    let value = f.execute(
        &[
            "call",
            "exec_command",
            "--json",
            r#"{"cmd":"true","unknown_field":1}"#,
        ],
        false,
    );
    assert_eq!(value["status"], "error");
    assert_eq!(value["result"]["isError"], true);
}

#[test]
#[ignore = "requires Linux execution engine"]
fn setup_keeps_active_engine_and_private_socket() {
    let f = Fixture::new();
    f.body(&["exec", "true"]);
    let before = f.run(&["doctor"])["daemon"].clone();
    let result = f.run(&[
        "setup",
        "--engine",
        f.engine.to_str().unwrap(),
        "--workspace",
        f.path().to_str().unwrap(),
    ]);
    assert_eq!(result["unchanged"], true);
    assert_eq!(f.run(&["doctor"])["daemon"], before);
    assert_eq!(
        fs::metadata(f.path().join("mcp.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
#[ignore = "requires Linux execution engine"]
fn engine_restart_refuses_old_session_input() {
    let f = Fixture::new();
    let start = f.body(&["start"]);
    let sid = start["session_id"].as_str().unwrap();
    f.stop();
    let value = f.execute(&["input", sid, "printf UNEXPECTED\n"], false);
    assert_eq!(value["status"], "error");
    assert_eq!(value["result"]["execution_may_have_started"], false);
    assert!(
        value["result"]["error"]
            .as_str()
            .unwrap()
            .contains("stopped/restarted")
    );
    assert_eq!(
        f.run(&["sessions"])["sessions"].as_array().unwrap().len(),
        1
    );
}

#[test]
#[ignore = "requires Linux execution engine"]
fn unknown_session_is_rejected_before_dispatch() {
    let f = Fixture::new();
    let value = f.execute(&["input", "amber-river", "printf UNEXPECTED\n"], false);
    assert_eq!(value["status"], "error");
    assert_eq!(value["result"]["execution_may_have_started"], false);
}

#[test]
#[ignore = "requires Linux execution engine"]
fn native_worker_launches_through_symlink_without_python_on_path() {
    let f = Fixture::new();
    let link = f.path().join("lkjyolo");
    symlink(env!("CARGO_BIN_EXE_lkjyolo"), &link).unwrap();
    let output = Command::new(&link)
        .arg("--state")
        .arg(f.path())
        .args(["exec", "--request-id", "native", "printf NATIVE"])
        .env("PATH", "/nonexistent")
        .current_dir("/")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["result"]["structuredContent"]["output"], "NATIVE");
}

#[test]
#[ignore = "requires Linux execution engine"]
fn legacy_receipt_is_available_through_native_cli() {
    let f = Fixture::new();
    let journal = lkjyolo::journal::Journal::new(f.path()).unwrap();
    let old: Value =
        serde_json::from_str(include_str!("fixtures/python-v0.1-receipt.json")).unwrap();
    journal.db.execute("INSERT INTO requests(id,digest,tool,arguments,status,created,updated,generation,result) VALUES(?,?,?,?,?,?,?,?,?)",
        rusqlite::params![old["id"].as_str(),old["digest"].as_str(),old["tool"].as_str(),old["arguments"].as_str(),old["status"].as_str(),old["created"].as_f64(),old["updated"].as_f64(),old["generation"].as_str(),old["result"].as_str()]).unwrap();
    let receipt = f.run(&[
        "call",
        "exec_command",
        "--request-id",
        "legacy-replay",
        "--json",
        old["arguments"].as_str().unwrap(),
    ]);
    assert_eq!(
        receipt["result"]["structuredContent"]["output"],
        "SAVED_BY_PYTHON\n"
    );
    assert!(!Engine::new(f.path()).unwrap().ready());
    assert_eq!(receipt, f.run(&["result", "legacy-replay"]));
}

#[test]
#[ignore = "requires Linux execution engine"]
fn running_legacy_worker_identity_is_not_marked_unknown() {
    let f = Fixture::new();
    let j = lkjyolo::journal::Journal::new(f.path()).unwrap();
    j.prepare("legacy-running", "exec_command", &json!({"cmd":"true"}))
        .unwrap();
    let mut worker = Command::new("sleep").arg("10").spawn().unwrap();
    j.db.execute("UPDATE requests SET status='running',worker_pid=?,worker_fingerprint=? WHERE id='legacy-running'",
        rusqlite::params![worker.id(),fingerprint(worker.id())]).unwrap();
    let result = f.run(&["result", "legacy-running"]);
    worker.kill().unwrap();
    worker.wait().unwrap();
    assert_eq!(result["status"], "running");
    assert_eq!(f.run(&["requests"])["requests"][0]["status"], "unknown");
}

#[test]
#[ignore = "requires Linux execution engine"]
fn serialized_wait_and_poll_keep_the_last_observed_exit() {
    let f = Fixture::new();
    let gate = f.path().join("finish");
    let start = f.body(&[
        "exec",
        "--yield-ms",
        "10",
        "while [ ! -e finish ]; do sleep 0.02; done; exit 7",
    ]);
    let sid = start["session_id"].as_str().unwrap();
    let waiting = f.run(&[
        "wait",
        "--request-id",
        "overlap-wait",
        "--client-wait",
        "0",
        "--seconds",
        "15",
        sid,
    ]);
    assert!(matches!(
        waiting["status"].as_str(),
        Some("queued" | "running")
    ));
    let journal = Journal::new(f.path()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while journal.get("overlap-wait").unwrap().generation.is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    // Observe the held lock instead of relying on worker scheduling delays.
    let lock_path = f
        .path()
        .join("session-locks")
        .join(format!("{}.lock", lkjyolo::util::sha256(sid.as_bytes())));
    loop {
        assert!(Instant::now() < deadline);
        if let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
                Ok(()) => fs2::FileExt::unlock(&file).unwrap(),
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let polling = f.run(&[
        "poll",
        "--request-id",
        "overlap-poll",
        "--client-wait",
        "0",
        sid,
    ]);
    assert!(matches!(
        polling["status"].as_str(),
        Some("queued" | "running")
    ));

    // Session operations are serialized, so release the process before
    // collecting the queued poll. Both callers must observe the same exit.
    fs::write(gate, b"finish").unwrap();
    let finished = f.run(&["result", "overlap-wait", "--wait", "45"]);
    assert_eq!(finished["status"], "done");
    assert_eq!(result_body(&finished["result"])["exit_code"], 7);
    let polled = f.run(&["result", "overlap-poll", "--wait", "45"]);
    assert_eq!(polled["status"], "done");
    assert_eq!(result_body(&polled["result"])["exit_code"], 7);
    let journal = Journal::new(f.path()).unwrap();
    let wait = journal.get("overlap-wait").unwrap();
    let poll = journal.get("overlap-poll").unwrap();
    assert!(wait.created < poll.created);
    assert!(wait.updated <= poll.updated);

    let observed = f.run(&["sessions"]);
    let session = &observed["sessions"][0];
    assert_eq!(session["status"], "exited");
    assert_eq!(session["exit_code"], 7);
    assert_eq!(session["last_request_id"], "overlap-poll");
    // Compare timestamps after the same public JSON serialization boundary.
    assert_eq!(session["last_seen_at"], polled["updated_at"]);
}
