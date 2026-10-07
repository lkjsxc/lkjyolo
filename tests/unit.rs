use lkjyolo::{
    cli::decode_json,
    engine::Engine,
    journal::{Journal, validate_id},
    package::plugin_zip,
    runtime::{read_log, result_body},
    util::*,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::{
    fs::{PermissionsExt, symlink},
    net::UnixListener,
};
use std::path::Path;
fn tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
}

#[test]
fn duplicate_request_preserves_result_and_rejects_changed_arguments() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    j.prepare(
        "build-1",
        "exec_command",
        &json!({"cmd":"true","tty":false}),
    )
    .unwrap();
    j.finish(
        "build-1",
        "done",
        &json!({"structuredContent":{"exit_code":0}}),
    )
    .unwrap();
    let replay = j
        .prepare(
            "build-1",
            "exec_command",
            &json!({"tty":false,"cmd":"true"}),
        )
        .unwrap();
    assert_eq!(replay.status, "done");
    assert!(
        j.prepare("build-1", "exec_command", &json!({"cmd":"false"}))
            .is_err()
    );
    assert_eq!(j.rows(10).unwrap().len(), 1);
}

#[test]
fn invalid_ids_and_payloads_never_create_requests() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    for id in ["../outside", "", "a\nb", "-start", "日本語"] {
        assert!(validate_id(id).is_err());
    }
    assert!(validate_id(&"x".repeat(121)).is_err());
    assert!(j.prepare("a", "invalid", &json!({})).is_err());
    assert!(j.prepare("b", "exec_command", &json!([])).is_err());
    assert!(j.rows(10).unwrap().is_empty());
}

#[test]
fn concurrent_first_use_initializes_one_journal() {
    let tmp = tempdir().unwrap();
    std::thread::scope(|scope| {
        let mut jobs = Vec::new();
        for _ in 0..20 {
            let path = tmp.path();
            jobs.push(scope.spawn(move || {
                Journal::new(path)
                    .unwrap()
                    .prepare("same", "exec_command", &json!({"cmd":"true"}))
                    .unwrap()
            }));
        }
        for job in jobs {
            assert_eq!(job.join().unwrap().status, "queued");
        }
    });
    assert_eq!(
        Journal::new(tmp.path()).unwrap().rows(100).unwrap().len(),
        1
    );
}

#[test]
fn missing_worker_becomes_unknown_and_cannot_be_requeued() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    j.prepare("uncertain", "exec_command", &json!({"cmd":"true"}))
        .unwrap();
    j.db.execute(
        "UPDATE requests SET status='running',worker_pid=2147483647,worker_fingerprint='missing'",
        [],
    )
    .unwrap();
    assert_eq!(
        j.reconcile(j.get("uncertain").unwrap()).unwrap().status,
        "unknown"
    );
    assert_eq!(
        j.prepare("uncertain", "exec_command", &json!({"cmd":"true"}))
            .unwrap()
            .status,
        "unknown"
    );
}

#[test]
fn stale_read_does_not_overwrite_a_completed_result() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    j.prepare("race", "exec_command", &json!({"cmd":"true"}))
        .unwrap();
    j.db.execute(
        "UPDATE requests SET status='running',worker_pid=2147483647,worker_fingerprint='missing'",
        [],
    )
    .unwrap();
    let stale = j.get("race").unwrap();
    j.finish(
        "race",
        "done",
        &json!({"structuredContent":{"exit_code":0}}),
    )
    .unwrap();
    assert_eq!(j.reconcile(stale).unwrap().status, "done");
}

#[test]
fn python_receipt_replays_without_mutating_legacy_storage() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    let f: Value = serde_json::from_str(include_str!("fixtures/python-v0.1-receipt.json")).unwrap();
    j.db.execute("INSERT INTO requests(id,digest,tool,arguments,status,created,updated,generation,result) VALUES(?,?,?,?,?,?,?,?,?)",
        rusqlite::params![f["id"].as_str(),f["digest"].as_str(),f["tool"].as_str(),f["arguments"].as_str(),f["status"].as_str(),
            f["created"].as_f64(),f["updated"].as_f64(),f["generation"].as_str(),f["result"].as_str()]).unwrap();
    let args: Value = serde_json::from_str(f["arguments"].as_str().unwrap()).unwrap();
    let row = j.prepare("legacy-replay", "exec_command", &args).unwrap();
    assert_eq!(row.digest, f["digest"]);
    assert_eq!(row.arguments, f["arguments"]);
    assert_eq!(row.result.as_deref(), f["result"].as_str());
    assert_eq!(
        row.receipt().unwrap()["result"]["structuredContent"]["output"],
        "SAVED_BY_PYTHON\n"
    );
}

