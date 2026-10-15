use crate::{ENGINE_SHA256, ENGINE_VERSION, VERSION, engine::setup, util::*};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zip::{ZipWriter, write::SimpleFileOptions};

fn download(url: &str, limit: u64) -> Result<Vec<u8>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(format!("lkjyolo/{VERSION}"))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;
    let response = client.get(url).send()?.error_for_status()?;
    let mut body = Vec::new();
    response.take(limit + 1).read_to_end(&mut body)?;
    ensure!(body.len() as u64 <= limit, "Download exceeded size limit");
    Ok(body)
}

pub fn install(state: &Path, workspace: &Path) -> Result<Value> {
    ensure!(
        cfg!(all(target_os = "linux", target_arch = "x86_64")),
        "Pinned installer supports Linux x86_64; use setup with a source-built engine on other Linux architectures"
    );
    let root = home()?.join(".local/share/lkjyolo");
    let release = root.join("releases").join(VERSION);
    let engine_dir = root.join("engines").join(ENGINE_VERSION);
    let bin_dir = home()?.join(".local/bin");
    let link = bin_dir.join("lkjyolo");
    if let Ok(meta) = fs::symlink_metadata(&link) {
        ensure!(
            meta.file_type().is_symlink(),
            "Refusing to replace an existing non-symlink: {}",
            link.display()
        );
    }
    let workspace = absolute(workspace)?.canonicalize()?;
    ensure!(workspace.is_dir(), "workspace must be a directory");
    fs::create_dir_all(&release)?;
    fs::create_dir_all(&engine_dir)?;
    fs::create_dir_all(&bin_dir)?;
    let _lock = lock(&root.join("install.lock"))?;
    let engine = engine_dir.join("chatgpt-exec-mcp");
    let base = format!(
        "https://github.com/CeraCharlesCC/chatgpt-exec-mcp/releases/download/v{ENGINE_VERSION}/"
    );
    if engine.exists() {
        ensure!(
            file_sha256(&engine)? == ENGINE_SHA256,
            "Installed engine digest mismatch; inspect it before replacement"
        );
    } else {
        let data = download(
            &(base.clone() + "chatgpt-exec-mcp-linux-x86_64"),
            100 * 1024 * 1024,
        )?;
        ensure!(
            sha256(&data) == ENGINE_SHA256,
            "Upstream engine checksum mismatch; nothing installed"
        );
        atomic_write(&engine, &data, 0o755)?;
    }
    for (name, expected) in [
        (
            "LICENSE",
            "4458503dd48e88c4e0b945fb252a08b93c40ec757309b8ffa7c594dfa1e35104",
        ),
        (
            "NOTICE",
            "d4bb2abc4f3def8cb7cf8af4f3879c26dc125df6d497fea78c750f812b104e4f",
        ),
        (
            "THIRD_PARTY_NOTICES.md",
            "d4872ddfe50f60e738f52112acb735555f3843e7338bc77f01f0d5ad7eaeb685",
        ),
    ] {
        let path = engine_dir.join(name);
        if path.is_file() && file_sha256(&path)? == expected {
            continue;
        }
        let data = download(&(base.clone() + name), 1024 * 1024)?;
        ensure!(
            sha256(&data) == expected,
            "Upstream {name} checksum mismatch"
        );
        atomic_write(&path, &data, 0o644)?;
    }
    // Preflight before touching the installed CLI. Preserves live engine identity,
    // Python-format settings, journal, and active session process groups.
    let configuration = setup(state, &engine, &workspace, Some(ENGINE_SHA256))?;
    let source = std::env::current_exe()?;
    let binary = release.join("lkjyolo");
    if !binary.exists() || file_sha256(&binary)? != file_sha256(&source)? {
        atomic_write(&binary, &fs::read(&source)?, 0o755)?;
    }
    let previous = fs::read_link(&link).ok();
    let temporary = bin_dir.join(format!(".lkjyolo-{}", uuid::Uuid::new_v4().simple()));
    symlink(&binary, &temporary)?;
    if let Err(error) = fs::rename(&temporary, &link) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    File::open(&bin_dir)?.sync_all()?;
    Ok(
        json!({"installed":link,"version":VERSION,"runtime":"rust","engine":engine,
        "engine_sha256":ENGINE_SHA256,"previous_target":previous,"configuration":configuration}),
    )
}

