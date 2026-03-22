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
use z_billing_client::LlmUsageEvent;

use serde::Serialize;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::{ControlPlane, SessionStatus};
use aura_swarm_core::SessionId;
use aura_swarm_store::{EngineType, SessionConfig};

use crate::auth::AuthUser;
use crate::billing::{make_event_id, try_extract_usage, BillingService};
use crate::error::ApiError;
use crate::state::GatewayState;

/// Context for WebSocket billing integration.
#[derive(Clone)]
struct WsBillingContext {
    billing: Option<Arc<BillingService>>,
    identity_id: String,
    agent_id: String,
    session_id: String,
}

/// `session_init` message sent to the harness runtime.
#[derive(Debug, Serialize)]
struct HarnessSessionInit {
    #[serde(rename = "type")]
    msg_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

impl HarnessSessionInit {
    fn from_config(config: &SessionConfig) -> Self {
        Self {
            msg_type: "session_init",
            system_prompt: config.system_prompt.clone(),
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            max_turns: config.max_turns,
            workspace: config.workspace.as_ref().and_then(|w| w.git_repo_url.clone()),
            token: None,
        }
    }
}

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
        .get_session(&user.identity_id, &session_id)
        .await?;

    // Check session is active
    if session.status != SessionStatus::Active {
        return Err(ApiError::Conflict("session is not active".to_string()));
    }

    // Get agent details to check engine_type
    let agent = state
        .control
        .get_agent(&user.identity_id, &session.agent_id)
        .await?;
    let engine_type = agent.spec.engine_type;
    let session_config = session.config.clone();

    // Get agent endpoint
    let endpoint = state
        .control
        .resolve_agent_endpoint(&session.agent_id)
        .await?
        .ok_or(ApiError::AgentUnavailable)?;

    let timeout = state.config.websocket_timeout();
    let agent_id_str = session.agent_id.to_string();
    let session_id_str = session_id.to_string();
    let identity_id_str = user.identity_id.to_string();

    let billing_ctx = WsBillingContext {
        billing: state.billing.clone(),
        identity_id: identity_id_str.clone(),
        agent_id: agent_id_str.clone(),
        session_id: session_id_str.clone(),
    };

    tracing::info!(
        session_id = %session_id_str,
        agent_id = %agent_id_str,
        identity_id = %identity_id_str,
        "WebSocket connection initiated"
    );

