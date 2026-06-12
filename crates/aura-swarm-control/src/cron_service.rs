//! `ProcessCronService`: control-plane tick loop that fires registered
//! process triggers, plus the auto-hibernate loop (Swarm TEE upgrade
//! phase 9).
//!
//! # Cron tick (trigger outside, data inside)
//!
//! Every ~30s the service scans the `process_triggers` CF for enabled
//! triggers whose `next_run_at` has passed, then for each due trigger:
//!
//! 1. **Advances the slot first** — `next_run_at` is recomputed from the
//!    cron expression and persisted together with `last_run_at` BEFORE
//!    anything is fired. A crash, wake failure, or pod error therefore
//!    can never double-fire the same slot: the slot is consumed up
//!    front and a failed delivery is simply retried at the *next* cron
//!    slot (at-most-once-per-slot, per the rollout plan).
//! 2. Wakes the agent if it is Hibernating/Stopped (existing lifecycle
//!    wake path) and polls until the pod is Running/Idle with a
//!    resolvable endpoint, bounded by a wake timeout.
//! 3. POSTs `{pod}/v1/processes/{process_id}/trigger` — process id
//!    only, never a payload; the process definition stays sealed inside
//!    the agent VM.
//!
//! Ticks never overlap: the run loop awaits each tick to completion and
//! a `try_lock` guard makes a concurrently-invoked tick a no-op.
//!
//! # Pod trigger auth
//!
//! The cron service has no user JWT (it acts on behalf of the
//! platform, not a user). It authenticates to the pod with the
//! platform-internal bearer token (`INTERNAL_TOKEN`) — the same value
//! the scheduler injects into confidential pods as
//! `AURA_SWARM_INTERNAL_TOKEN`, so the harness can verify it.
//!
//! Trust assumption: pod endpoints are only reachable on the
//! cluster-internal network (today's gateway file proxy sends no
//! bearer at all and the secrets/run proxies forward the user JWT,
//! which the harness does not introspect — the pod network boundary is
//! the real control). Any holder of the internal token can fire a
//! trigger, but a trigger carries no payload: it can only start a
//! process the owner already defined inside the TEE.
//!
//! # Auto-hibernate
//!
//! A second loop (~60s) watches Idle agents. When an agent has been
//! observed Idle for longer than `hibernate_after_idle` and has no
//! active sessions, it is hibernated through the existing lifecycle
//! path, so cron agents wake → run → sleep. Idle-since tracking is
//! in-memory: a control-plane restart resets the timers, which merely
//! delays hibernation by one window (never hibernates early).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use aura_swarm_core::AgentId;
use aura_swarm_store::{Agent, AgentState, ProcessTrigger, Store};
use chrono::{DateTime, Utc};

use crate::error::{ControlError, Result};
use crate::service::ControlPlane;
use crate::triggers::next_occurrence_after;

/// Run a blocking store operation off the async runtime.
async fn blocking_store<S, F, T>(store: &Arc<S>, f: F) -> Result<T>
where
    S: Store + 'static,
    F: FnOnce(&S) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || f(&*store))
        .await
        .map_err(|e| ControlError::Internal(format!("task join error: {e}")))?
}

/// Configuration for the cron + auto-hibernate loops.
#[derive(Debug, Clone)]
pub struct CronServiceConfig {
    /// Interval between cron scans.
    pub tick_interval: Duration,
    /// How long to wait for a woken agent to become Ready before giving
    /// up on this slot (delivery retries at the next cron slot).
    pub wake_timeout: Duration,
    /// Poll interval while waiting for a woken agent to become Ready.
    pub wake_poll_interval: Duration,
    /// Interval between auto-hibernate scans.
    pub hibernate_check_interval: Duration,
    /// How long an agent must be continuously Idle before it is
    /// auto-hibernated.
    pub hibernate_after_idle: Duration,
}

impl Default for CronServiceConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(30),
            wake_timeout: Duration::from_secs(180),
            wake_poll_interval: Duration::from_secs(3),
            hibernate_check_interval: Duration::from_secs(60),
            hibernate_after_idle: Duration::from_secs(
                crate::types::ControlConfig::default().hibernate_after_idle_seconds,
            ),
        }
    }
}

