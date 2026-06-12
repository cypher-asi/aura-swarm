//! Process-trigger registration + read endpoints (Swarm TEE upgrade
//! phase 8, "trigger outside, data inside").
//!
//! The harness inside each agent VM pushes its current trigger set to
//! `PUT /internal/agents/:id/process-triggers` after every process
//! mutation (replace semantics — the body is the full desired set).
//! Owners can inspect what is registered via
//! `GET /v1/agents/:id/process-triggers`.
//!
//! # Trust boundary
//!
//! Only `(process_id, cron, enabled, next_run_at)` may ever cross from
//! the VM into the control plane. The registration body deserializes
//! into [`TriggerRegistration`], an explicit DTO that structurally
//! cannot carry a prompt, config, or run data — serde strips any
//! unexpected fields a (buggy or malicious) caller includes. Cron
//! expressions and process ids are re-validated server-side before
//! they are persisted.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::{ControlPlane, TriggerRegistration};
use aura_swarm_core::AgentId;
use aura_swarm_store::ProcessTrigger;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

use super::internal::require_internal_auth;

fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

/// Registered trigger metadata as returned by the read API.
#[derive(Debug, Serialize)]
pub(crate) struct ProcessTriggerView {
    pub process_id: String,
    pub cron: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<DateTime<Utc>>,
    pub registered_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<ProcessTrigger> for ProcessTriggerView {
    fn from(t: ProcessTrigger) -> Self {
        Self {
            process_id: t.process_id,
            cron: t.cron,
            enabled: t.enabled,
            next_run_at: t.next_run_at,
            last_run_at: t.last_run_at,
            registered_at: t.registered_at,
            updated_at: t.updated_at,
        }
    }
}

/// `PUT /internal/agents/:agent_id/process-triggers`
///
/// Replace-semantics sync from the harness: the body is the full
/// desired trigger set (`[{process_id, cron, enabled, next_run_at}]`);
/// triggers absent from the body are unregistered. Requires the
/// internal bearer token, like the other `/internal` routes.
pub(crate) async fn replace_triggers<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Json(body): Json<Vec<TriggerRegistration>>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    require_internal_auth(&state, &headers)?;
    let agent_id = parse_agent_id(&agent_id)?;

    let stored = state
        .control
        .replace_process_triggers_internal(&agent_id, body)
        .await?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "registered": stored.len() })),
    ))
}

/// `DELETE /internal/agents/:agent_id/process-triggers/:process_id`
///
/// Unregister a single trigger. The replace-sync above is the primary
/// mechanism; this exists for targeted cleanup.
pub(crate) async fn delete_trigger<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    headers: HeaderMap,
    Path((agent_id, process_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    require_internal_auth(&state, &headers)?;
    let agent_id = parse_agent_id(&agent_id)?;

    state
        .control
        .delete_process_trigger_internal(&agent_id, &process_id)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/agents/:agent_id/process-triggers`
///
/// Owner-facing read of the registered trigger metadata (owner JWT +
/// ownership check, like the other agent routes). Returns only what
/// the agent exported plus control-plane bookkeeping — there is no
/// process content here to leak.
pub(crate) async fn list_triggers<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    user: AuthUser,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;

    let triggers = state
        .control
        .list_process_triggers(&user.user_id, &agent_id)
        .await?;

    let views: Vec<ProcessTriggerView> = triggers.into_iter().map(Into::into).collect();
    Ok(Json(serde_json::json!({ "triggers": views })))
}
