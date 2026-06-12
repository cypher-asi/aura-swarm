//! Kubernetes scheduler implementation.
//!
//! This module provides the `K8sScheduler` which manages agent pods in a
//! Kubernetes cluster using the Kata Containers runtime for microVM isolation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Event, Pod};
use kube::api::{Api, DeleteParams, ListParams, LogParams, PostParams};
use kube::runtime::watcher::{self, watcher, Config as WatcherConfig};
use kube::Client;
use serde::Serialize;
use tracing::{debug, error, info, warn};

use aura_swarm_core::AgentId;
use aura_swarm_store::{AgentSpec, AgentState, BoxTier, LogLine};

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
    /// `reason` records why the pod is going away (e.g. `terminated`,
    /// `hibernate`, `stop`, `tier_change`, `stale_image`); it is attached
    /// to the final-tail log snapshot shipped to the gateway.
    ///
    /// # Errors
    ///
    /// Returns an error if pod deletion fails (except 404).
    async fn terminate_agent(&self, agent_id: &AgentId, reason: &str) -> Result<()>;

    /// Fetch the stdout tail of an agent's pod via the K8s pod-logs API.
    ///
    /// Returns up to `tail_lines` parsed log lines, optionally limited to
    /// lines emitted at or after `since`.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerError::PodNotFound`] when the agent has no pod,
    /// or an error if the logs cannot be retrieved.
    async fn get_pod_logs(
        &self,
        agent_id: &AgentId,
        tail_lines: u32,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<LogLine>>;

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

/// Maximum time a pod is allowed to remain in a non-Ready Provisioning state
/// before the scheduler escalates it to `AgentState::Error`. Applies to pods
/// that are `Pending`/`Unschedulable` or sitting with no progress signal: more
/// wall-clock time will not unstick a pod the scheduler cannot place, and
/// surfacing it quickly is what fixed the "stuck on Cooking..." UX in aura-os
/// when a brand-new pod could not be scheduled.
const POD_STUCK_TIMEOUT: chrono::Duration = chrono::Duration::seconds(120);

/// Grace period for pods that are still *progressing* through container
/// creation / image pull (`ContainerCreating`, `PodInitializing`, ...). Unlike
/// an unschedulable pod, one that is pulling an image is not stuck -- it just
/// needs time, and a fresh multi-GB harness image being pulled onto every node
/// during a redeploy routinely exceeds [`POD_STUCK_TIMEOUT`]. Escalating those
/// to `Error` at 120s falsely terminalized healthy `Idle`/`Provisioning`
/// agents mid-rollout (and tripped strict redeploy verification). Genuine pull
/// failures (`ErrImagePull`/`ImagePullBackOff`/`CrashLoopBackOff`/...) are still
/// caught immediately by [`K8sScheduler::extract_container_error`], so anything
/// reaching this grace window is legitimately in flight.
const POD_IMAGE_PULL_TIMEOUT: chrono::Duration = chrono::Duration::seconds(600);

/// Number of lines captured for the final-tail snapshot shipped to the
/// gateway on pod termination. Pod stdout vanishes with the pod, so this
/// is the only platform-log history that survives hibernate/stop.
const FINAL_TAIL_LINES: u32 = 1000;

/// Byte cap applied to pod-log reads. Keeps a snapshot payload safely
/// under the gateway's request-body limit (1 MB by default) and bounds
/// live tail responses.
const POD_LOG_LIMIT_BYTES: i64 = 700_000;

/// Upper bound on `tail_lines` accepted for a live pod-log read.
const MAX_TAIL_LINES: u32 = 5000;

/// Time budget for capturing the final tail during termination. The
/// capture is strictly best-effort: exceeding this budget skips the
/// snapshot rather than delaying pod deletion.
const FINAL_TAIL_CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// Parse the raw output of the K8s pod-logs API (requested with
/// `timestamps=true`) into structured entries.
///
/// Each line is prefixed with an RFC3339 timestamp followed by a single
/// space; the prefix is stripped into [`LogLine::timestamp`]. Lines
/// without a parsable prefix (shouldn't happen, but logs are untrusted
/// input) are kept verbatim with `fallback` as their timestamp so no
/// output is silently dropped.
fn parse_pod_log_output(raw: &str, fallback: DateTime<Utc>) -> Vec<LogLine> {
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|raw_line| match raw_line.split_once(' ') {
            Some((prefix, rest)) => match DateTime::parse_from_rfc3339(prefix) {
                Ok(ts) => LogLine {
                    timestamp: ts.with_timezone(&Utc),
                    line: rest.to_string(),
                },
                Err(_) => LogLine {
                    timestamp: fallback,
                    line: raw_line.to_string(),
                },
            },
            None => LogLine {
                timestamp: fallback,
                line: raw_line.to_string(),
            },
        })
        .collect()
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

        let involved = &event.involved_object;
        if involved.kind.as_deref() != Some("Pod") {
            return;
        }

        // Get the involved pod name
        let Some(ref pod_name) = involved.name else {
            return;
        };

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

        if !error_reasons.contains(&reason) {
            return;
        }

        warn!(
            pod_name,
            reason, message, "Pod creation error detected from event"
        );

        // Fetch the pod so we can both extract the agent ID and check whether
        // the warning is still relevant against the pod's *current* state.
        // Pod names are based on user-provided agent names, so we use labels
        // rather than name prefixes to identify swarm-agent pods.
        let pod = match self.pods_api().get_opt(pod_name).await {
            Ok(Some(pod)) => pod,
            Ok(None) => {
                debug!(pod_name, "Pod not found when handling error event");
                return;
            }
            Err(e) => {
                debug!(pod_name, error = %e, "Failed to fetch pod for error event");
                return;
            }
        };

        let Some(agent_id) = Self::extract_agent_id(&pod) else {
            return;
        };

        // Stale-warning guard: Warning events for `FailedAttachVolume` and
        // friends can fire on already-running pods during control-plane
        // re-evaluations (e.g. an EKS kube-controller-manager leader
        // re-election re-resolving every PV's plugin). When the pod is
        // currently `Running` and `Ready` the warning is stale -- the
        // volume is in fact attached, the workload is healthy, and
        // escalating to `Error` would produce a wedged gateway record.
        //
        // The wedge is non-obvious: `handle_pod_update`'s state_cache dedup
        // would early-return on the next pod update (cache still says
        // Running, mapped state still Running, no change to push), leaving
        // the gateway DB stuck in Error indefinitely. Drop the event here
        // instead.
        let phase = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Unknown");
        let ready = Self::is_pod_ready(&pod);

        if phase == "Running" && ready {
            debug!(
                agent_id = %agent_id,
                pod_name,
                reason,
                "Ignoring Warning event on Running+Ready pod (stale vs. current state)"
            );
            return;
        }

        // Recycle guard: a pod that is being intentionally torn down (its
        // replacement carries the rolling upgrade) must not escalate to Error.
        // This mirrors the guard already enforced in `handle_pod_update`.
        if Self::is_pod_recycling(&pod) {
            debug!(
                agent_id = %agent_id,
                pod_name,
                reason,
                "Ignoring Warning event for a recycling pod"
            );
            return;
        }

        // Transient-infra guard: FailedCreatePodSandBox / NetworkNotReady /
        // FailedMount / FailedScheduling / FailedAttachVolume routinely fire as
        // *transient* warnings while the cluster churns pods -- most visibly
        // during a redeploy, when the desired-state reconciler rolling-replaces
        // every stale-image pod and the cluster briefly contends on CNI IP
        // allocation, sandbox setup, and subPath mounts on the shared state
        // PVC. Escalating an otherwise-active agent to Error on the first such
        // warning flips healthy idle/provisioning agents to Error for a few
        // seconds (until the replacement pod is Ready) and trips the strict
        // redeploy verification, even though nothing is actually broken.
        //
        // Only escalate once the pod has genuinely been wedged past
        // POD_STUCK_TIMEOUT, matching the time-based escalation in
        // `handle_pod_update`. Younger pods are left alone; the periodic 30s
        // reconcile re-checks every pod and the stuck-pod path surfaces real
        // failures once the grace window elapses.
        if detect_stuck_pod(&pod, Utc::now(), POD_STUCK_TIMEOUT, POD_IMAGE_PULL_TIMEOUT).is_none() {
            debug!(
                agent_id = %agent_id,
                pod_name,
                reason,
                "Deferring transient pod Warning; pod still within stuck-pod grace window"
            );
            return;
        }

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
            return;
        }

        // Keep `state_cache` in sync with what we just pushed to the gateway.
        // Without this, a subsequent `handle_pod_update` that observes the
        // pod recovering to `Running+Ready` would early-return on its
        // `update_if_changed(Running)` dedup check (cache still records the
        // pre-event mapped state) and the gateway would remain stuck in
        // `Error`. Recording `Error` here ensures the next legitimate
        // transition is pushed.
        self.state_cache
            .update_if_changed(agent_id, AgentState::Error);

        info!(
            agent_id = %agent_id,
            reason,
            "Notified gateway of pod error from Kubernetes event"
        );
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
                            let pod_name = pod.metadata.name.as_deref().unwrap_or("unknown");
                            info!(
                                agent_id = %agent_info.agent_id,
                                pod_name,
                                expected_image = %self.config.image,
                                "Desired-state reconciler: deleting pod with stale image"
                            );
                            if let Err(e) = self.terminate_agent(&agent_id, "stale_image").await {
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
                stale_deleted, "Desired-state reconciler: pass complete"
            );
        } else {
            debug!("Desired-state reconciler: all pods converged");
        }

        // Re-evaluate every existing pod after the convergence pass. The pod
        // watcher only fires on Kubernetes Apply/Delete events, so a pod that
        // is silently stuck (e.g. Unschedulable with no further events for
        // minutes) would never trip the time-based escalation in
        // `handle_pod_update`. Running it on every 30s reconcile tick gives
        // stuck pods a deterministic upper-bound on how long they can stay
        // hidden in Provisioning. `state_cache.update_if_changed` dedupes the
        // no-op case so this does not flood the gateway.
        for pod in pod_by_agent.values() {
            self.handle_pod_update(pod).await;
        }
    }

    /// Fetch the list of agents that should have running pods from the gateway.
    async fn fetch_active_agents(&self) -> Result<Vec<ActiveAgentInfo>> {
        let url = format!("{}/internal/agents/active", self.config.gateway_url);

        let response = self
            .http_client
            .get(&url)
            .bearer_auth(&self.config.gateway_token)
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
        let container = pod.spec.as_ref()?.containers.first()?;
        let pod_image = container.image.as_deref()?;
        Some(pod_image != self.config.image)
    }

    async fn handle_pod_update(&self, pod: &Pod) {
        let Some(agent_id) = Self::extract_agent_id(pod) else {
            return;
        };

        if Self::is_pod_recycling(pod) {
            debug!(
                agent_id = %agent_id,
                "Ignoring pod status update during intentional pod recycle"
            );
            self.endpoint_cache.remove(&agent_id);
            self.state_cache.remove(&agent_id);
            return;
        }

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
        let (mut new_state, mut message) = if container_error {
            (AgentState::Error, error_message)
        } else {
            match (phase, ready) {
                ("Running", true) => (AgentState::Running, None),
                ("Running", false) | ("Pending", _) => (AgentState::Provisioning, None),
                ("Failed", _) => {
                    let msg = pod
                        .status
                        .as_ref()
                        .and_then(|s| s.message.clone())
                        .or_else(|| Some("Pod phase Failed".to_string()));
                    (AgentState::Error, msg)
                }
                ("Succeeded", _) => (AgentState::Stopped, None),
                _ => return, // Don't update for unknown states
            }
        };

        // Time-based escalation: if a pod has been sitting in Provisioning for
        // longer than its allowed budget (POD_STUCK_TIMEOUT for unschedulable /
        // no-progress pods, POD_IMAGE_PULL_TIMEOUT for pods still pulling an
        // image), surface it as Error with whatever diagnostic the pod object
        // can provide. Without this, brand-new agents can sit in Provisioning
        // forever and the desktop UI never gets a definitive failure event;
        // with too tight a budget, healthy pods pulling a fresh harness image
        // during a redeploy get falsely terminalized to Error.
        if matches!(new_state, AgentState::Provisioning) {
            if let Some(stuck_msg) =
                detect_stuck_pod(pod, Utc::now(), POD_STUCK_TIMEOUT, POD_IMAGE_PULL_TIMEOUT)
            {
                warn!(
                    agent_id = %agent_id,
                    phase,
                    ready,
                    reason = %stuck_msg,
                    "Pod stuck in Provisioning past timeout; escalating to Error"
                );
                new_state = AgentState::Error;
                message = Some(stuck_msg);
            }
        }

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

    fn is_pod_recycling(pod: &Pod) -> bool {
        if pod.metadata.deletion_timestamp.is_some() {
            return true;
        }

        pod.metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get("swarm.io/recycle-reason"))
            .is_some_and(|reason| reason == "harness-upgrade")
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

    /// Read a pod's stdout tail via the K8s pod-logs API and parse it
    /// into structured entries.
    async fn fetch_pod_logs(
        &self,
        pod_name: &str,
        tail_lines: u32,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<LogLine>> {
        let pods = self.pods_api();
        let params = LogParams {
            timestamps: true,
            tail_lines: Some(i64::from(tail_lines.min(MAX_TAIL_LINES))),
            since_time: since,
            limit_bytes: Some(POD_LOG_LIMIT_BYTES),
            ..LogParams::default()
        };
        let raw = pods.logs(pod_name, &params).await?;
        Ok(parse_pod_log_output(&raw, Utc::now()))
    }

    /// Best-effort: capture the pod's final stdout tail and ship it to
    /// the gateway as a termination snapshot.
    ///
    /// Pod logs vanish with the pod, so this runs on every termination
    /// path *before* the delete. It must never block or fail termination:
    /// the capture is bounded by [`FINAL_TAIL_CAPTURE_TIMEOUT`] and every
    /// failure is logged and swallowed.
    async fn capture_and_ship_final_tail(&self, agent_id: &AgentId, pod_name: &str, reason: &str) {
        let captured = tokio::time::timeout(
            FINAL_TAIL_CAPTURE_TIMEOUT,
            self.fetch_pod_logs(pod_name, FINAL_TAIL_LINES, None),
        )
        .await;

        let entries = match captured {
            Ok(Ok(entries)) => entries,
            Ok(Err(e)) => {
                warn!(
                    agent_id = %agent_id,
                    pod_name,
                    error = %e,
                    "Failed to capture final log tail; terminating without snapshot"
                );
                return;
            }
            Err(_) => {
                warn!(
                    agent_id = %agent_id,
                    pod_name,
                    "Timed out capturing final log tail; terminating without snapshot"
                );
                return;
            }
        };

        if entries.is_empty() {
            debug!(agent_id = %agent_id, pod_name, "Pod produced no logs; skipping snapshot");
            return;
        }

        if let Err(e) = self.ship_log_snapshot(agent_id, reason, entries).await {
            warn!(
                agent_id = %agent_id,
                pod_name,
                error = %e,
                "Failed to ship final log snapshot to gateway"
            );
        } else {
            info!(agent_id = %agent_id, pod_name, reason, "Shipped final log snapshot to gateway");
        }
    }

    /// POST a captured log tail to the gateway's internal snapshot
    /// endpoint (`POST /internal/agents/:id/log-snapshot`).
    async fn ship_log_snapshot(
        &self,
        agent_id: &AgentId,
        reason: &str,
        entries: Vec<LogLine>,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct LogSnapshotRequest<'a> {
            captured_at: DateTime<Utc>,
            reason: &'a str,
            entries: Vec<LogLine>,
        }

        let url = format!(
            "{}/internal/agents/{}/log-snapshot",
            self.config.gateway_url,
            agent_id.to_hex()
        );

        let body = LogSnapshotRequest {
            captured_at: Utc::now(),
            reason,
            entries,
        };

        let response = self
            .http_client
            .post(&url)
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

    fn pod_reference_time(pod: &Pod) -> Option<DateTime<Utc>> {
        if let Some(start) = pod.status.as_ref().and_then(|s| s.start_time.as_ref()) {
            return Some(start.0);
        }
        pod.metadata.creation_timestamp.as_ref().map(|t| t.0)
    }

    /// The configured `INTERNAL_TOKEN` used for gateway internal APIs.
    ///
    /// Empty when the scheduler is started without `INTERNAL_TOKEN` /
    /// `GATEWAY_TOKEN` (dev mode); callers can use this to skip the
    /// gateway-auth boot probe in that case.
    #[must_use]
    pub fn gateway_token(&self) -> &str {
        &self.config.gateway_token
    }

    /// Probe the gateway's `/internal/health` with the configured bearer
    /// token. Used at startup to fail fast on `INTERNAL_TOKEN` drift between
    /// the scheduler and gateway deployments, which would otherwise manifest
    /// as new agents silently getting stuck in `Provisioning`.
    ///
    /// See [`verify_gateway_auth`] for the error semantics.
    ///
    /// # Errors
    ///
    /// Returns the same [`GatewayAuthError`] variants as [`verify_gateway_auth`].
    pub async fn verify_gateway_auth(&self) -> std::result::Result<(), GatewayAuthError> {
        verify_gateway_auth(
            &self.http_client,
            &self.config.gateway_url,
            &self.config.gateway_token,
        )
        .await
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

/// Returns a diagnostic message if `pod` has been alive for at least
/// `threshold` and is still not Ready. Used to escalate silently-stuck
/// pods (e.g. `Unschedulable`, slow `ContainerCreating`) to
/// `AgentState::Error` so callers stop seeing an indefinite Provisioning.
///
/// Pure helper -- takes `now` as a parameter so the time logic is fully
/// unit-testable without a live cluster.
fn detect_stuck_pod(
    pod: &Pod,
    now: DateTime<Utc>,
    stuck_threshold: chrono::Duration,
    pull_threshold: chrono::Duration,
) -> Option<String> {
    let reference = K8sScheduler::pod_reference_time(pod)?;
    let age = now - reference;
    // Nothing can be considered stuck before the (smaller) base budget.
    if age < stuck_threshold {
        return None;
    }

    let status = pod.status.as_ref();

    // Prefer the most specific signal we can find. Order matters: a
    // PodScheduled=False message (e.g. "0/3 nodes are available...") is far
    // more useful than the generic phase fallback. An unschedulable pod is
    // genuinely stuck -- escalate at the base `stuck_threshold` because more
    // time will not place it.
    if let Some(conditions) = status.and_then(|s| s.conditions.as_ref()) {
        for condition in conditions {
            if condition.type_ == "PodScheduled" && condition.status == "False" {
                let reason = condition.reason.as_deref().unwrap_or("Unschedulable");
                let detail = condition
                    .message
                    .clone()
                    .unwrap_or_else(|| reason.to_string());
                return Some(format!(
                    "pod stuck unscheduled for {}s ({reason}): {detail}",
                    age.num_seconds()
                ));
            }
        }
    }

    // Latest container waiting reason (e.g. ContainerCreating with a slow image
    // pull). The hard-error reasons (ImagePullBackOff/ErrImagePull/
    // CrashLoopBackOff/...) are already caught upstream by
    // `extract_container_error`, so anything we see here is still *progressing*
    // through creation. Give it the longer `pull_threshold` budget so a fresh
    // harness image pull during a redeploy is not mistaken for a wedged pod.
    let container_statuses = status.and_then(|s| s.container_statuses.as_ref());
    let init_container_statuses = status.and_then(|s| s.init_container_statuses.as_ref());
    for cs in container_statuses
        .into_iter()
        .flatten()
        .chain(init_container_statuses.into_iter().flatten())
    {
        if let Some(waiting) = cs.state.as_ref().and_then(|s| s.waiting.as_ref()) {
            if let Some(reason) = waiting.reason.as_deref() {
                if age < pull_threshold {
                    // Still inside the image-pull grace window; the pod is
                    // creating, not stuck. Let it keep cooking.
                    return None;
                }
                let detail = waiting.message.clone().unwrap_or_else(|| reason.to_string());
                return Some(format!(
                    "pod stuck in {reason} for {}s: {detail}",
                    age.num_seconds()
                ));
            }
        }
    }

    let phase = status
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("Unknown");
    Some(format!(
        "pod stuck in phase {phase} for {}s with no progress signal",
        age.num_seconds()
    ))
}

/// Outcome of [`verify_gateway_auth`].
///
/// Distinguishes hard misconfiguration (wrong / missing token) from transient
/// transport errors so callers can fail fast on the former and retry the
/// latter while the gateway is still coming up.
#[derive(Debug)]
pub enum GatewayAuthError {
    /// Gateway rejected the bearer token (HTTP 401/403). Almost always means
    /// the scheduler's `INTERNAL_TOKEN` does not match the gateway's, e.g.
    /// because only one of the two deployments was rolled with a new secret.
    /// Do NOT retry; new agents will silently sit in `Provisioning` forever
    /// because `notify_status_change` rides this same auth path.
    InvalidToken {
        /// HTTP status code (401 or 403).
        status: u16,
        /// Response body returned by the gateway, for diagnostics.
        body: String,
    },
    /// Could not reach the gateway at all (DNS, connection refused, timeout,
    /// TLS handshake error). Retriable during boot - the gateway service may
    /// just not be ready yet.
    Transport(String),
    /// Gateway returned a non-success status that is not 401/403 (e.g. 5xx
    /// during a deploy, or an unexpected 4xx). Treated as a hard failure
    /// because it implies the internal contract is broken in some way the
    /// scheduler cannot reason about.
    Unexpected {
        /// HTTP status code returned by the gateway.
        status: u16,
        /// Response body returned by the gateway, for diagnostics.
        body: String,
    },
}

impl GatewayAuthError {
    /// Whether this error is worth retrying during scheduler boot.
    ///
    /// Only transport errors are retriable - 401/403 means the operator has
    /// to fix the secret, and unexpected statuses mean the gateway is
    /// returning something we don't understand.
    #[must_use]
    pub const fn is_retriable(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}

impl std::fmt::Display for GatewayAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidToken { status, body } => write!(
                f,
                "gateway rejected INTERNAL_TOKEN (HTTP {status}); \
                 scheduler->gateway internal callbacks will fail and new agents \
                 will get stuck in Provisioning. Body: {body}"
            ),
            Self::Transport(msg) => {
                write!(f, "transport error reaching gateway /internal/health: {msg}")
            }
            Self::Unexpected { status, body } => write!(
                f,
                "gateway /internal/health returned unexpected HTTP {status}: {body}"
            ),
        }
    }
}

impl std::error::Error for GatewayAuthError {}

/// Verify the scheduler can authenticate to the gateway's protected
/// `/internal/health` endpoint with the configured bearer token.
///
/// The scheduler's reconciler and status callbacks all hit `/internal/*`
/// with this token, so if it is wrong everything downstream of pod readiness
/// silently breaks: new agents never transition `Provisioning -> Running` and
/// even the stuck-pod escalation in `handle_pod_update` 401s. Probing
/// `/internal/health` once at startup makes that misconfiguration loud
/// instead of silent.
///
/// # Errors
///
/// - [`GatewayAuthError::InvalidToken`] on 401/403 (do not retry).
/// - [`GatewayAuthError::Transport`] on network/DNS/timeout failures
///   (retriable while the gateway boots).
/// - [`GatewayAuthError::Unexpected`] on any other non-success status.
pub async fn verify_gateway_auth(
    http_client: &reqwest::Client,
    gateway_url: &str,
    gateway_token: &str,
) -> std::result::Result<(), GatewayAuthError> {
    let url = format!("{gateway_url}/internal/health");

    let response = http_client
        .get(&url)
        .bearer_auth(gateway_token)
        .send()
        .await
        .map_err(|e| GatewayAuthError::Transport(e.to_string()))?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }

    let body = response.text().await.unwrap_or_default();
    match status.as_u16() {
        401 | 403 => Err(GatewayAuthError::InvalidToken {
            status: status.as_u16(),
            body,
        }),
        other => Err(GatewayAuthError::Unexpected {
            status: other,
            body,
        }),
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

        // Register with billing reporter if configured. Tiered (confidential)
        // agents are billed under their SKU; legacy agents (tier == None)
        // keep plain cpu/mem-hour reporting.
        if let Some(reporter) = &self.billing_reporter {
            let tier = spec.tier.as_deref().and_then(BoxTier::from_name);
            reporter.register_pod(
                &agent_id.to_hex(),
                user_id_hex,
                spec.cpu_millicores,
                spec.memory_mb,
                tier,
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

    async fn terminate_agent(&self, agent_id: &AgentId, reason: &str) -> Result<()> {
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

        // Pod logs vanish with the pod: capture a final tail and ship it
        // to the gateway first. Strictly best-effort — never blocks or
        // fails the termination.
        self.capture_and_ship_final_tail(agent_id, &pod_name, reason)
            .await;

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

    async fn get_pod_logs(
        &self,
        agent_id: &AgentId,
        tail_lines: u32,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<LogLine>> {
        let pod_name = self
            .find_pod_name_by_agent(agent_id)
            .await?
            .ok_or_else(|| SchedulerError::PodNotFound(agent_id.to_hex()))?;

        self.fetch_pod_logs(&pod_name, tail_lines, since).await
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
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use std::collections::BTreeMap;

    fn test_agent_id() -> AgentId {
        AgentId::generate()
    }

    fn test_spec() -> AgentSpec {
        AgentSpec::default()
    }

    #[test]
    fn parse_pod_log_output_strips_timestamp_prefix() {
        let fallback = Utc::now();
        let raw = "2026-06-12T08:00:00.123456789Z booting harness\n\
                   2026-06-12T08:00:01Z attestation ok\n";

        let entries = parse_pod_log_output(raw, fallback);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].line, "booting harness");
        assert_eq!(
            entries[0].timestamp.timestamp_millis(),
            DateTime::parse_from_rfc3339("2026-06-12T08:00:00.123456789Z")
                .unwrap()
                .timestamp_millis()
        );
        assert_eq!(entries[1].line, "attestation ok");
        assert!(entries[1].timestamp > entries[0].timestamp);
    }

    #[test]
    fn parse_pod_log_output_keeps_unparsable_lines_with_fallback() {
        let fallback = Utc::now();
        let raw = "no-timestamp-prefix here\nbareline\n\n";

        let entries = parse_pod_log_output(raw, fallback);
        assert_eq!(entries.len(), 2, "empty lines dropped, content kept");
        assert_eq!(entries[0].line, "no-timestamp-prefix here");
        assert_eq!(entries[0].timestamp, fallback);
        assert_eq!(entries[1].line, "bareline");
    }

    #[test]
    fn parse_pod_log_output_preserves_spaces_in_message() {
        let entries = parse_pod_log_output(
            "2026-06-12T08:00:00Z msg with  spaces   kept\n",
            Utc::now(),
        );
        assert_eq!(entries[0].line, "msg with  spaces   kept");
    }

    #[tokio::test]
    async fn mock_scheduler_pod_logs() {
        let scheduler = MockScheduler::new();
        let agent_id = test_agent_id();

        // No pod -> PodNotFound
        assert!(matches!(
            scheduler.get_pod_logs(&agent_id, 100, None).await,
            Err(SchedulerError::PodNotFound(_))
        ));

        scheduler
            .schedule_agent(&agent_id, "user-1", "test-agent", &test_spec())
            .await
            .unwrap();

        let t0 = Utc::now();
        let t1 = t0 + chrono::Duration::seconds(1);
        scheduler.set_logs(
            &agent_id,
            vec![
                aura_swarm_store::LogLine {
                    timestamp: t0,
                    line: "first".to_string(),
                },
                aura_swarm_store::LogLine {
                    timestamp: t1,
                    line: "second".to_string(),
                },
            ],
        );

        // Tail keeps the newest lines.
        let logs = scheduler.get_pod_logs(&agent_id, 1, None).await.unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].line, "second");

        // Since filters older lines.
        let logs = scheduler
            .get_pod_logs(&agent_id, 100, Some(t1))
            .await
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].line, "second");
    }

    #[test]
    fn pod_with_deletion_timestamp_is_recycling() {
        let pod = Pod {
            metadata: kube::api::ObjectMeta {
                deletion_timestamp: Some(Time(chrono::Utc::now())),
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(K8sScheduler::is_pod_recycling(&pod));
    }

    #[test]
    fn pod_with_harness_recycle_annotation_is_recycling() {
        let mut annotations = BTreeMap::new();
        annotations.insert(
            "swarm.io/recycle-reason".to_string(),
            "harness-upgrade".to_string(),
        );

        let pod = Pod {
            metadata: kube::api::ObjectMeta {
                annotations: Some(annotations),
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(K8sScheduler::is_pod_recycling(&pod));
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
        scheduler.terminate_agent(&agent_id, "terminated").await.unwrap();
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

        let result = scheduler.terminate_agent(&agent_id, "terminated").await;
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

    // =========================================================================
    // detect_stuck_pod tests
    //
    // These exercise the pure stuck-pod heuristic without spinning up a real
    // kube client, by hand-building Pod objects with the relevant status
    // fields populated.
    // =========================================================================

    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateWaiting, ContainerStatus, PodCondition,
        PodStatus as K8sPodStatus,
    };

    fn pod_with_age_secs(age_secs: i64) -> Pod {
        let now = Utc::now();
        Pod {
            metadata: kube::api::ObjectMeta {
                creation_timestamp: Some(Time(now - chrono::Duration::seconds(age_secs))),
                ..Default::default()
            },
            status: Some(K8sPodStatus {
                phase: Some("Pending".to_string()),
                start_time: Some(Time(now - chrono::Duration::seconds(age_secs))),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn unschedulable_condition(message: &str) -> PodCondition {
        PodCondition {
            type_: "PodScheduled".to_string(),
            status: "False".to_string(),
            reason: Some("Unschedulable".to_string()),
            message: Some(message.to_string()),
            ..Default::default()
        }
    }

    fn waiting_container(reason: &str, message: Option<&str>) -> ContainerStatus {
        ContainerStatus {
            name: "agent".to_string(),
            ready: false,
            restart_count: 0,
            image: "test".to_string(),
            image_id: String::new(),
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some(reason.to_string()),
                    message: message.map(str::to_string),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    const STUCK_THRESHOLD: chrono::Duration = chrono::Duration::seconds(120);
    const PULL_THRESHOLD: chrono::Duration = chrono::Duration::seconds(600);

    #[test]
    fn detect_stuck_pod_under_threshold_returns_none() {
        let pod = pod_with_age_secs(30);
        let now = Utc::now();
        assert!(detect_stuck_pod(&pod, now, STUCK_THRESHOLD, PULL_THRESHOLD).is_none());
    }

    #[test]
    fn detect_stuck_pod_unschedulable_surfaces_condition_message() {
        let mut pod = pod_with_age_secs(150);
        pod.status.as_mut().unwrap().conditions = Some(vec![unschedulable_condition(
            "0/3 nodes are available: 3 Insufficient cpu",
        )]);

        let msg =
            detect_stuck_pod(&pod, Utc::now(), STUCK_THRESHOLD, PULL_THRESHOLD).expect("stuck");

        assert!(
            msg.contains("Unschedulable"),
            "expected reason in message: {msg}"
        );
        assert!(
            msg.contains("Insufficient cpu"),
            "expected detail in message: {msg}"
        );
    }

    #[test]
    fn detect_stuck_pod_container_creating_within_pull_grace_returns_none() {
        // A pod still pulling its image past the base stuck threshold but well
        // inside the image-pull grace window is progressing, not stuck. This is
        // the redeploy regression guard: a fresh harness image pull must not be
        // escalated to Error and falsely terminalize a healthy agent.
        let mut pod = pod_with_age_secs(200);
        pod.status.as_mut().unwrap().container_statuses = Some(vec![waiting_container(
            "ContainerCreating",
            Some("pulling image foo:bar"),
        )]);

        assert!(
            detect_stuck_pod(&pod, Utc::now(), STUCK_THRESHOLD, PULL_THRESHOLD).is_none(),
            "ContainerCreating within the pull grace window must not be flagged stuck"
        );
    }

    #[test]
    fn detect_stuck_pod_container_creating_past_pull_grace_surfaces_waiting_reason() {
        let mut pod = pod_with_age_secs(700);
        pod.status.as_mut().unwrap().container_statuses = Some(vec![waiting_container(
            "ContainerCreating",
            Some("pulling image foo:bar"),
        )]);

        let msg =
            detect_stuck_pod(&pod, Utc::now(), STUCK_THRESHOLD, PULL_THRESHOLD).expect("stuck");

        assert!(
            msg.contains("ContainerCreating"),
            "expected reason in message: {msg}"
        );
        assert!(
            msg.contains("pulling image foo:bar"),
            "expected detail in message: {msg}"
        );
    }

    #[test]
    fn detect_stuck_pod_falls_back_to_phase_when_no_signal() {
        let pod = pod_with_age_secs(180);
        let msg =
            detect_stuck_pod(&pod, Utc::now(), STUCK_THRESHOLD, PULL_THRESHOLD).expect("stuck");

        assert!(msg.contains("Pending"), "expected phase in message: {msg}");
        assert!(msg.contains("180s"), "expected age in message: {msg}");
    }

    #[test]
    fn detect_stuck_pod_without_timestamps_returns_none() {
        let pod = Pod::default();
        assert!(detect_stuck_pod(&pod, Utc::now(), STUCK_THRESHOLD, PULL_THRESHOLD).is_none());
    }

    // =========================================================================
    // is_pod_ready tests
    //
    // These exercise the Ready-condition probe used by the stale-warning
    // guard in `handle_k8s_event`. The guard relies on `is_pod_ready` to
    // decide whether to ignore a `FailedAttachVolume`/`FailedMount` Warning
    // event arriving on an already-running pod (e.g. during a kube
    // controller-manager re-election that re-evaluates every PV plugin).
    // =========================================================================

    fn ready_condition(status: &str) -> PodCondition {
        PodCondition {
            type_: "Ready".to_string(),
            status: status.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn is_pod_ready_true_when_ready_condition_is_true() {
        let pod = Pod {
            status: Some(K8sPodStatus {
                phase: Some("Running".to_string()),
                conditions: Some(vec![ready_condition("True")]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(K8sScheduler::is_pod_ready(&pod));
    }

    #[test]
    fn is_pod_ready_false_when_ready_condition_is_false() {
        let pod = Pod {
            status: Some(K8sPodStatus {
                phase: Some("Running".to_string()),
                conditions: Some(vec![ready_condition("False")]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!K8sScheduler::is_pod_ready(&pod));
    }

    #[test]
    fn is_pod_ready_false_when_no_conditions() {
        let pod = Pod {
            status: Some(K8sPodStatus {
                phase: Some("Running".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!K8sScheduler::is_pod_ready(&pod));
    }

    #[test]
    fn is_pod_ready_false_when_no_status() {
        let pod = Pod::default();
        assert!(!K8sScheduler::is_pod_ready(&pod));
    }

    // =========================================================================
    // state_cache divergence regression test
    //
    // Before the fix in `handle_k8s_event`, the scheduler had two paths
    // pushing status to the gateway -- pod watcher (cached) and event
    // watcher (uncached) -- and the in-memory cache only tracked the
    // former. A Warning event would push `Error` to the gateway DB while
    // leaving the cache at `Running`, after which the next pod update
    // (still showing Running+Ready) would early-return on the dedup check
    // and the gateway DB would stay wedged in `Error` forever.
    //
    // The fix keeps the cache synchronized with event-driven pushes. This
    // test reproduces the wedge scenario at the cache level.
    // =========================================================================

    #[test]
    fn state_cache_event_path_keeps_cache_in_sync_with_gateway() {
        let cache = StateCache::new();
        let agent_id = test_agent_id();

        // Pod watcher records that the agent is Running.
        assert!(cache.update_if_changed(agent_id, AgentState::Running));

        // Event watcher pushes Error to the gateway (e.g. FailedAttachVolume).
        // After the fix, it must also update the cache so future pod-watcher
        // updates can detect the divergence and re-notify.
        assert!(cache.update_if_changed(agent_id, AgentState::Error));

        // Pod watcher fires again -- pod is back to Running+Ready. The cache
        // must allow this transition through (cache miss vs. Error), since
        // otherwise the gateway DB stays stuck in Error.
        assert!(
            cache.update_if_changed(agent_id, AgentState::Running),
            "cache must allow Error -> Running transition to unstick the gateway"
        );
    }

    // =========================================================================
    // verify_gateway_auth tests
    //
    // These exercise the boot-time gateway auth probe via wiremock so we can
    // simulate the real `Authorization: Bearer ...` check the gateway runs in
    // require_internal_auth without needing to spin up the gateway crate.
    // =========================================================================

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("build reqwest client")
    }

    #[tokio::test]
    async fn verify_gateway_auth_ok_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal/health"))
            .and(header("authorization", "Bearer good-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok"
            })))
            .mount(&server)
            .await;

        let result = verify_gateway_auth(&test_http_client(), &server.uri(), "good-token").await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[tokio::test]
    async fn verify_gateway_auth_returns_invalid_token_on_401() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal/health"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let err = verify_gateway_auth(&test_http_client(), &server.uri(), "wrong-token")
            .await
            .expect_err("expected InvalidToken error");

        assert!(
            matches!(err, GatewayAuthError::InvalidToken { status: 401, .. }),
            "expected InvalidToken{{401}}, got {err:?}"
        );
        assert!(!err.is_retriable(), "401 must not be retriable");
        assert!(
            err.to_string().contains("INTERNAL_TOKEN"),
            "error message should mention INTERNAL_TOKEN for operator clarity: {err}"
        );
    }

    #[tokio::test]
    async fn verify_gateway_auth_returns_invalid_token_on_403() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal/health"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let err = verify_gateway_auth(&test_http_client(), &server.uri(), "any")
            .await
            .expect_err("expected InvalidToken error");

        assert!(
            matches!(err, GatewayAuthError::InvalidToken { status: 403, .. }),
            "expected InvalidToken{{403}}, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_gateway_auth_returns_unexpected_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal/health"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = verify_gateway_auth(&test_http_client(), &server.uri(), "any")
            .await
            .expect_err("expected Unexpected error");

        assert!(
            matches!(err, GatewayAuthError::Unexpected { status: 500, .. }),
            "expected Unexpected{{500}}, got {err:?}"
        );
        assert!(
            !err.is_retriable(),
            "Unexpected statuses should not be retried at boot"
        );
    }

    #[tokio::test]
    async fn verify_gateway_auth_returns_transport_on_dead_endpoint() {
        // Bind a port, then drop the listener so the address is closed. Any
        // OS will return connection refused for connections to it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener);
        let dead_url = format!("http://{addr}");

        let err = verify_gateway_auth(&test_http_client(), &dead_url, "any")
            .await
            .expect_err("expected Transport error");

        assert!(
            matches!(err, GatewayAuthError::Transport(_)),
            "expected Transport, got {err:?}"
        );
        assert!(
            err.is_retriable(),
            "Transport errors must be retriable so boot can wait for the gateway"
        );
    }
}
