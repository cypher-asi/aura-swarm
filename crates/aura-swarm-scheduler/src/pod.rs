//! Pod specification builder for Kubernetes.
//!
//! This module provides helpers to construct Kubernetes pod specs
//! for Aura agent pods with all necessary configuration.

use aura_swarm_core::AgentId;
use aura_swarm_store::{AgentSpec, IsolationLevel, StorageEncryption};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, EnvVarSource, HTTPGetAction,
    PersistentVolumeClaimVolumeSource, Pod, PodSecurityContext, PodSpec, Probe,
    ResourceRequirements, SecretKeySelector, SecurityContext, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::ObjectMeta;
use std::collections::BTreeMap;

use crate::SchedulerConfig;

/// The container port for the Aura runtime HTTP server.
const AURA_PORT: i32 = 8080;

/// Node label and taint key for the SEV-SNP bare-metal node group.
/// Confidential pods select these nodes and tolerate the matching taint.
const CONFIDENTIAL_NODE_KEY: &str = "swarm.io/confidential-node";

/// Build a Kubernetes pod spec for an agent.
///
/// This creates a complete pod specification including:
/// - Kata Containers runtime class for microVM isolation
/// - Resource requests and limits
/// - Environment variables for agent configuration
/// - Volume mounts for persistent state
/// - Health probes for readiness and liveness
///
/// A Pod ID (UUID v4) is generated internally and used in the pod name.
#[must_use]
pub fn build_pod(
    agent_id: &AgentId,
    user_id_hex: &str,
    agent_name: &str,
    spec: &AgentSpec,
    config: &SchedulerConfig,
) -> Pod {
    let pod_id = uuid::Uuid::new_v4();
    let pod_name = build_pod_name(agent_name, &pod_id);
    let agent_id_hex = agent_id.to_hex();

    // KBS key id for the sealed-state DEK. Prefer the id recorded in the
    // agent's spec; fall back to the deterministic per-agent id. Only
    // injected into confidential pods.
    let state_key_id = spec.storage_encryption.as_ref().map_or_else(
        || StorageEncryption::state_key_id(agent_id),
        |enc| enc.key_id().to_string(),
    );

    Pod {
        metadata: build_metadata(&pod_name, &agent_id_hex, agent_name, &pod_id, config),
        spec: Some(build_pod_spec(
            &agent_id_hex,
            user_id_hex,
            spec,
            &state_key_id,
            config,
        )),
        ..Default::default()
    }
}

/// Build a DNS-safe pod name from the agent name and pod ID.
///
/// Format: `{sanitized_agent_name}-{first 8 hex chars of pod_id}`
/// Sanitized: lowercase, non-alphanumeric replaced with `-`, consecutive `-`
/// collapsed, leading/trailing `-` trimmed, capped at 63 chars total.
fn build_pod_name(agent_name: &str, pod_id: &uuid::Uuid) -> String {
    let suffix = &pod_id.simple().to_string()[..8];
    let max_name_len = 63 - 1 - suffix.len(); // 63 - hyphen - suffix

    let sanitized: String = agent_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // Collapse consecutive hyphens and trim
    let mut collapsed = String::with_capacity(sanitized.len());
    let mut prev_hyphen = true; // treat start as hyphen to trim leading
    for c in sanitized.chars() {
        if c == '-' {
            if !prev_hyphen {
                collapsed.push(c);
            }
            prev_hyphen = true;
        } else {
            collapsed.push(c);
            prev_hyphen = false;
        }
    }
    let trimmed = collapsed.trim_end_matches('-');

    // Truncate to max length, ensuring we don't cut in the middle and leave a trailing hyphen
    let name_part = if trimmed.len() > max_name_len {
        trimmed[..max_name_len].trim_end_matches('-')
    } else {
        trimmed
    };

    if name_part.is_empty() {
        format!("agent-{suffix}")
    } else {
        format!("{name_part}-{suffix}")
    }
}

