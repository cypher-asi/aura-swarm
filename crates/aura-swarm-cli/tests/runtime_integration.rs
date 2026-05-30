//! Integration tests against an aura-harness server.
//!
//! These tests require aura-harness to be running on localhost:8080.
//!
//! They exercise the migrated runtime contract: a run is created with
//! `POST /v1/run` (returning a `run_id`), then the client attaches to
//! `WS /stream/:run_id` and exchanges `user_message` / streaming frames.
//!
//! Token resolution (first match wins):
//!   1. `AURA_RUNTIME_TOKEN` env var
//!   2. `AURA_SWARM_TOKEN` env var
//!   3. Stored credentials from `aswarm login`
//!   4. zOS login via `AURA_ZOS_EMAIL` + `AURA_ZOS_PASSWORD` env vars
//!
//! Run with:
//!   cargo test -p aura-swarm-cli --test runtime_integration
//!
//! Or with output:
//!   cargo test -p aura-swarm-cli --test runtime_integration -- --nocapture

use std::path::PathBuf;
use std::time::Duration;

use aura_swarm_auth::{AuthConfig, ZosClient};
use aura_swarm_protocol::{InboundMessage, OutboundMessage, RuntimeRequest, RuntimeRunResponse};
use futures::{SinkExt, StreamExt};
use tokio::sync::OnceCell;
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

/// Default aura-harness HTTP base (override with `AURA_RUNTIME_URL`).
const RUNTIME_BASE_URL: &str = "http://localhost:8080";

/// User id stamped onto the `RuntimeRequest` (must be non-empty or the harness
/// rejects the run with code `invalid_workspace`).
const TEST_USER_ID: &str = "aura-swarm-cli-integration-test";

/// Timeout for receiving messages.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for initial connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

// =============================================================================
// Token Resolution
// =============================================================================

/// Cached token shared across all tests in this binary.
static AUTH_TOKEN: OnceCell<Option<String>> = OnceCell::const_new();

/// Resolve the auth token once, trying multiple sources.
async fn get_token() -> Option<String> {
    AUTH_TOKEN
        .get_or_init(|| async {
            // 1. Explicit env vars
            if let Ok(t) = std::env::var("AURA_RUNTIME_TOKEN") {
                if !t.is_empty() {
                    return Some(t);
                }
            }
            if let Ok(t) = std::env::var("AURA_SWARM_TOKEN") {
                if !t.is_empty() {
                    return Some(t);
                }
            }

            // 2. Stored credentials from `aswarm login`
            if let Some(t) = load_stored_token().await {
                return Some(t);
            }

            // 3. Login with email/password from env
            if let Some(t) = login_from_env().await {
                return Some(t);
            }

            None
        })
        .await
        .clone()
}

/// Load token from the CLI credential store.
async fn load_stored_token() -> Option<String> {
    let path = credentials_path()?;
    let data = tokio::fs::read_to_string(path).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    v.get("access_token")?.as_str().map(String::from)
}

/// Credentials file path (mirrors `aswarm` CLI).
fn credentials_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("aura-swarm").join("credentials.json"))
}

/// Login via zOS using `AURA_ZOS_EMAIL` + `AURA_ZOS_PASSWORD`.
async fn login_from_env() -> Option<String> {
    let email = std::env::var("AURA_ZOS_EMAIL").ok()?;
    let password = std::env::var("AURA_ZOS_PASSWORD").ok()?;

    if email.is_empty() || password.is_empty() {
        return None;
    }

    let client = ZosClient::new(AuthConfig::default()).ok()?;
    let resp = client.login(&email, &password).await.ok()?;
    Some(resp.access_token)
}

// =============================================================================
// Test Helpers
// =============================================================================

/// HTTP base URL for the harness (`AURA_RUNTIME_URL`, default localhost:8080).
fn runtime_base_url() -> String {
    std::env::var("AURA_RUNTIME_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| RUNTIME_BASE_URL.to_string())
}

/// Derive the `ws://` base from the HTTP base.
fn runtime_ws_base() -> String {
    let base = runtime_base_url();
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base
    }
}

/// Create a chat run via `POST /v1/run` and return its `run_id`.
async fn start_chat_run() -> Result<String, String> {
    let token = get_token().await;

    let mut request = RuntimeRequest::chat(TEST_USER_ID);
    request.auth_jwt = token.clone();

    let url = format!("{}/v1/run", runtime_base_url());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let mut builder = client.post(&url).json(&request);
    if let Some(ref t) = token {
        builder = builder.bearer_auth(t);
    }

    let resp = builder.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("POST /v1/run failed ({status}): {body}"));
    }

    let run: RuntimeRunResponse = resp.json().await.map_err(|e| e.to_string())?;
    Ok(run.run_id)
}

