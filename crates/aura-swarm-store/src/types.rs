//! Domain types stored in the database.
//!
//! These types represent the persisted state of agents, sessions, and users.

use aura_swarm_core::{AgentId, SessionId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// An agent record stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// Unique identifier for the agent.
    pub agent_id: AgentId,
    /// Owner user ID.
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
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            cpu_millicores: 500,
            memory_mb: 512,
            runtime_version: "latest".to_string(),
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

impl SessionStatus {
    /// Convert the status to its numeric representation.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// A user record stored in the database (synced from Zero-ID).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// Unique identifier for the user.
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