#[test]
fn equivalent_legacy_json_format_is_accepted_but_different_payload_is_not() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    j.prepare("format", "exec_command", &json!({"cmd":"true"}))
        .unwrap();
    j.db.execute(
        "UPDATE requests SET digest='legacy',arguments='{\"cmd\": \"true\"}'",
        [],
    )
    .unwrap();
    assert!(
        j.prepare("format", "exec_command", &json!({"cmd":"true"}))
            .is_ok()
    );
    assert!(
        j.prepare("format", "exec_command", &json!({"cmd":"false"}))
            .is_err()
    );
}

#[test]
fn state_directory_rejects_shared_access_and_symlinks() {
    let tmp = tempdir().unwrap();
    let shared = tmp.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(private_dir(&shared).is_err());
    let link = tmp.path().join("link");
    symlink(tmp.path(), &link).unwrap();
    assert!(private_dir(&link).is_err());
}

#[test]
fn log_byte_ranges_preserve_binary_and_reject_path_escape() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("outputs");
    fs::create_dir(&root).unwrap();
    let path = root.join("log");
    fs::write(&path, b"abc\xffdef").unwrap();
    write_json(
        &tmp.path().join("engine.json"),
        &json!({"output_store_dir":root}),
    )
    .unwrap();
    let value = read_log(tmp.path(), &path, 2, 3).unwrap();
    assert_eq!(value["base64"], "Y/9k");
    assert_eq!(value["next_offset"], 5);
    assert_eq!(value["eof"], false);
    assert!(read_log(tmp.path(), &tmp.path().join("engine.json"), 0, 10).is_err());
    symlink(tmp.path().join("engine.json"), root.join("escape")).unwrap();
    assert!(read_log(tmp.path(), &root.join("escape"), 0, 10).is_err());
}

#[test]
fn json_arguments_and_result_fallback() {
    let tmp = tempdir().unwrap();
    let file = tmp.path().join("args.json");
    fs::write(&file, r#"{"cmd":"printf \"$HOME\""}"#).unwrap();
    assert_eq!(
        decode_json(&format!("@{}", file.display())).unwrap()["cmd"],
        "printf \"$HOME\""
    );
    assert!(decode_json("[]").is_err());
    assert_eq!(
        result_body(&json!({"content":[{"type":"text","text":"{\"exit_code\":4}"}]})),
        json!({"exit_code":4})
    );
}

#[test]
fn plugin_packaging_preserves_every_source_file_and_binding() {
    let tmp = tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/lkjyolo");
    let mut expected = BTreeMap::new();
    let mut directories = vec![source.clone()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                directories.push(entry.path());
            } else {
                assert!(
                    kind.is_file(),
                    "Plugin source must contain only regular files"
                );
                let path = entry.path();
                let relative = path.strip_prefix(&source).unwrap().to_str().unwrap();
                expected.insert(format!("lkjyolo/{relative}"), fs::read(&path).unwrap());
            }
        }
    }
    let result = plugin_zip(&source, tmp.path()).unwrap();
    assert_eq!(result["files"], expected.len());
    let file = fs::File::open(result["archive"].as_str().unwrap()).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();
    assert_eq!(zip.len(), expected.len());
    for index in 0..zip.len() {
        let mut file = zip.by_index(index).unwrap();
        let original = expected
            .remove(file.name())
            .expect("Unexpected or duplicate plugin archive entry");
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, original, "Contents differ for {}", file.name());
    }
    assert!(expected.is_empty(), "Plugin archive omitted source files");

    // ZipArchive deduplicates names, so also inspect every physical entry.
    let mut archive = fs::File::open(result["archive"].as_str().unwrap()).unwrap();
    let mut names = BTreeSet::new();
    while let Some(mut file) = zip::read::read_zipfile_from_stream(&mut archive).unwrap() {
        assert!(
            names.insert(file.name().to_owned()),
            "Duplicate plugin entry"
        );
        std::io::copy(&mut file, &mut std::io::sink()).unwrap();
    }
    assert_eq!(names.len(), zip.len());
}

