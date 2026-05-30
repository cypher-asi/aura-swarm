//! Workspace-resolve proxy handler.
//!
//! The automaton HTTP/WS proxy handlers were removed when the harness migrated
//! to the unified `POST /v1/run` + `WS /stream/:run_id` contract (see
//! [`crate::handlers::run`] and [`crate::handlers::ws`]). This module retains
//! only the workspace-resolve proxy, which is unchanged on the pod side.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::AgentId;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
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

/// `GET /v1/agents/:agent_id/workspace/resolve?project_name=...`
pub(crate) async fn workspace_resolve<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    user: AuthUser,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;
    let project_name = params.get("project_name").cloned().unwrap_or_default();
    let url = format!("http://{endpoint}/workspace/resolve");
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
        .get(&url)
        .query(&[("project_name", &project_name)])
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(url = %url, error = %e, "workspace resolve proxy failed");
            ApiError::AgentUnavailable
        })?;
    let status =
        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = resp.text().await.unwrap_or_default();
    Ok((status, [("content-type", "application/json")], body).into_response())
}
