//! Tests for the real subprocess transport: spawning a process, framing its
//! NDJSON stdout, passing env and flags, and propagating a non-zero exit as a
//! typed error. These drive a small Python stand-in for the `claude` CLI (see
//! `tests/fixtures/fake_cli.py`), so they need `python3` but no network/API.

use std::path::PathBuf;

use claude_agent_sdk::{query, ClaudeAgentOptions, ClaudeSDKClient, ContentBlock, Error, Message};
use futures::StreamExt;

fn fake_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_cli.sh")
}

fn have_python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn subprocess_round_trip_with_fake_cli() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let options = ClaudeAgentOptions::builder()
        .cli_path(fake_cli())
        .env("CLAUDE_FAKE_MODEL", "claude-fake-xyz")
        .build();

    let mut stream = query("hello there", options).await.expect("spawns");

    let mut echoed = None;
    let mut cost = None;
    let mut saw_init = false;
    while let Some(message) = stream.next().await {
        match message.expect("message ok") {
            Message::System(system) if system.subtype == "init" => {
                saw_init = true;
                // The SDK must have set the SDK entrypoint in the child env.
                assert_eq!(system.data["entrypoint_seen"], "sdk-cli");
                assert_eq!(system.data["model"], "claude-fake-xyz");
                // CLAUDECODE must be stripped so the child is not seen as nested.
                assert_eq!(system.data["claudecode_seen"], "");
            }
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::Text(t) = block {
                        echoed = Some(t.text.clone());
                    }
                }
            }
            Message::Result(result) => {
                cost = result.total_cost_usd;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_init, "init frame seen");
    assert_eq!(echoed.as_deref(), Some("echo: hello there"));
    assert_eq!(cost, Some(0.0));
}

#[tokio::test]
async fn nonzero_exit_becomes_result_error() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let options = ClaudeAgentOptions::builder()
        .cli_path(fake_cli())
        .env("CLAUDE_FAKE_EXIT_NONZERO", "1")
        .build();

    let mut stream = query("boom", options).await.expect("spawns");
    let mut err = None;
    while let Some(message) = stream.next().await {
        match message {
            Ok(Message::Result(result)) => {
                // The error result is yielded first...
                assert!(result.is_error);
                assert_eq!(result.subtype, "error_during_execution");
            }
            Ok(_) => {}
            Err(e) => {
                // ...then the non-zero exit surfaces as a typed ResultError.
                err = Some(e);
                break;
            }
        }
    }
    let err = err.expect("a terminal error");
    match &err {
        Error::Result(result_error) => {
            assert_eq!(result_error.exit_code, Some(3));
            assert!(result_error
                .errors
                .iter()
                .any(|e| e.contains("simulated failure")));
        }
        other => panic!("expected ResultError, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_cli_path_errors_cleanly() {
    let options = ClaudeAgentOptions::builder()
        .cli_path("/nonexistent/path/to/claude-xyz")
        .build();
    match query("hi", options).await {
        Err(Error::CliNotFound { .. }) | Err(Error::CliConnection(_)) => {}
        Err(other) => panic!("expected a connection error, got {other:?}"),
        Ok(_) => panic!("expected an error spawning a nonexistent CLI"),
    }
}

/// A crash without an error result surfaces as a `ProcessError` carrying the
/// captured stderr tail, even though no stderr callback was configured.
#[tokio::test]
async fn process_error_carries_stderr_tail() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let options = ClaudeAgentOptions::builder()
        .cli_path(fake_cli())
        .env("CLAUDE_FAKE_STDERR_CRASH", "1")
        .build();
    // The fake dies before answering `initialize`, so the failure surfaces
    // from `query()` itself (pending control requests fail fast rather than
    // timing out). Accept it from the stream too, for robustness.
    let err = match query("hi", options).await {
        Err(e) => e,
        Ok(mut stream) => {
            let mut found = None;
            while let Some(message) = stream.next().await {
                if let Err(e) = message {
                    found = Some(e);
                    break;
                }
            }
            found.expect("a process error")
        }
    };
    match err {
        Error::Process {
            exit_code, stderr, ..
        } => {
            assert_eq!(exit_code, Some(2));
            let stderr = stderr.expect("stderr tail attached");
            assert!(stderr.contains("simulated crash"), "{stderr}");
            assert!(stderr.contains("something minor"), "{stderr}");
        }
        other => panic!("expected Error::Process, got {other:?}"),
    }
}

/// A multi-turn `ClaudeSDKClient` session over the real subprocess transport.
#[tokio::test]
async fn client_multi_turn_session() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let options = ClaudeAgentOptions::builder().cli_path(fake_cli()).build();
    let mut client = ClaudeSDKClient::new(options);
    client.connect().await.expect("connects");
    assert!(client.server_info().is_some(), "initialize response kept");

    for turn in ["one", "two", "three"] {
        client.query(turn, "default").await.expect("sends");
        let mut echoed = None;
        let mut responses = client.receive_response();
        while let Some(message) = responses.next().await {
            if let Message::Assistant(assistant) = message.expect("ok") {
                for block in &assistant.content {
                    if let ContentBlock::Text(t) = block {
                        echoed = Some(t.text.clone());
                    }
                }
            }
        }
        assert_eq!(echoed.as_deref(), Some(format!("echo: {turn}").as_str()));
    }
    client.disconnect().await;
}
