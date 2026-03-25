//! Agent management endpoints.
//!
//! This module provides handlers for agent CRUD operations and lifecycle management.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::{Agent, AgentSpec, AgentState, ControlPlane, CreateAgentRequest};
use aura_swarm_core::AgentId;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

// =============================================================================
// Request/Response Types
// =============================================================================

/// Response for a single agent.
#[derive(Debug, Serialize)]
pub(crate) struct AgentResponse {
    /// Agent ID.
    pub(crate) agent_id: String,
    /// Human-readable name.
    pub(crate) name: String,
    /// Current status.
    pub(crate) status: AgentState,
    /// Resource specification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) spec: Option<AgentSpec>,
    /// Creation timestamp.
    pub(crate) created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub(crate) updated_at: DateTime<Utc>,
    /// Last heartbeat timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_heartbeat_at: Option<DateTime<Utc>>,
    /// Error message if agent failed (e.g., provisioning error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error_message: Option<String>,
}

impl From<Agent> for AgentResponse {
    fn from(agent: Agent) -> Self {
        Self {
            agent_id: agent.agent_id.to_string(),
            name: agent.name,
            status: agent.status,
            spec: Some(agent.spec),
            created_at: agent.created_at,
            updated_at: agent.updated_at,
            last_heartbeat_at: agent.last_heartbeat_at,
            error_message: agent.error_message,
        }
    }
}

/// Response for agent list.
#[derive(Debug, Serialize)]
pub(crate) struct ListAgentsResponse {
    /// List of agents.
    pub(crate) agents: Vec<AgentResponse>,
}

/// Request to create an agent.
#[derive(Debug, Deserialize)]
pub(crate) struct CreateAgentBody {
    /// Human-readable name for the agent.
    pub(crate) name: String,
    /// Optional resource specification.
    #[serde(default)]
    pub(crate) spec: Option<AgentSpec>,
    /// Optional caller-supplied agent ID (e.g. from aura-network).
    /// If omitted, one is generated automatically.
    #[serde(default)]
    pub(crate) agent_id: Option<String>,
}

/// Response for lifecycle operations (start, stop, etc.).
#[derive(Debug, Serialize)]
pub(crate) struct LifecycleResponse {
    /// Agent ID.
    pub(crate) agent_id: String,
    /// New status after the operation.
    pub(crate) status: AgentState,
}

/// Query parameters for log retrieval.
#[derive(Debug, Deserialize)]
pub(crate) struct LogQuery {
    /// Number of lines to retrieve (default: 100).
    #[serde(default = "default_tail")]
    pub(crate) tail: u32,
    /// Retrieve logs since this timestamp.
    #[serde(default)]
    pub(crate) since: Option<String>,
}

const fn default_tail() -> u32 {
    100
}

/// Response for agent logs.
#[derive(Debug, Serialize)]
pub(crate) struct LogsResponse {
    /// Log entries.
    pub(crate) logs: Vec<LogEntry>,
}

/// A single log entry.
#[derive(Debug, Serialize)]
pub(crate) struct LogEntry {
    /// Timestamp of the log.
    pub(crate) timestamp: DateTime<Utc>,
    /// Log level.
    pub(crate) level: String,
    /// Log message.
    pub(crate) message: String,
}

/// Response for agent status.
#[derive(Debug, Serialize)]
pub(crate) struct StatusResponse {
    /// Current agent status.
    pub(crate) status: AgentState,
    /// Uptime in seconds.
    pub(crate) uptime_seconds: u64,
    /// Number of active sessions.
    pub(crate) active_sessions: u32,
    /// Last heartbeat timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_heartbeat_at: Option<DateTime<Utc>>,
    /// Resource usage.
    pub(crate) resource_usage: ResourceUsage,
}

/// Resource usage metrics.
#[derive(Debug, Serialize)]
pub(crate) struct ResourceUsage {
    /// CPU usage percentage (0-100).
    pub(crate) cpu_percent: f64,
    /// Memory usage in megabytes.
    pub(crate) memory_mb: u64,
}

// =============================================================================
// Handlers
// =============================================================================

/// List all agents for the authenticated user.
///
/// # Errors
///
/// Returns an error if the control plane operation fails.
pub(crate) async fn list_agents<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agents = state.control.list_agents(&user.user_id).await?;

    let response = ListAgentsResponse {
        agents: agents.into_iter().map(AgentResponse::from).collect(),
    };

    Ok(Json(response))
}

/// Create a new agent.
///
/// # Errors
///
/// Returns an error if:
/// - The agent name is invalid
/// - The user has reached their quota
/// - The control plane operation fails
pub(crate) async fn create_agent<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Json(body): Json<CreateAgentBody>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    // Validate name
    if body.name.is_empty() || body.name.len() > 64 {
        return Err(ApiError::BadRequest(
            "name must be 1-64 characters".to_string(),
        ));
    }

    // Check for valid characters (alphanumeric + hyphens)
    if !body
        .name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::BadRequest(
            "name must contain only alphanumeric characters, hyphens, or underscores".to_string(),
        ));
    }

    let mut request = if let Some(spec) = body.spec {
        CreateAgentRequest::with_spec(body.name, spec)
    } else {
        CreateAgentRequest::new(body.name)
    };

    if let Some(id_str) = body.agent_id {
        let id = parse_agent_id(&id_str)?;
        request = request.with_agent_id(id);
    }

    let (agent, created) = state.control.create_agent(&user.user_id, request).await?;

    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };

    Ok((status, Json(AgentResponse::from(agent))))
}

/// Get a single agent by ID.
///
/// # Errors
///
/// Returns an error if the agent is not found or the user doesn't own it.
pub(crate) async fn get_agent<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let agent = state.control.get_agent(&user.user_id, &agent_id).await?;

    Ok(Json(AgentResponse::from(agent)))
}