/// Attach to a run's event stream WebSocket, forwarding the bearer token.
async fn attach_to_run(
    run_id: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    let url = format!("{}/stream/{run_id}", runtime_ws_base());
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("invalid ws url {url}: {e}"))?;
    if let Some(token) = get_token().await {
        if let Ok(value) = format!("Bearer {token}").parse() {
            request.headers_mut().insert("Authorization", value);
        }
    }

    match timeout(CONNECT_TIMEOUT, connect_async(request)).await {
        Ok(Ok((ws_stream, _))) => Ok(ws_stream),
        Ok(Err(e)) => Err(format!("Failed to attach to {url}: {e}")),
        Err(_) => Err(format!("Connection timeout to {url}")),
    }
}

/// Start a chat run, attach to it, send a user_message, and collect all
/// `OutboundMessage`s until `assistant_message_end` or `error`.
async fn send_prompt_and_collect(prompt: &str) -> Result<(Vec<OutboundMessage>, String), String> {
    let run_id = start_chat_run().await?;
    let ws_stream = attach_to_run(&run_id).await?;
    let (mut write, mut read) = ws_stream.split();

    // Send user_message directly — the run is already configured server-side.
    let user_msg = InboundMessage::UserMessage {
        content: prompt.to_string(),
    };
    let json = serde_json::to_string(&user_msg).map_err(|e| e.to_string())?;
    write
        .send(Message::Text(json))
        .await
        .map_err(|e| e.to_string())?;

    // Collect messages
    let mut messages = Vec::new();
    let mut text_buffer = String::new();

    loop {
        match timeout(MESSAGE_TIMEOUT, read.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                match serde_json::from_str::<OutboundMessage>(&text) {
                    Ok(msg) => {
                        match &msg {
                            OutboundMessage::TextDelta { text: ref delta } => {
                                text_buffer.push_str(delta);
                            }
                            OutboundMessage::ThinkingDelta {
                                thinking: ref delta,
                            } => {
                                text_buffer.push_str(delta);
                            }
                            _ => {}
                        }

                        let is_terminal = matches!(
                            &msg,
                            OutboundMessage::AssistantMessageEnd(_) | OutboundMessage::Error(_)
                        );

                        messages.push(msg);

                        if is_terminal {
                            break;
                        }
                    }
                    Err(e) => {
                        return Err(format!("Failed to parse message: {e} - raw: {text}"));
                    }
                }
            }
            Ok(Some(Ok(Message::Close(_)))) => {
                return Err("Connection closed unexpectedly".to_string());
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                return Err(format!("WebSocket error: {e}"));
            }
            Ok(None) => {
                return Err("Connection closed".to_string());
            }
            Err(_) => {
                return Err(format!(
                    "Timeout waiting for response ({}s)",
                    MESSAGE_TIMEOUT.as_secs()
                ));
            }
        }
    }

    let close_frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "Request complete".into(),
    };
    let _ = write.send(Message::Close(Some(close_frame))).await;

    Ok((messages, text_buffer))
}

// =============================================================================
// Integration Tests
// =============================================================================

