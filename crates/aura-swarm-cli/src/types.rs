//! API response types for the gateway client.
//!
//! These types mirror the responses from the aura-swarm-gateway API.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// =============================================================================
// Agent Types
// =============================================================================

/// Agent state as returned by the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Pod is being created, Aura initializing.
    Provisioning,
    /// Agent is active and accepting sessions.
    Running,
    /// No active sessions, still running.
    Idle,
    /// State saved, pod terminated, instant wake.
    Hibernating,
    /// Graceful shutdown in progress.
    Stopping,
    /// Pod terminated, state preserved.
    Stopped,
    /// Health check failed or crash.
    Error,
}

impl AgentState {
    /// Human-readable display string.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Provisioning => "Provisioning",
            Self::Running => "Running",
            Self::Idle => "Idle",
            Self::Hibernating => "Hibernating",
            Self::Stopping => "Stopping",
            Self::Stopped => "Stopped",
            Self::Error => "Error",
        }
    }

    /// Color for displaying in the TUI.
    #[must_use]
    pub const fn color(&self) -> ratatui::style::Color {
        use ratatui::style::Color;
        match self {
            Self::Running => Color::Green,
            Self::Idle => Color::Yellow,
            Self::Provisioning => Color::Cyan,
            Self::Hibernating => Color::Magenta,
            Self::Stopping => Color::Yellow,
            Self::Stopped => Color::Gray,
            Self::Error => Color::Red,
        }
    }
}

/// Agent resource specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    /// CPU allocation in millicores.
    pub cpu_millicores: u32,
    /// Memory allocation in megabytes.
    pub memory_mb: u32,
    /// Aura runtime version.
    pub runtime_version: String,
}

/// Agent response from the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// Agent ID (hex string).
    pub agent_id: String,
    /// Human-readable name.
    pub name: String,
    /// Current lifecycle state.
    pub status: AgentState,
    /// Resource specification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<AgentSpec>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Last heartbeat timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Error message if agent is in Error/Failed state.
    #[serde(default)]
    pub error_message: Option<String>,
}

/// Response for listing agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListAgentsResponse {
    /// List of agents.
    pub agents: Vec<Agent>,
}

/// Request to create an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentRequest {
    /// Human-readable name for the agent.
    pub name: String,
}

/// Response for lifecycle operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleResponse {
    /// Agent ID.
    pub agent_id: String,
    /// New status after the operation.
    pub status: AgentState,
}

// =============================================================================
// Session Types
// =============================================================================

/// Response for creating a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    /// Session ID.
    pub session_id: String,
    /// Agent ID.
    pub agent_id: String,
    /// WebSocket URL for connecting.
    pub ws_url: String,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
}

// =============================================================================
// WebSocket Message Types
// =============================================================================

/// Message sent to/from the agent via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Message role: "user" or "assistant".
    pub role: String,
    /// Message content.
    pub content: String,
}

impl ChatMessage {
    /// Create a user message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    /// Create an assistant message.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }

    /// Check if this is a user message.
    #[must_use]
    pub fn is_user(&self) -> bool {
        self.role == "user"
    }
}

// =============================================================================
// Error Response
// =============================================================================

/// Error response from the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorResponse {
    /// Error message.
    pub error: String,
    /// Optional error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

// =============================================================================
// WebSocket Streaming Protocol Types (Aura Runtime Protocol)
// =============================================================================

/// Client -> Server: Messages sent to the Aura runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Send a prompt to the agent.
    Prompt {
        /// Unique request ID for tracking.
        request_id: String,
        /// The user's prompt text.
        prompt: String,
        /// Optional agent ID (for multi-agent scenarios).
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        /// Optional workspace path.
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    /// Cancel an in-progress request.
    Cancel {
        /// Request ID to cancel.
        request_id: String,
    },
}