/// Delete an agent.
///
/// # Errors
///
/// Returns an error if the agent is not found, the user doesn't own it,
/// or the agent is not in a stopped state.
pub(crate) async fn delete_agent<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    state.control.delete_agent(&user.user_id, &agent_id).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Start an agent.
///
/// # Errors
///
/// Returns an error if the agent is not found, the user doesn't own it,
/// or the state transition is invalid.
pub(crate) async fn start_agent<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let agent = state.control.start_agent(&user.user_id, &agent_id).await?;

    Ok(Json(LifecycleResponse {
        agent_id: agent.agent_id.to_string(),
        status: agent.status,
    }))
}

/// Stop an agent.
///
/// # Errors
///
/// Returns an error if the agent is not found, the user doesn't own it,
/// or the state transition is invalid.
pub(crate) async fn stop_agent<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let agent = state.control.stop_agent(&user.user_id, &agent_id).await?;

    Ok(Json(LifecycleResponse {
        agent_id: agent.agent_id.to_string(),
        status: agent.status,
    }))
}

/// Restart an agent.
///
/// # Errors
///
/// Returns an error if the agent is not found, the user doesn't own it,
/// or the state transition is invalid.
pub(crate) async fn restart_agent<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let agent = state
        .control
        .restart_agent(&user.user_id, &agent_id)
        .await?;

    Ok(Json(LifecycleResponse {
        agent_id: agent.agent_id.to_string(),
        status: agent.status,
    }))
}

/// Hibernate an agent.
///
/// # Errors
///
/// Returns an error if the agent is not found, the user doesn't own it,
/// or the state transition is invalid.
pub(crate) async fn hibernate_agent<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let agent = state
        .control
        .hibernate_agent(&user.user_id, &agent_id)
        .await?;

    Ok(Json(LifecycleResponse {
        agent_id: agent.agent_id.to_string(),
        status: agent.status,
    }))
}

/// Wake a hibernating agent.
///
/// # Errors
///
/// Returns an error if the agent is not found, the user doesn't own it,
/// or the state transition is invalid.
pub(crate) async fn wake_agent<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let agent = state.control.wake_agent(&user.user_id, &agent_id).await?;

    Ok(Json(LifecycleResponse {
        agent_id: agent.agent_id.to_string(),
        status: agent.status,
    }))
}

/// Get agent logs.
///
/// Note: This is a placeholder implementation. Real logs would come from
/// the Kubernetes pod or a log aggregation service.
///
/// # Errors
///
/// Returns an error if the agent ID is invalid.
pub(crate) async fn get_logs<C, V>(
    State(_state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<LogQuery>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    // Validate agent ID format
    parse_agent_id(&agent_id)?;

    // Placeholder: In a real implementation, this would fetch logs from K8s or a log service
    tracing::debug!(
        agent_id = %agent_id,
        user_id = %user.user_id,
        tail = query.tail,
        since = ?query.since,
        "Fetching agent logs"
    );

    Ok(Json(LogsResponse { logs: vec![] }))
}

/// Get agent status.
///
/// Note: This is a placeholder implementation. Real status would come from
/// the control plane with metrics from the scheduler.
///
/// # Errors
///
/// Returns an error if the agent is not found or the user doesn't own it.
#[allow(clippy::cast_sign_loss)]
pub(crate) async fn get_status<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let agent = state.control.get_agent(&user.user_id, &agent_id).await?;

    // Calculate uptime (placeholder - would come from pod start time)
    // Safe: max(0) ensures non-negative
    let uptime_seconds = (Utc::now() - agent.created_at).num_seconds().max(0) as u64;

    Ok(Json(StatusResponse {
        status: agent.status,
        uptime_seconds,
        active_sessions: 0, // Placeholder
        last_heartbeat_at: agent.last_heartbeat_at,
        resource_usage: ResourceUsage {
            cpu_percent: 0.0,
            memory_mb: 0,
        },
    }))
}

/// Response for the remote agent state endpoint.
///
/// Returns lifecycle state only — no resource metrics or logs.
/// Designed for consumption by external clients (e.g. aura-os-link `SwarmClient`).
#[derive(Debug, Serialize)]
pub(crate) struct AgentStateResponse {
    /// Current lifecycle state.
    pub(crate) state: AgentState,
    /// Uptime in seconds (0 if not running).
    pub(crate) uptime_seconds: u64,
    /// Number of active sessions.
    pub(crate) active_sessions: u32,
    /// Last heartbeat from the agent runtime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_heartbeat_at: Option<DateTime<Utc>>,
    /// Error message if the agent is in an error state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error_message: Option<String>,
}

/// Get remote agent state (lifecycle only).
///
/// This endpoint returns a minimal state payload suitable for external
/// consumers that need to know whether the backing VM is ready, without
/// full resource metrics or logs.
///
/// # Errors
///
/// Returns an error if the agent is not found or the user doesn't own it.
#[allow(clippy::cast_sign_loss)]
pub(crate) async fn get_agent_state<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let agent = state.control.get_agent(&user.user_id, &agent_id).await?;

    let uptime_seconds = if matches!(agent.status, AgentState::Running | AgentState::Idle) {
        (Utc::now() - agent.created_at).num_seconds().max(0) as u64
    } else {
        0
    };

    Ok(Json(AgentStateResponse {
        state: agent.status,
        uptime_seconds,
        active_sessions: 0,
        last_heartbeat_at: agent.last_heartbeat_at,
        error_message: agent.error_message,
    }))
}

// =============================================================================
// Helpers
// =============================================================================

/// Parse an agent ID from a string.
fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}