impl CronServiceConfig {
    /// Load configuration from environment variables, falling back to
    /// defaults:
    ///
    /// - `PROCESS_CRON_TICK_SECONDS` (default 30)
    /// - `PROCESS_CRON_WAKE_TIMEOUT_SECONDS` (default 180)
    /// - `AUTO_HIBERNATE_CHECK_SECONDS` (default 60)
    /// - `HIBERNATE_AFTER_IDLE_SECONDS` (default 1800)
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();
        let secs = |name: &str| -> Option<u64> {
            std::env::var(name).ok().and_then(|v| v.parse().ok())
        };
        if let Some(n) = secs("PROCESS_CRON_TICK_SECONDS") {
            config.tick_interval = Duration::from_secs(n.max(1));
        }
        if let Some(n) = secs("PROCESS_CRON_WAKE_TIMEOUT_SECONDS") {
            config.wake_timeout = Duration::from_secs(n);
        }
        if let Some(n) = secs("AUTO_HIBERNATE_CHECK_SECONDS") {
            config.hibernate_check_interval = Duration::from_secs(n.max(1));
        }
        if let Some(n) = secs("HIBERNATE_AFTER_IDLE_SECONDS") {
            config.hibernate_after_idle = Duration::from_secs(n);
        }
        config
    }
}

/// Client used to deliver a fire-trigger request to an agent pod.
///
/// Abstracted so tests can record/mock deliveries.
#[async_trait]
pub trait PodTriggerClient: Send + Sync {
    /// POST `{endpoint}/v1/processes/{process_id}/trigger`.
    ///
    /// # Errors
    ///
    /// Returns an error when the pod is unreachable or rejects the
    /// request.
    async fn fire_trigger(&self, endpoint: &str, process_id: &str) -> Result<()>;
}

/// HTTP implementation of [`PodTriggerClient`].
pub struct HttpPodTriggerClient {
    client: reqwest::Client,
    /// Platform-internal bearer (see module docs for the trust model).
    bearer_token: Option<String>,
}

impl HttpPodTriggerClient {
    /// Create a new client. `bearer_token` should be the platform
    /// `INTERNAL_TOKEN`; when `None` (dev mode) the request is sent
    /// without an Authorization header, matching the existing file
    /// proxy behavior against dev harnesses that don't enforce auth.
    #[must_use]
    pub fn new(bearer_token: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self {
            client,
            bearer_token,
        }
    }
}

#[async_trait]
impl PodTriggerClient for HttpPodTriggerClient {
    async fn fire_trigger(&self, endpoint: &str, process_id: &str) -> Result<()> {
        let url = format!("http://{endpoint}/v1/processes/{process_id}/trigger");
        let mut req = self.client.post(&url);
        if let Some(token) = &self.bearer_token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ControlError::Internal(format!("trigger request failed: {e}")))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(ControlError::Internal(format!(
                "pod rejected trigger: status {}",
                resp.status()
            )))
        }
    }
}

/// Outcome counters for one cron tick (used for logging and tests).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickOutcome {
    /// Triggers whose slot was due and was consumed this tick.
    pub due: u32,
    /// Due triggers successfully delivered to their pod.
    pub delivered: u32,
    /// Triggers whose missing `next_run_at` was seeded from the cron.
    pub seeded: u32,
}

/// The control-plane cron + auto-hibernate service. See module docs.
pub struct ProcessCronService<S, C, P> {
    store: Arc<S>,
    control: Arc<C>,
    pod_client: Arc<P>,
    config: CronServiceConfig,
    /// Guards against overlapping ticks (`try_lock` skip).
    tick_guard: tokio::sync::Mutex<()>,
    /// When each agent was first observed Idle by the hibernate loop.
    idle_since: Mutex<HashMap<AgentId, DateTime<Utc>>>,
}

