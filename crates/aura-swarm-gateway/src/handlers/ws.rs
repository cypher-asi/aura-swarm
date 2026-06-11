//! WebSocket run-attach proxy handler.
//!
//! This module provides bidirectional WebSocket proxying between clients and
//! agent pods for an in-flight run. A run is first created via
//! `POST /v1/agents/:agent_id/run` ([`crate::handlers::run`]); the client then
//! attaches to `GET /v1/agents/:agent_id/stream/:run_id`, which proxies to the
//! pod's `ws://{pod}/stream/{run_id}`.
//!
//! Successful harness protocol traffic remains transparent. The gateway only
//! emits a typed error frame when it cannot connect to the pod after the client
//! websocket has already upgraded.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::response::Response;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tokio_tungstenite::MaybeTlsStream;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;
use aura_swarm_core::{AgentId, SessionId};

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::handlers::run::validate_run_id;
use crate::state::GatewayState;

/// Parse an agent ID from a hex string.
fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

/// WebSocket run-attach handler.
///
/// Verifies agent ownership, resolves the swarm session bound to the run, then
/// upgrades and proxies messages bidirectionally between the client and the
/// pod's `/stream/:run_id`. No message inspection or side effects -- pure
/// transparent proxy, except that the swarm session is closed when the proxy
/// ends so the agent can transition to Idle when no sessions remain.
///
/// # Errors
///
/// Returns an error if the agent or run session is not found, the user doesn't
/// own it, the session is not active, or the agent is unavailable.
pub(crate) async fn run_attach_handler<C, V>(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState<C, V>>>,
    Path((agent_id, run_id)): Path<(String, String)>,
    user: AuthUser,
) -> Result<Response, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let run_id = validate_run_id(&run_id)?.to_string();

    // Verify ownership and resolve the session that the run handler created.
    let session = state
        .control
        .find_session_by_run(&user.user_id, &agent_id, &run_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("active run {run_id}")))?;

    let endpoint = state
        .control
        .resolve_agent_endpoint(&agent_id)
        .await?
        .ok_or(ApiError::AgentUnavailable)?;

    let timeout = state.config.websocket_timeout();
    let keepalive = state.config.websocket_keepalive();
    let agent_id_str = agent_id.to_string();
    let session_id = session.session_id;
    let session_id_str = session_id.to_string();
    let user_id_str = user.user_id.to_string();
    let token = user.token;

    tracing::info!(
        run_id = %run_id,
        session_id = %session_id_str,
        agent_id = %agent_id_str,
        user_id = %user_id_str,
        "WebSocket run attach initiated"
    );

    let control = Arc::clone(&state.control);

    Ok(ws.on_upgrade(move |socket| {
        handle_websocket(
            socket,
            endpoint,
            run_id,
            session_id,
            session_id_str,
            agent_id_str,
            user_id_str,
            token,
            timeout,
            keepalive,
            control,
        )
    }))
}

/// Handle the WebSocket connection after upgrade.
///
/// Connects to the agent's `/stream/:run_id` endpoint and proxies
/// bidirectionally. When the proxy ends for any reason, the session is
/// automatically closed so that the agent can transition to Idle when no
/// sessions remain.
#[allow(clippy::too_many_arguments)]
async fn handle_websocket<C: ControlPlane + 'static>(
    mut client_socket: WebSocket,
    agent_endpoint: String,
    run_id: String,
    session_id: SessionId,
    session_id_str: String,
    agent_id_str: String,
    user_id_str: String,
    token: String,
    timeout: std::time::Duration,
    keepalive: Option<std::time::Duration>,
    control: Arc<C>,
) {
    let agent_url = format!("ws://{agent_endpoint}/stream/{run_id}");
    let Some(agent_socket) =
        connect_to_agent(&agent_url, &token, timeout, &session_id_str, &agent_id_str).await
    else {
        send_gateway_error(
            &mut client_socket,
            "agent_stream_unavailable",
            "Gateway could not connect to the remote agent stream.",
        )
        .await;
        close_session_on_proxy_end(&control, &user_id_str, &session_id, &session_id_str).await;
        return;
    };

    tracing::info!(
        session_id = %session_id_str,
        agent_id = %agent_id_str,
        "Connected to agent, starting proxy"
    );

    let (client_write, client_read) = client_socket.split();
    let (agent_write, agent_read) = agent_socket.split();

    let client_to_agent =
        forward_client_to_agent(client_read, agent_write, keepalive, &session_id_str);
    let agent_to_client =
        forward_agent_to_client(agent_read, client_write, keepalive, &session_id_str);

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
    // how the connection was terminated. This prevents orphaned Active
    // sessions that block the Running -> Idle transition.
    close_session_on_proxy_end(&control, &user_id_str, &session_id, &session_id_str).await;

    tracing::info!(session_id = %session_id_str, "WebSocket proxy ended");
}

