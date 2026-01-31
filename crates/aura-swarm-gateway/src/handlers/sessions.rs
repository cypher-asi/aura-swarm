//! Session management endpoints.
//!
//! This module provides handlers for session creation and retrieval.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::{ControlPlane, Session, SessionStatus};
use aura_swarm_core::{AgentId, SessionId};

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

// =============================================================================
// Response Types
// =============================================================================

/// Response for a created session.
#[derive(Debug, Serialize)]
pub struct CreateSessionResponse {
    /// Session ID.
    pub session_id: String,
    /// Agent ID.
    pub agent_id: String,
    /// WebSocket URL for connecting to this session.
    pub ws_url: String,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Response for a session.
#[derive(Debug, Serialize)]
pub struct SessionResponse {
    /// Session ID.
    pub session_id: String,
    /// Agent ID.
    pub agent_id: String,
    /// Current status.
    pub status: SessionStatus,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// When the session was closed (if closed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
}

impl From<Session> for SessionResponse {
    fn from(session: Session) -> Self {
        Self {
            session_id: session.session_id.to_string(),
            agent_id: session.agent_id.to_string(),
            status: session.status,
            created_at: session.created_at,
            closed_at: session.closed_at,
        }
    }
}

/// Response for listing sessions.
#[derive(Debug, Serialize)]
pub struct ListSessionsResponse {
    /// List of sessions.
    pub sessions: Vec<SessionResponse>,
}

// =============================================================================
// Handlers
// =============================================================================

/// Create a new session for an agent.
///
/// If the agent is hibernating, it will be automatically woken up.
///
/// # Errors
///
/// Returns an error if the agent is not found, the user doesn't own it,
/// or the agent is not in a runnable state.
pub async fn create_session<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;

    let session = state
        .control
        .create_session(&user.user_id, &agent_id)
        .await?;

    let response = CreateSessionResponse {
        session_id: session.session_id.to_string(),
        agent_id: session.agent_id.to_string(),
        ws_url: format!("/v1/sessions/{}/ws", session.session_id),
        created_at: session.created_at,
    };

    Ok((StatusCode::CREATED, Json(response)))
}

/// Get a session by ID.
///
/// # Errors
///
/// Returns an error if the session is not found or the user doesn't own it.
pub async fn get_session<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(session_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let session_id = parse_session_id(&session_id)?;

    let session = state
        .control
        .get_session(&user.user_id, &session_id)
        .await?;

    Ok(Json(SessionResponse::from(session)))
}

/// List all sessions for an agent.
///
/// # Errors
///
/// Returns an error if the agent is not found or the user doesn't own it.
pub async fn list_sessions<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;

    let sessions = state
        .control
        .list_sessions(&user.user_id, &agent_id)
        .await?;

    let response = ListSessionsResponse {
        sessions: sessions.into_iter().map(SessionResponse::from).collect(),
    };

    Ok(Json(response))
}

/// Close a session.
///
/// # Errors
///
/// Returns an error if the session is not found or the user doesn't own it.
pub async fn close_session<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(session_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let session_id = parse_session_id(&session_id)?;

    state
        .control
        .close_session(&user.user_id, &session_id)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

// =============================================================================
// Helpers
// =============================================================================

/// Parse an agent ID from a string.
fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}

/// Parse a session ID from a string.
fn parse_session_id(s: &str) -> Result<SessionId, ApiError> {
    s.parse()
        .map_err(|_| ApiError::BadRequest(format!("invalid session ID: {s}")))
}