impl<S, C, P> ProcessCronService<S, C, P>
where
    S: Store + 'static,
    C: ControlPlane + 'static,
    P: PodTriggerClient + 'static,
{
    /// Create a new service over the shared store, control plane, and
    /// pod trigger client.
    #[must_use]
    pub fn new(
        store: Arc<S>,
        control: Arc<C>,
        pod_client: Arc<P>,
        config: CronServiceConfig,
    ) -> Self {
        Self {
            store,
            control,
            pod_client,
            config,
            tick_guard: tokio::sync::Mutex::new(()),
            idle_since: Mutex::new(HashMap::new()),
        }
    }

    /// Run the cron tick loop forever. Spawn this as a background task.
    pub async fn run_cron_loop(self: Arc<Self>) {
        tracing::info!(
            tick_interval_secs = self.config.tick_interval.as_secs(),
            "ProcessCronService cron loop started"
        );
        let mut interval = tokio::time::interval(self.config.tick_interval);
        // A long tick (e.g. waiting on a wake) must not cause a burst of
        // catch-up ticks afterwards.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match self.tick(Utc::now()).await {
                Ok(outcome) if outcome.due > 0 || outcome.seeded > 0 => {
                    tracing::info!(
                        due = outcome.due,
                        delivered = outcome.delivered,
                        seeded = outcome.seeded,
                        "Cron tick fired due process triggers"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "Cron tick failed"),
            }
        }
    }

    /// Run the auto-hibernate loop forever. Spawn this as a background
    /// task.
    pub async fn run_auto_hibernate_loop(self: Arc<Self>) {
        tracing::info!(
            check_interval_secs = self.config.hibernate_check_interval.as_secs(),
            hibernate_after_idle_secs = self.config.hibernate_after_idle.as_secs(),
            "ProcessCronService auto-hibernate loop started"
        );
        let mut interval = tokio::time::interval(self.config.hibernate_check_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match self.hibernate_check(Utc::now()).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(count = n, "Auto-hibernated idle agents"),
                Err(e) => tracing::warn!(error = %e, "Auto-hibernate check failed"),
            }
        }
    }

    /// Run one cron scan at logical time `now`.
    ///
    /// Public (with an injected clock) so tests can drive ticks
    /// deterministically.
    ///
    /// # Errors
    ///
    /// Returns an error only when the trigger scan itself fails;
    /// per-trigger failures are logged and consume their slot.
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<TickOutcome> {
        // Skip entirely if another tick is still running.
        let Ok(_guard) = self.tick_guard.try_lock() else {
            tracing::debug!("Skipping cron tick: previous tick still running");
            return Ok(TickOutcome::default());
        };

        let triggers = blocking_store(&self.store, |s| Ok(s.list_all_process_triggers()?)).await?;

        let mut outcome = TickOutcome::default();
        for trigger in triggers {
            if !trigger.enabled {
                continue;
            }
            match trigger.next_run_at {
                Some(at) if at <= now => {
                    outcome.due += 1;
                    if self.consume_and_fire(trigger, now).await {
                        outcome.delivered += 1;
                    }
                }
                Some(_) => {}
                // Never scheduled by the agent and never fired: seed the
                // schedule from the cron expression (do not fire — the
                // slot starts at the next occurrence).
                None if trigger.last_run_at.is_none() => {
                    if self.seed_next_run(trigger, now).await {
                        outcome.seeded += 1;
                    }
                }
                // Fired before but no future occurrence: schedule
                // exhausted (e.g. year-bound cron). Nothing to do.
                None => {}
            }
        }
        Ok(outcome)
    }

    /// Persist a freshly computed `next_run_at` for a trigger that has
    /// none. Returns whether the seed was persisted.
    async fn seed_next_run(&self, mut trigger: ProcessTrigger, now: DateTime<Utc>) -> bool {
        let next = match next_occurrence_after(&trigger.cron, now) {
            Ok(Some(next)) => next,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!(
                    agent_id = %trigger.agent_id,
                    process_id = %trigger.process_id,
                    error = %e,
                    "Stored trigger has an unparseable cron; skipping"
                );
                return false;
            }
        };
        trigger.next_run_at = Some(next);
        trigger.updated_at = now;
        match blocking_store(&self.store, move |s| Ok(s.put_process_trigger(&trigger)?)).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to seed trigger next_run_at");
                false
            }
        }
    }

    /// Consume the due slot (persist advanced `next_run_at` +
    /// `last_run_at` FIRST) and then attempt delivery. Returns whether
    /// the trigger was delivered to the pod.
    async fn consume_and_fire(&self, mut trigger: ProcessTrigger, now: DateTime<Utc>) -> bool {
        let agent_id = trigger.agent_id;
        let process_id = trigger.process_id.clone();

        // At-most-once-per-slot: advance the schedule BEFORE firing so a
        // crash or delivery failure cannot re-fire this slot. A failed
        // delivery retries at the next cron occurrence.
        trigger.next_run_at = match next_occurrence_after(&trigger.cron, now) {
            Ok(next) => next,
            Err(e) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    process_id = %process_id,
                    error = %e,
                    "Due trigger has an unparseable cron; skipping"
                );
                return false;
            }
        };
        trigger.last_run_at = Some(now);
        trigger.updated_at = now;
        let persisted = trigger.clone();
        if let Err(e) =
            blocking_store(&self.store, move |s| Ok(s.put_process_trigger(&persisted)?)).await
        {
            // Do NOT fire if the slot could not be consumed: firing
            // anyway would risk a double-fire after a crash/retry.
            tracing::warn!(
                agent_id = %agent_id,
                process_id = %process_id,
                error = %e,
                "Failed to persist consumed trigger slot; not firing"
            );
            return false;
        }

        match self.deliver_trigger(&agent_id, &process_id).await {
            Ok(()) => {
                // Best-effort usage bookkeeping: a failed append must not
                // turn a delivered trigger into a reported failure.
                let event = aura_swarm_store::UsageEvent::new(
                    agent_id,
                    aura_swarm_store::UsageEventKind::TriggerFired {
                        process_id: process_id.clone(),
                    },
                );
                if let Err(e) =
                    blocking_store(&self.store, move |s| Ok(s.append_usage_event(&event)?)).await
                {
                    tracing::warn!(
                        agent_id = %agent_id,
                        process_id = %process_id,
                        error = %e,
                        "Failed to append TriggerFired usage event; usage stats may undercount"
                    );
                }
                tracing::info!(
                    agent_id = %agent_id,
                    process_id = %process_id,
                    next_run_at = ?trigger.next_run_at,
                    "Fired process trigger"
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    process_id = %process_id,
                    error = %e,
                    "Failed to deliver process trigger; slot consumed, will retry at next cron slot"
                );
                false
            }
        }
    }

    /// Wake the agent if needed, wait for it to be Ready, and POST the
    /// trigger to its pod.
    async fn deliver_trigger(&self, agent_id: &AgentId, process_id: &str) -> Result<()> {
        let agent = self.load_agent(agent_id).await?;

        match agent.status {
            AgentState::Hibernating | AgentState::Stopped => {
                // The cron service acts as the platform; the existing
                // wake path's ownership check is satisfied with the
                // owner recorded on the agent itself.
                self.control.wake_agent(&agent.user_id, agent_id).await?;
            }
            AgentState::Provisioning | AgentState::Running | AgentState::Idle => {}
            AgentState::Stopping | AgentState::Error => {
                return Err(ControlError::AgentNotRunnable(*agent_id));
            }
        }

        let endpoint = self.await_ready(agent_id).await?;
        self.pod_client.fire_trigger(&endpoint, process_id).await
    }

    /// Poll the agent until it is Running/Idle with a resolvable pod
    /// endpoint, bounded by the configured wake timeout.
    async fn await_ready(&self, agent_id: &AgentId) -> Result<String> {
        let deadline = tokio::time::Instant::now() + self.config.wake_timeout;
        loop {
            let agent = self.load_agent(agent_id).await?;
            match agent.status {
                AgentState::Running | AgentState::Idle => {
                    if let Some(endpoint) =
                        self.control.resolve_agent_endpoint(agent_id).await?
                    {
                        return Ok(endpoint);
                    }
                }
                AgentState::Provisioning => {}
                // The agent regressed (stopped, errored, or was
                // re-hibernated underneath us): give up on this slot.
                AgentState::Hibernating | AgentState::Stopping | AgentState::Stopped
                | AgentState::Error => {
                    return Err(ControlError::AgentNotRunnable(*agent_id));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ControlError::Internal(format!(
                    "timed out waiting for agent {agent_id} to become ready"
                )));
            }
            tokio::time::sleep(self.config.wake_poll_interval).await;
        }
    }

    async fn load_agent(&self, agent_id: &AgentId) -> Result<Agent> {
        let aid = *agent_id;
        blocking_store(&self.store, move |s| {
            s.get_agent(&aid)?.ok_or(ControlError::AgentNotFound(aid))
        })
        .await
    }

    /// Run one auto-hibernate scan at logical time `now`; returns the
    /// number of agents hibernated.
    ///
    /// Public (with an injected clock) so tests can drive checks
    /// deterministically.
    ///
    /// # Errors
    ///
    /// Returns an error when the Idle-agent scan fails; per-agent
    /// hibernate failures are logged and retried on the next scan.
    pub async fn hibernate_check(&self, now: DateTime<Utc>) -> Result<u32> {
        let idle_agents =
            blocking_store(&self.store, |s| Ok(s.list_agents_by_status(AgentState::Idle)?))
                .await?;

        let threshold = chrono::Duration::from_std(self.config.hibernate_after_idle)
            .unwrap_or_else(|_| chrono::Duration::days(36500));

        // Update idle-since bookkeeping and collect agents past the
        // threshold (without holding the lock across awaits).
        let candidates: Vec<Agent> = {
            let mut idle_since = self.idle_since.lock().expect("idle_since poisoned");
            let idle_ids: std::collections::HashSet<AgentId> =
                idle_agents.iter().map(|a| a.agent_id).collect();
            // Agents that left Idle restart their countdown next time.
            idle_since.retain(|id, _| idle_ids.contains(id));

            idle_agents
                .into_iter()
                .filter(|agent| {
                    let since = *idle_since.entry(agent.agent_id).or_insert(now);
                    now.signed_duration_since(since) >= threshold
                })
                .collect()
        };

        let mut hibernated = 0u32;
        for agent in candidates {
            let aid = agent.agent_id;
            // Defense in depth: Idle should already imply zero active
            // sessions, but never hibernate under an active session.
            let active = blocking_store(&self.store, move |s| {
                Ok(crate::session::count_active_sessions(s, &aid)?)
            })
            .await?;
            if active > 0 {
                tracing::debug!(
                    agent_id = %aid,
                    active_sessions = active,
                    "Skipping auto-hibernate: agent has active sessions"
                );
                continue;
            }

            match self.control.hibernate_agent(&agent.user_id, &aid).await {
                Ok(_) => {
                    self.idle_since
                        .lock()
                        .expect("idle_since poisoned")
                        .remove(&aid);
                    tracing::info!(agent_id = %aid, "Auto-hibernated idle agent");
                    hibernated += 1;
                }
                Err(e) => {
                    // E.g. a session was created concurrently and the
                    // Idle -> Hibernating transition is no longer valid.
                    tracing::debug!(
                        agent_id = %aid,
                        error = %e,
                        "Auto-hibernate skipped (state changed or transition failed)"
                    );
                }
            }
        }
        Ok(hibernated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler_client::NoopSchedulerClient;
    use crate::service::ControlPlaneService;
    use crate::types::{ControlConfig, CreateAgentRequest};
    use aura_swarm_core::UserId;
    use aura_swarm_store::RocksStore;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::TempDir;

    /// Pod client recording deliveries, optionally failing them.
    #[derive(Default)]
    struct MockPodClient {
        fired: Mutex<Vec<(String, String)>>,
        fail: AtomicBool,
    }

    impl MockPodClient {
        fn fired(&self) -> Vec<(String, String)> {
            self.fired.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PodTriggerClient for MockPodClient {
        async fn fire_trigger(&self, endpoint: &str, process_id: &str) -> Result<()> {
            self.fired
                .lock()
                .unwrap()
                .push((endpoint.to_string(), process_id.to_string()));
            if self.fail.load(Ordering::SeqCst) {
                return Err(ControlError::Internal("pod unavailable".to_string()));
            }
            Ok(())
        }
    }

    type TestControl = ControlPlaneService<RocksStore, NoopSchedulerClient>;
    type TestService = ProcessCronService<RocksStore, TestControl, MockPodClient>;

    fn test_config() -> CronServiceConfig {
        CronServiceConfig {
            tick_interval: Duration::from_millis(10),
            wake_timeout: Duration::from_secs(2),
            wake_poll_interval: Duration::from_millis(10),
            hibernate_check_interval: Duration::from_millis(10),
            hibernate_after_idle: Duration::from_millis(100),
        }
    }

    fn setup(
        config: CronServiceConfig,
    ) -> (Arc<TestService>, Arc<RocksStore>, Arc<TestControl>, Arc<MockPodClient>, TempDir, UserId)
    {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(RocksStore::open(dir.path()).unwrap());
        let control = Arc::new(ControlPlaneService::new(
            Arc::clone(&store),
            ControlConfig::default(),
        ));
        let pod_client = Arc::new(MockPodClient::default());
        let service = Arc::new(ProcessCronService::new(
            Arc::clone(&store),
            Arc::clone(&control),
            Arc::clone(&pod_client),
            config,
        ));
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        (service, store, control, pod_client, dir, user_id)
    }

    async fn create_agent_in_state(
        control: &TestControl,
        store: &RocksStore,
        user_id: &UserId,
        state: AgentState,
    ) -> AgentId {
        let (agent, _) = control
            .create_agent(user_id, CreateAgentRequest::new("cron-agent"))
            .await
            .unwrap();
        store.update_agent_status(&agent.agent_id, state).unwrap();
        agent.agent_id
    }

    fn put_trigger(
        store: &RocksStore,
        agent_id: &AgentId,
        process_id: &str,
        cron: &str,
        enabled: bool,
        next_run_at: Option<DateTime<Utc>>,
        last_run_at: Option<DateTime<Utc>>,
    ) {
        let now = Utc::now();
        store
            .put_process_trigger(&ProcessTrigger {
                agent_id: *agent_id,
                process_id: process_id.to_string(),
                cron: cron.to_string(),
                enabled,
                next_run_at,
                last_run_at,
                registered_at: now,
                updated_at: now,
            })
            .unwrap();
    }

    // =========================================================================
    // Cron slot logic
    // =========================================================================

    #[tokio::test]
    async fn due_trigger_fires_once_and_advances_next_run() {
        let (service, store, control, pod, _dir, user_id) = setup(test_config());
        let agent_id = create_agent_in_state(&control, &store, &user_id, AgentState::Idle).await;

        let now = Utc::now();
        put_trigger(
            &store,
            &agent_id,
            "p1",
            "*/5 * * * *",
            true,
            Some(now - chrono::Duration::minutes(1)),
            None,
        );

        let outcome = service.tick(now).await.unwrap();
        assert_eq!(outcome.due, 1);
        assert_eq!(outcome.delivered, 1);
        assert_eq!(pod.fired().len(), 1);
        assert_eq!(pod.fired()[0].1, "p1");

        let stored = store.get_process_trigger(&agent_id, "p1").unwrap().unwrap();
        assert_eq!(stored.last_run_at, Some(now));
        let next = stored.next_run_at.expect("must be advanced");
        assert!(next > now, "next_run_at must move past now");

        // Repeated ticks within the same slot must not re-fire.
        for _ in 0..3 {
            let outcome = service.tick(now + chrono::Duration::seconds(1)).await.unwrap();
            assert_eq!(outcome.due, 0);
        }
        assert_eq!(pod.fired().len(), 1, "at-most-once per slot");

        // The next slot fires again.
        let outcome = service.tick(next).await.unwrap();
        assert_eq!(outcome.due, 1);
        assert_eq!(pod.fired().len(), 2);
    }

    #[tokio::test]
    async fn delivery_failure_consumes_slot_and_retries_next_slot() {
        let (service, store, control, pod, _dir, user_id) = setup(test_config());
        let agent_id = create_agent_in_state(&control, &store, &user_id, AgentState::Idle).await;
        pod.fail.store(true, Ordering::SeqCst);

        let now = Utc::now();
        put_trigger(
            &store,
            &agent_id,
            "p1",
            "*/5 * * * *",
            true,
            Some(now),
            None,
        );

        let outcome = service.tick(now).await.unwrap();
        assert_eq!(outcome.due, 1);
        assert_eq!(outcome.delivered, 0);
        assert_eq!(pod.fired().len(), 1, "one delivery attempt");

        // The slot is consumed even though delivery failed (crash-safe
        // at-most-once); the same slot is never retried.
        let stored = store.get_process_trigger(&agent_id, "p1").unwrap().unwrap();
        let next = stored.next_run_at.unwrap();
        assert!(next > now);
        assert_eq!(service.tick(now).await.unwrap().due, 0);
        assert_eq!(pod.fired().len(), 1);

        // Retry happens at the NEXT cron slot.
        pod.fail.store(false, Ordering::SeqCst);
        let outcome = service.tick(next).await.unwrap();
        assert_eq!(outcome.delivered, 1);
        assert_eq!(pod.fired().len(), 2);
    }

    #[tokio::test]
    async fn disabled_and_future_triggers_are_skipped() {
        let (service, store, control, pod, _dir, user_id) = setup(test_config());
        let agent_id = create_agent_in_state(&control, &store, &user_id, AgentState::Idle).await;

        let now = Utc::now();
        put_trigger(&store, &agent_id, "off", "*/5 * * * *", false, Some(now), None);
        put_trigger(
            &store,
            &agent_id,
            "future",
            "*/5 * * * *",
            true,
            Some(now + chrono::Duration::hours(1)),
            None,
        );

        let outcome = service.tick(now).await.unwrap();
        assert_eq!(outcome, TickOutcome::default());
        assert!(pod.fired().is_empty());
    }

    #[tokio::test]
    async fn missing_next_run_is_seeded_without_firing() {
        let (service, store, control, pod, _dir, user_id) = setup(test_config());
        let agent_id = create_agent_in_state(&control, &store, &user_id, AgentState::Idle).await;

        let now = Utc::now();
        put_trigger(&store, &agent_id, "p1", "*/5 * * * *", true, None, None);

        let outcome = service.tick(now).await.unwrap();
        assert_eq!(outcome.seeded, 1);
        assert_eq!(outcome.due, 0);
        assert!(pod.fired().is_empty());

        let stored = store.get_process_trigger(&agent_id, "p1").unwrap().unwrap();
        assert!(stored.next_run_at.unwrap() > now);
    }

    #[tokio::test]
    async fn exhausted_schedule_is_not_reseeded() {
        let (service, store, control, pod, _dir, user_id) = setup(test_config());
        let agent_id = create_agent_in_state(&control, &store, &user_id, AgentState::Idle).await;

        let now = Utc::now();
        // Fired in the past, year-bound cron with no future occurrence.
        put_trigger(
            &store,
            &agent_id,
            "p1",
            "0 0 0 1 1 ? 2020",
            true,
            None,
            Some(now - chrono::Duration::days(1)),
        );

        let outcome = service.tick(now).await.unwrap();
        assert_eq!(outcome, TickOutcome::default());
        assert!(pod.fired().is_empty());
    }

    #[tokio::test]
    async fn delivered_trigger_appends_usage_event_failed_delivery_does_not() {
        let (service, store, control, pod, _dir, user_id) = setup(test_config());
        let agent_id = create_agent_in_state(&control, &store, &user_id, AgentState::Idle).await;

        let now = Utc::now();
        put_trigger(&store, &agent_id, "p1", "*/5 * * * *", true, Some(now), None);
        service.tick(now).await.unwrap();

        let fired: Vec<_> = store
            .list_usage_events_by_agent(&agent_id)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.kind {
                aura_swarm_store::UsageEventKind::TriggerFired { process_id } => Some(process_id),
                _ => None,
            })
            .collect();
        assert_eq!(fired, vec!["p1".to_string()], "delivery records the event");

        // A failed delivery consumes the slot but records no event.
        pod.fail.store(true, Ordering::SeqCst);
        let stored = store.get_process_trigger(&agent_id, "p1").unwrap().unwrap();
        let next = stored.next_run_at.unwrap();
        service.tick(next).await.unwrap();

        let count = store
            .list_usage_events_by_agent(&agent_id)
            .unwrap()
            .into_iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    aura_swarm_store::UsageEventKind::TriggerFired { .. }
                )
            })
            .count();
        assert_eq!(count, 1, "failed delivery must not record TriggerFired");
    }

    // =========================================================================
    // Wake-then-fire
    // =========================================================================

    #[tokio::test]
    async fn hibernating_agent_is_woken_then_fired() {
        let (service, store, control, pod, _dir, user_id) = setup(test_config());
        let agent_id =
            create_agent_in_state(&control, &store, &user_id, AgentState::Hibernating).await;

        let now = Utc::now();
        put_trigger(&store, &agent_id, "p1", "*/5 * * * *", true, Some(now), None);

        // Simulate the scheduler callback flipping the woken agent to
        // Running shortly after the wake (Provisioning) transition.
        let flip_store = Arc::clone(&store);
        let flip_id = agent_id;
        let flipper = tokio::spawn(async move {
            for _ in 0..100 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let agent = flip_store.get_agent(&flip_id).unwrap().unwrap();
                if agent.status == AgentState::Provisioning {
                    flip_store
                        .update_agent_status(&flip_id, AgentState::Running)
                        .unwrap();
                    return;
                }
            }
            panic!("agent never entered Provisioning");
        });

        let outcome = service.tick(now).await.unwrap();
        flipper.await.unwrap();

        assert_eq!(outcome.delivered, 1);
        assert_eq!(pod.fired().len(), 1);
        // The Noop scheduler resolves a dev endpoint for active agents.
        assert_eq!(pod.fired()[0].0, "localhost:8080");
        let agent = store.get_agent(&agent_id).unwrap().unwrap();
        assert_eq!(agent.status, AgentState::Running);
    }

    #[tokio::test]
    async fn wake_timeout_gives_up_but_slot_stays_consumed() {
        let mut config = test_config();
        config.wake_timeout = Duration::from_millis(50);
        let (service, store, control, pod, _dir, user_id) = setup(config);
        let agent_id =
            create_agent_in_state(&control, &store, &user_id, AgentState::Hibernating).await;

        let now = Utc::now();
        put_trigger(&store, &agent_id, "p1", "*/5 * * * *", true, Some(now), None);

        // No one flips the agent to Running: the wake wait must time out.
        let outcome = service.tick(now).await.unwrap();
        assert_eq!(outcome.due, 1);
        assert_eq!(outcome.delivered, 0);
        assert!(pod.fired().is_empty());

        // Slot was consumed before the wake attempt (crash-safe).
        let stored = store.get_process_trigger(&agent_id, "p1").unwrap().unwrap();
        assert!(stored.next_run_at.unwrap() > now);
        assert_eq!(stored.last_run_at, Some(now));
    }

    // =========================================================================
    // Auto-hibernate
    // =========================================================================

    #[tokio::test]
    async fn idle_agent_hibernates_after_threshold() {
        let (service, store, control, _pod, _dir, user_id) = setup(test_config());
        let agent_id = create_agent_in_state(&control, &store, &user_id, AgentState::Idle).await;

        let t0 = Utc::now();
        // First observation only starts the countdown.
        assert_eq!(service.hibernate_check(t0).await.unwrap(), 0);
        // Still below the 100ms threshold.
        assert_eq!(
            service
                .hibernate_check(t0 + chrono::Duration::milliseconds(50))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store.get_agent(&agent_id).unwrap().unwrap().status,
            AgentState::Idle
        );

        // Past the threshold: hibernate.
        assert_eq!(
            service
                .hibernate_check(t0 + chrono::Duration::milliseconds(200))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store.get_agent(&agent_id).unwrap().unwrap().status,
            AgentState::Hibernating
        );
        assert!(service.idle_since.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn leaving_idle_resets_the_countdown() {
        let (service, store, control, _pod, _dir, user_id) = setup(test_config());
        let agent_id = create_agent_in_state(&control, &store, &user_id, AgentState::Idle).await;

        let t0 = Utc::now();
        assert_eq!(service.hibernate_check(t0).await.unwrap(), 0);

        // Agent becomes active again: countdown entry must be dropped.
        store
            .update_agent_status(&agent_id, AgentState::Running)
            .unwrap();
        assert_eq!(
            service
                .hibernate_check(t0 + chrono::Duration::milliseconds(200))
                .await
                .unwrap(),
            0
        );
        assert!(service.idle_since.lock().unwrap().is_empty());

        // Back to Idle: the countdown starts over from the new
        // observation, so a check right after stays below threshold.
        store
            .update_agent_status(&agent_id, AgentState::Idle)
            .unwrap();
        let t1 = t0 + chrono::Duration::milliseconds(300);
        assert_eq!(service.hibernate_check(t1).await.unwrap(), 0);
        assert_eq!(
            service
                .hibernate_check(t1 + chrono::Duration::milliseconds(50))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store.get_agent(&agent_id).unwrap().unwrap().status,
            AgentState::Idle
        );
    }

    #[tokio::test]
    async fn active_session_blocks_auto_hibernate() {
        let (service, store, control, _pod, _dir, user_id) = setup(test_config());
        let agent_id = create_agent_in_state(&control, &store, &user_id, AgentState::Running).await;

        // Create an active session, then force the agent record to Idle
        // to simulate a stale state with a live session.
        control
            .create_session(&user_id, &agent_id, Default::default())
            .await
            .unwrap();
        store
            .update_agent_status(&agent_id, AgentState::Idle)
            .unwrap();

        let t0 = Utc::now();
        assert_eq!(service.hibernate_check(t0).await.unwrap(), 0);
        assert_eq!(
            service
                .hibernate_check(t0 + chrono::Duration::milliseconds(200))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store.get_agent(&agent_id).unwrap().unwrap().status,
            AgentState::Idle,
            "an agent with active sessions must never be auto-hibernated"
        );
    }
}
