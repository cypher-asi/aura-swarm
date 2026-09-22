//! Authenticated, read-only Git inspection for a running agent pod.
//! The gateway verifies ownership and resolves the pod; Harness owns
//! filesystem sandboxing and Git execution inside that environment.

use std::sync::Arc;
use std::time::Duration;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::AgentId;
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

const MAX_PATH_BYTES: usize = 4 * 1024;

#[derive(Deserialize)]
pub(crate) struct GitStatusRequest {
    path: String,
}

#[derive(Deserialize)]
pub(crate) struct GitDiffRequest {
    path: String,
    file: String,
    area: String,
}

fn validate_path(value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() || value.len() > MAX_PATH_BYTES {
        return Err(ApiError::BadRequest("invalid remote Git path".into()));
    }
    Ok(())
}

fn pod_git_error(status: u16) -> ApiError {
    match status {
        400 => ApiError::BadRequest("remote Git request was rejected".into()),
        403 => ApiError::Forbidden,
        404 => ApiError::NotFound("remote Git path".into()),
        413 => ApiError::PayloadTooLarge("remote Git result exceeds its limit".into()),
        _ => ApiError::AgentUnavailable,
    }
}

async fn proxy_git<C, V>(
    state: &GatewayState<C, V>,
    user: &AuthUser,
    agent_id: &str,
    endpoint_path: &'static str,
    query: &[(&str, &str)],
) -> Result<Json<serde_json::Value>, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id =
        AgentId::from_hex(agent_id).map_err(|_| ApiError::BadRequest("invalid agent ID".into()))?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let endpoint = state
        .control
        .resolve_agent_endpoint(&agent_id)
        .await?
        .ok_or(ApiError::AgentUnavailable)?;
    let url = format!("http://{endpoint}{endpoint_path}");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default();
    let response = client
        .get(url)
        .query(query)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| ApiError::AgentUnavailable)?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        tracing::warn!(status, "agent pod rejected Git inspection");
        return Err(pod_git_error(status));
    }
    let body = response
        .json()
        .await
        .map_err(|_| ApiError::Internal("invalid Git response from agent pod".into()))?;
    Ok(Json(body))
}

pub(crate) async fn git_status<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    user: AuthUser,
    Json(request): Json<GitStatusRequest>,
) -> Result<Json<serde_json::Value>, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    validate_path(&request.path)?;
    proxy_git(
        &state,
        &user,
        &agent_id,
        "/api/git/status",
        &[("path", &request.path)],
    )
    .await
}

pub(crate) async fn git_diff<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    user: AuthUser,
    Json(request): Json<GitDiffRequest>,
) -> Result<Json<serde_json::Value>, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    validate_path(&request.path)?;
    validate_path(&request.file)?;
    if !matches!(request.area.as_str(), "staged" | "worktree") {
        return Err(ApiError::BadRequest("invalid remote Git area".into()));
    }
    proxy_git(
        &state,
        &user,
        &agent_id,
        "/api/git/diff",
        &[
            ("path", &request.path),
            ("file", &request.file),
            ("area", &request.area),
        ],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn rejects_unbounded_paths_and_maps_pod_failures() {
        assert!(validate_path("/workspace/project").is_ok());
        assert!(validate_path("").is_err());
        assert!(validate_path(&"x".repeat(MAX_PATH_BYTES + 1)).is_err());
        assert_eq!(pod_git_error(403).status_code(), StatusCode::FORBIDDEN);
        assert_eq!(
            pod_git_error(413).status_code(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            pod_git_error(503).status_code(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
