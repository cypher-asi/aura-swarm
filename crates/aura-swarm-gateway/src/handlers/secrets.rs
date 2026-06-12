//! Secrets vault pass-through proxies (Swarm TEE upgrade phase 6 / R1).
//!
//! Proxies the in-TEE secrets vault served by the harness inside the
//! agent pod (`/secrets`, `/secrets/:name`). The gateway is a **pure
//! pass-through**: it never persists, caches, or logs secret values.
//! Request/response bodies are forwarded verbatim and are not captured
//! by any middleware — the router's `TraceLayer` records only
//! method/uri/status, never bodies or the `Authorization` header.
//!
//! Auth follows the files/run proxy pattern exactly: owner-JWT
//! authentication via [`AuthUser`], ownership check through the control
//! plane, and endpoint resolution that yields `503 agent_unavailable`
//! when the agent has no running pod (no implicit wake).
//!
//! Trust-boundary note: secret values transit the gateway in plaintext
//! *within TLS* between client ↔ gateway and within the cluster network
//! gateway ↔ pod. End-to-end HPKE encryption (client sealing directly
//! to the TEE) is future work; at rest the vault seals values under the
//! attestation-released per-agent DEK inside the guest.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::AgentId;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

/// Parse an agent ID from a hex string.
fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

/// Validate a secret name before interpolating it into a proxied path.
///
/// Mirrors the harness vault's name rules (`[A-Za-z0-9._-]`, ≤128
/// chars) and the run-proxy's `validate_run_id` guard against path
/// injection.
fn validate_secret_name(name: &str) -> Result<&str, ApiError> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(ApiError::BadRequest(format!("invalid secret name: {name}")));
    }
    Ok(name)
}

/// Resolve the pod endpoint, surfacing `503 agent_unavailable` when the
/// agent is not running (same semantics as the files proxy).
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

/// Forward a pod response to the caller, preserving the status code.
///
/// The body is relayed verbatim and intentionally never logged.
async fn forward_response(resp: reqwest::Response) -> Result<Response, ApiError> {
    let status =
        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = resp.bytes().await.unwrap_or_default();
    Ok((status, [("content-type", "application/json")], body).into_response())
}

/// Proxy a bodyless request (GET/DELETE) to the pod vault, forwarding
/// the caller's bearer token (the harness auth-gates its routes).
async fn proxy_no_body(
    method: reqwest::Method,
    endpoint: &str,
    path_and_query: &str,
    token: &str,
) -> Result<Response, ApiError> {
    let url = format!("http://{endpoint}{path_and_query}");
    let resp = http_client()
        .request(method, &url)
        .header("authorization", format!("Bearer {token}"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            // Transport errors only — never request/response content.
            tracing::warn!(url = %url, error = %e, "secrets proxy request failed");
            ApiError::AgentUnavailable
        })?;
    forward_response(resp).await
}

/// `GET /v1/agents/:agent_id/secrets`
///
/// Proxies to pod `GET /secrets` — names + metadata only, never values.
pub(crate) async fn list_secrets<C, V>(
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
    proxy_no_body(reqwest::Method::GET, &endpoint, "/secrets", &user.token).await
}

#[derive(Deserialize)]
pub(crate) struct GetSecretQuery {
    #[serde(default)]
    reveal: Option<bool>,
}

/// `GET /v1/agents/:agent_id/secrets/:name`
///
/// Proxies to pod `GET /secrets/:name`, passing the `reveal` query
/// parameter through. With `reveal=true` the response carries the
/// secret value; the gateway relays it without inspecting or logging
/// the body.
pub(crate) async fn get_secret<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, name)): Path<(String, String)>,
    Query(query): Query<GetSecretQuery>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let name = validate_secret_name(&name)?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;

    let mut path = format!("/secrets/{name}");
    if let Some(reveal) = query.reveal {
        path.push_str(if reveal { "?reveal=true" } else { "?reveal=false" });
    }
    proxy_no_body(reqwest::Method::GET, &endpoint, &path, &user.token).await
}

/// `PUT /v1/agents/:agent_id/secrets/:name`
///
/// Proxies the JSON body (`{ "value": ..., "description": ... }`) to
/// pod `PUT /secrets/:name` verbatim. The body is handled as opaque
/// bytes — it is never deserialized, stored, or logged here.
pub(crate) async fn put_secret<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, name)): Path<(String, String)>,
    user: AuthUser,
    body: Bytes,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let name = validate_secret_name(&name)?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;

    let url = format!("http://{endpoint}/secrets/{name}");
    let resp = http_client()
        .put(&url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", user.token))
        .body(body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(url = %url, error = %e, "secrets proxy PUT failed");
            ApiError::AgentUnavailable
        })?;
    forward_response(resp).await
}

/// `DELETE /v1/agents/:agent_id/secrets/:name`
///
/// Proxies to pod `DELETE /secrets/:name`.
pub(crate) async fn delete_secret<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, name)): Path<(String, String)>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let name = validate_secret_name(&name)?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;
    proxy_no_body(
        reqwest::Method::DELETE,
        &endpoint,
        &format!("/secrets/{name}"),
        &user.token,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_secret_names() {
        assert!(validate_secret_name("api-key").is_ok());
        assert!(validate_secret_name("stripe.live_2024").is_ok());
        assert!(validate_secret_name(&"a".repeat(128)).is_ok());
    }

    #[test]
    fn blocks_path_traversal_and_specials() {
        assert!(validate_secret_name("../../etc/passwd").is_err());
        assert!(validate_secret_name("a/b").is_err());
        assert!(validate_secret_name("a?reveal=true").is_err());
        assert!(validate_secret_name("a b").is_err());
        assert!(validate_secret_name("").is_err());
        assert!(validate_secret_name(&"a".repeat(129)).is_err());
    }
}
