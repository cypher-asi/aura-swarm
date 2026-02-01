//! Kubernetes scheduler implementation.
//!
//! This module provides the `K8sScheduler` which manages agent pods in a
//! Kubernetes cluster using the Kata Containers runtime for microVM isolation.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use kube::runtime::watcher::{self, watcher, Config as WatcherConfig};
use kube::Client;
use serde::Serialize;
use tracing::{error, info, warn};

use aura_swarm_core::AgentId;
use aura_swarm_store::{AgentSpec, AgentState};

use crate::cache::EndpointCache;
use crate::pod::{build_pod, pod_name_for_agent};
use crate::types::{PodInfo, PodPhase, PodStatus, SchedulerConfig};
use crate::{Result, SchedulerError};

/// The `Scheduler` trait defines the interface for pod lifecycle management.
#[async_trait]
pub trait Scheduler: Send + Sync {
    /// Schedule a new agent pod in the cluster.
    ///
    /// # Errors
    ///
    /// Returns an error if pod creation fails.
    async fn schedule_agent(
        &self,
        agent_id: &AgentId,
        user_id_hex: &str,
        spec: &AgentSpec,
    ) -> Result<()>;

    /// Terminate an agent pod.
    ///
    /// # Errors
    ///
    /// Returns an error if pod deletion fails (except 404).
    async fn terminate_agent(&self, agent_id: &AgentId) -> Result<()>;

    /// Get the current status of an agent's pod.
    ///
    /// # Errors
    ///
    /// Returns an error if the pod status cannot be retrieved.
    async fn get_pod_status(&self, agent_id: &AgentId) -> Result<PodStatus>;

    /// Get the endpoint (IP:port) for an agent's pod, if running.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint cannot be determined.
    async fn get_pod_endpoint(&self, agent_id: &AgentId) -> Result<Option<String>>;

    /// List all pods managed by this scheduler.
    ///
    /// # Errors
    ///
    /// Returns an error if listing fails.
    async fn list_pods(&self) -> Result<Vec<PodInfo>>;

    /// Check if an agent's pod is healthy.
    ///
    /// # Errors
    ///
    /// Returns an error if the health check fails.
    async fn check_agent_health(&self, agent_id: &AgentId) -> Result<bool>;
}

/// Kubernetes-based scheduler for agent pods.
///
/// This scheduler creates and manages pods in a Kubernetes cluster,
/// using Kata Containers with Firecracker for microVM isolation.
pub struct K8sScheduler {
    client: Client,
    config: SchedulerConfig,
    endpoint_cache: EndpointCache,
    http_client: reqwest::Client,
}

impl K8sScheduler {
    /// Create a new Kubernetes scheduler.
    ///
    /// This will attempt to connect to the cluster using in-cluster config
    /// or kubeconfig file.
    ///
    /// # Errors
    ///
    /// Returns an error if the Kubernetes client cannot be created.
    pub async fn new(config: SchedulerConfig) -> Result<Self> {
        let client = Client::try_default().await?;

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| SchedulerError::Config(format!("Failed to create HTTP client: {e}")))?;

