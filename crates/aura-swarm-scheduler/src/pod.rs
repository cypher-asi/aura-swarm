//! Pod specification builder for Kubernetes.
//!
//! This module provides helpers to construct Kubernetes pod specs
//! for Aura agent pods with all necessary configuration.

use aura_swarm_core::AgentId;
use aura_swarm_store::{AgentSpec, IsolationLevel};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, HTTPGetAction,
    PersistentVolumeClaimVolumeSource, Pod, PodSecurityContext, PodSpec, Probe,
    ResourceRequirements, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::ObjectMeta;
use std::collections::BTreeMap;
use std::io::Write;

use base64::Engine as _;
use flate2::{write::GzEncoder, Compression};

use crate::SchedulerConfig;

/// The container port for the Aura runtime HTTP server.
const AURA_PORT: i32 = 8080;

/// Kata annotation carrying the gzip+base64 CoCo initdata. The kata-agent
/// inflates it inside the pod VM and writes `aa.toml`/`cdh.toml`, which point
/// the in-guest attestation agent + confidential-data-hub at the KBS. Without
/// it the CDH has no KBS endpoint and the harness's DEK fetch fails with
/// "[CDH] [ERROR]: Get Resource failed" (surfaced as a CDH 500) — the request
/// never even reaches the KBS, so the agent never opens sealed state.
const CC_INIT_DATA_ANNOTATION: &str = "io.katacontainers.config.hypervisor.cc_init_data";

/// Build the gzip+base64 CoCo `cc_init_data` payload that points the in-guest
/// attestation agent (`aa.toml`) and confidential-data-hub (`cdh.toml`) at the
/// KBS. `kbs_guest_url` must be VPC-routable from the pod VM (e.g. the KBS pod
/// IP), NOT the in-cluster ClusterIP DNS. Mirrors the initdata the proven
/// `deploy/05-peer-pods-smoke-test.sh` injects.
fn build_cc_init_data(kbs_guest_url: &str) -> Option<String> {
    // The `cc_kbc` KBC name selects the CoCo KBS attestation client; the AA and
    // CDH both target the same KBS URL. Single-quoted TOML strings are fine here
    // because a validated http(s) URL contains no single quotes.
    let toml = format!(
        "algorithm = \"sha384\"\n\
         version = \"0.1.0\"\n\
         \n\
         [data]\n\
         \"aa.toml\" = '''\n\
         [token_configs]\n\
         [token_configs.coco_as]\n\
         url = '{url}'\n\
         \n\
         [token_configs.kbs]\n\
         url = '{url}'\n\
         '''\n\
         \n\
         \"cdh.toml\" = '''\n\
         socket = 'unix:///run/confidential-containers/cdh.sock'\n\
         credentials = []\n\
         \n\
         [kbc]\n\
         name = 'cc_kbc'\n\
         url = '{url}'\n\
         '''\n",
        url = kbs_guest_url
    );

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(toml.as_bytes()).ok()?;
    let gzipped = encoder.finish().ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(gzipped))
}

