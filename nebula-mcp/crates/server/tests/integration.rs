//! End-to-end integration tests driving the server over in-memory pipes.

use std::sync::Arc;

use nebula_mcp_core::config::{Config, ConfigStore};
use nebula_mcp_server::server::Server;
use nebula_mcp_tools::{build_registry, ToolServices};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// Build a server with a permissive-but-bounded policy for tests.
fn make_server(cancel: CancellationToken) -> Arc<Server> {
    let mut config = Config::default();
    config.security.allowed_paths = vec!["/tmp/**".into(), "/tmp".into()];
    config.security.allowed_commands = vec!["echo".into(), "sleep".into()];
    config.security.default_timeout_secs = 30;
    config.security.max_runtime_secs = 60;
    config.logging.level = "error".into();
    let store = ConfigStore::new(config);
    let registry = build_registry(&ToolServices::new());
    Arc::new(Server::new(registry, store, std::env::temp_dir(), cancel))
}

/// Feed newline-delimited requests through the server and collect responses.
async fn round_trip(server: Arc<Server>, input: String) -> Vec<Value> {
    let (client_side, server_side) = tokio::io::duplex(1 << 20);
    let reader_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut client_side = client_side;
        client_side.read_to_end(&mut buf).await.unwrap();
        buf
    });

    // `reader` borrows `input` only for the duration of this await.
    let reader = std::io::Cursor::new(input.into_bytes());
    server.serve(reader, server_side).await.unwrap();

    let bytes = reader_task.await.unwrap();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect()
}

fn line(v: Value) -> String {
    format!("{}\n", serde_json::to_string(&v).unwrap())
}

#[tokio::test]
async fn full_protocol_round_trip() {
    let server = make_server(CancellationToken::new());
    let input = [
        line(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"it","version":"1"}}})),
        line(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})),
        line(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})),
        line(serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"terminal.run","arguments":{"program":"echo","args":["integration"]}}})),
        line(serde_json::json!({"jsonrpc":"2.0","id":4,"method":"ping"})),
    ]
    .concat();

    let responses = round_trip(server, input).await;
    // Responses may be interleaved; index by id.
    let by_id = |id: i64| responses.iter().find(|r| r["id"] == id).unwrap();

    let init = by_id(1);
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");

    let list = by_id(2);
    let count = list["result"]["tools"].as_array().unwrap().len();
    assert!(count > 100, "expected 100+ tools, got {count}");

    let call = by_id(3);
    assert_eq!(call["result"]["isError"], false);
    assert!(call["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("integration"));

    let ping = by_id(4);
    assert!(ping["result"].is_object());
}

#[tokio::test]
async fn permission_denied_is_tool_error_not_protocol_error() {
    let server = make_server(CancellationToken::new());
    let input = line(serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"terminal.run","arguments":{"program":"rm","args":["-rf","/"]}}
    }));
    let responses = round_trip(server, input).await;
    let r = &responses[0];
    // Command not on allowlist -> tool result with isError, model-readable.
    assert_eq!(r["result"]["isError"], true);
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("command_not_allowed"), "got: {text}");
}

#[tokio::test]
async fn unknown_tool_is_protocol_error() {
    let server = make_server(CancellationToken::new());
    let input = line(serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"no.such.tool","arguments":{}}
    }));
    let responses = round_trip(server, input).await;
    assert!(responses[0]["error"].is_object());
    assert_eq!(responses[0]["error"]["code"], -32601);
}

#[tokio::test]
async fn cancellation_aborts_inflight_call() {
    if which::which("sleep").is_err() {
        return;
    }
    let server = make_server(CancellationToken::new());
    // Start a long sleep, then cancel it via notification.
    let input = [
        line(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"terminal.run","arguments":{"program":"sleep","args":["30"],"timeoutSecs":30}}})),
        line(serde_json::json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}})),
    ]
    .concat();

    let responses = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        round_trip(server, input),
    )
    .await
    .expect("cancellation should return well under the sleep duration");
    let r = &responses[0];
    assert_eq!(r["result"]["isError"], true);
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("cancelled"), "got: {text}");
}

#[tokio::test]
async fn concurrent_calls_all_answered() {
    let server = make_server(CancellationToken::new());
    let mut lines = Vec::new();
    for i in 0..25 {
        lines.push(line(serde_json::json!({
            "jsonrpc":"2.0","id": i,"method":"tools/call",
            "params":{"name":"terminal.run","arguments":{"program":"echo","args":[format!("n{i}")]}}
        })));
    }
    let responses = round_trip(server, lines.concat()).await;
    assert_eq!(responses.len(), 25);
    for i in 0..25 {
        let r = responses.iter().find(|r| r["id"] == i).unwrap();
        assert_eq!(r["result"]["isError"], false);
    }
}

#[tokio::test]
async fn progress_notifications_are_emitted() {
    if which::which("echo").is_err() {
        return;
    }
    let server = make_server(CancellationToken::new());
    // Supplying a progress token in `_meta` opts the call into progress updates.
    let input = line(serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{
            "name":"terminal.run",
            "arguments":{"program":"echo","args":["progress"]},
            "_meta":{"progressToken":"tok-42"}
        }
    }));
    let frames = round_trip(server, input).await;

    // There should be a progress notification carrying our token, plus the
    // final response.
    let progress = frames
        .iter()
        .find(|f| f["method"] == "notifications/progress")
        .expect("expected a notifications/progress frame");
    assert_eq!(progress["params"]["progressToken"], "tok-42");

    let response = frames
        .iter()
        .find(|f| f["id"] == 1)
        .expect("expected the tool response");
    assert_eq!(response["result"]["isError"], false);
}
