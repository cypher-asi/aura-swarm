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
    /// Resolved box tier ("small" / "standard" / "pro").
    /// Absent for legacy agents created before tiers existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tier: Option<String>,
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
            tier: agent.spec.tier.clone(),
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
    /// Optional box tier ("small" / "standard" / "pro"). Defaults to
    /// "standard". The legacy `spec` field is still accepted; its resources
    /// are mapped to the nearest tier when no tier is given.
    #[serde(default)]
    pub(crate) tier: Option<String>,
    /// Legacy raw resource specification (mapped to the nearest tier).
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

/// Request body for a tier change.
#[derive(Debug, Deserialize)]
pub(crate) struct ChangeTierBody {
    /// Target box tier ("small" / "standard" / "pro").
    pub(crate) tier: String,
}

/// Response for a tier change.
#[derive(Debug, Serialize)]
pub(crate) struct ChangeTierResponse {
    /// Agent ID.
    pub(crate) agent_id: String,
    /// Tier before the change (`null` = the agent was a legacy agent that
    /// has just been converted to the new architecture).
    pub(crate) previous_tier: Option<String>,
    /// Tier the agent is on now.
    pub(crate) tier: String,
    /// Whether anything changed (`false` for a same-tier no-op).
    pub(crate) changed: bool,
    /// Whether the running pod was recreated to apply the new size
    /// (`false` for asleep agents: takes effect on next wake/start).
    pub(crate) pod_recreated: bool,
    /// Agent status after the operation.
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

/// Response for agent status, with real metrics derived from the agent's
/// usage-event log plus current state (Swarm TEE upgrade phase 11).
#[derive(Debug, Serialize)]
pub(crate) struct StatusResponse {
    /// Current agent status.
    pub(crate) status: AgentState,
    /// Seconds since the current pod was scheduled (0 when no pod).
    pub(crate) uptime_seconds: u64,
    /// Number of active sessions.
    pub(crate) active_sessions: u32,
    /// Last heartbeat timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_heartbeat_at: Option<DateTime<Utc>>,
    /// Current box tier; absent for legacy agents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tier: Option<String>,
    /// Seconds the agent had a pod in the last 24 hours.
    pub(crate) awake_seconds_24h: u64,
    /// Estimated cost in cents for the last 24 hours (priced at the rates
    /// recorded on the usage events; zbilling stays the billing source of
    /// truth).
    pub(crate) estimated_cost_cents_24h: u64,
    /// When the agent was last woken from hibernation/stop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_wake_at: Option<DateTime<Utc>>,
    /// Wakes in the last 24 hours.
    pub(crate) wakes_24h: u32,
    /// Process triggers fired into the agent in the last 24 hours.
    pub(crate) triggers_fired_24h: u32,
    /// Resource usage.
    pub(crate) resource_usage: ResourceUsage,
}

/// Resource usage metrics.
#[derive(Debug, Serialize)]
pub(crate) struct ResourceUsage {
    /// CPU usage percentage (0-100). Always 0 today: there is no live
    /// metrics source for in-VM CPU yet.
    pub(crate) cpu_percent: f64,
    /// Allocated memory in megabytes while a pod is active (0 otherwise).
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

    if let Some(tier) = body.tier {
        request = request.with_tier(tier);
    }

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

/// Change an agent's box tier (`POST /v1/agents/:id/tier`).
///
/// Same-tier requests are a no-op (200 with the current state). Asleep
/// agents are record-only updates; awake agents get a credit-checked
/// recreate-with-state. Calling this on a legacy agent converts it to the
/// new architecture (per-agent early migration).
///
/// # Errors
///
/// Returns an error if the agent is not found, the user doesn't own it,
/// the tier name is unknown (400), the agent is mid-transition (409), or
/// the user can't afford the new tier's rate (402).
pub(crate) async fn change_tier<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
    Json(body): Json<ChangeTierBody>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let outcome = state
        .control
        .change_tier(&user.user_id, &agent_id, &body.tier)
        .await?;