#[test]
fn http_redirect_is_not_followed_for_tool_mutations() {
    let tmp = tempfile::Builder::new()
        .prefix("ly-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .unwrap();
    let listener = UnixListener::bind(tmp.path().join("mcp.sock")).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(request["method"], "tools/call");
        assert_eq!(request["params"]["arguments"]["cmd"], "true");
        drop(reader);
        stream.write_all(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: http://localhost/again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let error = Engine::new(tmp.path())
        .unwrap()
        .rpc(
            "tools/call",
            json!({"name":"exec_command","arguments":{"cmd":"true"}}),
            2,
        )
        .unwrap_err();
    assert!(error.to_string().contains("307"));
    server.join().unwrap();
}

#[test]
fn pid_fingerprint_rejects_missing_process() {
    assert!(fingerprint(u32::MAX).is_none());
}

// Request creation order need not match observation order: a long wait can
// finish after a poll that was submitted later.
#[test]
#[cfg(target_os = "linux")]
fn session_history_uses_completed_observations_and_preserves_exit() {
    use lkjyolo::runtime::sessions;
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    let pid = std::process::id();
    write_json(
        &tmp.path().join("daemon.json"),
        &json!({"pid":pid,"fingerprint":fingerprint(pid).unwrap(),
            "generation":"current","started_at":0.0,"engine_sha256":"test"}),
    )
    .unwrap();
    let record = |id: &str, tool: &str, created: f64, updated: f64, body: Value| {
        let arguments = if tool == "start_session" {
            json!({})
        } else {
            json!({"session_id":"session"})
        };
        j.prepare(id, tool, &arguments).unwrap();
        j.generation(id, "current").unwrap();
        j.finish(id, "done", &json!({"structuredContent":body}))
            .unwrap();
        j.db.execute(
            "UPDATE requests SET created=?,updated=? WHERE id=?",
            rusqlite::params![created, updated, id],
        )
        .unwrap();
    };
    record(
        "start",
        "start_session",
        1.0,
        1.0,
        json!({"session_id":"session"}),
    );
    record("wait", "wait_for_exit", 2.0, 4.0, json!({"exit_code":7}));
    record(
        "poll",
        "write_stdin",
        3.0,
        3.0,
        json!({"session_id":"session"}),
    );
    let observed = sessions(tmp.path()).unwrap();
    let session = &observed["sessions"][0];
    assert_eq!(session["status"], "exited");
    assert_eq!(session["exit_code"], 7);
    assert_eq!(session["last_request_id"], "wait");
    assert_eq!(session["last_seen_at"], 4.0);

    // A delayed running receipt or later error cannot undo an observed exit.
    record(
        "delayed-poll",
        "write_stdin",
        3.5,
        5.0,
        json!({"session_id":"session"}),
    );
    j.prepare(
        "late-error",
        "write_stdin",
        &json!({"session_id":"session"}),
    )
    .unwrap();
    j.generation("late-error", "current").unwrap();
    j.finish("late-error", "error", &json!({"isError":true}))
        .unwrap();
    j.db.execute(
        "UPDATE requests SET created=6,updated=6 WHERE id='late-error'",
        [],
    )
    .unwrap();
    let observed = sessions(tmp.path()).unwrap();
    let session = &observed["sessions"][0];
    assert_eq!(session["status"], "exited");
    assert_eq!(session["exit_code"], 7);
    assert_eq!(session["last_request_id"], "late-error");
    assert_eq!(session["last_seen_at"], 6.0);

    // Queued and running requests are not completed observations.
    j.prepare("pending", "write_stdin", &json!({"session_id":"session"}))
        .unwrap();
    j.generation("pending", "current").unwrap();
    assert_eq!(sessions(tmp.path()).unwrap(), observed);
    j.running("pending").unwrap();
    assert_eq!(sessions(tmp.path()).unwrap(), observed);
}

#[test]
#[cfg(target_os = "linux")]
fn session_history_timestamp_ties_are_deterministic_and_exit_wins() {
    use lkjyolo::runtime::sessions;
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    let pid = std::process::id();
    write_json(
        &tmp.path().join("daemon.json"),
        &json!({"pid":pid,"fingerprint":fingerprint(pid).unwrap(),
            "generation":"current","started_at":0.0,"engine_sha256":"test"}),
    )
    .unwrap();
    for (id, tool, body) in [
        (
            "a-exit",
            "wait_for_exit",
            json!({"session_id":"session","exit_code":0}),
        ),
        ("z-poll", "write_stdin", json!({"session_id":"session"})),
    ] {
        j.prepare(id, tool, &json!({"session_id":"session"}))
            .unwrap();
        j.generation(id, "current").unwrap();
        j.finish(id, "done", &json!({"structuredContent":body}))
            .unwrap();
        j.db.execute("UPDATE requests SET created=1,updated=2 WHERE id=?", [id])
            .unwrap();
    }
    let observed = sessions(tmp.path()).unwrap();
    let session = &observed["sessions"][0];
    assert_eq!(session["status"], "exited");
    assert_eq!(session["exit_code"], 0);
    assert_eq!(session["last_request_id"], "z-poll");
    assert_eq!(session["last_seen_at"], 2.0);
    assert_eq!(sessions(tmp.path()).unwrap(), observed);
}

#[test]
#[cfg(target_os = "linux")]
fn malformed_exit_codes_do_not_create_or_replace_terminal_observations() {
    use lkjyolo::runtime::sessions;
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    let pid = std::process::id();
    write_json(
        &tmp.path().join("daemon.json"),
        &json!({"pid":pid,"fingerprint":fingerprint(pid).unwrap(),
            "generation":"current","started_at":0.0,"engine_sha256":"test"}),
    )
    .unwrap();
    j.prepare("start", "start_session", &json!({})).unwrap();
    j.generation("start", "current").unwrap();
    j.finish(
        "start",
        "done",
        &json!({"structuredContent":{"session_id":"session"}}),
    )
    .unwrap();
    for (phase, expected) in [("running", None), ("exited", Some(-9))] {
        if let Some(exit_code) = expected {
            j.prepare("exit", "wait_for_exit", &json!({"session_id":"session"}))
                .unwrap();
            j.generation("exit", "current").unwrap();
            j.finish(
                "exit",
                "done",
                &json!({"structuredContent":{"exit_code":exit_code}}),
            )
            .unwrap();
        }
        for (index, invalid) in [
            Value::Null,
            json!("0"),
            json!({}),
            json!([]),
            json!(true),
            json!(0.5),
            json!(u64::MAX),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("{phase}-{index}");
            j.prepare(&id, "write_stdin", &json!({"session_id":"session"}))
                .unwrap();
            j.generation(&id, "current").unwrap();
            j.finish(
                &id,
                "done",
                &json!({"structuredContent":{"session_id":"session","exit_code":invalid}}),
            )
            .unwrap();
            // An arguments-only invalid exit must not invent a new session.
            let phantom_id = format!("phantom-{id}");
            j.prepare(
                &phantom_id,
                "wait_for_exit",
                &json!({"session_id":phantom_id}),
            )
            .unwrap();
            j.generation(&phantom_id, "current").unwrap();
            j.finish(
                &phantom_id,
                "done",
                &json!({"structuredContent":{"exit_code":invalid}}),
            )
            .unwrap();
            let observed = sessions(tmp.path()).unwrap();
            assert_eq!(
                observed["sessions"].as_array().unwrap().len(),
                1,
                "{invalid}"
            );
            let session = &observed["sessions"][0];
            if let Some(exit_code) = expected {
                assert_eq!(session["status"], "exited", "{invalid}");
                assert_eq!(session["exit_code"], exit_code, "{invalid}");
            } else {
                assert_eq!(
                    session["status"], "running_at_last_observation",
                    "{invalid}"
                );
                assert!(session.get("exit_code").is_none(), "{invalid}");
            }
        }
    }
}
