//! WebSocket client for agent chat with streaming support.
//!
//! This module handles WebSocket connections to agents for real-time streaming chat
//! using the aura-swarm-protocol types (unified for local harness and remote gateway).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aura_swarm_protocol::{InboundMessage, OutboundMessage, SessionInit};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::Message;

use crate::types::{ToolInfo, TurnCompleteInfo};

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
    /// Send a `session_init` message to the harness.
    ///
    /// Must be sent immediately after connecting (both local and remote).
    /// Currently unused — session init is sent inline during connect — but
    /// retained as the canonical API for callers that manage the lifecycle
    /// in two steps (connect then init).
    #[allow(dead_code)]
    pub async fn send_session_init(&self, init: SessionInit) -> Result<(), WsError> {
        let msg = InboundMessage::SessionInit(init);
        let json = serde_json::to_string(&msg)?;
        self.tx
            .send(json)
            .await
            .map_err(|e| WsError::Send(e.to_string()))
    }

    /// Send a user message to the agent.
    pub async fn send_prompt(&self, content: &str) -> Result<(), WsError> {
        let msg = InboundMessage::UserMessage {
            content: content.to_string(),
        };
        let json = serde_json::to_string(&msg)?;
        self.tx
            .send(json)
            .await
            .map_err(|e| WsError::Send(e.to_string()))
    }

    /// Send a cancel request to stop the current turn.
    pub async fn cancel(&self) -> Result<(), WsError> {
        let msg = InboundMessage::Cancel;
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
        tools: Vec<ToolInfo>,
    },
    /// A new turn has started.
    TurnStart,
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
    /// External tool callback requested by the harness.
    ToolCallbackRequest {
        /// Callback ID for matching the response.
        callback_id: String,
        /// Tool name.
        tool_name: String,
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

    let (ws_stream, _) = tokio::time::timeout(Duration::from_secs(10), connect_async(request))
        .await
        .map_err(|_| WsError::Connection("connection timed out".to_string()))?
        .map_err(|e| WsError::Connection(e.to_string()))?;

    let (write, read) = ws_stream.split();

    let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(32);
    let (event_tx, event_rx) = mpsc::channel::<WsEvent>(32);

    tokio::spawn(ws_writer(write, outgoing_rx));
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
/// Parses protocol `OutboundMessage` types and converts them to `WsEvent` variants.
async fn ws_reader(
    mut read: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    tx: mpsc::Sender<WsEvent>,
) {
    let mut current_tool_name: Option<String> = None;

    let _ = tx.send(WsEvent::Connected).await;

    while let Some(result) = read.next().await {
        match result {
            Ok(Message::Text(text)) => {
                if let Ok(msg) = serde_json::from_str::<OutboundMessage>(&text) {
                    if let Some(event) = outbound_message_to_event(msg, &mut current_tool_name) {
                        let _ = tx.send(event).await;
                    }
                } else {
                    tracing::debug!(text = %text, "Failed to parse server message");
                    let _ = tx
                        .send(WsEvent::Error {
                            message: "Protocol error: unrecognized message".to_string(),
                            code: Some("parse_error".to_string()),
                        })
                        .await;
                }
            }
            Ok(Message::Close(_)) => {
                let _ = tx.send(WsEvent::Disconnected).await;
                break;
            }
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

/// Convert an `OutboundMessage` from the protocol crate to a `WsEvent`.
fn outbound_message_to_event(
    msg: OutboundMessage,
    current_tool_name: &mut Option<String>,
) -> Option<WsEvent> {
    match msg {
        OutboundMessage::SessionReady(ready) => Some(WsEvent::SessionReady {
            session_id: ready.session_id,
            tools: ready.tools,
        }),
        OutboundMessage::AssistantMessageStart { .. } => Some(WsEvent::TurnStart),
        OutboundMessage::TextDelta { text } => Some(WsEvent::TextDelta(text)),
        OutboundMessage::ThinkingDelta { thinking } => Some(WsEvent::ThinkingDelta(thinking)),
        OutboundMessage::ToolUseStart { id: _, name } => {
            *current_tool_name = Some(name.clone());
            Some(WsEvent::ToolStart {
                tool_name: name,
                args: serde_json::Value::Null,
            })
        }
        OutboundMessage::ToolResult {
            name,
            result,
            is_error,
        } => Some(WsEvent::ToolComplete {
            tool_name: name,
            result,
            is_error,
        }),
        OutboundMessage::AssistantMessageEnd(end) => {
            Some(WsEvent::TurnComplete(TurnCompleteInfo {
                steps: 0,
                input_tokens: end.usage.input_tokens,
                output_tokens: end.usage.output_tokens,
                model: if end.usage.model.is_empty() {
                    None
                } else {
                    Some(end.usage.model)
                },
                stop_reason: Some(end.stop_reason),
            }))
        }
        OutboundMessage::Error(err) => Some(WsEvent::Error {
            message: err.message,
            code: Some(err.code),
        }),
        OutboundMessage::ToolCallbackRequest(req) => Some(WsEvent::ToolCallbackRequest {
            callback_id: req.callback_id,
            tool_name: req.tool_name,
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
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is always after UNIX_EPOCH")
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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use aura_swarm_protocol::{
        AssistantMessageEnd, ErrorMsg, FilesChanged, SessionReady, SessionUsage,
        ToolCallbackRequest,
    };

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
    // OutboundMessage -> WsEvent conversion tests
    // =========================================================================

    #[test]
    fn session_ready_to_event() {
        let msg = OutboundMessage::SessionReady(SessionReady {
            session_id: "sess-1".to_string(),
            tools: vec![ToolInfo {
                name: "fs_read".to_string(),
                description: "Read a file".to_string(),
            }],
        });
        let mut tool_name = None;
        let event = outbound_message_to_event(msg, &mut tool_name).unwrap();
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
    fn assistant_message_start_to_event() {
        let msg = OutboundMessage::AssistantMessageStart {
            message_id: "msg-1".to_string(),
        };
        let mut tool_name = None;
        let event = outbound_message_to_event(msg, &mut tool_name).unwrap();
        assert!(matches!(event, WsEvent::TurnStart));
    }

    #[test]
    fn text_delta_to_event() {
        let msg = OutboundMessage::TextDelta {
            text: "hello".to_string(),
        };
        let mut tool_name = None;
        let event = outbound_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::TextDelta(text) => assert_eq!(text, "hello"),
            _ => panic!("Expected TextDelta event"),
        }
    }

    #[test]
    fn thinking_delta_to_event() {
        let msg = OutboundMessage::ThinkingDelta {
            thinking: "reasoning...".to_string(),
        };
        let mut tool_name = None;
        let event = outbound_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::ThinkingDelta(thinking) => assert_eq!(thinking, "reasoning..."),
            _ => panic!("Expected ThinkingDelta event"),
        }
    }

    #[test]
    fn tool_use_start_to_event() {
        let msg = OutboundMessage::ToolUseStart {
            id: "tool-1".to_string(),
            name: "fs_read".to_string(),
        };
        let mut tool_name_state = None;
        let event = outbound_message_to_event(msg, &mut tool_name_state).unwrap();
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
    fn tool_result_to_event() {
        let msg = OutboundMessage::ToolResult {
            name: "fs_read".to_string(),
            result: "file contents".to_string(),
            is_error: false,
        };
        let mut tool_name = None;
        let event = outbound_message_to_event(msg, &mut tool_name).unwrap();
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
    fn assistant_message_end_to_event() {
        let msg = OutboundMessage::AssistantMessageEnd(AssistantMessageEnd {
            message_id: "msg-1".to_string(),
            stop_reason: "end_turn".to_string(),
            usage: SessionUsage {
                input_tokens: 200,
                output_tokens: 100,
                cumulative_input_tokens: 400,
                cumulative_output_tokens: 200,
                context_utilization: 0.5,
                model: "claude-opus-4-6-20250514".to_string(),
                provider: String::new(),
            },
            files_changed: FilesChanged::default(),
        });
        let mut tool_name = None;
        let event = outbound_message_to_event(msg, &mut tool_name).unwrap();
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
    fn assistant_message_end_empty_model() {
        let msg = OutboundMessage::AssistantMessageEnd(AssistantMessageEnd {
            message_id: "msg-1".to_string(),
            stop_reason: "end_turn".to_string(),
            usage: SessionUsage {
                input_tokens: 50,
                output_tokens: 25,
                model: String::new(),
                ..SessionUsage::default()
            },
            files_changed: FilesChanged::default(),
        });
        let mut tool_name = None;
        let event = outbound_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::TurnComplete(info) => {
                assert_eq!(info.model, None);
            }
            _ => panic!("Expected TurnComplete event"),
        }
    }

    #[test]
    fn error_to_event() {
        let msg = OutboundMessage::Error(ErrorMsg {
            code: "rate_limit".to_string(),
            message: "Too many requests".to_string(),
            recoverable: true,
        });
        let mut tool_name = None;
        let event = outbound_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::Error { message, code } => {
                assert_eq!(message, "Too many requests");
                assert_eq!(code, Some("rate_limit".to_string()));
            }
            _ => panic!("Expected Error event"),
        }
    }

    #[test]
    fn tool_callback_request_to_event() {
        let msg = OutboundMessage::ToolCallbackRequest(ToolCallbackRequest {
            callback_id: "cb-42".to_string(),
            tool_name: "get_task_context".to_string(),
            input: serde_json::json!({"task_id": "t-1"}),
        });
        let mut tool_name = None;
        let event = outbound_message_to_event(msg, &mut tool_name).unwrap();
        match event {
            WsEvent::ToolCallbackRequest {
                callback_id,
                tool_name,
            } => {
                assert_eq!(callback_id, "cb-42");
                assert_eq!(tool_name, "get_task_context");
            }
            _ => panic!("Expected ToolCallbackRequest event"),
        }
    }

    #[test]
    fn full_flow_simulation() {
        let messages: Vec<OutboundMessage> = vec![
            OutboundMessage::SessionReady(SessionReady {
                session_id: "sess-1".to_string(),
                tools: vec![],
            }),
            OutboundMessage::AssistantMessageStart {
                message_id: "msg-1".to_string(),
            },
            OutboundMessage::TextDelta {
                text: "Hello".to_string(),
            },
            OutboundMessage::TextDelta {
                text: ", world!".to_string(),
            },
            OutboundMessage::AssistantMessageEnd(AssistantMessageEnd {
                message_id: "msg-1".to_string(),
                stop_reason: "end_turn".to_string(),
                usage: SessionUsage {
                    input_tokens: 10,
                    output_tokens: 3,
                    ..SessionUsage::default()
                },
                files_changed: FilesChanged::default(),
            }),
        ];

        let mut tool_name_state = None;
        let mut text_buffer = String::new();
        let mut session_ready = false;
        let mut turn_info = None;

        for msg in messages {
            if let Some(event) = outbound_message_to_event(msg, &mut tool_name_state) {
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

    // =========================================================================
    // InboundMessage serialization tests
    // =========================================================================

    #[test]
    fn user_message_serializes_correctly() {
        let msg = InboundMessage::UserMessage {
            content: "Hello, agent!".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "user_message");
        assert_eq!(parsed["content"], "Hello, agent!");
    }

    #[test]
    fn cancel_message_serializes_correctly() {
        let msg = InboundMessage::Cancel;
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "cancel");
    }

    #[test]
    fn session_init_serializes_correctly() {
        let msg = InboundMessage::SessionInit(SessionInit {
            model: Some("claude-opus-4-6-20250514".to_string()),
            system_prompt: Some("You are helpful".to_string()),
            ..SessionInit::default()
        });
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "session_init");
        assert_eq!(parsed["model"], "claude-opus-4-6-20250514");
        assert_eq!(parsed["system_prompt"], "You are helpful");
    }
}
