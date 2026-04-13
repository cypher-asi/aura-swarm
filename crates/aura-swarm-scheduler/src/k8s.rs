//! Kubernetes scheduler implementation.
//!
//! This module provides the `K8sScheduler` which manages agent pods in a
//! Kubernetes cluster using the Kata Containers runtime for microVM isolation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Event, Pod};
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use kube::runtime::watcher::{self, watcher, Config as WatcherConfig};
use kube::Client;
use serde::Serialize;
use tracing::{debug, error, info, warn};

use aura_swarm_core::AgentId;
use aura_swarm_store::{AgentSpec, AgentState};

use crate::billing::ComputeUsageReporter;
use crate::cache::{EndpointCache, StateCache};
use crate::pod::build_pod;
use crate::types::{ActiveAgentInfo, PodInfo, PodPhase, PodStatus, SchedulerConfig};
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
        agent_name: &str,
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
    state_cache: StateCache,
    http_client: reqwest::Client,
    /// Optional billing reporter for compute usage tracking.
    billing_reporter: Option<Arc<ComputeUsageReporter>>,
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
            state_cache: StateCache::new(),
            http_client,
            billing_reporter: None,
        })
    }

    /// Create a new scheduler with billing integration.
    ///
    /// # Errors
    ///
    /// Returns an error if the Kubernetes client cannot be created.
    pub async fn with_billing(
        config: SchedulerConfig,
        billing_reporter: Arc<ComputeUsageReporter>,
    ) -> Result<Self> {
        let mut scheduler = Self::new(config).await?;
        scheduler.billing_reporter = Some(billing_reporter);
        Ok(scheduler)
    }

    /// Create a new scheduler with a pre-configured client.
    ///
    /// This is useful for testing with mock clients.
    ///
    /// # Errors
    ///
    /// Returns `SchedulerError::Config` if the HTTP client cannot be created.
    pub fn with_client(client: Client, config: SchedulerConfig) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| SchedulerError::Config(format!("Failed to create HTTP client: {e}")))?;

        Ok(Self {
            client,
            config,
            endpoint_cache: EndpointCache::new(),
            state_cache: StateCache::new(),
            http_client,
            billing_reporter: None,
        })
    }

    /// Set the billing reporter after construction.
    pub fn set_billing_reporter(&mut self, reporter: Arc<ComputeUsageReporter>) {
        self.billing_reporter = Some(reporter);
    }

    /// Get the billing reporter if configured.
    #[must_use]
    pub fn billing_reporter(&self) -> Option<&Arc<ComputeUsageReporter>> {
        self.billing_reporter.as_ref()
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

    /// Get the events API client for the configured namespace.
    fn events_api(&self) -> Api<Event> {
        Api::namespaced(self.client.clone(), &self.config.namespace)
    }

    /// Find a pod belonging to an agent by label selector.
    async fn find_pod_by_agent(&self, agent_id: &AgentId) -> Result<Option<Pod>> {
        let pods = self.pods_api();
        let label = format!("swarm.io/agent-id={}", agent_id.to_hex());
        let params = ListParams::default().labels(&label);
        let list = pods.list(&params).await?;
        Ok(list.items.into_iter().next())
    }

    /// Find the pod name for an agent by label selector.
    async fn find_pod_name_by_agent(&self, agent_id: &AgentId) -> Result<Option<String>> {
        Ok(self
            .find_pod_by_agent(agent_id)
            .await?
            .and_then(|p| p.metadata.name))
    }

    /// Run the reconciliation loop, watching for pod changes and notifying the gateway.
    ///
    /// This method runs indefinitely, processing pod events as they occur.
    /// It should be spawned as a background task.
    ///
    /// Status updates are sent to the gateway's internal endpoint via HTTP.
    pub async fn run_reconciler(&self) {
        tokio::join!(
            self.run_pod_watcher(),
            self.run_event_watcher(),
            self.run_desired_state_reconciler(),
        );
    }

    /// Watch for pod changes.
    async fn run_pod_watcher(&self) {
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
                    info!("Initial pod watch complete, triggering desired-state sync");
                    self.reconcile_desired_state_once().await;
                }
                Err(e) => {
                    error!(error = %e, "Watcher error, will retry");
                    // The watcher will automatically reconnect
                }
            }
        }

        warn!("Pod reconciliation loop exited unexpectedly");
    }

    /// Watch for Kubernetes Events (Warning events indicate errors).
    async fn run_event_watcher(&self) {
        let events = self.events_api();
        // Watch events for Pods in our namespace
        let config = WatcherConfig::default().fields("involvedObject.kind=Pod");

        let watch = watcher(events, config);

        futures::pin_mut!(watch);

        info!(
            namespace = %self.config.namespace,
            "Starting event watcher for pod errors"
        );

        while let Some(event) = watch.next().await {
            match event {
                Ok(watcher::Event::Apply(k8s_event) | watcher::Event::InitApply(k8s_event)) => {
                    self.handle_k8s_event(&k8s_event).await;
                }
                Ok(watcher::Event::Init | watcher::Event::InitDone | watcher::Event::Delete(_)) => {
                    // Ignore init and delete events for events
                }
                Err(e) => {
                    debug!(error = %e, "Event watcher error, will retry");
                }
            }
        }

        warn!("Event watcher loop exited unexpectedly");
    }

    /// Handle a Kubernetes Event (check for pod creation errors).
    async fn handle_k8s_event(&self, event: &Event) {
        // Only process Warning events
        let event_type = event.type_.as_deref().unwrap_or("Normal");
        if event_type != "Warning" {
            return;
        }

        // Get the involved pod name
        let involved = &event.involved_object;
        let Some(ref pod_name) = involved.name else {
            return;
        };

        // Only process events for our agent pods
        if !pod_name.starts_with("agent-") {
            return;
        }

        let reason = event.reason.as_deref().unwrap_or("Unknown");
        let message = event.message.as_deref().unwrap_or("No message");

        // Check for critical errors that should transition to Error state
        let error_reasons = [
            "FailedCreatePodSandBox",
            "FailedMount",
            "FailedScheduling",
            "FailedAttachVolume",
            "NetworkNotReady",
        ];

        if error_reasons.contains(&reason) {
            warn!(
                pod_name,
                reason, message, "Pod creation error detected from event"
            );

            // Pod name is truncated (agent-{first 16 hex chars}), so we need to
            // fetch the pod to get the full agent ID from the annotation
            let agent_id = match self.pods_api().get_opt(pod_name).await {
                Ok(Some(pod)) => Self::extract_agent_id(&pod),
                Ok(None) => {
                    debug!(pod_name, "Pod not found when handling error event");
                    None
                }
                Err(e) => {
                    debug!(pod_name, error = %e, "Failed to fetch pod for error event");
                    None
                }
            };

            if let Some(agent_id) = agent_id {
                let error_msg = format!("{reason}: {message}");
                if let Err(e) = self
                    .notify_status_change(&agent_id, AgentState::Error, Some(error_msg))
                    .await
                {
                    error!(
                        agent_id = %agent_id,
                        error = %e,
                        "Failed to notify gateway of pod error from event"
                    );
                } else {
                    info!(
                        agent_id = %agent_id,
                        reason,
                        "Notified gateway of pod error from Kubernetes event"
                    );
                }
            }
        }
    }

    /// Periodically reconcile desired agent state against actual pods.
    ///
    /// Runs every 30 seconds. For each cycle it queries the gateway for agents
    /// that should be running, lists actual pods, and:
    /// - Creates pods for agents that are missing one
    /// - Deletes (one at a time) pods running a stale container image so the
    ///   next cycle recreates them with the current image
    async fn run_desired_state_reconciler(&self) {
        // Give the pod watcher time to populate before the first periodic tick;
        // the initial sync is already handled via InitDone.
        tokio::time::sleep(Duration::from_secs(60)).await;

        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            interval.tick().await;
            self.reconcile_desired_state_once().await;
        }
    }

    /// Run a single desired-state reconciliation pass.
    async fn reconcile_desired_state_once(&self) {
        let active_agents = match self.fetch_active_agents().await {
            Ok(agents) => agents,
            Err(e) => {
                warn!(error = %e, "Desired-state reconciler: failed to fetch active agents from gateway");
                return;
            }
        };

        if active_agents.is_empty() {
            debug!("Desired-state reconciler: no active agents reported by gateway");
            return;
        }

        let pods = self.pods_api();
        let params = ListParams::default().labels("app=swarm-agent");
        let pod_list = match pods.list(&params).await {
            Ok(list) => list,
            Err(e) => {
                warn!(error = %e, "Desired-state reconciler: failed to list pods");
                return;
            }
        };

        // Build a map of agent_id_hex -> pod for currently existing pods
        let mut pod_by_agent: HashMap<String, Pod> = HashMap::new();
        for pod in pod_list.items {
            if let Some(agent_id) = Self::extract_agent_id(&pod) {
                pod_by_agent.insert(agent_id.to_hex(), pod);
            }
        }

        let mut created = 0u32;
        let mut stale_deleted = false;

        for agent_info in &active_agents {
            let agent_id = match AgentId::from_hex(&agent_info.agent_id) {
                Ok(id) => id,
                Err(_) => continue,
            };

            match pod_by_agent.get(&agent_info.agent_id) {
                None => {
                    // Pod is missing -- recreate it
                    info!(
                        agent_id = %agent_info.agent_id,
                        name = %agent_info.name,
                        "Desired-state reconciler: recreating missing pod"
                    );
                    if let Err(e) = self
                        .schedule_agent(
                            &agent_id,
                            &agent_info.user_id,
                            &agent_info.name,
                            &agent_info.spec,
                        )
                        .await
                    {
                        error!(
                            agent_id = %agent_info.agent_id,
                            error = %e,
                            "Desired-state reconciler: failed to recreate pod"
                        );
                    } else {
                        created += 1;
                    }
                }
                Some(pod) => {
                    // Pod exists -- check for image drift (rolling, one at a time)
                    if !stale_deleted {
                        if let Some(true) = self.pod_has_stale_image(pod) {
                            let pod_name = pod
                                .metadata
                                .name
                                .as_deref()
                                .unwrap_or("unknown");
                            info!(
                                agent_id = %agent_info.agent_id,
                                pod_name,
                                expected_image = %self.config.image,
                                "Desired-state reconciler: deleting pod with stale image"
                            );
                            if let Err(e) = self.terminate_agent(&agent_id).await {
                                error!(
                                    agent_id = %agent_info.agent_id,
                                    error = %e,
                                    "Desired-state reconciler: failed to delete stale pod"
                                );
                            } else {
                                stale_deleted = true;
                            }
                        }
                    }
                }
            }
        }

        if created > 0 || stale_deleted {
            info!(
                created,
                stale_deleted,
                "Desired-state reconciler: pass complete"
            );
        } else {
            debug!("Desired-state reconciler: all pods converged");
        }
    }

    /// Fetch the list of agents that should have running pods from the gateway.
    async fn fetch_active_agents(&self) -> Result<Vec<ActiveAgentInfo>> {
        let url = format!("{}/internal/agents/active", self.config.gateway_url);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|e| SchedulerError::Config(format!("Failed to fetch active agents: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(SchedulerError::Config(format!(
                "Gateway returned {status} for /internal/agents/active"
            )));
        }

        response
            .json::<Vec<ActiveAgentInfo>>()
            .await
            .map_err(|e| SchedulerError::Config(format!("Failed to parse active agents: {e}")))
    }

    /// Check whether a pod's container image differs from the scheduler's
    /// configured image. Returns `Some(true)` if stale, `Some(false)` if
    /// current, or `None` if the image couldn't be determined.
    fn pod_has_stale_image(&self, pod: &Pod) -> Option<bool> {
        let container = pod
            .spec
            .as_ref()?
            .containers
            .first()?;
        let pod_image = container.image.as_deref()?;
        Some(pod_image != self.config.image)
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

        // Check for container errors (waiting state with error reasons)
        let (container_error, error_message) = Self::extract_container_error(pod);

        // Map pod phase to agent state, considering container errors
        let (new_state, message) = if container_error {
            (AgentState::Error, error_message)
        } else {
            match (phase, ready) {
                ("Running", true) => (AgentState::Running, None),
                ("Running", false) | ("Pending", _) => (AgentState::Provisioning, None),
                ("Failed", _) => {
                    let msg = pod.status.as_ref().and_then(|s| s.message.clone());
                    (AgentState::Error, msg)
                }
                ("Succeeded", _) => (AgentState::Stopped, None),
                _ => return, // Don't update for unknown states
            }
        };

        // Only notify the gateway when the mapped state actually changes
        if !self.state_cache.update_if_changed(agent_id, new_state) {
            debug!(
                agent_id = %agent_id,
                state = ?new_state,
                "Skipping redundant status notification"
            );
            return;
        }

        if let Err(e) = self
            .notify_status_change(&agent_id, new_state, message.clone())
            .await
        {
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
                message = ?message,
                "Notified gateway of agent status change"
            );
        }
    }

    /// Extract container error information from pod status.
    ///
    /// Checks container statuses for waiting states that indicate failures
    /// (e.g., `ImagePullBackOff`, `CrashLoopBackOff`, `CreateContainerError`).
    fn extract_container_error(pod: &Pod) -> (bool, Option<String>) {
        let Some(status) = pod.status.as_ref() else {
            return (false, None);
        };

        // Check pod conditions for failure reasons
        if let Some(conditions) = &status.conditions {
            for condition in conditions {
                // PodScheduled=False with reason means scheduling failed
                if condition.type_ == "PodScheduled"
                    && condition.status == "False"
                    && condition.reason.as_deref() != Some("Unschedulable")
                {
                    if let Some(msg) = &condition.message {
                        return (true, Some(msg.clone()));
                    }
                }
            }
        }

        // Check container statuses for waiting errors
        let container_statuses = status
            .container_statuses
            .as_ref()
            .into_iter()
            .flatten()
            .chain(
                status
                    .init_container_statuses
                    .as_ref()
                    .into_iter()
                    .flatten(),
            );

        for cs in container_statuses {
            if let Some(state) = &cs.state {
                if let Some(waiting) = &state.waiting {
                    // These reasons indicate a persistent error, not just "ContainerCreating"
                    let error_reasons = [
                        "ImagePullBackOff",
                        "ErrImagePull",
                        "CrashLoopBackOff",
                        "CreateContainerError",
                        "CreateContainerConfigError",
                        "InvalidImageName",
                        "RunContainerError",
                    ];

                    if let Some(reason) = &waiting.reason {
                        if error_reasons.contains(&reason.as_str()) {
                            let msg = waiting.message.clone().unwrap_or_else(|| reason.clone());
                            return (true, Some(msg));
                        }
                    }
                }

                // Also check terminated state for errors
                if let Some(terminated) = &state.terminated {
                    if terminated.exit_code != 0 {
                        let msg = terminated
                            .message
                            .clone()
                            .or_else(|| terminated.reason.clone())
                            .unwrap_or_else(|| format!("Exit code: {}", terminated.exit_code));
                        return (true, Some(msg));
                    }
                }
            }
        }

        (false, None)
    }

    async fn handle_pod_deleted(&self, pod: &Pod) {
        let Some(agent_id) = Self::extract_agent_id(pod) else {
            return;
        };

        self.endpoint_cache.remove(&agent_id);
        self.state_cache.remove(&agent_id);

        // Notify the gateway that the pod disappeared. The control plane keeps
        // active and hibernating agents in their logical states so the
        // desired-state reconciler can recreate pods without losing AgentIds.
        if let Err(e) = self
            .notify_status_change(
                &agent_id,
                AgentState::Stopped,
                Some("Pod deleted".to_string()),
            )
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
            error_message: Option<String>,
        }

        let url = format!(
            "{}/internal/agents/{}/status",
            self.config.gateway_url,
            agent_id.to_hex()
        );

        let body = StatusUpdate {
            status,
            error_message: message,
        };

        let response = self
            .http_client
            .patch(&url)
            .bearer_auth(&self.config.gateway_token)
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
        let agent_id_hex = pod
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("swarm.io/agent-id"))?;

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
        agent_name: &str,
        spec: &AgentSpec,
    ) -> Result<()> {
        // Validate resources
        self.config
            .validate_resources(spec.cpu_millicores, spec.memory_mb)?;

        let pods = self.pods_api();

        // Check if a pod for this agent already exists (by label)
        if self.find_pod_name_by_agent(agent_id).await?.is_some() {
            warn!(
                agent_id = %agent_id,
                "Pod already exists for agent, skipping creation"
            );
            return Ok(());
        }

        // Build and create the pod
        let pod = build_pod(agent_id, user_id_hex, agent_name, spec, &self.config);
        let pod_name = pod.metadata.name.clone().unwrap_or_default();
        pods.create(&PostParams::default(), &pod).await?;

        // Register with billing reporter if configured
        if let Some(reporter) = &self.billing_reporter {
            reporter.register_pod(
                &agent_id.to_hex(),
                user_id_hex,
                spec.cpu_millicores,
                spec.memory_mb,
            );
            debug!(
                agent_id = %agent_id,
                "Registered pod with billing reporter"
            );
        }

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

        // Unregister from billing reporter if configured
        if let Some(reporter) = &self.billing_reporter {
            reporter.unregister_pod(&agent_id.to_hex());
            debug!(
                agent_id = %agent_id,
                "Unregistered pod from billing reporter"
            );
        }

        self.endpoint_cache.remove(agent_id);
        self.state_cache.remove(agent_id);

        let Some(pod_name) = self.find_pod_name_by_agent(agent_id).await? else {
            warn!(agent_id = %agent_id, "Pod not found for agent, already terminated");
            return Ok(());
        };

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

        let pod_name = self
            .find_pod_name_by_agent(agent_id)
            .await?
            .ok_or_else(|| SchedulerError::PodNotFound(agent_id.to_hex()))?;

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

        // Fetch from K8s via label selector
        let pod = self.find_pod_by_agent(agent_id).await?;

        if let Some(pod) = pod {
            if let Some(ip) = pod.status.as_ref().and_then(|s| s.pod_ip.as_ref()) {
                let endpoint = format!("{ip}:8080");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_scheduler::MockScheduler;
    use aura_swarm_core::UserId;

    fn test_agent_id() -> AgentId {
        AgentId::generate()
    }

    fn test_spec() -> AgentSpec {
        AgentSpec::default()
    }

    #[tokio::test]
    async fn mock_scheduler_schedule_and_terminate() {
        let scheduler = MockScheduler::new();
        let agent_id = test_agent_id();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let spec = test_spec();

        // Schedule
        scheduler
            .schedule_agent(&agent_id, &user_id.to_string(), "test-agent", &spec)
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
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let spec = test_spec();

        // Schedule twice
        scheduler
            .schedule_agent(&agent_id, &user_id.to_string(), "test-agent", &spec)
            .await
            .unwrap();
        scheduler
            .schedule_agent(&agent_id, &user_id.to_string(), "test-agent", &spec)
            .await
            .unwrap();

        // Should still be 1 pod
        assert_eq!(scheduler.pod_count(), 1);
    }

    #[tokio::test]
    async fn mock_scheduler_endpoint() {
        let scheduler = MockScheduler::new();
        let agent_id = test_agent_id();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let spec = test_spec();

        scheduler
            .schedule_agent(&agent_id, &user_id.to_string(), "test-agent", &spec)
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
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let spec = test_spec();

        let agent1 = AgentId::generate();
        let agent2 = AgentId::generate();

        scheduler
            .schedule_agent(&agent1, &user_id.to_string(), "agent-1", &spec)
            .await
            .unwrap();
        scheduler
            .schedule_agent(&agent2, &user_id.to_string(), "agent-2", &spec)
            .await
            .unwrap();

        let pods = scheduler.list_pods().await.unwrap();
        assert_eq!(pods.len(), 2);
    }

    #[tokio::test]
    async fn mock_get_status_not_found() {
        let scheduler = MockScheduler::new();
        let agent_id = test_agent_id();

        let result = scheduler.get_pod_status(&agent_id).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SchedulerError::PodNotFound(_)
        ));
    }

    #[tokio::test]
    async fn mock_terminate_nonexistent() {
        let scheduler = MockScheduler::new();
        let agent_id = test_agent_id();

        let result = scheduler.terminate_agent(&agent_id).await;
        assert!(result.is_ok());
        assert_eq!(scheduler.pod_count(), 0);
    }

    #[tokio::test]
    async fn mock_get_endpoint_not_found() {
        let scheduler = MockScheduler::new();
        let agent_id = test_agent_id();

        let result = scheduler.get_pod_endpoint(&agent_id).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn mock_check_health_not_found() {
        let scheduler = MockScheduler::new();
        let agent_id = test_agent_id();

        let healthy = scheduler.check_agent_health(&agent_id).await.unwrap();
        assert!(!healthy);
    }
}
