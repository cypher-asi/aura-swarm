//! Pod specification builder for Kubernetes.
//!
//! This module provides helpers to construct Kubernetes pod specs
//! for Aura agent pods with all necessary configuration.

use aura_swarm_core::AgentId;
use aura_swarm_store::AgentSpec;
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, HTTPGetAction, PersistentVolumeClaimVolumeSource, Pod,
    PodSecurityContext, PodSpec, Probe, ResourceRequirements, Volume, VolumeMount,
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
    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), "swarm-agent".to_string());
    labels.insert("swarm.io/agent-id".to_string(), agent_id_hex.to_string());
    labels.insert("swarm.io/user-id".to_string(), user_id_hex.to_string());

    let mut annotations = BTreeMap::new();
    annotations.insert(
        "swarm.io/created-at".to_string(),
        chrono::Utc::now().to_rfc3339(),
    );

    ObjectMeta {
        name: Some(pod_name.to_string()),
        namespace: Some(config.namespace.clone()),
        labels: Some(labels),
        annotations: Some(annotations),
        ..Default::default()
    }
}

fn build_pod_spec(
    agent_id_hex: &str,
    user_id_hex: &str,
    spec: &AgentSpec,
    config: &SchedulerConfig,
) -> PodSpec {
    PodSpec {
        runtime_class_name: Some(config.runtime_class.clone()),
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

fn build_env_vars(agent_id_hex: &str, user_id_hex: &str, config: &SchedulerConfig) -> Vec<EnvVar> {
    vec![
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
    ]
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

    fn test_agent_id() -> AgentId {
        let user_id = UserId::from_bytes([1u8; 32]);
        AgentId::generate(&user_id, "test-agent")
    }

    fn test_spec() -> AgentSpec {
        AgentSpec {
            cpu_millicores: 500,
            memory_mb: 512,
            runtime_version: "latest".to_string(),
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
    }

    #[test]
    fn build_pod_uses_spec_resources() {
        let agent_id = test_agent_id();
        let user_id = UserId::from_bytes([1u8; 32]);
        let spec = AgentSpec {
            cpu_millicores: 1000,
            memory_mb: 2048,
            runtime_version: "v1.0".to_string(),
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
}
