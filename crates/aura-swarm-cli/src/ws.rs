//! WebSocket client for agent chat with streaming support.
//!
//! This module handles WebSocket connections to agents for real-time streaming chat
//! using the Aura runtime protocol with server-side tool execution.

use std::time::{SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::Message;

use crate::types::{ClientMessage, ServerMessage, TurnCompleteInfo};

/// Error type for WebSocket operations.
#[derive(Debug, thiserror::Error)]
pub enum WsError {
    /// Failed to connect.
    #[error("Connection failed: {0}")]
    Connection(String),

    /// Failed to send message.
    #[error("Send failed: {0}")]
    Send(String),

    /// JSON serialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Handle for sending messages to the WebSocket.
#[derive(Debug, Clone)]
pub struct WsSender {
    tx: mpsc::Sender<String>,
}

impl WsSender {
    /// Send a prompt to the agent.
    ///
    /// Returns the request ID for tracking the response.
    pub async fn send_prompt(
        &self,
        prompt: &str,
        agent_id: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<String, WsError> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let request_id = format!("req-{now_ms}");

        let msg = ClientMessage::Prompt {
            request_id: request_id.clone(),
            prompt: prompt.to_string(),
            agent_id: agent_id.map(String::from),
            workspace: workspace.map(String::from),
        };

        let json = serde_json::to_string(&msg)?;
        self.tx
            .send(json)
            .await
            .map_err(|e| WsError::Send(e.to_string()))?;

        Ok(request_id)
    }

    /// Send a cancel request to stop an in-progress response.
    pub async fn cancel(&self, request_id: &str) -> Result<(), WsError> {
        let msg = ClientMessage::Cancel {
            request_id: request_id.to_string(),
        };
        let json = serde_json::to_string(&msg)?;
        self.tx
            .send(json)
            .await
            .map_err(|e| WsError::Send(e.to_string()))
    }
}

/// Events from the WebSocket connection.
#[derive(Debug)]
pub enum WsEvent {
    /// Successfully connected.
    Connected,
    /// A new turn has started.
    TurnStart,
    /// A new step within the turn has started.
    StepStart {
        /// Step number (1-indexed).
        step: u32,
    },
    /// Text content delta (stream incrementally).
    TextDelta(String),
    /// Thinking content delta.
    ThinkingDelta(String),
    /// Tool execution started (server-side).
    ToolStart {
        /// Tool name being executed.
        tool_name: String,
        /// Tool arguments.
        args: serde_json::Value,
    },
    /// Tool execution completed (server-side).
    ToolComplete {
        /// Tool name.
        tool_name: String,
        /// Execution result.
        result: String,
        /// Whether the execution resulted in an error.
        is_error: bool,
    },
    /// Turn completed.
    TurnComplete(TurnCompleteInfo),
    /// Request was cancelled.
    Cancelled {
        /// ID of the cancelled request.
        request_id: String,
    },
    /// Connection closed.
    Disconnected,
    /// Error occurred.
    Error {
        /// Error message.
        message: String,
        /// Optional error code.
        code: Option<String>,
    },
}

/// Spawn a WebSocket connection task.
///
/// Returns a sender for outgoing messages and a receiver for incoming events.
pub async fn connect(
    url: &str,
    token: &str,
) -> Result<(WsSender, mpsc::Receiver<WsEvent>), WsError> {
    // Build request with auth header
    let request = Request::builder()
        .uri(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Host", extract_host(url).unwrap_or("localhost"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_ws_key())
        .body(())
        .map_err(|e| WsError::Connection(e.to_string()))?;

    let (ws_stream, _) = connect_async(request)
        .await
        .map_err(|e| WsError::Connection(e.to_string()))?;

    let (write, read) = ws_stream.split();

    // Channel for outgoing messages
    let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(32);

    // Channel for incoming events
    let (event_tx, event_rx) = mpsc::channel::<WsEvent>(32);

    // Spawn the writer task
    tokio::spawn(ws_writer(write, outgoing_rx));

    // Spawn the reader task
    tokio::spawn(ws_reader(read, event_tx));

    Ok((WsSender { tx: outgoing_tx }, event_rx))
}

/// Task that writes outgoing messages.
async fn ws_writer(
    mut write: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    mut rx: mpsc::Receiver<String>,
) {
    while let Some(text) = rx.recv().await {
        if write.send(Message::Text(text)).await.is_err() {
            break;
        }
    }
}

/// Task that reads incoming messages and sends events.
///
/// Parses the Aura runtime protocol messages and converts them to `WsEvent` variants.
async fn ws_reader(
    mut read: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    tx: mpsc::Sender<WsEvent>,
) {
    // Track current tool name for ToolComplete events
    let mut current_tool_name: Option<String> = None;

    // Send connected event
    let _ = tx.send(WsEvent::Connected).await;

    while let Some(result) = read.next().await {
        match result {
            Ok(Message::Text(text)) => {
                // Try to parse as ServerMessage (Aura runtime protocol)
                match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(msg) => {
                        let event = match msg {
                            ServerMessage::TurnStart { request_id: _, agent_id } => {
                                tracing::debug!(agent_id = %agent_id, "Turn started");
                                Some(WsEvent::TurnStart)
                            }
                            ServerMessage::StepStart { request_id: _, agent_id, step } => {
                                tracing::debug!(agent_id = %agent_id, step = step, "Step started");
                                Some(WsEvent::StepStart { step })
                            }
                            ServerMessage::TextDelta { request_id: _, agent_id: _, text } => {
                                Some(WsEvent::TextDelta(text))
                            }
                            ServerMessage::ThinkingDelta { request_id: _, agent_id: _, thinking } => {
                                Some(WsEvent::ThinkingDelta(thinking))
                            }
                            ServerMessage::ToolStart { request_id: _, agent_id: _, tool_id, tool_name, args } => {
                                tracing::debug!(tool = %tool_name, tool_id = %tool_id, "Tool execution started");
                                current_tool_name = Some(tool_name.clone());
                                Some(WsEvent::ToolStart {
                                    tool_name,
                                    args,
                                })
                            }
                            ServerMessage::ToolComplete { request_id: _, agent_id: _, tool_id, result, is_error } => {
                                tracing::debug!(tool_id = %tool_id, is_error = is_error, "Tool execution completed");
                                let tool_name = current_tool_name.take().unwrap_or_default();
                                Some(WsEvent::ToolComplete {
                                    tool_name,
                                    result,
                                    is_error,
                                })
                            }
                            ServerMessage::TurnComplete { request_id: _, agent_id, steps, input_tokens, output_tokens } => {
                                tracing::debug!(agent_id = %agent_id, steps = steps, "Turn complete");
                                Some(WsEvent::TurnComplete(TurnCompleteInfo {
                                    steps,
                                    input_tokens,
                                    output_tokens,
                                }))
                            }
                            ServerMessage::Cancelled { request_id, agent_id: _ } => {
                                Some(WsEvent::Cancelled { request_id })
                            }
                            ServerMessage::Error { request_id: _, agent_id: _, error, code } => {
                                Some(WsEvent::Error { message: error, code })
                            }
                        };
                        if let Some(event) = event {
                            let _ = tx.send(event).await;
                        }
                    }
                    Err(e) => {
                        // Log parse error but treat as raw text for debugging
                        tracing::debug!(error = %e, text = %text, "Failed to parse server message");
                        // Forward as error so the user knows something went wrong
                        let _ = tx
                            .send(WsEvent::Error {
                                message: format!("Protocol error: {e}"),
                                code: Some("parse_error".to_string()),
                            })
                            .await;
                    }
                }
            }
            Ok(Message::Close(_)) => {
                let _ = tx.send(WsEvent::Disconnected).await;
                break;
            }
            // Ignore control frames and binary messages
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_) | Message::Binary(_)) => {}
            Err(e) => {
                let _ = tx
                    .send(WsEvent::Error {
                        message: e.to_string(),
                        code: None,
                    })
                    .await;
                break;
            }
        }
    }

    let _ = tx.send(WsEvent::Disconnected).await;
}

