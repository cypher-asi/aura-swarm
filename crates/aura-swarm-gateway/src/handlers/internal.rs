//! Internal endpoints for scheduler callbacks.
//!
//! These endpoints are not authenticated — they are protected by network
//! policies that restrict access to cluster-internal traffic only.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::AgentId;
use aura_swarm_store::AgentState;

use crate::error::ApiError;
use crate::state::GatewayState;

fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateStatusRequest {
    pub status: AgentState,
    #[serde(default)]
    #[serde(alias = "message")]
    pub error_message: Option<String>,
}

/// `PATCH /internal/agents/:agent_id/status`
///
/// Called by the scheduler to report pod status changes.
pub(crate) async fn update_agent_status<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    Json(body): Json<UpdateStatusRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;

    state
        .control
        .update_agent_status_internal(&agent_id, body.status, body.error_message)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /internal/health`
///
/// Internal health check for cluster probes (no auth required).
pub(crate) async fn internal_health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
}
