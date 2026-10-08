use lkjyolo::{diagnostics::inspect, journal::Journal};
use rusqlite::params;
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

fn tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
}

fn insert(j: &Journal, id: &str, status: &str, result: Value) {
    j.prepare(
        id,
        "exec_command",
        &json!({"cmd":"SECRET_COMMAND_DO_NOT_REPORT"}),
    )
    .unwrap();
    j.finish(id, status, &result).unwrap();
}
fn outcome(j: &Journal, path: &std::path::Path, status: &str, result: Value) -> Value {
    insert(j, "case", status, result);
    inspect(path, Some("case"), 1).unwrap()["record"].clone()
}

#[test]
fn missing_state_is_not_created() {
    let tmp = tempdir().unwrap();
    let state = tmp.path().join("absent");
    let r = inspect(&state, Some("request"), 10).unwrap();
    assert_eq!(r["journal"], "absent");
    assert_eq!(r["platform_safety_verdict"], "not_observable");
    assert!(!state.exists());
}

#[test]
fn absent_id_does_not_mean_not_executed_or_blocked() {
    let tmp = tempdir().unwrap();
    let _j = Journal::new(tmp.path()).unwrap();
    let r = inspect(tmp.path(), Some("missing"), 10).unwrap();
    assert_eq!(r["record"]["outcome"], "journal_entry_absent");
    assert_eq!(r["record"]["execution"], "undetermined");
    assert_eq!(r["automatic_replay"], false);
}

#[test]
fn done_is_not_success_without_a_typed_exit_code() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    for body in [
        json!({}),
        json!({"exit_code":"0"}),
        json!({"exit_code":false}),
        json!({"exit_code":0.5}),
    ] {
        assert_eq!(
            outcome(&j, tmp.path(), "done", json!({"structuredContent":body}))["outcome"],
            "response_saved_without_exit_evidence"
        );
    }
}

#[test]
fn typed_exit_codes_classify_success_and_failure() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    for (code, expected) in [
        (0, "command_exited_zero"),
        (2, "command_exited_nonzero"),
        (101, "command_exited_nonzero"),
        (-1, "command_exited_nonzero"),
    ] {
        let r = outcome(
            &j,
            tmp.path(),
            "done",
            json!({"structuredContent":{"exit_code":code}}),
        );
        assert_eq!(r["outcome"], expected);
        assert_eq!(r["exit_code"], code);
    }
}

#[test]
fn legacy_text_envelope_is_classified_without_returning_output() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    let r = outcome(
        &j,
        tmp.path(),
        "done",
        json!({"content":[{"type":"text","text":json!({"exit_code":0,"output":"SECRET_OUTPUT"}).to_string()}]}),
    );
    assert_eq!(r["outcome"], "command_exited_zero");
    assert!(!r.to_string().contains("SECRET"));
}

#[test]
fn uncertain_and_error_states_take_priority_over_exit_zero() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    for (status, expected) in [
        ("unknown", "execution_uncertain"),
        ("error", "recorded_error"),
        ("queued", "recorded_queued"),
        ("running", "recorded_running_liveness_unverified"),
    ] {
        let r = outcome(
            &j,
            tmp.path(),
            status,
            json!({"structuredContent":{"exit_code":0}}),
        );
        assert_eq!(r["outcome"], expected);
        assert!(r.get("exit_code").is_none());
    }
}

#[test]
fn tool_error_takes_priority_over_exit_zero() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    for result in [
        json!({"isError":true,"structuredContent":{"exit_code":0}}),
        json!({"structuredContent":{"isError":true,"exit_code":0}}),
    ] {
        assert_eq!(
            outcome(&j, tmp.path(), "done", result)["outcome"],
            "saved_tool_error"
        );
    }
}

#[test]
fn continuation_is_not_claimed_to_be_currently_alive() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    let r = outcome(
        &j,
        tmp.path(),
        "done",
        json!({"structuredContent":{"session_id":"SECRET_SESSION"}}),
    );
    assert_eq!(r["outcome"], "continuation_returned_liveness_unverified");
    assert!(!r.to_string().contains("SECRET_SESSION"));
}