        Ok(Self {
            client,
            config,
            endpoint_cache: EndpointCache::new(),
            http_client,
        })
    }

    /// Create a new scheduler with a pre-configured client.
    ///
    /// This is useful for testing with mock clients.
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client cannot be created (should never happen with default TLS).
    #[must_use]
    pub fn with_client(client: Client, config: SchedulerConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            config,
            endpoint_cache: EndpointCache::new(),
            http_client,
        }
    }

    /// Get a reference to the scheduler config.
    #[must_use]
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Get the pods API client for the configured namespace.
    fn pods_api(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.config.namespace)
    }

    /// Run the reconciliation loop, watching for pod changes and notifying the gateway.
    ///
    /// This method runs indefinitely, processing pod events as they occur.
    /// It should be spawned as a background task.
    ///
    /// Status updates are sent to the gateway's internal endpoint via HTTP.
    pub async fn run_reconciler(&self) {
        let pods = self.pods_api();
        let config = WatcherConfig::default().labels("app=swarm-agent");

        let watch = watcher(pods, config);

        futures::pin_mut!(watch);

        info!(
            namespace = %self.config.namespace,
            gateway_url = %self.config.gateway_url,
            "Starting pod reconciliation loop"
        );

        while let Some(event) = watch.next().await {
            match event {
                Ok(watcher::Event::Apply(pod) | watcher::Event::InitApply(pod)) => {
                    self.handle_pod_update(&pod).await;
                }
                Ok(watcher::Event::Delete(pod)) => {
                    self.handle_pod_deleted(&pod).await;
                }
                Ok(watcher::Event::Init) => {
                    info!("Watcher initialized, starting reconciliation");
                }
                Ok(watcher::Event::InitDone) => {
                    info!("Initial reconciliation complete");
                }
                Err(e) => {
                    error!(error = %e, "Watcher error, will retry");
                    // The watcher will automatically reconnect
                }
            }
        }

        warn!("Reconciliation loop exited unexpectedly");
    }

    async fn handle_pod_update(&self, pod: &Pod) {
        let Some(agent_id) = Self::extract_agent_id(pod) else {
            return;
        };

        let phase = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Unknown");

        let ready = Self::is_pod_ready(pod);

        // Update endpoint cache if we have an IP
        if let Some(ip) = pod.status.as_ref().and_then(|s| s.pod_ip.as_ref()) {
            self.endpoint_cache.insert(agent_id, format!("{ip}:8080"));
        }

        // Map pod phase to agent state
        let new_state = match (phase, ready) {
            ("Running", true) => AgentState::Running,
            ("Running", false) | ("Pending", _) => AgentState::Provisioning,
            ("Failed", _) => AgentState::Error,
            ("Succeeded", _) => AgentState::Stopped,
            _ => return, // Don't update for unknown states
        };

        // Notify the gateway of the status change
        if let Err(e) = self.notify_status_change(&agent_id, new_state, None).await {
            error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to notify gateway of status change"
            );
        } else {
            info!(
                agent_id = %agent_id,
                phase,
                ready,
                new_state = ?new_state,
                "Notified gateway of agent status change"
            );
        }
    }

    async fn handle_pod_deleted(&self, pod: &Pod) {
        let Some(agent_id) = Self::extract_agent_id(pod) else {
            return;
        };

        // Remove from endpoint cache
        self.endpoint_cache.remove(&agent_id);

        // Notify gateway that pod is deleted (transition to Stopped)
        // Note: The gateway will check if agent is hibernating and skip if so
        if let Err(e) = self
            .notify_status_change(&agent_id, AgentState::Stopped, Some("Pod deleted".to_string()))
            .await
        {
            error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to notify gateway of pod deletion"
            );
        } else {
            info!(agent_id = %agent_id, "Notified gateway of pod deletion");
        }
    }

    /// Notify the gateway of an agent status change via HTTP.
    async fn notify_status_change(
        &self,
        agent_id: &AgentId,
        status: AgentState,
        message: Option<String>,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct StatusUpdate {
            status: AgentState,
            #[serde(skip_serializing_if = "Option::is_none")]
            message: Option<String>,
        }

        let url = format!(
            "{}/internal/agents/{}/status",
            self.config.gateway_url,
            agent_id.to_hex()
        );

        let body = StatusUpdate { status, message };

        let response = self
            .http_client
            .patch(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| SchedulerError::Config(format!("Failed to call gateway: {e}")))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let status_code = response.status();
            let error_text = response.text().await.unwrap_or_default();
            Err(SchedulerError::Config(format!(
                "Gateway returned {status_code}: {error_text}"
            )))
        }
    }

    fn extract_agent_id(pod: &Pod) -> Option<AgentId> {
        // Try to get full agent ID from annotation first (preferred, no truncation)
        // Fall back to label for backwards compatibility
        let agent_id_hex = pod
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("swarm.io/agent-id-full"))
            .or_else(|| {
                pod.metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("swarm.io/agent-id"))
            })?;

        match AgentId::from_hex(agent_id_hex) {
            Ok(id) => Some(id),
            Err(e) => {
                warn!(
                    agent_id_hex,
                    error = %e,
                    "Invalid agent ID in pod label/annotation"
                );
                None
            }
        }
    }

    fn is_pod_ready(pod: &Pod) -> bool {
        pod.status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|c| c.type_ == "Ready" && c.status == "True")
            })
    }

    fn extract_pod_status(pod: &Pod) -> PodStatus {
        let status = pod.status.as_ref();

        let phase = status
            .and_then(|s| s.phase.as_deref())
            .map(PodPhase::from_k8s_phase)
            .unwrap_or_default();

        let ready = Self::is_pod_ready(pod);

        let restart_count = status
            .and_then(|s| s.container_statuses.as_ref())
            .and_then(|cs| cs.first())
            .map_or(0, |c| c.restart_count.unsigned_abs());

        let started_at = status.and_then(|s| s.start_time.as_ref()).map(|t| t.0);

        let message = status.and_then(|s| s.message.clone());

        PodStatus {
            phase,
            ready,
            restart_count,
            started_at,
            message,
        }
    }
}

