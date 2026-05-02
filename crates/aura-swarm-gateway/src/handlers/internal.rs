//! Internal endpoints for scheduler callbacks.
//!
//! These endpoints are not authenticated — they are protected by network
//! policies that restrict access to cluster-internal traffic only.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

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
/// Requires the internal bearer token when configured.
pub(crate) async fn update_agent_status<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Json(body): Json<UpdateStatusRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    // Verify internal token when configured
    if let Some(ref expected) = state.config.internal_token {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match provided {
            Some(token) if token == expected => {}
            _ => return Err(ApiError::Unauthorized),
        }
    }

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

/// Compact representation of an agent for internal reconciliation and deploy checks.
#[derive(Debug, Serialize)]
pub(crate) struct InternalAgentEntry {
    pub agent_id: String,
    pub user_id: String,
    pub name: String,
    pub status: AgentState,
    pub spec: aura_swarm_store::AgentSpec,
}

impl From<aura_swarm_store::Agent> for InternalAgentEntry {
    fn from(agent: aura_swarm_store::Agent) -> Self {
        Self {
            agent_id: agent.agent_id.to_hex(),
            user_id: agent.user_id.to_string(),
            name: agent.name,
            status: agent.status,
            spec: agent.spec,
        }
    }
}

/// `GET /internal/agents/active`
///
/// Returns agents in Provisioning / Running / Idle states so the scheduler
/// can reconcile desired vs actual pod state. No auth required -- protected
/// by network policies restricting access to cluster-internal traffic.
pub(crate) async fn list_active_agents<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agents = state.control.list_active_agents().await?;

    let entries: Vec<InternalAgentEntry> =
        agents.into_iter().map(InternalAgentEntry::from).collect();

    Ok(Json(entries))
}

/// `GET /internal/agents/all`
///
/// Returns every persisted agent so deploy verification can prove machine IDs
/// are preserved across redeploy, including inactive lifecycle states.
pub(crate) async fn list_all_agents<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agents = state.control.list_all_agents().await?;
    let entries: Vec<InternalAgentEntry> =
        agents.into_iter().map(InternalAgentEntry::from).collect();

    Ok(Json(entries))
}
