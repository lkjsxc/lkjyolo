//! Bounded, non-reconciling inspection of the existing execution journal.
//!
//! This module never opens the execution socket, starts workers, consumes session
//! output, initializes a journal, or infers an OpenAI verdict from local records.
use crate::{journal::validate_id, runtime::result_body};
use anyhow::{Result, anyhow, ensure};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

pub const MAX_RESULT_BYTES: usize = 1024 * 1024;
const PROJECTION: &str = "SELECT status, tool, created, updated,
    CASE WHEN length(CAST(result AS BLOB)) <= 1048576 THEN result END,
    result IS NOT NULL AND length(CAST(result AS BLOB)) > 1048576
    FROM requests";

fn bounded_status(status: &str) -> &str {
    match status {
        "queued" | "running" | "done" | "error" | "unknown" => status,
        _ => "unrecognized",
    }
}
fn bounded_tool(tool: &str) -> &str {
    match tool {
        "exec_command" | "start_session" | "write_stdin" | "wait_for_exit" | "apply_patch" => tool,
        _ => "other",
    }
}

fn classify(status: &str, raw: Option<&str>, oversized: bool) -> Value {
    let parsed = raw.map(serde_json::from_str::<Value>);
    let result_state = if oversized {
        "omitted_over_1_mib"
    } else {
        match &parsed {
            None => "absent",
            Some(Err(_)) => "malformed_json",
            Some(Ok(value)) if !value.is_object() => "not_an_object",
            Some(Ok(_)) => "parsed",
        }
    };
    let result = parsed.and_then(Result::ok).unwrap_or(Value::Null);
    let body = result_body(&result);
    let exit = body.get("exit_code").and_then(Value::as_i64);
    let outcome = match status {
        "queued" => "recorded_queued",
        "running" => "recorded_running_liveness_unverified",
        "unknown" => "execution_uncertain",
        "error" => "recorded_error",
        "done" if result_state != "parsed" => "saved_result_unavailable",
        "done" if result["isError"] == true || body["isError"] == true => "saved_tool_error",
        "done" if exit == Some(0) => "command_exited_zero",
        "done" if exit.is_some() => "command_exited_nonzero",
        "done" if body["session_id"].is_string() => "continuation_returned_liveness_unverified",
        "done" => "response_saved_without_exit_evidence",
        _ => "unrecognized_journal_status",
    };
    // Never return arbitrary error text, output, session IDs or other payload fields.
    let mut value = json!({"outcome":outcome,"saved_result":result_state});
    if outcome == "command_exited_zero" || outcome == "command_exited_nonzero" {
        value["exit_code"] = json!(exit);
    }
    value
}

fn evidence_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let status: String = row.get(0)?;
    let tool: String = row.get(1)?;
    let created: f64 = row.get(2)?;
    let updated: f64 = row.get(3)?;
    let result: Option<String> = row.get(4)?;
    let oversized: bool = row.get(5)?;
    let mut value = classify(&status, result.as_deref(), oversized);
    value["status"] = json!(bounded_status(&status));
    value["tool"] = json!(bounded_tool(&tool));
    value["created_at"] = json!(created);
    value["updated_at"] = json!(updated);
    Ok(value)
}

/// Inspect a bounded snapshot. A missing ID is not evidence of non-execution.
/// Stored result bodies are parsed internally, but never returned. Arguments,
/// credentials, settings and session payloads are not selected from the database.
pub fn inspect(state: &Path, request_id: Option<&str>, limit: usize) -> Result<Value> {
    ensure!((1..=1000).contains(&limit), "limit_must_be_1_to_1000");
    if let Some(id) = request_id {
        validate_id(id)?;
    }
    let mut report = json!({
        "schema_version":1,
        "diagnostic_version":"2026-10-08",
        "scope":"local_journal_snapshot_only",
        "platform_safety_verdict":"not_observable",
        "automatic_replay":false,
        "notes":[
            "Missing records do not prove non-execution or an OpenAI block.",
            "done means a saved response, not necessarily command success.",
            "Session and running records do not prove current process liveness.",
            "This sample excludes calls absent from this journal, including pre-journal failures and read-only tools.",
            "Counts are receipt outcomes, not a safety-block rate or a unique-command success rate."
        ]
    });
    let path = state.join("requests.sqlite3");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            report["journal"] = json!("absent");
            return Ok(report);
        }
        Err(_) => return Err(anyhow!("journal_metadata_unavailable")),
        Ok(metadata) => ensure!(
            metadata.file_type().is_file(),
            "journal_must_be_regular_file"
        ),
    }
    // Do not use Journal::new: it initializes schema and may change permissions.
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| anyhow!("journal_open_failed"))?;
    db.busy_timeout(Duration::from_secs(2))
        .map_err(|_| anyhow!("journal_configuration_failed"))?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")
        .map_err(|_| anyhow!("journal_configuration_failed"))?;
    let transaction = db
        .unchecked_transaction()
        .map_err(|_| anyhow!("journal_snapshot_unavailable"))?;
    report["journal"] = json!("read_only_snapshot");
    if let Some(id) = request_id {
        let sql = format!("{PROJECTION} WHERE id=?1");
        let mut statement = transaction
            .prepare(&sql)
            .map_err(|_| anyhow!("journal_query_failed"))?;
        let mut rows = statement
            .query([id])
            .map_err(|_| anyhow!("journal_query_failed"))?;
        let record = match rows.next().map_err(|_| anyhow!("journal_read_failed"))? {
            Some(row) => evidence_row(row).map_err(|_| anyhow!("journal_entry_decode_failed"))?,
            None => json!({"outcome":"journal_entry_absent","execution":"undetermined"}),
        };
        report["request_id"] = json!(id);
        report["record"] = record;
    } else {
        let sql = format!("{PROJECTION} ORDER BY created DESC, id DESC LIMIT ?1");
        let mut statement = transaction
            .prepare(&sql)
            .map_err(|_| anyhow!("journal_query_failed"))?;
        let mut rows = statement
            .query([(limit + 1) as i64])
            .map_err(|_| anyhow!("journal_query_failed"))?;
        let mut outcomes = BTreeMap::<String, usize>::new();
        let mut tools = BTreeMap::<String, usize>::new();
        let mut statuses = BTreeMap::<String, usize>::new();
        let mut sampled = 0;
        let mut more = false;
        while let Some(row) = rows.next().map_err(|_| anyhow!("journal_read_failed"))? {
            if sampled == limit {
                more = true;
                break;
            }
            let record = evidence_row(row).map_err(|_| anyhow!("journal_entry_decode_failed"))?;
            for (key, counts) in [
                ("outcome", &mut outcomes),
                ("tool", &mut tools),
                ("status", &mut statuses),
            ] {
                *counts
                    .entry(record[key].as_str().unwrap().to_owned())
                    .or_default() += 1;
            }
            sampled += 1;
        }
        report["sample"] = json!({
            "order":"created_desc_id_desc","limit":limit,"sampled":sampled,
            "more_available":more,"outcomes":outcomes,"tools":tools,"statuses":statuses
        });
    }
    transaction
        .commit()
        .map_err(|_| anyhow!("journal_snapshot_failed"))?;
    Ok(report)
}
