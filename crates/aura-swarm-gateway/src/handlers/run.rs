//! Run proxy handlers (`POST /v1/run` contract).
//!
//! Proxies the harness run lifecycle to the agent pod that hosts the actual
//! runtime. A run is created with `POST /v1/run` (returning a `run_id`); the
//! client then attaches to the event stream via the WebSocket handler in
//! [`crate::handlers::ws`].
//!
//! Unlike the legacy automaton handlers, every request here FORWARDS the
//! caller's bearer `Authorization` header to the pod — the migrated harness
//! gateway auth-gates all `/v1/run*` routes.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::AgentId;
use aura_swarm_protocol::{
    AgentIdentity, ModelSelection, RuntimeRequest, RuntimeRequestType, RuntimeRunResponse,
    WorkspaceLocation,
};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

/// Parse an agent ID from a hex string.
fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

/// Validate a harness `run_id` before interpolating it into a proxied path.
///
/// Mirrors the old `automaton.rs::validate_automaton_id` guard to prevent path
/// injection. Reused by the WS-attach handler.
pub(crate) fn validate_run_id(id: &str) -> Result<&str, ApiError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::BadRequest(format!("invalid run ID: {id}")));
    }
    Ok(id)
}

async fn resolve_endpoint<C, V>(
    state: &GatewayState<C, V>,
    agent_id: &AgentId,
) -> Result<String, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    state
        .control
        .resolve_agent_endpoint(agent_id)
        .await?
        .ok_or(ApiError::AgentUnavailable)
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

/// Proxy a GET to the pod, forwarding the bearer token.
async fn proxy_get_auth(endpoint: &str, path: &str, token: &str) -> Result<Response, ApiError> {
    let url = format!("http://{endpoint}{path}");
    let resp = http_client()
        .get(&url)
        .header("authorization", format!("Bearer {token}"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(url = %url, error = %e, "run proxy GET failed");
            ApiError::AgentUnavailable
        })?;

    forward_response(resp).await
}

/// Proxy a POST to the pod, forwarding the bearer token.
async fn proxy_post_auth(
    endpoint: &str,
    path: &str,
    token: &str,
    body: Bytes,
) -> Result<Response, ApiError> {
    let url = format!("http://{endpoint}{path}");
    let resp = http_client()
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(url = %url, error = %e, "run proxy POST failed");
            ApiError::AgentUnavailable
        })?;

    forward_response(resp).await
}

/// Convert a pod `reqwest::Response` into an axum response, preserving status.
async fn forward_response(resp: reqwest::Response) -> Result<Response, ApiError> {
    let status =
        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = resp.text().await.unwrap_or_default();
    Ok((status, [("content-type", "application/json")], body).into_response())
}

// ---------------------------------------------------------------------------
// Request body
// ---------------------------------------------------------------------------

