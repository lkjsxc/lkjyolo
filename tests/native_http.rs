#![cfg(target_os = "linux")]
//! Real HTTP transport, compiled CLI, detached workers, SQLite and native pipes.
use lkjyolo::{engine::Settings, journal::Journal, runtime, util};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Server {
    child: Child,
    client: Client,
    url: String,
    _dir: tempfile::TempDir,
}

#[test]
fn duplicate_workers_exit_while_request_lock_is_held() {
    let server = Server::new();
    let state = server._dir.path().join("state");
    let j = Journal::new(&state).unwrap();
    let args = json!({
        "cmd":"printf x >> counter; while [ ! -f release ]; do sleep .02; done",
        "yield_time_ms":10, "timeout_ms":10000
    });
    j.prepare("duplicate-worker", "exec_command", &args)
        .unwrap();
    let locks = state.join("request-locks");
    util::private_dir(&locks).unwrap();
    let guard = util::lock(&locks.join("duplicate-worker.lock")).unwrap();
    let worker = || {
        Command::new(
            std::env::var_os("LKJYOLO_NATIVE_TEST_BINARY")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_lkjyolo").into()),
        )
        .arg("--state")
        .arg(&state)
        .arg("_worker")
        .arg("duplicate-worker")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
    };
    // Model two submitters that both observed queued, even before the winner
    // has marked it running. The loser must not queue behind the request lock.
    let mut duplicate = worker();
    assert!(util::wait_child(&mut duplicate, 1).unwrap().success());
    assert_eq!(j.get("duplicate-worker").unwrap().status, "queued");
    drop(guard);

    let mut owner = worker();
    let first = runtime::collect(&state, "duplicate-worker", 1).unwrap();
    assert_eq!(first["status"], "done");
    assert_eq!(first["result"]["structuredContent"]["finished"], false);
    assert!(owner.try_wait().unwrap().is_none());
    let mut duplicate = worker();
    assert!(util::wait_child(&mut duplicate, 1).unwrap().success());
    assert!(owner.try_wait().unwrap().is_none());
    fs::write(server._dir.path().join("release"), "").unwrap();
    assert!(util::wait_child(&mut owner, 4).unwrap().success());
    assert_eq!(j.get("duplicate-worker").unwrap().receipt().unwrap(), first);
    assert_eq!(fs::read(server._dir.path().join("counter")).unwrap(), b"x");
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("native-http-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let state = dir.path().join("state");
        util::private_dir(&state).unwrap();
        util::write_json(
            &state.join("settings.json"),
            &Settings {
                version: 1,
                engine: "/engine-must-not-be-started".into(),
                engine_sha256: String::new(),
                workspace: dir.path().to_owned(),
                environment: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            },
        )
        .unwrap();
        let token = dir.path().join("token");
        util::atomic_write(
            &token,
            b"native-http-test-token-01234567890123456789",
            0o600,
        )
        .unwrap();
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        let origin = format!("http://{addr}");
        let config = dir.path().join("server.json");
        util::write_json(
            &config,
            &json!({"listen":addr.to_string(),"public_origin":origin,
            "backend":"local","state":state,"token_file":token}),
        )
        .unwrap();
        let log = fs::File::create(dir.path().join("server.log")).unwrap();
        drop(socket);
        let binary = std::env::var_os("LKJYOLO_NATIVE_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_lkjyolo").into());
        let child = Command::new(binary)
            .arg("serve")
            .arg("--config")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            url: format!("{origin}/lkjyolo/mcp"),
            _dir: dir,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if server
                .client
                .post(&server.url)
                .bearer_auth("native-http-test-token-01234567890123456789")
                .header("accept", "application/json, text/event-stream")
                .json(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
                .send()
                .is_ok()
            {
                break;
            }
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(server._dir.path().join("server.log")).unwrap()
            );
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        server
    }
    fn call(&self, name: &str, args: Value) -> Value {
        let response: Value = self.client.post(&self.url).bearer_auth("native-http-test-token-01234567890123456789")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}))
            .send().unwrap().error_for_status().unwrap().json().unwrap();
        assert!(response["error"].is_null(), "{response}");
        response["result"]["structuredContent"].clone()
    }
    fn saved(&self, mut value: Value) -> Value {
        let deadline = Instant::now() + Duration::from_secs(8);
        while matches!(value["status"].as_str(), Some("queued" | "running")) {
            assert!(Instant::now() < deadline, "{value}");
            std::thread::sleep(Duration::from_millis(20));
            value = self.call("get_result", json!({"request_id":value["request_id"]}));
        }
        assert_eq!(value["status"], "done", "{value}");
        value
    }
}

