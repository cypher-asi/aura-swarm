//! Remote agent types for the SwarmClient.
//!
//! These types use `remote_agent` naming to match the aura-os integration
//! contract. They mirror the gateway's agent/session types but are
//! purpose-built for external consumers (e.g. aura-os-link).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// =============================================================================
// Remote Agent Types
// =============================================================================

/// Lifecycle state of a remote agent's backing VM/pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAgentState {
    /// VM/pod is being created and runtime is initializing.
    Provisioning,
    /// Agent is active and accepting sessions.
    Running,
    /// No active sessions, still running.
    Idle,
    /// State saved, VM terminated, instant wake available.
    Hibernating,
    /// Graceful shutdown in progress.
    Stopping,
    /// VM terminated, state preserved on disk.
    Stopped,
    /// Health check failed or crash detected.
    Error,
}

impl RemoteAgentState {
    /// Whether the agent can accept new sessions in this state.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Running | Self::Idle)
    }

    /// Whether the agent can be woken from this state.
    #[must_use]
    pub const fn is_wakeable(&self) -> bool {
        matches!(self, Self::Hibernating)
    }

    /// Whether the agent is in a terminal/inactive state.
    #[must_use]
    pub const fn is_inactive(&self) -> bool {
        matches!(self, Self::Stopped | Self::Error)
    }
}

/// Resource specification for a remote agent's VM/pod.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAgentSpec {
    /// CPU allocation in millicores.
    #[serde(default = "default_cpu")]
    pub cpu_millicores: u32,
    /// Memory allocation in megabytes.
    #[serde(default = "default_memory")]
    pub memory_mb: u32,
    /// Aura runtime version.
    #[serde(default = "default_runtime_version")]
    pub runtime_version: String,
    /// Isolation level for the agent runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<RemoteIsolationLevel>,
}

fn default_cpu() -> u32 {
    500
}
fn default_memory() -> u32 {
    512
}
fn default_runtime_version() -> String {
    "latest".to_string()
}

impl Default for RemoteAgentSpec {
    fn default() -> Self {
        Self {
            cpu_millicores: default_cpu(),
            memory_mb: default_memory(),
            runtime_version: default_runtime_version(),
            isolation: None,
        }
    }
}

/// Isolation level for the remote agent's execution environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RemoteIsolationLevel {
    /// Standard container (shared kernel). Faster startup, lower overhead.
    Container,
    /// Firecracker microVM (dedicated kernel). Stronger isolation.
    #[default]
    MicroVm,
}

/// A remote agent record as returned by the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAgent {
    /// Unique identifier for the remote agent.
    pub agent_id: String,
    /// Human-readable name.
    pub name: String,
    /// Current lifecycle state.
    pub status: RemoteAgentState,
    /// Resource specification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<RemoteAgentSpec>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Last heartbeat from the agent runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Error message when agent is in Error state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Response for listing remote agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRemoteAgentsResponse {
    /// List of remote agents.
    pub agents: Vec<RemoteAgent>,
}

/// Request to create a remote agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRemoteAgentRequest {
    /// Human-readable name for the agent.
    pub name: String,
    /// Optional resource specification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<RemoteAgentSpec>,
    /// Caller-supplied agent ID for identity parity with the local agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

/// Response from the remote agent state endpoint.
///
/// Contains only the VM/pod state — no resource metrics or logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAgentStateResponse {
    /// Current lifecycle state of the remote agent.
    pub state: RemoteAgentState,
    /// Uptime in seconds (0 if not running).
    pub uptime_seconds: u64,
    /// Number of active sessions.
    pub active_sessions: u32,
    /// Last heartbeat from the agent runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Error message if the agent is in an error state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

// =============================================================================
// Session Types
// =============================================================================

/// Per-session configuration sent when creating a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionConfig {
    /// System prompt override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Model identifier override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Maximum tokens per model response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Maximum agentic steps per turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
}

/// Request body for creating a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    /// Per-session configuration for the harness runtime.
    #[serde(default)]
    pub config: SessionConfig,
}

/// Response for a created session.
///
/// With the migration to the `POST /v1/run` contract there is no per-session
/// WebSocket. A created session points the caller at the agent's
/// run-creation endpoint (`run_url`); the caller starts a run there and then
/// attaches to that run's event stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    /// Session ID.
    pub session_id: String,
    /// Agent ID the session belongs to.
    pub agent_id: String,
    /// Relative run-creation URL (`/v1/agents/:agent_id/run`).
    pub run_url: String,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Session status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Session is active and can receive messages.
    Active,
    /// Session has been closed.
    Closed,
}

