//! Files use ordinary OS permissions. Hash guards prevent accidental stale edits;
//! they are not a sandbox and do not serialize unrelated editors.
use crate::util::*;
use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    pub path: PathBuf,
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "read_limit")]
    pub limit: u64,
}
fn read_limit() -> u64 {
    32768
}

pub fn read(args: ReadArgs) -> Result<Value> {
    ensure!(args.path.is_absolute(), "path must be absolute");
    ensure!(
        (1..=262144).contains(&args.limit),
        "limit must be 1..262144 bytes"
    );
    let path = args.path.canonicalize()?;
    let metadata = fs::metadata(&path)?;
    if metadata.is_dir() {
        let mut entries = Vec::new();
        // Bound each response even for very large directories.
        for entry in fs::read_dir(&path)?.skip(args.offset as usize).take(501) {
            let entry = entry?;
            let kind = entry.file_type()?;
            entries.push(json!({"name":entry.file_name().to_string_lossy(),"kind":if kind.is_dir(){"directory"}else if kind.is_symlink(){"symlink"}else{"file"}}));
        }
        let truncated = entries.len() > 500;
        entries.truncate(500);
        return Ok(
            json!({"path":path,"kind":"directory","entries":entries,"next_offset":args.offset+entries.len() as u64,"truncated":truncated,"note":"Directory offsets are entry counts; concurrent changes may reorder entries."}),
        );
    }
    ensure!(
        metadata.is_file(),
        "Only regular files and directories can be read"
    );
    let mut file = File::open(&path)?;
    let mut bytes = Vec::new();
    let mut hash = None;
    let size;
    if metadata.len() <= 16 * 1024 * 1024 {
        let mut full = Vec::new();
        file.take(16 * 1024 * 1024 + 1).read_to_end(&mut full)?;
        ensure!(
            full.len() <= 16 * 1024 * 1024,
            "File grew during read; retry"
        );
        size = full.len() as u64;
        hash = Some(sha256(&full));
        let start = args.offset.min(size) as usize;
        let end = start.saturating_add(args.limit as usize).min(full.len());
        bytes.extend_from_slice(&full[start..end]);
    } else {
        size = metadata.len();
        file.seek(SeekFrom::Start(args.offset))?;
        file.take(args.limit).read_to_end(&mut bytes)?;
    }
    let next = args
        .offset
        .checked_add(bytes.len() as u64)
        .context("offset overflow")?;
    let mut value = json!({"path":path,"kind":"file","offset":args.offset,"next_offset":next,"size_bytes":size,"eof":next>=size});
    match String::from_utf8(bytes) {
        Ok(text) => value["text"] = json!(text),
        Err(error) => {
            value["base64"] =
                json!(base64::engine::general_purpose::STANDARD.encode(error.as_bytes()))
        }
    }
    // A full hash is available for editable files. Large log reads stay cheap.
    if let Some(hash) = hash {
        value["sha256"] = json!(hash);
    }
    Ok(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub path: PathBuf,
    pub before_sha256: String,
    pub content: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchArgs {
    pub changes: Vec<Change>,
}

pub fn validate_patch(args: &PatchArgs) -> Result<()> {
    ensure!(
        !args.changes.is_empty() && args.changes.len() <= 64,
        "Use 1..64 changes"
    );
    let mut total = 0;
    for c in &args.changes {
        ensure!(c.path.is_absolute(), "path must be absolute");
        ensure!(
            c.before_sha256 == "absent"
                || (c.before_sha256.len() == 64
                    && c.before_sha256
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
            "before_sha256 must be absent or a lowercase SHA-256"
        );
        ensure!(
            c.content.is_some() || c.before_sha256 != "absent",
            "Cannot delete an absent file"
        );
        total += c.content.as_ref().map_or(0, String::len);
    }
    ensure!(total <= 1024 * 1024, "Patch content exceeds 1 MiB");
    Ok(())
}

fn check(path: &Path, expected: &str) -> Result<u32> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "Target must be a regular non-symlink file"
            );
            ensure!(
                expected != "absent" && file_sha256(path)? == expected,
                "Hash conflict: {}",
                path.display()
            );
            Ok(meta.permissions().mode() & 0o777)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ensure!(expected == "absent", "File disappeared: {}", path.display());
            Ok(0o644)
        }
        Err(e) => Err(e.into()),
    }
}

pub fn apply(state: &Path, value: &Value, dispatched: &mut bool) -> Result<Value> {
    let args: PatchArgs = serde_json::from_value(value.clone())?;
    validate_patch(&args)?;
    let _lock = lock(&state.join("files.lock"))?;
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for c in &args.changes {
        let parent = c.path.parent().context("Missing parent")?.canonicalize()?;
        let path = parent.join(c.path.file_name().context("Missing file name")?);
        ensure!(seen.insert(path.clone()), "Duplicate target");
        let mode = check(&path, &c.before_sha256)?;
        targets.push((path, mode));
    }
    let mut changed = Vec::new();
    for (c, (path, mode)) in args.changes.iter().zip(targets) {
        check(&path, &c.before_sha256)?;
        // From this point a crash can leave a partial batch: the journal records
        // unknown, never retries it. Each individual replacement is atomic.
        *dispatched = true;
        if let Some(content) = &c.content {
            atomic_write(&path, content.as_bytes(), mode)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
            changed.push(json!({"path":path,"sha256":sha256(content.as_bytes())}));
        } else {
            fs::remove_file(&path)?;
            File::open(path.parent().unwrap())?.sync_all()?;
            changed.push(json!({"path":path,"deleted":true}));
        }
    }
    Ok(json!({"changed":changed}))
}