/// Server -> Client: Messages from the Aura runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// A new turn has started.
    TurnStart {
        /// Request ID.
        request_id: String,
        /// Agent ID processing the request.
        agent_id: String,
    },
    /// A new step within the turn has started.
    StepStart {
        /// Request ID.
        request_id: String,
        /// Agent ID.
        agent_id: String,
        /// Step number (1-indexed).
        step: u32,
    },
    /// The turn has completed.
    TurnComplete {
        /// Request ID.
        request_id: String,
        /// Agent ID.
        agent_id: String,
        /// Total number of steps in this turn.
        steps: u32,
        /// Input tokens used.
        input_tokens: u32,
        /// Output tokens used.
        output_tokens: u32,
    },
    /// Streamed text content.
    TextDelta {
        /// Request ID.
        request_id: String,
        /// Agent ID.
        agent_id: String,
        /// Text fragment to append.
        text: String,
    },
    /// Extended thinking content.
    ThinkingDelta {
        /// Request ID.
        request_id: String,
        /// Agent ID.
        agent_id: String,
        /// Thinking text fragment.
        thinking: String,
    },
    /// Tool execution started (server-side).
    ToolStart {
        /// Request ID.
        request_id: String,
        /// Agent ID.
        agent_id: String,
        /// Unique tool invocation ID.
        tool_id: String,
        /// Name of the tool being executed.
        tool_name: String,
        /// Tool arguments.
        args: serde_json::Value,
    },
    /// Tool execution completed (server-side).
    ToolComplete {
        /// Request ID.
        request_id: String,
        /// Agent ID.
        agent_id: String,
        /// Tool invocation ID.
        tool_id: String,
        /// Tool execution result.
        result: String,
        /// Whether the tool execution resulted in an error.
        is_error: bool,
    },
    /// An error occurred.
    Error {
        /// Request ID (if available).
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        /// Agent ID (if available).
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        /// Error message.
        error: String,
        /// Optional error code.
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    /// Request was successfully cancelled.
    Cancelled {
        /// Request ID.
        request_id: String,
        /// Agent ID.
        agent_id: String,
    },
}

