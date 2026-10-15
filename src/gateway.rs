//! Stateless Streamable HTTP gateway. Coder credentials never reach the workspace,
//! disk, process arguments, receipts or logs. The upstream is the same OAuth resource.
use crate::bootstrap::Bootstrap;
use crate::{
    VERSION, mcp,
    routing::{Identity, OwnedWorkspaces, Routing, verify_owner},
};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine as _;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    net::{IpAddr, SocketAddr},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Semaphore;

const MAX_BODY: usize = 2 * 1024 * 1024;
#[derive(Debug)]
struct RejectedCredential;
impl std::fmt::Display for RejectedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Coder rejected the credential")
    }
}
impl std::error::Error for RejectedCredential {}
#[derive(Deserialize)]
pub struct Config {
    pub listen: SocketAddr,
    pub public_origin: String,
    #[serde(flatten)]
    pub backend: BackendConfig,
}
#[derive(Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum BackendConfig {
    Local {
        state: PathBuf,
        token_file: PathBuf,
    },
    Coder {
        #[serde(default)]
        owner_id: Option<String>,
        #[serde(default)]
        workspace: Option<String>,
        #[serde(default)]
        owned_workspaces: Option<OwnedWorkspaces>,
        #[serde(default)]
        bootstrap: Option<Bootstrap>,
        #[serde(default)]
        upstream_ip: Option<IpAddr>,
        #[serde(default = "default_cli")]
        cli: String,
    },
}
fn default_cli() -> String {
    "/home/coder/.local/bin/lkjyolo".into()
}

enum Backend {
    Local {
        state: PathBuf,
        token: Vec<u8>,
    },
    Coder {
        client: Client,
        upstream: String,
        routing: Routing,
        bootstrap: Option<Bootstrap>,
        cli: String,
    },
}
pub struct Server {
    origin: String,
    authority: String,
    backend: Backend,
    permits: Arc<Semaphore>,
}

