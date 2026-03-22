//! WebSocket client for agent chat with streaming support.
//!
//! This module handles WebSocket connections to agents for real-time streaming chat
//! using the Aura runtime protocol.
//!
//! Endpoint: WS /stream

use std::time::{SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::Message;

use crate::types::{
    ClientMessage, HarnessClientMessage, HarnessServerMessage, HarnessSessionInit,
    HarnessToolInfo, ServerMessage, TurnCompleteInfo,
};

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

    /// Send a harness `session_init` message.
    pub async fn send_session_init(&self, init: HarnessSessionInit) -> Result<(), WsError> {
        let msg = HarnessClientMessage::SessionInit(init);
        let json = serde_json::to_string(&msg)?;
        self.tx
            .send(json)
            .await
            .map_err(|e| WsError::Send(e.to_string()))
    }

    /// Send a harness `user_message`.
    pub async fn send_user_message(&self, content: &str) -> Result<(), WsError> {
        let msg = HarnessClientMessage::UserMessage {
            content: content.to_string(),
        };
        let json = serde_json::to_string(&msg)?;
        self.tx
            .send(json)
            .await
            .map_err(|e| WsError::Send(e.to_string()))
    }

    /// Send a harness `cancel` message.
    pub async fn send_harness_cancel(&self) -> Result<(), WsError> {
        let msg = HarnessClientMessage::Cancel;
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
    /// Harness session initialized and ready.
    SessionReady {
        /// Session ID assigned by the harness.
        session_id: String,
        /// Tools available in the session.
        tools: Vec<HarnessToolInfo>,
    },
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
                // Try harness protocol first, then legacy
                if let Ok(harness_msg) = serde_json::from_str::<HarnessServerMessage>(&text) {
                    if let Some(event) =
                        harness_message_to_event(harness_msg, &mut current_tool_name)
                    {
                        let _ = tx.send(event).await;
                    }
                } else if let Ok(legacy_msg) = serde_json::from_str::<ServerMessage>(&text) {
                    if let Some(event) =
                        legacy_message_to_event(legacy_msg, &mut current_tool_name)
                    {
                        let _ = tx.send(event).await;
                    }
                } else {
                    tracing::debug!(text = %text, "Failed to parse server message");
                    let _ = tx
                        .send(WsEvent::Error {
                            message: format!("Protocol error: unrecognized message"),
                            code: Some("parse_error".to_string()),
                        })
                        .await;
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

/// Convert a `HarnessServerMessage` to a `WsEvent`.
fn harness_message_to_event(
    msg: HarnessServerMessage,
    current_tool_name: &mut Option<String>,
) -> Option<WsEvent> {
    match msg {
        HarnessServerMessage::SessionReady { session_id, tools } => {
            Some(WsEvent::SessionReady { session_id, tools })
        }
        HarnessServerMessage::AssistantMessageStart { .. } => Some(WsEvent::TurnStart),
        HarnessServerMessage::TextDelta { text } => Some(WsEvent::TextDelta(text)),
        HarnessServerMessage::ThinkingDelta { thinking } => Some(WsEvent::ThinkingDelta(thinking)),
        HarnessServerMessage::ToolUseStart { id: _, name } => {
            *current_tool_name = Some(name.clone());
            Some(WsEvent::ToolStart {
                tool_name: name,
                args: serde_json::Value::Null,
            })
        }
        HarnessServerMessage::ToolResult {
            name,
            result,
            is_error,
        } => Some(WsEvent::ToolComplete {
            tool_name: name,
            result,
            is_error,
        }),
        HarnessServerMessage::AssistantMessageEnd {
            stop_reason,
            usage,
            ..
        } => Some(WsEvent::TurnComplete(TurnCompleteInfo {
            steps: 0,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            model: if usage.model.is_empty() {
                None
            } else {
                Some(usage.model)
            },
            stop_reason: Some(stop_reason),
        })),
        HarnessServerMessage::Error {
            code, message, ..
        } => Some(WsEvent::Error {
            message,
            code: Some(code),
        }),
    }
}

/// Convert a legacy `ServerMessage` to a `WsEvent`.
fn legacy_message_to_event(
    msg: ServerMessage,
    current_tool_name: &mut Option<String>,
) -> Option<WsEvent> {
    match msg {
        ServerMessage::TurnStart {
            request_id: _,
            agent_id,
        } => {
            tracing::debug!(agent_id = %agent_id, "Turn started");
            Some(WsEvent::TurnStart)
        }
        ServerMessage::StepStart {
            request_id: _,
            agent_id,
            step,
        } => {
            tracing::debug!(agent_id = %agent_id, step = step, "Step started");
            Some(WsEvent::StepStart { step })
        }
        ServerMessage::TextDelta {
            request_id: _,
            agent_id: _,
            text,
        } => Some(WsEvent::TextDelta(text)),
        ServerMessage::ThinkingDelta {
            request_id: _,
            agent_id: _,
            thinking,
        } => Some(WsEvent::ThinkingDelta(thinking)),
        ServerMessage::ToolStart {
            request_id: _,
            agent_id: _,
            tool_id,
            tool_name,
            args,
        } => {
            tracing::debug!(tool = %tool_name, tool_id = %tool_id, "Tool execution started");
            *current_tool_name = Some(tool_name.clone());
            Some(WsEvent::ToolStart { tool_name, args })
        }
        ServerMessage::ToolComplete {
            request_id: _,
            agent_id: _,
            tool_id,
            result,
            is_error,
        } => {
            tracing::debug!(tool_id = %tool_id, is_error = is_error, "Tool execution completed");
            let tool_name = current_tool_name.take().unwrap_or_default();
            Some(WsEvent::ToolComplete {
                tool_name,
                result,
                is_error,
            })
        }
        ServerMessage::TurnComplete {
            request_id: _,
            agent_id,
            steps,
            input_tokens,
            output_tokens,
        } => {
            tracing::debug!(agent_id = %agent_id, steps = steps, "Turn complete");
            Some(WsEvent::TurnComplete(TurnCompleteInfo {
                steps,
                input_tokens: u64::from(input_tokens),
                output_tokens: u64::from(output_tokens),
                model: None,
                stop_reason: None,
            }))
        }
        ServerMessage::Cancelled {
            request_id,
            agent_id: _,
        } => Some(WsEvent::Cancelled { request_id }),
        ServerMessage::Error {
            request_id: _,
            agent_id: _,
            error,
            code,
        } => Some(WsEvent::Error {
            message: error,
            code,
        }),
    }
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
/// Delegates to the production `legacy_message_to_event` function.
#[cfg(test)]
fn server_message_to_event(
    msg: ServerMessage,
    current_tool_name: &mut Option<String>,
) -> Option<WsEvent> {
    legacy_message_to_event(msg, current_tool_name)
}

// =============================================================================
// Tests (Aura Runtime Protocol)
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
        assert_eq!(
            extract_host("ws://localhost:8080/stream"),
            Some("localhost:8080")
        );
        assert_eq!(
            extract_host("ws://192.168.1.1:3000/api/ws"),
            Some("192.168.1.1:3000")
        );
        assert_eq!(extract_host("ws://example.com"), Some("example.com"));
    }

    #[test]
    fn extract_host_from_wss_url() {
        assert_eq!(
            extract_host("wss://secure.example.com/stream"),
            Some("secure.example.com")
        );
        assert_eq!(
            extract_host("wss://api.example.com:443/ws"),
            Some("api.example.com:443")
        );
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
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn generate_ws_key_is_valid_base64() {
        let key = generate_ws_key();
        assert!(key.len() >= 20);
        assert!(key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
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
    fn convert_text_delta_to_event() {
        let msg = ServerMessage::TextDelta {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            text: "Hello, world!".to_string(),
        };
        let mut tool_name = None;
        let event = server_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::TextDelta(text) => assert_eq!(text, "Hello, world!"),
            _ => panic!("Expected TextDelta event"),
        }
    }

    #[test]
    fn convert_tool_start_to_event() {
        let msg = ServerMessage::ToolStart {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            tool_id: "t1".to_string(),
            tool_name: "read_file".to_string(),
            args: json!({"path": "/etc/passwd"}),
        };
        let mut tool_name_state = None;
        let event = server_message_to_event(msg, &mut tool_name_state).unwrap();
        match event {
            WsEvent::ToolStart { tool_name, args } => {
                assert_eq!(tool_name, "read_file");
                assert_eq!(args["path"], "/etc/passwd");
            }
            _ => panic!("Expected ToolStart event"),
        }
        assert_eq!(tool_name_state, Some("read_file".to_string()));
    }

    #[test]
    fn convert_tool_complete_to_event() {
        let msg = ServerMessage::ToolComplete {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            tool_id: "t1".to_string(),
            result: "file contents".to_string(),
            is_error: false,
        };
        let mut tool_name_state = Some("read_file".to_string());
        let event = server_message_to_event(msg, &mut tool_name_state).unwrap();
        match event {
            WsEvent::ToolComplete {
                tool_name,
                result,
                is_error,
            } => {
                assert_eq!(tool_name, "read_file");
                assert_eq!(result, "file contents");
                assert!(!is_error);
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
            output_tokens: 800,
        };
        let mut tool_name = None;
        let event = server_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::TurnComplete(info) => {
                assert_eq!(info.steps, 3);
                assert_eq!(info.input_tokens, 1500u64);
                assert_eq!(info.output_tokens, 800u64);
                assert_eq!(info.model, None);
                assert_eq!(info.stop_reason, None);
            }
            _ => panic!("Expected TurnComplete event"),
        }
    }

    #[test]
    fn convert_error_to_event() {
        let msg = ServerMessage::Error {
            request_id: "req-1".to_string(),
            agent_id: Some("agent-1".to_string()),
            error: "Something went wrong".to_string(),
            code: Some("TURN_ERROR".to_string()),
        };
        let mut tool_name = None;
        let event = server_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::Error { message, code } => {
                assert_eq!(message, "Something went wrong");
                assert_eq!(code, Some("TURN_ERROR".to_string()));
            }
            _ => panic!("Expected Error event"),
        }
    }

    // =========================================================================
    // ClientMessage Serialization Tests
    // =========================================================================

    #[test]
    fn prompt_message_serializes_correctly() {
        let msg = ClientMessage::Prompt {
            request_id: "req-test".to_string(),
            prompt: "Hello, agent!".to_string(),
            agent_id: Some("agent-123".to_string()),
            workspace: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["type"], "prompt");
        assert_eq!(parsed["request_id"], "req-test");
        assert_eq!(parsed["prompt"], "Hello, agent!");
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
    fn simulate_simple_response_flow() {
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
                text: ", world!".to_string(),
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
    fn simulate_tool_use_flow() {
        let messages = vec![
            ServerMessage::TurnStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
            },
            ServerMessage::ToolStart {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                tool_id: "t1".to_string(),
                tool_name: "read_file".to_string(),
                args: json!({"path": "README.md"}),
            },
            ServerMessage::ToolComplete {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                tool_id: "t1".to_string(),
                result: "# README\nProject docs".to_string(),
                is_error: false,
            },
            ServerMessage::TextDelta {
                request_id: "req-1".to_string(),
                agent_id: "agent-1".to_string(),
                text: "I found the file.".to_string(),
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
        let mut tool_names = Vec::new();
        let mut tool_succeeded = false;

        for msg in messages {
            if let Some(event) = server_message_to_event(msg, &mut tool_name_state) {
                match event {
                    WsEvent::ToolComplete {
                        tool_name,
                        is_error,
                        ..
                    } => {
                        tool_names.push(tool_name);
                        tool_succeeded = !is_error;
                    }
                    _ => {}
                }
            }
        }

        assert_eq!(tool_names, vec!["read_file"]);
        assert!(tool_succeeded);
    }

    // =========================================================================
    // Harness Protocol Conversion Tests
    // =========================================================================

    #[test]
    fn harness_session_ready_to_event() {
        let msg = HarnessServerMessage::SessionReady {
            session_id: "sess-1".to_string(),
            tools: vec![HarnessToolInfo {
                name: "fs_read".to_string(),
                description: "Read a file".to_string(),
            }],
        };
        let mut tool_name = None;
        let event = harness_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::SessionReady { session_id, tools } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0].name, "fs_read");
            }
            _ => panic!("Expected SessionReady event"),
        }
    }

    #[test]
    fn harness_assistant_message_start_to_event() {
        let msg = HarnessServerMessage::AssistantMessageStart {
            message_id: "msg-1".to_string(),
        };
        let mut tool_name = None;
        let event = harness_message_to_event(msg, &mut tool_name).unwrap();
        assert!(matches!(event, WsEvent::TurnStart));
    }

    #[test]
    fn harness_text_delta_to_event() {
        let msg = HarnessServerMessage::TextDelta {
            text: "hello".to_string(),
        };
        let mut tool_name = None;
        let event = harness_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::TextDelta(text) => assert_eq!(text, "hello"),
            _ => panic!("Expected TextDelta event"),
        }
    }

    #[test]
    fn harness_thinking_delta_to_event() {
        let msg = HarnessServerMessage::ThinkingDelta {
            thinking: "reasoning...".to_string(),
        };
        let mut tool_name = None;
        let event = harness_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::ThinkingDelta(thinking) => assert_eq!(thinking, "reasoning..."),
            _ => panic!("Expected ThinkingDelta event"),
        }
    }

    #[test]
    fn harness_tool_use_start_to_event() {
        let msg = HarnessServerMessage::ToolUseStart {
            id: "tool-1".to_string(),
            name: "fs_read".to_string(),
        };
        let mut tool_name_state = None;
        let event = harness_message_to_event(msg, &mut tool_name_state).unwrap();
        match event {
            WsEvent::ToolStart { tool_name, args } => {
                assert_eq!(tool_name, "fs_read");
                assert_eq!(args, serde_json::Value::Null);
            }
            _ => panic!("Expected ToolStart event"),
        }
        assert_eq!(tool_name_state, Some("fs_read".to_string()));
    }

    #[test]
    fn harness_tool_result_to_event() {
        let msg = HarnessServerMessage::ToolResult {
            name: "fs_read".to_string(),
            result: "file contents".to_string(),
            is_error: false,
        };
        let mut tool_name = None;
        let event = harness_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::ToolComplete {
                tool_name,
                result,
                is_error,
            } => {
                assert_eq!(tool_name, "fs_read");
                assert_eq!(result, "file contents");
                assert!(!is_error);
            }
            _ => panic!("Expected ToolComplete event"),
        }
    }

    #[test]
    fn harness_assistant_message_end_to_event() {
        use crate::types::{HarnessFilesChanged, HarnessUsage};

        let msg = HarnessServerMessage::AssistantMessageEnd {
            message_id: "msg-1".to_string(),
            stop_reason: "end_turn".to_string(),
            usage: HarnessUsage {
                input_tokens: 200,
                output_tokens: 100,
                cumulative_input_tokens: 400,
                cumulative_output_tokens: 200,
                context_utilization: 0.5,
                model: "claude-opus-4-6-20250514".to_string(),
                provider: String::new(),
            },
            files_changed: HarnessFilesChanged::default(),
        };
        let mut tool_name = None;
        let event = harness_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::TurnComplete(info) => {
                assert_eq!(info.steps, 0);
                assert_eq!(info.input_tokens, 200u64);
                assert_eq!(info.output_tokens, 100u64);
                assert_eq!(info.model, Some("claude-opus-4-6-20250514".to_string()));
                assert_eq!(info.stop_reason, Some("end_turn".to_string()));
            }
            _ => panic!("Expected TurnComplete event"),
        }
    }

    #[test]
    fn harness_assistant_message_end_empty_model() {
        use crate::types::{HarnessFilesChanged, HarnessUsage};

        let msg = HarnessServerMessage::AssistantMessageEnd {
            message_id: "msg-1".to_string(),
            stop_reason: "end_turn".to_string(),
            usage: HarnessUsage {
                input_tokens: 50,
                output_tokens: 25,
                model: String::new(),
                ..HarnessUsage::default()
            },
            files_changed: HarnessFilesChanged::default(),
        };
        let mut tool_name = None;
        let event = harness_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::TurnComplete(info) => {
                assert_eq!(info.model, None);
            }
            _ => panic!("Expected TurnComplete event"),
        }
    }

    #[test]
    fn harness_error_to_event() {
        let msg = HarnessServerMessage::Error {
            code: "rate_limit".to_string(),
            message: "Too many requests".to_string(),
            recoverable: true,
        };
        let mut tool_name = None;
        let event = harness_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::Error { message, code } => {
                assert_eq!(message, "Too many requests");
                assert_eq!(code, Some("rate_limit".to_string()));
            }
            _ => panic!("Expected Error event"),
        }
    }

    #[test]
    fn harness_full_flow_simulation() {
        use crate::types::{HarnessFilesChanged, HarnessUsage};

        let messages: Vec<HarnessServerMessage> = vec![
            HarnessServerMessage::SessionReady {
                session_id: "sess-1".to_string(),
                tools: vec![],
            },
            HarnessServerMessage::AssistantMessageStart {
                message_id: "msg-1".to_string(),
            },
            HarnessServerMessage::TextDelta {
                text: "Hello".to_string(),
            },
            HarnessServerMessage::TextDelta {
                text: ", world!".to_string(),
            },
            HarnessServerMessage::AssistantMessageEnd {
                message_id: "msg-1".to_string(),
                stop_reason: "end_turn".to_string(),
                usage: HarnessUsage {
                    input_tokens: 10,
                    output_tokens: 3,
                    ..HarnessUsage::default()
                },
                files_changed: HarnessFilesChanged::default(),
            },
        ];

        let mut tool_name_state = None;
        let mut text_buffer = String::new();
        let mut session_ready = false;
        let mut turn_info = None;

        for msg in messages {
            if let Some(event) = harness_message_to_event(msg, &mut tool_name_state) {
                match event {
                    WsEvent::SessionReady { .. } => session_ready = true,
                    WsEvent::TextDelta(text) => text_buffer.push_str(&text),
                    WsEvent::TurnComplete(info) => turn_info = Some(info),
                    _ => {}
                }
            }
        }

        assert!(session_ready);
        assert_eq!(text_buffer, "Hello, world!");
        let info = turn_info.unwrap();
        assert_eq!(info.input_tokens, 10u64);
        assert_eq!(info.output_tokens, 3u64);
    }
}
