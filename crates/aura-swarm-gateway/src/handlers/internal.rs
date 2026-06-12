//! Internal endpoints for scheduler callbacks.
//!
//! These endpoints are not authenticated — they are protected by network
//! policies that restrict access to cluster-internal traffic only.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::AgentId;
use aura_swarm_store::{AgentLogSnapshot, AgentState, LogLine};

use crate::error::ApiError;
use crate::state::GatewayState;

fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

pub(crate) fn require_internal_auth<C, V>(
    state: &GatewayState<C, V>,
    headers: &HeaderMap,
) -> Result<(), ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let Some(expected) = state.config.internal_token.as_deref() else {
        tracing::error!("Rejecting internal request because INTERNAL_TOKEN is not configured");
        return Err(ApiError::Unauthorized);
    };

    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if token == expected => Ok(()),
        _ => Err(ApiError::Unauthorized),
    }
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
    require_internal_auth(&state, &headers)?;

    let agent_id = parse_agent_id(&agent_id)?;

    state
        .control
        .update_agent_status_internal(&agent_id, body.status, body.error_message)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Request body for a pod-log termination snapshot shipped by the
/// scheduler (Swarm TEE upgrade phase 12).
#[derive(Debug, Deserialize)]
pub(crate) struct LogSnapshotRequest {
    /// When the scheduler captured the tail.
    pub captured_at: DateTime<Utc>,
    /// Why the pod was terminated.
    pub reason: String,
    /// The captured tail, oldest line first.
    pub entries: Vec<LogLine>,
}

/// `POST /internal/agents/:agent_id/log-snapshot`
///
/// Called by the scheduler with the pod's final stdout tail before it
/// deletes the pod. Stored in the capped `agent_logs` CF so platform
/// logs survive hibernate/stop. Requires the internal bearer token.
pub(crate) async fn store_log_snapshot<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Json(body): Json<LogSnapshotRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    require_internal_auth(&state, &headers)?;

    let agent_id = parse_agent_id(&agent_id)?;

    state
        .control
        .store_log_snapshot_internal(AgentLogSnapshot {
            agent_id,
            captured_at: body.captured_at,
            reason: body.reason,
            entries: body.entries,
        })
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /internal/health`
///
/// Internal health check for service-to-service probes. Reports the
/// store's `schema_version` (R2) so deploy verification can prove the
/// v1 → v2 migration ran; `null` if the store read fails (the probe
/// itself stays healthy).
pub(crate) async fn internal_health<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    require_internal_auth(&state, &headers)?;
    let schema_version = state.control.schema_version().await.ok();
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "schema_version": schema_version,
        })),
    ))
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
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    require_internal_auth(&state, &headers)?;

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
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    require_internal_auth(&state, &headers)?;

    let agents = state.control.list_all_agents().await?;
    let entries: Vec<InternalAgentEntry> =
        agents.into_iter().map(InternalAgentEntry::from).collect();

    Ok(Json(entries))
}
