use mq_mcp::server::{HttpConfig, start_http};

async fn spawn_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);

    let bind = addr.to_string();
    let handle = tokio::spawn({
        let bind = bind.clone();
        async move {
            let _ = start_http(HttpConfig {
                bind,
                allowed_hosts: vec![],
            })
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

#[tokio::test]
async fn test_streamable_http_initialize_and_call_tool() {
    let (url, handle) = spawn_server().await;
    let client = reqwest::Client::new();

    let response = client
        .post(&url)
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
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .send()
        .await
        .expect("initialized notification")
        .status();
    assert_eq!(status, 202);

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
