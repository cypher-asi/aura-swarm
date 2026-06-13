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

/// An active agent as reported by the gateway's internal API.
///
/// Used by the desired-state reconciler to determine which agents
/// should have running pods.
#[derive(Debug, Clone, Deserialize)]
pub struct ActiveAgentInfo {
    /// Agent ID (hex string).
    pub agent_id: String,
    /// Owner user ID (hex string).
    pub user_id: String,
    /// Human-readable agent name.
    pub name: String,
    /// Agent resource spec.
    pub spec: aura_swarm_store::AgentSpec,
}

/// Backend used to realize a confidential (`ConfidentialVM`) agent pod.
///
/// Selects *how* a confidential pod runs, independent of the agent's
/// [`IsolationLevel`]:
///
/// - [`SnpLocal`](ConfidentialRuntime::SnpLocal): on-node `kata-qemu-snp`.
///   The pod boots its SEV-SNP guest on a tainted bare-metal worker, so it
///   carries the `swarm.io/confidential-node` node selector + toleration.
///   The current default; byte-identical to the pre-Peer-Pods behavior.
/// - [`PeerPods`](ConfidentialRuntime::PeerPods): CAA `kata-remote`. The
///   workload runs in an off-cluster AWS-managed SEV-SNP pod VM; the shim
///   runs on an ordinary worker, so the SNP node selector/toleration are
///   dropped. Attestation still happens via the in-guest CDH.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConfidentialRuntime {
    /// On-node `kata-qemu-snp` on the SEV-SNP bare-metal pool (default).
    #[default]
    SnpLocal,
    /// CAA `kata-remote` per-agent AWS-managed SEV-SNP pod VM.
    PeerPods,
}

/// Configuration for the Kubernetes scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// Kubernetes namespace for agent pods.
    pub namespace: String,
    /// Default isolation level for agents that don't specify one.
    /// Determines whether pods run as containers or microVMs.
    pub default_isolation: IsolationLevel,
    /// Container image for the aura-harness reasoning engine.
    pub image: String,
    /// Internal URL of the control plane service (deprecated, use `gateway_url`).
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
    /// Billing configuration.
    #[serde(default)]
    pub billing: crate::billing::SchedulerBillingConfig,
    /// URL of the Aura router service for LLM proxy routing.
    pub aura_router_url: String,
    /// URL of the Aura storage service.
    pub aura_storage_url: String,
    /// URL of the Aura network service.
    pub aura_network_url: String,
    /// Internal bearer token for gateway service-to-service APIs.
    pub gateway_token: String,
    /// URL of the Trustee KBS (key broker service), injected into
    /// confidential agent pods as `AURA_KBS_URL` so the harness can fetch
    /// its state DEK after attestation. Container (dev-mode) pods do not
    /// receive this variable.
    #[serde(default = "default_kbs_url")]
    pub kbs_url: String,
    /// Backend used to realize confidential (`ConfidentialVM`) agent pods.
    /// `SnpLocal` (default) keeps on-node `kata-qemu-snp`; `PeerPods`
    /// switches confidential pods to the CAA `kata-remote` runtime and
    /// drops the SNP node selector/toleration. Selected via
    /// `CONFIDENTIAL_RUNTIME`.
    #[serde(default)]
    pub confidential_runtime: ConfidentialRuntime,
    /// Effective `runtimeClassName` for confidential pods. Defaults to
    /// `kata-qemu-snp` (derived from `IsolationLevel::ConfidentialVM`) in
    /// `SnpLocal` mode and `kata-remote` in `PeerPods` mode; an explicit
    /// `CONFIDENTIAL_RUNTIME_CLASS` overrides both.
    #[serde(default = "default_confidential_runtime_class")]
    pub confidential_runtime_class: String,
}

fn default_kbs_url() -> String {
    "http://kbs.swarm-system.svc.cluster.local:8080".to_string()
}

