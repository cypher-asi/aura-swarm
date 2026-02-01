//! Control plane service implementation.
//!
//! This module provides the `ControlPlane` trait and `ControlPlaneService` implementation
//! that coordinates agent lifecycle and session management.

use std::sync::Arc;

use async_trait::async_trait;
use aura_swarm_core::{AgentId, SessionId, UserId};
use aura_swarm_store::{Agent, AgentState, Session, Store};
use chrono::Utc;

use crate::error::{ControlError, Result};
use crate::lifecycle;
use crate::scheduler_client::SchedulerClient;
use crate::session;
use crate::types::{ControlConfig, CreateAgentRequest};

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
    /// # Errors
    ///
    /// Returns `ControlError::QuotaExceeded` if the user has reached their limit.
    async fn create_agent(&self, user_id: &UserId, request: CreateAgentRequest) -> Result<Agent>;

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
    async fn create_session(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Session>;

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
    ) -> Result<()>;
}

/// The main control plane service implementation.
///
/// The service can optionally integrate with a scheduler for managing agent pods.
/// If no scheduler is configured, lifecycle operations only update the local store.
pub struct ControlPlaneService<S: Store, SC: SchedulerClient = crate::scheduler_client::NoopSchedulerClient> {
    store: Arc<S>,
    config: ControlConfig,
    scheduler: Option<Arc<SC>>,
}

impl<S: Store> ControlPlaneService<S, crate::scheduler_client::NoopSchedulerClient> {
    /// Create a new control plane service without scheduler integration.
    #[must_use]
    pub fn new(store: Arc<S>, config: ControlConfig) -> Self {
        Self {
            store,
            config,
            scheduler: None,
        }
    }

    /// Create with default configuration and no scheduler.
    #[must_use]
    pub fn with_defaults(store: Arc<S>) -> Self {
        Self::new(store, ControlConfig::default())
    }
}

impl<S: Store, SC: SchedulerClient> ControlPlaneService<S, SC> {
    /// Create a new control plane service with scheduler integration.
    #[must_use]
    pub fn with_scheduler(store: Arc<S>, config: ControlConfig, scheduler: Arc<SC>) -> Self {
        Self {
            store,
            config,
            scheduler: Some(scheduler),
        }
    }