    Ok(ws.on_upgrade(move |socket| {
        handle_websocket(
            socket,
            endpoint,
            session_id_str,
            agent_id_str,
            timeout,
            billing_ctx,
            session_config,
            engine_type,
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
    billing_ctx: WsBillingContext,
    session_config: SessionConfig,
    engine_type: EngineType,
) {
    let agent_url = format!("ws://{agent_endpoint}/stream");
    let Some(mut agent_socket) =
        connect_to_agent(&agent_url, timeout, &session_id, &agent_id).await
    else {
        return;
    };

    tracing::info!(
        session_id = %session_id,
        agent_id = %agent_id,
        "Connected to agent, starting proxy"
    );

    // Send session_init to harness before entering proxy mode
    if engine_type == EngineType::Harness {
        if let Err(e) =
            initialize_harness_session(&mut agent_socket, &session_config, &session_id).await
        {
            tracing::error!(
                session_id = %session_id,
                error = %e,
                "Failed to initialize harness session"
            );
            return;
        }
    }

    // Split both sockets for bidirectional forwarding
    let (client_write, client_read) = client_socket.split();
    let (agent_write, agent_read) = agent_socket.split();

    // Run both directions concurrently
    let client_to_agent = forward_client_to_agent(client_read, agent_write, &session_id);
    let agent_to_client =
        forward_agent_to_client(agent_read, client_write, &session_id, billing_ctx);

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

/// Send `session_init` to a harness pod and wait for `session_ready`.
async fn initialize_harness_session(
    agent_socket: &mut tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    config: &SessionConfig,
    session_id: &str,
) -> Result<(), String> {
    let init_msg = HarnessSessionInit::from_config(config);
    let json = serde_json::to_string(&init_msg)
        .map_err(|e| format!("Failed to serialize session_init: {e}"))?;

    agent_socket
        .send(TungsteniteMessage::Text(json))
        .await
        .map_err(|e| format!("Failed to send session_init: {e}"))?;

    tracing::debug!(session_id = %session_id, "Sent session_init to harness");

    let timeout = tokio::time::Duration::from_secs(30);
    match tokio::time::timeout(timeout, agent_socket.next()).await {
        Ok(Some(Ok(TungsteniteMessage::Text(text)))) => {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                if value.get("type").and_then(|t| t.as_str()) == Some("session_ready") {
                    tracing::info!(
                        session_id = %session_id,
                        "Harness session initialized"
                    );
                    return Ok(());
                }
                if value.get("type").and_then(|t| t.as_str()) == Some("error") {
                    let msg = value
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown");
                    return Err(format!("Harness returned error: {msg}"));
                }
            }
            Err(format!("Unexpected response from harness: {text}"))
        }
        Ok(Some(Ok(_))) => Err("Unexpected non-text message from harness".to_string()),
        Ok(Some(Err(e))) => Err(format!("Error reading from harness: {e}")),
        Ok(None) => Err("Harness closed connection during init".to_string()),
        Err(_) => Err("Timeout waiting for session_ready from harness".to_string()),
    }
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

/// Forward messages from agent to client, extracting LLM usage for billing.
async fn forward_agent_to_client(
    mut agent_read: SplitStream<
        tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    mut client_write: SplitSink<WebSocket, Message>,
    session_id: &str,
    billing_ctx: WsBillingContext,
) -> Result<(), String> {
    while let Some(msg_result) = agent_read.next().await {
        match msg_result {
            Ok(msg) => {
                let axum_msg = convert_and_track_usage(&msg, session_id, &billing_ctx);

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

/// Convert a tungstenite message to axum and track LLM usage if applicable.
fn convert_and_track_usage(
    msg: &TungsteniteMessage,
    _session_id: &str,
    billing_ctx: &WsBillingContext,
) -> Option<Message> {
    match msg {
        TungsteniteMessage::Text(text) => {
            // Try to extract and report LLM usage in background
            maybe_report_usage(text, billing_ctx);
            Some(Message::Text(text.clone()))
        }
        TungsteniteMessage::Binary(data) => Some(Message::Binary(data.clone())),
        TungsteniteMessage::Ping(data) => Some(Message::Ping(data.clone())),
        TungsteniteMessage::Pong(data) => Some(Message::Pong(data.clone())),
        TungsteniteMessage::Close(_) | TungsteniteMessage::Frame(_) => None,
    }
}

/// Extract LLM usage from a text message and report to billing in background.
fn maybe_report_usage(text: &str, billing_ctx: &WsBillingContext) {
    let Some(billing) = billing_ctx.billing.as_ref() else {
        return;
    };

    let Some(usage) = try_extract_usage(text) else {
        return;
    };

    let event = LlmUsageEvent {
        event_id: make_event_id(&billing_ctx.session_id, &usage.message_id),
        user_id: billing_ctx.identity_id.clone(),
        agent_id: Some(billing_ctx.agent_id.clone()),
        provider: usage.provider,
        model: usage.model,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        metadata: None,
    };

    let billing = Arc::clone(billing);
    tokio::spawn(async move {
        if let Err(e) = billing.report_llm_usage(event).await {
            tracing::warn!(error = %e, "Failed to report LLM usage to billing");
        }
    });
}

/// Parse a session ID from a string.
fn parse_session_id(s: &str) -> Result<SessionId, ApiError> {
    s.parse()
        .map_err(|_| ApiError::BadRequest(format!("invalid session ID: {s}")))
}