#[async_trait]
impl Scheduler for K8sScheduler {
    async fn schedule_agent(
        &self,
        agent_id: &AgentId,
        user_id_hex: &str,
        spec: &AgentSpec,
    ) -> Result<()> {
        // Validate resources
        self.config
            .validate_resources(spec.cpu_millicores, spec.memory_mb)?;

        let pods = self.pods_api();
        let pod_name = pod_name_for_agent(agent_id);

        // Check if pod already exists
        if pods.get_opt(&pod_name).await?.is_some() {
            warn!(
                agent_id = %agent_id,
                pod_name,
                "Pod already exists, skipping creation"
            );
            return Ok(());
        }

        // Build and create the pod
        let pod = build_pod(agent_id, user_id_hex, spec, &self.config);
        pods.create(&PostParams::default(), &pod).await?;

        info!(
            agent_id = %agent_id,
            pod_name,
            cpu = spec.cpu_millicores,
            memory = spec.memory_mb,
            "Created agent pod"
        );

        Ok(())
    }

    async fn terminate_agent(&self, agent_id: &AgentId) -> Result<()> {
        let pods = self.pods_api();
        let pod_name = pod_name_for_agent(agent_id);

        // Remove from endpoint cache
        self.endpoint_cache.remove(agent_id);

        match pods.delete(&pod_name, &DeleteParams::default()).await {
            Ok(_) => {
                info!(agent_id = %agent_id, pod_name, "Terminated agent pod");
                Ok(())
            }
            Err(kube::Error::Api(e)) if e.code == 404 => {
                warn!(pod_name, "Pod not found, already terminated");
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn get_pod_status(&self, agent_id: &AgentId) -> Result<PodStatus> {
        let pods = self.pods_api();
        let pod_name = pod_name_for_agent(agent_id);

        match pods.get_opt(&pod_name).await? {
            Some(pod) => Ok(Self::extract_pod_status(&pod)),
            None => Err(SchedulerError::PodNotFound(pod_name)),
        }
    }

    async fn get_pod_endpoint(&self, agent_id: &AgentId) -> Result<Option<String>> {
        // Check cache first
        if let Some(endpoint) = self.endpoint_cache.get(agent_id) {
            return Ok(Some(endpoint));
        }

        // Fetch from K8s
        let pods = self.pods_api();
        let pod_name = pod_name_for_agent(agent_id);

        if let Some(pod) = pods.get_opt(&pod_name).await? {
            if let Some(ip) = pod.status.as_ref().and_then(|s| s.pod_ip.as_ref()) {
                let endpoint = format!("{ip}:8080");

                // Update cache
                self.endpoint_cache.insert(*agent_id, endpoint.clone());

                return Ok(Some(endpoint));
            }
        }

        Ok(None)
    }

    async fn list_pods(&self) -> Result<Vec<PodInfo>> {
        let pods = self.pods_api();
        let params = ListParams::default().labels("app=swarm-agent");

        let pod_list = pods.list(&params).await?;
        let mut result = Vec::with_capacity(pod_list.items.len());

        for pod in pod_list.items {
            let Some(agent_id) = Self::extract_agent_id(&pod) else {
                continue;
            };

            let pod_name = pod
                .metadata
                .name
                .clone()
                .unwrap_or_else(|| "unknown".to_string());

            let node_name = pod.spec.as_ref().and_then(|s| s.node_name.clone());
            let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.clone());
            let status = Self::extract_pod_status(&pod);

            result.push(PodInfo {
                agent_id,
                pod_name,
                node_name,
                pod_ip,
                status,
            });
        }

        Ok(result)
    }

    async fn check_agent_health(&self, agent_id: &AgentId) -> Result<bool> {
        let Some(endpoint) = self.get_pod_endpoint(agent_id).await? else {
            return Ok(false);
        };

        let url = format!("http://{endpoint}/health");

        match self.http_client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => Ok(true),
            Ok(resp) => {
                warn!(
                    agent_id = %agent_id,
                    status = %resp.status(),
                    "Health check returned non-success status"
                );
                Ok(false)
            }
            Err(e) => {
                warn!(
                    agent_id = %agent_id,
                    error = %e,
                    "Health check request failed"
                );
                Ok(false)
            }
        }
    }
}

/// A mock scheduler for testing without a real Kubernetes cluster.
#[cfg(any(test, feature = "test-utils"))]
pub mod mock {
    use super::*;
    use chrono::Utc;
    use parking_lot::Mutex;
    use std::collections::HashMap;