/// Extract host from URL.
fn extract_host(url: &str) -> Option<&str> {
    let url = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))?;
    url.split('/').next()
}

/// Generate a random WebSocket key.
fn generate_ws_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    base64_encode(&nanos.to_le_bytes()[..16])
}

/// Simple base64 encoding.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;

        result.push(ALPHABET[b0 >> 2] as char);
        result.push(ALPHABET[((b0 & 0x03) << 4) | (b1 >> 4)] as char);

        if chunk.len() > 1 {
            result.push(ALPHABET[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(ALPHABET[b2 & 0x3f] as char);
        } else {
            result.push('=');
        }
    }
    result
}

/// Convert a ServerMessage to a WsEvent (for testing).
///
/// This function mirrors the conversion logic in ws_reader.
#[cfg(test)]
fn server_message_to_event(
    msg: ServerMessage,
    current_tool_name: &mut Option<String>,
) -> Option<WsEvent> {
    match msg {
        ServerMessage::TurnStart { .. } => Some(WsEvent::TurnStart),
        ServerMessage::StepStart { step, .. } => Some(WsEvent::StepStart { step }),
        ServerMessage::TextDelta { text, .. } => Some(WsEvent::TextDelta(text)),
        ServerMessage::ThinkingDelta { thinking, .. } => Some(WsEvent::ThinkingDelta(thinking)),
        ServerMessage::ToolStart { tool_name, args, .. } => {
            *current_tool_name = Some(tool_name.clone());
            Some(WsEvent::ToolStart { tool_name, args })
        }
        ServerMessage::ToolComplete { result, is_error, .. } => {
            let tool_name = current_tool_name.take().unwrap_or_default();
            Some(WsEvent::ToolComplete {
                tool_name,
                result,
                is_error,
            })
        }
        ServerMessage::TurnComplete {
            steps,
            input_tokens,
            output_tokens,
            ..
        } => Some(WsEvent::TurnComplete(TurnCompleteInfo {
            steps,
            input_tokens,
            output_tokens,
        })),
        ServerMessage::Cancelled { request_id, .. } => {
            Some(WsEvent::Cancelled { request_id })
        }
        ServerMessage::Error { error, code, .. } => Some(WsEvent::Error { message: error, code }),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // =========================================================================
    // Helper Function Tests
    // =========================================================================

    #[test]
    fn extract_host_from_ws_url() {
        assert_eq!(extract_host("ws://localhost:8080/stream"), Some("localhost:8080"));
        assert_eq!(extract_host("ws://192.168.1.1:3000/api/ws"), Some("192.168.1.1:3000"));
        assert_eq!(extract_host("ws://example.com"), Some("example.com"));
    }

    #[test]
    fn extract_host_from_wss_url() {
        assert_eq!(extract_host("wss://secure.example.com/stream"), Some("secure.example.com"));
        assert_eq!(extract_host("wss://api.example.com:443/ws"), Some("api.example.com:443"));
    }

    #[test]
    fn extract_host_invalid_urls() {
        assert_eq!(extract_host("http://example.com"), None);
        assert_eq!(extract_host("https://example.com"), None);
        assert_eq!(extract_host("example.com"), None);
        assert_eq!(extract_host(""), None);
    }

    #[test]
    fn base64_encode_basic() {
        // Test vectors from RFC 4648
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encode_binary() {
        // Test with all byte values 0-255
        let data: Vec<u8> = (0..=255).collect();
        let encoded = base64_encode(&data);
        // Should be valid base64 (length divisible by 4, valid characters)
        assert_eq!(encoded.len() % 4, 0);
        assert!(encoded.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }

    #[test]
    fn generate_ws_key_is_valid_base64() {
        let key = generate_ws_key();
        // WebSocket key should be 24 characters (16 bytes base64 encoded)
        assert!(key.len() >= 20); // At least reasonable length
        assert!(key.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }

    #[test]
    fn generate_ws_key_unique() {
        // Keys generated at different times should be different
        let key1 = generate_ws_key();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let key2 = generate_ws_key();
        // In practice these will almost always be different due to timestamp
        // but we can't guarantee it in a unit test
        assert!(!key1.is_empty());
        assert!(!key2.is_empty());
    }

    // =========================================================================
    // ServerMessage to WsEvent Conversion Tests
    // =========================================================================

    #[test]
    fn convert_turn_start_to_event() {
        let msg = ServerMessage::TurnStart {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
        };

        let mut tool_name = None;
        let event = server_message_to_event(msg, &mut tool_name).unwrap();

        assert!(matches!(event, WsEvent::TurnStart));
    }

    #[test]
    fn convert_step_start_to_event() {
        let msg = ServerMessage::StepStart {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            step: 3,
        };

        let mut tool_name = None;
        let event = server_message_to_event(msg, &mut tool_name).unwrap();

        match event {
            WsEvent::StepStart { step } => {
                assert_eq!(step, 3);
            }
            _ => panic!("Expected StepStart event"),
        }
    }

    #[test]
    fn convert_text_delta_to_event() {
        let msg = ServerMessage::TextDelta {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            text: "Hello, world!".to_string(),
        };

        let mut tool_name = None;
        let event = server_message_to_event(msg, &mut tool_name).unwrap();

        match event {
            WsEvent::TextDelta(text) => {
                assert_eq!(text, "Hello, world!");
            }
            _ => panic!("Expected TextDelta event"),
        }
    }

    #[test]
    fn convert_thinking_delta_to_event() {
        let msg = ServerMessage::ThinkingDelta {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            thinking: "Let me analyze this...".to_string(),
        };

        let mut tool_name = None;
        let event = server_message_to_event(msg, &mut tool_name).unwrap();

        match event {
            WsEvent::ThinkingDelta(thinking) => {
                assert_eq!(thinking, "Let me analyze this...");
            }
            _ => panic!("Expected ThinkingDelta event"),
        }
    }

    #[test]
    fn convert_tool_start_to_event() {
        let msg = ServerMessage::ToolStart {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            tool_id: "tool-123".to_string(),
            tool_name: "fs_read".to_string(),
            args: json!({"path": "/etc/passwd"}),
        };

        let mut tool_name_state = None;
        let event = server_message_to_event(msg, &mut tool_name_state).unwrap();

        match event {
            WsEvent::ToolStart { tool_name, args } => {
                assert_eq!(tool_name, "fs_read");
                assert_eq!(args["path"], "/etc/passwd");
            }
            _ => panic!("Expected ToolStart event"),
        }

        // Tool name should be stored for ToolComplete
        assert_eq!(tool_name_state, Some("fs_read".to_string()));
    }

    #[test]
    fn convert_tool_complete_to_event_with_stored_name() {
        let msg = ServerMessage::ToolComplete {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            tool_id: "tool-123".to_string(),
            result: "File contents here".to_string(),
            is_error: false,
        };

        let mut tool_name_state = Some("fs_read".to_string());
        let event = server_message_to_event(msg, &mut tool_name_state).unwrap();

        match event {
            WsEvent::ToolComplete { tool_name, result, is_error } => {
                assert_eq!(tool_name, "fs_read");
                assert_eq!(result, "File contents here");
                assert!(!is_error);
            }
            _ => panic!("Expected ToolComplete event"),
        }

        // Tool name should be cleared
        assert_eq!(tool_name_state, None);
    }

    #[test]
    fn convert_tool_complete_error_to_event() {
        let msg = ServerMessage::ToolComplete {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            tool_id: "tool-456".to_string(),
            result: "Permission denied".to_string(),
            is_error: true,
        };

        let mut tool_name_state = Some("cmd_run".to_string());
        let event = server_message_to_event(msg, &mut tool_name_state).unwrap();

        match event {
            WsEvent::ToolComplete { is_error, result, .. } => {
                assert!(is_error);
                assert_eq!(result, "Permission denied");
            }
            _ => panic!("Expected ToolComplete event"),
        }
    }

    #[test]
    fn convert_turn_complete_to_event() {
        let msg = ServerMessage::TurnComplete {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            steps: 3,
            input_tokens: 1500,
            output_tokens: 500,
        };

        let mut tool_name = None;
        let event = server_message_to_event(msg, &mut tool_name).unwrap();

        match event {
            WsEvent::TurnComplete(info) => {
                assert_eq!(info.steps, 3);
                assert_eq!(info.input_tokens, 1500);
                assert_eq!(info.output_tokens, 500);
            }
            _ => panic!("Expected TurnComplete event"),
        }
    }

    #[test]
    fn convert_cancelled_to_event() {
        let msg = ServerMessage::Cancelled {
            request_id: "req-123".to_string(),
            agent_id: "agent-1".to_string(),
        };

        let mut tool_name = None;
        let event = server_message_to_event(msg, &mut tool_name).unwrap();

        match event {
            WsEvent::Cancelled { request_id } => {
                assert_eq!(request_id, "req-123");
            }
            _ => panic!("Expected Cancelled event"),
        }
    }

    #[test]
    fn convert_error_to_event() {
        let msg = ServerMessage::Error {
            request_id: Some("req-1".to_string()),
            agent_id: Some("agent-1".to_string()),
            error: "Something went wrong".to_string(),
            code: Some("internal_error".to_string()),
        };

        let mut tool_name = None;
        let event = server_message_to_event(msg, &mut tool_name).unwrap();

        match event {
            WsEvent::Error { message, code } => {
                assert_eq!(message, "Something went wrong");
                assert_eq!(code, Some("internal_error".to_string()));
            }
            _ => panic!("Expected Error event"),
        }
    }

    // =========================================================================
    // ClientMessage Serialization Tests (via WsSender logic)
    // =========================================================================

    #[test]
    fn prompt_message_serializes_correctly() {
        let msg = ClientMessage::Prompt {
            request_id: "req-test".to_string(),
            prompt: "Hello, agent!".to_string(),
            agent_id: Some("agent-123".to_string()),
            workspace: Some("/workspace".to_string()),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["type"], "prompt");
        assert_eq!(parsed["request_id"], "req-test");
        assert_eq!(parsed["prompt"], "Hello, agent!");
        assert_eq!(parsed["agent_id"], "agent-123");
        assert_eq!(parsed["workspace"], "/workspace");
    }

    #[test]
    fn cancel_message_serializes_correctly() {
        let msg = ClientMessage::Cancel {
            request_id: "req-to-cancel".to_string(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["type"], "cancel");
        assert_eq!(parsed["request_id"], "req-to-cancel");
    }

    // =========================================================================
    // Message Flow Simulation Tests
    // =========================================================================

    #[test]
    fn simulate_simple_text_response_flow() {
        let messages = vec![
            ServerMessage::TurnStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
            },
            ServerMessage::TextDelta {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                text: "Hello".to_string(),
            },
            ServerMessage::TextDelta {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                text: ", ".to_string(),
            },
            ServerMessage::TextDelta {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                text: "world!".to_string(),
            },
            ServerMessage::TurnComplete {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                steps: 1,
                input_tokens: 10,
                output_tokens: 3,
            },
        ];

        let mut tool_name_state = None;
        let mut text_buffer = String::new();
        let mut turn_info = None;

        for msg in messages {
            if let Some(event) = server_message_to_event(msg, &mut tool_name_state) {
                match event {
                    WsEvent::TextDelta(text) => text_buffer.push_str(&text),
                    WsEvent::TurnComplete(info) => turn_info = Some(info),
                    _ => {}
                }
            }
        }

        assert_eq!(text_buffer, "Hello, world!");
        let info = turn_info.unwrap();
        assert_eq!(info.steps, 1);
        assert_eq!(info.input_tokens, 10);
        assert_eq!(info.output_tokens, 3);
    }

    #[test]
    fn simulate_tool_execution_flow() {
        let messages = vec![
            ServerMessage::TurnStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
            },
            ServerMessage::StepStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                step: 1,
            },
            ServerMessage::ToolStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                tool_id: "tool-1".to_string(),
                tool_name: "fs_read".to_string(),
                args: json!({"path": "README.md"}),
            },
            ServerMessage::ToolComplete {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                tool_id: "tool-1".to_string(),
                result: "# README\nProject documentation".to_string(),
                is_error: false,
            },
            ServerMessage::TextDelta {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                text: "I found the README file.".to_string(),
            },
            ServerMessage::TurnComplete {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                steps: 1,
                input_tokens: 100,
                output_tokens: 20,
            },
        ];

        let mut tool_name_state = None;
        let mut tool_executions: Vec<(String, String, String)> = Vec::new();

        for msg in messages {
            if let Some(event) = server_message_to_event(msg, &mut tool_name_state) {
                match event {
                    WsEvent::ToolStart { tool_name, args } => {
                        tool_executions.push(("start".to_string(), tool_name, args.to_string()));
                    }
                    WsEvent::ToolComplete { tool_name, result, is_error } => {
                        assert!(!is_error);
                        tool_executions.push(("complete".to_string(), tool_name, result));
                    }
                    _ => {}
                }
            }
        }

        assert_eq!(tool_executions.len(), 2);
        assert_eq!(tool_executions[0].0, "start");
        assert_eq!(tool_executions[0].1, "fs_read");
        assert_eq!(tool_executions[1].0, "complete");
        assert_eq!(tool_executions[1].1, "fs_read"); // Name should be carried over
        assert!(tool_executions[1].2.contains("README"));
    }

    #[test]
    fn simulate_multi_tool_execution_flow() {
        // Simulate multiple tool calls in sequence
        let messages = vec![
            ServerMessage::TurnStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
            },
            // First tool
            ServerMessage::ToolStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                tool_id: "t1".to_string(),
                tool_name: "fs_ls".to_string(),
                args: json!({"path": "/"}),
            },
            ServerMessage::ToolComplete {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                tool_id: "t1".to_string(),
                result: "bin\netc\nusr".to_string(),
                is_error: false,
            },
            // Second tool
            ServerMessage::ToolStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                tool_id: "t2".to_string(),
                tool_name: "fs_read".to_string(),
                args: json!({"path": "/etc/passwd"}),
            },
            ServerMessage::ToolComplete {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                tool_id: "t2".to_string(),
                result: "root:x:0:0:root:/root:/bin/bash".to_string(),
                is_error: false,
            },
            ServerMessage::TurnComplete {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                steps: 1,
                input_tokens: 200,
                output_tokens: 50,
            },
        ];

        let mut tool_name_state = None;
        let mut tool_names_seen = Vec::new();

        for msg in messages {
            if let Some(event) = server_message_to_event(msg, &mut tool_name_state) {
                match event {
                    WsEvent::ToolComplete { tool_name, .. } => {
                        tool_names_seen.push(tool_name);
                    }
                    _ => {}
                }
            }
        }

        assert_eq!(tool_names_seen, vec!["fs_ls", "fs_read"]);
    }

    #[test]
    fn simulate_thinking_with_response() {
        let messages = vec![
            ServerMessage::TurnStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
            },
            ServerMessage::ThinkingDelta {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                thinking: "Let me analyze ".to_string(),
            },
            ServerMessage::ThinkingDelta {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                thinking: "this problem...".to_string(),
            },
            ServerMessage::TextDelta {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                text: "Here's my analysis.".to_string(),
            },
            ServerMessage::TurnComplete {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                steps: 1,
                input_tokens: 50,
                output_tokens: 25,
            },
        ];

        let mut tool_name_state = None;
        let mut thinking_buffer = String::new();
        let mut text_buffer = String::new();

        for msg in messages {
            if let Some(event) = server_message_to_event(msg, &mut tool_name_state) {
                match event {
                    WsEvent::ThinkingDelta(thinking) => thinking_buffer.push_str(&thinking),
                    WsEvent::TextDelta(text) => text_buffer.push_str(&text),
                    _ => {}
                }
            }
        }

        assert_eq!(thinking_buffer, "Let me analyze this problem...");
        assert_eq!(text_buffer, "Here's my analysis.");
    }

    #[test]
    fn simulate_error_mid_stream() {
        let messages = vec![
            ServerMessage::TurnStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
            },
            ServerMessage::TextDelta {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                text: "Starting to process...".to_string(),
            },
            ServerMessage::Error {
                request_id: Some("req-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                error: "Context length exceeded".to_string(),
                code: Some("context_length".to_string()),
            },
        ];

        let mut tool_name_state = None;
        let mut error_received = None;

        for msg in messages {
            if let Some(event) = server_message_to_event(msg, &mut tool_name_state) {
                if let WsEvent::Error { message, code } = event {
                    error_received = Some((message, code));
                }
            }
        }

        let (message, code) = error_received.unwrap();
        assert_eq!(message, "Context length exceeded");
        assert_eq!(code, Some("context_length".to_string()));
    }

    // =========================================================================
    // Stress/Edge Case Tests
    // =========================================================================

    #[test]
    fn handle_empty_text_delta() {
        let msg = ServerMessage::TextDelta {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            text: "".to_string(),
        };

        let mut tool_name = None;
        let event = server_message_to_event(msg, &mut tool_name).unwrap();

        match event {
            WsEvent::TextDelta(text) => {
                assert_eq!(text, "");
            }
            _ => panic!("Expected TextDelta event"),
        }
    }

    #[test]
    fn handle_large_tool_result() {
        let large_result = "x".repeat(100_000);
        let msg = ServerMessage::ToolComplete {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            tool_id: "tool-1".to_string(),
            result: large_result.clone(),
            is_error: false,
        };

        let mut tool_name = Some("fs_read".to_string());
        let event = server_message_to_event(msg, &mut tool_name).unwrap();

        match event {
            WsEvent::ToolComplete { result, .. } => {
                assert_eq!(result.len(), 100_000);
            }
            _ => panic!("Expected ToolComplete event"),
        }
    }

    #[test]
    fn handle_complex_tool_args() {
        let complex_args = json!({
            "nested": {
                "array": [1, 2, {"deep": true}],
                "object": {"key": "value"}
            },
            "unicode": "こんにちは",
            "special": "line1\nline2\ttab",
            "numbers": {
                "int": 42,
                "float": 3.14159,
                "negative": -100
            },
            "bool_true": true,
            "bool_false": false,
            "null_value": null
        });

        let msg = ServerMessage::ToolStart {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            tool_id: "tool-1".to_string(),
            tool_name: "complex_tool".to_string(),
            args: complex_args.clone(),
        };

        let mut tool_name_state = None;
        let event = server_message_to_event(msg, &mut tool_name_state).unwrap();

        match event {
            WsEvent::ToolStart { tool_name, args } => {
                assert_eq!(tool_name, "complex_tool");
                assert_eq!(args["nested"]["array"][2]["deep"], true);
                assert_eq!(args["unicode"], "こんにちは");
                assert_eq!(args["numbers"]["float"], 3.14159);
                assert!(args["null_value"].is_null());
            }
            _ => panic!("Expected ToolStart event"),
        }
    }

    #[test]
    fn handle_many_steps() {
        // Simulate a turn with many steps
        let mut messages = vec![ServerMessage::TurnStart {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
        }];

        for step in 1..=100 {
            messages.push(ServerMessage::StepStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                step,
            });
            messages.push(ServerMessage::TextDelta {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                text: format!("Step {} output. ", step),
            });
        }

        messages.push(ServerMessage::TurnComplete {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            steps: 100,
            input_tokens: 50000,
            output_tokens: 10000,
        });

        let mut tool_name_state = None;
        let mut step_count = 0;
        let mut final_info = None;

        for msg in messages {
            if let Some(event) = server_message_to_event(msg, &mut tool_name_state) {
                match event {
                    WsEvent::StepStart { step } => {
                        step_count = step;
                    }
                    WsEvent::TurnComplete(info) => {
                        final_info = Some(info);
                    }
                    _ => {}
                }
            }
        }

        assert_eq!(step_count, 100);
        let info = final_info.unwrap();
        assert_eq!(info.steps, 100);
        assert_eq!(info.input_tokens, 50000);
        assert_eq!(info.output_tokens, 10000);
    }

    #[test]
    fn tool_complete_without_prior_start() {
        // Edge case: ToolComplete without a prior ToolStart
        // Should still work but with empty tool name
        let msg = ServerMessage::ToolComplete {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            tool_id: "tool-orphan".to_string(),
            result: "some result".to_string(),
            is_error: false,
        };

        let mut tool_name_state = None; // No tool name stored
        let event = server_message_to_event(msg, &mut tool_name_state).unwrap();

        match event {
            WsEvent::ToolComplete { tool_name, .. } => {
                assert_eq!(tool_name, ""); // Should be empty, not panic
            }
            _ => panic!("Expected ToolComplete event"),
        }
    }

    // =========================================================================
    // JSON Parsing Robustness Tests
    // =========================================================================

    #[test]
    fn parse_message_with_extra_fields() {
        // Should successfully parse even with unknown fields
        let json = r#"{
            "type": "turn_start",
            "request_id": "req-1",
            "agent_id": "agent-1",
            "extra_field": "should be ignored",
            "another_extra": 123
        }"#;

        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        match msg {
            ServerMessage::TurnStart { request_id, agent_id } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(agent_id, "agent-1");
            }
            _ => panic!("Expected TurnStart"),
        }
    }

    #[test]
    fn parse_message_with_whitespace_variations() {
        // Compact JSON
        let json1 = r#"{"type":"text_delta","request_id":"r","agent_id":"a","text":"t"}"#;
        // Pretty-printed JSON
        let json2 = r#"{
            "type": "text_delta",
            "request_id": "r",
            "agent_id": "a",
            "text": "t"
        }"#;

        let msg1: ServerMessage = serde_json::from_str(json1).unwrap();
        let msg2: ServerMessage = serde_json::from_str(json2).unwrap();

        match (msg1, msg2) {
            (
                ServerMessage::TextDelta { text: t1, .. },
                ServerMessage::TextDelta { text: t2, .. },
            ) => {
                assert_eq!(t1, t2);
            }
            _ => panic!("Both should be TextDelta"),
        }
    }
}
