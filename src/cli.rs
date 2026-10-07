use crate::{engine, journal::Journal, package, runtime, util::*};
use anyhow::{Result, ensure};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Persistent native execution through authenticated Coder"
)]
pub struct Cli {
    #[arg(long, env = "LKJYOLO_STATE", global = true)]
    pub state: Option<PathBuf>,
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Args, Debug)]
pub struct RequestOptions {
    #[arg(long)]
    pub request_id: Option<String>,
    #[arg(long, default_value_t=45, value_parser=clap::value_parser!(u64).range(0..=45))]
    pub client_wait: u64,
    #[arg(long, default_value_t=4000, value_parser=clap::value_parser!(u32).range(1..=262144))]
    pub max_output_tokens: u32,
}

#[derive(Args, Debug)]
pub struct SessionOptions {
    #[command(flatten)]
    pub request: RequestOptions,
    pub session_id: String,
}

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Serve the five-tool authenticated Streamable HTTP MCP
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Workspace-side JSON transport for the authenticated gateway
    McpCall {
        #[arg(long)]
        json_base64: String,
    },
    /// Configure the pinned engine without interrupting active work
    Setup {
        #[arg(long)]
        engine: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        sha256: Option<String>,
    },
    /// Install this native binary and the pinned engine in ~/.local
    Install {
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Build the existing Home Coder plugin ZIP without Python
    PackagePlugin {
        #[arg(long, default_value = "plugins/home-coder")]
        source: PathBuf,
        #[arg(long, default_value = "dist")]
        output: PathBuf,
    },
    Doctor,
    Tools,
    Exec {
        #[command(flatten)]
        request: RequestOptions,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        tty: bool,
        #[arg(long, default_value_t=1000, value_parser=clap::value_parser!(u64).range(10..=40000))]
        yield_ms: u64,
        command: String,
    },
    Start {
        #[command(flatten)]
        request: RequestOptions,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        pipe: bool,
        command: Option<String>,
    },
    Input {
        #[command(flatten)]
        session: SessionOptions,
        chars: String,
    },
    Poll(SessionOptions),
    Interrupt(SessionOptions),
    Wait {
        #[command(flatten)]
        session: SessionOptions,
        #[arg(long,default_value_t=35,value_parser=clap::value_parser!(u64).range(15..=40))]
        seconds: u64,
    },
    Call {
        tool: String,
        #[arg(long)]
        json: String,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long,default_value_t=45,value_parser=clap::value_parser!(u64).range(0..=45))]
        client_wait: u64,
    },
    Result {
        request_id: String,
        #[arg(long,default_value_t=0,value_parser=clap::value_parser!(u64).range(0..=45))]
        wait: u64,
    },
    Requests {
        #[arg(long,default_value_t=30,value_parser=clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
    },
    Sessions,
    Log {
        path: PathBuf,
        #[arg(long,default_value_t=0,value_parser=clap::value_parser!(u64).range(0..=i64::MAX as u64))]
        offset: u64,
        #[arg(long,default_value_t=16384,value_parser=clap::value_parser!(u64).range(1..=1048576))]
        limit: u64,
    },
    #[command(name = "_worker", hide = true)]
    Worker {
        request_id: String,
    },
}

pub fn decode_json(argument: &str) -> Result<Value> {
    let data = if argument == "-" {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        input
    } else if let Some(path) = argument.strip_prefix('@') {
        fs::read_to_string(path)?
    } else {
        argument.to_owned()
    };
    let value: Value = serde_json::from_str(&data)?;
    ensure!(value.is_object(), "Arguments must be a JSON object");
    Ok(value)
}

fn request(state: &Path, tool: &str, mut value: Value, options: &RequestOptions) -> Result<Value> {
    value["max_output_tokens"] = json!(options.max_output_tokens);
    runtime::submit(
        state,
        tool,
        &value,
        options.request_id.as_deref(),
        options.client_wait,
    )
}

