//! Pod specification builder for Kubernetes.
//!
//! This module provides helpers to construct Kubernetes pod specs
//! for Aura agent pods with all necessary configuration.

use aura_swarm_core::AgentId;
use aura_swarm_store::AgentSpec;
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, EnvVarSource, HTTPGetAction,
    PersistentVolumeClaimVolumeSource, Pod, PodSecurityContext, PodSpec, Probe,
    ResourceRequirements, SecretKeySelector, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::ObjectMeta;
use std::collections::BTreeMap;

use crate::SchedulerConfig;

/// The container port for the Aura runtime HTTP server.
const AURA_PORT: i32 = 8080;

/// Build a Kubernetes pod spec for an agent.
///
/// This creates a complete pod specification including:
/// - Kata Containers runtime class for microVM isolation
/// - Resource requests and limits
/// - Environment variables for agent configuration
/// - Volume mounts for persistent state
/// - Health probes for readiness and liveness
#[must_use]
pub fn build_pod(
    agent_id: &AgentId,
    user_id_hex: &str,
    spec: &AgentSpec,
    config: &SchedulerConfig,
) -> Pod {
    let pod_name = pod_name_for_agent(agent_id);
    let agent_id_hex = agent_id.to_hex();

    Pod {
        metadata: build_metadata(&pod_name, &agent_id_hex, user_id_hex, config),
        spec: Some(build_pod_spec(&agent_id_hex, user_id_hex, spec, config)),
        ..Default::default()
    }
}

/// Generate the pod name for an agent.
///
/// Uses the first 16 characters of the agent ID hex for brevity.
#[must_use]
pub fn pod_name_for_agent(agent_id: &AgentId) -> String {
    format!("agent-{}", &agent_id.to_hex()[..16])
}

fn build_metadata(
    pod_name: &str,
    agent_id_hex: &str,
    user_id_hex: &str,
    config: &SchedulerConfig,
) -> ObjectMeta {
    // Kubernetes labels have a max length of 63 characters.
    // Truncate hex IDs (64 chars) to fit, store full IDs in annotations.
    let agent_id_label = truncate_for_label(agent_id_hex);
    let user_id_label = truncate_for_label(user_id_hex);

    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), "swarm-agent".to_string());
    labels.insert("swarm.io/agent-id".to_string(), agent_id_label);
    labels.insert("swarm.io/user-id".to_string(), user_id_label);

    let mut annotations = BTreeMap::new();
    annotations.insert(
        "swarm.io/created-at".to_string(),
        chrono::Utc::now().to_rfc3339(),
    );
    // Store full IDs in annotations (no length limit)
    annotations.insert("swarm.io/agent-id-full".to_string(), agent_id_hex.to_string());
    annotations.insert("swarm.io/user-id-full".to_string(), user_id_hex.to_string());

    ObjectMeta {
        name: Some(pod_name.to_string()),
        namespace: Some(config.namespace.clone()),
        labels: Some(labels),
        annotations: Some(annotations),
        ..Default::default()
    }
}

/// Truncate a string to fit Kubernetes label value limit (63 chars).
fn truncate_for_label(s: &str) -> String {
    if s.len() <= 63 {
        s.to_string()
    } else {
        s[..63].to_string()
    }
}

fn build_pod_spec(
    agent_id_hex: &str,
    user_id_hex: &str,
    spec: &AgentSpec,
    config: &SchedulerConfig,
) -> PodSpec {
    // Use agent's isolation level if specified, otherwise use scheduler default
    let isolation = spec.isolation.unwrap_or(config.default_isolation);
    // runtime_class() returns None for standard containers (uses default runtime)
    let runtime_class_name = isolation.runtime_class().map(String::from);

    PodSpec {
        runtime_class_name,
        containers: vec![build_container(agent_id_hex, user_id_hex, spec, config)],
        volumes: Some(vec![build_state_volume(config)]),
        restart_policy: Some("Always".to_string()),
        termination_grace_period_seconds: Some(30),
        security_context: Some(build_security_context()),
        ..Default::default()
    }
}

fn build_container(
    agent_id_hex: &str,
    user_id_hex: &str,
    spec: &AgentSpec,
    config: &SchedulerConfig,
) -> Container {
    Container {
        name: "aura".to_string(),
        image: Some(config.image.clone()),
        ports: Some(vec![ContainerPort {
            container_port: AURA_PORT,
            name: Some("http".to_string()),
            ..Default::default()
        }]),
        env: Some(build_env_vars(agent_id_hex, user_id_hex, config)),
        resources: Some(build_resources(spec)),
        volume_mounts: Some(vec![build_state_mount(agent_id_hex)]),
        readiness_probe: Some(build_readiness_probe()),
        liveness_probe: Some(build_liveness_probe()),
        ..Default::default()
    }
}

/// Name of the Kubernetes secret containing LLM API keys.
const LLM_SECRETS_NAME: &str = "aura-swarm-secrets";