fn build_metadata(
    pod_name: &str,
    agent_id_hex: &str,
    agent_name: &str,
    pod_id: &uuid::Uuid,
    config: &SchedulerConfig,
) -> ObjectMeta {
    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), "swarm-agent".to_string());
    labels.insert("swarm.io/agent-id".to_string(), agent_id_hex.to_string());

    let mut annotations = BTreeMap::new();
    annotations.insert("swarm.io/agent-name".to_string(), agent_name.to_string());
    annotations.insert("swarm.io/pod-id".to_string(), pod_id.to_string());
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
    state_key_id: &str,
    config: &SchedulerConfig,
) -> PodSpec {
    // Use agent's isolation level if specified, otherwise use scheduler default
    let isolation = spec.isolation.unwrap_or(config.default_isolation);
    // runtime_class() returns None for standard containers (uses default runtime)
    let runtime_class_name = isolation.runtime_class().map(String::from);

    // SNP scheduling constraints apply ONLY to confidential agents. Legacy
    // agents must keep today's pod spec byte-for-byte (None == field omitted),
    // so the desired-state reconciler sees no diff for them during R1.
    let confidential = isolation == IsolationLevel::ConfidentialVM;
    let node_selector = confidential.then(|| {
        let mut selector = BTreeMap::new();
        selector.insert(CONFIDENTIAL_NODE_KEY.to_string(), "true".to_string());
        selector
    });
    let tolerations = confidential.then(|| {
        vec![Toleration {
            key: Some(CONFIDENTIAL_NODE_KEY.to_string()),
            operator: Some("Equal".to_string()),
            value: Some("true".to_string()),
            effect: Some("NoSchedule".to_string()),
            ..Default::default()
        }]
    });

    PodSpec {
        runtime_class_name,
        containers: vec![build_container(
            agent_id_hex,
            user_id_hex,
            spec,
            state_key_id,
            config,
            &config.image,
            isolation,
        )],
        node_selector,
        tolerations,
        volumes: Some(vec![build_state_volume(config)]),
        restart_policy: Some("Always".to_string()),
        termination_grace_period_seconds: Some(30),
        security_context: Some(build_security_context(isolation)),
        ..Default::default()
    }
}

fn build_container(
    agent_id_hex: &str,
    user_id_hex: &str,
    spec: &AgentSpec,
    state_key_id: &str,
    config: &SchedulerConfig,
    image: &str,
    isolation: IsolationLevel,
) -> Container {
    let mut env = build_env_vars(agent_id_hex, user_id_hex, config);
    // Confidential agents get the attestation/sealed-storage variables
    // APPENDED so the legacy env list (names, values, order) is untouched.
    if isolation == IsolationLevel::ConfidentialVM {
        env.extend(build_confidential_env_vars(state_key_id, config));
    }

    Container {
        name: "aura".to_string(),
        image: Some(image.to_string()),
        ports: Some(vec![ContainerPort {
            container_port: AURA_PORT,
            name: Some("http".to_string()),
            ..Default::default()
        }]),
        env: Some(env),
        resources: Some(build_resources(spec)),
        volume_mounts: Some(vec![build_state_mount(agent_id_hex)]),
        readiness_probe: Some(build_readiness_probe()),
        liveness_probe: Some(build_liveness_probe()),
        security_context: Some(build_container_security_context(isolation)),
        ..Default::default()
    }
}

/// Name of the Kubernetes secret containing LLM API keys.
const LLM_SECRETS_NAME: &str = "aura-swarm-secrets";