/// Response metadata from turn completion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnCompleteInfo {
    /// Total number of steps.
    pub steps: u32,
    /// Input tokens used.
    pub input_tokens: u32,
    /// Output tokens used.
    pub output_tokens: u32,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // ClientMessage Serialization Tests
    // =========================================================================

    #[test]
    fn client_message_prompt_serializes_correctly() {
        let msg = ClientMessage::Prompt {
            request_id: "req-123".to_string(),
            prompt: "Hello, agent!".to_string(),
            agent_id: Some("agent-456".to_string()),
            workspace: Some("/workspace/project".to_string()),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["type"], "prompt");
        assert_eq!(parsed["request_id"], "req-123");
        assert_eq!(parsed["prompt"], "Hello, agent!");
        assert_eq!(parsed["agent_id"], "agent-456");
        assert_eq!(parsed["workspace"], "/workspace/project");
    }

    #[test]
    fn client_message_prompt_omits_none_fields() {
        let msg = ClientMessage::Prompt {
            request_id: "req-123".to_string(),
            prompt: "Hello".to_string(),
            agent_id: None,
            workspace: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["type"], "prompt");
        assert!(parsed.get("agent_id").is_none());
        assert!(parsed.get("workspace").is_none());
    }

    #[test]
    fn client_message_cancel_serializes_correctly() {
        let msg = ClientMessage::Cancel {
            request_id: "req-789".to_string(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["type"], "cancel");
        assert_eq!(parsed["request_id"], "req-789");
    }

    #[test]
    fn client_message_prompt_with_special_characters() {
        let msg = ClientMessage::Prompt {
            request_id: "req-special".to_string(),
            prompt: "Hello \"world\"!\nNew line\tTab\u{1F600}".to_string(),
            agent_id: None,
            workspace: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        let roundtrip: ClientMessage = serde_json::from_str(&json).unwrap();

        match roundtrip {
            ClientMessage::Prompt { prompt, .. } => {
                assert!(prompt.contains("\"world\""));
                assert!(prompt.contains('\n'));
                assert!(prompt.contains('\t'));
                assert!(prompt.contains('\u{1F600}'));
            }
            _ => panic!("Expected Prompt variant"),
        }
    }

    #[test]
    fn client_message_prompt_with_empty_strings() {
        let msg = ClientMessage::Prompt {
            request_id: "".to_string(),
            prompt: "".to_string(),
            agent_id: Some("".to_string()),
            workspace: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        let roundtrip: ClientMessage = serde_json::from_str(&json).unwrap();

        match roundtrip {
            ClientMessage::Prompt { request_id, prompt, agent_id, .. } => {
                assert_eq!(request_id, "");
                assert_eq!(prompt, "");
                assert_eq!(agent_id, Some("".to_string()));
            }
            _ => panic!("Expected Prompt variant"),
        }
    }

    // =========================================================================
    // ServerMessage Deserialization Tests
    // =========================================================================

    #[test]
    fn server_message_turn_start_deserializes() {
        let json = r#"{"type":"turn_start","request_id":"req-1","agent_id":"agent-1"}"#;
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
    fn server_message_step_start_deserializes() {
        let json = r#"{"type":"step_start","request_id":"req-1","agent_id":"agent-1","step":3}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::StepStart { request_id, agent_id, step } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(agent_id, "agent-1");
                assert_eq!(step, 3);
            }
            _ => panic!("Expected StepStart"),
        }
    }

    #[test]
    fn server_message_turn_complete_deserializes() {
        let json = r#"{
            "type": "turn_complete",
            "request_id": "req-1",
            "agent_id": "agent-1",
            "steps": 5,
            "input_tokens": 1500,
            "output_tokens": 800
        }"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::TurnComplete { request_id, agent_id, steps, input_tokens, output_tokens } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(agent_id, "agent-1");
                assert_eq!(steps, 5);
                assert_eq!(input_tokens, 1500);
                assert_eq!(output_tokens, 800);
            }
            _ => panic!("Expected TurnComplete"),
        }
    }

    #[test]
    fn server_message_text_delta_deserializes() {
        let json = r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":"Hello, "}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::TextDelta { request_id, agent_id, text } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(agent_id, "agent-1");
                assert_eq!(text, "Hello, ");
            }
            _ => panic!("Expected TextDelta"),
        }
    }

    #[test]
    fn server_message_thinking_delta_deserializes() {
        let json = r#"{"type":"thinking_delta","request_id":"req-1","agent_id":"agent-1","thinking":"Analyzing..."}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::ThinkingDelta { request_id, agent_id, thinking } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(agent_id, "agent-1");
                assert_eq!(thinking, "Analyzing...");
            }
            _ => panic!("Expected ThinkingDelta"),
        }
    }

    #[test]
    fn server_message_tool_start_deserializes() {
        let json = r#"{
            "type": "tool_start",
            "request_id": "req-1",
            "agent_id": "agent-1",
            "tool_id": "tool-123",
            "tool_name": "fs_read",
            "args": {"path": "/etc/passwd", "max_bytes": 1024}
        }"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::ToolStart { request_id, agent_id, tool_id, tool_name, args } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(agent_id, "agent-1");
                assert_eq!(tool_id, "tool-123");
                assert_eq!(tool_name, "fs_read");
                assert_eq!(args["path"], "/etc/passwd");
                assert_eq!(args["max_bytes"], 1024);
            }
            _ => panic!("Expected ToolStart"),
        }
    }

    #[test]
    fn server_message_tool_start_with_complex_args() {
        let json = r#"{
            "type": "tool_start",
            "request_id": "req-1",
            "agent_id": "agent-1",
            "tool_id": "tool-456",
            "tool_name": "cmd_run",
            "args": {
                "program": "ls",
                "args": ["-la", "/tmp"],
                "env": {"PATH": "/usr/bin", "HOME": "/root"},
                "timeout_ms": 5000
            }
        }"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::ToolStart { args, .. } => {
                assert_eq!(args["program"], "ls");
                assert_eq!(args["args"][0], "-la");
                assert_eq!(args["args"][1], "/tmp");
                assert_eq!(args["env"]["PATH"], "/usr/bin");
                assert_eq!(args["timeout_ms"], 5000);
            }
            _ => panic!("Expected ToolStart"),
        }
    }

    #[test]
    fn server_message_tool_complete_success_deserializes() {
        let json = r#"{
            "type": "tool_complete",
            "request_id": "req-1",
            "agent_id": "agent-1",
            "tool_id": "tool-123",
            "result": "file contents here...",
            "is_error": false
        }"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::ToolComplete { request_id, agent_id, tool_id, result, is_error } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(agent_id, "agent-1");
                assert_eq!(tool_id, "tool-123");
                assert_eq!(result, "file contents here...");
                assert!(!is_error);
            }
            _ => panic!("Expected ToolComplete"),
        }
    }

    #[test]
    fn server_message_tool_complete_error_deserializes() {
        let json = r#"{
            "type": "tool_complete",
            "request_id": "req-1",
            "agent_id": "agent-1",
            "tool_id": "tool-123",
            "result": "ENOENT: file not found",
            "is_error": true
        }"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::ToolComplete { is_error, result, .. } => {
                assert!(is_error);
                assert!(result.contains("ENOENT"));
            }
            _ => panic!("Expected ToolComplete"),
        }
    }

    #[test]
    fn server_message_error_with_all_fields_deserializes() {
        let json = r#"{
            "type": "error",
            "request_id": "req-1",
            "agent_id": "agent-1",
            "error": "Rate limit exceeded",
            "code": "rate_limit"
        }"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::Error { request_id, agent_id, error, code } => {
                assert_eq!(request_id, Some("req-1".to_string()));
                assert_eq!(agent_id, Some("agent-1".to_string()));
                assert_eq!(error, "Rate limit exceeded");
                assert_eq!(code, Some("rate_limit".to_string()));
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn server_message_error_with_minimal_fields_deserializes() {
        let json = r#"{"type":"error","error":"Something went wrong"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::Error { request_id, agent_id, error, code } => {
                assert_eq!(request_id, None);
                assert_eq!(agent_id, None);
                assert_eq!(error, "Something went wrong");
                assert_eq!(code, None);
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn server_message_cancelled_deserializes() {
        let json = r#"{"type":"cancelled","request_id":"req-1","agent_id":"agent-1"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::Cancelled { request_id, agent_id } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(agent_id, "agent-1");
            }
            _ => panic!("Expected Cancelled"),
        }
    }

    // =========================================================================
    // Round-trip Serialization Tests
    // =========================================================================

    #[test]
    fn server_message_turn_start_roundtrip() {
        let original = ServerMessage::TurnStart {
            request_id: "req-roundtrip".to_string(),
            agent_id: "agent-roundtrip".to_string(),
        };

        let json = serde_json::to_string(&original).unwrap();
        let parsed: ServerMessage = serde_json::from_str(&json).unwrap();

        match (original, parsed) {
            (
                ServerMessage::TurnStart { request_id: r1, agent_id: a1 },
                ServerMessage::TurnStart { request_id: r2, agent_id: a2 },
            ) => {
                assert_eq!(r1, r2);
                assert_eq!(a1, a2);
            }
            _ => panic!("Roundtrip failed"),
        }
    }

    #[test]
    fn server_message_tool_start_roundtrip_with_nested_json() {
        let original = ServerMessage::ToolStart {
            request_id: "req-1".to_string(),
            agent_id: "agent-1".to_string(),
            tool_id: "tool-1".to_string(),
            tool_name: "complex_tool".to_string(),
            args: serde_json::json!({
                "nested": {
                    "deeply": {
                        "value": [1, 2, 3]
                    }
                },
                "array": ["a", "b", "c"],
                "null_field": null,
                "bool": true
            }),
        };

        let json = serde_json::to_string(&original).unwrap();
        let parsed: ServerMessage = serde_json::from_str(&json).unwrap();

        match parsed {
            ServerMessage::ToolStart { args, .. } => {
                assert_eq!(args["nested"]["deeply"]["value"][1], 2);
                assert_eq!(args["array"][2], "c");
                assert!(args["null_field"].is_null());
                assert_eq!(args["bool"], true);
            }
            _ => panic!("Expected ToolStart"),
        }
    }

    // =========================================================================
    // Edge Cases and Error Handling
    // =========================================================================

    #[test]
    fn server_message_unknown_type_fails() {
        let json = r#"{"type":"unknown_message_type","data":"test"}"#;
        let result: Result<ServerMessage, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn server_message_missing_required_field_fails() {
        // Missing agent_id for TurnStart
        let json = r#"{"type":"turn_start","request_id":"req-1"}"#;
        let result: Result<ServerMessage, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn server_message_wrong_type_for_field_fails() {
        // step should be u32, not string
        let json = r#"{"type":"step_start","request_id":"req-1","agent_id":"agent-1","step":"not_a_number"}"#;
        let result: Result<ServerMessage, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn server_message_text_delta_with_unicode() {
        let json = r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":"こんにちは 🌍 مرحبا"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::TextDelta { text, .. } => {
                assert!(text.contains("こんにちは"));
                assert!(text.contains("🌍"));
                assert!(text.contains("مرحبا"));
            }
            _ => panic!("Expected TextDelta"),
        }
    }

    #[test]
    fn server_message_large_token_counts() {
        let json = r#"{
            "type": "turn_complete",
            "request_id": "req-1",
            "agent_id": "agent-1",
            "steps": 100,
            "input_tokens": 4294967295,
            "output_tokens": 4294967295
        }"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::TurnComplete { input_tokens, output_tokens, .. } => {
                assert_eq!(input_tokens, u32::MAX);
                assert_eq!(output_tokens, u32::MAX);
            }
            _ => panic!("Expected TurnComplete"),
        }
    }

    // =========================================================================
    // TurnCompleteInfo Tests
    // =========================================================================

    #[test]
    fn turn_complete_info_default() {
        let info = TurnCompleteInfo::default();
        assert_eq!(info.steps, 0);
        assert_eq!(info.input_tokens, 0);
        assert_eq!(info.output_tokens, 0);
    }

    #[test]
    fn turn_complete_info_equality() {
        let a = TurnCompleteInfo {
            steps: 5,
            input_tokens: 1000,
            output_tokens: 500,
        };
        let b = TurnCompleteInfo {
            steps: 5,
            input_tokens: 1000,
            output_tokens: 500,
        };
        let c = TurnCompleteInfo {
            steps: 5,
            input_tokens: 1000,
            output_tokens: 501,
        };

        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // =========================================================================
    // ChatMessage Tests
    // =========================================================================

    #[test]
    fn chat_message_user_creation() {
        let msg = ChatMessage::user("Hello");
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, "Hello");
        assert!(msg.is_user());
    }

    #[test]
    fn chat_message_assistant_creation() {
        let msg = ChatMessage::assistant("Hi there!");
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "Hi there!");
        assert!(!msg.is_user());
    }

    #[test]
    fn chat_message_serialization_roundtrip() {
        let original = ChatMessage::user("Test message with \"quotes\" and\nnewlines");
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ChatMessage = serde_json::from_str(&json).unwrap();

        assert_eq!(original.role, parsed.role);
        assert_eq!(original.content, parsed.content);
    }

    // =========================================================================
    // AgentState Tests
    // =========================================================================

    #[test]
    fn agent_state_display_strings() {
        assert_eq!(AgentState::Running.as_str(), "Running");
        assert_eq!(AgentState::Idle.as_str(), "Idle");
        assert_eq!(AgentState::Provisioning.as_str(), "Provisioning");
        assert_eq!(AgentState::Hibernating.as_str(), "Hibernating");
        assert_eq!(AgentState::Stopping.as_str(), "Stopping");
        assert_eq!(AgentState::Stopped.as_str(), "Stopped");
        assert_eq!(AgentState::Error.as_str(), "Error");
    }

    #[test]
    fn agent_state_serialization() {
        let json = serde_json::to_string(&AgentState::Running).unwrap();
        assert_eq!(json, "\"running\"");

        let parsed: AgentState = serde_json::from_str("\"hibernating\"").unwrap();
        assert_eq!(parsed, AgentState::Hibernating);
    }

    // =========================================================================
    // Message Flow Simulation Tests
    // =========================================================================

    #[test]
    fn simulate_simple_turn_message_flow() {
        // Simulate a typical turn: turn_start -> text_delta(s) -> turn_complete
        let messages = vec![
            r#"{"type":"turn_start","request_id":"req-1","agent_id":"agent-1"}"#,
            r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":"Hello"}"#,
            r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":", "}"#,
            r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":"world!"}"#,
            r#"{"type":"turn_complete","request_id":"req-1","agent_id":"agent-1","steps":1,"input_tokens":10,"output_tokens":3}"#,
        ];

        let mut text_buffer = String::new();
        let mut turn_started = false;
        let mut turn_completed = false;

        for msg_json in messages {
            let msg: ServerMessage = serde_json::from_str(msg_json).unwrap();
            match msg {
                ServerMessage::TurnStart { .. } => turn_started = true,
                ServerMessage::TextDelta { text, .. } => text_buffer.push_str(&text),
                ServerMessage::TurnComplete { steps, .. } => {
                    turn_completed = true;
                    assert_eq!(steps, 1);
                }
                _ => {}
            }
        }

        assert!(turn_started);
        assert!(turn_completed);
        assert_eq!(text_buffer, "Hello, world!");
    }

    #[test]
    fn simulate_multi_step_turn_with_tools() {
        // Simulate: turn_start -> step_start -> tool_start -> tool_complete -> text_delta -> step_start -> text_delta -> turn_complete
        let messages = vec![
            r#"{"type":"turn_start","request_id":"req-1","agent_id":"agent-1"}"#,
            r#"{"type":"step_start","request_id":"req-1","agent_id":"agent-1","step":1}"#,
            r#"{"type":"tool_start","request_id":"req-1","agent_id":"agent-1","tool_id":"t1","tool_name":"fs_read","args":{"path":"README.md"}}"#,
            r##"{"type":"tool_complete","request_id":"req-1","agent_id":"agent-1","tool_id":"t1","result":"# Project\nThis is a test.","is_error":false}"##,
            r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":"I read the file. "}"#,
            r#"{"type":"step_start","request_id":"req-1","agent_id":"agent-1","step":2}"#,
            r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":"It contains project docs."}"#,
            r#"{"type":"turn_complete","request_id":"req-1","agent_id":"agent-1","steps":2,"input_tokens":150,"output_tokens":20}"#,
        ];

        let mut steps_seen = Vec::new();
        let mut tools_executed = Vec::new();
        let mut final_steps = 0;

        for msg_json in messages {
            let msg: ServerMessage = serde_json::from_str(msg_json).unwrap();
            match msg {
                ServerMessage::StepStart { step, .. } => steps_seen.push(step),
                ServerMessage::ToolStart { tool_name, .. } => tools_executed.push(tool_name),
                ServerMessage::TurnComplete { steps, .. } => final_steps = steps,
                _ => {}
            }
        }

        assert_eq!(steps_seen, vec![1, 2]);
        assert_eq!(tools_executed, vec!["fs_read"]);
        assert_eq!(final_steps, 2);
    }

    #[test]
    fn simulate_cancellation_flow() {
        // Client sends cancel, server responds with cancelled
        let client_msg = ClientMessage::Cancel {
            request_id: "req-to-cancel".to_string(),
        };
        let json = serde_json::to_string(&client_msg).unwrap();
        
        // Verify the cancel message is correctly formatted
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "cancel");
        assert_eq!(parsed["request_id"], "req-to-cancel");

        // Server would respond with:
        let server_response = r#"{"type":"cancelled","request_id":"req-to-cancel","agent_id":"agent-1"}"#;
        let msg: ServerMessage = serde_json::from_str(server_response).unwrap();
        
        match msg {
            ServerMessage::Cancelled { request_id, .. } => {
                assert_eq!(request_id, "req-to-cancel");
            }
            _ => panic!("Expected Cancelled"),
        }
    }

    #[test]
    fn simulate_error_during_turn() {
        // Turn starts but encounters an error
        let messages = vec![
            r#"{"type":"turn_start","request_id":"req-1","agent_id":"agent-1"}"#,
            r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":"Let me try..."}"#,
            r#"{"type":"error","request_id":"req-1","agent_id":"agent-1","error":"API rate limit exceeded","code":"rate_limit"}"#,
        ];

        let mut turn_started = false;
        let mut error_received = false;
        let mut error_code = None;

        for msg_json in messages {
            let msg: ServerMessage = serde_json::from_str(msg_json).unwrap();
            match msg {
                ServerMessage::TurnStart { .. } => turn_started = true,
                ServerMessage::Error { code, .. } => {
                    error_received = true;
                    error_code = code;
                }
                _ => {}
            }
        }

        assert!(turn_started);
        assert!(error_received);
        assert_eq!(error_code, Some("rate_limit".to_string()));
    }
}
