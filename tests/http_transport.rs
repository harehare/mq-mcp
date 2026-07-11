use std::path::PathBuf;

use mq_mcp::server::{HttpConfig, start_http};

async fn spawn_server_with_db(db_path: Option<PathBuf>) -> (String, tokio::task::JoinHandle<()>) {
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
                },
                db_path,
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
    spawn_server_with_db(None).await
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
async fn test_streamable_http_db_index_then_sql() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), "# Title\n\nHello world\n").unwrap();
    let db_path = dir.path().join("store.mq-db");

    let (url, handle) = spawn_server_with_db(Some(db_path)).await;
    let client = reqwest::Client::new();
    let session_id = init_session(&client, &url).await;

    let dir_path = dir.path().to_string_lossy().to_string();
    let index_body = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .body(format!(
            r##"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"db_index","arguments":{{"paths":["{dir_path}"]}}}}}}"##
        ))
        .send()
        .await
        .expect("db_index call")
        .text()
        .await
        .expect("db_index body");
    assert!(
        index_body.contains("a.md"),
        "unexpected db_index response: {index_body}"
    );

    let sql_body = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .body(
            r##"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"db_sql","arguments":{"query":"SELECT content FROM blocks WHERE block_type = 'heading'"}}}"##,
        )
        .send()
        .await
        .expect("db_sql call")
        .text()
        .await
        .expect("db_sql body");
    assert!(
        sql_body.contains("Title"),
        "unexpected db_sql response: {sql_body}"
    );

    handle.abort();
}