    /// A mock scheduler that stores pods in memory.
    #[derive(Default)]
    pub struct MockScheduler {
        pods: Mutex<HashMap<AgentId, MockPod>>,
    }

    struct MockPod {
        user_id_hex: String,
        spec: AgentSpec,
        status: PodStatus,
        endpoint: Option<String>,
    }

    impl MockScheduler {
        /// Create a new mock scheduler.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Set the endpoint for a pod.
        pub fn set_endpoint(&self, agent_id: &AgentId, endpoint: Option<String>) {
            if let Some(pod) = self.pods.lock().get_mut(agent_id) {
                pod.endpoint = endpoint;
            }
        }

        /// Set the status for a pod.
        pub fn set_status(&self, agent_id: &AgentId, status: PodStatus) {
            if let Some(pod) = self.pods.lock().get_mut(agent_id) {
                pod.status = status;
            }
        }

        /// Get the number of scheduled pods.
        #[must_use]
        pub fn pod_count(&self) -> usize {
            self.pods.lock().len()
        }

        /// Get the spec for a pod.
        #[must_use]
        pub fn get_spec(&self, agent_id: &AgentId) -> Option<AgentSpec> {
            self.pods.lock().get(agent_id).map(|p| p.spec.clone())
        }

        /// Get the user ID for a pod.
        #[must_use]
        pub fn get_user_id(&self, agent_id: &AgentId) -> Option<String> {
            self.pods
                .lock()
                .get(agent_id)
                .map(|p| p.user_id_hex.clone())
        }
    }

    #[async_trait]
    impl Scheduler for MockScheduler {
        async fn schedule_agent(
            &self,
            agent_id: &AgentId,
            user_id_hex: &str,
            spec: &AgentSpec,
        ) -> Result<()> {
            let mut pods = self.pods.lock();

            if pods.contains_key(agent_id) {
                return Ok(());
            }

            pods.insert(
                agent_id.clone(),
                MockPod {
                    user_id_hex: user_id_hex.to_string(),
                    spec: spec.clone(),
                    status: PodStatus {
                        phase: PodPhase::Pending,
                        ready: false,
                        restart_count: 0,
                        started_at: Some(Utc::now()),
                        message: None,
                    },
                    endpoint: None,
                },
            );

            Ok(())
        }

        async fn terminate_agent(&self, agent_id: &AgentId) -> Result<()> {
            self.pods.lock().remove(agent_id);
            Ok(())
        }

        async fn get_pod_status(&self, agent_id: &AgentId) -> Result<PodStatus> {
            self.pods
                .lock()
                .get(agent_id)
                .map(|p| p.status.clone())
                .ok_or_else(|| SchedulerError::PodNotFound(agent_id.to_hex()))
        }

        async fn get_pod_endpoint(&self, agent_id: &AgentId) -> Result<Option<String>> {
            Ok(self
                .pods
                .lock()
                .get(agent_id)
                .and_then(|p| p.endpoint.clone()))
        }

