use crate::util::*;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

pub const TOOLS: &[&str] = &[
    "exec_command",
    "start_session",
    "write_stdin",
    "wait_for_exit",
];
pub const TERMINAL: &[&str] = &["done", "error", "unknown"];

#[derive(Clone, Debug)]
pub struct Request {
    pub id: String,
    pub digest: String,
    pub tool: String,
    pub arguments: String,
    pub status: String,
    pub created: f64,
    pub updated: f64,
    pub worker_pid: Option<u32>,
    pub worker_fingerprint: Option<String>,
    pub generation: Option<String>,
    pub result: Option<String>,
}

impl Request {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            digest: row.get("digest")?,
            tool: row.get("tool")?,
            arguments: row.get("arguments")?,
            status: row.get("status")?,
            created: row.get("created")?,
            updated: row.get("updated")?,
            worker_pid: row.get("worker_pid")?,
            worker_fingerprint: row.get("worker_fingerprint")?,
            generation: row.get("generation")?,
            result: row.get("result")?,
        })
    }

    pub fn receipt(&self) -> Result<Value> {
        let mut value = json!({"request_id":self.id,"status":self.status,"tool":self.tool,"created_at":self.created,"updated_at":self.updated});
        if let Some(generation) = &self.generation {
            value["generation"] = json!(generation);
        }
        if let Some(result) = &self.result {
            value["result"] = serde_json::from_str(result)?;
        } else {
            value["next"] = json!(format!("lkjyolo result {} --wait 45", self.id));
        }
        Ok(value)
    }
}

pub struct Journal {
    pub db: Connection,
}

pub fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 120
            && id.as_bytes()[0].is_ascii_alphanumeric()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
        "request_id must contain 1–120 letters, digits, dots, underscores or hyphens"
    );
    Ok(())
}

impl Journal {
    pub fn new(state: &Path) -> Result<Self> {
        private_dir(state)?;
        // SQLite's busy timeout does not cover every journal-mode transition.
        // Serialize first-use WAL/schema setup across CLI and worker processes.
        let _initialization = lock(&state.join("journal.lock"))?;
        let path = state.join("requests.sqlite3");
        let db = Connection::open(&path)?;
        db.busy_timeout(Duration::from_secs(15))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS requests (
                id TEXT PRIMARY KEY, digest TEXT NOT NULL, tool TEXT NOT NULL,
                arguments TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                updated REAL NOT NULL, worker_pid INTEGER, worker_fingerprint TEXT,
                generation TEXT, result TEXT);
            CREATE INDEX IF NOT EXISTS requests_created ON requests(created);",
        )?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Self { db })
    }

    pub fn get(&self, id: &str) -> Result<Request> {
        self.db
            .query_row("SELECT * FROM requests WHERE id=?", [id], Request::from_row)
            .optional()?
            .with_context(|| format!("Unknown request_id: {id}"))
    }

    pub fn prepare(&self, id: &str, tool: &str, arguments: &Value) -> Result<Request> {
        validate_id(id)?;
        ensure!(TOOLS.contains(&tool), "Unsupported tool: {tool}");
        ensure!(
            arguments.is_object(),
            "Tool arguments must be a JSON object"
        );
        let canonical = serde_json::to_string(arguments)?;
        let digest = sha256(format!("{tool}\n{canonical}").as_bytes());
        self.db.execute("INSERT OR IGNORE INTO requests(id,digest,tool,arguments,status,created,updated) VALUES(?,?,?,?,?,?,?)",
            params![id,digest,tool,canonical,"queued",now(),now()])?;
        let row = self.get(id)?;
        // Python and serde can format equivalent JSON numbers differently.
        // Preserve old digest/arguments verbatim while accepting semantic replay.
        ensure!(
            row.tool == tool
                && (row.digest == digest
                    || serde_json::from_str::<Value>(&row.arguments)? == *arguments),
            "request_id already belongs to different arguments; use a new ID for a new action"
        );
        Ok(row)
    }

    pub fn running(&self, id: &str) -> Result<()> {
        let pid = std::process::id();
        let fp = fingerprint(pid).context("Cannot identify worker process")?;
        self.db.execute("UPDATE requests SET status='running',worker_pid=?,worker_fingerprint=?,updated=? WHERE id=?",
            params![pid,fp,now(),id])?;
        Ok(())
    }

    pub fn generation(&self, id: &str, generation: &str) -> Result<()> {
        self.db.execute(
            "UPDATE requests SET generation=?,updated=? WHERE id=?",
            params![generation, now(), id],
        )?;
        Ok(())
    }

    pub fn finish(&self, id: &str, status: &str, value: &Value) -> Result<()> {
        self.db.execute(
            "UPDATE requests SET status=?,result=?,updated=? WHERE id=?",
            params![status, serde_json::to_string(value)?, now(), id],
        )?;
        Ok(())
    }

    pub fn rows(&self, limit: usize) -> Result<Vec<Request>> {
        let mut stmt = self
            .db
            .prepare("SELECT * FROM requests ORDER BY created DESC, id DESC LIMIT ?")?;
        Ok(stmt
            .query_map([limit as i64], Request::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn reconcile(&self, row: Request) -> Result<Request> {
        let alive = row.worker_pid.and_then(fingerprint);
        if row.status == "running" && (alive.is_none() || alive != row.worker_fingerprint) {
            let result = json!({"isError":true,"error":"Worker disappeared after dispatch became possible. Inspect sessions, saved logs and command side effects; this request will never be automatically re-executed."});
            self.db.execute("UPDATE requests SET status='unknown',result=?,updated=? WHERE id=? AND status='running' AND worker_pid IS ? AND worker_fingerprint IS ?",
                params![result.to_string(),now(),row.id,row.worker_pid,row.worker_fingerprint])?;
            return self.get(&row.id);
        }
        Ok(row)
    }

    pub fn check_session(&self, arguments: &Value, generation: &str) -> Result<()> {
        let Some(sid) = arguments.get("session_id") else {
            return Ok(());
        };
        let mut stmt = self.db.prepare("SELECT result,generation FROM requests WHERE tool IN ('exec_command','start_session') AND status='done' ORDER BY created DESC")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let result: Value = serde_json::from_str(&row.get::<_, String>(0)?)?;
            if &result["structuredContent"]["session_id"] == sid && !sid.is_null() {
                ensure!(
                    row.get::<_, Option<String>>(1)?.as_deref() == Some(generation),
                    "Session belongs to a stopped/restarted engine. Inspect its request receipts; start a new session explicitly."
                );
                return Ok(());
            }
        }
        bail!(
            "Session was not created through this lkjyolo journal. Use its originating client or start a new lkjyolo session."
        )
    }
}