#[test]
fn sample_is_bounded_and_never_prints_arguments_results_or_ids() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    for i in 0..4 {
        insert(
            &j,
            &format!("SECRET-ID-{i}"),
            "error",
            json!({"error":"SECRET_ERROR","content":[{"type":"text","text":"SECRET_TOKEN"}]}),
        );
    }
    let r = inspect(tmp.path(), None, 3).unwrap();
    assert_eq!(r["sample"]["sampled"], 3);
    assert_eq!(r["sample"]["more_available"], true);
    assert_eq!(r["sample"]["outcomes"]["recorded_error"], 3);
    assert!(!r.to_string().contains("SECRET"));
    assert_eq!(
        inspect(tmp.path(), None, 4).unwrap()["sample"]["more_available"],
        false
    );
}

#[test]
fn malformed_and_oversized_results_are_not_success() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    insert(&j, "case", "done", json!({}));
    for (raw, state) in [
        ("SECRET_INVALID_JSON".to_owned(), "malformed_json"),
        ("[]".to_owned(), "not_an_object"),
        (
            json!({"structuredContent":{"exit_code":0,"output":"x".repeat(1024 * 1024)}})
                .to_string(),
            "omitted_over_1_mib",
        ),
    ] {
        j.db.execute("UPDATE requests SET result=? WHERE id='case'", [&raw])
            .unwrap();
        let r = inspect(tmp.path(), Some("case"), 1).unwrap();
        assert_eq!(r["record"]["outcome"], "saved_result_unavailable");
        assert_eq!(r["record"]["saved_result"], state);
        assert!(!r.to_string().contains("SECRET"));
    }
}

#[test]
fn unknown_database_labels_are_not_echoed() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    insert(&j, "case", "done", json!({}));
    j.db.execute(
        "UPDATE requests SET status=?,tool=?",
        params!["SECRET_STATUS", "SECRET_TOOL"],
    )
    .unwrap();
    let r = inspect(tmp.path(), Some("case"), 1).unwrap();
    assert_eq!(r["record"]["status"], "unrecognized");
    assert_eq!(r["record"]["tool"], "other");
    assert!(!r.to_string().contains("SECRET"));
}

#[test]
fn inspection_does_not_reconcile_a_dead_worker_or_modify_data() {
    let tmp = tempdir().unwrap();
    let j = Journal::new(tmp.path()).unwrap();
    insert(&j, "case", "running", json!({}));
    j.db.execute(
        "UPDATE requests SET worker_pid=2147483647,worker_fingerprint='dead'",
        [],
    )
    .unwrap();
    let before = j.get("case").unwrap().receipt().unwrap();
    inspect(tmp.path(), Some("case"), 1).unwrap();
    inspect(tmp.path(), None, 10).unwrap();
    assert_eq!(before, j.get("case").unwrap().receipt().unwrap());
    assert_eq!(j.rows(10).unwrap().len(), 1);
    assert!(!tmp.path().join("mcp.sock").exists());
}

#[test]
fn invalid_options_fail_without_initializing_state() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("absent");
    for limit in [0, 1001] {
        assert!(inspect(&path, None, limit).is_err());
    }
    assert!(inspect(&path, Some("../invalid"), 1).is_err());
    assert!(!path.exists());
}

#[test]
fn symlink_and_incompatible_database_are_not_initialized() {
    let tmp = tempdir().unwrap();
    let target = tmp.path().join("data");
    fs::write(&target, "SECRET_NOT_SQLITE").unwrap();
    std::os::unix::fs::symlink(&target, tmp.path().join("requests.sqlite3")).unwrap();
    assert!(inspect(tmp.path(), None, 1).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "SECRET_NOT_SQLITE");
}

#[test]
fn cli_error_never_exposes_database_or_path_contents() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("requests.sqlite3"), "SECRET_DATABASE").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lkjyolo-diagnostics"))
        .arg("--state")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(!error.contains("SECRET"));
    assert!(error.contains("local_diagnostics_unavailable"));
    assert!(output.stdout.is_empty());
}