fn build_env_vars(agent_id_hex: &str, user_id_hex: &str, config: &SchedulerConfig) -> Vec<EnvVar> {
    vec![
        // Agent identity
        EnvVar {
            name: "AGENT_ID".to_string(),
            value: Some(agent_id_hex.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "USER_ID".to_string(),
            value: Some(user_id_hex.to_string()),
            ..Default::default()
        },
        // Runtime configuration
        EnvVar {
            name: "STATE_DIR".to_string(),
            value: Some("/state".to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "AURA_LISTEN_ADDR".to_string(),
            value: Some(format!("0.0.0.0:{AURA_PORT}")),
            ..Default::default()
        },
        EnvVar {
            name: "CONTROL_PLANE_URL".to_string(),
            value: Some(config.control_plane_url.clone()),
            ..Default::default()
        },
        // LLM API keys (injected from Kubernetes secret)
        build_secret_env_var("ANTHROPIC_API_KEY", LLM_SECRETS_NAME, "ANTHROPIC_API_KEY"),
        build_secret_env_var("OPENAI_API_KEY", LLM_SECRETS_NAME, "OPENAI_API_KEY"),
    ]
}

/// Build an environment variable that references a Kubernetes secret.
fn build_secret_env_var(env_name: &str, secret_name: &str, secret_key: &str) -> EnvVar {
    EnvVar {
        name: env_name.to_string(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: secret_name.to_string(),
                key: secret_key.to_string(),
                optional: Some(true), // Don't fail pod startup if key is missing
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_resources(spec: &AgentSpec) -> ResourceRequirements {
    let cpu = Quantity(format!("{}m", spec.cpu_millicores));
    let memory = Quantity(format!("{}Mi", spec.memory_mb));

    let mut requests = BTreeMap::new();
    requests.insert("cpu".to_string(), cpu.clone());
    requests.insert("memory".to_string(), memory.clone());

    let mut limits = BTreeMap::new();
    limits.insert("cpu".to_string(), cpu);
    limits.insert("memory".to_string(), memory);

    ResourceRequirements {
        requests: Some(requests),
        limits: Some(limits),
        ..Default::default()
    }
}

fn build_state_volume(config: &SchedulerConfig) -> Volume {
    Volume {
        name: "state".to_string(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
            claim_name: config.state_pvc_name.clone(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_state_mount(agent_id_hex: &str) -> VolumeMount {
    VolumeMount {
        name: "state".to_string(),
        mount_path: "/state".to_string(),
        sub_path: Some(agent_id_hex.to_string()),
        ..Default::default()
    }
}

fn build_readiness_probe() -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some("/health".to_string()),
            port: IntOrString::Int(AURA_PORT),
            ..Default::default()
        }),
        initial_delay_seconds: Some(5),
        period_seconds: Some(10),
        timeout_seconds: Some(5),
        failure_threshold: Some(3),
        ..Default::default()
    }
}

fn build_liveness_probe() -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some("/health".to_string()),
            port: IntOrString::Int(AURA_PORT),
            ..Default::default()
        }),
        initial_delay_seconds: Some(30),
        period_seconds: Some(30),
        timeout_seconds: Some(10),
        failure_threshold: Some(3),
        ..Default::default()
    }
}

fn build_security_context() -> PodSecurityContext {
    PodSecurityContext {
        run_as_non_root: Some(true),
        run_as_user: Some(1000),
        fs_group: Some(1000),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_swarm_core::UserId;
    use aura_swarm_store::IsolationLevel;

    fn test_agent_id() -> AgentId {
        let user_id = UserId::from_bytes([1u8; 32]);
        AgentId::generate(&user_id, "test-agent")
    }

    fn test_spec() -> AgentSpec {
        AgentSpec {
            cpu_millicores: 500,
            memory_mb: 512,
            runtime_version: "latest".to_string(),
            isolation: None, // Uses scheduler default
        }
    }

    #[test]
    fn pod_name_format() {
        let agent_id = test_agent_id();
        let name = pod_name_for_agent(&agent_id);

        assert!(name.starts_with("agent-"));
        assert_eq!(name.len(), 6 + 16); // "agent-" + 16 hex chars
    }

    #[test]
    fn build_pod_has_required_fields() {
        let agent_id = test_agent_id();
        let user_id = UserId::from_bytes([1u8; 32]);
        let spec = test_spec();
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, &user_id.to_hex(), &spec, &config);

        // Metadata
        let meta = &pod.metadata;
        assert!(meta.name.is_some());
        assert_eq!(meta.namespace.as_deref(), Some("swarm-agents"));

        let labels = meta.labels.as_ref().unwrap();
        assert_eq!(labels.get("app"), Some(&"swarm-agent".to_string()));
        assert!(labels.contains_key("swarm.io/agent-id"));
        assert!(labels.contains_key("swarm.io/user-id"));

        // Spec
        let pod_spec = pod.spec.as_ref().unwrap();
        assert_eq!(pod_spec.runtime_class_name.as_deref(), Some("kata-fc"));
        assert_eq!(pod_spec.restart_policy.as_deref(), Some("Always"));
        assert_eq!(pod_spec.termination_grace_period_seconds, Some(30));

        // Container
        let container = &pod_spec.containers[0];
        assert_eq!(container.name, "aura");
        assert!(container.image.is_some());
        assert!(container.env.is_some());
        assert!(container.resources.is_some());
        assert!(container.readiness_probe.is_some());
        assert!(container.liveness_probe.is_some());

        // Environment variables
        let env = container.env.as_ref().unwrap();
        let env_names: Vec<_> = env.iter().map(|e| e.name.as_str()).collect();
        assert!(env_names.contains(&"AGENT_ID"));
        assert!(env_names.contains(&"USER_ID"));
        assert!(env_names.contains(&"STATE_DIR"));
        assert!(env_names.contains(&"AURA_LISTEN_ADDR"));
        assert!(env_names.contains(&"CONTROL_PLANE_URL"));
        assert!(env_names.contains(&"ANTHROPIC_API_KEY"));
        assert!(env_names.contains(&"OPENAI_API_KEY"));
    }

    #[test]
    fn build_pod_uses_spec_resources() {
        let agent_id = test_agent_id();
        let user_id = UserId::from_bytes([1u8; 32]);
        let spec = AgentSpec {
            cpu_millicores: 1000,
            memory_mb: 2048,
            runtime_version: "v1.0".to_string(),
            isolation: None,
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, &user_id.to_hex(), &spec, &config);
        let container = &pod.spec.as_ref().unwrap().containers[0];
        let resources = container.resources.as_ref().unwrap();

        let requests = resources.requests.as_ref().unwrap();
        assert_eq!(requests.get("cpu"), Some(&Quantity("1000m".to_string())));
        assert_eq!(
            requests.get("memory"),
            Some(&Quantity("2048Mi".to_string()))
        );

        let limits = resources.limits.as_ref().unwrap();
        assert_eq!(limits.get("cpu"), Some(&Quantity("1000m".to_string())));
        assert_eq!(limits.get("memory"), Some(&Quantity("2048Mi".to_string())));
    }

    #[test]
    fn build_pod_uses_default_isolation_when_none_specified() {
        let agent_id = test_agent_id();
        let user_id = UserId::from_bytes([1u8; 32]);
        let spec = AgentSpec {
            isolation: None, // Should use scheduler default (MicroVM)
            ..test_spec()
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, &user_id.to_hex(), &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        assert_eq!(pod_spec.runtime_class_name.as_deref(), Some("kata-fc"));
    }

    #[test]
    fn build_pod_uses_agent_isolation_when_specified() {
        let agent_id = test_agent_id();
        let user_id = UserId::from_bytes([1u8; 32]);
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::Container), // Override to container
            ..test_spec()
        };
        let config = SchedulerConfig::default(); // Default is MicroVM

        let pod = build_pod(&agent_id, &user_id.to_hex(), &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        // Container isolation uses default runtime (no RuntimeClass specified)
        assert_eq!(pod_spec.runtime_class_name, None);
    }

    #[test]
    fn build_pod_respects_scheduler_default_isolation() {
        let agent_id = test_agent_id();
        let user_id = UserId::from_bytes([1u8; 32]);
        let spec = AgentSpec {
            isolation: None, // Use scheduler default
            ..test_spec()
        };
        let mut config = SchedulerConfig::default();
        config.default_isolation = IsolationLevel::Container; // Change default

        let pod = build_pod(&agent_id, &user_id.to_hex(), &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        // Container isolation uses default runtime (no RuntimeClass specified)
        assert_eq!(pod_spec.runtime_class_name, None);
    }

    #[test]
    fn build_pod_injects_llm_api_keys_from_secret() {
        let agent_id = test_agent_id();
        let user_id = UserId::from_bytes([1u8; 32]);
        let spec = test_spec();
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, &user_id.to_hex(), &spec, &config);
        let container = &pod.spec.as_ref().unwrap().containers[0];
        let env = container.env.as_ref().unwrap();

        // Find the ANTHROPIC_API_KEY env var
        let anthropic_env = env.iter().find(|e| e.name == "ANTHROPIC_API_KEY").unwrap();

        // Verify it references a secret, not a direct value
        assert!(anthropic_env.value.is_none());
        let value_from = anthropic_env.value_from.as_ref().unwrap();
        let secret_ref = value_from.secret_key_ref.as_ref().unwrap();
        assert_eq!(secret_ref.name, "aura-swarm-secrets");
        assert_eq!(secret_ref.key, "ANTHROPIC_API_KEY");
        assert_eq!(secret_ref.optional, Some(true));

        // Same for OPENAI_API_KEY
        let openai_env = env.iter().find(|e| e.name == "OPENAI_API_KEY").unwrap();
        assert!(openai_env.value.is_none());
        let value_from = openai_env.value_from.as_ref().unwrap();
        let secret_ref = value_from.secret_key_ref.as_ref().unwrap();
        assert_eq!(secret_ref.name, "aura-swarm-secrets");
        assert_eq!(secret_ref.key, "OPENAI_API_KEY");
    }
}