/// Build a Kubernetes pod spec for an agent.
///
/// This creates a complete pod specification including:
/// - `kata-remote` runtime class for confidential VM isolation (Peer Pods)
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

    // KBS key id for the sealed-state DEK, as recorded in the agent's
    // spec. Only injected into confidential pods.
    let state_key_id = spec.storage_encryption.key_id().to_string();

    // The RuntimeClass (Some("kata-remote") for confidential pods, None for
    // container pods) also drives the containerd runtime-handler annotation in
    // the metadata, so resolve it here and thread it through.
    let isolation = spec.isolation.unwrap_or(config.default_isolation);
    let runtime_class = isolation.runtime_class();

    Pod {
        metadata: build_metadata(
            &pod_name,
            &agent_id_hex,
            agent_name,
            &pod_id,
            config,
            runtime_class,
        ),
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
    runtime_class: Option<&str>,
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

    // Confidential pods use a per-runtime (nydus guest-pull) snapshotter via the
    // `kata-remote` RuntimeClass. On containerd 1.7.x, runtime-level snapshotter
    // support is experimental and is ONLY engaged when the pod carries the
    // `io.containerd.cri.runtime-handler` annotation (value = the RuntimeClass
    // handler, which is the runtime class name here). Without it, containerd
    // unpacks the workload image into the DEFAULT (overlayfs) snapshotter at
    // pull time; the kata-remote runtime's nydus snapshotter then cannot find
    // the layer content at container create and the pod fails with
    // "error unpacking image ... content digest ...: not found"
    // (CreateContainerError) on every node, regardless of the guest-pull flags.
    // Container (dev) pods use the default runtime/snapshotter and need no hint.
    // Refs: containerd #8674, kata-containers #8407, nydus-snapshotter docs.
    if let Some(handler) = runtime_class {
        annotations.insert(
            "io.containerd.cri.runtime-handler".to_string(),
            handler.to_string(),
        );

        // Confidential (kata-remote / Peer Pods) agents attest and fetch their
        // sealed-state DEK through the in-guest CDH, which only knows where the
        // KBS is via this initdata. The guest-reachable KBS address is resolved
        // at schedule time (see K8sScheduler::resolve_kbs_guest_url) because the
        // pod VM cannot reach the ClusterIP `kbs_url`. If it is unset we emit no
        // annotation (the scheduler fails fast before this for confidential
        // pods), so non-confidential/dev pods and tests are unaffected.
        if let Some(kbs_guest_url) = config.kbs_guest_url.as_deref() {
            if let Some(init_data) = build_cc_init_data(kbs_guest_url) {
                annotations.insert(CC_INIT_DATA_ANNOTATION.to_string(), init_data);
            }
        }
    }

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
    // Confidential pods run as Peer Pods (CAA `kata-remote`): the per-agent
    // SEV-SNP pod VM is launched off-cluster, the kata shim runs on an
    // ordinary worker, and attestation happens via the in-guest CDH. There is
    // no on-node metal pool, so the only placement constraint is an OPTIONAL
    // node selector (`config.agent_node_selector`) used to pin the kata shim
    // onto a dedicated peer-pods worker pool (the clean `aura-swarm-tee-hosts`
    // node group) so new agents get their own kata.peerpods.io/vm headroom and
    // a clean guest-pull image cache. Container (dev-mode) pods get the default
    // runtime and never receive the selector.
    let runtime_class_name = isolation.runtime_class().map(str::to_string);
    let node_selector = if isolation == IsolationLevel::ConfidentialVM
        && !config.agent_node_selector.is_empty()
    {
        Some(config.agent_node_selector.clone())
    } else {
        None
    };

    PodSpec {
        runtime_class_name,
        node_selector,
        containers: vec![build_container(
            agent_id_hex,
            user_id_hex,
            spec,
            state_key_id,
            config,
            &config.image,
            isolation,
        )],
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
    // appended after the base env list.
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
/// - `ConfidentialVM` (Kata/QEMU SNP): the confidential guest kernel is
///   the multi-tenant boundary, so it is safe to give the user a
///   root-capable dev environment inside the VM — they can `apt install`,
///   manage services, and modify the OS. Runs as root (uid 0); `fs_group`
///   stays 1000 so the `/state` PVC (EFS access point owned by 1000)
///   remains group-accessible.
/// - `Container` (shared-kernel runc, dev-mode): root would be a
///   host-compromise risk in a multi-tenant cluster, so the locked-down
///   non-root posture is preserved.
fn build_security_context(isolation: IsolationLevel) -> PodSecurityContext {
    match isolation {
        IsolationLevel::ConfidentialVM => PodSecurityContext {
            run_as_non_root: Some(false),
            run_as_user: Some(0),
            // Set the primary GID explicitly so the effective user is "0:0".
            // Confidential agents run as Peer Pods with guest pull, where the
            // workload rootfs is materialized INSIDE the pod VM. If only
            // run_as_user is set, containerd resolves the bare uid by
            // host-mounting the (empty) guest-pull snapshot to read its GID from
            // /etc/passwd and fails CreateContainer with "mount callback failed
            // ... /etc/passwd: no such file or directory". Providing uid AND gid
            // makes containerd use them directly and skip that host-side lookup.
            run_as_group: Some(0),
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
/// Mirrors [`build_security_context`]: confidential agents run as root
/// with privilege escalation allowed (so `sudo` works for OS
/// modification), while runc-isolated agents stay non-root with
/// privilege escalation denied.
fn build_container_security_context(isolation: IsolationLevel) -> SecurityContext {
    match isolation {
        IsolationLevel::ConfidentialVM => SecurityContext {
            run_as_non_root: Some(false),
            run_as_user: Some(0),
            // Mirror the pod-level "0:0" so containerd never host-mounts the
            // guest-pull rootfs to resolve the uid's GID from /etc/passwd (which
            // fails under Peer Pods guest pull; see build_security_context).
            run_as_group: Some(0),
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
    use aura_swarm_store::{IsolationLevel, StorageEncryption};

    fn test_agent_id() -> AgentId {
        AgentId::generate()
    }

    /// A small confidential spec with the deterministic per-agent key id.
    /// `isolation: None` exercises the scheduler-default path
    /// (`ConfidentialVM` unless configured otherwise).
    fn test_spec(agent_id: &AgentId) -> AgentSpec {
        AgentSpec {
            cpu_millicores: 500,
            memory_mb: 512,
            runtime_version: "latest".to_string(),
            isolation: None,
            tier: "small".to_string(),
            storage_encryption: StorageEncryption::sealed_for(agent_id),
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
        let spec = test_spec(&agent_id);
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
        assert_eq!(
            pod_spec.runtime_class_name.as_deref(),
            Some("kata-remote")
        );
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
            ..test_spec(&agent_id)
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
            ..test_spec(&agent_id)
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        assert_eq!(
            pod_spec.runtime_class_name.as_deref(),
            Some("kata-remote")
        );
    }

    #[test]
    fn build_pod_uses_agent_isolation_when_specified() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::Container),
            ..test_spec(&agent_id)
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        assert_eq!(pod_spec.runtime_class_name, None);
    }

    #[test]
    fn confidential_pod_sets_containerd_runtime_handler_annotation() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            ..test_spec(&agent_id)
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let annotations = pod.metadata.annotations.as_ref().unwrap();

        // Required for containerd 1.7 runtime-level (nydus guest-pull) snapshotter
        // selection; without it the workload image unpacks to overlayfs and the
        // kata-remote runtime fails with "content digest ...: not found".
        assert_eq!(
            annotations.get("io.containerd.cri.runtime-handler"),
            Some(&"kata-remote".to_string())
        );
    }

    #[test]
    fn container_pod_omits_containerd_runtime_handler_annotation() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::Container),
            ..test_spec(&agent_id)
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let annotations = pod.metadata.annotations.as_ref().unwrap();

        // Container pods use the default runtime/snapshotter; no per-runtime hint.
        assert!(!annotations.contains_key("io.containerd.cri.runtime-handler"));
    }

    #[test]
    fn confidential_pod_runs_as_root_for_dev_env() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            ..test_spec(&agent_id)
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        let sec = pod_spec.security_context.as_ref().unwrap();
        assert_eq!(sec.run_as_non_root, Some(false));
        assert_eq!(sec.run_as_user, Some(0));
        // Numeric primary GID so the effective user is "0:0" and containerd does
        // not host-mount the guest-pull rootfs to resolve the GID from
        // /etc/passwd (which fails under Peer Pods guest pull).
        assert_eq!(sec.run_as_group, Some(0));
        assert_eq!(sec.fs_group, Some(1000));

        let container_sec = pod_spec.containers[0].security_context.as_ref().unwrap();
        assert_eq!(container_sec.run_as_user, Some(0));
        assert_eq!(container_sec.run_as_group, Some(0));
        assert_eq!(container_sec.allow_privilege_escalation, Some(true));
    }

    #[test]
    fn container_pod_stays_non_root() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::Container),
            ..test_spec(&agent_id)
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

    // The pre-TEE-upgrade byte-for-byte legacy pod snapshot test was
    // deleted in R3: the kata-fc/legacy pod shape can no longer be built
    // (its purpose — proving zero churn for legacy pods during the
    // dual-mode window — is fulfilled).

    #[test]
    fn confidential_pod_uses_kata_remote_without_node_scheduling() {
        let agent_id = test_agent_id();
        let key_id = format!("swarm/agents/{agent_id}/state-key");
        let spec = AgentSpec {
            cpu_millicores: 1000,
            memory_mb: 2048,
            runtime_version: "latest".to_string(),
            isolation: Some(IsolationLevel::ConfidentialVM),
            tier: "standard".to_string(),
            storage_encryption: StorageEncryption::Sealed {
                key_id: key_id.clone(),
            },
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "tee-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        // Confidential pods always run as Peer Pods (CAA kata-remote).
        assert_eq!(pod_spec.runtime_class_name.as_deref(), Some("kata-remote"));

        // No node selector / toleration: the per-agent SEV-SNP pod VM runs
        // off-cluster and the kata shim lands on an ordinary worker (there is
        // no on-node metal pool to target).
        assert!(
            pod_spec.node_selector.is_none(),
            "confidential pods must not carry an on-node SNP node selector"
        );
        assert!(
            pod_spec.tolerations.is_none(),
            "confidential pods must not carry an on-node SNP toleration"
        );

        // Confidential env vars, appended after the base set
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

        // Base env vars are still all present
        assert!(env.iter().any(|e| e.name == "AGENT_ID"));
        assert!(env.iter().any(|e| e.name == "CONTROL_PLANE_URL"));
    }

    #[test]
    fn confidential_pod_gets_agent_node_selector_when_configured() {
        let agent_id = test_agent_id();
        let key_id = format!("swarm/agents/{agent_id}/state-key");
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            storage_encryption: StorageEncryption::Sealed { key_id },
            ..test_spec(&agent_id)
        };
        let mut config = SchedulerConfig::default();
        config
            .agent_node_selector
            .insert("aura.swarm/pool".to_string(), "tee".to_string());

        let pod = build_pod(&agent_id, "user-hex", "tee-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        // The kata shim is pinned to the dedicated peer-pods pool.
        let sel = pod_spec
            .node_selector
            .as_ref()
            .expect("confidential pod should carry the configured node selector");
        assert_eq!(sel.get("aura.swarm/pool"), Some(&"tee".to_string()));
    }

    #[test]
    fn confidential_pod_injects_cc_init_data_when_kbs_guest_url_set() {
        use std::io::Read as _;

        let agent_id = test_agent_id();
        let key_id = format!("swarm/agents/{agent_id}/state-key");
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            storage_encryption: StorageEncryption::Sealed { key_id },
            ..test_spec(&agent_id)
        };
        let mut config = SchedulerConfig::default();
        config.kbs_guest_url = Some("http://10.0.3.169:8080".to_string());

        let pod = build_pod(&agent_id, "user-hex", "tee-agent", &spec, &config);
        let annotations = pod.metadata.annotations.as_ref().unwrap();
        let b64 = annotations
            .get(CC_INIT_DATA_ANNOTATION)
            .expect("confidential pod with a resolved KBS guest URL must carry cc_init_data");

        // It must be gzip+base64 of TOML that points the in-guest AA/CDH at the
        // resolved (VPC-routable) KBS address.
        let gz = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("cc_init_data must be valid base64");
        let mut toml = String::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_string(&mut toml)
            .expect("cc_init_data must be valid gzip");
        assert!(toml.contains("name = 'cc_kbc'"), "TOML: {toml}");
        assert!(
            toml.matches("url = 'http://10.0.3.169:8080'").count() >= 3,
            "KBS URL must be set for coco_as, kbs, and the cdh kbc: {toml}"
        );
    }

    #[test]
    fn confidential_pod_omits_cc_init_data_without_kbs_guest_url() {
        let agent_id = test_agent_id();
        let key_id = format!("swarm/agents/{agent_id}/state-key");
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            storage_encryption: StorageEncryption::Sealed { key_id },
            ..test_spec(&agent_id)
        };
        // Default config has kbs_guest_url = None (resolved only at schedule time).
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "tee-agent", &spec, &config);
        let annotations = pod.metadata.annotations.as_ref().unwrap();
        assert!(
            !annotations.contains_key(CC_INIT_DATA_ANNOTATION),
            "no initdata annotation should be emitted until the KBS guest URL is resolved"
        );
    }

    #[test]
    fn container_pod_never_gets_agent_node_selector() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::Container),
            ..test_spec(&agent_id)
        };
        let mut config = SchedulerConfig::default();
        config
            .agent_node_selector
            .insert("aura.swarm/pool".to_string(), "tee".to_string());

        let pod = build_pod(&agent_id, "user-hex", "dev-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        // Dev-mode (container) pods run on the default runtime and must not be
        // pinned to the confidential pool.
        assert!(pod_spec.runtime_class_name.is_none());
        assert!(pod_spec.node_selector.is_none());
    }

    #[test]
    fn confidential_pod_preserves_root_in_guest_and_kbs_env() {
        let agent_id = test_agent_id();
        let key_id = format!("swarm/agents/{agent_id}/state-key");
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            storage_encryption: StorageEncryption::Sealed {
                key_id: key_id.clone(),
            },
            ..test_spec(&agent_id)
        };
        let config = SchedulerConfig::default();

        let pod = build_pod(&agent_id, "user-hex", "tee-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        assert_eq!(pod_spec.runtime_class_name.as_deref(), Some("kata-remote"));

        // Root-in-guest security context is preserved (the guest is still
        // the boundary, only the VM's location moves).
        let pod_sec = pod_spec.security_context.as_ref().unwrap();
        assert_eq!(pod_sec.run_as_non_root, Some(false));
        assert_eq!(pod_sec.run_as_user, Some(0));
        let container_sec = pod_spec.containers[0].security_context.as_ref().unwrap();
        assert_eq!(container_sec.run_as_user, Some(0));
        assert_eq!(container_sec.allow_privilege_escalation, Some(true));

        // KBS/CDH env wiring is unchanged: attestation still runs via the
        // CDH inside the pod VM.
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
    }

    #[test]
    fn pod_uses_spec_recorded_key_id() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            ..test_spec(&agent_id)
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
            ..test_spec(&agent_id)
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

        // Container (dev-mode) pods must NOT get the token even when
        // configured: they have no attestation boot flow.
        let dev_spec = AgentSpec {
            isolation: Some(IsolationLevel::Container),
            ..test_spec(&agent_id)
        };
        let dev_pod = build_pod(&agent_id, "user-hex", "dev", &dev_spec, &config);
        let dev_env = dev_pod.spec.as_ref().unwrap().containers[0]
            .env
            .as_ref()
            .unwrap();
        assert!(
            !dev_env
                .iter()
                .any(|e| e.name == "AURA_SWARM_INTERNAL_TOKEN"),
            "container pod env must not carry the confidential vars"
        );
    }

    #[test]
    fn confidential_pod_omits_internal_token_in_dev_mode() {
        let agent_id = test_agent_id();
        let spec = AgentSpec {
            isolation: Some(IsolationLevel::ConfidentialVM),
            ..test_spec(&agent_id)
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
            ..test_spec(&agent_id)
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
            ..test_spec(&agent_id)
        };
        let mut config = SchedulerConfig::default();
        config.default_isolation = IsolationLevel::Container;

        let pod = build_pod(&agent_id, "user-hex", "test-agent", &spec, &config);
        let pod_spec = pod.spec.as_ref().unwrap();

        assert_eq!(pod_spec.runtime_class_name, None);
    }
}