    /// Create a new control plane service with optional scheduler integration.
    ///
    /// Use this when the scheduler may or may not be configured at runtime.
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
        }
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

    /// Verify that the user owns the given agent.
    fn verify_ownership(user_id: &UserId, agent: &Agent) -> Result<()> {
        if agent.user_id != *user_id {
            return Err(ControlError::NotOwner {
                user_id: *user_id,
                agent_id: agent.agent_id,
            });
        }
        Ok(())
    }

    /// Get an agent and verify ownership.
    fn get_and_verify(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        let agent = self
            .store
            .get_agent(agent_id)?
            .ok_or(ControlError::AgentNotFound(*agent_id))?;

        Self::verify_ownership(user_id, &agent)?;
        Ok(agent)
    }

    /// Perform a validated state transition.
    fn transition_state(&self, agent: &mut Agent, target: AgentState) -> Result<()> {
        lifecycle::validate_transition(&agent.agent_id, agent.status, target)?;
        agent.status = target;
        agent.updated_at = Utc::now();
        self.store.put_agent(agent)?;
        Ok(())
    }

    /// Schedule an agent pod via the scheduler service.
    async fn schedule_agent_pod(&self, agent: &Agent) -> Result<()> {
        if let Some(scheduler) = &self.scheduler {
            scheduler
                .schedule_agent(&agent.agent_id, &agent.user_id.to_hex(), &agent.spec)
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
impl<S: Store + 'static, SC: SchedulerClient + 'static> ControlPlane for ControlPlaneService<S, SC> {
    // =========================================================================
    // Agent CRUD Operations
    // =========================================================================

    async fn create_agent(&self, user_id: &UserId, request: CreateAgentRequest) -> Result<Agent> {
        // Check quota
        let count = self.store.count_agents_by_user(user_id)?;
        if count >= self.config.max_agents_per_user {
            return Err(ControlError::QuotaExceeded {
                user_id: *user_id,
                limit: self.config.max_agents_per_user,
            });
        }

        let now = Utc::now();
        let spec = request.spec.unwrap_or_default();
        let agent_id = AgentId::generate(user_id, &request.name);

        let agent = Agent {
            agent_id,
            user_id: *user_id,
            name: request.name,
            status: AgentState::Provisioning,
            spec,
            created_at: now,
            updated_at: now,
            last_heartbeat_at: None,
            error_message: None,
        };

        self.store.put_agent(&agent)?;

        // Schedule the agent pod
        if let Err(e) = self.schedule_agent_pod(&agent).await {
            tracing::error!(
                agent_id = %agent.agent_id,
                error = %e,
                "Failed to schedule agent pod, marking as error"
            );
            // Update status to Error with the error message
            self.store
                .update_agent_error(
                    &agent.agent_id,
                    AgentState::Error,
                    Some(e.to_string()),
                )
                .ok();
            return Err(e);
        }

        tracing::info!(
            agent_id = %agent.agent_id,
            user_id = %user_id,
            name = %agent.name,
            "Created agent"
        );

        Ok(agent)
    }

    async fn get_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        self.get_and_verify(user_id, agent_id)
    }

    async fn list_agents(&self, user_id: &UserId) -> Result<Vec<Agent>> {
        Ok(self.store.list_agents_by_user(user_id)?)
    }

    async fn delete_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<()> {
        let agent = self.get_and_verify(user_id, agent_id)?;

        // Can only delete stopped or error agents
        if !lifecycle::is_terminal(agent.status) {
            return Err(ControlError::InvalidState {
                agent_id: *agent_id,
                from: agent.status,
                to: AgentState::Stopped, // Indicate they need to stop first
            });
        }

        // Delete all sessions for this agent
        let sessions = self.store.list_sessions_by_agent(agent_id)?;
        for session in sessions {
            self.store.delete_session(&session.session_id)?;
        }

        self.store.delete_agent(agent_id)?;

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
        let mut agent = self.get_and_verify(user_id, agent_id)?;

        // Can only start from Stopped state
        self.transition_state(&mut agent, AgentState::Provisioning)?;

        // Schedule the agent pod
        if let Err(e) = self.schedule_agent_pod(&agent).await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to schedule agent pod on start"
            );
            self.store
                .update_agent_status(agent_id, AgentState::Error)
                .ok();
            return Err(e);
        }

        tracing::info!(agent_id = %agent_id, "Starting agent");

        Ok(agent)
    }

    async fn stop_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        let mut agent = self.get_and_verify(user_id, agent_id)?;

        // Close all active sessions
        let sessions = self.store.list_sessions_by_agent(agent_id)?;
        for session in sessions {
            if session.status == aura_swarm_store::SessionStatus::Active {
                self.store.update_session_status(
                    &session.session_id,
                    aura_swarm_store::SessionStatus::Closed,
                )?;
            }
        }

        // Transition to Stopping
        self.transition_state(&mut agent, AgentState::Stopping)?;

        // Terminate the agent pod
        if let Err(e) = self.terminate_agent_pod(agent_id).await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to terminate agent pod on stop"
            );
            // Don't fail the stop operation, just log - the scheduler will clean up
        }

        tracing::info!(agent_id = %agent_id, "Stopping agent");

        Ok(agent)
    }

    async fn restart_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        // Stop the agent (this will terminate the pod)
        let mut agent = self.stop_agent(user_id, agent_id).await?;

        // Transition to Stopped state
        self.transition_state(&mut agent, AgentState::Stopped)?;

        // Start again (this will schedule a new pod)
        self.transition_state(&mut agent, AgentState::Provisioning)?;

        // Schedule the new pod
        if let Err(e) = self.schedule_agent_pod(&agent).await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to schedule agent pod on restart"
            );
            self.store
                .update_agent_status(agent_id, AgentState::Error)
                .ok();
            return Err(e);
        }

        tracing::info!(agent_id = %agent_id, "Restarting agent");

        Ok(agent)
    }

    async fn hibernate_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        let mut agent = self.get_and_verify(user_id, agent_id)?;

        // Close all active sessions
        let sessions = self.store.list_sessions_by_agent(agent_id)?;
        for session in sessions {
            if session.status == aura_swarm_store::SessionStatus::Active {
                self.store.update_session_status(
                    &session.session_id,
                    aura_swarm_store::SessionStatus::Closed,
                )?;
            }
        }

        self.transition_state(&mut agent, AgentState::Hibernating)?;

        // Terminate the agent pod (but keep state saved)
        if let Err(e) = self.terminate_agent_pod(agent_id).await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to terminate agent pod on hibernate"
            );
            // Don't fail the hibernate operation
        }

        tracing::info!(agent_id = %agent_id, "Hibernating agent");

        Ok(agent)
    }

    async fn wake_agent(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Agent> {
        let mut agent = self.get_and_verify(user_id, agent_id)?;

        if !lifecycle::can_wake(agent.status) {
            return Err(ControlError::InvalidState {
                agent_id: *agent_id,
                from: agent.status,
                to: AgentState::Running,
            });
        }

        // For hibernating, go through Provisioning to trigger pod scheduling
        // For stopped, also go through Provisioning
        self.transition_state(&mut agent, AgentState::Provisioning)?;

        // Schedule the agent pod
        if let Err(e) = self.schedule_agent_pod(&agent).await {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to schedule agent pod on wake"
            );
            self.store
                .update_agent_status(agent_id, AgentState::Error)
                .ok();
            return Err(e);
        }

        tracing::info!(agent_id = %agent_id, "Waking agent");

        Ok(agent)
    }

    // =========================================================================
    // Session Operations
    // =========================================================================

    async fn create_session(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Session> {
        let (session, state_change) = session::create_session(&*self.store, user_id, agent_id)?;

        tracing::info!(
            session_id = %session.session_id,
            agent_id = %agent_id,
            state_change = ?state_change,
            "Created session"
        );

        Ok(session)
    }

    async fn get_session(&self, user_id: &UserId, session_id: &SessionId) -> Result<Session> {
        session::get_session(&*self.store, user_id, session_id)
    }

    async fn close_session(&self, user_id: &UserId, session_id: &SessionId) -> Result<()> {
        let closed = session::close_session(&*self.store, user_id, session_id)?;

        if closed {
            tracing::info!(session_id = %session_id, "Closed session");
        }

        Ok(())
    }

    async fn list_sessions(&self, user_id: &UserId, agent_id: &AgentId) -> Result<Vec<Session>> {
        session::list_sessions(&*self.store, user_id, agent_id)
    }

    // =========================================================================
    // Operational
    // =========================================================================

    async fn process_heartbeat(&self, agent_id: &AgentId) -> Result<()> {
        let mut agent = self
            .store
            .get_agent(agent_id)?
            .ok_or(ControlError::AgentNotFound(*agent_id))?;

        agent.last_heartbeat_at = Some(Utc::now());
        agent.updated_at = Utc::now();
        self.store.put_agent(&agent)?;

        tracing::debug!(agent_id = %agent_id, "Processed heartbeat");

        Ok(())
    }

    async fn resolve_agent_endpoint(&self, agent_id: &AgentId) -> Result<Option<String>> {
        let agent = self
            .store
            .get_agent(agent_id)?
            .ok_or(ControlError::AgentNotFound(*agent_id))?;

        // Only return endpoint for active agents
        if lifecycle::is_active(agent.status) {
            // Query the scheduler for the pod's actual endpoint (IP:port)
            if let Some(scheduler) = &self.scheduler {
                scheduler.get_pod_endpoint(agent_id).await
            } else {
                // No scheduler configured, return mock endpoint for local dev
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
    ) -> Result<()> {
        // Verify agent exists
        if self.store.get_agent(agent_id)?.is_none() {
            return Err(ControlError::AgentNotFound(*agent_id));
        }

        self.store.update_agent_status(agent_id, status)?;

        tracing::info!(
            agent_id = %agent_id,
            status = ?status,
            "Updated agent status (internal)"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler_client::NoopSchedulerClient;
    use aura_swarm_store::RocksStore;
    use tempfile::TempDir;

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
        let user_id = UserId::from_bytes([1u8; 32]);
        (service, dir, user_id)
    }

    #[tokio::test]
    async fn create_agent_success() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let agent = service.create_agent(&user_id, request).await.unwrap();

        assert_eq!(agent.name, "test-agent");
        assert_eq!(agent.user_id, user_id);
        assert_eq!(agent.status, AgentState::Provisioning);
    }

    #[tokio::test]
    async fn create_agent_quota_exceeded() {
        let (service, _dir, user_id) = setup();

        // Create max agents
        for i in 0..3 {
            let request = CreateAgentRequest::new(format!("agent-{i}"));
            service.create_agent(&user_id, request).await.unwrap();
        }

        // Try to create one more
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
        let other_user = UserId::from_bytes([99u8; 32]);

        let request = CreateAgentRequest::new("test-agent");
        let agent = service.create_agent(&user_id, request).await.unwrap();

        let result = service.get_agent(&other_user, &agent.agent_id).await;
        assert!(matches!(result, Err(ControlError::NotOwner { .. })));
    }

    #[tokio::test]
    async fn agent_lifecycle() {
        let (service, _dir, user_id) = setup();

        // Create agent (starts in Provisioning)
        let request = CreateAgentRequest::new("test-agent");
        let agent = service.create_agent(&user_id, request).await.unwrap();
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
        let agent = service.create_agent(&user_id, request).await.unwrap();

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
        let agent = service.create_agent(&user_id, request).await.unwrap();
        service
            .store
            .update_agent_status(&agent.agent_id, AgentState::Running)
            .unwrap();

        // Create session
        let session = service
            .create_session(&user_id, &agent.agent_id)
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
        let agent = service.create_agent(&user_id, request).await.unwrap();

        assert!(agent.last_heartbeat_at.is_none());

        service.process_heartbeat(&agent.agent_id).await.unwrap();

        let updated = service.store.get_agent(&agent.agent_id).unwrap().unwrap();
        assert!(updated.last_heartbeat_at.is_some());
    }

    #[tokio::test]
    async fn resolve_endpoint_active() {
        let (service, _dir, user_id) = setup();

        let request = CreateAgentRequest::new("test-agent");
        let agent = service.create_agent(&user_id, request).await.unwrap();
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
        let agent = service.create_agent(&user_id, request).await.unwrap();
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
}
