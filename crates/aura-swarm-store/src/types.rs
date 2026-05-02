//! Domain types stored in the database.
//!
//! These types represent the persisted state of agents, sessions, and users.

use aura_swarm_core::{AgentId, SessionId, UserId};
pub use aura_swarm_protocol::ExternalToolDef;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// An agent record stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// Unique identifier for the agent.
    pub agent_id: AgentId,
    /// Owner user ID (from zOS).
    pub user_id: UserId,
    /// Human-readable name.
    pub name: String,
    /// Current lifecycle state.
    pub status: AgentState,
    /// Resource specification.
    pub spec: AgentSpec,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last modification timestamp.
    pub updated_at: DateTime<Utc>,
    /// Last heartbeat from the agent runtime.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Error message when agent is in Error state (e.g., provisioning failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Resource specification for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    /// CPU allocation in millicores.
    pub cpu_millicores: u32,
    /// Memory allocation in megabytes.
    pub memory_mb: u32,
    /// Aura runtime version.
    pub runtime_version: String,
    /// Isolation level for the agent runtime.
    /// If not specified, uses the scheduler's default.
    #[serde(default)]
    pub isolation: Option<IsolationLevel>,
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            cpu_millicores: 500,
            memory_mb: 512,
            runtime_version: "latest".to_string(),
            isolation: None,
        }
    }
}

/// Isolation level for agent execution.
///
/// Determines whether the agent runs in a lightweight container
/// or a more secure microVM with its own kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    /// Run in a standard container (shared kernel).
    /// Faster startup, lower overhead, less isolation.
    /// Use for trusted workloads or development.
    Container,
    /// Run in a Firecracker microVM (dedicated kernel).
    /// Stronger isolation, slightly higher overhead.
    /// Default for production agent workloads.
    #[default]
    MicroVM,
}

impl IsolationLevel {
    /// Get the Kubernetes `RuntimeClass` name for this isolation level.
    ///
    /// Returns `None` for container isolation (uses default runtime),
    /// or `Some("kata-fc")` for microVM isolation.
    #[must_use]
    pub const fn runtime_class(&self) -> Option<&'static str> {
        match self {
            Self::Container => None, // Use default container runtime
            Self::MicroVM => Some("kata-fc"),
        }
    }
}

/// Lifecycle states for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum AgentState {
    /// Pod is being created, Aura initializing.
    Provisioning = 1,
    /// Agent is active and accepting sessions.
    Running = 2,
    /// No active sessions, still running.
    Idle = 3,
    /// State saved, pod terminated, instant wake.
    Hibernating = 4,
    /// Graceful shutdown in progress.
    Stopping = 5,
    /// Pod terminated, state preserved.
    Stopped = 6,
    /// Health check failed or crash.
    Error = 7,
}

impl AgentState {
    /// Convert the state to its numeric representation.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Try to convert a numeric value to an `AgentState`.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Provisioning),
            2 => Some(Self::Running),
            3 => Some(Self::Idle),
            4 => Some(Self::Hibernating),
            5 => Some(Self::Stopping),
            6 => Some(Self::Stopped),
            7 => Some(Self::Error),
            _ => None,
        }
    }
}

/// A session record stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique identifier for the session.
    pub session_id: SessionId,
    /// Agent this session is connected to.
    pub agent_id: AgentId,
    /// User who owns this session.
    pub user_id: UserId,
    /// Current session status.
    pub status: SessionStatus,
    /// Per-session configuration for the harness runtime.
    #[serde(default)]
    pub config: SessionConfig,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// When the session was closed (if closed).
    pub closed_at: Option<DateTime<Utc>>,
}

/// Status of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SessionStatus {
    /// Session is active and can receive messages.
    Active = 1,
    /// Session has been closed.
    Closed = 2,
}

/// Per-session configuration for the harness runtime.
///
/// Stored alongside the session record so the gateway can construct
/// a `session_init` message when proxying the WebSocket connection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionConfig {
    /// System prompt override for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Model identifier override (e.g., "claude-opus-4-6-20250514").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Maximum tokens per model response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Maximum agentic steps per turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Workspace configuration for file operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceConfig>,
    /// External tool definitions registered for this session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_tools: Vec<ExternalToolDef>,
}

/// Workspace configuration for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Git repository URL to clone into the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_repo_url: Option<String>,
    /// Git branch to check out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
}

impl SessionStatus {
    /// Convert the status to its numeric representation.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// A user record stored in the database (synced from zOS).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// Unique identifier for the user (from zOS).
    pub user_id: UserId,
    /// User's email address.
    pub email: String,
    /// Whether the email has been verified.
    pub email_verified: bool,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last login timestamp.
    pub last_login_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_state_as_u8_roundtrip() {
        let variants = [
            AgentState::Provisioning,
            AgentState::Running,
            AgentState::Idle,
            AgentState::Hibernating,
            AgentState::Stopping,
            AgentState::Stopped,
            AgentState::Error,
        ];
        for state in variants {
            let roundtripped = AgentState::from_u8(state.as_u8());
            assert_eq!(roundtripped, Some(state));
        }
    }

    #[test]
    fn agent_state_from_invalid_u8() {
        assert_eq!(AgentState::from_u8(0), None);
        assert_eq!(AgentState::from_u8(255), None);
    }

    #[test]
    fn session_status_serde_roundtrip() {
        for status in [SessionStatus::Active, SessionStatus::Closed] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: SessionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn isolation_level_runtime_class() {
        assert_eq!(IsolationLevel::Container.runtime_class(), None);
        assert_eq!(IsolationLevel::MicroVM.runtime_class(), Some("kata-fc"));
    }

    #[test]
    fn agent_spec_default() {
        let spec = AgentSpec::default();
        assert_eq!(spec.cpu_millicores, 500);
        assert_eq!(spec.memory_mb, 512);
        assert_eq!(spec.runtime_version, "latest");
        assert!(spec.isolation.is_none());
    }

    #[test]
    fn agent_serde_json_roundtrip() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent_id = AgentId::generate();
        let now = chrono::Utc::now();

        let agent = Agent {
            agent_id,
            user_id,
            name: "my-agent".to_string(),
            status: AgentState::Running,
            spec: AgentSpec {
                cpu_millicores: 1000,
                memory_mb: 2048,
                runtime_version: "v2".to_string(),
                isolation: Some(IsolationLevel::MicroVM),
            },
            created_at: now,
            updated_at: now,
            last_heartbeat_at: Some(now),
            error_message: Some("test error".to_string()),
        };

        let json = serde_json::to_string(&agent).unwrap();
        let parsed: Agent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.agent_id, agent.agent_id);
        assert_eq!(parsed.user_id, agent.user_id);
        assert_eq!(parsed.name, "my-agent");
        assert_eq!(parsed.status, AgentState::Running);
        assert_eq!(parsed.spec.cpu_millicores, 1000);
        assert_eq!(parsed.spec.memory_mb, 2048);
        assert_eq!(parsed.spec.runtime_version, "v2");
        assert_eq!(parsed.error_message, Some("test error".to_string()));
    }
}
