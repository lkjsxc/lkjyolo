use crate::{ENGINE_VERSION, VERSION, util::*};
use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const PROTOCOL: &str = "2026-07-28";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Settings {
    pub version: u32,
    pub engine: PathBuf,
    pub engine_sha256: String,
    pub workspace: PathBuf,
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Identity {
    pub pid: u32,
    pub fingerprint: String,
    pub generation: String,
    pub started_at: f64,
    pub engine_sha256: String,
}

pub struct Engine {
    pub state: PathBuf,
}

impl Engine {
    pub fn new(state: &Path) -> Result<Self> {
        private_dir(state)?;
        Ok(Self {
            state: state.to_path_buf(),
        })
    }

    pub fn settings(&self) -> Result<Settings> {
        let raw = fs::read(self.state.join("settings.json"))
            .context("Not configured. Run lkjyolo setup --engine /path/to/chatgpt-exec-mcp --workspace /path/to/workspace")?;
        Ok(serde_json::from_slice(&raw)?)
    }

    fn client(&self, timeout: u64) -> Result<Client> {
        Ok(Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .unix_socket(self.state.join("mcp.sock"))
            .timeout(Duration::from_secs(timeout))
            .redirect(reqwest::redirect::Policy::none())
            .build()?)
    }

    pub fn ready(&self) -> bool {
        (|| -> Result<bool> {
            let response = self.client(1)?.get("http://localhost/readyz").send()?;
            let status = response.status();
            let mut body = Vec::new();
            response.take(16).read_to_end(&mut body)?;
            Ok(status.as_u16() == 200 && body == b"ready")
        })()
        .unwrap_or(false)
    }

    pub fn identity(&self) -> Option<Identity> {
        let value: Identity =
            serde_json::from_slice(&fs::read(self.state.join("daemon.json")).ok()?).ok()?;
        if fingerprint(value.pid).as_deref() == Some(&value.fingerprint)
            && !value.fingerprint.is_empty()
        {
            Some(value)
        } else {
            None
        }
    }

    pub fn ensure(&self) -> Result<Identity> {
        ensure!(
            cfg!(target_os = "linux"),
            "The managed runtime requires Linux /proc process identity"
        );
        let _lock = lock(&self.state.join("daemon.lock"))?;
        if self.ready() {
            return self.identity().context(
                "Socket is active but has no verified daemon identity; inspect it before recovery",
            );
        }
        ensure!(
            self.identity().is_none(),
            "The managed engine is alive but unresponsive; inspect doctor before restarting"
        );
        let settings = self.settings()?;
        let digest = file_sha256(&settings.engine)?;
        ensure!(
            digest == settings.engine_sha256,
            "Engine digest changed; review the binary and run setup with the intended digest"
        );
        let log = append_log(&self.state.join("engine.log"))?;
        let mut command = Command::new(&settings.engine);
        command
            .args(["--config"])
            .arg(self.state.join("engine.json"))
            .arg("--listen-unix")
            .arg(self.state.join("mcp.sock"))
            .current_dir(&settings.workspace)
            .env_clear()
            .envs(&settings.environment)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        let mut child = detached(&mut command)
            .spawn()
            .context("Start execution engine")?;
        let identity = Identity {
            pid: child.id(),
            fingerprint: fingerprint(child.id()).context("Cannot identify new engine process")?,
            generation: uuid::Uuid::new_v4().simple().to_string(),
            started_at: now(),
            engine_sha256: digest,
        };
        write_json(&self.state.join("daemon.json"), &identity)?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if self.ready() {
                return Ok(identity);
            }
            if let Some(status) = child.try_wait()? {
                bail!(
                    "Engine exited ({status}); inspect {}",
                    self.state.join("engine.log").display()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!(
            "Engine readiness timed out; inspect {}",
            self.state.join("engine.log").display()
        )
    }

    pub fn rpc(&self, method: &str, mut params: Value, timeout: u64) -> Result<Value> {
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": PROTOCOL,
            "io.modelcontextprotocol/clientInfo": {"name":"lkjyolo","version":VERSION},
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        let id = uuid::Uuid::new_v4().simple().to_string();
        let client = self.client(timeout)?;
        let mut request = client
            .post("http://localhost/mcp")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL)
            .header("Mcp-Method", method);
        if method == "tools/call" {
            request = request.header(
                "Mcp-Name",
                params["name"].as_str().context("Missing tool name")?,
            );
        }
        // No retries: a transport failure after this point has an unknown outcome.
        let response = request
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .send()?;
        let status = response.status();
        let mut raw = Vec::new();
        response.take(8 * 1024 * 1024 + 1).read_to_end(&mut raw)?;
        ensure!(
            raw.len() <= 8 * 1024 * 1024,
            "Engine response exceeds 8 MiB; outcome unknown"
        );
        ensure!(
            status.as_u16() == 200,
            "Engine HTTP {status}: {}",
            String::from_utf8_lossy(&raw[..raw.len().min(2000)])
        );
        let envelope: Value = serde_json::from_slice(&raw)?;
        ensure!(
            envelope["id"] == id,
            "Engine returned a mismatched request ID"
        );
        if let Some(error) = envelope.get("error") {
            return Ok(json!({"isError":true,"error":error}));
        }
        envelope
            .get("result")
            .cloned()
            .context("Engine returned no result")
    }
}

pub fn setup(
    state: &Path,
    executable: &Path,
    workspace: &Path,
    expected: Option<&str>,
) -> Result<Value> {
    ensure!(
        cfg!(target_os = "linux"),
        "The managed runtime requires Linux /proc process identity"
    );
    let engine = Engine::new(state)?;
    let executable = absolute(executable)?.canonicalize()?;
    let workspace = absolute(workspace)?.canonicalize()?;
    ensure!(workspace.is_dir(), "workspace must be a directory");
    let digest = file_sha256(&executable)?;
    if let Some(expected) = expected {
        ensure!(digest == expected, "Engine checksum mismatch");
    }
    let version = Command::new(&executable).arg("--version").output()?;
    let version = String::from_utf8(version.stdout)?.trim().to_owned();
    ensure!(
        version == format!("chatgpt-exec-mcp {ENGINE_VERSION}"),
        "Unsupported engine: {version}"
    );
    let _lock = lock(&engine.state.join("daemon.lock"))?;
    if engine.ready() || engine.identity().is_some() {
        let old = engine.settings()?;
        ensure!(
            old.engine_sha256 == digest && old.workspace == workspace,
            "Engine is running; setup will not replace its configuration or terminate active work"
        );
        return Ok(json!({"configured":true,"unchanged":true,"state":state}));
    }
    let mut paths = vec![home()?.join(".local/bin"), home()?.join(".cargo/bin")];
    paths.extend(
        [
            "/usr/local/sbin",
            "/usr/local/bin",
            "/usr/sbin",
            "/usr/bin",
            "/sbin",
            "/bin",
        ]
        .map(PathBuf::from),
    );
    for path in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    let environment = BTreeMap::from([
        ("HOME".into(), home()?.to_string_lossy().into_owned()),
        (
            "USER".into(),
            std::env::var("USER").unwrap_or_else(|_| "coder".into()),
        ),
        (
            "PATH".into(),
            std::env::join_paths(paths)?.to_string_lossy().into_owned(),
        ),
        (
            "LANG".into(),
            std::env::var("LANG")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "C.UTF-8".into()),
        ),
        ("TERM".into(), "xterm-256color".into()),
    ]);
    let settings = Settings {
        version: 1,
        engine: executable,
        engine_sha256: digest.clone(),
        workspace: workspace.clone(),
        environment,
    };
    write_json(
        &state.join("engine.json"),
        &json!({
            "version":1,"workspace":workspace,"shell":"/bin/bash","output_store_dir":state.join("outputs"),
            "child_env":{"inherit":settings.environment.keys().collect::<Vec<_>>(),"rules":[]},
            "max_explicit_sessions":64,"max_exec_continuations":128,
            "explicit_session_idle_timeout":604800,"exec_continuation_idle_timeout":604800,
            "reaper_interval":60,"output_cap_bytes":1048576,"output_store_retention":604800,"output_store_max_bytes":4294967296u64
        }),
    )?;
    write_json(&state.join("settings.json"), &settings)?;
    Ok(
        json!({"configured":true,"state":state,"engine_version":version,"engine_sha256":digest,"workspace":workspace,
        "authority":"caller OS permissions; sudo and Docker remain available"}),
    )
}
