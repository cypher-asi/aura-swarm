//! HTTP client for the gateway REST API.
//!
//! This module provides a typed client for interacting with the aura-swarm-gateway.

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, StatusCode};

use crate::types::{
    Agent, ApiErrorResponse, CreateAgentRequest, CreateSessionResponse, LifecycleResponse,
    ListAgentsResponse,
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
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }

    /// Build headers for authenticated requests.
    fn auth_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token)).unwrap(),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers
    }

    /// Handle API error responses.
    async fn handle_error(response: reqwest::Response) -> ClientError {
        let status = response.status().as_u16();
        let message = match response.json::<ApiErrorResponse>().await {
            Ok(err) => err.error,
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
            .headers(self.auth_headers())
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

    /// Create a new agent.
    pub async fn create_agent(&self, name: &str) -> Result<Agent, ClientError> {
        let url = format!("{}/v1/agents", self.base_url);

        let request = CreateAgentRequest {
            name: name.to_string(),
        };

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers())
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
            .headers(self.auth_headers())
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
            .headers(self.auth_headers())
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
            .headers(self.auth_headers())
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
            .headers(self.auth_headers())
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
            .headers(self.auth_headers())
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
            .headers(self.auth_headers())
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
    // Session Operations
    // =========================================================================

    /// Create a new session for an agent.
    pub async fn create_session(
        &self,
        agent_id: &str,
    ) -> Result<CreateSessionResponse, ClientError> {
        let url = format!("{}/v1/agents/{}/sessions", self.base_url, agent_id);

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers())
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::handle_error(response).await);
        }

        let session: CreateSessionResponse = response
            .json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;

        Ok(session)
    }

    /// Close a session.
    pub async fn close_session(&self, session_id: &str) -> Result<(), ClientError> {
        let url = format!("{}/v1/sessions/{}", self.base_url, session_id);

        let response = self
            .client
            .delete(&url)
            .headers(self.auth_headers())
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

    /// Build a WebSocket URL for a session.
    #[must_use]
    pub fn ws_url(&self, session_id: &str) -> String {
        let ws_base = if self.base_url.starts_with("https://") {
            self.base_url.replace("https://", "wss://")
        } else {
            self.base_url.replace("http://", "ws://")
        };
        format!("{}/v1/sessions/{}/ws", ws_base, session_id)
    }
}
