use lkjyolo::{
    cli::decode_json,
    engine::Engine,
    journal::{Journal, validate_id},
    package::plugin_zip,
    runtime::{read_log, result_body},
    util::*,
};
use serde_json::{Value, json};
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
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/home-coder");
    let result = plugin_zip(&source, tmp.path()).unwrap();
    let file = fs::File::open(result["archive"].as_str().unwrap()).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();
    assert_eq!(zip.len(), 6);
    for index in 0..zip.len() {
        let mut file = zip.by_index(index).unwrap();
        let name = file.name().strip_prefix("home-coder/").unwrap().to_owned();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, fs::read(source.join(name)).unwrap());
    }
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
