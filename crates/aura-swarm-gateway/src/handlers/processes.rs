//! Process API pass-through proxy.
//!
//! Proxies the harness in-VM process API (`/v1/processes`) so an owner can
//! register/list scheduled processes (cron triggers) on a confidential agent
//! without direct pod access. Confidential agents run as Peer Pods whose
//! workload lives in an off-cluster pod VM, so `kubectl port-forward` cannot
//! reach the harness — this authenticated pod proxy is the supported path (see
//! `docs/spec/v0.2.0/06-agent-runtime.md` §2.6). Pure pass-through: owner-JWT
//! auth via [`AuthUser`] + ownership check through the control plane, body
//! forwarded verbatim, `503 agent_unavailable` when the agent has no running
//! pod. The harness still registers the trigger metadata back to the gateway
//! (`PUT /internal/agents/:id/process-triggers`) regardless of how the process
//! is created, so no extra control-plane handling is needed here.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::AgentId;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

/// Harness route the proxy targets inside the pod VM.
const HARNESS_PROCESSES_PATH: &str = "/v1/processes";

/// Parse an agent ID from a hex string.
fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

/// Resolve the pod endpoint, surfacing `503 agent_unavailable` when the agent
/// has no running pod (same semantics as the secrets/run proxies).
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

/// Forward a pod response to the caller, preserving the status code. The body
/// is relayed verbatim (process prompts/config stay opaque to the gateway).
async fn forward_response(resp: reqwest::Response) -> Result<Response, ApiError> {
    let status =
        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = resp.bytes().await.unwrap_or_default();
    Ok((status, [("content-type", "application/json")], body).into_response())
}

/// `GET /v1/agents/:agent_id/processes`
///
/// Proxies to pod `GET /v1/processes` — the owner's registered processes.
pub(crate) async fn list_processes<C, V>(
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
    let url = format!("http://{endpoint}{HARNESS_PROCESSES_PATH}");
    let resp = http_client()
        .get(&url)
        .header("authorization", format!("Bearer {}", user.token))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(url = %url, error = %e, "processes proxy GET failed");
            ApiError::AgentUnavailable
        })?;
    forward_response(resp).await
}

/// `POST /v1/agents/:agent_id/processes`
///
/// Proxies the JSON body to pod `POST /v1/processes` verbatim (create a
/// process / cron trigger). The body is opaque bytes — never deserialized,
/// stored, or logged here.
pub(crate) async fn create_process<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    user: AuthUser,
    body: Bytes,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;
    let url = format!("http://{endpoint}{HARNESS_PROCESSES_PATH}");
    let resp = http_client()
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", user.token))
        .body(body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(url = %url, error = %e, "processes proxy POST failed");
            ApiError::AgentUnavailable
        })?;
    forward_response(resp).await
}