pub fn dispatch(cli: Cli) -> Result<Option<Value>> {
    if let Action::Serve { config } = &cli.action {
        crate::gateway::serve(serde_json::from_slice(&fs::read(config)?)?)?;
        return Ok(None);
    }
    let state = absolute(&cli.state.unwrap_or(home()?.join(".local/state/lkjyolo")))?;
    let value = match cli.action {
        Action::Serve { .. } => unreachable!(),
        Action::McpCall { json_base64 } => {
            use base64::Engine as _;
            ensure!(json_base64.len() <= 3 * 1024 * 1024, "Payload too large");
            let bytes = base64::engine::general_purpose::STANDARD.decode(json_base64)?;
            let value: Value = serde_json::from_slice(&bytes)?;
            let name = value["name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Tool name required"))?;
            crate::mcp::call(
                &state,
                name,
                value.get("arguments").cloned().unwrap_or(json!({})),
            )?
        }
        Action::Setup {
            engine,
            workspace,
            sha256,
        } => engine::setup(&state, &engine, &workspace, sha256.as_deref())?,
        Action::Install { workspace } => {
            package::install(&state, &workspace.unwrap_or(home()?.join("workspace")))?
        }
        Action::PackagePlugin { source, output } => package::plugin_zip(&source, &output)?,
        Action::Doctor => runtime::doctor(&state)?,
        Action::Tools => {
            let engine = engine::Engine::new(&state)?;
            engine.ensure()?;
            engine.rpc("tools/list", json!({}), 50)?
        }
        Action::Exec {
            request: options,
            cwd,
            tty,
            yield_ms,
            command,
        } => {
            let mut value = json!({"cmd":command,"tty":tty,"yield_time_ms":yield_ms});
            if let Some(cwd) = cwd {
                value["workdir"] = json!(cwd);
            }
            request(&state, "exec_command", value, &options)?
        }
        Action::Start {
            request: options,
            cwd,
            pipe,
            command,
        } => {
            let mut value = json!({"tty":!pipe});
            if let Some(cwd) = cwd {
                value["workdir"] = json!(cwd);
            }
            if let Some(command) = command {
                value["cmd"] = json!(command);
            }
            request(&state, "start_session", value, &options)?
        }
        Action::Input { session, chars } => request(
            &state,
            "write_stdin",
            json!({"session_id":session.session_id,"chars":chars}),
            &session.request,
        )?,
        Action::Poll(session) => request(
            &state,
            "write_stdin",
            json!({"session_id":session.session_id,"chars":""}),
            &session.request,
        )?,
        Action::Interrupt(session) => request(
            &state,
            "write_stdin",
            json!({"session_id":session.session_id,"chars":"\u{3}"}),
            &session.request,
        )?,
        Action::Wait { session, seconds } => request(
            &state,
            "wait_for_exit",
            json!({"session_id":session.session_id,"wait_seconds":seconds}),
            &session.request,
        )?,
        Action::Call {
            tool,
            json,
            request_id,
            client_wait,
        } => runtime::submit(
            &state,
            &tool,
            &decode_json(&json)?,
            request_id.as_deref(),
            client_wait,
        )?,
        Action::Result { request_id, wait } => runtime::collect(&state, &request_id, wait)?,
        Action::Requests { limit } => {
            let journal = Journal::new(&state)?;
            let mut results = Vec::new();
            for row in journal.rows(limit as usize)? {
                let mut value = journal.reconcile(row)?.receipt()?;
                value.as_object_mut().unwrap().remove("result");
                results.push(value);
            }
            json!({"requests":results})
        }
        Action::Sessions => runtime::sessions(&state)?,
        Action::Log {
            path,
            offset,
            limit,
        } => runtime::read_log(&state, &path, offset, limit)?,
        Action::Worker { request_id } => {
            runtime::run_worker(&state, &request_id)?;
            return Ok(None);
        }
    };
    Ok(Some(value))
}
