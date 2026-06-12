//! Control plane service implementation.
//!
//! This module provides the `ControlPlane` trait and `ControlPlaneService` implementation
//! that coordinates agent lifecycle and session management.

use std::sync::Arc;

use async_trait::async_trait;
use aura_swarm_core::{AgentId, SessionId, UserId};
use aura_swarm_store::{Agent, AgentState, BoxTier, Session, SessionConfig, SessionStatus, Store};
use chrono::Utc;

use crate::billing::BillingChecker;
use crate::error::{ControlError, Result};
use crate::kbs::KbsClient;
use crate::lifecycle;
use crate::scheduler_client::SchedulerClient;
use crate::session;
use crate::types::{ControlConfig, CreateAgentRequest};

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

    /// Terminate an agent pod via the scheduler service.
    async fn terminate_agent_pod(&self, agent_id: &AgentId) -> Result<()> {
        if let Some(scheduler) = &self.scheduler {
            scheduler.terminate_agent(agent_id).await?;
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

        if let Err(e) = self.terminate_agent_pod(agent_id).await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to terminate agent pod on stop"
            );
        }

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

        if let Err(e) = self.terminate_agent_pod(agent_id).await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to terminate agent pod on hibernate"
            );
        }

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

        tracing::info!(agent_id = %agent_id, "Waking agent");

        Ok(agent)
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
    /// provisioning to exercise create rollback.
    #[derive(Default)]
    struct MockKbsClient {
        provisioned: Mutex<Vec<String>>,
        revoked: Mutex<Vec<String>>,
        fail_provision: bool,
    }

    impl MockKbsClient {
        fn failing() -> Self {
            Self {
                fail_provision: true,
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

        async fn revoke_dek(&self, key_id: &str) -> Result<()> {
            self.revoked.lock().unwrap().push(key_id.to_string());
            Ok(())
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
}
