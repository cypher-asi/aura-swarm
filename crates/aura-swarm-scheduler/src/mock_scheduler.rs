//! Mock scheduler for testing without a real Kubernetes cluster.

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use std::collections::HashMap;

use aura_swarm_core::AgentId;
use aura_swarm_store::AgentSpec;

use crate::pod::pod_name_for_agent;
use crate::types::{PodInfo, PodPhase, PodStatus};
use crate::{Result, Scheduler, SchedulerError};

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
