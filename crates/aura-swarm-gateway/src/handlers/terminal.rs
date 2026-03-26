//! Terminal proxy endpoints.
//!
//! Proxies terminal spawn/kill HTTP requests and terminal I/O WebSocket
//! connections to the agent pod. The gateway is a transparent proxy; the
//! JSON terminal protocol (`input`/`output`/`resize`/`exit`) flows
//! unmodified between the client and the agent pod.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tokio_tungstenite::MaybeTlsStream;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::AgentId;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    s.parse()
        .map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

async fn resolve_endpoint<C: ControlPlane>(
    control: &C,
    agent_id: &AgentId,
) -> Result<String, ApiError> {
    control
        .resolve_agent_endpoint(agent_id)
        .await?
        .ok_or(ApiError::AgentUnavailable)
}

// ---------------------------------------------------------------------------
// POST /v1/agents/:agent_id/terminal  – spawn terminal on the pod
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct SpawnTerminalBody {
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
    cwd: Option<String>,
}

fn default_cols() -> u16 { 80 }
fn default_rows() -> u16 { 24 }

pub(crate) async fn spawn_terminal<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    _user: AuthUser,
    Json(body): Json<SpawnTerminalBody>,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let endpoint = resolve_endpoint(&*state.control, &agent_id).await?;

    let url = format!("http://{endpoint}/api/terminal");
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "cols": body.cols,
            "rows": body.rows,
            "cwd": body.cwd,
        }))
        .send()
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to proxy spawn terminal to pod");
            ApiError::AgentUnavailable
        })?;

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let bytes = resp.bytes().await.unwrap_or_default();

    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(bytes))
        .unwrap())
}

// ---------------------------------------------------------------------------
// DELETE /v1/agents/:agent_id/terminal/:terminal_id  – kill terminal on pod
// ---------------------------------------------------------------------------

pub(crate) async fn kill_terminal<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, terminal_id)): Path<(String, String)>,
    _user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let endpoint = resolve_endpoint(&*state.control, &agent_id).await?;

    let url = format!("http://{endpoint}/api/terminal/{terminal_id}");
    let client = reqwest::Client::new();
    let resp = client.delete(&url).send().await.map_err(|e| {
        tracing::error!(error = %e, "Failed to proxy kill terminal to pod");
        ApiError::AgentUnavailable
    })?;

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    Ok(Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap())
}

// ---------------------------------------------------------------------------
// GET /v1/agents/:agent_id/terminal/:terminal_id/ws  – WebSocket proxy
// ---------------------------------------------------------------------------

pub(crate) async fn terminal_ws<C, V>(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, terminal_id)): Path<(String, String)>,
    _user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let endpoint = resolve_endpoint(&*state.control, &agent_id).await?;
    let timeout = state.config.websocket_timeout();
    let agent_id_str = agent_id.to_string();

    Ok(ws.on_upgrade(move |socket| {
        handle_terminal_ws(socket, endpoint, terminal_id, agent_id_str, timeout)
    }))
}

async fn handle_terminal_ws(
    client_socket: WebSocket,
    agent_endpoint: String,
    terminal_id: String,
    agent_id_str: String,
    timeout: std::time::Duration,
) {
    let agent_url = format!("ws://{agent_endpoint}/ws/terminal/{terminal_id}");

    let agent_socket = match tokio::time::timeout(
        timeout,
        tokio_tungstenite::connect_async(&agent_url),
    )
    .await
    {
        Ok(Ok((socket, _))) => socket,
        Ok(Err(e)) => {
            tracing::error!(
                agent_id = %agent_id_str,
                terminal_id = %terminal_id,
                error = %e,
                "Failed to connect to agent terminal"
            );
            return;
        }
        Err(_) => {
            tracing::error!(
                agent_id = %agent_id_str,
                terminal_id = %terminal_id,
                "Timeout connecting to agent terminal"
            );
            return;
        }
    };

    tracing::info!(
        agent_id = %agent_id_str,
        terminal_id = %terminal_id,
        "Terminal WebSocket proxy started"
    );

    let (client_write, client_read) = client_socket.split();
    let (agent_write, agent_read) = agent_socket.split();

    let c2a = forward_client_to_agent(client_read, agent_write);
    let a2c = forward_agent_to_client(agent_read, client_write);

    tokio::select! {
        _ = c2a => {}
        _ = a2c => {}
    }

    tracing::info!(
        agent_id = %agent_id_str,
        terminal_id = %terminal_id,
        "Terminal WebSocket proxy ended"
    );
}

async fn forward_client_to_agent(
    mut client_read: futures::stream::SplitStream<WebSocket>,
    mut agent_write: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        TungsteniteMessage,
    >,
) {
    while let Some(Ok(msg)) = client_read.next().await {
        let tung = match msg {
            Message::Text(t) => TungsteniteMessage::Text(t),
            Message::Binary(b) => TungsteniteMessage::Binary(b),
            Message::Ping(p) => TungsteniteMessage::Ping(p),
            Message::Pong(p) => TungsteniteMessage::Pong(p),
            Message::Close(_) => break,
        };
        if agent_write.send(tung).await.is_err() {
            break;
        }
    }
}

async fn forward_agent_to_client(
    mut agent_read: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    mut client_write: futures::stream::SplitSink<WebSocket, Message>,
) {
    while let Some(Ok(msg)) = agent_read.next().await {
        let axum_msg = match &msg {
            TungsteniteMessage::Text(t) => Some(Message::Text(t.clone())),
            TungsteniteMessage::Binary(b) => Some(Message::Binary(b.clone())),
            TungsteniteMessage::Ping(p) => Some(Message::Ping(p.clone())),
            TungsteniteMessage::Pong(p) => Some(Message::Pong(p.clone())),
            TungsteniteMessage::Close(_) | TungsteniteMessage::Frame(_) => None,
        };
        if let Some(m) = axum_msg {
            if client_write.send(m).await.is_err() {
                break;
            }
        } else if matches!(msg, TungsteniteMessage::Close(_)) {
            break;
        }
    }
}
