//! WebSocket proxy handler.
//!
//! This module provides bidirectional WebSocket proxying between clients and agent pods.
//! The gateway is a transparent proxy -- it does not inspect, inject, or mutate any
//! WebSocket messages. The client drives the full harness protocol end-to-end.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::response::Response;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tokio_tungstenite::MaybeTlsStream;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::{ControlPlane, SessionStatus};
use aura_swarm_core::SessionId;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

/// WebSocket connection handler.
///
/// Validates session ownership and upgrades to a WebSocket connection, then
/// proxies messages bidirectionally between the client and the agent pod.
/// No message inspection or side effects -- pure transparent proxy.
///
/// # Errors
///
/// Returns an error if the session is not found, the user doesn't own it,
/// the session is not active, or the agent is unavailable.
pub(crate) async fn websocket_handler<C, V>(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(session_id): Path<String>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let session_id = parse_session_id(&session_id)?;

    let session = state
        .control
        .get_session(&user.user_id, &session_id)
        .await?;

    if session.status != SessionStatus::Active {
        return Err(ApiError::Conflict("session is not active".to_string()));
    }

    let endpoint = state
        .control
        .resolve_agent_endpoint(&session.agent_id)
        .await?
        .ok_or(ApiError::AgentUnavailable)?;

    let timeout = state.config.websocket_timeout();
    let agent_id_str = session.agent_id.to_string();
    let session_id_str = session_id.to_string();
    let user_id_str = user.user_id.to_string();

    tracing::info!(
        session_id = %session_id_str,
        agent_id = %agent_id_str,
        user_id = %user_id_str,
        "WebSocket connection initiated"
    );

    let control = Arc::clone(&state.control);

    Ok(ws.on_upgrade(move |socket| {
        handle_websocket(
            socket,
            endpoint,
            session_id,
            session_id_str,
            agent_id_str,
            user_id_str,
            timeout,
            control,
        )
    }))
}

/// Handle the WebSocket connection after upgrade.
///
/// Connects to the agent's `/stream` endpoint and proxies bidirectionally.
/// When the proxy ends for any reason, the session is automatically closed
/// so that the agent can transition to Idle when no sessions remain.
async fn handle_websocket<C: ControlPlane + 'static>(
    client_socket: WebSocket,
    agent_endpoint: String,
    session_id: SessionId,
    session_id_str: String,
    agent_id_str: String,
    user_id_str: String,
    timeout: std::time::Duration,
    control: Arc<C>,
) {
    let agent_url = format!("ws://{agent_endpoint}/stream");
    let Some(agent_socket) =
        connect_to_agent(&agent_url, timeout, &session_id_str, &agent_id_str).await
    else {
        return;
    };

    tracing::info!(
        session_id = %session_id_str,
        agent_id = %agent_id_str,
        "Connected to agent, starting proxy"
    );

    let (client_write, client_read) = client_socket.split();
    let (agent_write, agent_read) = agent_socket.split();

    let client_to_agent = forward_client_to_agent(client_read, agent_write, &session_id_str);
    let agent_to_client = forward_agent_to_client(agent_read, client_write, &session_id_str);

    tokio::select! {
        result = client_to_agent => {
            if let Err(e) = result {
                tracing::debug!(session_id = %session_id_str, error = %e, "Client to agent forward ended");
            }
        }
        result = agent_to_client => {
            if let Err(e) = result {
                tracing::debug!(session_id = %session_id_str, error = %e, "Agent to client forward ended");
            }
        }
    }

    // Always close the session when the WebSocket proxy ends, regardless of
    // how the connection was terminated.  This prevents orphaned Active
    // sessions that block the Running → Idle transition.
    if let Ok(user_id) = user_id_str.parse::<aura_swarm_core::UserId>() {
        match control.close_session(&user_id, &session_id).await {
            Ok(()) => {
                tracing::info!(session_id = %session_id_str, "Session closed on proxy end");
            }
            Err(e) => {
                tracing::warn!(session_id = %session_id_str, error = %e, "Failed to close session on proxy end");
            }
        }
    }

    tracing::info!(session_id = %session_id_str, "WebSocket proxy ended");
}

/// Connect to the agent's WebSocket endpoint with timeout.
async fn connect_to_agent(
    url: &str,
    timeout: std::time::Duration,
    session_id: &str,
    agent_id: &str,
) -> Option<tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>> {
    match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(url)).await {
        Ok(Ok((socket, _))) => Some(socket),
        Ok(Err(e)) => {
            tracing::error!(
                session_id = %session_id,
                agent_id = %agent_id,
                error = %e,
                "Failed to connect to agent"
            );
            None
        }
        Err(_) => {
            tracing::error!(
                session_id = %session_id,
                agent_id = %agent_id,
                "Timeout connecting to agent"
            );
            None
        }
    }
}

/// Forward messages from client to agent (transparent).
async fn forward_client_to_agent(
    mut client_read: SplitStream<WebSocket>,
    mut agent_write: SplitSink<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        TungsteniteMessage,
    >,
    session_id: &str,
) -> Result<(), String> {
    while let Some(msg_result) = client_read.next().await {
        match msg_result {
            Ok(msg) => {
                let tungstenite_msg = match msg {
                    Message::Text(text) => TungsteniteMessage::Text(text.clone()),
                    Message::Binary(data) => TungsteniteMessage::Binary(data.clone()),
                    Message::Ping(data) => TungsteniteMessage::Ping(data.clone()),
                    Message::Pong(data) => TungsteniteMessage::Pong(data.clone()),
                    Message::Close(_) => {
                        tracing::debug!(session_id = %session_id, "Client closed connection");
                        break;
                    }
                };

                if let Err(e) = agent_write.send(tungstenite_msg).await {
                    return Err(format!("Failed to send to agent: {e}"));
                }
            }
            Err(e) => {
                return Err(format!("Error reading from client: {e}"));
            }
        }
    }
    Ok(())
}

/// Forward messages from agent to client (transparent, no side effects).
async fn forward_agent_to_client(
    mut agent_read: SplitStream<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    mut client_write: SplitSink<WebSocket, Message>,
    session_id: &str,
) -> Result<(), String> {
    while let Some(msg_result) = agent_read.next().await {
        match msg_result {
            Ok(msg) => {
                let axum_msg = match &msg {
                    TungsteniteMessage::Text(text) => Some(Message::Text(text.clone())),
                    TungsteniteMessage::Binary(data) => Some(Message::Binary(data.clone())),
                    TungsteniteMessage::Ping(data) => Some(Message::Ping(data.clone())),
                    TungsteniteMessage::Pong(data) => Some(Message::Pong(data.clone())),
                    TungsteniteMessage::Close(_) | TungsteniteMessage::Frame(_) => None,
                };

                if let Some(axum_msg) = axum_msg {
                    if let Err(e) = client_write.send(axum_msg).await {
                        return Err(format!("Failed to send to client: {e}"));
                    }
                } else if matches!(msg, TungsteniteMessage::Close(_)) {
                    tracing::debug!(session_id = %session_id, "Agent closed connection");
                    break;
                }
            }
            Err(e) => {
                return Err(format!("Error reading from agent: {e}"));
            }
        }
    }
    Ok(())
}

/// Parse a session ID from a string.
fn parse_session_id(s: &str) -> Result<SessionId, ApiError> {
    s.parse()
        .map_err(|_| ApiError::BadRequest(format!("invalid session ID: {s}")))
}
