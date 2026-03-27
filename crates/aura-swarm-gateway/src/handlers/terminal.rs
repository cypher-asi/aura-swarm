//! Terminal WebSocket proxy.
//!
//! Provides a single WebSocket endpoint that proxies the full terminal
//! lifecycle (spawn, I/O, kill-on-close) to the agent pod. No HTTP
//! forwarding is needed — the client sends a `spawn` message as the
//! first frame and the pod allocates the PTY. When either side closes
//! the connection the pod tears down the terminal automatically.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tokio_tungstenite::MaybeTlsStream;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::AgentId;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

/// `GET /v1/agents/:agent_id/terminal/ws`
///
/// Upgrades to a WebSocket and transparently proxies all frames to
/// `ws://{pod_ip}:8080/ws/terminal` on the agent pod.
pub(crate) async fn terminal_ws<C, V>(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;

    let endpoint = state
        .control
        .resolve_agent_endpoint(&agent_id)
        .await?
        .ok_or(ApiError::AgentUnavailable)?;

    let timeout = state.config.websocket_timeout();
    let agent_id_str = agent_id.to_string();
    let token = user.token;

    Ok(ws.on_upgrade(move |socket| {
        proxy_terminal(socket, endpoint, agent_id_str, token, timeout)
    }))
}

async fn proxy_terminal(
    client_socket: WebSocket,
    agent_endpoint: String,
    agent_id_str: String,
    token: String,
    timeout: std::time::Duration,
) {
    let agent_url = format!("ws://{agent_endpoint}/ws/terminal");

    let mut request = match agent_url.into_client_request() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(agent_id = %agent_id_str, error = %e, "Invalid agent terminal URL");
            return;
        }
    };
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}").parse().expect("valid header value"),
    );

    let agent_socket = match tokio::time::timeout(
        timeout,
        tokio_tungstenite::connect_async(request),
    )
    .await
    {
        Ok(Ok((socket, _))) => socket,
        Ok(Err(e)) => {
            tracing::error!(
                agent_id = %agent_id_str,
                error = %e,
                "Failed to connect to agent terminal"
            );
            return;
        }
        Err(_) => {
            tracing::error!(
                agent_id = %agent_id_str,
                "Timeout connecting to agent terminal"
            );
            return;
        }
    };

    tracing::info!(agent_id = %agent_id_str, "Terminal WS proxy started");

    let (client_write, client_read) = client_socket.split();
    let (agent_write, agent_read) = agent_socket.split();

    let c2a = forward_client_to_agent(client_read, agent_write);
    let a2c = forward_agent_to_client(agent_read, client_write);

    tokio::select! {
        _ = c2a => {}
        _ = a2c => {}
    }

    tracing::info!(agent_id = %agent_id_str, "Terminal WS proxy ended");
}

async fn forward_client_to_agent(
    mut rx: futures::stream::SplitStream<WebSocket>,
    mut tx: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        TungsteniteMessage,
    >,
) {
    while let Some(Ok(msg)) = rx.next().await {
        let tung = match msg {
            Message::Text(t) => TungsteniteMessage::Text(t),
            Message::Binary(b) => TungsteniteMessage::Binary(b),
            Message::Ping(p) => TungsteniteMessage::Ping(p),
            Message::Pong(p) => TungsteniteMessage::Pong(p),
            Message::Close(_) => break,
        };
        if tx.send(tung).await.is_err() {
            break;
        }
    }
}

async fn forward_agent_to_client(
    mut rx: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    mut tx: futures::stream::SplitSink<WebSocket, Message>,
) {
    while let Some(Ok(msg)) = rx.next().await {
        let axum_msg = match &msg {
            TungsteniteMessage::Text(t) => Some(Message::Text(t.clone())),
            TungsteniteMessage::Binary(b) => Some(Message::Binary(b.clone())),
            TungsteniteMessage::Ping(p) => Some(Message::Ping(p.clone())),
            TungsteniteMessage::Pong(p) => Some(Message::Pong(p.clone())),
            TungsteniteMessage::Close(_) | TungsteniteMessage::Frame(_) => None,
        };
        if let Some(m) = axum_msg {
            if tx.send(m).await.is_err() {
                break;
            }
        } else if matches!(msg, TungsteniteMessage::Close(_)) {
            break;
        }
    }
}
