//! API response types for the gateway client.
//!
//! These types mirror the responses from the aura-swarm-gateway API.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// Re-export protocol types used by ws.rs and app.rs
pub use aura_swarm_protocol::ToolInfo;

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
    /// Optional caller-supplied agent ID (e.g. from aura-network).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
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
// Turn Metadata
// =============================================================================

/// Response metadata from turn completion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnCompleteInfo {
    /// Total number of steps.
    pub steps: u32,
    /// Input tokens used.
    pub input_tokens: u64,
    /// Output tokens used.
    pub output_tokens: u64,
    /// Model used for the turn.
    pub model: Option<String>,
    /// Stop reason for the turn.
    pub stop_reason: Option<String>,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn turn_complete_info_default() {
        let info = TurnCompleteInfo::default();
        assert_eq!(info.steps, 0);
        assert_eq!(info.input_tokens, 0u64);
        assert_eq!(info.output_tokens, 0u64);
        assert_eq!(info.model, None);
        assert_eq!(info.stop_reason, None);
    }
}
