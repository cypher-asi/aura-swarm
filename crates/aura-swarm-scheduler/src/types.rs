//! Types for the scheduler crate.

use aura_swarm_core::AgentId;
use aura_swarm_store::IsolationLevel;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Status of a pod in Kubernetes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodStatus {
    /// Current phase of the pod lifecycle.
    pub phase: PodPhase,
    /// Whether the pod is ready to serve traffic.
    pub ready: bool,
    /// Number of times the pod has restarted.
    pub restart_count: u32,
    /// When the pod was started.
    pub started_at: Option<DateTime<Utc>>,
    /// Human-readable message about the pod's status.
    pub message: Option<String>,
}

impl Default for PodStatus {
    fn default() -> Self {
        Self {
            phase: PodPhase::Unknown,
            ready: false,
            restart_count: 0,
            started_at: None,
            message: None,
        }
    }
}

/// Phase of the pod lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PodPhase {
    /// Pod has been accepted but containers are not yet running.
    Pending,
    /// Pod is running with at least one container.
    Running,
    /// All containers terminated successfully.
    Succeeded,
    /// At least one container failed.
    Failed,
    /// Pod status cannot be determined.
    #[default]
    Unknown,
}

impl PodPhase {
    /// Parse a pod phase from a Kubernetes phase string.
    #[must_use]
    pub fn from_k8s_phase(phase: &str) -> Self {
        match phase {
            "Pending" => Self::Pending,
            "Running" => Self::Running,
            "Succeeded" => Self::Succeeded,
            "Failed" => Self::Failed,
            _ => Self::Unknown,
        }
    }

    /// Check if the pod is in a terminal state.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }

    /// Check if the pod is running or pending.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

/// Information about a scheduled pod.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodInfo {
    /// Agent ID this pod belongs to.
    pub agent_id: AgentId,
    /// Kubernetes pod name.
    pub pod_name: String,
    /// Node the pod is scheduled on.
    pub node_name: Option<String>,
    /// Pod's IP address.
    pub pod_ip: Option<String>,
    /// Current status of the pod.
    pub status: PodStatus,
}

/// Configuration for the Kubernetes scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// Kubernetes namespace for agent pods.
    pub namespace: String,
    /// Default isolation level for agents that don't specify one.
    /// Determines whether pods run as containers or microVMs.
    pub default_isolation: IsolationLevel,
    /// Container image for the Aura runtime.
    pub image: String,
    /// Internal URL of the control plane service (deprecated, use gateway_url).
    pub control_plane_url: String,
    /// Internal URL of the gateway service for status callbacks.
    pub gateway_url: String,
    /// PVC name for agent state storage.
    pub state_pvc_name: String,
    /// Default CPU allocation in millicores.
    pub default_cpu_millicores: u32,
    /// Default memory allocation in megabytes.
    pub default_memory_mb: u32,
    /// Maximum CPU allowed in millicores.
    pub max_cpu_millicores: u32,
    /// Maximum memory allowed in megabytes.
    pub max_memory_mb: u32,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            namespace: "swarm-agents".to_string(),
            default_isolation: IsolationLevel::MicroVM,
            image: "ghcr.io/cypher-asi/aura-runtime:latest".to_string(),
            control_plane_url: "http://aura-swarm-gateway.swarm-system.svc:8080".to_string(),
            gateway_url: "http://aura-swarm-gateway.swarm-system.svc:8080".to_string(),
            state_pvc_name: "swarm-agent-state".to_string(),
            default_cpu_millicores: 500,
            default_memory_mb: 512,
            max_cpu_millicores: 4000,
            max_memory_mb: 8192,
        }
    }
}

