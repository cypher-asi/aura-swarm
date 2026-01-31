//! Request and response types for control plane operations.
//!
//! These types define the API contracts for agent and session management.

use aura_swarm_store::AgentSpec;
use serde::{Deserialize, Serialize};

/// Request to create a new agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentRequest {
    /// Human-readable name for the agent.
    pub name: String,
    /// Optional resource specification. Uses defaults if not provided.
    #[serde(default)]
    pub spec: Option<AgentSpec>,
}

impl CreateAgentRequest {
    /// Create a new request with the given name and default spec.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            spec: None,
        }
    }

    /// Create a new request with a custom spec.
    #[must_use]
    pub fn with_spec(name: impl Into<String>, spec: AgentSpec) -> Self {
        Self {
            name: name.into(),
            spec: Some(spec),
        }
    }
}

/// Options for retrieving agent logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogOptions {
    /// Maximum number of log lines to return.
    #[serde(default = "LogOptions::default_lines")]
    pub lines: u32,
    /// If true, stream logs in real-time.
    #[serde(default)]
    pub follow: bool,
    /// Filter logs since this timestamp (RFC3339).
    #[serde(default)]
    pub since: Option<String>,
    /// Filter logs until this timestamp (RFC3339).
    #[serde(default)]
    pub until: Option<String>,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            lines: Self::default_lines(),
            follow: false,
            since: None,
            until: None,
        }
    }
}

impl LogOptions {
    const fn default_lines() -> u32 {
        100
    }

    /// Create options to get the last N lines.
    #[must_use]
    pub const fn tail(lines: u32) -> Self {
        Self {
            lines,
            follow: false,
            since: None,
            until: None,
        }
    }

    /// Create options to follow logs in real-time.
    #[must_use]
    pub const fn following() -> Self {
        Self {
            lines: 100,
            follow: true,
            since: None,
            until: None,
        }
    }
}

/// Agent status information returned from status queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
    /// Whether the agent is healthy.
    pub healthy: bool,
    /// Current CPU usage (0.0 - 1.0).
    pub cpu_usage: f64,
    /// Current memory usage in bytes.
    pub memory_bytes: u64,
    /// Number of active sessions.
    pub active_sessions: u32,
    /// Uptime in seconds.
    pub uptime_seconds: u64,
}

/// Configuration for the control plane service.
#[derive(Debug, Clone)]
pub struct ControlConfig {
    /// Maximum number of agents per user.
    pub max_agents_per_user: u32,
    /// How long an agent can be idle before transitioning to Idle state (seconds).
    pub idle_timeout_seconds: u64,
    /// How long an Idle agent waits before auto-hibernating (seconds).
    pub hibernate_after_idle_seconds: u64,
    /// Interval for heartbeat checks (seconds).
    pub heartbeat_interval_seconds: u64,
    /// How long without heartbeat before marking agent as Error (seconds).
    pub heartbeat_timeout_seconds: u64,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            max_agents_per_user: 10,
            idle_timeout_seconds: 300,          // 5 minutes
            hibernate_after_idle_seconds: 1800, // 30 minutes
            heartbeat_interval_seconds: 30,
            heartbeat_timeout_seconds: 90,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_agent_request_new() {
        let req = CreateAgentRequest::new("my-agent");
        assert_eq!(req.name, "my-agent");
        assert!(req.spec.is_none());
    }

    #[test]
    fn create_agent_request_with_spec() {
        let spec = AgentSpec {
            cpu_millicores: 1000,
            memory_mb: 1024,
            runtime_version: "v1.0.0".to_string(),
        };
        let req = CreateAgentRequest::with_spec("my-agent", spec.clone());
        assert_eq!(req.name, "my-agent");
        assert_eq!(req.spec.unwrap().cpu_millicores, 1000);
    }

    #[test]
    fn log_options_defaults() {
        let opts = LogOptions::default();
        assert_eq!(opts.lines, 100);
        assert!(!opts.follow);
    }

    #[test]
    fn control_config_defaults() {
        let config = ControlConfig::default();
        assert_eq!(config.max_agents_per_user, 10);
        assert_eq!(config.idle_timeout_seconds, 300);
    }
}
