//! HTTP client for the gateway REST API.
//!
//! This module provides a typed client for interacting with the aura-swarm-gateway.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, StatusCode};

use aura_swarm_protocol::RuntimeRunResponse;

use crate::types::{
    Agent, ApiErrorResponse, CreateAgentRequest, LifecycleResponse, ListAgentsResponse,
};

/// Error type for client operations.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// API returned an error response.
    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },

    /// Failed to parse response.
    #[error("Failed to parse response: {0}")]
    Parse(String),
}

/// Client for the gateway REST API.
#[derive(Debug, Clone)]
pub struct GatewayClient {
    client: Client,
    base_url: String,
    token: String,
}

impl GatewayClient {
    /// Create a new gateway client.
    ///
    /// # Arguments
    ///
    /// * `base_url` - Base URL of the gateway (e.g., "http://localhost:8080")
    /// * `token` - JWT token for authentication
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self, ClientError> {
        let client = Client::builder().timeout(Duration::from_secs(30)).build()?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        })
    }

    /// Build headers for authenticated requests.
    fn auth_headers(&self) -> Result<HeaderMap, ClientError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token))
                .map_err(|e| ClientError::Parse(format!("invalid authorization header: {e}")))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    /// Handle API error responses.
    async fn handle_error(response: reqwest::Response) -> ClientError {
        let status = response.status().as_u16();
        let message = match response.json::<ApiErrorResponse>().await {
            Ok(err) => err.error.message,
            Err(_) => "Unknown error".to_string(),
        };
        ClientError::Api { status, message }
    }

    // =========================================================================
    // Agent Operations
    // =========================================================================

    /// List all agents.
    pub async fn list_agents(&self) -> Result<Vec<Agent>, ClientError> {
        let url = format!("{}/v1/agents", self.base_url);

        let response = self
            .client
            .get(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::handle_error(response).await);
        }

        let body: ListAgentsResponse = response
            .json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;

        Ok(body.agents)
    }

    /// Create a new agent, optionally supplying an existing agent ID.
    pub async fn create_agent(
        &self,
        name: &str,
        agent_id: Option<&str>,
    ) -> Result<Agent, ClientError> {
        let url = format!("{}/v1/agents", self.base_url);

        let request = CreateAgentRequest {
            name: name.to_string(),
            agent_id: agent_id.map(String::from),
        };

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::handle_error(response).await);
        }

        let agent: Agent = response
            .json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;

        Ok(agent)
    }

    /// Delete an agent.
    pub async fn delete_agent(&self, agent_id: &str) -> Result<(), ClientError> {
        let url = format!("{}/v1/agents/{}", self.base_url, agent_id);

        let response = self
            .client
            .delete(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if response.status() != StatusCode::NO_CONTENT && !response.status().is_success() {
            return Err(Self::handle_error(response).await);
        }

        Ok(())
    }

    /// Start an agent.
    pub async fn start_agent(&self, agent_id: &str) -> Result<LifecycleResponse, ClientError> {
        let url = format!("{}/v1/agents/{}/start", self.base_url, agent_id);

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::handle_error(response).await);
        }

        let result: LifecycleResponse = response
            .json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;

        Ok(result)
    }

    /// Stop an agent.
    pub async fn stop_agent(&self, agent_id: &str) -> Result<LifecycleResponse, ClientError> {
        let url = format!("{}/v1/agents/{}/stop", self.base_url, agent_id);

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::handle_error(response).await);
        }

        let result: LifecycleResponse = response
            .json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;

        Ok(result)
    }

    /// Restart an agent.
    pub async fn restart_agent(&self, agent_id: &str) -> Result<LifecycleResponse, ClientError> {
        let url = format!("{}/v1/agents/{}/restart", self.base_url, agent_id);

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::handle_error(response).await);
        }

        let result: LifecycleResponse = response
            .json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;

        Ok(result)
    }

    /// Hibernate an agent.
    pub async fn hibernate_agent(&self, agent_id: &str) -> Result<LifecycleResponse, ClientError> {
        let url = format!("{}/v1/agents/{}/hibernate", self.base_url, agent_id);

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::handle_error(response).await);
        }

        let result: LifecycleResponse = response
            .json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;

        Ok(result)
    }

    /// Wake a hibernating agent.
    pub async fn wake_agent(&self, agent_id: &str) -> Result<LifecycleResponse, ClientError> {
        let url = format!("{}/v1/agents/{}/wake", self.base_url, agent_id);

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::handle_error(response).await);
        }

        let result: LifecycleResponse = response
            .json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;

        Ok(result)
    }

    // =========================================================================
    // Run Operations
    // =========================================================================

    /// Start a chat run for an agent via the gateway `POST /v1/agents/:id/run`.
    ///
    /// Returns the harness run handle; `event_stream_url` is the swarm-facing
    /// WS path to attach to (already rewritten by the gateway).
    pub async fn create_run(&self, agent_id: &str) -> Result<RuntimeRunResponse, ClientError> {
        let url = format!("{}/v1/agents/{}/run", self.base_url, agent_id);

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .json(&serde_json::json!({}))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::handle_error(response).await);
        }

        let run: RuntimeRunResponse = response
            .json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;

        Ok(run)
    }

    /// Stop a run via the gateway `POST /v1/agents/:id/run/:run_id/stop`.
    pub async fn stop_run(&self, agent_id: &str, run_id: &str) -> Result<(), ClientError> {
        let url = format!(
            "{}/v1/agents/{}/run/{}/stop",
            self.base_url, agent_id, run_id
        );

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if response.status() != StatusCode::NO_CONTENT && !response.status().is_success() {
            return Err(Self::handle_error(response).await);
        }

        Ok(())
    }

    // =========================================================================
    // Utility
    // =========================================================================

    /// Get the base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Get the authentication token.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Build a WebSocket URL for attaching to a run's event stream.
    #[must_use]
    pub fn run_ws_url(&self, agent_id: &str, run_id: &str) -> String {
        let ws_base = if self.base_url.starts_with("https://") {
            self.base_url.replace("https://", "wss://")
        } else {
            self.base_url.replace("http://", "ws://")
        };
        format!("{ws_base}/v1/agents/{agent_id}/stream/{run_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_trims_trailing_slash() {
        let client = GatewayClient::new("http://example.com/", "tok").unwrap();
        assert_eq!(client.base_url(), "http://example.com");
    }

    #[test]
    fn new_preserves_clean_url() {
        let client = GatewayClient::new("http://example.com", "tok").unwrap();
        assert_eq!(client.base_url(), "http://example.com");
    }

    #[test]
    fn new_stores_token() {
        let client = GatewayClient::new("http://example.com", "my-secret").unwrap();
        assert_eq!(client.token(), "my-secret");
    }

    #[test]
    fn run_ws_url_http_to_ws() {
        let client = GatewayClient::new("http://localhost:8080", "tok").unwrap();
        let url = client.run_ws_url("agent-1", "run-1");
        assert!(url.starts_with("ws://"), "url: {url}");
        assert!(!url.contains("http://"), "url: {url}");
    }

    #[test]
    fn run_ws_url_https_to_wss() {
        let client = GatewayClient::new("https://gateway.example.com", "tok").unwrap();
        let url = client.run_ws_url("agent-2", "run-2");
        assert!(url.starts_with("wss://"), "url: {url}");
        assert!(!url.contains("https://"), "url: {url}");
    }

    #[test]
    fn run_ws_url_includes_agent_and_run() {
        let client = GatewayClient::new("http://localhost:8080", "tok").unwrap();
        let url = client.run_ws_url("abc-123", "run-9");
        assert_eq!(url, "ws://localhost:8080/v1/agents/abc-123/stream/run-9");
    }

    #[test]
    fn run_ws_url_with_trailing_slash_base() {
        let client = GatewayClient::new("https://api.example.com/", "tok").unwrap();
        let url = client.run_ws_url("a1", "r1");
        assert_eq!(url, "wss://api.example.com/v1/agents/a1/stream/r1");
    }
}