impl Server {
    pub fn new(config: Config) -> Result<Self> {
        let url = reqwest::Url::parse(&config.public_origin)?;
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "public_origin must be an origin without credentials, query or path"
        );
        let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && loopback),
            "HTTPS required except for loopback development"
        );
        let origin = url.origin().ascii_serialization();
        let authority = origin.split_once("://").unwrap().1.to_owned();
        let backend = match config.backend {
            BackendConfig::Local { state, token_file } => {
                let meta = fs::metadata(&token_file)?;
                ensure!(
                    meta.is_file()
                        && meta.mode() & 0o077 == 0
                        && meta.uid() == unsafe { libc::geteuid() },
                    "Token file must be private and owned by this user"
                );
                let token = fs::read_to_string(token_file)?.trim().as_bytes().to_vec();
                ensure!(
                    token.len() >= 32
                        && token.len() <= 4096
                        && token.iter().all(|b| b.is_ascii_graphic()),
                    "Token must contain 32..4096 visible ASCII characters"
                );
                Backend::Local { state, token }
            }
            BackendConfig::Coder {
                owner_id,
                workspace,
                owned_workspaces,
                bootstrap,
                cli,
                upstream_ip,
            } => {
                ensure!(url.scheme() == "https", "Coder OAuth requires HTTPS");
                let routing = Routing::new(owner_id, workspace, owned_workspaces)?;
                ensure!(
                    cli.starts_with('/')
                        && cli
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)),
                    "cli must be a simple absolute path"
                );
                if let Some(ref bootstrap) = bootstrap {
                    bootstrap.validate(&origin, &cli)?;
                }
                let mut builder = Client::builder()
                    .timeout(Duration::from_secs(70))
                    .connect_timeout(Duration::from_secs(10))
                    .redirect(reqwest::redirect::Policy::none())
                    .no_proxy()
                    .user_agent(format!("lkjyolo/{VERSION}"));
                // Override DNS without changing the HTTPS origin, SNI or certificate checks.
                if let Some(ip) = upstream_ip {
                    builder = builder.resolve(
                        url.host_str().context("Missing host")?,
                        SocketAddr::new(ip, url.port_or_known_default().unwrap()),
                    );
                }
                let client = builder.build()?;
                Backend::Coder {
                    client,
                    upstream: format!("{origin}/api/experimental/mcp/http"),
                    routing,
                    bootstrap,
                    cli,
                }
            }
        };
        Ok(Self {
            origin,
            authority,
            backend,
            permits: Arc::new(Semaphore::new(16)),
        })
    }

    fn authorized(&self, bearer: &str) -> Result<Option<Identity>> {
        match &self.backend {
            Backend::Local { token, .. } => {
                let bytes = bearer.as_bytes();
                let mut diff = bytes.len() ^ token.len();
                for (i, b) in token.iter().enumerate() {
                    diff |= usize::from(*b ^ bytes.get(i).copied().unwrap_or(0));
                }
                Ok((diff == 0).then(|| Identity { id: "local".into() }))
            }
            Backend::Coder {
                client,
                upstream,
                routing,
                ..
            } => {
                let response = match upstream_call(
                    client,
                    upstream,
                    bearer,
                    "coder_get_authenticated_user",
                    json!({}),
                ) {
                    Ok(value) => value,
                    Err(e) if e.is::<RejectedCredential>() => return Ok(None),
                    Err(e) => return Err(e),
                };
                let body = upstream_body(&response);
                Ok(if response["isError"] == true {
                    None
                } else {
                    routing.identity(&body)
                })
            }
        }
    }

    fn call(&self, bearer: &str, identity: &Identity, name: &str, args: Value) -> Result<Value> {
        mcp::validate(name, &args)?;
        match &self.backend {
            Backend::Local { state, .. } => mcp::call(state, name, args),
            Backend::Coder {
                client,
                upstream,
                routing,
                bootstrap,
                cli,
                ..
            } => {
                let workspace = match routing {
                    Routing::Single {
                        owner_id,
                        workspace,
                    } => {
                        ensure!(identity.id == *owner_id, "Owner mismatch");
                        workspace.clone()
                    }
                    Routing::Owned(_) => {
                        let listed = upstream_call(
                            client,
                            upstream,
                            bearer,
                            "coder_list_workspaces",
                            json!({"owner":"me"}),
                        )?;
                        ensure!(
                            listed["isError"] != true,
                            "Could not list your Coder workspaces"
                        );
                        let id = routing.select(identity, &upstream_body(&listed))?;
                        let inspected = upstream_call(
                            client,
                            upstream,
                            bearer,
                            "coder_get_workspace",
                            json!({"workspace_id":id}),
                        )?;
                        ensure!(
                            inspected["isError"] != true,
                            "Could not verify your Coder workspace"
                        );
                        verify_owner(identity, &id, &upstream_body(&inspected))?;
                        id
                    }
                };
                let payload = base64::engine::general_purpose::STANDARD
                    .encode(serde_json::to_vec(&json!({"name":name,"arguments":args}))?);
                // Only a validated path and the standard base64 alphabet enter the shell.
                // User commands are data inside the payload; the OAuth token is never included.
                let prefix = bootstrap
                    .as_ref()
                    .map(|b| b.prefix(cli))
                    .unwrap_or_default();
                let command = format!("{prefix}{cli} mcp-call --json-base64 '{payload}'");
                let response = upstream_call(
                    client,
                    upstream,
                    bearer,
                    "coder_workspace_bash",
                    json!({"workspace":workspace,"command":command,"timeout_ms":60000}),
                )?;
                let body = upstream_body(&response);
                let output=body["output"].as_str().context("Coder did not return command output; recover using get_result with the original request_id")?;
                let value: Value=serde_json::from_str(output).context("Incomplete workspace response; recover using get_result with the original request_id")?;
                ensure!(value.is_object(), "Unexpected workspace response");
                Ok(value)
            }
        }
    }
}

fn upstream_body(result: &Value) -> Value {
    if result["structuredContent"].is_object() || result["structuredContent"].is_array() {
        return result["structuredContent"].clone();
    }
    if let Some(parts) = result["content"].as_array() {
        for part in parts {
            if part["type"] == "text"
                && let Some(text) = part["text"].as_str()
                && let Ok(value) = serde_json::from_str::<Value>(text)
                && (value.is_object() || value.is_array())
            {
                return value;
            }
        }
    }
    Value::Null
}

