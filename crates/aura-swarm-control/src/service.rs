//! Control plane service implementation.
//!
//! This module provides the `ControlPlane` trait and `ControlPlaneService` implementation
//! that coordinates agent lifecycle and session management.

use std::sync::Arc;

use async_trait::async_trait;
use aura_swarm_core::{AgentId, SessionId, UserId};
use aura_swarm_store::{
    Agent, AgentLogSnapshot, AgentState, BoxTier, Session, SessionConfig, SessionStatus, Store,
    UsageEvent, UsageEventKind,
};
use chrono::{DateTime, Utc};

use crate::billing::BillingChecker;
use crate::error::{ControlError, Result};
use crate::kbs::KbsClient;
use crate::lifecycle;
use crate::scheduler_client::SchedulerClient;
use crate::session;
use crate::types::{AgentLogEntry, ControlConfig, CreateAgentRequest, LogSource};

/// Tier name + hourly price to record on a usage event, captured from the
/// agent's spec **at event time** so later re-pricing never rewrites usage
/// history. Legacy agents (no tier) record `(None, None)`.
fn event_pricing(spec: &aura_swarm_store::AgentSpec) -> (Option<String>, Option<u32>) {
    let price = spec
        .tier
        .as_deref()
        .and_then(BoxTier::from_name)
        .map(|t| t.hourly_price_cents());
    (spec.tier.clone(), price)
}

/// Outcome of the R2 startup DEK backfill
/// ([`ControlPlaneService::backfill_missing_deks`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DekBackfillSummary {
    /// Agents whose spec declares sealed storage.
    pub sealed_agents: u32,
    /// DEKs newly provisioned by this pass.
    pub provisioned: u32,
    /// DEKs that were already registered (untouched).
    pub already_present: u32,
    /// Agents skipped due to a failed provision or an indeterminate
    /// existence check (retried on next startup).
    pub failed: u32,
}

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

/// Trait defining the control plane operations.
///
/// This trait provides the complete API for managing agents and sessions.
/// Implementations handle state persistence, validation, and coordination.
#[async_trait]
pub trait ControlPlane: Send + Sync {
    // =========================================================================
    // Agent CRUD Operations
    // =========================================================================

    /// Create a new agent for the given user.
    ///
    /// Returns `(agent, true)` if a new agent was created, or `(agent, false)` if
    /// a caller-supplied `agent_id` matched an existing agent owned by the same
    /// user (idempotent return).
    ///
    /// # Errors
    ///
    /// Returns `ControlError::QuotaExceeded` if the user has reached their limit.
    /// Returns `ControlError::AgentAlreadyExists` if the ID is owned by another user.
    async fn create_agent(
        &self,
        user_id: &UserId,
        request: CreateAgentRequest,
    ) -> Result<(Agent, bool)>;

    /// Get an agent by ID, verifying ownership.
    ///
    /// # Errors
    ///
    /// Returns `ControlError::AgentNotFound` if the agent doesn't exist.
    /// Returns `ControlError::NotOwner` if the user doesn't own the agent.
    async fn get_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent>;

    /// List all agents for a user.
    async fn list_agents(&self, user_id: &UserId) -> Result<Vec<Agent>>;

    /// Delete an agent.
    ///
    /// The agent must be in a stopped state before deletion.
    ///
    /// # Errors
    ///
    /// Returns `ControlError::InvalidState` if the agent is not stopped.
    async fn delete_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<()>;

    // =========================================================================
    // Lifecycle Operations
    // =========================================================================

    /// Start an agent (transition from Stopped to Provisioning).
    async fn start_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent>;

    /// Stop an agent gracefully.
    async fn stop_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent>;

    /// Restart an agent (stop then start).
    async fn restart_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent>;

    /// Hibernate an agent (save state, terminate pod).
    async fn hibernate_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent>;

    /// Wake a hibernating agent.
    async fn wake_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent>;

    /// Change an agent's box tier (upgrade / downgrade).
    ///
    /// Semantics (Swarm TEE upgrade phase 10):
    ///
    /// - Same-tier request: no-op, returns the current state.
    /// - Hibernating / Stopped / Error: the record is updated only; the new
    ///   size takes effect on the next wake/start (no pod churn).
    /// - Running / Idle: pre-flight credit check at the new tier's rate,
    ///   active sessions are closed, then recreate-with-state (terminate
    ///   pod → update spec → schedule pod, same path as restart). Sealed
    ///   state on EFS is untouched.
    /// - Legacy agent (`tier == None`): assigning a tier converts it to the
    ///   new architecture (confidential isolation + sealed storage) and
    ///   provisions its state DEK in the KBS — per-agent early migration.
    /// - A `TierChanged` usage event with both hourly prices is appended in
    ///   every non-noop case.
    ///
    /// # Errors
    ///
    /// Returns `ControlError::InvalidTier` for an unknown tier name,
    /// `ControlError::InvalidState` if the agent is mid-transition
    /// (Provisioning / Stopping), `ControlError::InsufficientCredits` if an
    /// awake agent can't afford the new rate, plus the usual
    /// `AgentNotFound` / `NotOwner` ownership errors.
    async fn change_tier(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        tier: &str,
    ) -> Result<crate::types::TierChangeOutcome>;

    // =========================================================================
    // Usage / Cost Stats (Swarm TEE upgrade phase 11)
    // =========================================================================

