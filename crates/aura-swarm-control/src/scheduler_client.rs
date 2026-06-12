//! HTTP client for communicating with the scheduler service.
//!
//! This module provides the `SchedulerClient` for making HTTP requests to the
//! scheduler service to manage agent pod lifecycles.

use std::time::Duration;

use async_trait::async_trait;
use aura_swarm_core::AgentId;
use aura_swarm_store::{AgentSpec, LogLine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{ControlError, Result};

/// Trait for scheduler communication.
///
/// This trait abstracts the scheduler client interface, allowing for
/// mock implementations in tests.
#[async_trait]
pub trait SchedulerClient: Send + Sync {
    /// Schedule a new agent pod in the cluster.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or the scheduler rejects the request.
    async fn schedule_agent(
        &self,
        agent_id: &AgentId,
        user_id_hex: &str,
        agent_name: &str,
        spec: &AgentSpec,
    ) -> Result<()>;

    /// Terminate an agent pod.
    ///
    /// `reason` (e.g. `hibernate` / `stop` / `tier_change` / `destroy`)
    /// is attached to the final-tail log snapshot the scheduler ships to
    /// the gateway before deleting the pod.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails.
    async fn terminate_agent(&self, agent_id: &AgentId, reason: &str) -> Result<()>;

    /// Fetch the live stdout tail of an agent's pod via the scheduler's
    /// pod-logs endpoint.
    ///
    /// Returns `Ok(None)` when the agent has no pod (scheduler 404).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails.
    async fn get_pod_logs(
        &self,
        agent_id: &AgentId,
        tail: u32,
        since: Option<DateTime<Utc>>,
    ) -> Result<Option<Vec<LogLine>>>;

    /// Get the status of an agent's pod.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or the pod is not found.
    async fn get_pod_status(&self, agent_id: &AgentId) -> Result<PodStatusResponse>;

    /// Get the network endpoint for an agent's pod.
    ///
    /// Returns the endpoint (IP:port) if the pod is running and has an IP.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails.
    async fn get_pod_endpoint(&self, agent_id: &AgentId) -> Result<Option<String>>;
}

/// Response from the scheduler's pod status endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodStatusResponse {
    /// Current phase of the pod lifecycle.
    pub phase: String,
    /// Whether the pod is ready to serve traffic.
    pub ready: bool,
    /// Number of times the pod has restarted.
    pub restart_count: u32,
    /// Human-readable message about the pod's status.
    pub message: Option<String>,
}

/// HTTP client for the scheduler service.
///
/// This client makes HTTP requests to the scheduler service's REST API
/// for managing agent pod lifecycles.
#[derive(Debug, Clone)]
pub struct HttpSchedulerClient {
    client: reqwest::Client,
    base_url: String,
    /// Bearer token for the scheduler API (`INTERNAL_TOKEN`). The
    /// scheduler enforces it on every `/v1` route when configured.
    token: Option<String>,
}

impl HttpSchedulerClient {
    /// Create a new scheduler client.
    ///
    /// # Arguments
    ///
    /// * `base_url` - The base URL of the scheduler service (e.g., `http://scheduler:8080`)
    ///
    /// # Errors
    ///
    /// Returns `ControlError::Internal` if the HTTP client cannot be created.
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| ControlError::Internal(format!("failed to create HTTP client: {e}")))?;

        Ok(Self {
            client,
            base_url: base_url.into(),
            token: None,
        })
    }

    /// Create a new scheduler client with a custom reqwest client.
    #[must_use]
    pub fn with_client(client: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            token: None,
        }
    }

    /// Set the bearer token sent on every request (the shared
    /// `INTERNAL_TOKEN`). Required against a scheduler that has token
    /// enforcement enabled.
    #[must_use]
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|t| !t.is_empty());
        self
    }

    /// Get the base URL of the scheduler service.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Attach the bearer token to a request when configured.
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }
}

/// Request body for scheduling an agent pod.
#[derive(Debug, Serialize)]
struct ScheduleRequest<'a> {
    user_id: &'a str,
    agent_name: &'a str,
    spec: &'a AgentSpec,
}

