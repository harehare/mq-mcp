use mq_mcp::{
    FileAccess,
    server::{EngineConfig, HttpConfig, start_http},
};

async fn spawn_server_with_config(
    bearer_token: Option<String>,
) -> (String, tokio::task::JoinHandle<()>) {
    spawn_server_with_engine(bearer_token, EngineConfig::default()).await
}

async fn spawn_server_with_engine(
    bearer_token: Option<String>,
    engine: EngineConfig,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);

    let bind = addr.to_string();
    let handle = tokio::spawn({
        let bind = bind.clone();
        async move {
            let _ = start_http(
                HttpConfig {
                    bind,
                    allowed_hosts: vec![],
                    bearer_token,
                },
                engine,
            )
            .await;
        }
    });

    // Give the listener a moment to come up.
    for _ in 0..50 {
        if reqwest::Client::new()
            .get(format!("http://{addr}/mcp"))
            .header("Accept", "text/event-stream")
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    (format!("http://{addr}/mcp"), handle)
}

async fn spawn_server() -> (String, tokio::task::JoinHandle<()>) {
    spawn_server_with_config(None).await
}

async fn init_session(client: &reqwest::Client, url: &str) -> String {
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        )
        .send()
        .await
        .expect("initialize request");
    assert_eq!(response.status(), 200);
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .expect("session id header")
        .to_str()
        .expect("session id string")
        .to_string();

    let status = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .send()
        .await
        .expect("initialized notification")
        .status();
    assert_eq!(status, 202);

    session_id
}

#[tokio::test]
async fn test_streamable_http_initialize_and_call_tool() {
    let (url, handle) = spawn_server().await;
    let client = reqwest::Client::new();
    let session_id = init_session(&client, &url).await;

    let body = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .body(
            r##"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"extract_headings","arguments":{"markdown":"# Hello"}}}"##,
        )
        .send()
        .await
        .expect("tool call")
        .text()
        .await
        .expect("tool call body");

    assert!(body.contains("# Hello"), "unexpected tool response: {body}");

    handle.abort();
}

#[tokio::test]
async fn test_bearer_token_rejects_missing_or_wrong_token() {
    let (url, handle) = spawn_server_with_config(Some("secret".to_string())).await;
    let client = reqwest::Client::new();

    let no_token_status = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        )
        .send()
        .await
        .expect("request without token")
        .status();
    assert_eq!(no_token_status, 401);

    let wrong_token_status = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", "Bearer wrong")
        .body(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        )
        .send()
        .await
        .expect("request with wrong token")
        .status();
    assert_eq!(wrong_token_status, 401);

    handle.abort();
}

#[tokio::test]
async fn test_bearer_token_accepts_correct_token() {
    let (url, handle) = spawn_server_with_config(Some("secret".to_string())).await;
    let client = reqwest::Client::new();

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", "Bearer secret")
        .body(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        )
        .send()
        .await
        .expect("request with correct token");
    assert_eq!(response.status(), 200);

    handle.abort();
}

/// Sends one JSON-RPC request in an initialized session and returns the parsed response.
async fn rpc(
    client: &reqwest::Client,
    url: &str,
    session_id: &str,
    request: serde_json::Value,
) -> serde_json::Value {
    let body = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .body(request.to_string())
        .send()
        .await
        .expect("rpc request")
        .text()
        .await
        .expect("rpc body");

    // The response is a server-sent event (after an empty priming event) or plain JSON.
    let json = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .find(|data| data.starts_with('{'))
        .unwrap_or(body.trim());
    serde_json::from_str(json).unwrap_or_else(|e| panic!("unparseable response ({e}): {body}"))
}

async fn call_tool(
    client: &reqwest::Client,
    url: &str,
    session_id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    rpc(
        client,
        url,
        session_id,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }),
    )
    .await
}

#[tokio::test]
async fn test_files_are_readable_only_inside_the_configured_root() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("doc.md"),
        "# One\n\n## Two\n\n```rust\nfn main() {}\n```\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("outside.md"), "# Secret\n").unwrap();

    let (url, handle) = spawn_server_with_engine(
        None,
        EngineConfig {
            files: FileAccess::new([root], 1024 * 1024).unwrap(),
            ..EngineConfig::default()
        },
    )
    .await;
    let client = reqwest::Client::new();
    let session_id = init_session(&client, &url).await;
    let call = |name: &'static str, arguments: serde_json::Value| {
        call_tool(&client, &url, &session_id, name, arguments)
    };

    // A flat argument object, exactly as an MCP client sends it.
    let outline = call("document_outline", serde_json::json!({ "path": "doc.md" })).await;
    let result = &outline["result"];
    assert_eq!(result["isError"], false, "{outline}");
    assert_eq!(
        result["structuredContent"]["stats"]["headings"], 2,
        "{outline}"
    );
    assert_eq!(
        result["structuredContent"]["stats"]["code_languages"]["rust"],
        1
    );

    let code = call(
        "extract_code_blocks",
        serde_json::json!({ "path": "doc.md", "format": "json" }),
    )
    .await;
    assert_eq!(
        code["result"]["structuredContent"]["results"][0]["lang"], "rust",
        "{code}"
    );

    let outside = call(
        "extract_headings",
        serde_json::json!({ "path": "../outside.md" }),
    )
    .await;
    assert_eq!(outside["result"]["isError"], true, "{outside}");
    assert!(!outside.to_string().contains("Secret"), "{outside}");

    let listing = call("list_files", serde_json::json!({})).await;
    let files = listing["result"]["structuredContent"]["files"]
        .as_array()
        .unwrap();
    assert_eq!(files.len(), 1, "{listing}");
    assert!(files[0]["path"].as_str().unwrap().ends_with("doc.md"));

    let tools = rpc(
        &client,
        &url,
        &session_id,
        serde_json::json!({ "jsonrpc": "2.0", "id": 99, "method": "tools/list" }),
    )
    .await;
    let tools = tools["result"]["tools"].as_array().unwrap();
    let outline_tool = tools
        .iter()
        .find(|t| t["name"] == "document_outline")
        .unwrap();
    assert_eq!(outline_tool["outputSchema"]["type"], "object");

    handle.abort();
}
