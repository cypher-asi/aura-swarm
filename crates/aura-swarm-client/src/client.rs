//! `SwarmClient` — typed HTTP client for remote agent management.
//!
//! All methods use `remote_agent` naming to align with the aura-os integration
//! contract. Internally they call the gateway's `/v1/agents` and `/v1/sessions`
//! endpoints.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, StatusCode};

use aura_swarm_protocol::RuntimeRunResponse;

use crate::error::SwarmClientError;
use crate::types::{
    ApiErrorResponse, CreateRemoteAgentRequest, CreateSessionRequest, CreateSessionResponse,
    ListRemoteAgentsResponse, ListSessionsResponse, RemoteAgent, RemoteAgentLogEntry,
    RemoteAgentLogsResponse, RemoteAgentSpec, RemoteAgentStateResponse, SessionResponse,
};

/// Typed HTTP client for the aura-swarm gateway.
///
/// Uses `remote_agent` naming for all agent-related operations to establish
/// a clear contract boundary between the local aura-os agent and its backing
/// remote VM managed by the swarm.
#[derive(Debug, Clone)]
pub struct SwarmClient {
    client: Client,
    base_url: String,
    token: String,
}

impl SwarmClient {
    /// Create a new `SwarmClient`.
    ///
    /// # Arguments
    ///
    /// * `base_url` — Base URL of the gateway (e.g. `http://localhost:8080`)
    /// * `token` — JWT bearer token for authentication
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying HTTP client cannot be built.
    pub fn new(
        base_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self, SwarmClientError> {
        let client = Client::builder().timeout(Duration::from_secs(30)).build()?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        })
    }

    /// Create a `SwarmClient` with a custom timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying HTTP client cannot be built.
    pub fn with_timeout(
        base_url: impl Into<String>,
        token: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, SwarmClientError> {
        let client = Client::builder().timeout(timeout).build()?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        })
    }

    // =========================================================================
    // Remote Agent CRUD
    // =========================================================================

    /// Create a remote agent.
    ///
    /// If `remote_agent_id` is supplied the gateway will use it as the agent ID,
    /// enabling ID parity between the local aura-os agent and the remote VM.
    ///
    /// # Errors
    ///
    /// Returns an error on transport failure or if the gateway rejects the request.
    pub async fn create_remote_agent(
        &self,
        name: &str,
        spec: Option<RemoteAgentSpec>,
        remote_agent_id: Option<&str>,
    ) -> Result<RemoteAgent, SwarmClientError> {
        let url = format!("{}/v1/agents", self.base_url);

        let body = CreateRemoteAgentRequest {
            name: name.to_string(),
            spec,
            agent_id: remote_agent_id.map(String::from),
        };

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::extract_error(response).await);
        }

        response
            .json()
            .await
            .map_err(|e| SwarmClientError::Parse(e.to_string()))
    }

    /// Get a remote agent by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent is not found or the request fails.
    pub async fn get_remote_agent(
        &self,
        remote_agent_id: &str,
    ) -> Result<RemoteAgent, SwarmClientError> {
        let url = format!("{}/v1/agents/{}", self.base_url, remote_agent_id);

        let response = self
            .client
            .get(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::extract_error(response).await);
        }

        response
            .json()
            .await
            .map_err(|e| SwarmClientError::Parse(e.to_string()))
    }

    /// List all remote agents for the authenticated user.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails.
    pub async fn list_remote_agents(&self) -> Result<Vec<RemoteAgent>, SwarmClientError> {
        let url = format!("{}/v1/agents", self.base_url);

        let response = self
            .client
            .get(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::extract_error(response).await);
        }

        let body: ListRemoteAgentsResponse = response
            .json()
            .await
            .map_err(|e| SwarmClientError::Parse(e.to_string()))?;

        Ok(body.agents)
    }

    /// Delete a remote agent.
    ///
    /// The agent must typically be in a stopped state before deletion.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent is not found or cannot be deleted.
    pub async fn delete_remote_agent(&self, remote_agent_id: &str) -> Result<(), SwarmClientError> {
        let url = format!("{}/v1/agents/{}", self.base_url, remote_agent_id);

        let response = self
            .client
            .delete(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if response.status() != StatusCode::NO_CONTENT && !response.status().is_success() {
            return Err(Self::extract_error(response).await);
        }

        Ok(())
    }

    /// Get the current state of a remote agent's VM/pod.
    ///
    /// Returns lifecycle state only — no resource metrics or logs.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent is not found or the request fails.
    pub async fn get_remote_agent_state(
        &self,
        remote_agent_id: &str,
    ) -> Result<RemoteAgentStateResponse, SwarmClientError> {
        let url = format!("{}/v1/agents/{}/state", self.base_url, remote_agent_id);

        let response = self
            .client
            .get(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::extract_error(response).await);
        }

        response
            .json()
            .await
            .map_err(|e| SwarmClientError::Parse(e.to_string()))
    }

    /// Get a remote agent's VM/platform logs.
    ///
    /// Returns the live pod stdout tail (when a pod is running) merged
    /// with stored termination snapshots, each entry tagged with its
    /// [`crate::types::RemoteLogSource`] and sorted by timestamp.
    ///
    /// * `tail` — keep only the last N merged entries (server default 100)
    /// * `since` — RFC3339 timestamp; only entries at or after it
    ///
    /// # Errors
    ///
    /// Returns an error if the agent is not found or the request fails.
    pub async fn get_agent_logs(
        &self,
        remote_agent_id: &str,
        tail: Option<u32>,
        since: Option<&str>,
    ) -> Result<Vec<RemoteAgentLogEntry>, SwarmClientError> {
        let url = format!("{}/v1/agents/{}/logs", self.base_url, remote_agent_id);

        let mut request = self.client.get(&url).headers(self.auth_headers()?);
        if let Some(tail) = tail {
            request = request.query(&[("tail", tail.to_string())]);
        }
        if let Some(since) = since {
            request = request.query(&[("since", since)]);
        }

        let response = request.send().await?;

        if !response.status().is_success() {
            return Err(Self::extract_error(response).await);
        }

        let body: RemoteAgentLogsResponse = response
            .json()
            .await
            .map_err(|e| SwarmClientError::Parse(e.to_string()))?;

        Ok(body.logs)
    }

    // =========================================================================
    // Session Operations
    // =========================================================================

    /// Create a new session for a remote agent.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent is not found, not ready, or the request fails.
    pub async fn create_session(
        &self,
        remote_agent_id: &str,
        config: CreateSessionRequest,
    ) -> Result<CreateSessionResponse, SwarmClientError> {
        let url = format!("{}/v1/agents/{}/sessions", self.base_url, remote_agent_id);

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .json(&config)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::extract_error(response).await);
        }

        response
            .json()
            .await
            .map_err(|e| SwarmClientError::Parse(e.to_string()))
    }

    /// Get a session by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the session is not found or the request fails.
    pub async fn get_session(&self, session_id: &str) -> Result<SessionResponse, SwarmClientError> {
        let url = format!("{}/v1/sessions/{}", self.base_url, session_id);

        let response = self
            .client
            .get(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::extract_error(response).await);
        }

        response
            .json()
            .await
            .map_err(|e| SwarmClientError::Parse(e.to_string()))
    }

    /// List sessions for a remote agent.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent is not found or the request fails.
    pub async fn list_sessions(
        &self,
        remote_agent_id: &str,
    ) -> Result<Vec<SessionResponse>, SwarmClientError> {
        let url = format!("{}/v1/agents/{}/sessions", self.base_url, remote_agent_id);

        let response = self
            .client
            .get(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::extract_error(response).await);
        }

        let body: ListSessionsResponse = response
            .json()
            .await
            .map_err(|e| SwarmClientError::Parse(e.to_string()))?;

        Ok(body.sessions)
    }

    /// Close a session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session is not found or the request fails.
    pub async fn close_session(&self, session_id: &str) -> Result<(), SwarmClientError> {
        let url = format!("{}/v1/sessions/{}", self.base_url, session_id);

        let response = self
            .client
            .delete(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if response.status() != StatusCode::NO_CONTENT && !response.status().is_success() {
            return Err(Self::extract_error(response).await);
        }

        Ok(())
    }

    // =========================================================================
    // Run Operations
    // =========================================================================

    /// Start a chat run for a remote agent via `POST /v1/agents/:id/run`.
    ///
    /// Returns the harness run handle. `run_id` identifies the run and
    /// `event_stream_url` is the swarm-facing WS path (already rewritten by the
    /// gateway) that the caller attaches to with
    /// [`crate::ws::connect_run_stream`]. Use [`SwarmClient::run_ws_url`] to
    /// build a fully-qualified `ws(s)://` URL for that path.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent is not found, not ready, or the request fails.
    pub async fn create_run(
        &self,
        remote_agent_id: &str,
    ) -> Result<RuntimeRunResponse, SwarmClientError> {
        let url = format!("{}/v1/agents/{}/run", self.base_url, remote_agent_id);

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .json(&serde_json::json!({}))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Self::extract_error(response).await);
        }

        response
            .json()
            .await
            .map_err(|e| SwarmClientError::Parse(e.to_string()))
    }

    /// Stop an in-flight run via `POST /v1/agents/:id/run/:run_id/stop`.
    ///
    /// # Errors
    ///
    /// Returns an error if the run is not found or the request fails.
    pub async fn stop_run(
        &self,
        remote_agent_id: &str,
        run_id: &str,
    ) -> Result<(), SwarmClientError> {
        let url = format!(
            "{}/v1/agents/{}/run/{}/stop",
            self.base_url, remote_agent_id, run_id
        );

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .send()
            .await?;

        if response.status() != StatusCode::NO_CONTENT && !response.status().is_success() {
            return Err(Self::extract_error(response).await);
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

    /// Build a fully-qualified WebSocket URL for attaching to a run's event
    /// stream (`/v1/agents/:agent_id/stream/:run_id`).
    #[must_use]
    pub fn run_ws_url(&self, remote_agent_id: &str, run_id: &str) -> String {
        let ws_base = if self.base_url.starts_with("https://") {
            self.base_url.replace("https://", "wss://")
        } else {
            self.base_url.replace("http://", "ws://")
        };
        format!("{ws_base}/v1/agents/{remote_agent_id}/stream/{run_id}")
    }

    // =========================================================================
    // Internal helpers
    // =========================================================================

    fn auth_headers(&self) -> Result<HeaderMap, SwarmClientError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token)).map_err(|e| {
                SwarmClientError::Parse(format!("invalid authorization header: {e}"))
            })?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    async fn extract_error(response: reqwest::Response) -> SwarmClientError {
        let status = response.status().as_u16();
        match response.json::<ApiErrorResponse>().await {
            Ok(err) => SwarmClientError::from_api_response(status, err),
            Err(_) => SwarmClientError::from_status(status),
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_trims_trailing_slash() {
        let client = SwarmClient::new("http://example.com/", "tok").unwrap();
        assert_eq!(client.base_url(), "http://example.com");
    }

    #[test]
    fn new_preserves_clean_url() {
        let client = SwarmClient::new("http://example.com", "tok").unwrap();
        assert_eq!(client.base_url(), "http://example.com");
    }

    #[test]
    fn new_stores_token() {
        let client = SwarmClient::new("http://example.com", "my-secret").unwrap();
        assert_eq!(client.token(), "my-secret");
    }

    #[test]
    fn run_ws_url_http_to_ws() {
        let client = SwarmClient::new("http://localhost:8080", "tok").unwrap();
        let url = client.run_ws_url("agent-1", "run-1");
        assert!(url.starts_with("ws://"));
        assert!(!url.contains("http://"));
    }

    #[test]
    fn run_ws_url_https_to_wss() {
        let client = SwarmClient::new("https://gateway.example.com", "tok").unwrap();
        let url = client.run_ws_url("agent-2", "run-2");
        assert!(url.starts_with("wss://"));
    }

    #[test]
    fn run_ws_url_includes_agent_and_run() {
        let client = SwarmClient::new("http://localhost:8080", "tok").unwrap();
        assert_eq!(
            client.run_ws_url("abc-123", "run-9"),
            "ws://localhost:8080/v1/agents/abc-123/stream/run-9"
        );
    }

    #[test]
    fn run_ws_url_with_trailing_slash_base() {
        let client = SwarmClient::new("https://api.example.com/", "tok").unwrap();
        assert_eq!(
            client.run_ws_url("a1", "r1"),
            "wss://api.example.com/v1/agents/a1/stream/r1"
        );
    }

    // Endpoint path correctness tests — verify the client constructs the right URLs.
    // These use wiremock to assert the actual HTTP requests.

    mod endpoint_paths {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use super::*;
        use crate::types::RemoteAgentState;

        async fn setup() -> (MockServer, SwarmClient) {
            let server = MockServer::start().await;
            let client = SwarmClient::new(server.uri(), "test-token").unwrap();
            (server, client)
        }

        #[tokio::test]
        async fn create_remote_agent_path() {
            let (server, client) = setup().await;

            Mock::given(method("POST"))
                .and(path("/v1/agents"))
                .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "agent_id": "abc",
                    "name": "test",
                    "status": "provisioning",
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z"
                })))
                .expect(1)
                .mount(&server)
                .await;

            let agent = client
                .create_remote_agent("test", None, Some("abc"))
                .await
                .unwrap();
            assert_eq!(agent.agent_id, "abc");
        }

        #[tokio::test]
        async fn get_remote_agent_path() {
            let (server, client) = setup().await;

            Mock::given(method("GET"))
                .and(path("/v1/agents/agent-42"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "agent_id": "agent-42",
                    "name": "test",
                    "status": "running",
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z"
                })))
                .expect(1)
                .mount(&server)
                .await;

            let agent = client.get_remote_agent("agent-42").await.unwrap();
            assert_eq!(agent.agent_id, "agent-42");
        }

        #[tokio::test]
        async fn list_remote_agents_path() {
            let (server, client) = setup().await;

            Mock::given(method("GET"))
                .and(path("/v1/agents"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({ "agents": [] })),
                )
                .expect(1)
                .mount(&server)
                .await;

            let agents = client.list_remote_agents().await.unwrap();
            assert!(agents.is_empty());
        }

        #[tokio::test]
        async fn delete_remote_agent_path() {
            let (server, client) = setup().await;

            Mock::given(method("DELETE"))
                .and(path("/v1/agents/agent-42"))
                .respond_with(ResponseTemplate::new(204))
                .expect(1)
                .mount(&server)
                .await;

            client.delete_remote_agent("agent-42").await.unwrap();
        }

        #[tokio::test]
        async fn get_remote_agent_state_path() {
            let (server, client) = setup().await;

            Mock::given(method("GET"))
                .and(path("/v1/agents/agent-42/state"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "state": "running",
                    "uptime_seconds": 120,
                    "active_sessions": 1
                })))
                .expect(1)
                .mount(&server)
                .await;

            let state = client.get_remote_agent_state("agent-42").await.unwrap();
            assert_eq!(state.state, RemoteAgentState::Running);
            assert_eq!(state.uptime_seconds, 120);
        }

        #[tokio::test]
        async fn get_agent_logs_path_and_query() {
            use wiremock::matchers::query_param;

            use crate::types::RemoteLogSource;

            let (server, client) = setup().await;

            Mock::given(method("GET"))
                .and(path("/v1/agents/agent-42/logs"))
                .and(query_param("tail", "50"))
                .and(query_param("since", "2026-06-12T08:00:00Z"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "logs": [
                        {
                            "timestamp": "2026-06-12T08:00:00Z",
                            "line": "old shutdown",
                            "source": "snapshot"
                        },
                        {
                            "timestamp": "2026-06-12T08:01:00Z",
                            "line": "booting harness",
                            "source": "live"
                        }
                    ]
                })))
                .expect(1)
                .mount(&server)
                .await;

            let logs = client
                .get_agent_logs("agent-42", Some(50), Some("2026-06-12T08:00:00Z"))
                .await
                .unwrap();
            assert_eq!(logs.len(), 2);
            assert_eq!(logs[0].line, "old shutdown");
            assert_eq!(logs[0].source, RemoteLogSource::Snapshot);
            assert_eq!(logs[1].line, "booting harness");
            assert_eq!(logs[1].source, RemoteLogSource::Live);
        }

        #[tokio::test]
        async fn get_agent_logs_path_without_params() {
            let (server, client) = setup().await;

            Mock::given(method("GET"))
                .and(path("/v1/agents/agent-42/logs"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({ "logs": [] })),
                )
                .expect(1)
                .mount(&server)
                .await;

            let logs = client.get_agent_logs("agent-42", None, None).await.unwrap();
            assert!(logs.is_empty());
        }

        #[tokio::test]
        async fn create_session_path() {
            let (server, client) = setup().await;

            Mock::given(method("POST"))
                .and(path("/v1/agents/agent-42/sessions"))
                .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "session_id": "sess-1",
                    "agent_id": "agent-42",
                    "run_url": "/v1/agents/agent-42/run",
                    "created_at": "2026-01-01T00:00:00Z"
                })))
                .expect(1)
                .mount(&server)
                .await;

            let session = client
                .create_session("agent-42", CreateSessionRequest::default())
                .await
                .unwrap();
            assert_eq!(session.session_id, "sess-1");
            assert_eq!(session.run_url, "/v1/agents/agent-42/run");
        }

        #[tokio::test]
        async fn create_run_path() {
            let (server, client) = setup().await;

            Mock::given(method("POST"))
                .and(path("/v1/agents/agent-42/run"))
                .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "run_id": "run-7",
                    "event_stream_url": "/v1/agents/agent-42/stream/run-7"
                })))
                .expect(1)
                .mount(&server)
                .await;

            let run = client.create_run("agent-42").await.unwrap();
            assert_eq!(run.run_id, "run-7");
            assert_eq!(run.event_stream_url, "/v1/agents/agent-42/stream/run-7");
        }

        #[tokio::test]
        async fn stop_run_path() {
            let (server, client) = setup().await;

            Mock::given(method("POST"))
                .and(path("/v1/agents/agent-42/run/run-7/stop"))
                .respond_with(ResponseTemplate::new(204))
                .expect(1)
                .mount(&server)
                .await;

            client.stop_run("agent-42", "run-7").await.unwrap();
        }

        #[tokio::test]
        async fn get_session_path() {
            let (server, client) = setup().await;

            Mock::given(method("GET"))
                .and(path("/v1/sessions/sess-1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "session_id": "sess-1",
                    "agent_id": "agent-42",
                    "status": "active",
                    "created_at": "2026-01-01T00:00:00Z"
                })))
                .expect(1)
                .mount(&server)
                .await;

            let session = client.get_session("sess-1").await.unwrap();
            assert_eq!(session.session_id, "sess-1");
        }

        #[tokio::test]
        async fn list_sessions_path() {
            let (server, client) = setup().await;

            Mock::given(method("GET"))
                .and(path("/v1/agents/agent-42/sessions"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({ "sessions": [] })),
                )
                .expect(1)
                .mount(&server)
                .await;

            let sessions = client.list_sessions("agent-42").await.unwrap();
            assert!(sessions.is_empty());
        }

        #[tokio::test]
        async fn close_session_path() {
            let (server, client) = setup().await;

            Mock::given(method("DELETE"))
                .and(path("/v1/sessions/sess-1"))
                .respond_with(ResponseTemplate::new(204))
                .expect(1)
                .mount(&server)
                .await;

            client.close_session("sess-1").await.unwrap();
        }

        #[tokio::test]
        async fn api_error_extracted() {
            let (server, client) = setup().await;

            Mock::given(method("GET"))
                .and(path("/v1/agents/missing"))
                .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "error": {
                        "code": "not_found",
                        "message": "Agent not found"
                    }
                })))
                .expect(1)
                .mount(&server)
                .await;

            let err = client.get_remote_agent("missing").await.unwrap_err();
            assert!(err.is_not_found());
            match err {
                SwarmClientError::Api {
                    status,
                    code,
                    message,
                } => {
                    assert_eq!(status, 404);
                    assert_eq!(code, "not_found");
                    assert_eq!(message, "Agent not found");
                }
                _ => panic!("expected Api error"),
            }
        }
    }
}