    Ok(Json(ChangeTierResponse {
        agent_id: outcome.agent.agent_id.to_string(),
        previous_tier: outcome.previous_tier,
        tier: outcome.tier,
        changed: outcome.changed,
        pod_recreated: outcome.pod_recreated,
        status: outcome.agent.status,
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

/// Get agent status with real metrics derived from the agent's
/// usage-event log (last 24 hours) plus its current state.
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

    let now = Utc::now();
    let usage = state
        .control
        .get_agent_usage(&user.user_id, &agent_id, now - chrono::Duration::hours(24), now)
        .await?;
    let agg = usage.aggregation;

    let pod_active = matches!(
        agent.status,
        AgentState::Provisioning | AgentState::Running | AgentState::Idle
    );

    // Uptime is the age of the currently open billable interval. Agents
    // created before lifecycle events existed can be awake without an
    // open interval; fall back to the pre-phase-11 created_at heuristic.
    // Safe: max(0) ensures non-negative.
    let uptime_seconds = if pod_active {
        let started = agg.open_interval_started_at.unwrap_or(agent.created_at);
        (now - started).num_seconds().max(0) as u64
    } else {
        0
    };

    let active_sessions = state
        .control
        .count_active_sessions(&agent_id)
        .await
        .unwrap_or(0);

    Ok(Json(StatusResponse {
        status: agent.status,
        uptime_seconds,
        active_sessions,
        last_heartbeat_at: agent.last_heartbeat_at,
        tier: agent.spec.tier.clone(),
        awake_seconds_24h: agg.awake_seconds,
        estimated_cost_cents_24h: agg.cost_cents,
        last_wake_at: agg.last_wake_at,
        wakes_24h: agg.wakes,
        triggers_fired_24h: agg.triggers_fired,
        resource_usage: ResourceUsage {
            cpu_percent: 0.0,
            memory_mb: if pod_active {
                u64::from(agent.spec.memory_mb)
            } else {
                0
            },
        },
    }))
}

/// Response for the remote agent state endpoint.
///
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
    /// Agent ID.
    pub(crate) agent_id: String,
    /// Human-readable name.
    pub(crate) name: String,
    /// CPU allocation in millicores.
    pub(crate) cpu_millicores: u32,
    /// Memory allocation in megabytes.
    pub(crate) memory_mb: u32,
    /// Runtime version.
    pub(crate) runtime_version: String,
    /// Git commit of the harness build running in the pod, as reported by
    /// the harness `/health` endpoint. Absent if the pod is unreachable or
    /// runs an older image without baked-in build metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) harness_git_sha: Option<String>,
    /// Isolation level ("container" or "micro_vm").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) isolation: Option<String>,
    /// Pod network endpoint (IP:port) if running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) endpoint: Option<String>,
    /// When the agent was created.
    pub(crate) created_at: DateTime<Utc>,
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

    let isolation = agent
        .spec
        .isolation
        .map(|i| format!("{i:?}").to_lowercase());
    let endpoint = state
        .control
        .resolve_agent_endpoint(&agent.agent_id)
        .await
        .ok()
        .flatten();
    let active_sessions = state
        .control
        .count_active_sessions(&agent.agent_id)
        .await
        .unwrap_or(0);
    let harness_git_sha = match endpoint.as_deref() {
        Some(ep) => harness_git_sha_for_endpoint(&state.harness_info_cache, ep).await,
        None => None,
    };

    Ok(Json(AgentStateResponse {
        state: agent.status,
        uptime_seconds,
        active_sessions,
        last_heartbeat_at: agent.last_heartbeat_at,
        error_message: agent.error_message,
        agent_id: agent.agent_id.to_string(),
        name: agent.name,
        cpu_millicores: agent.spec.cpu_millicores,
        memory_mb: agent.spec.memory_mb,
        runtime_version: agent.spec.runtime_version,
        harness_git_sha,
        isolation,
        endpoint,
        created_at: agent.created_at,
    }))
}

/// Resolve the harness git SHA for a pod endpoint, using the per-endpoint
/// cache to avoid probing the pod on every state poll.
///
/// Reachable pods are cached (including a `None` answer from older harness
/// images that lack build metadata). Network failures are *not* cached so a
/// pod that is still starting up gets retried on the next poll.
async fn harness_git_sha_for_endpoint(
    cache: &crate::state::HarnessInfoCache,
    endpoint: &str,
) -> Option<String> {
    if let Some(cached) = cache
        .lock()
        .expect("harness info cache lock poisoned")
        .get(endpoint)
    {
        return cached.clone();
    }

    match probe_harness_git_sha(endpoint).await {
        Ok(sha) => {
            cache
                .lock()
                .expect("harness info cache lock poisoned")
                .insert(endpoint.to_string(), sha.clone());
            sha
        }
        Err(()) => None,
    }
}

/// Fetch `git_sha` from the harness `/health` endpoint on the pod.
///
/// Returns `Ok(None)` when the pod responded but reported no SHA, and
/// `Err(())` when the pod could not be reached or returned garbage.
async fn probe_harness_git_sha(endpoint: &str) -> Result<Option<String>, ()> {
    let url = format!("http://{endpoint}/health");
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ())?
        .get(&url)
        .timeout(std::time::Duration::from_millis(1500))
        .send()
        .await
        .map_err(|e| {
            tracing::debug!(url = %url, error = %e, "harness health probe failed");
        })?;
    if !resp.status().is_success() {
        return Err(());
    }
    let body: serde_json::Value = resp.json().await.map_err(|_| ())?;
    Ok(body
        .get("git_sha")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned))
}

// =============================================================================
// Helpers
// =============================================================================

/// Parse an agent ID from a string.
fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}
