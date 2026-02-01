//! Internal API endpoints.
//!
//! These endpoints are used for service-to-service communication within the cluster.
//! They are NOT exposed externally and don't require JWT authentication.
//!
//! # Security
//!
//! Internal endpoints should be protected by network policies that only allow
//! traffic from within the swarm-system namespace.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::{AgentState, ControlPlane};
use aura_swarm_core::AgentId;

use crate::state::GatewayState;

// =============================================================================
// Request/Response Types
// =============================================================================

/// Request body for updating agent status.
#[derive(Debug, Deserialize)]
pub struct StatusUpdateRequest {
    /// The new agent state.
    pub status: AgentState,
    /// Optional message describing the status change.
    #[serde(default)]
    pub message: Option<String>,
}

/// Response for status update.
#[derive(Debug, Serialize)]
pub struct StatusUpdateResponse {
    /// Whether the update was successful.
    pub success: bool,
    /// The agent's new status.
    pub status: AgentState,
}

/// Error response for internal endpoints.
#[derive(Debug, Serialize)]
pub struct InternalErrorResponse {
    /// Error message.
    pub error: String,
    /// Error code.
    pub code: u16,
}

// =============================================================================
// Handlers
// =============================================================================

/// Update an agent's status from the scheduler.
///
/// This endpoint is called by the scheduler when pod status changes are detected.
/// It allows the scheduler to notify the gateway of state transitions like:
/// - Pod becoming ready (Provisioning -> Running)
/// - Pod failing (any -> Error)
/// - Pod being deleted (any -> Stopped)
///
/// # Security
///
/// This endpoint does NOT require JWT authentication. It should only be
/// accessible from within the cluster via network policies.
///
/// # Errors
///
/// Returns an error if the agent ID is invalid or the store update fails.
pub async fn update_agent_status<C, V>(
    State(state): State<Arc<GatewayState<C, V>>>,
    Path(agent_id): Path<String>,
    Json(body): Json<StatusUpdateRequest>,
) -> impl IntoResponse
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    // Parse agent ID
    let agent_id = match AgentId::from_hex(&agent_id) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(Err::<StatusUpdateResponse, InternalErrorResponse>(
                    InternalErrorResponse {
                        error: format!("Invalid agent ID: {e}"),
                        code: 400,
                    },
                )),
            )
                .into_response();
        }
    };

    tracing::info!(
        agent_id = %agent_id,
        new_status = ?body.status,
        message = ?body.message,
        "Received status update from scheduler"
    );

    // Update the agent status via the control plane
    match state
        .control
        .update_agent_status_internal(&agent_id, body.status)
        .await
    {
        Ok(()) => {
            tracing::info!(
                agent_id = %agent_id,
                status = ?body.status,
                "Updated agent status from scheduler"
            );
            Json(Ok::<StatusUpdateResponse, InternalErrorResponse>(
                StatusUpdateResponse {
                    success: true,
                    status: body.status,
                },
            ))
            .into_response()
        }
        Err(e) => {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to update agent status"
            );
            let code = e.http_status_code();
            (
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(Err::<StatusUpdateResponse, InternalErrorResponse>(
                    InternalErrorResponse {
                        error: format!("Failed to update status: {e}"),
                        code,
                    },
                )),
            )
                .into_response()
        }
    }
}

/// Health check for internal services.
///
/// This is a simple endpoint that schedulers can use to verify connectivity.
pub async fn internal_health() -> impl IntoResponse {
    #[derive(Serialize)]
    struct InternalHealthResponse {
        status: &'static str,
    }

    Json(InternalHealthResponse { status: "ok" })
}
