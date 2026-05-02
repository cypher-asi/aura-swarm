//! Automaton (dev-loop / task-runner) proxy handlers.
//!
//! Proxies automaton HTTP and WebSocket requests to the agent pod that
//! implements the actual automaton runtime, following the same
//! resolve-then-forward pattern as the file and terminal proxies.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::AgentId;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

fn validate_automaton_id(id: &str) -> Result<&str, ApiError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::BadRequest(format!("invalid automaton ID: {id}")));
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

async fn proxy_post(endpoint: &str, path: &str, body: Bytes) -> Result<Response, ApiError> {
    let url = format!("http://{endpoint}{path}");
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
        .post(&url)
        .header("content-type", "application/json")
        .body(body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(url = %url, error = %e, "automaton proxy request failed");
            ApiError::AgentUnavailable
        })?;

    let status =
        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = resp.text().await.unwrap_or_default();

    Ok((status, [("content-type", "application/json")], body).into_response())
}

async fn proxy_get(endpoint: &str, path: &str) -> Result<Response, ApiError> {
    let url = format!("http://{endpoint}{path}");
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(url = %url, error = %e, "automaton proxy request failed");
            ApiError::AgentUnavailable
        })?;

    let status =
        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = resp.text().await.unwrap_or_default();

    Ok((status, [("content-type", "application/json")], body).into_response())
}

// ---------------------------------------------------------------------------
// HTTP proxy handlers
// ---------------------------------------------------------------------------

/// `POST /v1/agents/:agent_id/automaton/start`
pub(crate) async fn automaton_start<C, V>(
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
    tracing::info!(%agent_id, %endpoint, "Proxying automaton start");
    proxy_post(&endpoint, "/automaton/start", body).await
}

/// `GET /v1/agents/:agent_id/automaton/:automaton_id/status`
pub(crate) async fn automaton_status<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, automaton_id)): Path<(String, String)>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let automaton_id = validate_automaton_id(&automaton_id)?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;
    proxy_get(&endpoint, &format!("/automaton/{automaton_id}/status")).await
}

/// `POST /v1/agents/:agent_id/automaton/:automaton_id/pause`
pub(crate) async fn automaton_pause<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, automaton_id)): Path<(String, String)>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let automaton_id = validate_automaton_id(&automaton_id)?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;
    proxy_post(
        &endpoint,
        &format!("/automaton/{automaton_id}/pause"),
        Bytes::new(),
    )
    .await
}

/// `POST /v1/agents/:agent_id/automaton/:automaton_id/stop`
pub(crate) async fn automaton_stop<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, automaton_id)): Path<(String, String)>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let automaton_id = validate_automaton_id(&automaton_id)?;
    let endpoint = resolve_endpoint(&state, &agent_id).await?;
    proxy_post(
        &endpoint,
        &format!("/automaton/{automaton_id}/stop"),
        Bytes::new(),
    )
    .await
}

// ---------------------------------------------------------------------------
// Workspace resolve proxy
// ---------------------------------------------------------------------------

/// `GET /v1/agents/:agent_id/workspace/resolve?project_name=...`
pub(crate) async fn workspace_resolve<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    user: AuthUser,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
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
        .timeout(std::time::Duration::from_secs(15))
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

// ---------------------------------------------------------------------------
// WebSocket proxy handler (automaton event stream)
// ---------------------------------------------------------------------------

/// `GET /v1/agents/:agent_id/stream/automaton/:automaton_id`
pub(crate) async fn automaton_events_ws<C, V>(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, automaton_id)): Path<(String, String)>,
    user: AuthUser,
    headers: HeaderMap,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let automaton_id = validate_automaton_id(&automaton_id)?.to_string();
    let endpoint = resolve_endpoint(&state, &agent_id).await?;
    let timeout = state.config.websocket_timeout();

    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    tracing::info!(%agent_id, %automaton_id, %endpoint, "Proxying automaton event stream");

    Ok(ws.on_upgrade(move |socket| {
        handle_automaton_ws_proxy(socket, endpoint, automaton_id, auth_header, timeout)
    }))
}

async fn handle_automaton_ws_proxy(
    client_socket: WebSocket,
    agent_endpoint: String,
    automaton_id: String,
    auth_header: Option<String>,
    timeout: std::time::Duration,
) {
    let agent_url = format!("ws://{agent_endpoint}/stream/automaton/{automaton_id}");

    let mut request = match agent_url.into_client_request() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Failed to build WS request for automaton proxy");
            return;
        }
    };
    if let Some(ref auth) = auth_header {
        if let Ok(val) = auth.parse() {
            request.headers_mut().insert("Authorization", val);
        }
    }

    let agent_socket = match tokio::time::timeout(
        timeout,
        tokio_tungstenite::connect_async(request),
    )
    .await
    {
        Ok(Ok((socket, _))) => socket,
        Ok(Err(e)) => {
            tracing::error!(%automaton_id, error = %e, "Failed to connect to agent automaton WS");
            return;
        }
        Err(_) => {
            tracing::error!(%automaton_id, "Timeout connecting to agent automaton WS");
            return;
        }
    };

    tracing::info!(%automaton_id, "Automaton WS proxy connected");

    let (mut client_write, mut client_read) = client_socket.split();
    let (mut agent_write, mut agent_read) = agent_socket.split();

    let agent_to_client = async {
        while let Some(msg_result) = agent_read.next().await {
            match msg_result {
                Ok(TungsteniteMessage::Text(text)) => {
                    if client_write.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                Ok(TungsteniteMessage::Binary(data)) => {
                    if client_write.send(Message::Binary(data)).await.is_err() {
                        break;
                    }
                }
                Ok(TungsteniteMessage::Close(_)) => break,
                Err(_) => break,
                _ => {}
            }
        }
    };

    let client_to_agent = async {
        while let Some(msg_result) = client_read.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    if agent_write
                        .send(TungsteniteMessage::Text(text))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Err(_) => break,
                _ => {}
            }
        }
    };

    tokio::select! {
        () = agent_to_client => {}
        () = client_to_agent => {}
    }

    tracing::info!(%automaton_id, "Automaton WS proxy ended");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_automaton_id() {
        assert!(validate_automaton_id("abc-123_def").is_ok());
        assert!(validate_automaton_id("a1b2c3").is_ok());
    }

    #[test]
    fn blocks_path_traversal() {
        assert!(validate_automaton_id("../../secret").is_err());
        assert!(validate_automaton_id("../etc/passwd").is_err());
    }

    #[test]
    fn blocks_special_chars() {
        assert!(validate_automaton_id("id;rm -rf /").is_err());
        assert!(validate_automaton_id("id&cmd").is_err());
        assert!(validate_automaton_id("id/path").is_err());
    }

    #[test]
    fn blocks_empty_and_long() {
        assert!(validate_automaton_id("").is_err());
        assert!(validate_automaton_id(&"a".repeat(65)).is_err());
        assert!(validate_automaton_id(&"a".repeat(64)).is_ok());
    }
}