/// Error response from the scheduler.
///
/// `code` is part of the wire schema and must be deserialized even though
/// only `error` is currently surfaced to callers.
#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    #[allow(dead_code)]
    code: u16,
}

#[async_trait]
impl SchedulerClient for HttpSchedulerClient {
    async fn schedule_agent(
        &self,
        agent_id: &AgentId,
        user_id_hex: &str,
        agent_name: &str,
        spec: &AgentSpec,
    ) -> Result<()> {
        let url = format!("{}/v1/agents/{}/schedule", self.base_url, agent_id.to_hex());

        let request = ScheduleRequest {
            user_id: user_id_hex,
            agent_name,
            spec,
        };

        let response = self
            .authorize(self.client.post(&url))
            .json(&request)
            .send()
            .await
            .map_err(|e| ControlError::Internal(format!("Scheduler request failed: {e}")))?;

        if response.status().is_success() {
            tracing::debug!(agent_id = %agent_id, "Scheduled agent via scheduler API");
            Ok(())
        } else {
            let status = response.status();
            let error = response.json::<ErrorResponse>().await.map_or_else(
                |_| format!("Scheduler returned status {status}"),
                |e| e.error,
            );

            tracing::error!(
                agent_id = %agent_id,
                status = %status,
                error = %error,
                "Failed to schedule agent"
            );

            Err(ControlError::Internal(format!("Scheduler error: {error}")))
        }
    }

    async fn terminate_agent(&self, agent_id: &AgentId, reason: &str) -> Result<()> {
        let url = format!("{}/v1/agents/{}", self.base_url, agent_id.to_hex());

        let response = self
            .authorize(self.client.delete(&url))
            .query(&[("reason", reason)])
            .send()
            .await
            .map_err(|e| ControlError::Internal(format!("Scheduler request failed: {e}")))?;

        if response.status().is_success() {
            tracing::debug!(agent_id = %agent_id, "Terminated agent via scheduler API");
            Ok(())
        } else {
            let status = response.status();
            let error = response.json::<ErrorResponse>().await.map_or_else(
                |_| format!("Scheduler returned status {status}"),
                |e| e.error,
            );

            tracing::error!(
                agent_id = %agent_id,
                status = %status,
                error = %error,
                "Failed to terminate agent"
            );

            Err(ControlError::Internal(format!("Scheduler error: {error}")))
        }
    }

    async fn get_pod_logs(
        &self,
        agent_id: &AgentId,
        tail: u32,
        since: Option<DateTime<Utc>>,
    ) -> Result<Option<Vec<LogLine>>> {
        let url = format!("{}/v1/agents/{}/logs", self.base_url, agent_id.to_hex());

        let mut request = self
            .authorize(self.client.get(&url))
            .query(&[("tail", tail.to_string())]);
        if let Some(since) = since {
            request = request.query(&[("since", since.to_rfc3339())]);
        }

        let response = request
            .send()
            .await
            .map_err(|e| ControlError::Internal(format!("Scheduler request failed: {e}")))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if response.status().is_success() {
            #[derive(Deserialize)]
            struct LogsResponse {
                entries: Vec<LogLine>,
            }
            let resp: LogsResponse = response
                .json()
                .await
                .map_err(|e| ControlError::Internal(format!("Failed to parse response: {e}")))?;
            Ok(Some(resp.entries))
        } else {
            let status = response.status();
            let error = response.json::<ErrorResponse>().await.map_or_else(
                |_| format!("Scheduler returned status {status}"),
                |e| e.error,
            );

            Err(ControlError::Internal(format!("Scheduler error: {error}")))
        }
    }