fn upstream_call(
    client: &Client,
    url: &str,
    bearer: &str,
    name: &str,
    arguments: Value,
) -> Result<Value> {
    // Legacy Coder deployments require an initialized MCP session. Negotiate
    // per operation so sessions and credentials cannot cross between callers.
    let initialize_id = uuid::Uuid::new_v4().to_string();
    let response=client.post(url).bearer_auth(bearer)
        .header("MCP-Protocol-Version","2025-06-18").header(header::ACCEPT,"application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":initialize_id,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"lkjyolo","version":VERSION}}})).send()?;
    let session = response.headers().get("mcp-session-id").cloned();
    ensure!(
        session.as_ref().is_none_or(|s| s.as_bytes().len() <= 8192),
        "Coder returned an oversized session header"
    );
    let initialized = decode_upstream_response(response, &initialize_id)?;
    let version = initialized["protocolVersion"]
        .as_str()
        .context("Coder did not negotiate an MCP version")?;
    ensure!(
        matches!(
            version,
            "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
        ),
        "Coder negotiated an unsupported MCP version"
    );
    let request = || {
        let mut request = client
            .post(url)
            .bearer_auth(bearer)
            .header("MCP-Protocol-Version", version)
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(session) = &session {
            request = request.header("Mcp-Session-Id", session.clone());
        }
        request
    };
    let response = request()
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()?;
    check_upstream_status(&response)?;

    // Never automatically replay a tool call, even if its session expires.
    let id = uuid::Uuid::new_v4().to_string();
    let response = request()
        .json(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
        .send()?;
    decode_upstream_response(response, &id)
}

fn check_upstream_status(response: &reqwest::blocking::Response) -> Result<()> {
    // Never interpolate response bodies (which may contain secrets) into errors.
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(RejectedCredential.into());
    }
    ensure!(
        response.status().is_success(),
        "Coder upstream returned HTTP {}",
        response.status().as_u16()
    );
    Ok(())
}

fn decode_upstream_response(response: reqwest::blocking::Response, id: &str) -> Result<Value> {
    check_upstream_status(&response)?;
    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
    let mut bytes = Vec::new();
    response
        .take((MAX_BODY + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_BODY,
        "Coder response exceeded size limit; recover using get_result"
    );
    let value: Value = if is_sse {
        let text = std::str::from_utf8(&bytes)?;
        let mut result = None;
        let text = text.replace("\r\n", "\n");
        for event in text.split("\n\n") {
            let data = event
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("data:")
                        .map(|v| v.strip_prefix(' ').unwrap_or(v))
                })
                .collect::<Vec<_>>()
                .join("\n");
            if let Ok(v) = serde_json::from_str::<Value>(&data)
                && v["id"] == id
            {
                result = Some(v);
            }
        }
        result.context("No matching Coder response")?
    } else {
        serde_json::from_slice(&bytes)?
    };
    ensure!(
        value["jsonrpc"] == "2.0" && value["id"] == id,
        "Mismatched Coder response"
    );
    ensure!(
        value.get("error").is_none(),
        "Coder RPC failed (code {}); inspect the original request_id before retrying",
        value["error"]["code"].as_i64().unwrap_or(0)
    );
    value.get("result").cloned().context("Missing Coder result")
}

pub fn router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/lkjyolo/mcp", post(handle))
        .route(
            "/lkjyolo/healthz",
            get(|| async { axum::Json(json!({"status":"ok","version":VERSION})) }),
        )
        .route(
            "/.well-known/oauth-protected-resource/lkjyolo/mcp",
            get(metadata),
        )
        .with_state(server)
}
async fn metadata(State(server): State<Arc<Server>>) -> Response {
    match server.backend {
        Backend::Coder {..} => axum::Json(json!({"resource":server.origin,"authorization_servers":[server.origin],"bearer_methods_supported":["header"]})).into_response(),
        Backend::Local {..} => StatusCode::NOT_FOUND.into_response(),
    }
}
fn error(status: StatusCode, id: Value, code: i64, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})),
    )
        .into_response()
}
fn unauthorized(server: &Server) -> Response {
    let mut response = error(
        StatusCode::UNAUTHORIZED,
        Value::Null,
        -32001,
        "Authentication required",
    );
    let challenge = match server.backend {
        Backend::Coder { .. } => format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/lkjyolo/mcp\"",
            server.origin
        ),
        Backend::Local { .. } => "Bearer".into(),
    };
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, challenge.parse().unwrap());
    response
}
fn header_str<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key)?.to_str().ok()
}