    /// Aggregate one agent's usage over `[from, to)`: billable intervals
    /// (priced at event time), totals, counters, and the most recent raw
    /// events within the range (capped at
    /// [`crate::usage::RECENT_EVENTS_CAP`]).
    ///
    /// zbilling remains the billing source of truth — this is the
    /// user-facing stats layer.
    ///
    /// # Errors
    ///
    /// Returns the usual `AgentNotFound` / `NotOwner` ownership errors.
    async fn get_agent_usage(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<crate::usage::AgentUsage>;

    /// Aggregate usage over `[from, to)` for every agent the user
    /// currently owns (destroyed agents are not included).
    ///
    /// # Errors
    ///
    /// Returns an error if the store operation fails.
    async fn get_user_usage(
        &self,
        user_id: &UserId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(Agent, crate::usage::UsageAggregation)>>;

    // =========================================================================
    // Per-VM Logs (Swarm TEE upgrade phase 12)
    // =========================================================================

    /// Merged VM/platform logs for an agent: the live pod stdout tail
    /// (when a pod is running, fetched via the scheduler) interleaved
    /// with stored termination snapshots, sorted by timestamp and capped
    /// to the last `tail` entries. A scheduler failure degrades to
    /// snapshots only — it never fails the read.
    ///
    /// These are host-visible platform logs (boot, attestation, health,
    /// harness lifecycle). Detailed in-VM agent logs stay sealed inside
    /// the guest and are not served here.
    ///
    /// # Errors
    ///
    /// Returns the usual `AgentNotFound` / `NotOwner` ownership errors.
    async fn get_agent_logs(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        tail: u32,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<AgentLogEntry>>;

    /// Store a pod-log termination snapshot shipped by the scheduler.
    ///
    /// Internal operation: no ownership check — callers must be behind
    /// internal-token auth. The per-agent snapshot cap is enforced by
    /// the store (oldest snapshots pruned on insert).
    ///
    /// # Errors
    ///
    /// Returns `ControlError::AgentNotFound` if the agent doesn't exist.
    async fn store_log_snapshot_internal(&self, snapshot: AgentLogSnapshot) -> Result<()>;

    // =========================================================================
    // Session Operations
    // =========================================================================

    /// Create a new session for an agent.
    ///
    /// If the agent is hibernating, it will be automatically woken.
    async fn create_session(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        config: SessionConfig,
    ) -> Result<Session>;

    /// Create a new session pre-bound to a harness `run_id`.
    ///
    /// Used by the `POST /v1/run` proxy so the WS-attach path can later resolve
    /// the owning session (for ownership checks + the Running -> Idle
    /// transition) from the run id alone.
    async fn create_run_session(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        run_id: String,
    ) -> Result<Session>;

    /// Find the active session bound to `run_id` for an agent.
    ///
    /// Returns `Ok(None)` if no active session is attached to that run.
    async fn find_session_by_run(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        run_id: &str,
    ) -> Result<Option<Session>>;

    /// Get a session by ID.
    async fn get_session(&self, user_id: &UserId, session_id: &SessionId) -> Result<Session>;

    /// Close a session.
    async fn close_session(&self, user_id: &UserId, session_id: &SessionId) -> Result<()>;

    /// List all sessions for an agent.
    async fn list_sessions(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Vec<Session>>;

    // =========================================================================
    // Operational
    // =========================================================================

    /// Process a heartbeat from an agent.
    async fn process_heartbeat(&self, agent_id: &AgentId) -> Result<()>;

    /// Resolve the network endpoint for an agent.
    ///
    /// Returns the endpoint URL if the agent is running.
    async fn resolve_agent_endpoint(&self, agent_id: &AgentId) -> Result<Option<String>>;

    // =========================================================================
    // Internal Operations (for scheduler callbacks)
    // =========================================================================

    /// Count active sessions for an agent (no ownership check).
    ///
    /// Used by internal endpoints to report accurate session metrics.
    ///
    /// # Errors
    ///
    /// Returns `ControlError::AgentNotFound` if the agent doesn't exist.
    async fn count_active_sessions(&self, agent_id: &AgentId) -> Result<u32>;

    /// Reconcile agent states on startup.
    ///
    /// Finds agents stored as `Running` that have zero active sessions and
    /// transitions them to `Idle`. This corrects stale state that can occur
    /// when the process restarts or sessions are lost without a clean close.
    ///
    /// Returns the number of agents reconciled.
    async fn reconcile_idle_agents(&self) -> Result<u32>;

    /// Update an agent's status without ownership verification.
    ///
    /// This is used by the scheduler to report pod status changes. It does NOT
    /// verify ownership because the scheduler operates at a system level.
    ///
    /// # Security
    ///
    /// This method should only be called from internal endpoints that are
    /// protected by network policies.
    ///
    /// # Errors
    ///
    /// Returns `ControlError::AgentNotFound` if the agent doesn't exist.
    async fn update_agent_status_internal(
        &self,
        agent_id: &AgentId,
        status: AgentState,
        error_message: Option<String>,
    ) -> Result<()>;

    /// List agents that should have running pods.
    ///
    /// Returns all agents in Provisioning, Running, or Idle states. Used by
    /// the scheduler's desired-state reconciliation loop to detect missing
    /// or stale pods.
    async fn list_active_agents(&self) -> Result<Vec<Agent>>;

    /// List every persisted agent regardless of lifecycle state.
    ///
    /// Used by deploy verification to prove that redeploys preserved machine
    /// identities, including hibernating, stopped, and error agents.
    async fn list_all_agents(&self) -> Result<Vec<Agent>>;

    /// The store's on-disk schema version (Swarm TEE upgrade R2).
    ///
    /// Exposed through `/internal/health` so deploy verification can
    /// prove the v1 → v2 migration ran (2 = tiered/sealed fleet).
    ///
    /// # Errors
    ///
    /// Returns an error if the store read fails.
    async fn schema_version(&self) -> Result<u32>;

    // =========================================================================
    // Process Triggers (Swarm TEE upgrade phase 8)
    // =========================================================================

    /// Replace the full registered trigger set for an agent with the
    /// desired set pushed by the harness (replace-semantics sync).
    ///
    /// Internal operation: no ownership check — callers must be behind
    /// internal-token auth. Validates every cron expression and
    /// process id server-side before persisting.
    ///
    /// # Errors
    ///
    /// Returns `ControlError::AgentNotFound` if the agent doesn't exist.
    /// Returns `ControlError::InvalidTrigger` for invalid input.
    async fn replace_process_triggers_internal(
        &self,
        agent_id: &AgentId,
        registrations: Vec<crate::triggers::TriggerRegistration>,
    ) -> Result<Vec<aura_swarm_store::ProcessTrigger>>;

    /// Delete one registered trigger for an agent.
    ///
    /// Internal operation: no ownership check — callers must be behind
    /// internal-token auth.
    ///
    /// # Errors
    ///
    /// Returns `ControlError::TriggerNotFound` if no such trigger exists.
    async fn delete_process_trigger_internal(
        &self,
        agent_id: &AgentId,
        process_id: &str,
    ) -> Result<()>;

    /// List the registered trigger metadata for an agent, verifying
    /// ownership (owner-facing read).
    ///
    /// # Errors
    ///
    /// Returns `ControlError::AgentNotFound` / `ControlError::NotOwner`
    /// per the usual ownership rules.
    async fn list_process_triggers(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
    ) -> Result<Vec<aura_swarm_store::ProcessTrigger>>;
}

/// The main control plane service implementation.
///
/// The service can optionally integrate with a scheduler for managing agent pods
/// and a billing checker for credit balance verification.
pub struct ControlPlaneService<
    S: Store,
    SC: SchedulerClient = crate::scheduler_client::NoopSchedulerClient,
> {
    store: Arc<S>,
    config: ControlConfig,
    scheduler: Option<Arc<SC>>,
    billing: Option<Arc<BillingChecker>>,
    kbs: Option<Arc<dyn KbsClient>>,
}

impl<S: Store> ControlPlaneService<S, crate::scheduler_client::NoopSchedulerClient> {
    /// Create a new control plane service without scheduler integration.
    #[must_use]
    pub fn new(store: Arc<S>, config: ControlConfig) -> Self {
        Self {
            store,
            config,
            scheduler: None,
            billing: None,
            kbs: None,
        }
    }

    /// Create with default configuration and no scheduler.
    #[must_use]
    pub fn with_defaults(store: Arc<S>) -> Self {
        Self::new(store, ControlConfig::default())
    }
}

impl<S: Store + 'static, SC: SchedulerClient> ControlPlaneService<S, SC> {
    /// Create a new control plane service with scheduler integration.
    #[must_use]
    pub fn with_scheduler(store: Arc<S>, config: ControlConfig, scheduler: Arc<SC>) -> Self {
        Self {
            store,
            config,
            scheduler: Some(scheduler),
            billing: None,
            kbs: None,
        }
    }

    /// Create a new control plane service with optional scheduler integration.
    #[must_use]
    pub fn with_optional_scheduler(
        store: Arc<S>,
        config: ControlConfig,
        scheduler: Option<Arc<SC>>,
    ) -> Self {
        Self {
            store,
            config,
            scheduler,
            billing: None,
            kbs: None,
        }
    }

    /// Create a control plane service with all optional integrations.
    #[must_use]
    pub fn with_integrations(
        store: Arc<S>,
        config: ControlConfig,
        scheduler: Option<Arc<SC>>,
        billing: Option<Arc<BillingChecker>>,
    ) -> Self {
        Self {
            store,
            config,
            scheduler,
            billing,
            kbs: None,
        }
    }

    /// Set the billing checker after construction.
    pub fn set_billing(&mut self, billing: Arc<BillingChecker>) {
        self.billing = Some(billing);
    }

    /// Set the KBS client used for the per-agent state DEK lifecycle.
    ///
    /// Use [`crate::kbs::HttpKbsClient`] in production and
    /// [`crate::kbs::NoopKbsClient`] in dev mode.
    pub fn set_kbs(&mut self, kbs: Arc<dyn KbsClient>) {
        self.kbs = Some(kbs);
    }

    /// Get a reference to the store.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Get the configuration.
    #[must_use]
    pub const fn config(&self) -> &ControlConfig {
        &self.config
    }

    /// Check if a scheduler is configured.
    #[must_use]
    pub fn has_scheduler(&self) -> bool {
        self.scheduler.is_some()
    }

    /// Check if billing is configured.
    #[must_use]
    pub fn has_billing(&self) -> bool {
        self.billing.is_some()
    }

    /// Check if a KBS client is configured.
    #[must_use]
    pub fn has_kbs(&self) -> bool {
        self.kbs.is_some()
    }

    /// Provision the per-agent state DEK in the KBS for a sealed agent.
    ///
    /// Legacy agents (no `storage_encryption`) make no KBS calls.
    async fn provision_state_dek(&self, agent: &Agent) -> Result<()> {
        let Some(encryption) = &agent.spec.storage_encryption else {
            return Ok(());
        };
        if let Some(kbs) = &self.kbs {
            kbs.provision_dek(encryption.key_id()).await?;
            tracing::info!(
                agent_id = %agent.agent_id,
                key_id = %encryption.key_id(),
                "Provisioned agent state DEK in KBS"
            );
        } else {
            tracing::debug!(
                agent_id = %agent.agent_id,
                "No KBS client configured, skipping DEK provisioning"
            );
        }
        Ok(())
    }

    /// Revoke the per-agent state DEK in the KBS (crypto-erase), tolerating
    /// failures: destroy must still complete, but the failure is logged
    /// loudly so the orphaned key can be cleaned up.
    ///
    /// Legacy agents (no `storage_encryption`) make no KBS calls.
    async fn revoke_state_dek_best_effort(&self, agent: &Agent) {
        let Some(encryption) = &agent.spec.storage_encryption else {
            return;
        };
        if let Some(kbs) = &self.kbs {
            match kbs.revoke_dek(encryption.key_id()).await {
                Ok(()) => {
                    tracing::info!(
                        agent_id = %agent.agent_id,
                        key_id = %encryption.key_id(),
                        "Revoked agent state DEK in KBS (crypto-erase)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent.agent_id,
                        key_id = %encryption.key_id(),
                        error = %e,
                        "Agent destroyed but DEK revocation in KBS failed; \
                         the key must be deleted manually to complete crypto-erase"
                    );
                }
            }
        }
    }

    /// R2 post-migration reconciliation: ensure every sealed agent has a
    /// state DEK registered in the KBS, provisioning the missing ones.
    ///
    /// The v1 → v2 store migration marks legacy agents as sealed without
    /// touching the KBS, so on the first post-migration startup those
    /// agents have no DEK yet — their first sealed boot would fail
    /// attestation-gated key release. This pass backfills them.
    ///
    /// Put-if-absent semantics: Trustee's resource POST is an
    /// unconditional overwrite and cannot report "exists", so the client
    /// GETs first ([`KbsClient::dek_exists`]) and only provisions on a
    /// definitive 404. An existing DEK is **never** overwritten
    /// (overwriting would brick the agent's sealed state), and an
    /// indeterminate existence check (auth/transport error) skips the
    /// agent with a loud log instead of provisioning blind.
    ///
    /// Per-agent failures are counted, logged, and do not abort the
    /// pass; rerunning at next startup converges.
    ///
    /// # Errors
    ///
    /// Returns an error only if the agent listing itself fails.
    pub async fn backfill_missing_deks(&self) -> Result<DekBackfillSummary> {
        let mut summary = DekBackfillSummary::default();

        let Some(kbs) = &self.kbs else {
            tracing::debug!("DEK backfill skipped: no KBS client configured");
            return Ok(summary);
        };

        let agents = blocking_store(&self.store, move |s| Ok(s.list_all_agents()?)).await?;

        for agent in agents {
            let Some(encryption) = &agent.spec.storage_encryption else {
                continue;
            };
            summary.sealed_agents += 1;
            let key_id = encryption.key_id();

            match kbs.dek_exists(key_id).await {
                Ok(true) => summary.already_present += 1,
                Ok(false) => match kbs.provision_dek(key_id).await {
                    Ok(()) => {
                        tracing::info!(
                            agent_id = %agent.agent_id,
                            key_id = %key_id,
                            "DEK backfill: provisioned missing state DEK"
                        );
                        summary.provisioned += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            agent_id = %agent.agent_id,
                            key_id = %key_id,
                            error = %e,
                            "DEK backfill: provisioning failed; will retry next startup"
                        );
                        summary.failed += 1;
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent.agent_id,
                        key_id = %key_id,
                        error = %e,
                        "DEK backfill: existence check indeterminate; skipping \
                         (never provisioning blind over a possibly-existing DEK)"
                    );
                    summary.failed += 1;
                }
            }
        }

        Ok(summary)
    }

    /// Check billing credits for agent creation at the given tier's rate.
    async fn check_agent_credits(&self, user_id: &UserId, tier: BoxTier) -> Result<()> {
        if let Some(billing) = &self.billing {
            billing
                .check_agent_credits_for_tier(&user_id.to_string(), tier)
                .await?;
        }
        Ok(())
    }

    /// Check billing credits for session creation.
    async fn check_session_credits(&self, user_id: &UserId) -> Result<()> {
        if let Some(billing) = &self.billing {
            billing.check_session_credits(&user_id.to_string()).await?;
        }
        Ok(())
    }

    /// Get an agent and verify ownership.
    async fn get_and_verify(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        let uid = *user_id;
        let aid = *agent_id;
        blocking_store(&self.store, move |s| {
            let agent = s.get_agent(&aid)?.ok_or(ControlError::AgentNotFound(aid))?;
            if agent.user_id != uid {
                return Err(ControlError::NotOwner {
                    user_id: uid,
                    agent_id: agent.agent_id,
                });
            }
            Ok(agent)
        })
        .await
    }

    /// Perform a validated state transition.
    async fn transition_state(&self, agent: &mut Agent, target: AgentState) -> Result<()> {
        lifecycle::validate_transition(&agent.agent_id, agent.status, target)?;
        agent.status = target;
        agent.updated_at = Utc::now();
        let snapshot = agent.clone();
        blocking_store(&self.store, move |s| Ok(s.put_agent(&snapshot)?)).await
    }

    /// Schedule an agent pod via the scheduler service.
    async fn schedule_agent_pod(&self, agent: &Agent) -> Result<()> {
        if let Some(scheduler) = &self.scheduler {
            scheduler
                .schedule_agent(
                    &agent.agent_id,
                    &agent.user_id.to_string(),
                    &agent.name,
                    &agent.spec,
                )
                .await?;
            tracing::info!(
                agent_id = %agent.agent_id,
                "Scheduled agent pod via scheduler"
            );
        } else {
            tracing::debug!(
                agent_id = %agent.agent_id,
                "No scheduler configured, skipping pod scheduling"
            );
        }
        Ok(())
    }

    /// Terminate an agent pod via the scheduler service. `reason` is
    /// forwarded so the scheduler can label the final-tail log snapshot
    /// it ships back before deleting the pod.
    async fn terminate_agent_pod(&self, agent_id: &AgentId, reason: &str) -> Result<()> {
        if let Some(scheduler) = &self.scheduler {
            scheduler.terminate_agent(agent_id, reason).await?;
            tracing::info!(
                agent_id = %agent_id,
                "Terminated agent pod via scheduler"
            );
        } else {
            tracing::debug!(
                agent_id = %agent_id,
                "No scheduler configured, skipping pod termination"
            );
        }
        Ok(())
    }

    /// Append a usage event, best-effort: stats bookkeeping must never
    /// fail the surrounding user-facing operation, so failures are only
    /// logged (the aggregation undercounts instead of the API erroring).
    async fn append_usage_event_best_effort(&self, agent_id: &AgentId, kind: UsageEventKind) {
        let event = UsageEvent::new(*agent_id, kind);
        let result =
            blocking_store(&self.store, move |s| Ok(s.append_usage_event(&event)?)).await;
        if let Err(e) = result {
            tracing::warn!(
                agent_id = %agent_id,
                error = %e,
                "Failed to append usage event; usage stats may undercount"
            );
        }
    }

    /// Record that a pod was scheduled for the agent (opens a billable
    /// interval priced at the spec's current tier rate).
    async fn record_pod_scheduled(&self, agent: &Agent) {
        let (tier, hourly_price_cents) = event_pricing(&agent.spec);
        self.append_usage_event_best_effort(
            &agent.agent_id,
            UsageEventKind::PodScheduled {
                tier,
                hourly_price_cents,
            },
        )
        .await;
    }

    /// Record that the agent's pod was terminated (closes the billable
    /// interval), with the reason known by the call site.
    async fn record_pod_terminated(&self, agent: &Agent, reason: &str) {
        let (tier, hourly_price_cents) = event_pricing(&agent.spec);
        self.append_usage_event_best_effort(
            &agent.agent_id,
            UsageEventKind::PodTerminated {
                tier,
                hourly_price_cents,
                reason: reason.to_string(),
            },
        )
        .await;
    }

    /// Append a `TierChanged` usage event capturing both hourly prices so
    /// cost intervals split exactly at the change. Legacy agents carry no
    /// `from` tier/price (they were billed by cpu/mem-hours).
    async fn append_tier_changed_event(
        &self,
        agent_id: &AgentId,
        from: Option<String>,
        to: BoxTier,
    ) -> Result<()> {
        let event = aura_swarm_store::UsageEvent::new(
            *agent_id,
            aura_swarm_store::UsageEventKind::TierChanged {
                from_hourly_price_cents: from
                    .as_deref()
                    .and_then(BoxTier::from_name)
                    .map(|t| t.hourly_price_cents()),
                from,
                to: to.as_str().to_string(),
                to_hourly_price_cents: to.hourly_price_cents(),
            },
        );
        blocking_store(&self.store, move |s| Ok(s.append_usage_event(&event)?)).await
    }

    /// Recreate-with-state for an in-place spec change on an awake agent:
    /// close active sessions, terminate the pod, persist the new spec (plus
    /// the `TierChanged` event), and schedule a replacement pod — the same
    /// path as restart. Sealed state on EFS is untouched, and the scheduler
    /// re-registers the new pod with the billing reporter under the new SKU.
    async fn recreate_pod_with_spec(
        &self,
        agent: &mut Agent,
        new_spec: aura_swarm_store::AgentSpec,
        previous_tier: Option<String>,
        new_tier: BoxTier,
    ) -> Result<()> {
        let agent_id = agent.agent_id;

        // The recreate interrupts in-flight runs, same as restart/stop today.
        blocking_store(&self.store, move |s| {
            let sessions = s.list_sessions_by_agent(&agent_id)?;
            for sess in sessions {
                if sess.status == aura_swarm_store::SessionStatus::Active {
                    s.update_session_status(
                        &sess.session_id,
                        aura_swarm_store::SessionStatus::Closed,
                    )?;
                }
            }
            Ok(())
        })
        .await?;

        self.transition_state(agent, AgentState::Stopping).await?;

        if let Err(e) = self.terminate_agent_pod(&agent_id, "tier_change").await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to terminate agent pod on tier change"
            );
        }

        // The old pod's interval closes at the OLD tier's price.
        self.record_pod_terminated(agent, "tier_change").await;

        // Persist the new spec with the Stopped transition, then record the
        // tier change before the new pod is scheduled so the event is never
        // lost to a scheduling failure.
        agent.spec = new_spec;
        self.transition_state(agent, AgentState::Stopped).await?;
        self.append_tier_changed_event(&agent_id, previous_tier, new_tier)
            .await?;

        self.transition_state(agent, AgentState::Provisioning)
            .await?;

        if let Err(e) = self.schedule_agent_pod(agent).await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to schedule agent pod on tier change"
            );
            blocking_store(&self.store, move |s| {
                s.update_agent_status(&agent_id, AgentState::Error).ok();
                Ok(())
            })
            .await?;
            return Err(e);
        }

        // The replacement pod opens a new interval at the NEW tier's price.
        self.record_pod_scheduled(agent).await;

        Ok(())
    }
}