/// Test basic connectivity: create run -> attach -> user_message -> streaming.
#[tokio::test]
async fn test_connection() {
    let run_id = start_chat_run().await.expect("Failed to create run");
    println!("OK Created run {run_id}");

    let ws_stream = attach_to_run(&run_id)
        .await
        .expect("Failed to attach to run stream");
    println!("OK WebSocket attach successful");

    let (mut write, mut read) = ws_stream.split();

    // Send user_message
    let user_msg = InboundMessage::UserMessage {
        content: "ping".to_string(),
    };
    let json = serde_json::to_string(&user_msg).expect("Failed to serialize");
    write
        .send(Message::Text(json))
        .await
        .expect("Failed to send user_message");
    println!("OK Sent user_message");

    let mut received_any = false;
    let mut message_count = 0;

    for _ in 0..50 {
        match timeout(Duration::from_secs(10), read.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                received_any = true;
                message_count += 1;

                if let Ok(msg) = serde_json::from_str::<OutboundMessage>(&text) {
                    match &msg {
                        OutboundMessage::AssistantMessageEnd(_) | OutboundMessage::Error(_) => {
                            println!("  [{message_count:>3}] Terminal message");
                            break;
                        }
                        OutboundMessage::TextDelta { text: delta } => {
                            if message_count <= 3 {
                                println!(
                                    "  [{message_count:>3}] TextDelta (+{} chars)",
                                    delta.len()
                                );
                            }
                        }
                        _ => {
                            println!("  [{message_count:>3}] {:?}", std::mem::discriminant(&msg));
                        }
                    }
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }

    assert!(received_any, "Should receive at least one message");
    println!("OK Round-trip communication verified ({message_count} messages)");

    let close_frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "Test complete".into(),
    };
    let _ = write.send(Message::Close(Some(close_frame))).await;
}

/// Test a simple prompt that should get a text response.
#[tokio::test]
async fn test_simple_prompt() {
    let prompt = "Say hello in exactly 3 words.";
    let (messages, text) = send_prompt_and_collect(prompt)
        .await
        .expect("Failed to get response");

    println!("\nMessages received: {}", messages.len());
    for (i, msg) in messages.iter().enumerate() {
        let label = match msg {
            OutboundMessage::AssistantMessageStart { .. } => "AssistantMessageStart",
            OutboundMessage::TextDelta { text: t } => {
                println!("  [{i}] TextDelta (+{} chars)", t.len());
                continue;
            }
            OutboundMessage::ThinkingDelta { thinking: t } => {
                println!("  [{i}] ThinkingDelta (+{} chars)", t.len());
                continue;
            }
            OutboundMessage::ToolUseStart { name, .. } => {
                println!("  [{i}] ToolUseStart({name})");
                continue;
            }
            OutboundMessage::ToolResult { name, .. } => {
                println!("  [{i}] ToolResult({name})");
                continue;
            }
            OutboundMessage::AssistantMessageEnd(_) => "AssistantMessageEnd",
            OutboundMessage::Error(e) => {
                println!("  [{i}] Error({})", e.message);
                continue;
            }
            _ => "Other",
        };
        println!("  [{i}] {label}");
    }
    println!("Response text: {text}");

    let has_text_delta = messages
        .iter()
        .any(|m| matches!(m, OutboundMessage::TextDelta { .. }));
    let has_thinking_delta = messages
        .iter()
        .any(|m| matches!(m, OutboundMessage::ThinkingDelta { .. }));
    let has_end = messages
        .iter()
        .any(|m| matches!(m, OutboundMessage::AssistantMessageEnd(_)));

    assert!(
        has_text_delta || has_thinking_delta,
        "Expected at least one TextDelta or ThinkingDelta, got {} messages: {:?}",
        messages.len(),
        messages.iter().map(variant_name).collect::<Vec<_>>(),
    );
    assert!(has_end, "Missing AssistantMessageEnd message");

    if has_text_delta {
        assert!(!text.is_empty(), "Response text should not be empty");
    }

    println!("OK Simple prompt test passed");
}

fn variant_name(msg: &OutboundMessage) -> &'static str {
    match msg {
        OutboundMessage::SessionReady(_) => "SessionReady",
        OutboundMessage::AssistantMessageStart { .. } => "AssistantMessageStart",
        OutboundMessage::TextDelta { .. } => "TextDelta",
        OutboundMessage::ThinkingDelta { .. } => "ThinkingDelta",
        OutboundMessage::ToolUseStart { .. } => "ToolUseStart",
        OutboundMessage::ToolResult { .. } => "ToolResult",
        OutboundMessage::AssistantMessageEnd(_) => "AssistantMessageEnd",
        OutboundMessage::Error(_) => "Error",
        OutboundMessage::ToolCallbackRequest(_) => "ToolCallbackRequest",
    }
}

/// Test cancellation mid-stream.
#[tokio::test]
async fn test_cancellation() {
    let run_id = start_chat_run().await.expect("Failed to create run");
    let ws_stream = attach_to_run(&run_id).await.expect("Failed to attach");
    let (mut write, mut read) = ws_stream.split();

    // Send a long prompt
    let user_msg = InboundMessage::UserMessage {
        content: "Write a very long essay about the history of computing, at least 1000 words."
            .to_string(),
    };
    write
        .send(Message::Text(serde_json::to_string(&user_msg).unwrap()))
        .await
        .expect("Failed to send");

    // Wait for some streaming content
    let mut got_content = false;
    for _ in 0..10 {
        if let Ok(Some(Ok(Message::Text(text)))) =
            timeout(Duration::from_secs(5), read.next()).await
        {
            if let Ok(OutboundMessage::TextDelta { .. }) = serde_json::from_str(&text) {
                got_content = true;
                break;
            }
        }
    }

    if !got_content {
        println!("Warning: never got content before cancel");
        return;
    }

    // Send cancel
    let cancel = InboundMessage::Cancel;
    write
        .send(Message::Text(serde_json::to_string(&cancel).unwrap()))
        .await
        .expect("Failed to send cancel");
    println!("Sent cancel");

    // Drain remaining messages
    for _ in 0..20 {
        match timeout(Duration::from_secs(2), read.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(msg) = serde_json::from_str::<OutboundMessage>(&text) {
                    if matches!(
                        msg,
                        OutboundMessage::AssistantMessageEnd(_) | OutboundMessage::Error(_)
                    ) {
                        println!("OK Stream ended after cancel");
                        break;
                    }
                }
            }
            _ => break,
        }
    }

    let close_frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "Test complete".into(),
    };
    let _ = write.send(Message::Close(Some(close_frame))).await;
    println!("OK Cancellation test completed");
}