fn build_env_vars(agent_id_hex: &str, user_id_hex: &str, config: &SchedulerConfig) -> Vec<EnvVar> {
    vec![
        EnvVar {
            name: "AGENT_ID".to_string(),
            value: Some(agent_id_hex.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "MACHINE_ID".to_string(),
            value: Some(agent_id_hex.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "AURA_MACHINE_ID".to_string(),
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
        EnvVar {
            name: "AURA_DATA_DIR".to_string(),
            value: Some("/state".to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "AURA_LLM_ROUTING".to_string(),
            value: Some("proxy".to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "AURA_ROUTER_URL".to_string(),
            value: Some(config.aura_router_url.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "AURA_STORAGE_URL".to_string(),
            value: Some(config.aura_storage_url.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "AURA_NETWORK_URL".to_string(),
            value: Some(config.aura_network_url.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "ENABLE_FS_TOOLS".to_string(),
            value: Some("true".to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "ENABLE_CMD_TOOLS".to_string(),
            value: Some("true".to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "AURA_PROJECT_BASE".to_string(),
            value: Some("/home/aura".to_string()),
            ..Default::default()
        },
    ]
}

/// Build the extra environment variables for confidential (SEV-SNP) agents.
///
/// These drive the harness attestation boot flow: fetch the per-agent state
/// DEK identified by `AURA_STATE_KEY_ID` from the KBS at `AURA_KBS_URL`
/// (released only after successful attestation), then open the sealed state
/// overlay (`AURA_STATE_ENCRYPTION=sealed`).
///
/// Confidential pods additionally get `AURA_SWARM_INTERNAL_TOKEN` (the
/// platform `INTERNAL_TOKEN` from the scheduler env) so the harness
/// `TriggerRegistrar` can authenticate trigger-metadata registration to
/// the gateway's `/internal` endpoints, and so the harness can verify the
/// bearer the control-plane cron service presents when firing
/// `POST /v1/processes/:id/trigger`. The control-plane URL and agent id
/// the registrar also needs are already injected for every pod as
/// `CONTROL_PLANE_URL` and `AGENT_ID` (legacy env list, unchanged).
fn build_confidential_env_vars(state_key_id: &str, config: &SchedulerConfig) -> Vec<EnvVar> {
    let mut env = vec![
        EnvVar {
            name: "AURA_KBS_URL".to_string(),
            value: Some(config.kbs_url.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "AURA_STATE_ENCRYPTION".to_string(),
            value: Some("sealed".to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "AURA_STATE_KEY_ID".to_string(),
            value: Some(state_key_id.to_string()),
            ..Default::default()
        },
    ];
    // Dev mode without INTERNAL_TOKEN: omit the variable entirely so the
    // harness falls back to its unauthenticated dev behavior.
    if !config.gateway_token.is_empty() {
        env.push(EnvVar {
            name: "AURA_SWARM_INTERNAL_TOKEN".to_string(),
            value: Some(config.gateway_token.clone()),
            ..Default::default()
        });
    }
    env
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

/// Build the pod-level security context for an agent.
///
/// The security posture is gated on the isolation level:
///
/// - `MicroVM` (Kata/Firecracker): the guest kernel is the multi-tenant
///   boundary, so it is safe to give the user a root-capable dev
///   environment inside the VM — they can `apt install`, manage
///   services, and modify the OS. Runs as root (uid 0); `fs_group`
///   stays 1000 so the `/state` PVC (EFS access point owned by 1000)
///   remains group-accessible.
/// - `Container` (shared-kernel runc): root would be a host-compromise
///   risk in a multi-tenant cluster, so the locked-down non-root
///   posture is preserved.
fn build_security_context(isolation: IsolationLevel) -> PodSecurityContext {
    match isolation {
        // ConfidentialVM shares the MicroVM posture: the (confidential)
        // guest kernel is the multi-tenant boundary. SNP-specific pod
        // wiring (node selector, KBS env) lands in a later phase.
        IsolationLevel::MicroVM | IsolationLevel::ConfidentialVM => PodSecurityContext {
            run_as_non_root: Some(false),
            run_as_user: Some(0),
            fs_group: Some(1000),
            ..Default::default()
        },
        IsolationLevel::Container => PodSecurityContext {
            run_as_non_root: Some(true),
            run_as_user: Some(1000),
            fs_group: Some(1000),
            ..Default::default()
        },
    }
}

/// Build the container-level security context for an agent.
///
/// Mirrors [`build_security_context`]: microVM agents run as root with
/// privilege escalation allowed (so `sudo` works for OS modification),
/// while runc-isolated agents stay non-root with privilege escalation
/// denied.
fn build_container_security_context(isolation: IsolationLevel) -> SecurityContext {
    match isolation {
        IsolationLevel::MicroVM | IsolationLevel::ConfidentialVM => SecurityContext {
            run_as_non_root: Some(false),
            run_as_user: Some(0),
            allow_privilege_escalation: Some(true),
            ..Default::default()
        },
        IsolationLevel::Container => SecurityContext {
            run_as_non_root: Some(true),
            run_as_user: Some(1000),
            allow_privilege_escalation: Some(false),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_swarm_store::IsolationLevel;

    fn test_agent_id() -> AgentId {
        AgentId::generate()
    }

    fn test_spec() -> AgentSpec {
        AgentSpec {
            cpu_millicores: 500,
            memory_mb: 512,
            runtime_version: "latest".to_string(),
            isolation: None,
            tier: None,
            storage_encryption: None,
        }
    }

    #[test]
    fn pod_name_uses_agent_name_and_suffix() {
        let pod_id = uuid::Uuid::new_v4();
        let name = build_pod_name("my-agent", &pod_id);
        let suffix = &pod_id.simple().to_string()[..8];

        assert!(name.starts_with("my-agent-"));
        assert!(name.ends_with(suffix));
        assert!(name.len() <= 63);
    }

    #[test]
    fn pod_name_sanitizes_special_chars() {
        let pod_id = uuid::Uuid::new_v4();
        let name = build_pod_name("My Agent!@#", &pod_id);

        assert!(name.starts_with("my-agent-"));
        assert!(!name.contains('!'));
        assert!(!name.contains('@'));
    }

    #[test]
    fn pod_name_collapses_hyphens() {
        let pod_id = uuid::Uuid::new_v4();
        let name = build_pod_name("a---b", &pod_id);
        assert!(name.starts_with("a-b-"));
    }

    #[test]
    fn pod_name_falls_back_for_empty() {
        let pod_id = uuid::Uuid::new_v4();
        let name = build_pod_name("", &pod_id);
        assert!(name.starts_with("agent-"));
    }

    #[test]
    fn pod_name_caps_at_63_chars() {
        let pod_id = uuid::Uuid::new_v4();
        let long_name = "a".repeat(100);
        let name = build_pod_name(&long_name, &pod_id);
        assert!(name.len() <= 63);
    }

    #[test]
    fn build_pod_has_required_fields() {
        let agent_id = test_agent_id();
        let spec = test_spec();
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);

        // Metadata
        let meta = &pod.metadata;
        assert!(meta.name.is_some());
        assert_eq!(meta.namespace.as_deref(), Some("swarm-agents"));

        let labels = meta.labels.as_ref().unwrap();
        assert_eq!(labels.get("app"), Some(&"swarm-agent".to_string()));
        assert!(labels.contains_key("swarm.io/agent-id"));

        let annotations = meta.annotations.as_ref().unwrap();
        assert_eq!(
            annotations.get("swarm.io/agent-name"),
            Some(&"test-agent".to_string())
        );
        assert!(annotations.contains_key("swarm.io/pod-id"));
        assert!(annotations.contains_key("swarm.io/created-at"));

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
        assert!(env_names.contains(&"MACHINE_ID"));
        assert!(env_names.contains(&"AURA_MACHINE_ID"));
        assert!(env_names.contains(&"USER_ID"));
        assert!(env_names.contains(&"STATE_DIR"));
        assert!(env_names.contains(&"AURA_LISTEN_ADDR"));
        assert!(env_names.contains(&"CONTROL_PLANE_URL"));
        assert!(env_names.contains(&"AURA_LLM_ROUTING"));
        assert!(env_names.contains(&"AURA_ROUTER_URL"));
        assert!(env_names.contains(&"AURA_STORAGE_URL"));
        assert!(env_names.contains(&"AURA_NETWORK_URL"));
    }

    #[test]
    fn build_pod_uses_spec_resources() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            cpu_millicores: 1000,
            memory_mb: 2048,
            runtime_version: "v1.0".to_string(),
            isolation: None,
            ..test_spec()
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
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
        let spec = AgentSpec {
            isolation: None,
            ..test_spec()
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        assert_eq!(pod_spec.runtime_class_name.as_deref(), Some("kata-fc"));
    }

    #[test]
    fn build_pod_uses_agent_isolation_when_specified() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::Container),
            ..test_spec()
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        assert_eq!(pod_spec.runtime_class_name, None);
    }

    #[test]
    fn microvm_pod_runs_as_root_for_dev_env() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::MicroVM),
            ..test_spec()
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        let sec = pod_spec.security_context.as_ref().unwrap();
        assert_eq!(sec.run_as_non_root, Some(false));
        assert_eq!(sec.run_as_user, Some(0));
        assert_eq!(sec.fs_group, Some(1000));

        let container_sec = pod_spec.containers[0].security_context.as_ref().unwrap();
        assert_eq!(container_sec.run_as_user, Some(0));
        assert_eq!(container_sec.allow_privilege_escalation, Some(true));
    }

    #[test]
    fn container_pod_stays_non_root() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::Container),
            ..test_spec()
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        let sec = pod_spec.security_context.as_ref().unwrap();
        assert_eq!(sec.run_as_non_root, Some(true));
        assert_eq!(sec.run_as_user, Some(1000));

        let container_sec = pod_spec.containers[0].security_context.as_ref().unwrap();
        assert_eq!(container_sec.run_as_user, Some(1000));
        assert_eq!(container_sec.allow_privilege_escalation, Some(false));
    }

    /// CRITICAL dual-mode invariant (TEE upgrade R1): a legacy agent
    /// (tier == None, MicroVM isolation) must produce a pod spec identical
    /// to the pre-upgrade scheduler's output — no new env vars, node
    /// selectors, tolerations, or reordered fields. Any diff here would
    /// make the desired-state reconciler churn every legacy pod.
    ///
    /// The expected value below is a frozen snapshot of the pod spec shape
    /// as of the pre-TEE-upgrade scheduler. Do not update it to "fix" this
    /// test unless legacy pods are intentionally being changed (R2+).
    #[test]
    fn legacy_pod_spec_is_byte_for_byte_unchanged() {
        let agent_id = AgentId::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let agent_id_hex = agent_id.to_hex();
        let spec = test_spec(); // legacy: isolation None, tier None, no storage encryption
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let actual = serde_json::to_value(pod.spec.as_ref().unwrap()).unwrap();

        let env_var = |name: &str, value: &str| {
            serde_json::json!({ "name": name, "value": value })
        };
        let expected = serde_json::json!({
            "containers": [{
                "env": [
                    env_var("AGENT_ID", &agent_id_hex),
                    env_var("MACHINE_ID", &agent_id_hex),
                    env_var("AURA_MACHINE_ID", &agent_id_hex),
                    env_var("USER_ID", "user-hex"),
                    env_var("STATE_DIR", "/state"),
                    env_var("AURA_LISTEN_ADDR", "0.0.0.0:8080"),
                    env_var("CONTROL_PLANE_URL", "http://aura-swarm-gateway.swarm-system.svc:8080"),
                    env_var("AURA_DATA_DIR", "/state"),
                    env_var("AURA_LLM_ROUTING", "proxy"),
                    env_var("AURA_ROUTER_URL", "https://aura-router.onrender.com"),
                    env_var("AURA_STORAGE_URL", "https://aura-storage.onrender.com"),
                    env_var("AURA_NETWORK_URL", "https://aura-network.onrender.com"),
                    env_var("ENABLE_FS_TOOLS", "true"),
                    env_var("ENABLE_CMD_TOOLS", "true"),
                    env_var("AURA_PROJECT_BASE", "/home/aura"),
                ],
                "image": "ghcr.io/cypher-asi/aura-harness:latest",
                "livenessProbe": {
                    "failureThreshold": 3,
                    "httpGet": { "path": "/health", "port": 8080 },
                    "initialDelaySeconds": 30,
                    "periodSeconds": 30,
                    "timeoutSeconds": 10
                },
                "name": "aura",
                "ports": [{ "containerPort": 8080, "name": "http" }],
                "readinessProbe": {
                    "failureThreshold": 3,
                    "httpGet": { "path": "/health", "port": 8080 },
                    "initialDelaySeconds": 5,
                    "periodSeconds": 10,
                    "timeoutSeconds": 5
                },
                "resources": {
                    "limits": { "cpu": "500m", "memory": "512Mi" },
                    "requests": { "cpu": "500m", "memory": "512Mi" }
                },
                "securityContext": {
                    "allowPrivilegeEscalation": true,
                    "runAsNonRoot": false,
                    "runAsUser": 0
                },
                "volumeMounts": [{
                    "mountPath": "/state",
                    "name": "state",
                    "subPath": agent_id_hex
                }]
            }],
            "restartPolicy": "Always",
            "runtimeClassName": "kata-fc",
            "securityContext": {
                "fsGroup": 1000,
                "runAsNonRoot": false,
                "runAsUser": 0
            },
            "terminationGracePeriodSeconds": 30,
            "volumes": [{
                "name": "state",
                "persistentVolumeClaim": { "claimName": "swarm-agent-state" }
            }]
        });

        assert_eq!(
            actual, expected,
            "legacy pod spec drifted from the pre-TEE-upgrade snapshot"
        );
    }

    #[test]
    fn confidential_pod_gets_snp_wiring() {
        use aura_swarm_store::StorageEncryption;

        let agent_id = test_agent_id();
        let key_id = format!("swarm/agents/{agent_id}/state-key");
        let spec = AgentSpec {
            cpu_millicores: 1000,
            memory_mb: 2048,
            runtime_version: "latest".to_string(),
            isolation: Some(IsolationLevel::ConfidentialVM),
            tier: Some("standard".to_string()),
            storage_encryption: Some(StorageEncryption::Sealed {
                key_id: key_id.clone(),
            }),
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "tee-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        // Runtime class
        assert_eq!(
            pod_spec.runtime_class_name.as_deref(),
            Some("kata-qemu-snp")
        );

        // SNP node selector
        let selector = pod_spec.node_selector.as_ref().unwrap();
        assert_eq!(
            selector.get("swarm.io/confidential-node"),
            Some(&"true".to_string())
        );

        // Matching toleration
        let tolerations = pod_spec.tolerations.as_ref().unwrap();
        assert_eq!(tolerations.len(), 1);
        let tol = &tolerations[0];
        assert_eq!(tol.key.as_deref(), Some("swarm.io/confidential-node"));
        assert_eq!(tol.operator.as_deref(), Some("Equal"));
        assert_eq!(tol.value.as_deref(), Some("true"));
        assert_eq!(tol.effect.as_deref(), Some("NoSchedule"));

        // Confidential env vars, appended after the legacy set
        let env = pod_spec.containers[0].env.as_ref().unwrap();
        let get = |name: &str| {
            env.iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.as_deref())
        };
        assert_eq!(
            get("AURA_KBS_URL"),
            Some("http://kbs.swarm-system.svc.cluster.local:8080")
        );
        assert_eq!(get("AURA_STATE_ENCRYPTION"), Some("sealed"));
        assert_eq!(get("AURA_STATE_KEY_ID"), Some(key_id.as_str()));
        assert_eq!(
            env.last().map(|e| e.name.as_str()),
            Some("AURA_STATE_KEY_ID"),
            "confidential vars must be appended, not interleaved"
        );

        // Legacy env vars are still all present
        assert!(env.iter().any(|e| e.name == "AGENT_ID"));
        assert!(env.iter().any(|e| e.name == "CONTROL_PLANE_URL"));
    }

    #[test]
    fn confidential_pod_falls_back_to_deterministic_key_id() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            // No storage_encryption recorded: fall back to the
            // deterministic per-agent key id.
            ..test_spec()
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "tee-agent", &spec, &config);
        let env = pod.spec.as_ref().unwrap().containers[0].env.as_ref().unwrap();
        let key_id = env
            .iter()
            .find(|e| e.name == "AURA_STATE_KEY_ID")
            .and_then(|e| e.value.clone())
            .unwrap();

        assert_eq!(key_id, format!("swarm/agents/{agent_id}/state-key"));
    }

    #[test]
    fn confidential_pod_gets_internal_token_when_configured() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            ..test_spec()
        };
        let mut config = SchedulerConfig::default();
        config.gateway_token = "super-secret".to_string();

        let pod = build_pod(&agent_id, "user-hex", "tee-agent", &spec, &config);
        let env = pod.spec.as_ref().unwrap().containers[0].env.as_ref().unwrap();

        let token = env
            .iter()
            .find(|e| e.name == "AURA_SWARM_INTERNAL_TOKEN")
            .and_then(|e| e.value.as_deref());
        assert_eq!(token, Some("super-secret"));
        assert_eq!(
            env.last().map(|e| e.name.as_str()),
            Some("AURA_SWARM_INTERNAL_TOKEN"),
            "internal token must be appended after the other confidential vars"
        );

        // The trigger registrar's other inputs are already present under
        // their existing names.
        assert!(env.iter().any(|e| e.name == "CONTROL_PLANE_URL"));
        assert!(env.iter().any(|e| e.name == "AGENT_ID"));

        // Legacy pods must NOT get the token even when configured.
        let legacy_pod = build_pod(&agent_id, "user-hex", "legacy", &test_spec(), &config);
        let legacy_env = legacy_pod.spec.as_ref().unwrap().containers[0]
            .env
            .as_ref()
            .unwrap();
        assert!(
            !legacy_env
                .iter()
                .any(|e| e.name == "AURA_SWARM_INTERNAL_TOKEN"),
            "legacy pod env must stay byte-for-byte unchanged"
        );
    }

    #[test]
    fn confidential_pod_omits_internal_token_in_dev_mode() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            ..test_spec()
        };
        // Default config has no INTERNAL_TOKEN (dev mode).
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "tee-agent", &spec, &config);
        let env = pod.spec.as_ref().unwrap().containers[0].env.as_ref().unwrap();
        assert!(!env.iter().any(|e| e.name == "AURA_SWARM_INTERNAL_TOKEN"));
    }

    #[test]
    fn confidential_pod_uses_configured_kbs_url() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            ..test_spec()
        };
        let mut config = SchedulerConfig::default();
        config.kbs_url = "http://kbs.example:9999".to_string();

        let pod = build_pod(&agent_id, "user-hex", "tee-agent", &spec, &config);
        let env = pod.spec.as_ref().unwrap().containers[0].env.as_ref().unwrap();
        let kbs = env
            .iter()
            .find(|e| e.name == "AURA_KBS_URL")
            .and_then(|e| e.value.clone());

        assert_eq!(kbs.as_deref(), Some("http://kbs.example:9999"));
    }

    #[test]
    fn build_pod_respects_scheduler_default_isolation() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: None,
            ..test_spec()
        };
        let mut config = SchedulerConfig::default();
        config.default_isolation = IsolationLevel::Container;

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        assert_eq!(pod_spec.runtime_class_name, None);
    }
}