impl SchedulerConfig {
    /// Create a new scheduler config with the given namespace.
    #[must_use]
    pub fn with_namespace(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            ..Default::default()
        }
    }

    /// Load configuration from environment variables.
    ///
    /// Supported environment variables:
    /// - `SCHEDULER_NAMESPACE`: Kubernetes namespace for agent pods
    /// - `AURA_RUNTIME_IMAGE`: Container image for the Aura runtime
    /// - `CONTROL_PLANE_URL`: Internal URL of the control plane service (deprecated)
    /// - `GATEWAY_URL`: Internal URL of the gateway service for status callbacks
    /// - `STATE_PVC_NAME`: PVC name for agent state storage
    /// - `DEFAULT_ISOLATION`: Default isolation level ("container" or "microvm")
    /// - `DEFAULT_CPU_MILLICORES`: Default CPU allocation
    /// - `DEFAULT_MEMORY_MB`: Default memory allocation
    /// - `MAX_CPU_MILLICORES`: Maximum CPU allowed
    /// - `MAX_MEMORY_MB`: Maximum memory allowed
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(val) = std::env::var("SCHEDULER_NAMESPACE") {
            config.namespace = val;
        }
        if let Ok(val) = std::env::var("AURA_RUNTIME_IMAGE") {
            config.image = val;
        }
        if let Ok(val) = std::env::var("CONTROL_PLANE_URL") {
            config.control_plane_url = val.clone();
            // Also use as gateway_url if GATEWAY_URL not set
            if std::env::var("GATEWAY_URL").is_err() {
                config.gateway_url = val;
            }
        }
        if let Ok(val) = std::env::var("GATEWAY_URL") {
            config.gateway_url = val;
        }
        if let Ok(val) = std::env::var("STATE_PVC_NAME") {
            config.state_pvc_name = val;
        }
        if let Ok(val) = std::env::var("DEFAULT_ISOLATION") {
            config.default_isolation = match val.to_lowercase().as_str() {
                "container" | "runc" => IsolationLevel::Container,
                "microvm" | "kata" | "kata-fc" => IsolationLevel::MicroVM,
                _ => config.default_isolation,
            };
        }
        if let Ok(val) = std::env::var("DEFAULT_CPU_MILLICORES") {
            if let Ok(n) = val.parse() {
                config.default_cpu_millicores = n;
            }
        }
        if let Ok(val) = std::env::var("DEFAULT_MEMORY_MB") {
            if let Ok(n) = val.parse() {
                config.default_memory_mb = n;
            }
        }
        if let Ok(val) = std::env::var("MAX_CPU_MILLICORES") {
            if let Ok(n) = val.parse() {
                config.max_cpu_millicores = n;
            }
        }
        if let Ok(val) = std::env::var("MAX_MEMORY_MB") {
            if let Ok(n) = val.parse() {
                config.max_memory_mb = n;
            }
        }

        config
    }

    /// Validate resource requests against limits.
    ///
    /// # Errors
    ///
    /// Returns an error if CPU or memory exceed the configured maximums.
    pub fn validate_resources(&self, cpu_millicores: u32, memory_mb: u32) -> crate::Result<()> {
        if cpu_millicores > self.max_cpu_millicores {
            return Err(crate::SchedulerError::Config(format!(
                "CPU request {}m exceeds maximum {}m",
                cpu_millicores, self.max_cpu_millicores
            )));
        }
        if memory_mb > self.max_memory_mb {
            return Err(crate::SchedulerError::Config(format!(
                "Memory request {}Mi exceeds maximum {}Mi",
                memory_mb, self.max_memory_mb
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_phase_from_k8s() {
        assert_eq!(PodPhase::from_k8s_phase("Pending"), PodPhase::Pending);
        assert_eq!(PodPhase::from_k8s_phase("Running"), PodPhase::Running);
        assert_eq!(PodPhase::from_k8s_phase("Succeeded"), PodPhase::Succeeded);
        assert_eq!(PodPhase::from_k8s_phase("Failed"), PodPhase::Failed);
        assert_eq!(PodPhase::from_k8s_phase("Unknown"), PodPhase::Unknown);
        assert_eq!(PodPhase::from_k8s_phase("Invalid"), PodPhase::Unknown);
    }

    #[test]
    fn pod_phase_states() {
        assert!(PodPhase::Succeeded.is_terminal());
        assert!(PodPhase::Failed.is_terminal());
        assert!(!PodPhase::Running.is_terminal());
        assert!(!PodPhase::Pending.is_terminal());

        assert!(PodPhase::Running.is_active());
        assert!(PodPhase::Pending.is_active());
        assert!(!PodPhase::Failed.is_active());
        assert!(!PodPhase::Succeeded.is_active());
    }

    #[test]
    fn scheduler_config_defaults() {
        let config = SchedulerConfig::default();
        assert_eq!(config.namespace, "swarm-agents");
        assert_eq!(config.default_isolation, IsolationLevel::MicroVM);
        assert_eq!(config.default_isolation.runtime_class(), Some("kata-fc"));
        assert_eq!(config.default_cpu_millicores, 500);
        assert_eq!(config.default_memory_mb, 512);
    }

    #[test]
    fn scheduler_config_validate_resources() {
        let config = SchedulerConfig::default();

        // Valid resources
        assert!(config.validate_resources(500, 512).is_ok());
        assert!(config.validate_resources(4000, 8192).is_ok());

        // Invalid resources
        assert!(config.validate_resources(5000, 512).is_err());
        assert!(config.validate_resources(500, 10000).is_err());
    }
}
