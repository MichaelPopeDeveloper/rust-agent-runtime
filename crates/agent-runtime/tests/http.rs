//! End-to-end HTTP tests: a real `axum` server on an ephemeral port, exercised
//! with `reqwest`. Agent routes are driven against the SDK's fake `claude`
//! fixture (no network, no API); the direct Messages API client is driven
//! against a tiny mock upstream that checks the headers the real API needs.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_runtime::{Config, app};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

fn fake_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../claude-agent-sdk/tests/fixtures/fake_cli.sh")
}

fn have_python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Serve `router` on an ephemeral loopback port; returns its base URL.
async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

fn test_config() -> Config {
    Config {
        cli_path: Some(fake_cli()),
        ..Config::default()
    }
}

#[tokio::test]
async fn health_is_open() {
    let base = serve(app(Config {
        auth_token: Some("secret".into()),
        ..test_config()
    }))
    .await;
    let body: Value = reqwest::get(format!("{base}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn protected_routes_require_the_token() {
    let base = serve(app(Config {
        auth_token: Some("secret".into()),
        ..test_config()
    }))
    .await;
    let client = reqwest::Client::new();
    let denied = client
        .post(format!("{base}/agent/run"))
        .json(&json!({"prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let wrong = client
        .post(format!("{base}/agent/run"))
        .bearer_auth("nope")
        .json(&json!({"prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
}

#[tokio::test]
async fn agent_run_returns_the_final_result() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let base = serve(app(test_config())).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/agent/run"))
        .json(&json!({"prompt": "hello runtime", "max_turns": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["result"], "echo: hello runtime");
    assert_eq!(body["session_id"], "fake-session");
    assert_eq!(body["is_error"], false);
    assert!(body["message_count"].as_u64().unwrap() >= 2);
}

#[tokio::test]
async fn agent_run_rejects_cwd_unless_allowed() {
    let base = serve(app(test_config())).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/agent/run"))
        .json(&json!({"prompt": "hi", "cwd": "/"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn agent_run_rejects_empty_prompt() {
    let base = serve(app(test_config())).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/agent/run"))
        .json(&json!({"prompt": "   "}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn agent_stream_emits_sse_events() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let base = serve(app(test_config())).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/agent/stream"))
        .json(&json!({"prompt": "stream me"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("event: assistant"), "{body}");
    assert!(body.contains("event: result"), "{body}");
    assert!(body.contains("echo: stream me"), "{body}");
    assert!(
        body.trim_end().ends_with("event: done\ndata: ") || body.contains("event: done"),
        "{body}"
    );
}

/// A stand-in for `api.anthropic.com` that checks the headers the real API
/// requires and returns a canned Messages response.
fn mock_upstream(hits: Arc<AtomicUsize>) -> Router {
    async fn messages(
        State(hits): State<Arc<AtomicUsize>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        hits.fetch_add(1, Ordering::SeqCst);
        assert_eq!(headers.get("x-api-key").unwrap(), "test-key");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert!(
            headers.get("authorization").is_none(),
            "must not send Authorization"
        );
        assert_eq!(body["messages"][0]["content"], "Hello, Claude!");
        Json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": body["model"],
            "content": [{"type": "text", "text": "Hello from the mock API"}],
        }))
    }
    Router::new()
        .route("/v1/messages", post(messages))
        .with_state(hits)
}

#[tokio::test]
async fn messages_complete_calls_the_api_correctly() {
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = serve(mock_upstream(hits.clone())).await;
    let base = serve(app(Config {
        anthropic_api_key: Some("test-key".into()),
        anthropic_base_url: upstream,
        ..Config::default()
    }))
    .await;

    let body: Value = reqwest::Client::new()
        .post(format!("{base}/messages/complete"))
        .json(&json!({"prompt": "Hello, Claude!", "model": "claude-sonnet-5-5"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["text"], "Hello from the mock API");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn messages_routes_need_a_server_key() {
    let base = serve(app(Config::default())).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/messages/complete"))
        .json(&json!({"prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
}
