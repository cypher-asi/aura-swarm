//! WebSocket proxy handler.
//!
//! This module provides bidirectional WebSocket proxying between clients and agent pods.

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
/// Validates the session and upgrades to a WebSocket connection, then
/// proxies messages bidirectionally between the client and the agent pod.
///
/// # Errors
///
/// Returns an error if the session is not found, the user doesn't own it,
/// the session is not active, or the agent is unavailable.
pub async fn websocket_handler<C, V>(
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

    // Validate session ownership
    let session = state
        .control
        .get_session(&user.user_id, &session_id)
        .await?;

    // Check session is active
    if session.status != SessionStatus::Active {
        return Err(ApiError::Conflict("session is not active".to_string()));
    }

    // Get agent endpoint
    let endpoint = state
        .control
        .resolve_agent_endpoint(&session.agent_id)
        .await?
        .ok_or(ApiError::AgentUnavailable)?;

    let timeout = state.config.websocket_timeout();
    let agent_id_str = session.agent_id.to_string();

    tracing::info!(
        session_id = %session_id,
        agent_id = %agent_id_str,
        user_id = %user.user_id,
        "WebSocket connection initiated"
    );

    Ok(ws.on_upgrade(move |socket| {
        handle_websocket(
            socket,
            endpoint,
            session_id.to_string(),
            agent_id_str,
            timeout,
        )
    }))
}

/// Handle the WebSocket connection after upgrade.
///
/// Connects to the agent's `/stream` endpoint for real-time streaming.
async fn handle_websocket(
    client_socket: WebSocket,
    agent_endpoint: String,
    session_id: String,
    agent_id: String,
    timeout: std::time::Duration,
) {
    // Connect to agent's streaming endpoint
    let agent_url = format!("ws://{agent_endpoint}/stream");

    let agent_socket =
        match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(&agent_url)).await {
            Ok(Ok((socket, _))) => socket,
            Ok(Err(e)) => {
                tracing::error!(
                    session_id = %session_id,
                    agent_id = %agent_id,
                    error = %e,
                    "Failed to connect to agent"
                );
                return;
            }
            Err(_) => {
                tracing::error!(
                    session_id = %session_id,
                    agent_id = %agent_id,
                    "Timeout connecting to agent"
                );
                return;
            }
        };

    tracing::info!(
        session_id = %session_id,
        agent_id = %agent_id,
        "Connected to agent, starting proxy"
    );

    // Split both sockets for bidirectional forwarding
    let (client_write, client_read) = client_socket.split();
    let (agent_write, agent_read) = agent_socket.split();

    // Run both directions concurrently
    let client_to_agent = forward_client_to_agent(client_read, agent_write, &session_id);
    let agent_to_client = forward_agent_to_client(agent_read, client_write, &session_id);

    tokio::select! {
        result = client_to_agent => {
            if let Err(e) = result {
                tracing::debug!(session_id = %session_id, error = %e, "Client to agent forward ended");
            }
        }
        result = agent_to_client => {
            if let Err(e) = result {
                tracing::debug!(session_id = %session_id, error = %e, "Agent to client forward ended");
            }
        }
    }

    tracing::info!(session_id = %session_id, "WebSocket proxy ended");
}

/// Forward messages from client to agent.
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

/// Forward messages from agent to client.
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
                let axum_msg = match msg {
                    TungsteniteMessage::Text(text) => Message::Text(text),
                    TungsteniteMessage::Binary(data) => Message::Binary(data),
                    TungsteniteMessage::Ping(data) => Message::Ping(data),
                    TungsteniteMessage::Pong(data) => Message::Pong(data),
                    TungsteniteMessage::Close(_) => {
                        tracing::debug!(session_id = %session_id, "Agent closed connection");
                        break;
                    }
                    TungsteniteMessage::Frame(_) => continue,
                };

                if let Err(e) = client_write.send(axum_msg).await {
                    return Err(format!("Failed to send to client: {e}"));
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