/// A session record as returned by the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResponse {
    /// Session ID.
    pub session_id: String,
    /// Agent ID.
    pub agent_id: String,
    /// Current status.
    pub status: SessionStatus,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// When the session was closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
}

/// Response for listing sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListSessionsResponse {
    /// List of sessions.
    pub sessions: Vec<SessionResponse>,
}

// =============================================================================
// Error Response
// =============================================================================

/// Inner error body returned by the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorBody {
    /// Error code (e.g. "unauthorized", "internal_error").
    #[serde(default)]
    pub code: String,
    /// Human-readable error message.
    #[serde(default)]
    pub message: String,
}

/// Structured error response from the gateway API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorResponse {
    /// Structured error body.
    pub error: ApiErrorBody,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_agent_state_serde_roundtrip() {
        let variants = [
            (RemoteAgentState::Provisioning, "\"provisioning\""),
            (RemoteAgentState::Running, "\"running\""),
            (RemoteAgentState::Idle, "\"idle\""),
            (RemoteAgentState::Hibernating, "\"hibernating\""),
            (RemoteAgentState::Stopping, "\"stopping\""),
            (RemoteAgentState::Stopped, "\"stopped\""),
            (RemoteAgentState::Error, "\"error\""),
        ];

        for (state, expected_json) in variants {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, expected_json, "serialize {state:?}");
            let parsed: RemoteAgentState = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, state, "roundtrip {state:?}");
        }
    }

    #[test]
    fn remote_agent_state_readiness() {
        assert!(RemoteAgentState::Running.is_ready());
        assert!(RemoteAgentState::Idle.is_ready());
        assert!(!RemoteAgentState::Provisioning.is_ready());
        assert!(!RemoteAgentState::Stopped.is_ready());
        assert!(!RemoteAgentState::Error.is_ready());
    }

    #[test]
    fn remote_agent_state_wakeable() {
        assert!(RemoteAgentState::Hibernating.is_wakeable());
        assert!(!RemoteAgentState::Running.is_wakeable());
        assert!(!RemoteAgentState::Stopped.is_wakeable());
    }

    #[test]
    fn remote_agent_state_inactive() {
        assert!(RemoteAgentState::Stopped.is_inactive());
        assert!(RemoteAgentState::Error.is_inactive());
        assert!(!RemoteAgentState::Running.is_inactive());
        assert!(!RemoteAgentState::Hibernating.is_inactive());
    }

    #[test]
    fn remote_agent_spec_default() {
        let spec = RemoteAgentSpec::default();
        assert_eq!(spec.cpu_millicores, 500);
        assert_eq!(spec.memory_mb, 512);
        assert_eq!(spec.runtime_version, "latest");
        assert!(spec.isolation.is_none());
    }

    #[test]
    fn remote_agent_spec_serde_roundtrip() {
        let spec = RemoteAgentSpec {
            cpu_millicores: 1000,
            memory_mb: 2048,
            runtime_version: "v2.0".to_string(),
            isolation: Some(RemoteIsolationLevel::MicroVm),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: RemoteAgentSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.cpu_millicores, 1000);
        assert_eq!(parsed.memory_mb, 2048);
        assert_eq!(parsed.runtime_version, "v2.0");
        assert_eq!(parsed.isolation, Some(RemoteIsolationLevel::MicroVm));
    }

    #[test]
    fn remote_isolation_level_serde() {
        let json = serde_json::to_string(&RemoteIsolationLevel::Container).unwrap();
        assert_eq!(json, "\"container\"");
        let json = serde_json::to_string(&RemoteIsolationLevel::MicroVm).unwrap();
        assert_eq!(json, "\"micro_vm\"");

        let parsed: RemoteIsolationLevel = serde_json::from_str("\"container\"").unwrap();
        assert_eq!(parsed, RemoteIsolationLevel::Container);
        let parsed: RemoteIsolationLevel = serde_json::from_str("\"micro_vm\"").unwrap();
        assert_eq!(parsed, RemoteIsolationLevel::MicroVm);
    }

    #[test]
    fn remote_isolation_level_default() {
        assert_eq!(
            RemoteIsolationLevel::default(),
            RemoteIsolationLevel::MicroVm
        );
    }

    #[test]
    fn remote_agent_full_serde_roundtrip() {
        let agent = RemoteAgent {
            agent_id: "abc123".to_string(),
            name: "test-agent".to_string(),
            status: RemoteAgentState::Running,
            spec: Some(RemoteAgentSpec::default()),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_heartbeat_at: Some(chrono::Utc::now()),
            error_message: None,
        };
        let json = serde_json::to_string(&agent).unwrap();
        let parsed: RemoteAgent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_id, "abc123");
        assert_eq!(parsed.name, "test-agent");
        assert_eq!(parsed.status, RemoteAgentState::Running);
        assert!(parsed.spec.is_some());
    }

    #[test]
    fn remote_agent_deserializes_without_optional_fields() {
        let json = r#"{
            "agent_id": "abc",
            "name": "test",
            "status": "stopped",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }"#;
        let agent: RemoteAgent = serde_json::from_str(json).unwrap();
        assert_eq!(agent.agent_id, "abc");
        assert_eq!(agent.status, RemoteAgentState::Stopped);
        assert!(agent.spec.is_none());
        assert!(agent.last_heartbeat_at.is_none());
        assert!(agent.error_message.is_none());
    }

    #[test]
    fn remote_agent_state_response_serde() {
        let resp = RemoteAgentStateResponse {
            state: RemoteAgentState::Running,
            uptime_seconds: 3600,
            active_sessions: 2,
            last_heartbeat_at: Some(chrono::Utc::now()),
            error_message: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: RemoteAgentStateResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.state, RemoteAgentState::Running);
        assert_eq!(parsed.uptime_seconds, 3600);
        assert_eq!(parsed.active_sessions, 2);
    }

    #[test]
    fn create_remote_agent_request_serde() {
        let req = CreateRemoteAgentRequest {
            name: "my-agent".to_string(),
            spec: None,
            agent_id: Some("caller-id-123".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"agent_id\":\"caller-id-123\""));

        let parsed: CreateRemoteAgentRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "my-agent");
        assert_eq!(parsed.agent_id, Some("caller-id-123".to_string()));
    }

    #[test]
    fn create_remote_agent_request_minimal() {
        let json = r#"{"name": "minimal"}"#;
        let req: CreateRemoteAgentRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.name, "minimal");
        assert!(req.spec.is_none());
        assert!(req.agent_id.is_none());
    }

    #[test]
    fn create_session_request_serde() {
        let req = CreateSessionRequest {
            config: SessionConfig {
                system_prompt: Some("You are helpful.".to_string()),
                model: Some("claude-opus-4-6-20250514".to_string()),
                max_tokens: Some(4096),
                max_turns: Some(10),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: CreateSessionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.config.system_prompt,
            Some("You are helpful.".to_string())
        );
        assert_eq!(parsed.config.max_tokens, Some(4096));
    }

    #[test]
    fn session_config_default_is_empty() {
        let config = SessionConfig::default();
        assert!(config.system_prompt.is_none());
        assert!(config.model.is_none());
        assert!(config.max_tokens.is_none());
        assert!(config.max_turns.is_none());
    }

    #[test]
    fn session_status_serde() {
        let json = serde_json::to_string(&SessionStatus::Active).unwrap();
        assert_eq!(json, "\"active\"");
        let json = serde_json::to_string(&SessionStatus::Closed).unwrap();
        assert_eq!(json, "\"closed\"");
    }

    #[test]
    fn create_session_response_serde() {
        let json = r#"{
            "session_id": "sess-1",
            "agent_id": "agent-1",
            "run_url": "/v1/agents/agent-1/run",
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let resp: CreateSessionResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.session_id, "sess-1");
        assert_eq!(resp.run_url, "/v1/agents/agent-1/run");
    }

    #[test]
    fn api_error_response_serde() {
        let json = r#"{"error": {"code": "not_found", "message": "Agent not found"}}"#;
        let resp: ApiErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error.code, "not_found");
        assert_eq!(resp.error.message, "Agent not found");
    }

    #[test]
    fn list_remote_agents_response_serde() {
        let json = r#"{"agents": []}"#;
        let resp: ListRemoteAgentsResponse = serde_json::from_str(json).unwrap();
        assert!(resp.agents.is_empty());
    }

    #[test]
    fn list_sessions_response_serde() {
        let json = r#"{"sessions": []}"#;
        let resp: ListSessionsResponse = serde_json::from_str(json).unwrap();
        assert!(resp.sessions.is_empty());
    }
}