async fn send_gateway_error(socket: &mut WebSocket, code: &str, message: &str) {
    let payload = gateway_error_payload(code, message);
    let _ = socket.send(Message::Text(payload.to_string().into())).await;
    let _ = socket.send(Message::Close(None)).await;
}

fn gateway_error_payload(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "code": code,
        "message": message,
        "recoverable": true,
    })
}

async fn close_session_on_proxy_end<C: ControlPlane + 'static>(
    control: &Arc<C>,
    user_id_str: &str,
    session_id: &SessionId,
    session_id_str: &str,
) {
    if let Ok(user_id) = user_id_str.parse::<aura_swarm_core::UserId>() {
        match control.close_session(&user_id, session_id).await {
            Ok(()) => {
                tracing::info!(session_id = %session_id_str, "Session closed on proxy end");
            }
            Err(e) => {
                tracing::warn!(session_id = %session_id_str, error = %e, "Failed to close session on proxy end");
            }
        }
    }
}

/// Connect to the agent's WebSocket endpoint with timeout, forwarding the
/// client JWT so the harness can use it for LLM proxy and domain tool calls.
async fn connect_to_agent(
    url: &str,
    token: &str,
    timeout: std::time::Duration,
    session_id: &str,
    agent_id: &str,
) -> Option<tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>> {
    let mut request = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(session_id = %session_id, error = %e, "Invalid agent URL");
            return None;
        }
    };
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}")
            .parse()
            .expect("valid header value"),
    );

    match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(request)).await {
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

/// Await the next keepalive tick, or never resolve when keepalive is
/// disabled. Lets a `tokio::select!` branch stay inert without a
/// dedicated code path.
pub(crate) async fn next_keepalive_tick(interval: &mut Option<tokio::time::Interval>) {
    match interval {
        Some(i) => {
            i.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Build the optional keepalive interval, skipping the immediate first
/// tick so we don't fire a ping the instant the proxy starts.
pub(crate) fn keepalive_interval(
    keepalive: Option<std::time::Duration>,
) -> Option<tokio::time::Interval> {
    keepalive.map(|period| {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.reset(); // first tick after `period`, not immediately
        interval
    })
}

/// Forward messages from client to agent (transparent), originating a
/// periodic Ping toward the agent to keep the path warm.
async fn forward_client_to_agent(
    mut client_read: SplitStream<WebSocket>,
    mut agent_write: SplitSink<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        TungsteniteMessage,
    >,
    keepalive: Option<std::time::Duration>,
    session_id: &str,
) -> Result<(), String> {
    let mut ping = keepalive_interval(keepalive);
    loop {
        tokio::select! {
            msg_result = client_read.next() => {
                let Some(msg_result) = msg_result else { break };
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
            () = next_keepalive_tick(&mut ping) => {
                if let Err(e) = agent_write.send(TungsteniteMessage::Ping(Vec::new())).await {
                    return Err(format!("Keepalive ping to agent failed: {e}"));
                }
            }
        }
    }
    Ok(())
}

/// Forward messages from agent to client (transparent, no side effects),
/// originating a periodic Ping toward the client to keep the path warm.
async fn forward_agent_to_client(
    mut agent_read: SplitStream<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    mut client_write: SplitSink<WebSocket, Message>,
    keepalive: Option<std::time::Duration>,
    session_id: &str,
) -> Result<(), String> {
    let mut ping = keepalive_interval(keepalive);
    loop {
        tokio::select! {
            msg_result = agent_read.next() => {
                let Some(msg_result) = msg_result else { break };
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
            () = next_keepalive_tick(&mut ping) => {
                if let Err(e) = client_write.send(Message::Ping(Vec::new())).await {
                    return Err(format!("Keepalive ping to client failed: {e}"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::gateway_error_payload;

    #[test]
    fn gateway_error_payload_matches_harness_error_shape() {
        let payload = gateway_error_payload("agent_stream_unavailable", "no stream");

        assert_eq!(payload["type"], "error");
        assert_eq!(payload["code"], "agent_stream_unavailable");
        assert_eq!(payload["message"], "no stream");
        assert_eq!(payload["recoverable"], true);
    }
}