pub fn validate_protocol(headers: &HeaderMap, request: &Value) -> Result<&'static str> {
    let version = header_str(headers, "mcp-protocol-version").unwrap_or("2025-03-26");
    let version = *mcp::VERSIONS
        .iter()
        .find(|v| **v == version)
        .context("Unsupported protocol version")?;
    if version == "2026-07-28" {
        let meta = &request["params"]["_meta"];
        ensure!(
            meta["io.modelcontextprotocol/protocolVersion"] == version
                && header_str(headers, "mcp-method") == request["method"].as_str(),
            "Header mismatch"
        );
        ensure!(
            meta["io.modelcontextprotocol/clientInfo"]["name"].is_string()
                && meta["io.modelcontextprotocol/clientInfo"]["version"].is_string()
                && meta["io.modelcontextprotocol/clientCapabilities"].is_object(),
            "Client metadata required"
        );
        if matches!(
            request["method"].as_str(),
            Some("tools/call" | "resources/read" | "prompts/get")
        ) {
            let encoded = header_str(headers, "mcp-name").context("Mcp-Name required")?;
            let name = if let Some(value) = encoded
                .strip_prefix("=?base64?")
                .and_then(|s| s.strip_suffix("?="))
            {
                String::from_utf8(base64::engine::general_purpose::STANDARD.decode(value)?)?
            } else {
                encoded.into()
            };
            let expected = request["params"]["name"]
                .as_str()
                .or(request["params"]["uri"].as_str());
            ensure!(Some(name.as_str()) == expected, "Mcp-Name mismatch");
        }
    } else if request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] == "2026-07-28"
    {
        bail!("Header mismatch");
    }
    Ok(version)
}

