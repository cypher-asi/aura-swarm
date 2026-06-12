//! Usage / cost stats endpoints (Swarm TEE upgrade phase 11).
//!
//! - `GET /v1/agents/:agent_id/usage?from&to` — one agent's billable
//!   intervals, totals, counters, and recent raw events (owner only).
//! - `GET /v1/usage?from&to` — per-agent summaries plus a grand total for
//!   every agent the authenticated user owns (scoped by the JWT subject).
//!
//! `from` / `to` are RFC3339 timestamps; the range defaults to the last
//! 30 days. `to` is clamped to "now" because open intervals are closed at
//! the range end and time that has not elapsed must never be billed.
//! zbilling remains the billing source of truth — these are user-facing
//! stats.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::usage::UsageAggregation;
use aura_swarm_control::{AgentState, ControlPlane, UsageEvent};
use aura_swarm_core::AgentId;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::state::GatewayState;

/// Query parameters for the usage endpoints.
#[derive(Debug, Deserialize)]
pub(crate) struct UsageQuery {
    /// Range start (RFC3339). Defaults to 30 days before `to`.
    #[serde(default)]
    pub(crate) from: Option<String>,
    /// Range end (RFC3339). Defaults to now; clamped to now.
    #[serde(default)]
    pub(crate) to: Option<String>,
}

/// Response for `GET /v1/agents/:agent_id/usage`.
#[derive(Debug, Serialize)]
pub(crate) struct AgentUsageResponse {
    /// Agent ID.
    pub(crate) agent_id: String,
    /// Aggregated usage (range, intervals, totals, counters).
    #[serde(flatten)]
    pub(crate) usage: UsageAggregation,
    /// Most recent raw events within the range, oldest first (capped).
    pub(crate) events: Vec<UsageEvent>,
}

/// Per-agent summary row in `GET /v1/usage`.
#[derive(Debug, Serialize)]
pub(crate) struct UserAgentUsage {
    /// Agent ID.
    pub(crate) agent_id: String,
    /// Human-readable name.
    pub(crate) name: String,
    /// Current lifecycle state.
    pub(crate) status: AgentState,
    /// Current box tier.
    pub(crate) tier: String,
    /// Seconds the agent had a pod within the range.
    pub(crate) awake_seconds: u64,
    /// Estimated cost in cents within the range.
    pub(crate) cost_cents: u64,
    /// `Woke` events within the range.
    pub(crate) wakes: u32,
    /// `TriggerFired` events within the range.
    pub(crate) triggers_fired: u32,
    /// `TierChanged` events within the range.
    pub(crate) tier_changes: u32,
}

/// Response for `GET /v1/usage` (the authenticated user's agents only;
/// destroyed agents are not included).
#[derive(Debug, Serialize)]
pub(crate) struct UserUsageResponse {
    /// Range start (inclusive).
    pub(crate) from: DateTime<Utc>,
    /// Range end (exclusive).
    pub(crate) to: DateTime<Utc>,
    /// Per-agent summaries.
    pub(crate) agents: Vec<UserAgentUsage>,
    /// Grand total awake seconds across all agents.
    pub(crate) total_awake_seconds: u64,
    /// Grand total estimated cost in cents across all agents.
    pub(crate) total_cost_cents: u64,
}

/// Get usage / cost stats for one agent (`GET /v1/agents/:agent_id/usage`).
///
/// # Errors
///
/// Returns 400 for an invalid agent ID or time range, 404/403 per the
/// usual ownership rules.
pub(crate) async fn get_agent_usage<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Path(agent_id): Path<String>,
    Query(query): Query<UsageQuery>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let agent_id = parse_agent_id(&agent_id)?;
    let (from, to) = resolve_range(&query)?;

    let usage = state
        .control
        .get_agent_usage(&user.user_id, &agent_id, from, to)
        .await?;

    Ok(Json(AgentUsageResponse {
        agent_id: agent_id.to_string(),
        usage: usage.aggregation,
        events: usage.recent_events,
    }))
}

/// Get usage / cost stats for every agent the authenticated user owns
/// (`GET /v1/usage`). The user is taken from the JWT subject — there is
/// no way to query another user's usage.
///
/// # Errors
///
/// Returns 400 for an invalid time range.
pub(crate) async fn get_user_usage<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    user: AuthUser,
    Query(query): Query<UsageQuery>,
) -> Result<impl IntoResponse, ApiError>
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    let (from, to) = resolve_range(&query)?;

    let reports = state
        .control
        .get_user_usage(&user.user_id, from, to)
        .await?;

    let mut total_awake_seconds = 0u64;
    let mut total_cost_cents = 0u64;
    let agents = reports
        .into_iter()
        .map(|(agent, agg)| {
            total_awake_seconds += agg.awake_seconds;
            total_cost_cents += agg.cost_cents;
            UserAgentUsage {
                agent_id: agent.agent_id.to_string(),
                name: agent.name,
                status: agent.status,
                tier: agent.spec.tier,
                awake_seconds: agg.awake_seconds,
                cost_cents: agg.cost_cents,
                wakes: agg.wakes,
                triggers_fired: agg.triggers_fired,
                tier_changes: agg.tier_changes,
            }
        })
        .collect();

    Ok(Json(UserUsageResponse {
        from,
        to,
        agents,
        total_awake_seconds,
        total_cost_cents,
    }))
}

/// Resolve the query range: parse RFC3339 bounds, apply the last-30-days
/// default, and clamp `to` to now (open intervals close at the range end,
/// so a future `to` would bill time that has not elapsed).
pub(crate) fn resolve_range(
    query: &UsageQuery,
) -> Result<(DateTime<Utc>, DateTime<Utc>), ApiError> {
    let now = Utc::now();
    let to = match &query.to {
        Some(s) => parse_rfc3339(s, "to")?.min(now),
        None => now,
    };
    let from = match &query.from {
        Some(s) => parse_rfc3339(s, "from")?,
        None => to - Duration::days(30),
    };
    if from >= to {
        return Err(ApiError::BadRequest(
            "`from` must be earlier than `to`".to_string(),
        ));
    }
    Ok((from, to))
}

fn parse_rfc3339(s: &str, field: &str) -> Result<DateTime<Utc>, ApiError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| {
            ApiError::BadRequest(format!(
                "invalid `{field}`: expected an RFC3339 timestamp"
            ))
        })
}

/// Parse an agent ID from a string.
fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    AgentId::from_hex(s).map_err(|_| ApiError::BadRequest(format!("invalid agent ID: {s}")))
}
