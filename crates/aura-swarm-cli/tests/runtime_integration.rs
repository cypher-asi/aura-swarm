//! Integration tests against an aura-harness server.
//!
//! These tests require aura-harness to be running on localhost:8080.
//!
//! Run with:
//!   cargo test -p aura-swarm-cli --test runtime_integration -- --ignored
//!
//! Or run all integration tests:
//!   cargo test -p aura-swarm-cli --test runtime_integration -- --ignored --nocapture

use std::time::Duration;

use aura_swarm_protocol::{InboundMessage, OutboundMessage, SessionInit};
use futures::{SinkExt, StreamExt};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

/// Default aura-harness endpoint.
const RUNTIME_URL: &str = "ws://localhost:8080/stream";

/// Timeout for receiving messages.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for initial connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

// =============================================================================
// Test Helpers
// =============================================================================

/// Connect to the aura-harness WebSocket endpoint.
async fn connect_to_runtime() -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    let url = std::env::var("AURA_RUNTIME_URL").unwrap_or_else(|_| RUNTIME_URL.to_string());

    match timeout(CONNECT_TIMEOUT, connect_async(&url)).await {
        Ok(Ok((ws_stream, _))) => Ok(ws_stream),
        Ok(Err(e)) => Err(format!("Failed to connect to {url}: {e}")),
        Err(_) => Err(format!("Connection timeout to {url}")),
    }
}

/// Send a session_init, wait for session_ready, then send a user_message
/// and collect all OutboundMessage until assistant_message_end or error.
async fn send_prompt_and_collect(
    prompt: &str,
) -> Result<(Vec<OutboundMessage>, String), String> {
    let ws_stream = connect_to_runtime().await?;
    let (mut write, mut read) = ws_stream.split();

    // 1. Send session_init
    let init = InboundMessage::SessionInit(SessionInit::default());
    let json = serde_json::to_string(&init).map_err(|e| e.to_string())?;
    write
        .send(Message::Text(json))
        .await
        .map_err(|e| e.to_string())?;

    // 2. Wait for session_ready
    match timeout(Duration::from_secs(30), read.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            let msg: OutboundMessage = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            if !matches!(msg, OutboundMessage::SessionReady(_)) {
                return Err(format!("Expected session_ready, got: {text}"));
            }
        }
        other => return Err(format!("Unexpected response waiting for session_ready: {other:?}")),
    }

    // 3. Send user_message
    let user_msg = InboundMessage::UserMessage {
        content: prompt.to_string(),
    };
    let json = serde_json::to_string(&user_msg).map_err(|e| e.to_string())?;
    write
        .send(Message::Text(json))
        .await
        .map_err(|e| e.to_string())?;

    // 4. Collect messages
    let mut messages = Vec::new();
    let mut text_buffer = String::new();

    loop {
        match timeout(MESSAGE_TIMEOUT, read.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                match serde_json::from_str::<OutboundMessage>(&text) {
                    Ok(msg) => {
                        if let OutboundMessage::TextDelta { text: ref delta } = msg {
                            text_buffer.push_str(delta);
                        }

                        let is_terminal = matches!(
                            &msg,
                            OutboundMessage::AssistantMessageEnd(_)
                                | OutboundMessage::Error(_)
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

/// Test basic connectivity: session_init -> session_ready -> user_message -> streaming response.
#[tokio::test]
#[ignore = "Requires aura-harness running on localhost:8080"]
async fn test_connection() {
    let ws_stream = connect_to_runtime()
        .await
        .expect("Failed to connect to aura-harness");
    println!("OK WebSocket handshake successful");

    let (mut write, mut read) = ws_stream.split();

    // Send session_init
    let init = InboundMessage::SessionInit(SessionInit::default());
    let json = serde_json::to_string(&init).expect("Failed to serialize");
    write
        .send(Message::Text(json))
        .await
        .expect("Failed to send session_init");
    println!("OK Sent session_init");

    // Wait for session_ready
    match timeout(Duration::from_secs(30), read.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            let msg: OutboundMessage = serde_json::from_str(&text).expect("Failed to parse");
            assert!(
                matches!(msg, OutboundMessage::SessionReady(_)),
                "Expected session_ready, got: {text}"
            );
            println!("OK Received session_ready");
        }
        other => panic!("Unexpected response: {other:?}"),
    }

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
                                println!("  [{message_count:>3}] TextDelta (+{} chars)", delta.len());
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
#[ignore = "Requires aura-harness running on localhost:8080"]
async fn test_simple_prompt() {
    let prompt = "Say hello in exactly 3 words.";
    let (messages, text) = send_prompt_and_collect(prompt)
        .await
        .expect("Failed to get response");

    println!("\nMessages received: {}", messages.len());
    println!("Response text: {text}");

    let has_text_delta = messages
        .iter()
        .any(|m| matches!(m, OutboundMessage::TextDelta { .. }));
    let has_end = messages
        .iter()
        .any(|m| matches!(m, OutboundMessage::AssistantMessageEnd(_)));

    assert!(has_text_delta, "Missing TextDelta message");
    assert!(has_end, "Missing AssistantMessageEnd message");
    assert!(!text.is_empty(), "Response text should not be empty");

    println!("OK Simple prompt test passed");
}

/// Test cancellation mid-stream.
#[tokio::test]
#[ignore = "Requires aura-harness running on localhost:8080"]
async fn test_cancellation() {
    let ws_stream = connect_to_runtime().await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();

    // session_init
    let init = InboundMessage::SessionInit(SessionInit::default());
    write
        .send(Message::Text(serde_json::to_string(&init).unwrap()))
        .await
        .expect("Failed to send session_init");

    // Wait for session_ready
    match timeout(Duration::from_secs(30), read.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            let msg: OutboundMessage = serde_json::from_str(&text).unwrap();
            assert!(matches!(msg, OutboundMessage::SessionReady(_)));
        }
        other => panic!("Expected session_ready, got: {other:?}"),
    }

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
                    if matches!(msg, OutboundMessage::AssistantMessageEnd(_) | OutboundMessage::Error(_)) {
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