/// Default confidential `runtimeClassName`: the on-node SNP class, sourced
/// from [`IsolationLevel::ConfidentialVM`] so the two stay in lockstep.
fn default_confidential_runtime_class() -> String {
    IsolationLevel::ConfidentialVM
        .runtime_class()
        .unwrap_or("kata-qemu-snp")
        .to_string()
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            namespace: "swarm-agents".to_string(),
            default_isolation: IsolationLevel::ConfidentialVM,
            image: "ghcr.io/cypher-asi/aura-harness:latest".to_string(),
            control_plane_url: "http://aura-swarm-gateway.swarm-system.svc:8080".to_string(),
            gateway_url: "http://aura-swarm-gateway.swarm-system.svc:8080".to_string(),
            state_pvc_name: "swarm-agent-state".to_string(),
            default_cpu_millicores: 500,
            default_memory_mb: 512,
            max_cpu_millicores: 4000,
            max_memory_mb: 8192,
            billing: crate::billing::SchedulerBillingConfig::default(),
            aura_router_url: "https://aura-router.onrender.com".to_string(),
            aura_storage_url: "https://aura-storage.onrender.com".to_string(),
            aura_network_url: "https://aura-network.onrender.com".to_string(),
            gateway_token: String::new(),
            kbs_url: default_kbs_url(),
            confidential_runtime: ConfidentialRuntime::SnpLocal,
            confidential_runtime_class: default_confidential_runtime_class(),
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
    /// - `AURA_HARNESS_IMAGE`: Container image for the aura-harness reasoning engine
    /// - `CONTROL_PLANE_URL`: Internal URL of the control plane service (deprecated)
    /// - `GATEWAY_URL`: Internal URL of the gateway service for status callbacks
    /// - `STATE_PVC_NAME`: PVC name for agent state storage
    /// - `DEFAULT_ISOLATION`: Default isolation level ("container" for
    ///   dev-mode or "confidential_vm")
    /// - `DEFAULT_CPU_MILLICORES`: Default CPU allocation
    /// - `DEFAULT_MEMORY_MB`: Default memory allocation
    /// - `MAX_CPU_MILLICORES`: Maximum CPU allowed
    /// - `MAX_MEMORY_MB`: Maximum memory allowed
    /// - `AURA_ROUTER_URL`: URL of the Aura router service
    /// - `AURA_STORAGE_URL`: URL of the Aura storage service
    /// - `AURA_NETWORK_URL`: URL of the Aura network service
    /// - `INTERNAL_TOKEN`: bearer token for gateway internal APIs
    /// - `GATEWAY_TOKEN`: legacy name for `INTERNAL_TOKEN`
    /// - `KBS_URL`: Trustee KBS URL injected into confidential agent pods
    ///   as `AURA_KBS_URL`
    /// - `CONFIDENTIAL_RUNTIME`: confidential backend, `snp_local`
    ///   (default) or `peer_pods` (case-insensitive; anything else falls
    ///   back to `snp_local`)
    /// - `CONFIDENTIAL_RUNTIME_CLASS`: explicit override of the
    ///   confidential `runtimeClassName`; when set and non-empty it wins
    ///   over the mode-derived default
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(val) = std::env::var("SCHEDULER_NAMESPACE") {
            config.namespace = val;
        }
        if let Ok(val) = std::env::var("AURA_HARNESS_IMAGE") {
            config.image = val;
        }
        if let Ok(val) = std::env::var("CONTROL_PLANE_URL") {
            config.control_plane_url.clone_from(&val);
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
                "confidential_vm" | "confidential" | "snp" | "kata-qemu-snp" => {
                    IsolationLevel::ConfidentialVM
                }
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
        if let Ok(val) = std::env::var("AURA_ROUTER_URL") {
            config.aura_router_url = val;
        }
        if let Ok(val) = std::env::var("AURA_STORAGE_URL") {
            config.aura_storage_url = val;
        }
        if let Ok(val) = std::env::var("AURA_NETWORK_URL") {
            config.aura_network_url = val;
        }
        if let Ok(val) = std::env::var("INTERNAL_TOKEN") {
            config.gateway_token = val;
        } else if let Ok(val) = std::env::var("GATEWAY_TOKEN") {
            config.gateway_token = val;
        }
        if let Ok(val) = std::env::var("KBS_URL") {
            config.kbs_url = val;
        }

        // Confidential runtime backend. Be lenient: an unset, empty, or
        // unrecognized value falls back to `snp_local` (the safe, no-churn
        // default) rather than failing the scheduler.
        config.confidential_runtime = match std::env::var("CONFIDENTIAL_RUNTIME") {
            // Only `peer_pods` opts into the remote backend; `snp_local`,
            // unset, empty, and anything unrecognized all map to the safe
            // on-node default.
            Ok(val) if val.trim().eq_ignore_ascii_case("peer_pods") => {
                ConfidentialRuntime::PeerPods
            }
            _ => ConfidentialRuntime::SnpLocal,
        };

        // Effective runtime class: an explicit non-empty override wins;
        // otherwise derive it from the selected mode.
        config.confidential_runtime_class =
            match std::env::var("CONFIDENTIAL_RUNTIME_CLASS") {
                Ok(val) if !val.trim().is_empty() => val,
                _ => match config.confidential_runtime {
                    ConfidentialRuntime::SnpLocal => default_confidential_runtime_class(),
                    ConfidentialRuntime::PeerPods => "kata-remote".to_string(),
                },
            };

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
        assert_eq!(config.default_isolation, IsolationLevel::ConfidentialVM);
        assert_eq!(
            config.default_isolation.runtime_class(),
            Some("kata-qemu-snp")
        );
        assert_eq!(config.default_cpu_millicores, 500);
        assert_eq!(config.default_memory_mb, 512);
        assert_eq!(
            config.kbs_url,
            "http://kbs.swarm-system.svc.cluster.local:8080"
        );
        // Default confidential backend is on-node SNP (no fleet churn on
        // deploy); the effective class mirrors `IsolationLevel`.
        assert_eq!(config.confidential_runtime, ConfidentialRuntime::SnpLocal);
        assert_eq!(config.confidential_runtime_class, "kata-qemu-snp");
    }

    // Confidential-runtime env parsing. Mutating process-global env is not
    // parallel-safe, so all cases live in one serialized test that
    // set/asserts/removes the vars itself. No other test in this crate
    // touches `CONFIDENTIAL_RUNTIME*`.
    #[test]
    fn from_env_confidential_runtime_modes() {
        std::env::remove_var("CONFIDENTIAL_RUNTIME");
        std::env::remove_var("CONFIDENTIAL_RUNTIME_CLASS");

        // Unset -> snp_local / kata-qemu-snp (unchanged behavior).
        let config = SchedulerConfig::from_env();
        assert_eq!(config.confidential_runtime, ConfidentialRuntime::SnpLocal);
        assert_eq!(config.confidential_runtime_class, "kata-qemu-snp");

        // peer_pods -> kata-remote.
        std::env::set_var("CONFIDENTIAL_RUNTIME", "peer_pods");
        let config = SchedulerConfig::from_env();
        assert_eq!(config.confidential_runtime, ConfidentialRuntime::PeerPods);
        assert_eq!(config.confidential_runtime_class, "kata-remote");

        // Case-insensitive.
        std::env::set_var("CONFIDENTIAL_RUNTIME", "PEER_PODS");
        let config = SchedulerConfig::from_env();
        assert_eq!(config.confidential_runtime, ConfidentialRuntime::PeerPods);
        assert_eq!(config.confidential_runtime_class, "kata-remote");

        // Unrecognized -> snp_local fallback.
        std::env::set_var("CONFIDENTIAL_RUNTIME", "bogus");
        let config = SchedulerConfig::from_env();
        assert_eq!(config.confidential_runtime, ConfidentialRuntime::SnpLocal);
        assert_eq!(config.confidential_runtime_class, "kata-qemu-snp");

        // Explicit class override wins over the mode-derived default.
        std::env::set_var("CONFIDENTIAL_RUNTIME", "peer_pods");
        std::env::set_var("CONFIDENTIAL_RUNTIME_CLASS", "foo");
        let config = SchedulerConfig::from_env();
        assert_eq!(config.confidential_runtime, ConfidentialRuntime::PeerPods);
        assert_eq!(config.confidential_runtime_class, "foo");

        std::env::remove_var("CONFIDENTIAL_RUNTIME");
        std::env::remove_var("CONFIDENTIAL_RUNTIME_CLASS");
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
