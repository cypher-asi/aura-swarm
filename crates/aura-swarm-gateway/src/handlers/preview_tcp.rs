//! Owner-authenticated Preview TCP tunnel proxy.
//!
//! This is intentionally narrower than a general-purpose tunnel: the caller
//! chooses only a port from a fixed development-server allowlist, while the
//! harness always connects to its own loopback interface.

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
use crate::handlers::ws::{keepalive_interval, next_keepalive_tick};
use crate::state::GatewayState;

type AgentSocket = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

pub(crate) fn is_preview_port_allowed(port: u16) -> bool {
    matches!(
        port,
        3000 | 3001
            | 3002
            | 3003
            | 3030
            | 4000
            | 4200
            | 4321
            | 5000
            | 5173
            | 5174
            | 5500
            | 5501
            | 5555
            | 6006
            | 7000
            | 7070
            | 8000
            | 8001
            | 8080
            | 8081
            | 8088
            | 8888
            | 9000
            | 9001
            | 9090
    )
}

/// `GET /v1/agents/:agent_id/preview/tcp/:port/ws`
pub(crate) async fn preview_tcp_ws<C, V>(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, port)): Path<(String, u16)>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    if !is_preview_port_allowed(port) {
        return Err(ApiError::BadRequest(
            "port is not allowed for Preview".to_string(),
        ));
    }

    let agent_id = parse_agent_id(&agent_id)?;
    let _ = state.control.get_agent(&user.user_id, &agent_id).await?;
    let endpoint = state
        .control
        .resolve_agent_endpoint(&agent_id)
        .await?
        .ok_or(ApiError::AgentUnavailable)?;

    let timeout = state.config.websocket_timeout();
    let keepalive = state.config.websocket_keepalive();
    let agent_id_str = agent_id.to_string();
    let agent_socket =
        connect_agent_tunnel(&endpoint, port, &user.token, timeout, &agent_id_str).await?;

    Ok(ws.on_upgrade(move |socket| {
        proxy_preview_tcp(socket, agent_socket, agent_id_str, port, keepalive)
    }))
}

async fn proxy_preview_tcp(
    client_socket: WebSocket,
    agent_socket: AgentSocket,
    agent_id: String,
    port: u16,
    keepalive: Option<std::time::Duration>,
) {
    tracing::info!(%agent_id, port, "Preview TCP proxy started");
    let (client_write, client_read) = client_socket.split();
    let (agent_write, agent_read) = agent_socket.split();

    tokio::select! {
        () = forward_client_to_agent(client_read, agent_write, keepalive) => {}
        () = forward_agent_to_client(agent_read, client_write, keepalive) => {}
    }
    tracing::info!(%agent_id, port, "Preview TCP proxy ended");
}

async fn connect_agent_tunnel(
    agent_endpoint: &str,
    port: u16,
    token: &str,
    timeout: std::time::Duration,
    agent_id: &str,
) -> Result<AgentSocket, ApiError> {
    let agent_url = format!("ws://{agent_endpoint}/ws/preview/tcp/{port}");
    let mut request = match agent_url.into_client_request() {
        Ok(request) => request,
        Err(error) => {
            tracing::error!(%agent_id, %error, "invalid agent Preview tunnel URL");
            return Err(ApiError::Internal(
                "invalid agent Preview tunnel URL".to_string(),
            ));
        }
    };
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}")
            .parse()
            .expect("valid header value"),
    );

    match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(request)).await {
        Ok(Ok((socket, _))) => Ok(socket),
        Ok(Err(error)) => {
            tracing::warn!(%agent_id, port, %error, "failed to connect Preview tunnel");
            Err(ApiError::AgentUnavailable)
        }
        Err(_) => {
            tracing::warn!(%agent_id, port, "timed out connecting Preview tunnel");
            Err(ApiError::AgentUnavailable)
        }
    }
}

async fn forward_client_to_agent(
    mut rx: futures::stream::SplitStream<WebSocket>,
    mut tx: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        TungsteniteMessage,
    >,
    keepalive: Option<std::time::Duration>,
) {
    let mut ping = keepalive_interval(keepalive);
    loop {
        tokio::select! {
            next = rx.next() => {
                let Some(Ok(message)) = next else { break };
                let upstream = match message {
                    Message::Binary(bytes) => TungsteniteMessage::Binary(bytes),
                    Message::Ping(bytes) => TungsteniteMessage::Ping(bytes),
                    Message::Pong(bytes) => TungsteniteMessage::Pong(bytes),
                    Message::Close(_) | Message::Text(_) => break,
                };
                if tx.send(upstream).await.is_err() { break; }
            }
            () = next_keepalive_tick(&mut ping) => {
                if tx.send(TungsteniteMessage::Ping(Vec::new())).await.is_err() { break; }
            }
        }
    }
}

async fn forward_agent_to_client(
    mut rx: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    mut tx: futures::stream::SplitSink<WebSocket, Message>,
    keepalive: Option<std::time::Duration>,
) {
    let mut ping = keepalive_interval(keepalive);
    loop {
        tokio::select! {
            next = rx.next() => {
                let Some(Ok(message)) = next else { break };
                let client = match message {
                    TungsteniteMessage::Binary(bytes) => Some(Message::Binary(bytes)),
                    TungsteniteMessage::Ping(bytes) => Some(Message::Ping(bytes)),
                    TungsteniteMessage::Pong(bytes) => Some(Message::Pong(bytes)),
                    TungsteniteMessage::Close(_) => break,
                    TungsteniteMessage::Text(_) | TungsteniteMessage::Frame(_) => None,
                };
                if let Some(message) = client {
                    if tx.send(message).await.is_err() { break; }
                }
            }
            () = next_keepalive_tick(&mut ping) => {
                if tx.send(Message::Ping(Vec::new())).await.is_err() { break; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_preview_port_allowed;

    #[test]
    fn preview_tunnel_port_policy_is_narrow() {
        assert!(is_preview_port_allowed(5173));
        assert!(is_preview_port_allowed(8080));
        assert!(!is_preview_port_allowed(22));
        assert!(!is_preview_port_allowed(5432));
        assert!(!is_preview_port_allowed(65535));
    }
}
