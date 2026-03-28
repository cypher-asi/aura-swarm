//! File operation proxies.
//!
//! Proxies directory listing and file read requests to the agent pod's
//! HTTP endpoints, following the same resolve-then-forward pattern as
//! the terminal WebSocket proxy.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::AgentId;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

#[derive(serde::Deserialize)]
pub(crate) struct ListFilesRequest {
    #[serde(default = "default_path")]
    path: String,
    #[serde(default = "default_depth")]
    depth: usize,
}

fn default_path() -> String { ".".into() }
fn default_depth() -> usize { 3 }

#[derive(serde::Deserialize)]
pub(crate) struct ReadFileRequest {
    path: String,
}

/// `POST /v1/agents/:agent_id/files`
///
/// Proxies the request to `http://{pod_ip}:8080/api/files` on the agent pod.
pub(crate) async fn list_files<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    _user: AuthUser,
    Json(body): Json<ListFilesRequest>,
) -> Result<Json<serde_json::Value>, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;

    let endpoint = state
        .control
        .resolve_agent_endpoint(&agent_id)
        .await?
        .ok_or(ApiError::AgentUnavailable)?;

    let depth = body.depth.min(20);
    let url = format!("http://{endpoint}/api/files");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .query(&[("path", &body.path), ("depth", &depth.to_string())])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to reach agent pod for file listing");
            ApiError::AgentUnavailable
        })?;

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| ApiError::Internal(format!("bad response from agent pod: {e}")))?;

    Ok(Json(json))
}

/// `POST /v1/agents/:agent_id/read-file`
///
/// Proxies the request to `http://{pod_ip}:8080/api/read-file` on the agent pod.
pub(crate) async fn read_file<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    _user: AuthUser,
    Json(body): Json<ReadFileRequest>,
) -> Result<Json<serde_json::Value>, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;

    let endpoint = state
        .control
        .resolve_agent_endpoint(&agent_id)
        .await?
        .ok_or(ApiError::AgentUnavailable)?;

    let url = format!("http://{endpoint}/api/read-file");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .query(&[("path", &body.path)])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to reach agent pod for file read");
            ApiError::AgentUnavailable
        })?;

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| ApiError::Internal(format!("bad response from agent pod: {e}")))?;

    Ok(Json(json))
}