async fn handle(State(server): State<Arc<Server>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    if header_str(&headers, "host") != Some(server.authority.as_str())
        || (headers.contains_key("origin")
            && header_str(&headers, "origin") != Some(server.origin.as_str()))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(bearer) = header_str(&headers, "authorization")
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| !s.is_empty() && s.len() <= 8192)
        .map(str::to_owned)
    else {
        return unauthorized(&server);
    };
    let Ok(permit) = server.permits.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let auth_server = server.clone();
    let auth_token = bearer.clone();
    let auth = tokio::task::spawn_blocking(move || {
        let result = auth_server.authorized(&auth_token);
        (permit, result)
    })
    .await;
    let (permit, identity) = match auth {
        Ok((permit, Ok(Some(identity)))) => (permit, identity),
        Ok((_, Ok(None))) => return unauthorized(&server),
        // Do not leak upstream errors or bearer credentials to clients/logs.
        failure => {
            // Only the error chain is logged. The bridge never puts bearer tokens,
            // request/response bodies, or user commands in these errors.
            match failure {
                Ok((_, Err(error))) => {
                    eprintln!("lkjyolo: authentication upstream failed: {error:#}");
                }
                Err(error) => eprintln!("lkjyolo: authentication task failed: {error}"),
                _ => unreachable!(),
            }
            return error(
                StatusCode::BAD_GATEWAY,
                Value::Null,
                -32603,
                "Authentication upstream unavailable; reconnect if your token expired",
            );
        }
    };
    if !header_str(&headers, "content-type")
        .is_some_and(|s| s.split(';').next() == Some("application/json"))
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    if !header_str(&headers, "accept")
        .is_some_and(|s| s.contains("application/json") && s.contains("text/event-stream"))
    {
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }
    let bytes = match tokio::time::timeout(Duration::from_secs(15), to_bytes(body, MAX_BODY)).await
    {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    let request: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, Value::Null, -32700, "Invalid JSON"),
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if request["jsonrpc"] != "2.0"
        || !request["method"].is_string()
        || (!id.is_null() && !id.is_string() && !id.is_number())
        || request.get("params").is_some_and(|p| !p.is_object())
    {
        return error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            -32600,
            "Invalid request",
        );
    }
    let version = match validate_protocol(&headers, &request) {
        Ok(v) => v,
        Err(_) => {
            let unsupported = header_str(&headers, "mcp-protocol-version")
                .is_some_and(|v| !mcp::VERSIONS.contains(&v));
            if unsupported {
                return (StatusCode::BAD_REQUEST,axum::Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32022,"message":"Unsupported protocol version","data":{"requested":header_str(&headers,"mcp-protocol-version"),"supported":mcp::VERSIONS}}}))).into_response();
            }
            return error(
                StatusCode::BAD_REQUEST,
                id,
                -32020,
                "MCP headers and metadata must match",
            );
        }
    };
    let method = request["method"].as_str().unwrap();
    if request.get("id").is_none() {
        // Notifications must never execute a mutating request.
        return if method.starts_with("notifications/") {
            StatusCode::ACCEPTED.into_response()
        } else {
            error(
                StatusCode::BAD_REQUEST,
                Value::Null,
                -32600,
                "Request ID required",
            )
        };
    }
    let result = match method {
        "initialize" if version != "2026-07-28" => {
            let requested = request["params"]["protocolVersion"]
                .as_str()
                .unwrap_or("2025-06-18");
            let negotiated = if mcp::VERSIONS[1..].contains(&requested) {
                requested
            } else {
                "2025-11-25"
            };
            json!({"protocolVersion":negotiated,"capabilities":{"tools":{}},"serverInfo":mcp::info(),"instructions":mcp::INSTRUCTIONS})
        }
        "server/discover" => {
            json!({"resultType":"complete","supportedVersions":mcp::VERSIONS,"capabilities":{"tools":{}},"_meta":{"io.modelcontextprotocol/serverInfo":mcp::info()},"instructions":mcp::INSTRUCTIONS,"ttlMs":300000})
        }
        "ping" => json!({}),
        "tools/list" => json!({"resultType":"complete","tools":mcp::tools()}),
        "tools/call" => {
            let Some(name) = request["params"]["name"].as_str().map(str::to_owned) else {
                return error(StatusCode::OK, id, -32602, "Tool name required");
            };
            let args = request["params"]
                .get("arguments")
                .cloned()
                .unwrap_or(json!({}));
            if let Err(e) = mcp::validate(&name, &args) {
                return error(StatusCode::OK, id, -32602, &e.to_string());
            }
            let call_server = server.clone();
            // Move the permit into the blocking task: client disconnect cannot
            // release capacity while the upstream call is still in flight.
            match tokio::task::spawn_blocking(move || {
                let _permit = permit;
                call_server.call(&bearer, &identity, &name, args)
            })
            .await
            {
                Ok(Ok(value)) => mcp::wrapped(value),
                Ok(Err(e)) => mcp::wrapped(
                    json!({"isError":true,"error":e.to_string(),"next":"Use get_result with the original request_id after a transport error. Do not repeat uncertain mutations with a new ID."}),
                ),
                Err(_) => mcp::wrapped(
                    json!({"isError":true,"error":"Worker unavailable; recover by original request_id"}),
                ),
            }
        }
        _ => {
            return error(
                if version == "2026-07-28" {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::OK
                },
                id,
                -32601,
                "Method not found",
            );
        }
    };
    let mut response = axum::Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

