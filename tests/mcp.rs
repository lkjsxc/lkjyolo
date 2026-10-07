use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use lkjyolo::{
    files,
    gateway::{self, BackendConfig, Config, Server},
    mcp, util,
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    sync::Arc,
};
use tower::ServiceExt;

const TOKEN: &str = "integration-test-only-token-0123456789012345";
fn server(dir: &tempfile::TempDir) -> Arc<Server> {
    let token = dir.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    Arc::new(
        Server::new(Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            public_origin: "http://localhost".into(),
            backend: BackendConfig::Local {
                state: dir.path().join("state"),
                token_file: token,
            },
        })
        .unwrap(),
    )
}
fn request(value: Value, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/lkjyolo/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::from(value.to_string())).unwrap()
}
fn rpc(method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
}
async fn decode(response: axum::response::Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), 3 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn http_auth_origin_protocol_and_no_notification_execution() {
    let dir = tempfile::tempdir().unwrap();
    let app = gateway::router(server(&dir));
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        for token in [None,Some("wrong")] {
            let response=app.clone().oneshot(request(rpc("tools/list",json!({})),token)).await.unwrap();
            assert_eq!(response.status(),StatusCode::UNAUTHORIZED);
            assert!(response.headers().contains_key("www-authenticate"));
        }
        let mut req=request(rpc("tools/list",json!({})),Some(TOKEN));
        req.headers_mut().insert("origin","https://evil.example".parse().unwrap());
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(),StatusCode::FORBIDDEN);
        let response=app.clone().oneshot(request(rpc("initialize",json!({"protocolVersion":"2025-06-18"})),Some(TOKEN))).await.unwrap();
        let value=decode(response).await;
        assert_eq!(value["result"]["protocolVersion"],"2025-06-18");
        let response=app.clone().oneshot(request(rpc("tools/list",json!({})),Some(TOKEN))).await.unwrap();
        let value=decode(response).await;
        let tools=value["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(),5);
        assert_eq!(tools[0]["annotations"]["readOnlyHint"],false);
        let mutation=json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"exec_command","arguments":{"request_id":"should-not-run","cmd":"touch /tmp/should-not-run"}}});
        assert_eq!(app.clone().oneshot(request(mutation,Some(TOKEN))).await.unwrap().status(),StatusCode::BAD_REQUEST);
        assert!(!dir.path().join("state").exists());
        let mut req=request(rpc("tools/list",json!({})),Some(TOKEN));
        req.headers_mut().insert("mcp-protocol-version","2099-01-01".parse().unwrap());
        let response=app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(),StatusCode::BAD_REQUEST);
        assert_eq!(decode(response).await["error"]["code"],-32022);
        let response=app.oneshot(Request::builder().uri("/lkjyolo/mcp").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::METHOD_NOT_ALLOWED);
    });
}

#[test]
fn modern_headers_are_bound_to_request_body() {
    let mut headers = HeaderMap::new();
    headers.insert("mcp-protocol-version", "2026-07-28".parse().unwrap());
    headers.insert("mcp-method", "tools/call".parse().unwrap());
    headers.insert("mcp-name", "read_file".parse().unwrap());
    let mut request = rpc(
        "tools/call",
        json!({"name":"read_file","_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"test","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}}}),
    );
    assert_eq!(
        gateway::validate_protocol(&headers, &request).unwrap(),
        "2026-07-28"
    );
    request["params"]["name"] = json!("exec_command");
    assert!(gateway::validate_protocol(&headers, &request).is_err());
    headers.remove("mcp-protocol-version");
    assert!(gateway::validate_protocol(&headers, &request).is_err());
}

#[test]
fn strict_arguments_and_bounded_shell_payload() {
    assert!(mcp::validate("coder_delete_workspace", &json!({})).is_err());
    assert!(
        mcp::validate(
            "exec_command",
            &json!({"request_id":"valid","cmd":"true","extra":true})
        )
        .is_err()
    );
    assert!(mcp::validate("exec_command", &json!({"request_id":"../bad","cmd":"true"})).is_err());
    assert!(
        mcp::validate(
            "exec_command",
            &json!({"request_id":"valid","cmd":"x".repeat(65536)})
        )
        .is_err()
    );
    assert!(
        mcp::validate(
            "write_stdin",
            &json!({"request_id":"valid","session_id":"coral-newt","chars":"x","wait_seconds":15})
        )
        .is_err()
    );
    assert!(mcp::validate("apply_patch",&json!({"request_id":"valid","changes":[{"path":"/tmp/x","before_sha256":"a".repeat(64)}]})).is_err());
    assert!(
        mcp::validate(
            "exec_command",
            &json!({"request_id":"valid","cmd":"printf '%s' \"$(date)\"","workdir":"/tmp"})
        )
        .is_ok()
    );
}

#[test]
fn guarded_edits_preflight_batch_and_reject_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    fs::write(&a, "old").unwrap();
    fs::set_permissions(&a, fs::Permissions::from_mode(0o755)).unwrap();
    let change = |path: &std::path::Path, hash: &str, content: Value| json!({"path":path,"before_sha256":hash,"content":content});
    let mut dispatched = false;
    let wrong = json!({"changes":[change(&a,&util::sha256(b"old"),json!("new")),change(&b,&"a".repeat(64),json!("new"))]});
    assert!(files::apply(dir.path(), &wrong, &mut dispatched).is_err());
    assert!(!dispatched);
    assert_eq!(fs::read_to_string(&a).unwrap(), "old");
    let valid = json!({"changes":[change(&a,&util::sha256(b"old"),json!("new")),change(&b,"absent",json!("created"))]});
    files::apply(dir.path(), &valid, &mut dispatched).unwrap();
    assert!(dispatched);
    assert_eq!(fs::read_to_string(&a).unwrap(), "new");
    assert_eq!(
        fs::metadata(&a).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(files::apply(dir.path(), &valid, &mut false).is_err());
    let link = dir.path().join("link");
    symlink(&a, &link).unwrap();
    assert!(
        files::apply(
            dir.path(),
            &json!({"changes":[change(&link,&util::sha256(b"new"),json!("oops"))]}),
            &mut false
        )
        .is_err()
    );
    files::apply(
        dir.path(),
        &json!({"changes":[change(&b,&util::sha256(b"created"),Value::Null)]}),
        &mut false,
    )
    .unwrap();
    assert!(!b.exists());
}

#[test]
fn file_reads_ranges_binary_hash_and_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"abc\xffdef").unwrap();
    let read = files::read(files::ReadArgs {
        path: path.clone(),
        offset: 2,
        limit: 3,
    })
    .unwrap();
    assert_eq!(read["base64"], "Y/9k");
    assert_eq!(read["next_offset"], 5);
    assert_eq!(read["sha256"], util::sha256(b"abc\xffdef"));
    let directory = files::read(files::ReadArgs {
        path: dir.path().to_path_buf(),
        offset: 0,
        limit: 100,
    })
    .unwrap();
    assert_eq!(directory["entries"].as_array().unwrap().len(), 1);
}

#[test]
fn config_json_decodes_coder_backend() {
    let config:Config=serde_json::from_value(json!({"listen":"127.0.0.1:8787","public_origin":"https://coder.example.com","backend":"coder","owner_id":"owner-id","workspace":"owner/workspace"})).unwrap();
    assert!(Server::new(config).is_ok());
}