fn files(directory: &Path, result: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "Unexpected plugin symlink: {}",
            entry.path().display()
        );
        if kind.is_dir() {
            files(&entry.path(), result)?;
        } else if kind.is_file() {
            result.push(entry.path());
        } else {
            anyhow::bail!(
                "Unexpected non-file plugin asset: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

pub fn plugin_zip(source: &Path, output: &Path) -> Result<Value> {
    plugin_zip_options(source, output, false, None)
}

pub fn plugin_zip_options(
    source: &Path,
    output: &Path,
    portable: bool,
    endpoint: Option<&str>,
) -> Result<Value> {
    ensure!(
        portable || endpoint.is_none(),
        "Endpoint overrides require --portable"
    );
    if let Some(endpoint) = endpoint {
        let url = reqwest::Url::parse(endpoint)?;
        ensure!(
            url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Endpoint must be HTTPS without credentials, query or fragment"
        );
    }
    let source = source.canonicalize()?;
    let read = |name: &str| -> Result<Value> {
        Ok(serde_json::from_slice(&fs::read(source.join(name))?)?)
    };
    let manifest = read("plugin.json")?;
    let legacy = read(".codex-plugin/plugin.json")?;
    let interface = &manifest["extensions"]["com.openai"]["interface"];
    ensure!(
        manifest["name"] == "lkjyolo" && legacy["name"] == manifest["name"],
        "Plugin identity mismatch"
    );
    ensure!(
        manifest["version"] == legacy["version"] && *interface == legacy["interface"],
        "Compatibility metadata mismatch"
    );
    ensure!(
        interface["shortDescription"]
            .as_str()
            .context("Missing shortDescription")?
            .chars()
            .count()
            <= 30,
        "Plugin subtitle exceeds 30 characters"
    );
    ensure!(
        manifest["extensions"]["com.openai"]["apps"] == "./.app.json",
        "Missing app binding"
    );
    ensure!(
        read(".app.json")?["apps"]["connector"]["id"]
            == "asdk_app_6abfcb784b68819186e299abbfd9a44a",
        "Unexpected app identity"
    );
    for name in ["mcp.json", ".mcp.json"] {
        ensure!(
            read(name)?["mcpServers"]["lkjyolo"]["url"] == "https://coder.lkjsxc.com/lkjyolo/mcp",
            "Unexpected MCP endpoint"
        );
    }
    let version = manifest["version"]
        .as_str()
        .context("Missing plugin version")?;
    ensure!(
        version.bytes().all(|b| b.is_ascii_digit() || b == b'.'),
        "Invalid plugin version"
    );
    let mut inventory = Vec::new();
    files(&source, &mut inventory)?;
    inventory.sort();
    fs::create_dir_all(output)?;
    let output = output.canonicalize()?;
    ensure!(
        !output.starts_with(&source),
        "Plugin output must be outside the source directory"
    );
    let suffix = if portable { "-portable" } else { "" };
    let target = output.join(format!("lkjyolo-{version}{suffix}.zip"));
    let mut archive = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let mut file_count = 0;
    for file in &inventory {
        let relative = file
            .strip_prefix(&source)?
            .to_str()
            .context("Plugin file name is not UTF-8")?;
        if portable && relative == ".app.json" {
            continue;
        }
        let mut data = fs::read(file)?;
        if portable && ["plugin.json", ".codex-plugin/plugin.json"].contains(&relative) {
            let mut value: Value = serde_json::from_slice(&data)?;
            value
                .as_object_mut()
                .context("Invalid manifest")?
                .remove("apps");
            if let Some(openai) = value
                .pointer_mut("/extensions/com.openai")
                .and_then(Value::as_object_mut)
            {
                openai.remove("apps");
            }
            data = serde_json::to_vec_pretty(&value)?;
        }
        if let Some(endpoint) = endpoint
            && ["mcp.json", ".mcp.json"].contains(&relative)
        {
            let mut value: Value = serde_json::from_slice(&data)?;
            value["mcpServers"]["lkjyolo"]["url"] = json!(endpoint);
            data = serde_json::to_vec_pretty(&value)?;
        }
        archive.start_file(format!("lkjyolo/{relative}"), options)?;
        archive.write_all(&data)?;
        file_count += 1;
    }
    let data = archive.finish()?.into_inner();
    atomic_write(&target, &data, 0o644)?;
    Ok(json!({"archive":target,"files":file_count,"plugin_version":version,"portable":portable}))
}