    async fn get_pod_status(&self, agent_id: &AgentId) -> Result<PodStatusResponse> {
        let url = format!("{}/v1/agents/{}/status", self.base_url, agent_id.to_hex());

        let response = self
            .authorize(self.client.get(&url))
            .send()
            .await
            .map_err(|e| ControlError::Internal(format!("Scheduler request failed: {e}")))?;

        if response.status().is_success() {
            response
                .json::<PodStatusResponse>()
                .await
                .map_err(|e| ControlError::Internal(format!("Failed to parse response: {e}")))
        } else if response.status() == reqwest::StatusCode::NOT_FOUND {
            Err(ControlError::AgentNotFound(*agent_id))
        } else {
            let status = response.status();
            let error = response.json::<ErrorResponse>().await.map_or_else(
                |_| format!("Scheduler returned status {status}"),
                |e| e.error,
            );

            Err(ControlError::Internal(format!("Scheduler error: {error}")))
        }
    }

    async fn get_pod_endpoint(&self, agent_id: &AgentId) -> Result<Option<String>> {
        let url = format!("{}/v1/agents/{}/endpoint", self.base_url, agent_id.to_hex());

        let response = self
            .authorize(self.client.get(&url))
            .send()
            .await
            .map_err(|e| ControlError::Internal(format!("Scheduler request failed: {e}")))?;

        if response.status().is_success() {
            #[derive(Deserialize)]
            struct EndpointResponse {
                endpoint: Option<String>,
            }
            let resp: EndpointResponse = response
                .json()
                .await
                .map_err(|e| ControlError::Internal(format!("Failed to parse response: {e}")))?;
            Ok(resp.endpoint)
        } else {
            let status = response.status();
            let error = response.json::<ErrorResponse>().await.map_or_else(
                |_| format!("Scheduler returned status {status}"),
                |e| e.error,
            );

            Err(ControlError::Internal(format!("Scheduler error: {error}")))
        }
    }
}

/// A no-op scheduler client for when scheduler integration is disabled.
///
/// This client simply logs operations without actually communicating
/// with a scheduler service.
#[derive(Debug, Clone, Default)]
pub struct NoopSchedulerClient;

impl NoopSchedulerClient {
    /// Create a new no-op scheduler client.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SchedulerClient for NoopSchedulerClient {
    async fn schedule_agent(
        &self,
        agent_id: &AgentId,
        _user_id_hex: &str,
        _agent_name: &str,
        _spec: &AgentSpec,
    ) -> Result<()> {
        tracing::warn!(
            agent_id = %agent_id,
            "NoopSchedulerClient: schedule_agent called but no scheduler configured"
        );
        Ok(())
    }

    async fn terminate_agent(&self, agent_id: &AgentId, _reason: &str) -> Result<()> {
        tracing::warn!(
            agent_id = %agent_id,
            "NoopSchedulerClient: terminate_agent called but no scheduler configured"
        );
        Ok(())
    }

    async fn get_pod_logs(
        &self,
        agent_id: &AgentId,
        _tail: u32,
        _since: Option<DateTime<Utc>>,
    ) -> Result<Option<Vec<LogLine>>> {
        tracing::warn!(
            agent_id = %agent_id,
            "NoopSchedulerClient: get_pod_logs called but no scheduler configured"
        );
        // No scheduler -> no live pod logs.
        Ok(None)
    }

    async fn get_pod_status(&self, agent_id: &AgentId) -> Result<PodStatusResponse> {
        tracing::warn!(
            agent_id = %agent_id,
            "NoopSchedulerClient: get_pod_status called but no scheduler configured"
        );
        // Return a mock "running" status
        Ok(PodStatusResponse {
            phase: "Running".to_string(),
            ready: true,
            restart_count: 0,
            message: Some("No scheduler configured".to_string()),
        })
    }

    async fn get_pod_endpoint(&self, agent_id: &AgentId) -> Result<Option<String>> {
        tracing::warn!(
            agent_id = %agent_id,
            "NoopSchedulerClient: get_pod_endpoint called but no scheduler configured"
        );
        // Return a mock endpoint for local dev
        Ok(Some("localhost:8080".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_client_creation() {
        let client = NoopSchedulerClient::new();
        assert!(format!("{client:?}").contains("NoopSchedulerClient"));
    }

    #[test]
    fn http_client_creation() {
        let client = HttpSchedulerClient::new("http://localhost:8080").unwrap();
        assert_eq!(client.base_url(), "http://localhost:8080");
    }
}