        async fn list_pods(&self) -> Result<Vec<PodInfo>> {
            let pods = self.pods.lock();
            Ok(pods
                .iter()
                .map(|(agent_id, pod)| PodInfo {
                    agent_id: agent_id.clone(),
                    pod_name: pod_name_for_agent(agent_id),
                    node_name: Some("mock-node".to_string()),
                    pod_ip: pod
                        .endpoint
                        .as_ref()
                        .map(|e| e.split(':').next().unwrap_or("10.0.0.1").to_string()),
                    status: pod.status.clone(),
                })
                .collect())
        }

        async fn check_agent_health(&self, agent_id: &AgentId) -> Result<bool> {
            Ok(self
                .pods
                .lock()
                .get(agent_id)
                .map(|p| p.status.ready)
                .unwrap_or(false))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockScheduler;
    use super::*;
    use aura_swarm_core::UserId;

    fn test_agent_id() -> AgentId {
        let user_id = UserId::from_bytes([1u8; 32]);
        AgentId::generate(&user_id, "test-agent")
    }

    fn test_spec() -> AgentSpec {
        AgentSpec::default()
    }

    #[tokio::test]
    async fn mock_scheduler_schedule_and_terminate() {
        let scheduler = MockScheduler::new();
        let agent_id = test_agent_id();
        let user_id = UserId::from_bytes([1u8; 32]);
        let spec = test_spec();

        // Schedule
        scheduler
            .schedule_agent(&agent_id, &user_id.to_hex(), &spec)
            .await
            .unwrap();
        assert_eq!(scheduler.pod_count(), 1);

        // Status should be pending
        let status = scheduler.get_pod_status(&agent_id).await.unwrap();
        assert_eq!(status.phase, PodPhase::Pending);
        assert!(!status.ready);

        // Terminate
        scheduler.terminate_agent(&agent_id).await.unwrap();
        assert_eq!(scheduler.pod_count(), 0);

        // Status should error
        assert!(scheduler.get_pod_status(&agent_id).await.is_err());
    }

    #[tokio::test]
    async fn mock_scheduler_idempotent_schedule() {
        let scheduler = MockScheduler::new();
        let agent_id = test_agent_id();
        let user_id = UserId::from_bytes([1u8; 32]);
        let spec = test_spec();

        // Schedule twice
        scheduler
            .schedule_agent(&agent_id, &user_id.to_hex(), &spec)
            .await
            .unwrap();
        scheduler
            .schedule_agent(&agent_id, &user_id.to_hex(), &spec)
            .await
            .unwrap();

        // Should still be 1 pod
        assert_eq!(scheduler.pod_count(), 1);
    }

    #[tokio::test]
    async fn mock_scheduler_endpoint() {
        let scheduler = MockScheduler::new();
        let agent_id = test_agent_id();
        let user_id = UserId::from_bytes([1u8; 32]);
        let spec = test_spec();

        scheduler
            .schedule_agent(&agent_id, &user_id.to_hex(), &spec)
            .await
            .unwrap();

        // No endpoint initially
        assert!(scheduler
            .get_pod_endpoint(&agent_id)
            .await
            .unwrap()
            .is_none());

        // Set endpoint
        scheduler.set_endpoint(&agent_id, Some("10.0.0.5:8080".to_string()));
        assert_eq!(
            scheduler.get_pod_endpoint(&agent_id).await.unwrap(),
            Some("10.0.0.5:8080".to_string())
        );
    }

    #[tokio::test]
    async fn mock_scheduler_list_pods() {
        let scheduler = MockScheduler::new();
        let user_id = UserId::from_bytes([1u8; 32]);
        let spec = test_spec();

        let agent1 = AgentId::generate(&user_id, "agent-1");
        let agent2 = AgentId::generate(&user_id, "agent-2");

        scheduler
            .schedule_agent(&agent1, &user_id.to_hex(), &spec)
            .await
            .unwrap();
        scheduler
            .schedule_agent(&agent2, &user_id.to_hex(), &spec)
            .await
            .unwrap();

        let pods = scheduler.list_pods().await.unwrap();
        assert_eq!(pods.len(), 2);
    }
}
