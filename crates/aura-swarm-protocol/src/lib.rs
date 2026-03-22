//! Shared WebSocket protocol types for the aura-harness `/stream` endpoint.
//!
//! This crate defines the canonical message format for communication between
//! clients and the aura-harness reasoning engine. Both the CLI and gateway
//! depend on these types to ensure protocol consistency.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

// =============================================================================
// Inbound Messages (Client -> Harness)
// =============================================================================

/// Client -> Harness: top-level message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundMessage {
    /// Initialize the session (must be the first message).
    SessionInit(SessionInit),
    /// Send a user message for processing.
    UserMessage {
        /// The user's message text.
        content: String,
    },
    /// Cancel the current turn.
    Cancel,
    /// External tool callback response (client -> harness).
    ToolCallbackResponse(ToolCallbackResponse),
}

/// Payload for `session_init`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionInit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_tools: Vec<ExternalToolDef>,
}

/// External tool definition for session registration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

// =============================================================================
// Outbound Messages (Harness -> Client)
// =============================================================================

/// Harness -> Client: top-level message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboundMessage {
    /// Session initialized and ready.
    SessionReady(SessionReady),
    /// Start of an assistant message (turn).
    AssistantMessageStart {
        message_id: String,
    },
    /// Incremental text content.
    TextDelta {
        text: String,
    },
    /// Incremental thinking content.
    ThinkingDelta {
        thinking: String,
    },
    /// A tool use has started.
    ToolUseStart {
        id: String,
        name: String,
    },
    /// Result of a tool execution.
    ToolResult {
        name: String,
        result: String,
        is_error: bool,
    },
    /// End of an assistant message (turn complete).
    AssistantMessageEnd(AssistantMessageEnd),
    /// An error occurred.
    Error(ErrorMsg),
    /// External tool callback request (harness -> client).
    ToolCallbackRequest(ToolCallbackRequest),
}

/// Payload for `session_ready`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReady {
    pub session_id: String,
    pub tools: Vec<ToolInfo>,
}

/// Minimal tool info in the session_ready response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

/// Payload for `assistant_message_end`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessageEnd {
    pub message_id: String,
    pub stop_reason: String,
    pub usage: SessionUsage,
    #[serde(default)]
    pub files_changed: FilesChanged,
}

/// Token usage information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cumulative_input_tokens: u64,
    #[serde(default)]
    pub cumulative_output_tokens: u64,
    #[serde(default)]
    pub context_utilization: f32,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub provider: String,
}

/// Summary of file mutations during a turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilesChanged {
    #[serde(default)]
    pub created: Vec<String>,
    #[serde(default)]
    pub modified: Vec<String>,
    #[serde(default)]
    pub deleted: Vec<String>,
}

/// Error message payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorMsg {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
}

// =============================================================================
// Tool Callback Protocol
// =============================================================================

/// Tool callback request from harness to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallbackRequest {
    pub callback_id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
}

/// Tool callback response from client to harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallbackResponse {
    pub callback_id: String,
    pub result: String,
    pub is_error: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_init_serializes() {
        let msg = InboundMessage::SessionInit(SessionInit {
            system_prompt: Some("You are helpful".into()),
            model: Some("claude-opus-4-6-20250514".into()),
            ..Default::default()
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"session_init\""));
        assert!(json.contains("\"system_prompt\":\"You are helpful\""));
    }

    #[test]
    fn user_message_serializes() {
        let msg = InboundMessage::UserMessage { content: "Hello".into() };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"user_message\""));
        assert!(json.contains("\"content\":\"Hello\""));
    }

    #[test]
    fn session_ready_deserializes() {
        let json = r#"{"type":"session_ready","session_id":"s1","tools":[{"name":"fs_read","description":"Read file"}]}"#;
        let msg: OutboundMessage = serde_json::from_str(json).unwrap();
        match msg {
            OutboundMessage::SessionReady(ready) => {
                assert_eq!(ready.session_id, "s1");
                assert_eq!(ready.tools.len(), 1);
            }
            _ => panic!("Expected SessionReady"),
        }
    }

    #[test]
    fn assistant_message_end_deserializes() {
        let json = r#"{
            "type":"assistant_message_end",
            "message_id":"m1",
            "stop_reason":"end_turn",
            "usage":{"input_tokens":100,"output_tokens":50,"model":"claude","provider":"anthropic"},
            "files_changed":{"created":[],"modified":["main.rs"],"deleted":[]}
        }"#;
        let msg: OutboundMessage = serde_json::from_str(json).unwrap();
        match msg {
            OutboundMessage::AssistantMessageEnd(end) => {
                assert_eq!(end.stop_reason, "end_turn");
                assert_eq!(end.usage.input_tokens, 100);
                assert_eq!(end.files_changed.modified, vec!["main.rs"]);
            }
            _ => panic!("Expected AssistantMessageEnd"),
        }
    }

    #[test]
    fn tool_callback_roundtrip() {
        let req = ToolCallbackRequest {
            callback_id: "cb-1".into(),
            tool_name: "task_done".into(),
            input: serde_json::json!({"notes": "done"}),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ToolCallbackRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.callback_id, "cb-1");
        assert_eq!(parsed.tool_name, "task_done");
    }

    #[test]
    fn cancel_serializes() {
        let msg = InboundMessage::Cancel;
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"cancel\""));
    }

    #[test]
    fn error_deserializes() {
        let json = r#"{"type":"error","code":"turn_error","message":"Something failed","recoverable":true}"#;
        let msg: OutboundMessage = serde_json::from_str(json).unwrap();
        match msg {
            OutboundMessage::Error(e) => {
                assert_eq!(e.code, "turn_error");
                assert!(e.recoverable);
            }
            _ => panic!("Expected Error"),
        }
    }
}