pub fn serve(config: Config) -> Result<()> {
    let address = config.listen;
    // reqwest's blocking client must be created and dropped outside a Tokio context.
    let server = Arc::new(Server::new(config)?);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(address).await?;
        eprintln!("lkjyolo {VERSION} listening on {}", listener.local_addr()?);
        axum::serve(listener, router(server.clone()))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[test]
    fn coder_bridge_checks_owner_and_keeps_bearer_out_of_workspace_command() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let args = json!({"request_id":"bridge-test","cmd":"printf '%s' \"$(date)\"; printf '\\n'","workdir":"/tmp"});
        let expected = args.clone();
        let mock = std::thread::spawn(move || {
            for step in 0..9 {
                let index = step / 3;
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut socket);
                let mut headers = String::new();
                let mut size = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        size = value.trim().parse::<usize>().unwrap();
                    }
                    headers.push_str(&line);
                }
                assert!(
                    headers
                        .to_lowercase()
                        .contains("authorization: bearer test-bearer-only-for-coder")
                );
                let mut bytes = vec![0; size];
                reader.read_exact(&mut bytes).unwrap();
                let rpc: Value = serde_json::from_slice(&bytes).unwrap();
                let session = format!("mcp-session-test-{index}");
                if step % 3 == 0 {
                    assert_eq!(rpc["method"], "initialize");
                    assert_eq!(rpc["params"]["protocolVersion"], "2025-06-18");
                    assert!(!headers.to_lowercase().contains("mcp-session-id:"));
                    // Cover both a stateful legacy server and a stateless server.
                    let session_header = if index == 1 {
                        String::new()
                    } else {
                        format!("Mcp-Session-Id: {session}\r\n")
                    };
                    let result = json!({"jsonrpc":"2.0","id":rpc["id"],"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"Coder","version":"test"}}}).to_string();
                    write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session_header}Content-Length: {}\r\nConnection: close\r\n\r\n{}",result.len(),result).unwrap();
                    continue;
                }
                let headers = headers.to_lowercase();
                assert!(headers.contains("mcp-protocol-version: 2025-03-26"));
                if index == 1 {
                    assert!(!headers.contains("mcp-session-id:"));
                } else {
                    assert!(headers.contains(&format!("mcp-session-id: {session}")));
                }
                if step % 3 == 1 {
                    assert_eq!(rpc["method"], "notifications/initialized");
                    assert!(rpc.get("id").is_none());
                    write!(
                        socket,
                        "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    continue;
                }
                assert_eq!(rpc["method"], "tools/call");
                let body = if index < 2 {
                    assert_eq!(rpc["params"]["name"], "coder_get_authenticated_user");
                    json!({"id":if index==0 {"expected-owner"} else {"other-owner"},"status":"active"})
                } else {
                    assert_eq!(rpc["params"]["name"], "coder_workspace_bash");
                    assert_eq!(rpc["params"]["arguments"]["workspace"], "owner/workspace");
                    let command = rpc["params"]["arguments"]["command"].as_str().unwrap();
                    assert!(!command.contains("test-bearer"));
                    assert!(!command.contains("$(date)"));
                    let encoded = command
                        .strip_prefix("/home/coder/.local/bin/lkjyolo mcp-call --json-base64 '")
                        .unwrap()
                        .strip_suffix('\'')
                        .unwrap();
                    let decoded: Value = serde_json::from_slice(
                        &base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .unwrap(),
                    )
                    .unwrap();
                    assert_eq!(decoded, json!({"name":"exec_command","arguments":expected}));
                    json!({"output":json!({"request_id":"bridge-test","status":"done","exit_code":7,"output":"verbatim result"}).to_string(),"exit_code":0})
                };
                // Coder tools actually return JSON inside a text content block.
                let result=json!({"jsonrpc":"2.0","id":rpc["id"],"result":{"content":[{"type":"text","text":body.to_string()}],"isError":false}}).to_string();
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",result.len(),result).unwrap();
            }
        });
        let server = Server {
            origin: "https://coder.example.com".into(),
            authority: "coder.example.com".into(),
            permits: Arc::new(Semaphore::new(1)),
            backend: Backend::Coder {
                client: Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
                upstream: format!("http://{address}"),
                routing: Routing::Single {
                    owner_id: "expected-owner".into(),
                    workspace: "owner/workspace".into(),
                },
                bootstrap: None,
                cli: default_cli(),
            },
        };
        let identity = server
            .authorized("test-bearer-only-for-coder")
            .unwrap()
            .unwrap();
        assert!(
            server
                .authorized("test-bearer-only-for-coder")
                .unwrap()
                .is_none()
        );
        assert!(
            server
                .call(
                    "test-bearer-only-for-coder",
                    &identity,
                    "coder_delete_workspace",
                    json!({})
                )
                .is_err()
        );
        let result = server
            .call(
                "test-bearer-only-for-coder",
                &identity,
                "exec_command",
                args,
            )
            .unwrap();
        assert_eq!(result["exit_code"], 7);
        assert_eq!(result["output"], "verbatim result");
        mock.join().unwrap();
    }
    #[test]
    fn owned_routing_isolates_users_and_rejects_foreign_workspace_before_execution() {
        const ALICE: &str = "10000000-0000-0000-0000-000000000001";
        const BOB: &str = "10000000-0000-0000-0000-000000000002";
        const AWORK: &str = "20000000-0000-0000-0000-000000000001";
        const BWORK: &str = "20000000-0000-0000-0000-000000000002";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mock = std::thread::spawn(move || {
            let mut executions = Vec::new();
            for _ in 0..33 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut socket);
                let mut size = 0;
                let mut bearer = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            size = value.trim().parse().unwrap();
                        }
                        if key.eq_ignore_ascii_case("authorization") {
                            bearer = value.trim().to_owned();
                        }
                    }
                }
                let mut bytes = vec![0; size];
                reader.read_exact(&mut bytes).unwrap();
                let rpc: Value = serde_json::from_slice(&bytes).unwrap();
                let alice = bearer == "Bearer alice-credential";
                let hostile = bearer == "Bearer foreign-list-credential";
                let user = if alice { ALICE } else { BOB };
                let workspace = if alice || hostile { AWORK } else { BWORK };
                let result = match rpc["method"].as_str().unwrap() {
                    "initialize" => {
                        json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"Coder","version":"test"}})
                    }
                    "notifications/initialized" => {
                        write!(socket,"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                        continue;
                    }
                    "tools/call" => {
                        let body = match rpc["params"]["name"].as_str().unwrap() {
                            "coder_get_authenticated_user" => json!({"id":user,"status":"active"}),
                            "coder_list_workspaces" => {
                                assert_eq!(rpc["params"]["arguments"], json!({"owner":"me"}));
                                json!([{"id":workspace,"name":"lkjyolo"}])
                            }
                            "coder_get_workspace" => {
                                assert_eq!(rpc["params"]["arguments"]["workspace_id"], workspace);
                                json!({"id":workspace,"owner_id":if hostile {ALICE} else {user}})
                            }
                            "coder_workspace_bash" => {
                                assert!(!hostile, "A foreign workspace must never execute");
                                assert_eq!(rpc["params"]["arguments"]["workspace"], workspace);
                                let command =
                                    rpc["params"]["arguments"]["command"].as_str().unwrap();
                                assert!(!command.contains("credential"));
                                let encoded = command.split('\'').nth(1).unwrap();
                                let payload: Value = serde_json::from_slice(
                                    &base64::engine::general_purpose::STANDARD
                                        .decode(encoded)
                                        .unwrap(),
                                )
                                .unwrap();
                                assert_eq!(payload["name"], "get_result");
                                assert_eq!(payload["arguments"]["request_id"], "same-id");
                                executions.push(workspace.to_owned());
                                json!({"output":json!({"request_id":"same-id","output":user}).to_string(),"exit_code":0})
                            }
                            other => panic!("Unexpected tool {other}"),
                        };
                        json!({"content":[{"type":"text","text":body.to_string()}],"isError":false})
                    }
                    other => panic!("Unexpected method {other}"),
                };
                let body = json!({"jsonrpc":"2.0","id":rpc["id"],"result":result}).to_string();
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
            assert_eq!(executions, vec![AWORK.to_owned(), BWORK.to_owned()]);
        });
        let server = Server {
            origin: "https://coder.example.com".into(),
            authority: "coder.example.com".into(),
            permits: Arc::new(Semaphore::new(16)),
            backend: Backend::Coder {
                client: Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .unwrap(),
                upstream: format!("http://{address}"),
                routing: Routing::new(None, None, Some(serde_json::from_value(json!({})).unwrap()))
                    .unwrap(),
                bootstrap: None,
                cli: default_cli(),
            },
        };
        for (token, expected) in [("alice-credential", ALICE), ("bob-credential", BOB)] {
            let identity = server.authorized(token).unwrap().unwrap();
            let result = server
                .call(
                    token,
                    &identity,
                    "get_result",
                    json!({"request_id":"same-id"}),
                )
                .unwrap();
            assert_eq!(result["output"], expected);
        }
        let token = "foreign-list-credential";
        let identity = server.authorized(token).unwrap().unwrap();
        assert!(
            server
                .call(
                    token,
                    &identity,
                    "get_result",
                    json!({"request_id":"same-id"})
                )
                .unwrap_err()
                .to_string()
                .contains("ownership")
        );
        mock.join().unwrap();
    }
}