#[test]
fn http_exec_streams_refs_replay_background_cancel_timeout_and_warnings() {
    let server = Server::new();
    let args = json!({"request_id":"separate","cmd":"printf out; printf err >&2; exit 7"});
    let first = server.saved(server.call("exec_command", args.clone()));
    assert_eq!(first["stdout"], "out");
    assert_eq!(first["stderr"], "err");
    assert_eq!(first["exit_code"], 7);
    assert_eq!(first["signal"], Value::Null);
    assert_eq!(first["stream_mode"], "pipes");
    assert_eq!(first["finished"], true);
    assert!(first.get("output").is_none());
    assert_eq!(server.call("exec_command", args), first);
    assert_eq!(
        server.call("get_result", json!({"request_id":"separate"})),
        first
    );
    let conflict = server.call(
        "exec_command",
        json!({"request_id":"separate","cmd":"touch must-not-exist"}),
    );
    assert_eq!(conflict["isError"], true);
    assert!(!server._dir.path().join("must-not-exist").exists());

    let duplicate =
        json!({"request_id":"concurrent","cmd":"printf x >> counter; sleep .1; printf once"});
    std::thread::scope(|scope| {
        let first = scope.spawn(|| server.saved(server.call("exec_command", duplicate.clone())));
        let second = scope.spawn(|| server.saved(server.call("exec_command", duplicate.clone())));
        assert_eq!(first.join().unwrap(), second.join().unwrap());
    });
    assert_eq!(fs::read(server._dir.path().join("counter")).unwrap(), b"x");

    let large = server.saved(server.call("exec_command",json!({"request_id":"large","cmd":"head -c 200000 /dev/zero & head -c 200000 /dev/zero >&2 & wait","max_output_tokens":4,"yield_time_ms":10000})));
    assert_eq!(large["exit_code"], 0);
    assert_eq!(large["truncated"], true);
    for name in ["stdout", "stderr"] {
        assert_eq!(large[name].as_str().unwrap().len(), 8);
        let r = &large[format!("{name}_ref")];
        let tail = server.call(
            "read_file",
            json!({"path":r["path"],"offset":r["offset"],"limit":262144}),
        );
        assert_eq!(tail["text"].as_str().unwrap().as_bytes(), vec![0; 199992]);
        assert_eq!(tail["eof"], true);
    }
    let binary = server.saved(server.call(
        "exec_command",
        json!({"cmd":"printf '\\377'; printf '\\376' >&2"}),
    ));
    assert_eq!(binary["stdout_base64"], "/w==");
    assert_eq!(binary["stderr_base64"], "/g==");
    let signalled = server.saved(server.call("exec_command", json!({"cmd":"kill -TERM $$"})));
    assert_eq!(signalled["signal"], 15);
    assert_eq!(signalled["exit_code"], Value::Null);

    let args = json!({"request_id":"background","cmd":"printf begin; printf warn >&2; sleep .4; printf end; exit 9","yield_time_ms":10});
    let initial = server.saved(server.call("exec_command", args.clone()));
    let sid = initial["session_id"].as_str().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let sessions = server.call("list_sessions", json!({}));
    let found = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid)
        .unwrap();
    assert_eq!(found["status"], "exited");
    assert_eq!(found["exit_code"], 9);
    assert!(found["workdir"].is_string());
    let end = server.saved(server.call("write_stdin", json!({"session_id":sid,"chars":""})));
    assert_eq!(
        format!(
            "{}{}",
            initial["stdout"].as_str().unwrap(),
            end["stdout"].as_str().unwrap()
        ),
        "beginend"
    );
    assert_eq!(
        format!(
            "{}{}",
            initial["stderr"].as_str().unwrap(),
            end["stderr"].as_str().unwrap()
        ),
        "warn"
    );
    assert_eq!(end["exit_code"], 9);
    assert_eq!(server.call("exec_command", args), initial);
    let empty = server.saved(server.call("write_stdin", json!({"session_id":sid})));
    assert_eq!(empty["stdout"], "");
    assert_eq!(empty["exit_code"], 9);

    let start = server.saved(server.call(
        "exec_command",
        json!({"cmd":"trap '' INT; sleep 60 & wait","yield_time_ms":10}),
    ));
    let cancel_start = Instant::now();
    let cancelled = std::thread::scope(|scope| {
        let waiting =
            scope.spawn(|| {
                server.saved(server.call("write_stdin",
            json!({"request_id":"waiting","session_id":start["session_id"],"wait_seconds":15})))
            });
        std::thread::sleep(Duration::from_millis(100));
        let cancelled = server.saved(server.call(
            "write_stdin",
            json!({"request_id":"cancel","session_id":start["session_id"],"chars":"\u{3}"}),
        ));
        assert_eq!(waiting.join().unwrap()["finished"], true);
        cancelled
    });
    assert!(
        cancel_start.elapsed() < Duration::from_secs(4),
        "Cancellation must not queue behind the 15 second wait"
    );
    assert_eq!(cancelled["cancelled"], true);
    assert_eq!(cancelled["finished"], true);
    assert_eq!(cancelled["signal"], 9);
    assert_eq!(
        server.call("get_result", json!({"request_id":"cancel"})),
        cancelled
    );
    let timed =
        server.saved(server.call("exec_command", json!({"cmd":"sleep 60","timeout_ms":50})));
    assert_eq!(timed["timed_out"], true);
    assert_eq!(timed["finished"], true);
    let warned = server.saved(server.call(
        "exec_command",
        json!({"request_id":"warn","cmd":"true # systemctl restart example"}),
    ));
    assert!(!warned["warnings"].as_array().unwrap().is_empty());
    assert_eq!(
        server.call("get_result", json!({"request_id":"warn"})),
        warned
    );
    assert!(!server._dir.path().join("state/daemon.json").exists());
}