#[async_trait]
impl<S: Store + 'static, SC: SchedulerClient + 'static> ControlPlane
    for ControlPlaneService<S, SC>
{
    // =========================================================================
    // Agent CRUD Operations
    // =========================================================================

    async fn create_agent(
        &self,
        user_id: &UserId,
        request: CreateAgentRequest,
    ) -> Result<(Agent, bool)> {
        // Resolve the box tier first: explicit tier name wins, then the
        // legacy raw spec is mapped to the nearest tier, then the default.
        // Every new agent is confidential + sealed regardless of input.
        let tier = match request.tier.as_deref() {
            Some(name) => BoxTier::from_name(name)
                .ok_or_else(|| ControlError::InvalidTier(name.to_string()))?,
            None => request
                .spec
                .as_ref()
                .map_or_else(BoxTier::default, |s| {
                    BoxTier::nearest_for_cpu(s.cpu_millicores)
                }),
        };

        self.check_agent_credits(user_id, tier).await?;

        let uid = *user_id;
        let max = self.config.max_agents_per_user;

        // Idempotent create: if a caller-supplied ID already exists, return it
        // (same user) or reject it (different user) instead of overwriting.
        if let Some(ref supplied_id) = request.agent_id {
            let check_id = *supplied_id;
            let check_uid = uid;
            let existing =
                blocking_store(&self.store, move |s| Ok(s.get_agent(&check_id)?)).await?;

            if let Some(agent) = existing {
                if agent.user_id == check_uid {
                    tracing::info!(
                        agent_id = %agent.agent_id,
                        user_id = %check_uid,
                        "Idempotent create: returning existing agent"
                    );
                    return Ok((agent, false));
                }
                return Err(ControlError::AgentAlreadyExists { agent_id: check_id });
            }
        }

        let agent = blocking_store(&self.store, move |s| {
            let count = s.count_agents_by_user(&uid)?;
            if count >= max {
                return Err(ControlError::QuotaExceeded {
                    user_id: uid,
                    limit: max,
                });
            }

            let now = Utc::now();
            let agent_id = request.agent_id.unwrap_or_else(AgentId::generate);
            // The tier dictates resources, isolation (ConfidentialVM), and
            // sealed storage with the agent's deterministic state-key id;
            // only the runtime version carries over from a legacy spec.
            let runtime_version = request
                .spec
                .map_or_else(|| "latest".to_string(), |s| s.runtime_version);
            let spec = tier.to_spec(&agent_id, runtime_version);

            let agent = Agent {
                agent_id,
                user_id: uid,
                name: request.name,
                status: AgentState::Provisioning,
                spec,
                created_at: now,
                updated_at: now,
                last_heartbeat_at: None,
                error_message: None,
            };

            s.put_agent(&agent)?;
            Ok(agent)
        })
        .await?;

        // Provision the per-agent state DEK in the KBS before scheduling:
        // a confidential pod is useless (cannot open its sealed state) if
        // the key it will request after attestation does not exist. On
        // failure the just-created record is rolled back so the create
        // fails cleanly with no half-created agent.
        if let Err(e) = self.provision_state_dek(&agent).await {
            tracing::error!(
                agent_id = %agent.agent_id,
                error = %e,
                "Failed to provision state DEK in KBS, rolling back agent creation"
            );
            let aid = agent.agent_id;
            blocking_store(&self.store, move |s| {
                s.delete_agent(&aid).ok();
                Ok(())
            })
            .await?;
            return Err(ControlError::Internal(format!(
                "failed to provision agent state DEK in KBS: {e}"
            )));
        }

        if let Err(e) = self.schedule_agent_pod(&agent).await {
            tracing::error!(
                agent_id = %agent.agent_id,
                error = %e,
                "Failed to schedule agent pod, marking as error"
            );
            let aid = agent.agent_id;
            let err_msg = e.to_string();
            blocking_store(&self.store, move |s| {
                s.update_agent_error(&aid, AgentState::Error, Some(err_msg))
                    .ok();
                Ok(())
            })
            .await?;
            return Err(e);
        }

        self.record_pod_scheduled(&agent).await;

        tracing::info!(
            agent_id = %agent.agent_id,
            user_id = %user_id,
            name = %agent.name,
            tier = %tier,
            "Created agent"
        );

        Ok((agent, true))
    }

    async fn get_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        self.get_and_verify(user_id, agent_id).await
    }

    async fn list_agents(&self, user_id: &UserId) -> Result<Vec<Agent>> {
        let uid = *user_id;
        blocking_store(&self.store, move |s| Ok(s.list_agents_by_user(&uid)?)).await
    }

    async fn delete_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<()> {
        let agent = self.get_and_verify(user_id, agent_id).await?;

        if !lifecycle::is_terminal(agent.status) {
            return Err(ControlError::InvalidState {
                agent_id: *agent_id,
                from: agent.status,
                to: AgentState::Stopped,
            });
        }

        let aid = *agent_id;
        blocking_store(&self.store, move |s| {
            let sessions = s.list_sessions_by_agent(&aid)?;
            for session in sessions {
                s.delete_session(&session.session_id)?;
            }
            // Registered process triggers must not outlive the agent —
            // otherwise the cron service would keep waking a ghost.
            s.delete_process_triggers_for_agent(&aid)?;
            s.delete_agent(&aid)?;
            Ok(())
        })
        .await?;

        // Crypto-erase: with the record gone (and the pod long terminated —
        // delete requires a terminal state), revoke the state DEK so the
        // sealed ciphertext on EFS is unrecoverable. Best-effort: a revoke
        // failure is logged loudly but does not fail the destroy.
        self.revoke_state_dek_best_effort(&agent).await;

        tracing::info!(
            agent_id = %agent_id,
            user_id = %user_id,
            "Deleted agent"
        );

        Ok(())
    }

    // =========================================================================
    // Lifecycle Operations
    // =========================================================================

    async fn start_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        let mut agent = self.get_and_verify(user_id, agent_id).await?;

        self.transition_state(&mut agent, AgentState::Provisioning)
            .await?;

        if let Err(e) = self.schedule_agent_pod(&agent).await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to schedule agent pod on start"
            );
            let aid = *agent_id;
            blocking_store(&self.store, move |s| {
                s.update_agent_status(&aid, AgentState::Error).ok();
                Ok(())
            })
            .await?;
            return Err(e);
        }

        self.record_pod_scheduled(&agent).await;

        tracing::info!(agent_id = %agent_id, "Starting agent");

        Ok(agent)
    }

    async fn stop_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        let mut agent = self.get_and_verify(user_id, agent_id).await?;

        let aid = *agent_id;
        blocking_store(&self.store, move |s| {
            let sessions = s.list_sessions_by_agent(&aid)?;
            for sess in sessions {
                if sess.status == aura_swarm_store::SessionStatus::Active {
                    s.update_session_status(
                        &sess.session_id,
                        aura_swarm_store::SessionStatus::Closed,
                    )?;
                }
            }
            Ok(())
        })
        .await?;

        self.transition_state(&mut agent, AgentState::Stopping)
            .await?;

        if let Err(e) = self.terminate_agent_pod(agent_id, "stop").await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to terminate agent pod on stop"
            );
        }

        // Recorded even if the terminate API call failed: the agent has
        // logically left the billable state and the reconciler reaps the
        // pod (a restart's terminate half also lands here as "stop").
        self.record_pod_terminated(&agent, "stop").await;

        tracing::info!(agent_id = %agent_id, "Stopping agent");

        Ok(agent)
    }

    async fn restart_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        let mut agent = self.stop_agent(user_id, agent_id).await?;

        self.transition_state(&mut agent, AgentState::Stopped)
            .await?;
        self.transition_state(&mut agent, AgentState::Provisioning)
            .await?;

        if let Err(e) = self.schedule_agent_pod(&agent).await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to schedule agent pod on restart"
            );
            let aid = *agent_id;
            blocking_store(&self.store, move |s| {
                s.update_agent_status(&aid, AgentState::Error).ok();
                Ok(())
            })
            .await?;
            return Err(e);
        }

        tracing::info!(agent_id = %agent_id, "Restarting agent");

        Ok(agent)
    }

    async fn hibernate_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        let mut agent = self.get_and_verify(user_id, agent_id).await?;

        let aid = *agent_id;
        blocking_store(&self.store, move |s| {
            let sessions = s.list_sessions_by_agent(&aid)?;
            for sess in sessions {
                if sess.status == aura_swarm_store::SessionStatus::Active {
                    s.update_session_status(
                        &sess.session_id,
                        aura_swarm_store::SessionStatus::Closed,
                    )?;
                }
            }
            Ok(())
        })
        .await?;

        self.transition_state(&mut agent, AgentState::Hibernating)
            .await?;

        if let Err(e) = self.terminate_agent_pod(agent_id, "hibernate").await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to terminate agent pod on hibernate"
            );
        }

        self.record_pod_terminated(&agent, "hibernate").await;
        self.append_usage_event_best_effort(agent_id, UsageEventKind::Hibernated)
            .await;

        tracing::info!(agent_id = %agent_id, "Hibernating agent");

        Ok(agent)
    }

    async fn wake_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        let mut agent = self.get_and_verify(user_id, agent_id).await?;

        if !lifecycle::can_wake(agent.status) {
            return Err(ControlError::InvalidState {
                agent_id: *agent_id,
                from: agent.status,
                to: AgentState::Running,
            });
        }

        self.transition_state(&mut agent, AgentState::Provisioning)
            .await?;

        if let Err(e) = self.schedule_agent_pod(&agent).await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to schedule agent pod on wake"
            );
            let aid = *agent_id;
            blocking_store(&self.store, move |s| {
                s.update_agent_status(&aid, AgentState::Error).ok();
                Ok(())
            })
            .await?;
            return Err(e);
        }

        self.append_usage_event_best_effort(agent_id, UsageEventKind::Woke)
            .await;
        self.record_pod_scheduled(&agent).await;

        tracing::info!(agent_id = %agent_id, "Waking agent");

        Ok(agent)
    }

    async fn change_tier(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        tier: &str,
    ) -> Result<crate::types::TierChangeOutcome> {
        let new_tier =
            BoxTier::from_name(tier).ok_or_else(|| ControlError::InvalidTier(tier.to_string()))?;

        let mut agent = self.get_and_verify(user_id, agent_id).await?;
        let previous_tier = agent.spec.tier.clone();

        // Same-tier request: no-op, return the current state.
        if previous_tier.as_deref() == Some(new_tier.as_str()) {
            return Ok(crate::types::TierChangeOutcome {
                previous_tier,
                tier: new_tier.as_str().to_string(),
                changed: false,
                pod_recreated: false,
                agent,
            });
        }

        let recreate_pod = match agent.status {
            // No pod: record-only, the new size applies on next wake/start.
            AgentState::Hibernating | AgentState::Stopped | AgentState::Error => false,
            // Awake: recreate-with-state, same path as restart.
            AgentState::Running | AgentState::Idle => true,
            // Mid-transition: the recreate would need a stop that the state
            // machine forbids; the caller retries once the agent settles.
            AgentState::Provisioning | AgentState::Stopping => {
                return Err(ControlError::InvalidState {
                    agent_id: *agent_id,
                    from: agent.status,
                    to: AgentState::Stopping,
                });
            }
        };

        if recreate_pod {
            // Pre-flight: the user must afford the new tier's rate before
            // any pod churn happens.
            self.check_agent_credits(user_id, new_tier).await?;
        }

        // The tier dictates resources, isolation (ConfidentialVM) and sealed
        // storage with the deterministic state-key id; only runtime_version
        // carries over. For an already-tiered agent this is purely a resize.
        let new_spec = new_tier.to_spec(agent_id, agent.spec.runtime_version.clone());

        // Legacy agent (tier == None) early migration: it has no state DEK
        // in the KBS yet, and the confidential pod it becomes cannot open
        // its sealed state without one. Provision it before anything is
        // persisted or recreated — on failure the record is untouched.
        if previous_tier.is_none() {
            let pending = Agent {
                spec: new_spec.clone(),
                ..agent.clone()
            };
            if let Err(e) = self.provision_state_dek(&pending).await {
                tracing::error!(
                    agent_id = %agent_id,
                    error = %e,
                    "Failed to provision state DEK in KBS for legacy tier migration"
                );
                return Err(ControlError::Internal(format!(
                    "failed to provision agent state DEK in KBS: {e}"
                )));
            }
        }

        if recreate_pod {
            self.recreate_pod_with_spec(&mut agent, new_spec, previous_tier.clone(), new_tier)
                .await?;
        } else {
            agent.spec = new_spec;
            agent.updated_at = Utc::now();
            let snapshot = agent.clone();
            blocking_store(&self.store, move |s| Ok(s.put_agent(&snapshot)?)).await?;
            self.append_tier_changed_event(agent_id, previous_tier.clone(), new_tier)
                .await?;
        }

        tracing::info!(
            agent_id = %agent_id,
            from = previous_tier.as_deref().unwrap_or("<legacy>"),
            to = %new_tier,
            pod_recreated = recreate_pod,
            "Changed agent tier"
        );

        Ok(crate::types::TierChangeOutcome {
            agent,
            previous_tier,
            tier: new_tier.as_str().to_string(),
            changed: true,
            pod_recreated: recreate_pod,
        })
    }

    // =========================================================================
    // Usage / Cost Stats (Swarm TEE upgrade phase 11)
    // =========================================================================

    async fn get_agent_usage(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<crate::usage::AgentUsage> {
        self.get_and_verify(user_id, agent_id).await?;

        let aid = *agent_id;
        let events =
            blocking_store(&self.store, move |s| Ok(s.list_usage_events_by_agent(&aid)?)).await?;

        let aggregation = crate::usage::aggregate(&events, from, to);

        // Most recent in-range raw events, oldest first, capped.
        let in_range: Vec<_> = events
            .into_iter()
            .filter(|e| e.timestamp >= from && e.timestamp < to)
            .collect();
        let skip = in_range.len().saturating_sub(crate::usage::RECENT_EVENTS_CAP);
        let recent_events = in_range.into_iter().skip(skip).collect();

        Ok(crate::usage::AgentUsage {
            aggregation,
            recent_events,
        })
    }

    async fn get_user_usage(
        &self,
        user_id: &UserId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(Agent, crate::usage::UsageAggregation)>> {
        let agents = self.list_agents(user_id).await?;

        let mut reports = Vec::with_capacity(agents.len());
        for agent in agents {
            let aid = agent.agent_id;
            let events =
                blocking_store(&self.store, move |s| Ok(s.list_usage_events_by_agent(&aid)?))
                    .await?;
            reports.push((agent, crate::usage::aggregate(&events, from, to)));
        }
        Ok(reports)
    }

    // =========================================================================
    // Per-VM Logs (Swarm TEE upgrade phase 12)
    // =========================================================================

    async fn get_agent_logs(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        tail: u32,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<AgentLogEntry>> {
        let agent = self.get_and_verify(user_id, agent_id).await?;

        let aid = *agent_id;
        let snapshots =
            blocking_store(&self.store, move |s| Ok(s.list_log_snapshots_by_agent(&aid)?))
                .await?;

        let mut entries: Vec<AgentLogEntry> = snapshots
            .into_iter()
            .flat_map(|snapshot| snapshot.entries)
            .map(|l| AgentLogEntry {
                timestamp: l.timestamp,
                line: l.line,
                source: LogSource::Snapshot,
            })
            .collect();

        // Live pod tail, only for states where a pod can exist.
        // Best-effort: a scheduler hiccup degrades the read to stored
        // snapshots instead of failing it.
        let pod_active = matches!(
            agent.status,
            AgentState::Provisioning | AgentState::Running | AgentState::Idle
        );
        if pod_active {
            if let Some(scheduler) = &self.scheduler {
                match scheduler.get_pod_logs(agent_id, tail, since).await {
                    Ok(Some(live)) => entries.extend(live.into_iter().map(|l| AgentLogEntry {
                        timestamp: l.timestamp,
                        line: l.line,
                        source: LogSource::Live,
                    })),
                    Ok(None) => {
                        tracing::debug!(agent_id = %agent_id, "No pod for live log tail");
                    }
                    Err(e) => {
                        tracing::warn!(
                            agent_id = %agent_id,
                            error = %e,
                            "Failed to fetch live pod logs; serving stored snapshots only"
                        );
                    }
                }
            }
        }

        if let Some(since) = since {
            entries.retain(|e| e.timestamp >= since);
        }
        // Stable sort keeps within-snapshot ordering for equal timestamps.
        entries.sort_by_key(|e| e.timestamp);
        let skip = entries.len().saturating_sub(tail as usize);
        Ok(entries.split_off(skip))
    }

    async fn store_log_snapshot_internal(&self, snapshot: AgentLogSnapshot) -> Result<()> {
        let agent_id = snapshot.agent_id;
        blocking_store(&self.store, move |s| {
            // Snapshots must not outlive (or predate) the agent record.
            if s.get_agent(&agent_id)?.is_none() {
                return Err(ControlError::AgentNotFound(agent_id));
            }
            Ok(s.put_log_snapshot(&snapshot)?)
        })
        .await?;

        tracing::debug!(agent_id = %agent_id, "Stored pod-log termination snapshot");
        Ok(())
    }

    // =========================================================================
    // Session Operations
    // =========================================================================

    async fn create_session(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        config: SessionConfig,
    ) -> Result<Session> {
        self.check_session_credits(user_id).await?;

        let uid = *user_id;
        let aid = *agent_id;
        let (session, state_change) = blocking_store(&self.store, move |s| {
            session::create_session(s, &uid, &aid, config)
        })
        .await?;

        tracing::info!(
            session_id = %session.session_id,
            agent_id = %agent_id,
            state_change = ?state_change,
            "Created session"
        );

        Ok(session)
    }

    async fn create_run_session(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        run_id: String,
    ) -> Result<Session> {
        let uid = *user_id;
        let aid = *agent_id;
        let (session, state_change) = blocking_store(&self.store, move |s| {
            session::create_session_with_run(s, &uid, &aid, SessionConfig::default(), Some(run_id))
        })
        .await?;

        tracing::info!(
            session_id = %session.session_id,
            agent_id = %agent_id,
            run_id = ?session.run_id,
            state_change = ?state_change,
            "Created run session"
        );

        Ok(session)
    }

    async fn find_session_by_run(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
        run_id: &str,
    ) -> Result<Option<Session>> {
        let uid = *user_id;
        let aid = *agent_id;
        let run_id = run_id.to_string();
        blocking_store(&self.store, move |s| {
            session::find_session_by_run(s, &uid, &aid, &run_id)
        })
        .await
    }

    async fn get_session(&self, user_id: &UserId, session_id: &SessionId) -> Result<Session> {
        let uid = *user_id;
        let sid = *session_id;
        blocking_store(&self.store, move |s| session::get_session(s, &uid, &sid)).await
    }

    async fn close_session(&self, user_id: &UserId, session_id: &SessionId) -> Result<()> {
        let uid = *user_id;
        let sid = *session_id;
        let closed =
            blocking_store(&self.store, move |s| session::close_session(s, &uid, &sid)).await?;

        if closed {
            tracing::info!(session_id = %session_id, "Closed session");
        }

        Ok(())
    }

    async fn list_sessions(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Vec<Session>> {
        let uid = *user_id;
        let aid = *agent_id;
        blocking_store(&self.store, move |s| session::list_sessions(s, &uid, &aid)).await
    }

    async fn count_active_sessions(&self, agent_id: &AgentId) -> Result<u32> {
        let aid = *agent_id;
        blocking_store(&self.store, move |s| {
            if s.get_agent(&aid)?.is_none() {
                return Err(ControlError::AgentNotFound(aid));
            }
            let count = session::count_active_sessions(s, &aid)?;
            #[allow(clippy::cast_possible_truncation)]
            Ok(count as u32)
        })
        .await
    }

    async fn reconcile_idle_agents(&self) -> Result<u32> {
        blocking_store(&self.store, move |s| {
            let running = s.list_agents_by_status(AgentState::Running)?;
            let idle = s.list_agents_by_status(AgentState::Idle)?;
            let mut reconciled = 0u32;

            // First pass: close orphaned Active sessions on all live agents.
            // A session is orphaned if it is Active but has no backing
            // WebSocket (which we can't check here), so on startup we
            // conservatively close ALL Active sessions — any real client
            // will simply create a new one when it reconnects.
            for agent in running.iter().chain(idle.iter()) {
                let sessions = s.list_sessions_by_agent(&agent.agent_id)?;
                for sess in &sessions {
                    if sess.status == SessionStatus::Active {
                        s.update_session_status(&sess.session_id, SessionStatus::Closed)?;
                        tracing::info!(
                            session_id = %sess.session_id,
                            agent_id = %agent.agent_id,
                            "Closed orphaned session on startup"
                        );
                    }
                }
            }

            // Second pass: transition Running agents (now with 0 sessions) to Idle.
            for agent in &running {
                let active = session::count_active_sessions(s, &agent.agent_id)?;
                if active == 0 {
                    s.update_agent_status(&agent.agent_id, AgentState::Idle)?;
                    tracing::info!(
                        agent_id = %agent.agent_id,
                        name = %agent.name,
                        "Reconciled stale Running → Idle (0 active sessions)"
                    );
                    reconciled += 1;
                }
            }

            Ok(reconciled)
        })
        .await
    }

    // =========================================================================
    // Operational
    // =========================================================================

    async fn process_heartbeat(&self, agent_id: &AgentId) -> Result<()> {
        let aid = *agent_id;
        blocking_store(&self.store, move |s| {
            let mut agent = s.get_agent(&aid)?.ok_or(ControlError::AgentNotFound(aid))?;

            agent.last_heartbeat_at = Some(Utc::now());
            agent.updated_at = Utc::now();
            s.put_agent(&agent)?;
            Ok(())
        })
        .await?;

        tracing::debug!(agent_id = %agent_id, "Processed heartbeat");

        Ok(())
    }

    async fn resolve_agent_endpoint(&self, agent_id: &AgentId) -> Result<Option<String>> {
        let aid = *agent_id;
        let agent = blocking_store(&self.store, move |s| {
            s.get_agent(&aid)?.ok_or(ControlError::AgentNotFound(aid))
        })
        .await?;

        if lifecycle::is_active(agent.status) {
            if let Some(scheduler) = &self.scheduler {
                scheduler.get_pod_endpoint(agent_id).await
            } else {
                Ok(Some("localhost:8080".to_string()))
            }
        } else {
            Ok(None)
        }
    }

    async fn update_agent_status_internal(
        &self,
        agent_id: &AgentId,
        status: AgentState,
        error_message: Option<String>,
    ) -> Result<()> {
        let aid = *agent_id;
        let err_msg = error_message.clone();
        let applied = blocking_store(&self.store, move |s| {
            let agent = s.get_agent(&aid)?.ok_or(ControlError::AgentNotFound(aid))?;

            // The scheduler maps K8s "Running + Ready" to AgentState::Running,
            // but the control plane distinguishes Running (has sessions) from
            // Idle (pod alive, no sessions). Don't let the scheduler overwrite
            // a session-driven state with a redundant pod-health signal.
            //
            // Likewise, a pod disappearance for an agent that is still meant to
            // be active (for example during image-roll convergence or an
            // unexpected pod eviction) must not turn the logical agent into a
            // terminal Stopped state. Keeping the current logical state lets the
            // desired-state reconciler recreate the pod against the same AgentId
            // and preserved PVC-backed state.
            let effective_status = match status {
                AgentState::Error if err_msg.is_none() => {
                    tracing::warn!(
                        agent_id = %aid,
                        current_status = ?agent.status,
                        "Ignoring scheduler Error update without an error message"
                    );
                    return Ok(false);
                }
                AgentState::Running
                    if matches!(agent.status, AgentState::Running | AgentState::Idle) =>
                {
                    return Ok(false);
                }
                AgentState::Stopped
                    if matches!(
                        agent.status,
                        AgentState::Provisioning
                            | AgentState::Running
                            | AgentState::Idle
                            | AgentState::Hibernating
                            | AgentState::Stopped
                            | AgentState::Error
                    ) =>
                {
                    return Ok(false);
                }
                other => other,
            };

            // A pod-active agent moving to Error means the pod crashed or
            // failed its health check: close the billable interval. The
            // Stopping → Error case is excluded — stop_agent already
            // recorded a "stop" termination. Best-effort: a failed append
            // must not fail the status update.
            if effective_status == AgentState::Error
                && matches!(
                    agent.status,
                    AgentState::Provisioning | AgentState::Running | AgentState::Idle
                )
            {
                let (tier, hourly_price_cents) = event_pricing(&agent.spec);
                let event = UsageEvent::new(
                    aid,
                    UsageEventKind::PodTerminated {
                        tier,
                        hourly_price_cents,
                        reason: "crash".to_string(),
                    },
                );
                if let Err(e) = s.append_usage_event(&event) {
                    tracing::warn!(
                        agent_id = %aid,
                        error = %e,
                        "Failed to append crash usage event; usage stats may undercount"
                    );
                }
            }

            if err_msg.is_some() || effective_status == AgentState::Error {
                s.update_agent_error(&aid, effective_status, err_msg)?;
            } else {
                s.update_agent_status(&aid, effective_status)?;
            }
            Ok(true)
        })
        .await?;

        if applied {
            tracing::info!(
                agent_id = %agent_id,
                status = ?status,
                error_message = ?error_message,
                "Updated agent status (internal)"
            );
        } else {
            tracing::debug!(
                agent_id = %agent_id,
                requested = ?status,
                "Skipped redundant scheduler status update"
            );
        }

        Ok(())
    }

    async fn list_active_agents(&self) -> Result<Vec<Agent>> {
        blocking_store(&self.store, move |s| {
            let mut agents = s.list_agents_by_status(AgentState::Provisioning)?;
            agents.extend(s.list_agents_by_status(AgentState::Running)?);
            agents.extend(s.list_agents_by_status(AgentState::Idle)?);
            Ok(agents)
        })
        .await
    }

    async fn list_all_agents(&self) -> Result<Vec<Agent>> {
        blocking_store(&self.store, move |s| Ok(s.list_all_agents()?)).await
    }

    async fn schema_version(&self) -> Result<u32> {
        blocking_store(&self.store, move |s| Ok(s.schema_version()?)).await
    }

    // =========================================================================
    // Process Triggers (Swarm TEE upgrade phase 8)
    // =========================================================================

    async fn replace_process_triggers_internal(
        &self,
        agent_id: &AgentId,
        registrations: Vec<crate::triggers::TriggerRegistration>,
    ) -> Result<Vec<aura_swarm_store::ProcessTrigger>> {
        if registrations.len() > crate::triggers::MAX_TRIGGERS_PER_AGENT {
            return Err(ControlError::InvalidTrigger(format!(
                "too many triggers: {} (max {})",
                registrations.len(),
                crate::triggers::MAX_TRIGGERS_PER_AGENT
            )));
        }

        // Validate the whole set up front so a bad entry rejects the
        // sync atomically instead of half-applying it.
        let triggers = registrations
            .into_iter()
            .map(|r| r.into_trigger(agent_id))
            .collect::<Result<Vec<_>>>()?;

        let aid = *agent_id;
        let stored = blocking_store(&self.store, move |s| {
            if s.get_agent(&aid)?.is_none() {
                return Err(ControlError::AgentNotFound(aid));
            }
            Ok(s.replace_process_triggers(&aid, triggers)?)
        })
        .await?;

        tracing::info!(
            agent_id = %agent_id,
            count = stored.len(),
            "Registered process triggers"
        );

        Ok(stored)
    }

    async fn delete_process_trigger_internal(
        &self,
        agent_id: &AgentId,
        process_id: &str,
    ) -> Result<()> {
        crate::triggers::validate_process_id(process_id)?;

        let aid = *agent_id;
        let pid = process_id.to_string();
        blocking_store(&self.store, move |s| {
            match s.delete_process_trigger(&aid, &pid) {
                Ok(()) => Ok(()),
                Err(aura_swarm_store::StoreError::NotFound) => {
                    Err(ControlError::TriggerNotFound(pid.clone()))
                }
                Err(e) => Err(e.into()),
            }
        })
        .await?;

        tracing::info!(agent_id = %agent_id, process_id = %process_id, "Deleted process trigger");

        Ok(())
    }

    async fn list_process_triggers(
        &self,
        user_id: &UserId,
        agent_id: &AgentId,
    ) -> Result<Vec<aura_swarm_store::ProcessTrigger>> {
        self.get_and_verify(user_id, agent_id).await?;

        let aid = *agent_id;
        blocking_store(&self.store, move |s| {
            Ok(s.list_process_triggers_by_agent(&aid)?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler_client::NoopSchedulerClient;
    use aura_swarm_store::RocksStore;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Mock KBS client recording provision/revoke calls, optionally failing
    /// provisioning to exercise create rollback. `existing` simulates DEKs
    /// already registered in the KBS (for the R2 backfill tests);
    /// `fail_exists` makes the existence check indeterminate.
    #[derive(Default)]
    struct MockKbsClient {
        provisioned: Mutex<Vec<String>>,
        revoked: Mutex<Vec<String>>,
        existing: Mutex<Vec<String>>,
        fail_provision: bool,
        fail_exists: bool,
    }

    impl MockKbsClient {
        fn failing() -> Self {
            Self {
                fail_provision: true,
                ..Self::default()
            }
        }

        fn with_existing(keys: Vec<String>) -> Self {
            Self {
                existing: Mutex::new(keys),
                ..Self::default()
            }
        }

        fn failing_exists() -> Self {
            Self {
                fail_exists: true,
                ..Self::default()
            }
        }

        fn provisioned(&self) -> Vec<String> {
            self.provisioned.lock().unwrap().clone()
        }

        fn revoked(&self) -> Vec<String> {
            self.revoked.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl KbsClient for MockKbsClient {
        async fn provision_dek(&self, key_id: &str) -> Result<()> {
            if self.fail_provision {
                return Err(ControlError::Internal("KBS unavailable".to_string()));
            }
            self.provisioned.lock().unwrap().push(key_id.to_string());
            Ok(())
        }

        async fn dek_exists(&self, key_id: &str) -> Result<bool> {
            if self.fail_exists {
                return Err(ControlError::Internal(
                    "KBS existence check failed".to_string(),
                ));
            }
            let pre_existing = self.existing.lock().unwrap().iter().any(|k| k == key_id);
            let provisioned = self.provisioned.lock().unwrap().iter().any(|k| k == key_id);
            Ok(pre_existing || provisioned)
        }

        async fn revoke_dek(&self, key_id: &str) -> Result<()> {
            self.revoked.lock().unwrap().push(key_id.to_string());
            Ok(())
        }
    }

    /// Mock scheduler client recording schedule/terminate calls (with the
    /// spec each pod was scheduled with) to verify recreate flows, and
    /// serving canned pod logs for the merge tests.
    #[derive(Default)]
    struct MockSchedulerClient {
        scheduled: Mutex<Vec<(AgentId, aura_swarm_store::AgentSpec)>>,
        terminated: Mutex<Vec<AgentId>>,
        termination_reasons: Mutex<Vec<String>>,
        /// Canned response for `get_pod_logs` (`None` = no pod).
        pod_logs: Mutex<Option<Vec<aura_swarm_store::LogLine>>>,
    }

    impl MockSchedulerClient {
        fn scheduled(&self) -> Vec<(AgentId, aura_swarm_store::AgentSpec)> {
            self.scheduled.lock().unwrap().clone()
        }

        fn terminated(&self) -> Vec<AgentId> {
            self.terminated.lock().unwrap().clone()
        }

        fn termination_reasons(&self) -> Vec<String> {
            self.termination_reasons.lock().unwrap().clone()
        }

        fn set_pod_logs(&self, logs: Option<Vec<aura_swarm_store::LogLine>>) {
            *self.pod_logs.lock().unwrap() = logs;
        }
    }

    #[async_trait]
    impl SchedulerClient for MockSchedulerClient {
        async fn schedule_agent(
            &self,
            agent_id: &AgentId,
            _user_id_hex: &str,
            _agent_name: &str,
            spec: &aura_swarm_store::AgentSpec,
        ) -> Result<()> {
            self.scheduled.lock().unwrap().push((*agent_id, spec.clone()));
            Ok(())
        }

        async fn terminate_agent(&self, agent_id: &AgentId, reason: &str) -> Result<()> {
            self.terminated.lock().unwrap().push(*agent_id);
            self.termination_reasons
                .lock()
                .unwrap()
                .push(reason.to_string());
            Ok(())
        }

        async fn get_pod_logs(
            &self,
            _agent_id: &AgentId,
            tail: u32,
            _since: Option<DateTime<Utc>>,
        ) -> Result<Option<Vec<aura_swarm_store::LogLine>>> {
            Ok(self.pod_logs.lock().unwrap().clone().map(|logs| {
                let skip = logs.len().saturating_sub(tail as usize);
                logs.into_iter().skip(skip).collect()
            }))
        }

        async fn get_pod_status(
            &self,
            _agent_id: &AgentId,
        ) -> Result<crate::scheduler_client::PodStatusResponse> {
            Ok(crate::scheduler_client::PodStatusResponse {
                phase: "Running".to_string(),
                ready: true,
                restart_count: 0,
                message: None,
            })
        }

        async fn get_pod_endpoint(&self, _agent_id: &AgentId) -> Result<Option<String>> {
            Ok(Some("localhost:8080".to_string()))
        }
    }

    fn setup() -> (
        ControlPlaneService<RocksStore, NoopSchedulerClient>,
        TempDir,
        UserId,
    ) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(RocksStore::open(dir.path()).unwrap());
        let config = ControlConfig {
            max_agents_per_user: 3,
            ..Default::default()
        };
        let service = ControlPlaneService::new(store, config);
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        (service, dir, user_id)
    }

    fn setup_with_kbs(
        kbs: Arc<MockKbsClient>,
    ) -> (
        ControlPlaneService<RocksStore, NoopSchedulerClient>,
        TempDir,
        UserId,
    ) {
        let (mut service, dir, user_id) = setup();
        service.set_kbs(kbs);
        (service, dir, user_id)
    }

    fn setup_with_scheduler(
        scheduler: Arc<MockSchedulerClient>,
    ) -> (
        ControlPlaneService<RocksStore, MockSchedulerClient>,
        TempDir,
        UserId,
    ) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(RocksStore::open(dir.path()).unwrap());
        let config = ControlConfig {
            max_agents_per_user: 3,
            ..Default::default()
        };
        let service = ControlPlaneService::with_scheduler(store, config, scheduler);
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        (service, dir, user_id)
    }

    /// A legacy (pre-migration) agent record: no tier, microVM isolation,
    /// plaintext state. Can no longer be produced by the create API.
    fn legacy_agent(user_id: UserId, status: AgentState) -> Agent {
        let now = Utc::now();
        Agent {
            agent_id: AgentId::generate(),
            user_id,
            name: "legacy-agent".to_string(),
            status,
            spec: aura_swarm_store::AgentSpec {
                cpu_millicores: 500,
                memory_mb: 512,
                runtime_version: "v1".to_string(),
                isolation: Some(aura_swarm_store::IsolationLevel::MicroVM),
                tier: None,
                storage_encryption: None,
            },
            created_at: now,
            updated_at: now,
            last_heartbeat_at: None,
            error_message: None,
        }
    }

    /// Billing checker wired to a mock zbilling that always reports an
    /// insufficient balance (fail-closed).
    async fn insufficient_billing() -> (Arc<BillingChecker>, wiremock::MockServer) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/usage/check"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sufficient": false,
                "balance_cents": 5,
                "required_cents": 30,
            })))
            .mount(&server)
            .await;

        let checker = BillingChecker::new_shared(crate::billing::BillingConfig {
            url: server.uri(),
            api_key: "test-key".to_string(),
            enabled: true,
            fail_closed: true,
            ..Default::default()
        })
        .unwrap();
        (checker, server)
    }

    #[tokio::test]
    async fn create_agent_success() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, created) = service.create_agent(&user_id, request).await.unwrap();

        assert!(created);
        assert_eq!(agent.name, "test-agent");
        assert_eq!(agent.user_id, user_id);
        assert_eq!(agent.status, AgentState::Provisioning);
    }

    #[tokio::test]
    async fn create_agent_defaults_to_standard_confidential_sealed() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();

        assert_eq!(agent.spec.tier.as_deref(), Some("standard"));
        assert_eq!(agent.spec.cpu_millicores, 1000);
        assert_eq!(agent.spec.memory_mb, 2048);
        assert_eq!(
            agent.spec.isolation,
            Some(aura_swarm_store::IsolationLevel::ConfidentialVM)
        );
        let enc = agent.spec.storage_encryption.expect("must be sealed");
        assert_eq!(
            enc.key_id(),
            format!("swarm/agents/{}/state-key", agent.agent_id)
        );
    }

    #[tokio::test]
    async fn create_agent_with_explicit_tier() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent").with_tier("pro");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();

        assert_eq!(agent.spec.tier.as_deref(), Some("pro"));
        assert_eq!(agent.spec.cpu_millicores, 2000);
        assert_eq!(agent.spec.memory_mb, 4096);
    }

    #[tokio::test]
    async fn create_agent_invalid_tier_rejected() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent").with_tier("mega");
        let result = service.create_agent(&user_id, request).await;
        assert!(matches!(result, Err(ControlError::InvalidTier(t)) if t == "mega"));
    }

    #[tokio::test]
    async fn create_agent_legacy_spec_maps_to_nearest_tier() {
        let (service, _dir, user_id) = setup();

        let cases = [(250u32, "small"), (500, "small"), (900, "standard"), (4000, "pro")];
        for (cpu, expected_tier) in cases {
            let spec = aura_swarm_store::AgentSpec {
                cpu_millicores: cpu,
                memory_mb: 512,
                runtime_version: "v1".to_string(),
                isolation: Some(aura_swarm_store::IsolationLevel::MicroVM),
                tier: None,
                storage_encryption: None,
            };
            let request = CreateAgentRequest::with_spec(format!("agent-{cpu}"), spec);
            let (agent, _) = service.create_agent(&user_id, request).await.unwrap();

            assert_eq!(
                agent.spec.tier.as_deref(),
                Some(expected_tier),
                "cpu={cpu} should map to {expected_tier}"
            );
            // Legacy spec input still yields a confidential, sealed agent
            // with tier resources — only runtime_version carries over.
            assert_eq!(
                agent.spec.isolation,
                Some(aura_swarm_store::IsolationLevel::ConfidentialVM)
            );
            assert!(agent.spec.storage_encryption.is_some());
            assert_eq!(agent.spec.runtime_version, "v1");

            // Clean up to stay under the 3-agent test quota.
            service
                .store
                .update_agent_status(&agent.agent_id, AgentState::Stopped)
                .unwrap();
            service
                .delete_agent(&user_id, &agent.agent_id)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn create_provisions_dek_for_confidential_agent() {
        let kbs = Arc::new(MockKbsClient::default());
        let (service, _dir, user_id) = setup_with_kbs(Arc::clone(&kbs));

        let (agent, created) = service
            .create_agent(&user_id, CreateAgentRequest::new("tee-agent"))
            .await
            .unwrap();
        assert!(created);

        let expected_key_id = format!("swarm/agents/{}/state-key", agent.agent_id);
        assert_eq!(kbs.provisioned(), vec![expected_key_id]);
        assert!(kbs.revoked().is_empty());
    }

    #[tokio::test]
    async fn create_fails_and_rolls_back_when_provision_fails() {
        let kbs = Arc::new(MockKbsClient::failing());
        let (service, _dir, user_id) = setup_with_kbs(Arc::clone(&kbs));

        let result = service
            .create_agent(&user_id, CreateAgentRequest::new("tee-agent"))
            .await;
        assert!(matches!(result, Err(ControlError::Internal(_))));
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("DEK"), "error should mention the DEK: {msg}");

        // No half-created agent must remain.
        let agents = service.list_agents(&user_id).await.unwrap();
        assert!(agents.is_empty(), "failed create must be rolled back");
    }

    #[tokio::test]
    async fn destroy_revokes_dek_for_confidential_agent() {
        let kbs = Arc::new(MockKbsClient::default());
        let (service, _dir, user_id) = setup_with_kbs(Arc::clone(&kbs));

        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("tee-agent"))
            .await
            .unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Stopped)
            .unwrap();

        service
            .delete_agent(&user_id, &agent.agent_id)
            .await
            .unwrap();

        let expected_key_id = format!("swarm/agents/{}/state-key", agent.agent_id);
        assert_eq!(kbs.revoked(), vec![expected_key_id]);
        assert!(service.store.get_agent(&agent.agent_id).unwrap().is_none());
    }

    #[tokio::test]
    async fn legacy_agent_makes_no_kbs_calls() {
        let kbs = Arc::new(MockKbsClient::default());
        let (service, _dir, user_id) = setup_with_kbs(Arc::clone(&kbs));

        // A legacy (pre-migration) agent can no longer be produced by the
        // create API, so insert one directly: no tier, no storage
        // encryption, plaintext state.
        let now = Utc::now();
        let legacy = Agent {
            agent_id: AgentId::generate(),
            user_id,
            name: "legacy-agent".to_string(),
            status: AgentState::Stopped,
            spec: aura_swarm_store::AgentSpec {
                cpu_millicores: 500,
                memory_mb: 512,
                runtime_version: "v1".to_string(),
                isolation: Some(aura_swarm_store::IsolationLevel::MicroVM),
                tier: None,
                storage_encryption: None,
            },
            created_at: now,
            updated_at: now,
            last_heartbeat_at: None,
            error_message: None,
        };
        service.store.put_agent(&legacy).unwrap();

        service
            .delete_agent(&user_id, &legacy.agent_id)
            .await
            .unwrap();

        assert!(kbs.provisioned().is_empty(), "legacy agents make no KBS calls");
        assert!(kbs.revoked().is_empty(), "legacy agents make no KBS calls");
    }

    // =====================================================================
    // R2 DEK backfill (post-migration reconciliation)
    // =====================================================================

    /// Insert a sealed agent record directly, simulating a record the
    /// v1 → v2 store migration produced (sealed spec, no DEK in the KBS).
    fn put_sealed_agent(
        service: &ControlPlaneService<RocksStore, NoopSchedulerClient>,
        user_id: UserId,
        status: AgentState,
    ) -> Agent {
        let agent_id = AgentId::generate();
        let now = Utc::now();
        let agent = Agent {
            agent_id,
            user_id,
            name: "migrated-agent".to_string(),
            status,
            spec: BoxTier::Standard.to_spec(&agent_id, "latest"),
            created_at: now,
            updated_at: now,
            last_heartbeat_at: None,
            error_message: None,
        };
        service.store.put_agent(&agent).unwrap();
        agent
    }

    #[tokio::test]
    async fn dek_backfill_provisions_missing_and_skips_plaintext() {
        let kbs = Arc::new(MockKbsClient::default());
        let (service, _dir, user_id) = setup_with_kbs(Arc::clone(&kbs));

        let migrated = put_sealed_agent(&service, user_id, AgentState::Hibernating);

        // A plaintext record (no storage_encryption) must be ignored —
        // possible on a v1 DB restored from backup before migration runs.
        let now = Utc::now();
        let plaintext = Agent {
            agent_id: AgentId::generate(),
            user_id,
            name: "plaintext".to_string(),
            status: AgentState::Stopped,
            spec: aura_swarm_store::AgentSpec::default(),
            created_at: now,
            updated_at: now,
            last_heartbeat_at: None,
            error_message: None,
        };
        service.store.put_agent(&plaintext).unwrap();

        let summary = service.backfill_missing_deks().await.unwrap();
        assert_eq!(summary.sealed_agents, 1);
        assert_eq!(summary.provisioned, 1);
        assert_eq!(summary.already_present, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(
            kbs.provisioned(),
            vec![format!("swarm/agents/{}/state-key", migrated.agent_id)]
        );
    }

    #[tokio::test]
    async fn dek_backfill_never_overwrites_existing_dek() {
        let (mut service, _dir, user_id) = setup();
        let agent = put_sealed_agent(&service, user_id, AgentState::Running);

        let existing_key = format!("swarm/agents/{}/state-key", agent.agent_id);
        let kbs = Arc::new(MockKbsClient::with_existing(vec![existing_key]));
        service.set_kbs(Arc::clone(&kbs) as Arc<dyn KbsClient>);

        let summary = service.backfill_missing_deks().await.unwrap();
        assert_eq!(summary.sealed_agents, 1);
        assert_eq!(summary.already_present, 1);
        assert_eq!(summary.provisioned, 0);
        assert!(
            kbs.provisioned().is_empty(),
            "an existing DEK must never be re-provisioned (overwrite would brick sealed state)"
        );
    }

    #[tokio::test]
    async fn dek_backfill_skips_on_indeterminate_existence() {
        let (mut service, _dir, user_id) = setup();
        put_sealed_agent(&service, user_id, AgentState::Running);

        let kbs = Arc::new(MockKbsClient::failing_exists());
        service.set_kbs(Arc::clone(&kbs) as Arc<dyn KbsClient>);

        let summary = service.backfill_missing_deks().await.unwrap();
        assert_eq!(summary.sealed_agents, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.provisioned, 0);
        assert!(
            kbs.provisioned().is_empty(),
            "unknown existence must never provision blind"
        );
    }

    #[tokio::test]
    async fn dek_backfill_is_idempotent_across_reruns() {
        let kbs = Arc::new(MockKbsClient::default());
        let (service, _dir, user_id) = setup_with_kbs(Arc::clone(&kbs));
        put_sealed_agent(&service, user_id, AgentState::Hibernating);

        let first = service.backfill_missing_deks().await.unwrap();
        assert_eq!(first.provisioned, 1);

        // The mock reports provisioned keys as existing, mirroring the
        // real KBS after the first pass.
        let second = service.backfill_missing_deks().await.unwrap();
        assert_eq!(second.provisioned, 0);
        assert_eq!(second.already_present, 1);
        assert_eq!(kbs.provisioned().len(), 1, "exactly one provision ever");
    }

    #[tokio::test]
    async fn dek_backfill_provision_failure_counts_and_continues() {
        let (mut service, _dir, user_id) = setup();
        put_sealed_agent(&service, user_id, AgentState::Hibernating);

        // dek_exists answers definitively (absent) but provisioning fails:
        // the agent is counted failed and retried on the next startup.
        let kbs = Arc::new(MockKbsClient::failing());
        service.set_kbs(Arc::clone(&kbs) as Arc<dyn KbsClient>);

        let summary = service.backfill_missing_deks().await.unwrap();
        assert_eq!(summary.sealed_agents, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.provisioned, 0);
    }

    #[tokio::test]
    async fn dek_backfill_without_kbs_is_a_noop() {
        let (service, _dir, user_id) = setup();
        put_sealed_agent(&service, user_id, AgentState::Hibernating);

        let summary = service.backfill_missing_deks().await.unwrap();
        assert_eq!(summary, DekBackfillSummary::default());
    }

    #[tokio::test]
    async fn schema_version_reported_through_control_plane() {
        let (service, _dir, _user_id) = setup();
        // Fresh test DBs are at v1 until the gateway runs migrations.
        assert_eq!(service.schema_version().await.unwrap(), 1);
        aura_swarm_store::migrations::run(service.store()).unwrap();
        assert_eq!(
            service.schema_version().await.unwrap(),
            aura_swarm_store::CURRENT_SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn create_agent_with_supplied_id() {
        let (service, _dir, user_id) = setup();

        let supplied_id = AgentId::from_uuid(uuid::Uuid::new_v4());
        let request = CreateAgentRequest::new("test-agent").with_agent_id(supplied_id);
        let (agent, created) = service.create_agent(&user_id, request).await.unwrap();

        assert!(created);
        assert_eq!(agent.agent_id, supplied_id);
        assert_eq!(agent.name, "test-agent");
    }

    #[tokio::test]
    async fn create_agent_idempotent_same_user() {
        let (service, _dir, user_id) = setup();

        let supplied_id = AgentId::from_uuid(uuid::Uuid::new_v4());
        let request = CreateAgentRequest::new("test-agent").with_agent_id(supplied_id);
        let (agent1, created1) = service.create_agent(&user_id, request).await.unwrap();
        assert!(created1);

        let request2 = CreateAgentRequest::new("test-agent").with_agent_id(supplied_id);
        let (agent2, created2) = service.create_agent(&user_id, request2).await.unwrap();
        assert!(!created2);
        assert_eq!(agent1.agent_id, agent2.agent_id);
        assert_eq!(agent1.created_at, agent2.created_at);
    }

    #[tokio::test]
    async fn create_agent_conflict_different_user() {
        let (service, _dir, user_id) = setup();
        let other_user = UserId::from_uuid(uuid::Uuid::new_v4());

        let supplied_id = AgentId::from_uuid(uuid::Uuid::new_v4());
        let request = CreateAgentRequest::new("test-agent").with_agent_id(supplied_id);
        service.create_agent(&user_id, request).await.unwrap();

        let request2 = CreateAgentRequest::new("test-agent").with_agent_id(supplied_id);
        let result = service.create_agent(&other_user, request2).await;
        assert!(matches!(
            result,
            Err(ControlError::AgentAlreadyExists { .. })
        ));
    }

    #[tokio::test]
    async fn create_agent_idempotent_does_not_count_toward_quota() {
        let (service, _dir, user_id) = setup();

        // Create max agents (quota is 3)
        let mut ids = Vec::new();
        for i in 0..3 {
            let request = CreateAgentRequest::new(format!("agent-{i}"));
            let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
            ids.push(agent.agent_id);
        }

        // Idempotent re-create of the first agent should succeed
        let request = CreateAgentRequest::new("agent-0").with_agent_id(ids[0]);
        let (agent, created) = service.create_agent(&user_id, request).await.unwrap();
        assert!(!created);
        assert_eq!(agent.agent_id, ids[0]);
    }

    #[tokio::test]
    async fn create_agent_quota_exceeded() {
        let (service, _dir, user_id) = setup();

        // Create max agents
        for i in 0..3 {
            let request = CreateAgentRequest::new(format!("agent-{i}"));
            service.create_agent(&user_id, request).await.unwrap();
        }

        // Try to create one more (without supplied ID → no idempotent match)
        let request = CreateAgentRequest::new("agent-overflow");
        let result = service.create_agent(&user_id, request).await;

        assert!(matches!(
            result,
            Err(ControlError::QuotaExceeded { limit: 3, .. })
        ));
    }

    #[tokio::test]
    async fn get_agent_not_owner() {
        let (service, _dir, user_id) = setup();
        let other_user = UserId::from_uuid(uuid::Uuid::new_v4());

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();

        let result = service.get_agent(&other_user, &agent.agent_id).await;
        assert!(matches!(result, Err(ControlError::NotOwner { .. })));
    }

    #[tokio::test]
    async fn agent_lifecycle() {
        let (service, _dir, user_id) = setup();

        // Create agent (starts in Provisioning)
        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        assert_eq!(agent.status, AgentState::Provisioning);

        // Simulate provisioning complete (normally done by scheduler)
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        // Hibernate
        let agent = service
            .hibernate_agent(&user_id, &agent.agent_id)
            .await
            .unwrap();
        assert_eq!(agent.status, AgentState::Hibernating);

        // Wake (goes through Provisioning for scheduler)
        let agent = service.wake_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(agent.status, AgentState::Provisioning);

        // Simulate provisioning complete
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        // Stop
        let agent = service.stop_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(agent.status, AgentState::Stopping);

        // Simulate stop complete
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Stopped)
            .unwrap();

        // Delete
        service
            .delete_agent(&user_id, &agent.agent_id)
            .await
            .unwrap();
        assert!(service.store.get_agent(&agent.agent_id).unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_requires_stopped() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();

        // Simulate running
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        // Try to delete while running
        let result = service.delete_agent(&user_id, &agent.agent_id).await;
        assert!(matches!(result, Err(ControlError::InvalidState { .. })));
    }

    #[tokio::test]
    async fn session_lifecycle() {
        let (service, _dir, user_id) = setup();

        // Create and start agent
        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        // Create session
        let session = service
            .create_session(
                &user_id,
                &agent.agent_id,
                aura_swarm_store::SessionConfig::default(),
            )
            .await
            .unwrap();
        assert_eq!(session.status, aura_swarm_store::SessionStatus::Active);

        // Get session
        let retrieved = service
            .get_session(&user_id, &session.session_id)
            .await
            .unwrap();
        assert_eq!(retrieved.session_id, session.session_id);

        // List sessions
        let sessions = service
            .list_sessions(&user_id, &agent.agent_id)
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);

        // Close session
        service
            .close_session(&user_id, &session.session_id)
            .await
            .unwrap();

        // Agent should transition to Idle
        let agent = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(agent.status, AgentState::Idle);
    }

    #[tokio::test]
    async fn heartbeat_updates_timestamp() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();

        assert!(agent.last_heartbeat_at.is_none());

        service.process_heartbeat(&agent.agent_id).await.unwrap();

        let updated = service.store.get_agent(&agent.agent_id).unwrap().unwrap();
        assert!(updated.last_heartbeat_at.is_some());
    }

    #[tokio::test]
    async fn resolve_endpoint_active() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        let endpoint = service
            .resolve_agent_endpoint(&agent.agent_id)
            .await
            .unwrap();
        assert!(endpoint.is_some());
    }

    #[tokio::test]
    async fn resolve_endpoint_stopped() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Stopped)
            .unwrap();

        let endpoint = service
            .resolve_agent_endpoint(&agent.agent_id)
            .await
            .unwrap();
        assert!(endpoint.is_none());
    }

    #[tokio::test]
    async fn get_agent_not_found() {
        let (service, _dir, user_id) = setup();
        let fake_id = AgentId::from_uuid(uuid::Uuid::new_v4());

        let result = service.get_agent(&user_id, &fake_id).await;
        assert!(matches!(result, Err(ControlError::AgentNotFound(_))));
    }

    #[tokio::test]
    async fn list_agents_empty() {
        let (service, _dir, user_id) = setup();

        let agents = service.list_agents(&user_id).await.unwrap();
        assert!(agents.is_empty());
    }

    #[tokio::test]
    async fn list_agents_multiple() {
        let (service, _dir, user_id) = setup();

        for i in 0..3 {
            let request = CreateAgentRequest::new(format!("agent-{i}"));
            service.create_agent(&user_id, request).await.unwrap();
        }

        let agents = service.list_agents(&user_id).await.unwrap();
        assert_eq!(agents.len(), 3);
    }

    #[tokio::test]
    async fn start_agent_from_stopped() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Stopped)
            .unwrap();

        let agent = service
            .start_agent(&user_id, &agent.agent_id)
            .await
            .unwrap();
        assert_eq!(agent.status, AgentState::Provisioning);
    }

    #[tokio::test]
    async fn start_agent_invalid_state_from_running() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        let result = service.start_agent(&user_id, &agent.agent_id).await;
        assert!(matches!(result, Err(ControlError::InvalidState { .. })));
    }

    #[tokio::test]
    async fn stop_agent_from_running() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        let agent = service.stop_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(agent.status, AgentState::Stopping);
    }

    #[tokio::test]
    async fn restart_agent_from_running() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        let agent = service
            .restart_agent(&user_id, &agent.agent_id)
            .await
            .unwrap();
        assert_eq!(agent.status, AgentState::Provisioning);
    }

    #[tokio::test]
    async fn hibernate_agent_invalid_state() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        assert_eq!(agent.status, AgentState::Provisioning);

        let result = service.hibernate_agent(&user_id, &agent.agent_id).await;
        assert!(matches!(result, Err(ControlError::InvalidState { .. })));
    }

    #[tokio::test]
    async fn wake_agent_invalid_state() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        let result = service.wake_agent(&user_id, &agent.agent_id).await;
        assert!(matches!(result, Err(ControlError::InvalidState { .. })));
    }

    #[tokio::test]
    async fn update_status_internal_success() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();

        service
            .update_agent_status_internal(&agent.agent_id, AgentState::Running, None)
            .await
            .unwrap();

        let updated = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(updated.status, AgentState::Running);
    }

    #[tokio::test]
    async fn update_status_internal_not_found() {
        let (service, _dir, _user_id) = setup();
        let fake_id = AgentId::from_uuid(uuid::Uuid::new_v4());

        let result = service
            .update_agent_status_internal(&fake_id, AgentState::Running, None)
            .await;
        assert!(matches!(result, Err(ControlError::AgentNotFound(_))));
    }

    #[tokio::test]
    async fn update_status_internal_with_error_message() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();

        service
            .update_agent_status_internal(
                &agent.agent_id,
                AgentState::Error,
                Some("pod crashed".to_string()),
            )
            .await
            .unwrap();

        let updated = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(updated.status, AgentState::Error);
        assert_eq!(updated.error_message.as_deref(), Some("pod crashed"));
    }

    #[tokio::test]
    async fn heartbeat_missing_agent() {
        let (service, _dir, _user_id) = setup();
        let fake_id = AgentId::from_uuid(uuid::Uuid::new_v4());

        let result = service.process_heartbeat(&fake_id).await;
        assert!(matches!(result, Err(ControlError::AgentNotFound(_))));
    }

    #[tokio::test]
    async fn internal_running_does_not_overwrite_idle() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();

        // Simulate scheduler pushing Running (Provisioning → Running)
        service
            .update_agent_status_internal(&agent.agent_id, AgentState::Running, None)
            .await
            .unwrap();
        let a = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(a.status, AgentState::Running);

        // Control plane transitions to Idle (last session closed)
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Idle)
            .unwrap();
        let a = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(a.status, AgentState::Idle);

        // Scheduler pushes Running again — should be a no-op
        service
            .update_agent_status_internal(&agent.agent_id, AgentState::Running, None)
            .await
            .unwrap();
        let a = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(
            a.status,
            AgentState::Idle,
            "Idle must not be overwritten by scheduler Running"
        );
    }

    #[tokio::test]
    async fn internal_running_does_not_overwrite_running() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();

        service
            .update_agent_status_internal(&agent.agent_id, AgentState::Running, None)
            .await
            .unwrap();

        // Second push — still a no-op (already Running)
        service
            .update_agent_status_internal(&agent.agent_id, AgentState::Running, None)
            .await
            .unwrap();
        let a = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(a.status, AgentState::Running);
    }

    #[tokio::test]
    async fn internal_stopped_does_not_overwrite_active_agent() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .update_agent_status_internal(&agent.agent_id, AgentState::Running, None)
            .await
            .unwrap();

        service
            .update_agent_status_internal(
                &agent.agent_id,
                AgentState::Stopped,
                Some("Pod deleted".to_string()),
            )
            .await
            .unwrap();

        let a = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(
            a.status,
            AgentState::Running,
            "Active agents must remain eligible for pod recreation when a pod disappears"
        );
    }

    #[tokio::test]
    async fn internal_stopped_does_not_overwrite_idle_agent() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Idle)
            .unwrap();

        service
            .update_agent_status_internal(
                &agent.agent_id,
                AgentState::Stopped,
                Some("Pod deleted".to_string()),
            )
            .await
            .unwrap();

        let a = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(
            a.status,
            AgentState::Idle,
            "Idle agents must keep their logical state while the scheduler recreates a missing pod"
        );
    }

    #[tokio::test]
    async fn internal_stopped_does_not_overwrite_hibernating_agent() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Hibernating)
            .unwrap();

        service
            .update_agent_status_internal(
                &agent.agent_id,
                AgentState::Stopped,
                Some("Pod deleted".to_string()),
            )
            .await
            .unwrap();

        let a = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(
            a.status,
            AgentState::Hibernating,
            "Hibernating agents must not be rewritten to Stopped when their pod deletion is acknowledged"
        );
    }

    #[tokio::test]
    async fn internal_stopped_applies_for_explicit_shutdown() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Stopping)
            .unwrap();

        service
            .update_agent_status_internal(
                &agent.agent_id,
                AgentState::Stopped,
                Some("Pod deleted".to_string()),
            )
            .await
            .unwrap();

        let a = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(
            a.status,
            AgentState::Stopped,
            "Explicit shutdown must still complete the Stopping -> Stopped transition"
        );
    }

    #[tokio::test]
    async fn internal_error_still_applies_over_idle() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Idle)
            .unwrap();

        // Error should always go through regardless of current state
        service
            .update_agent_status_internal(
                &agent.agent_id,
                AgentState::Error,
                Some("OOM".to_string()),
            )
            .await
            .unwrap();
        let a = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(a.status, AgentState::Error);
        assert_eq!(a.error_message.as_deref(), Some("OOM"));
    }

    #[tokio::test]
    async fn internal_error_without_message_is_ignored() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Idle)
            .unwrap();

        service
            .update_agent_status_internal(&agent.agent_id, AgentState::Error, None)
            .await
            .unwrap();

        let a = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(a.status, AgentState::Idle);
        assert_eq!(a.error_message, None);
    }

    #[tokio::test]
    async fn count_active_sessions_empty() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let (agent, _) = service.create_agent(&user_id, request).await.unwrap();

        let count = service
            .count_active_sessions(&agent.agent_id)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn reconcile_idle_agents_transitions_stale_running() {
        let (service, _dir, user_id) = setup();

        let (a1, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("agent-1"))
            .await
            .unwrap();
        let (a2, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("agent-2"))
            .await
            .unwrap();

        // Force both to Running (simulating scheduler push)
        service
            .store
            .update_agent_status(&a1.agent_id, AgentState::Running)
            .unwrap();
        service
            .store
            .update_agent_status(&a2.agent_id, AgentState::Running)
            .unwrap();

        let reconciled = service.reconcile_idle_agents().await.unwrap();
        assert_eq!(reconciled, 2);

        let a1 = service.get_agent(&user_id, &a1.agent_id).await.unwrap();
        let a2 = service.get_agent(&user_id, &a2.agent_id).await.unwrap();
        assert_eq!(a1.status, AgentState::Idle);
        assert_eq!(a2.status, AgentState::Idle);
    }

    #[tokio::test]
    async fn reconcile_closes_orphaned_sessions_then_idles() {
        let (service, _dir, user_id) = setup();

        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("agent-1"))
            .await
            .unwrap();

        // Force to Running and create a session (simulating an orphan)
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();
        let sess = service
            .create_session(&user_id, &agent.agent_id, Default::default())
            .await
            .unwrap();

        // Reconciliation should close the orphaned session and transition to Idle
        let reconciled = service.reconcile_idle_agents().await.unwrap();
        assert_eq!(reconciled, 1);

        let a = service.get_agent(&user_id, &agent.agent_id).await.unwrap();
        assert_eq!(a.status, AgentState::Idle);

        // Session should be closed
        let s = service
            .get_session(&user_id, &sess.session_id)
            .await
            .unwrap();
        assert_eq!(s.status, SessionStatus::Closed);
    }

    // =========================================================================
    // Process triggers (Swarm TEE upgrade phase 8)
    // =========================================================================

    fn registration(process_id: &str, cron: &str) -> crate::triggers::TriggerRegistration {
        serde_json::from_value(serde_json::json!({
            "process_id": process_id,
            "cron": cron,
            "enabled": true,
            "next_run_at": chrono::Utc::now(),
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn replace_triggers_sync_and_owner_read() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("cron-agent"))
            .await
            .unwrap();

        // First sync: two triggers.
        let stored = service
            .replace_process_triggers_internal(
                &agent.agent_id,
                vec![registration("p1", "*/5 * * * *"), registration("p2", "0 0 * * *")],
            )
            .await
            .unwrap();
        assert_eq!(stored.len(), 2);

        // Second sync drops p2, keeps p1: replace semantics.
        let stored = service
            .replace_process_triggers_internal(
                &agent.agent_id,
                vec![registration("p1", "*/10 * * * *")],
            )
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);

        let listed = service
            .list_process_triggers(&user_id, &agent.agent_id)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].process_id, "p1");
        assert_eq!(listed[0].cron, "*/10 * * * *");
    }

    #[tokio::test]
    async fn replace_triggers_rejects_invalid_cron_atomically() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("cron-agent"))
            .await
            .unwrap();

        let result = service
            .replace_process_triggers_internal(
                &agent.agent_id,
                vec![registration("ok", "*/5 * * * *"), registration("bad", "not a cron")],
            )
            .await;
        assert!(matches!(result, Err(ControlError::InvalidTrigger(_))));

        // Nothing was applied — the sync is all-or-nothing.
        let listed = service
            .list_process_triggers(&user_id, &agent.agent_id)
            .await
            .unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn replace_triggers_unknown_agent() {
        let (service, _dir, _user_id) = setup();
        let ghost = AgentId::generate();

        let result = service
            .replace_process_triggers_internal(&ghost, vec![registration("p1", "*/5 * * * *")])
            .await;
        assert!(matches!(result, Err(ControlError::AgentNotFound(_))));
    }

    #[tokio::test]
    async fn list_triggers_requires_ownership() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("cron-agent"))
            .await
            .unwrap();

        let other = UserId::from_uuid(uuid::Uuid::new_v4());
        let result = service.list_process_triggers(&other, &agent.agent_id).await;
        assert!(matches!(result, Err(ControlError::NotOwner { .. })));
    }

    #[tokio::test]
    async fn delete_trigger_internal_and_not_found() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("cron-agent"))
            .await
            .unwrap();

        service
            .replace_process_triggers_internal(
                &agent.agent_id,
                vec![registration("p1", "*/5 * * * *")],
            )
            .await
            .unwrap();

        service
            .delete_process_trigger_internal(&agent.agent_id, "p1")
            .await
            .unwrap();

        let result = service
            .delete_process_trigger_internal(&agent.agent_id, "p1")
            .await;
        assert!(matches!(result, Err(ControlError::TriggerNotFound(_))));
    }

    #[tokio::test]
    async fn destroy_agent_cleans_registered_triggers() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("cron-agent"))
            .await
            .unwrap();

        service
            .replace_process_triggers_internal(
                &agent.agent_id,
                vec![registration("p1", "*/5 * * * *"), registration("p2", "0 0 * * *")],
            )
            .await
            .unwrap();

        // Drive to a terminal state, then destroy.
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Stopped)
            .unwrap();
        service.delete_agent(&user_id, &agent.agent_id).await.unwrap();

        // Triggers must not outlive the agent.
        assert!(service
            .store
            .list_process_triggers_by_agent(&agent.agent_id)
            .unwrap()
            .is_empty());
        assert!(service.store.list_all_process_triggers().unwrap().is_empty());
    }

    // =========================================================================
    // Tier changes (Swarm TEE upgrade phase 10)
    // =========================================================================

    use aura_swarm_store::UsageEventKind;

    #[tokio::test]
    async fn change_tier_invalid_tier_rejected() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("tier-agent"))
            .await
            .unwrap();

        let result = service.change_tier(&user_id, &agent.agent_id, "mega").await;
        assert!(matches!(result, Err(ControlError::InvalidTier(t)) if t == "mega"));
    }

    #[tokio::test]
    async fn change_tier_requires_ownership() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("tier-agent"))
            .await
            .unwrap();

        let other = UserId::from_uuid(uuid::Uuid::new_v4());
        let result = service.change_tier(&other, &agent.agent_id, "pro").await;
        assert!(matches!(result, Err(ControlError::NotOwner { .. })));
    }

    #[tokio::test]
    async fn change_tier_same_tier_is_noop() {
        let scheduler = Arc::new(MockSchedulerClient::default());
        let (service, _dir, user_id) = setup_with_scheduler(Arc::clone(&scheduler));

        // Default create is "standard"; agent is Running.
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("tier-agent"))
            .await
            .unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        let outcome = service
            .change_tier(&user_id, &agent.agent_id, "standard")
            .await
            .unwrap();
        assert!(!outcome.changed);
        assert!(!outcome.pod_recreated);
        assert_eq!(outcome.previous_tier.as_deref(), Some("standard"));
        assert_eq!(outcome.tier, "standard");
        assert_eq!(outcome.agent.status, AgentState::Running);

        // No pod churn beyond the create, and no tier-change event
        // recorded (the only event is the create's PodScheduled).
        assert!(scheduler.terminated().is_empty());
        assert_eq!(scheduler.scheduled().len(), 1);
        let events = service
            .store
            .list_usage_events_by_agent(&agent.agent_id)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].kind,
            UsageEventKind::PodScheduled { .. }
        ));
    }

    #[tokio::test]
    async fn change_tier_asleep_updates_record_only() {
        let scheduler = Arc::new(MockSchedulerClient::default());
        let (service, _dir, user_id) = setup_with_scheduler(Arc::clone(&scheduler));

        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("tier-agent"))
            .await
            .unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Hibernating)
            .unwrap();

        let outcome = service
            .change_tier(&user_id, &agent.agent_id, "pro")
            .await
            .unwrap();
        assert!(outcome.changed);
        assert!(!outcome.pod_recreated, "asleep agents must not churn pods");
        assert_eq!(outcome.previous_tier.as_deref(), Some("standard"));
        assert_eq!(outcome.tier, "pro");

        // Record updated in place; state untouched until next wake.
        let stored = service.store.get_agent(&agent.agent_id).unwrap().unwrap();
        assert_eq!(stored.status, AgentState::Hibernating);
        assert_eq!(stored.spec.tier.as_deref(), Some("pro"));
        assert_eq!(stored.spec.cpu_millicores, 2000);
        assert_eq!(stored.spec.memory_mb, 4096);

        // No pod calls beyond the original create.
        assert!(scheduler.terminated().is_empty());
        assert_eq!(scheduler.scheduled().len(), 1);

        // TierChanged event with both hourly prices (after the create's
        // PodScheduled).
        let events = service
            .store
            .list_usage_events_by_agent(&agent.agent_id)
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].kind,
            UsageEventKind::PodScheduled { .. }
        ));
        assert_eq!(
            events[1].kind,
            UsageEventKind::TierChanged {
                from: Some("standard".to_string()),
                to: "pro".to_string(),
                from_hourly_price_cents: Some(BoxTier::Standard.hourly_price_cents()),
                to_hourly_price_cents: BoxTier::Pro.hourly_price_cents(),
            }
        );
    }

    #[tokio::test]
    async fn change_tier_awake_recreates_pod_and_closes_sessions() {
        let scheduler = Arc::new(MockSchedulerClient::default());
        let (service, _dir, user_id) = setup_with_scheduler(Arc::clone(&scheduler));

        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("tier-agent"))
            .await
            .unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();
        let session = service
            .create_session(&user_id, &agent.agent_id, SessionConfig::default())
            .await
            .unwrap();

        let outcome = service
            .change_tier(&user_id, &agent.agent_id, "small")
            .await
            .unwrap();
        assert!(outcome.changed);
        assert!(outcome.pod_recreated);
        assert_eq!(outcome.tier, "small");
        assert_eq!(outcome.agent.status, AgentState::Provisioning);

        // Recreate-with-state: terminate then schedule with the new spec.
        assert_eq!(scheduler.terminated(), vec![agent.agent_id]);
        let schedule_calls = scheduler.scheduled();
        assert_eq!(schedule_calls.len(), 2, "create + tier-change recreate");
        let (sched_id, sched_spec) = &schedule_calls[1];
        assert_eq!(*sched_id, agent.agent_id);
        assert_eq!(sched_spec.tier.as_deref(), Some("small"));
        assert_eq!(sched_spec.cpu_millicores, 500);
        assert_eq!(sched_spec.memory_mb, 1024);

        // The in-flight session was closed by the recreate.
        let sess = service
            .get_session(&user_id, &session.session_id)
            .await
            .unwrap();
        assert_eq!(sess.status, SessionStatus::Closed);

        // Full event sequence: create's PodScheduled, then the recreate's
        // terminate (priced at the OLD tier), the TierChanged record, and
        // the replacement pod's schedule (priced at the NEW tier).
        let events = service
            .store
            .list_usage_events_by_agent(&agent.agent_id)
            .unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(
            events[0].kind,
            UsageEventKind::PodScheduled {
                tier: Some("standard".to_string()),
                hourly_price_cents: Some(BoxTier::Standard.hourly_price_cents()),
            }
        );
        assert_eq!(
            events[1].kind,
            UsageEventKind::PodTerminated {
                tier: Some("standard".to_string()),
                hourly_price_cents: Some(BoxTier::Standard.hourly_price_cents()),
                reason: "tier_change".to_string(),
            }
        );
        assert_eq!(
            events[2].kind,
            UsageEventKind::TierChanged {
                from: Some("standard".to_string()),
                to: "small".to_string(),
                from_hourly_price_cents: Some(BoxTier::Standard.hourly_price_cents()),
                to_hourly_price_cents: BoxTier::Small.hourly_price_cents(),
            }
        );
        assert_eq!(
            events[3].kind,
            UsageEventKind::PodScheduled {
                tier: Some("small".to_string()),
                hourly_price_cents: Some(BoxTier::Small.hourly_price_cents()),
            }
        );
    }

    #[tokio::test]
    async fn change_tier_rejected_mid_transition() {
        let (service, _dir, user_id) = setup();

        // Fresh create leaves the agent in Provisioning.
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("tier-agent"))
            .await
            .unwrap();

        let result = service.change_tier(&user_id, &agent.agent_id, "pro").await;
        assert!(matches!(result, Err(ControlError::InvalidState { .. })));

        // Nothing changed and no tier-change event was recorded (only the
        // create's PodScheduled exists).
        let stored = service.store.get_agent(&agent.agent_id).unwrap().unwrap();
        assert_eq!(stored.spec.tier.as_deref(), Some("standard"));
        let events = service
            .store
            .list_usage_events_by_agent(&agent.agent_id)
            .unwrap();
        assert!(!events
            .iter()
            .any(|e| matches!(e.kind, UsageEventKind::TierChanged { .. })));
    }

    #[tokio::test]
    async fn change_tier_legacy_asleep_provisions_dek_and_converts() {
        let kbs = Arc::new(MockKbsClient::default());
        let (service, _dir, user_id) = setup_with_kbs(Arc::clone(&kbs));

        let legacy = legacy_agent(user_id, AgentState::Hibernating);
        service.store.put_agent(&legacy).unwrap();

        let outcome = service
            .change_tier(&user_id, &legacy.agent_id, "standard")
            .await
            .unwrap();
        assert!(outcome.changed);
        assert!(!outcome.pod_recreated);
        assert_eq!(outcome.previous_tier, None, "legacy agents have no tier");

        // The DEK was provisioned under the deterministic key id even though
        // no pod recreate happens until the next wake.
        let expected_key_id = format!("swarm/agents/{}/state-key", legacy.agent_id);
        assert_eq!(kbs.provisioned(), vec![expected_key_id.clone()]);

        // Converted to the new architecture: confidential + sealed.
        let stored = service.store.get_agent(&legacy.agent_id).unwrap().unwrap();
        assert_eq!(stored.spec.tier.as_deref(), Some("standard"));
        assert_eq!(
            stored.spec.isolation,
            Some(aura_swarm_store::IsolationLevel::ConfidentialVM)
        );
        assert_eq!(
            stored.spec.storage_encryption.as_ref().unwrap().key_id(),
            expected_key_id
        );
        assert_eq!(stored.status, AgentState::Hibernating);

        // Event records the legacy origin: no from-tier, no from-price.
        let events = service
            .store
            .list_usage_events_by_agent(&legacy.agent_id)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].kind,
            UsageEventKind::TierChanged {
                from: None,
                to: "standard".to_string(),
                from_hourly_price_cents: None,
                to_hourly_price_cents: BoxTier::Standard.hourly_price_cents(),
            }
        );
    }

    #[tokio::test]
    async fn change_tier_legacy_awake_provisions_dek_before_recreate() {
        let scheduler = Arc::new(MockSchedulerClient::default());
        let (mut service, _dir, user_id) = setup_with_scheduler(Arc::clone(&scheduler));
        let kbs = Arc::new(MockKbsClient::default());
        let kbs_for_service: Arc<MockKbsClient> = Arc::clone(&kbs);
        service.set_kbs(kbs_for_service);

        let legacy = legacy_agent(user_id, AgentState::Idle);
        service.store.put_agent(&legacy).unwrap();

        let outcome = service
            .change_tier(&user_id, &legacy.agent_id, "pro")
            .await
            .unwrap();
        assert!(outcome.pod_recreated);

        // DEK provisioned, pod recreated on the new (confidential) spec.
        let expected_key_id = format!("swarm/agents/{}/state-key", legacy.agent_id);
        assert_eq!(kbs.provisioned(), vec![expected_key_id]);
        assert_eq!(scheduler.terminated(), vec![legacy.agent_id]);
        let schedule_calls = scheduler.scheduled();
        assert_eq!(schedule_calls.len(), 1);
        assert_eq!(
            schedule_calls[0].1.isolation,
            Some(aura_swarm_store::IsolationLevel::ConfidentialVM)
        );
        assert_eq!(schedule_calls[0].1.tier.as_deref(), Some("pro"));
    }

    #[tokio::test]
    async fn change_tier_does_not_reprovision_existing_dek() {
        let kbs = Arc::new(MockKbsClient::default());
        let (service, _dir, user_id) = setup_with_kbs(Arc::clone(&kbs));

        // Create provisions the DEK once.
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("tee-agent"))
            .await
            .unwrap();
        assert_eq!(kbs.provisioned().len(), 1);
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        service
            .change_tier(&user_id, &agent.agent_id, "pro")
            .await
            .unwrap();

        // Re-provisioning would overwrite the DEK and brick the sealed
        // state; a tiered agent's tier change must never touch the KBS.
        assert_eq!(kbs.provisioned().len(), 1);
        assert!(kbs.revoked().is_empty());
    }

    #[tokio::test]
    async fn change_tier_legacy_dek_provision_failure_aborts() {
        let kbs = Arc::new(MockKbsClient::failing());
        let (service, _dir, user_id) = setup_with_kbs(Arc::clone(&kbs));

        let legacy = legacy_agent(user_id, AgentState::Hibernating);
        service.store.put_agent(&legacy).unwrap();

        let result = service.change_tier(&user_id, &legacy.agent_id, "pro").await;
        assert!(matches!(result, Err(ControlError::Internal(_))));
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("DEK"), "error should mention the DEK: {msg}");

        // The record is untouched: still legacy, still no event.
        let stored = service.store.get_agent(&legacy.agent_id).unwrap().unwrap();
        assert_eq!(stored.spec.tier, None);
        assert_eq!(stored.spec.storage_encryption, None);
        assert!(service
            .store
            .list_usage_events_by_agent(&legacy.agent_id)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn change_tier_insufficient_credits_rejects_awake_upgrade() {
        let (mut service, _dir, user_id) = setup();

        // Create before wiring in the failing billing checker.
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("tier-agent"))
            .await
            .unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        let (billing, _server) = insufficient_billing().await;
        service.set_billing(billing);

        let result = service.change_tier(&user_id, &agent.agent_id, "pro").await;
        assert!(matches!(
            result,
            Err(ControlError::InsufficientCredits { .. })
        ));

        // Pre-flight failed before any mutation: tier and state untouched,
        // no tier-change event (only the create's PodScheduled exists).
        let stored = service.store.get_agent(&agent.agent_id).unwrap().unwrap();
        assert_eq!(stored.spec.tier.as_deref(), Some("standard"));
        assert_eq!(stored.status, AgentState::Running);
        let events = service
            .store
            .list_usage_events_by_agent(&agent.agent_id)
            .unwrap();
        assert!(!events
            .iter()
            .any(|e| matches!(e.kind, UsageEventKind::TierChanged { .. })));
    }

    #[tokio::test]
    async fn change_tier_asleep_skips_credit_check() {
        let (mut service, _dir, user_id) = setup();

        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("tier-agent"))
            .await
            .unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Hibernating)
            .unwrap();

        let (billing, _server) = insufficient_billing().await;
        service.set_billing(billing);

        // Record-only changes have no pre-flight: the check happens when the
        // agent next wakes onto the new rate.
        let outcome = service
            .change_tier(&user_id, &agent.agent_id, "pro")
            .await
            .unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.tier, "pro");
    }

    // =========================================================================
    // Usage events + aggregation (Swarm TEE upgrade phase 11)
    // =========================================================================

    fn event_kinds(service: &ControlPlaneService<RocksStore>, agent_id: &AgentId) -> Vec<String> {
        service
            .store
            .list_usage_events_by_agent(agent_id)
            .unwrap()
            .into_iter()
            .map(|e| match e.kind {
                UsageEventKind::PodScheduled { .. } => "pod_scheduled".to_string(),
                UsageEventKind::PodTerminated { reason, .. } => {
                    format!("pod_terminated:{reason}")
                }
                UsageEventKind::Hibernated => "hibernated".to_string(),
                UsageEventKind::Woke => "woke".to_string(),
                UsageEventKind::TriggerFired { .. } => "trigger_fired".to_string(),
                UsageEventKind::TierChanged { .. } => "tier_changed".to_string(),
            })
            .collect()
    }

    #[tokio::test]
    async fn create_emits_pod_scheduled_priced_at_event_time() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("usage-agent").with_tier("pro"))
            .await
            .unwrap();

        let events = service
            .store
            .list_usage_events_by_agent(&agent.agent_id)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].kind,
            UsageEventKind::PodScheduled {
                tier: Some("pro".to_string()),
                hourly_price_cents: Some(BoxTier::Pro.hourly_price_cents()),
            }
        );
    }

    #[tokio::test]
    async fn full_lifecycle_emits_expected_event_sequence() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("usage-agent"))
            .await
            .unwrap();
        let aid = agent.agent_id;

        // create → Running → hibernate → wake → Running → stop
        service.store.update_agent_status(&aid, AgentState::Running).unwrap();
        service.hibernate_agent(&user_id, &aid).await.unwrap();
        service.wake_agent(&user_id, &aid).await.unwrap();
        service.store.update_agent_status(&aid, AgentState::Running).unwrap();
        service.stop_agent(&user_id, &aid).await.unwrap();

        assert_eq!(
            event_kinds(&service, &aid),
            vec![
                "pod_scheduled",
                "pod_terminated:hibernate",
                "hibernated",
                "woke",
                "pod_scheduled",
                "pod_terminated:stop",
            ]
        );
    }

    #[tokio::test]
    async fn scheduler_error_emits_crash_termination() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("usage-agent"))
            .await
            .unwrap();
        let aid = agent.agent_id;
        service.store.update_agent_status(&aid, AgentState::Running).unwrap();

        service
            .update_agent_status_internal(&aid, AgentState::Error, Some("OOM".to_string()))
            .await
            .unwrap();

        assert_eq!(
            event_kinds(&service, &aid),
            vec!["pod_scheduled", "pod_terminated:crash"]
        );
    }

    #[tokio::test]
    async fn stopping_to_error_does_not_double_count_termination() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("usage-agent"))
            .await
            .unwrap();
        let aid = agent.agent_id;
        service.store.update_agent_status(&aid, AgentState::Running).unwrap();
        service.stop_agent(&user_id, &aid).await.unwrap();

        // The pod dies messily during the stop: stop already recorded the
        // termination, the Error must not add a second one.
        service
            .update_agent_status_internal(&aid, AgentState::Error, Some("boom".to_string()))
            .await
            .unwrap();

        assert_eq!(
            event_kinds(&service, &aid),
            vec!["pod_scheduled", "pod_terminated:stop"]
        );
    }

    #[tokio::test]
    async fn legacy_agent_lifecycle_events_carry_no_pricing() {
        let (service, _dir, user_id) = setup();
        let legacy = legacy_agent(user_id, AgentState::Running);
        service.store.put_agent(&legacy).unwrap();

        service
            .hibernate_agent(&user_id, &legacy.agent_id)
            .await
            .unwrap();

        let events = service
            .store
            .list_usage_events_by_agent(&legacy.agent_id)
            .unwrap();
        assert_eq!(
            events[0].kind,
            UsageEventKind::PodTerminated {
                tier: None,
                hourly_price_cents: None,
                reason: "hibernate".to_string(),
            }
        );
        assert_eq!(events[1].kind, UsageEventKind::Hibernated);
    }

    #[tokio::test]
    async fn get_agent_usage_aggregates_and_enforces_ownership() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("usage-agent"))
            .await
            .unwrap();
        let aid = agent.agent_id;
        service.store.update_agent_status(&aid, AgentState::Running).unwrap();
        service.hibernate_agent(&user_id, &aid).await.unwrap();

        // `to` is nudged past now so events appended in the same
        // millisecond as the query are not flakily excluded.
        let now = Utc::now() + chrono::Duration::seconds(1);
        let usage = service
            .get_agent_usage(&user_id, &aid, now - chrono::Duration::hours(1), now)
            .await
            .unwrap();

        // One closed interval (create's schedule → hibernate's terminate)
        // at the standard rate, plus the raw events.
        assert_eq!(usage.aggregation.intervals.len(), 1);
        assert_eq!(
            usage.aggregation.intervals[0].hourly_price_cents,
            Some(BoxTier::Standard.hourly_price_cents())
        );
        assert!(usage.aggregation.open_interval_started_at.is_none());
        assert_eq!(usage.recent_events.len(), 3); // scheduled, terminated, hibernated

        let other = UserId::from_uuid(uuid::Uuid::new_v4());
        let result = service
            .get_agent_usage(&other, &aid, now - chrono::Duration::hours(1), now)
            .await;
        assert!(matches!(result, Err(ControlError::NotOwner { .. })));
    }

    #[tokio::test]
    async fn get_user_usage_covers_only_the_users_agents() {
        let (service, _dir, user_id) = setup();
        let other = UserId::from_uuid(uuid::Uuid::new_v4());

        let (mine, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("mine"))
            .await
            .unwrap();
        service
            .create_agent(&other, CreateAgentRequest::new("theirs"))
            .await
            .unwrap();

        let now = Utc::now();
        let reports = service
            .get_user_usage(&user_id, now - chrono::Duration::hours(1), now)
            .await
            .unwrap();

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0.agent_id, mine.agent_id);
        // The agent is still provisioning: its open interval runs to the
        // range end.
        assert!(reports[0].1.awake_seconds <= 3600);
        assert!(reports[0].1.open_interval_started_at.is_some());
    }

    // =========================================================================
    // Per-VM logs (phase 12)
    // =========================================================================

    fn log_line(millis: i64, line: &str) -> aura_swarm_store::LogLine {
        aura_swarm_store::LogLine {
            timestamp: chrono::DateTime::from_timestamp_millis(millis).unwrap(),
            line: line.to_string(),
        }
    }

    fn snapshot_for(
        agent_id: AgentId,
        captured_at_millis: i64,
        reason: &str,
        entries: Vec<aura_swarm_store::LogLine>,
    ) -> AgentLogSnapshot {
        AgentLogSnapshot {
            agent_id,
            captured_at: chrono::DateTime::from_timestamp_millis(captured_at_millis).unwrap(),
            reason: reason.to_string(),
            entries,
        }
    }

    #[tokio::test]
    async fn get_agent_logs_merges_live_and_snapshots_sorted() {
        let scheduler = Arc::new(MockSchedulerClient::default());
        let (service, _dir, user_id) = setup_with_scheduler(Arc::clone(&scheduler));

        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("logs-agent"))
            .await
            .unwrap();
        let aid = agent.agent_id;

        // A stored snapshot from a previous pod (old timestamps)...
        service
            .store_log_snapshot_internal(snapshot_for(
                aid,
                2_000,
                "hibernate",
                vec![log_line(1_000, "old boot"), log_line(2_000, "old shutdown")],
            ))
            .await
            .unwrap();

        // ...and a running pod serving a live tail (newer timestamps).
        scheduler.set_pod_logs(Some(vec![
            log_line(5_000, "new boot"),
            log_line(6_000, "ready"),
        ]));

        let entries = service
            .get_agent_logs(&user_id, &aid, 100, None)
            .await
            .unwrap();

        assert_eq!(
            entries
                .iter()
                .map(|e| (e.line.as_str(), e.source))
                .collect::<Vec<_>>(),
            vec![
                ("old boot", LogSource::Snapshot),
                ("old shutdown", LogSource::Snapshot),
                ("new boot", LogSource::Live),
                ("ready", LogSource::Live),
            ],
            "snapshot + live entries merged and time-ordered"
        );
    }

    #[tokio::test]
    async fn get_agent_logs_tail_and_since_applied_to_merged_set() {
        let scheduler = Arc::new(MockSchedulerClient::default());
        let (service, _dir, user_id) = setup_with_scheduler(Arc::clone(&scheduler));

        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("tail-agent"))
            .await
            .unwrap();
        let aid = agent.agent_id;

        service
            .store_log_snapshot_internal(snapshot_for(
                aid,
                3_000,
                "stop",
                vec![
                    log_line(1_000, "a"),
                    log_line(2_000, "b"),
                    log_line(3_000, "c"),
                ],
            ))
            .await
            .unwrap();
        scheduler.set_pod_logs(Some(vec![log_line(4_000, "d")]));

        // Tail caps the merged set, keeping the newest entries.
        let entries = service.get_agent_logs(&user_id, &aid, 2, None).await.unwrap();
        assert_eq!(
            entries.iter().map(|e| e.line.as_str()).collect::<Vec<_>>(),
            vec!["c", "d"]
        );

        // Since filters older snapshot entries too.
        let since = chrono::DateTime::from_timestamp_millis(3_000).unwrap();
        let entries = service
            .get_agent_logs(&user_id, &aid, 100, Some(since))
            .await
            .unwrap();
        assert_eq!(
            entries.iter().map(|e| e.line.as_str()).collect::<Vec<_>>(),
            vec!["c", "d"]
        );
    }

    #[tokio::test]
    async fn get_agent_logs_snapshots_only_when_no_pod_or_asleep() {
        let scheduler = Arc::new(MockSchedulerClient::default());
        let (service, _dir, user_id) = setup_with_scheduler(Arc::clone(&scheduler));

        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("asleep-agent"))
            .await
            .unwrap();
        let aid = agent.agent_id;

        service
            .store_log_snapshot_internal(snapshot_for(
                aid,
                2_000,
                "hibernate",
                vec![log_line(1_000, "from snapshot")],
            ))
            .await
            .unwrap();

        // Pod-active state but the scheduler reports no pod (404 -> None).
        scheduler.set_pod_logs(None);
        let entries = service
            .get_agent_logs(&user_id, &aid, 100, None)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, LogSource::Snapshot);

        // Hibernating agents never hit the scheduler.
        service.store.update_agent_status(&aid, AgentState::Hibernating).unwrap();
        scheduler.set_pod_logs(Some(vec![log_line(9_000, "must not appear")]));
        let entries = service
            .get_agent_logs(&user_id, &aid, 100, None)
            .await
            .unwrap();
        assert_eq!(
            entries.iter().map(|e| e.line.as_str()).collect::<Vec<_>>(),
            vec!["from snapshot"]
        );
    }

    #[tokio::test]
    async fn get_agent_logs_enforces_ownership() {
        let (service, _dir, user_id) = setup();
        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("owned-agent"))
            .await
            .unwrap();

        let other = UserId::from_uuid(uuid::Uuid::new_v4());
        let result = service
            .get_agent_logs(&other, &agent.agent_id, 100, None)
            .await;
        assert!(matches!(result, Err(ControlError::NotOwner { .. })));
    }

    #[tokio::test]
    async fn store_log_snapshot_internal_requires_existing_agent() {
        let (service, _dir, _user_id) = setup();
        let result = service
            .store_log_snapshot_internal(snapshot_for(
                AgentId::generate(),
                1_000,
                "stop",
                vec![log_line(1_000, "orphan")],
            ))
            .await;
        assert!(matches!(result, Err(ControlError::AgentNotFound(_))));
    }

    #[tokio::test]
    async fn lifecycle_forwards_termination_reasons_to_scheduler() {
        let scheduler = Arc::new(MockSchedulerClient::default());
        let (service, _dir, user_id) = setup_with_scheduler(Arc::clone(&scheduler));

        let (agent, _) = service
            .create_agent(&user_id, CreateAgentRequest::new("reasons-agent"))
            .await
            .unwrap();
        let aid = agent.agent_id;

        service.store.update_agent_status(&aid, AgentState::Running).unwrap();
        service.hibernate_agent(&user_id, &aid).await.unwrap();

        service.wake_agent(&user_id, &aid).await.unwrap();
        service.store.update_agent_status(&aid, AgentState::Running).unwrap();
        service.stop_agent(&user_id, &aid).await.unwrap();

        assert_eq!(
            scheduler.termination_reasons(),
            vec!["hibernate".to_string(), "stop".to_string()]
        );
    }
}