/// Body accepted by `POST /v1/agents/:agent_id/run`.
///
/// All fields are optional: an empty body starts a chat run with default
/// model/workspace selection. The gateway always overrides `user_id` and
/// `auth_jwt` from the authenticated caller, so clients cannot spoof them.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RunRequestBody {
    /// Run discriminator (chat / dev_loop / task_run). Defaults to chat.
    #[serde(default, rename = "type")]
    r#type: Option<RuntimeRequestType>,
    /// Agent identity bundle.
    #[serde(default)]
    agent_identity: Option<AgentIdentity>,
    /// Model selection.
    #[serde(default)]
    model: Option<ModelSelection>,
    /// Workspace location.
    #[serde(default)]
    workspace: Option<WorkspaceLocation>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /v1/agents/:agent_id/run`
///
/// Builds a [`RuntimeRequest`] from the body (chat by default), stamps the
/// authenticated `user_id` + `auth_jwt`, POSTs it to the pod's `/v1/run`,
/// tracks a swarm session keyed to the returned `run_id`, and returns the
/// harness [`RuntimeRunResponse`] with `event_stream_url` rewritten to the
/// swarm-facing WS path.
pub(crate) async fn run_start<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    user: AuthUser,
    body: Option<Json<RunRequestBody>>,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;

    let body = body.map(|b| b.0).unwrap_or_default();

    let mut request = RuntimeRequest::chat(user.user_id.to_string());
    if let Some(kind) = body.r#type {
        request.r#type = kind;
    }
    if let Some(identity) = body.agent_identity {
        request.agent_identity = identity;
    }
    if let Some(model) = body.model {
        request.model = model;
    }
    if let Some(workspace) = body.workspace {
        request.workspace = workspace;
    }
    request.auth_jwt = Some(user.token.clone());

    let payload = serde_json::to_vec(&request)
        .map_err(|e| ApiError::Internal(format!("failed to serialize run request: {e}")))?;

    tracing::info!(%agent_id, %endpoint, "Proxying run start");

    let url = format!("http://{endpoint}/v1/run");
    let resp = http_client()
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", user.token))
        .body(payload)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(url = %url, error = %e, "run start proxy failed");
            ApiError::AgentUnavailable
        })?;

    // On any non-success status, forward the pod's error verbatim.
    if !resp.status().is_success() {
        return forward_response(resp).await;
    }

    let run: RuntimeRunResponse = resp
        .json()
        .await
        .map_err(|e| ApiError::Internal(format!("invalid run response from pod: {e}")))?;

    let run_id = validate_run_id(&run.run_id)?.to_string();

    // Track a swarm session bound to this run so the WS-attach close path can
    // run ownership + idle detection from the run id alone.
    state
        .control
        .create_run_session(&user.user_id, &agent_id, run_id.clone())
        .await?;

    let rewritten = RuntimeRunResponse {
        event_stream_url: format!("/v1/agents/{agent_id}/stream/{run_id}"),
        run_id: run.run_id,
    };

    Ok((StatusCode::CREATED, Json(rewritten)).into_response())
}

/// `GET /v1/agents/:agent_id/run/list`
pub(crate) async fn run_list<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;
    proxy_get_auth(&endpoint, "/v1/run/list", &user.token).await
}

/// `GET /v1/agents/:agent_id/run/:run_id/status`
pub(crate) async fn run_status<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, run_id)): Path<(String, String)>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let run_id = validate_run_id(&run_id)?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;
    proxy_get_auth(&endpoint, &format!("/v1/run/{run_id}/status"), &user.token).await
}

/// `POST /v1/agents/:agent_id/run/:run_id/pause`
pub(crate) async fn run_pause<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, run_id)): Path<(String, String)>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let run_id = validate_run_id(&run_id)?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;
    proxy_post_auth(
        &endpoint,
        &format!("/v1/run/{run_id}/pause"),
        &user.token,
        Bytes::new(),
    )
    .await
}

/// `POST /v1/agents/:agent_id/run/:run_id/stop`
pub(crate) async fn run_stop<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, run_id)): Path<(String, String)>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let run_id = validate_run_id(&run_id)?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;
    proxy_post_auth(
        &endpoint,
        &format!("/v1/run/{run_id}/stop"),
        &user.token,
        Bytes::new(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_run_id() {
        assert!(validate_run_id("abc-123_def").is_ok());
        assert!(validate_run_id("a1b2c3").is_ok());
    }

    #[test]
    fn blocks_path_traversal() {
        assert!(validate_run_id("../../secret").is_err());
        assert!(validate_run_id("../etc/passwd").is_err());
    }

    #[test]
    fn blocks_special_chars() {
        assert!(validate_run_id("id;rm -rf /").is_err());
        assert!(validate_run_id("id&cmd").is_err());
        assert!(validate_run_id("id/path").is_err());
    }

    #[test]
    fn blocks_empty_and_long() {
        assert!(validate_run_id("").is_err());
        assert!(validate_run_id(&"a".repeat(65)).is_err());
        assert!(validate_run_id(&"a".repeat(64)).is_ok());
    }
}
