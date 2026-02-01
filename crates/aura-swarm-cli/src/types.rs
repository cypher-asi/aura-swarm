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
//
// Endpoint: WS /stream

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
    /// Tool execution started.
    ToolStart {
        /// Request ID.
        request_id: String,
        /// Agent ID.
        agent_id: String,
        /// Unique tool invocation ID.
        tool_id: String,
        /// Name of the tool being executed.
        tool_name: String,
        /// Tool arguments (optional).
        #[serde(default)]
        args: serde_json::Value,
    },
    /// Tool execution completed.
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
        /// Request ID.
        request_id: String,
        /// Agent ID (if available).
        #[serde(default)]
        agent_id: Option<String>,
        /// Error message.
        error: String,
        /// Optional error code.
        #[serde(default)]
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
// Tests (Aura Runtime Protocol)
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
            workspace: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["type"], "prompt");
        assert_eq!(parsed["request_id"], "req-123");
        assert_eq!(parsed["prompt"], "Hello, agent!");
        assert_eq!(parsed["agent_id"], "agent-456");
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
        let json = r#"{"type":"step_start","request_id":"req-1","agent_id":"agent-1","step":2}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::StepStart { step, .. } => {
                assert_eq!(step, 2);
            }
            _ => panic!("Expected StepStart"),
        }
    }

    #[test]
    fn server_message_text_delta_deserializes() {
        let json = r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":"Hello, "}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::TextDelta { text, .. } => {
                assert_eq!(text, "Hello, ");
            }
            _ => panic!("Expected TextDelta"),
        }
    }

    #[test]
    fn server_message_tool_start_deserializes() {
        let json = r#"{"type":"tool_start","request_id":"req-1","agent_id":"agent-1","tool_id":"t1","tool_name":"read_file","args":{"path":"/src/main.rs"}}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::ToolStart { tool_name, args, .. } => {
                assert_eq!(tool_name, "read_file");
                assert_eq!(args["path"], "/src/main.rs");
            }
            _ => panic!("Expected ToolStart"),
        }
    }

    #[test]
    fn server_message_tool_complete_deserializes() {
        let json = r#"{"type":"tool_complete","request_id":"req-1","agent_id":"agent-1","tool_id":"t1","result":"file contents","is_error":false}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::ToolComplete { result, is_error, .. } => {
                assert_eq!(result, "file contents");
                assert!(!is_error);
            }
            _ => panic!("Expected ToolComplete"),
        }
    }

    #[test]
    fn server_message_turn_complete_deserializes() {
        let json = r#"{"type":"turn_complete","request_id":"req-1","agent_id":"agent-1","steps":3,"input_tokens":1500,"output_tokens":800}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::TurnComplete { steps, input_tokens, output_tokens, .. } => {
                assert_eq!(steps, 3);
                assert_eq!(input_tokens, 1500);
                assert_eq!(output_tokens, 800);
            }
            _ => panic!("Expected TurnComplete"),
        }
    }

    #[test]
    fn server_message_error_deserializes() {
        let json = r#"{"type":"error","request_id":"req-1","agent_id":"agent-1","error":"Something went wrong","code":"TURN_ERROR"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::Error { error, code, .. } => {
                assert_eq!(error, "Something went wrong");
                assert_eq!(code, Some("TURN_ERROR".to_string()));
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn server_message_cancelled_deserializes() {
        let json = r#"{"type":"cancelled","request_id":"req-1","agent_id":"agent-1"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();

        match msg {
            ServerMessage::Cancelled { request_id, .. } => {
                assert_eq!(request_id, "req-1");
            }
            _ => panic!("Expected Cancelled"),
        }
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
    // TurnCompleteInfo Tests
    // =========================================================================

    #[test]
    fn turn_complete_info_default() {
        let info = TurnCompleteInfo::default();
        assert_eq!(info.steps, 0);
        assert_eq!(info.input_tokens, 0);
        assert_eq!(info.output_tokens, 0);
    }

    // =========================================================================
    // Message Flow Simulation Tests
    // =========================================================================

    #[test]
    fn simulate_simple_message_flow() {
        let messages = vec![
            r#"{"type":"turn_start","request_id":"req-1","agent_id":"agent-1"}"#,
            r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":"Hello"}"#,
            r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":", world!"}"#,
            r#"{"type":"turn_complete","request_id":"req-1","agent_id":"agent-1","steps":1,"input_tokens":10,"output_tokens":3}"#,
        ];

        let mut text_buffer = String::new();
        let mut started = false;
        let mut ended = false;

        for msg_json in messages {
            let msg: ServerMessage = serde_json::from_str(msg_json).unwrap();
            match msg {
                ServerMessage::TurnStart { .. } => started = true,
                ServerMessage::TextDelta { text, .. } => text_buffer.push_str(&text),
                ServerMessage::TurnComplete { .. } => ended = true,
                _ => {}
            }
        }

        assert!(started);
        assert!(ended);
        assert_eq!(text_buffer, "Hello, world!");
    }

    #[test]
    fn simulate_tool_use_flow() {
        let messages = vec![
            r#"{"type":"turn_start","request_id":"req-1","agent_id":"agent-1"}"#,
            r#"{"type":"tool_start","request_id":"req-1","agent_id":"agent-1","tool_id":"t1","tool_name":"read_file","args":{"path":"README.md"}}"#,
            r##"{"type":"tool_complete","request_id":"req-1","agent_id":"agent-1","tool_id":"t1","result":"# Project","is_error":false}"##,
            r#"{"type":"text_delta","request_id":"req-1","agent_id":"agent-1","text":"I read the file."}"#,
            r#"{"type":"turn_complete","request_id":"req-1","agent_id":"agent-1","steps":1,"input_tokens":100,"output_tokens":20}"#,
        ];

        let mut tools_used = Vec::new();
        let mut tool_succeeded = false;

        for msg_json in messages {
            let msg: ServerMessage = serde_json::from_str(msg_json).unwrap();
            match msg {
                ServerMessage::ToolStart { tool_name, .. } => tools_used.push(tool_name),
                ServerMessage::ToolComplete { is_error, .. } => tool_succeeded = !is_error,
                _ => {}
            }
        }

        assert_eq!(tools_used, vec!["read_file"]);
        assert!(tool_succeeded);
    }
}
