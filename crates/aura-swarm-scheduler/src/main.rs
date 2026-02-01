//! Aura Swarm Scheduler - Kubernetes Pod Scheduler Service
//!
//! This is the main entry point for the scheduler service.
//! It manages agent pods in Kubernetes and provides health endpoints.
//!
//! # HTTP Endpoints
//!
//! ## Health & Readiness
//! - `GET /health` - Health check
//! - `GET /ready` - Readiness check
//!
//! ## Agent Pod Management
//! - `POST /v1/agents/:agent_id/schedule` - Schedule (create) an agent pod
//! - `DELETE /v1/agents/:agent_id` - Terminate an agent pod
//! - `GET /v1/agents/:agent_id/status` - Get pod status

use std::sync::Arc;

use aura_swarm_core::AgentId;
use aura_swarm_scheduler::{K8sScheduler, Scheduler, SchedulerConfig, SchedulerError};
use aura_swarm_store::AgentSpec;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Application state shared across handlers.
struct AppState {
    scheduler: Arc<K8sScheduler>,
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            scheduler: Arc::clone(&self.scheduler),
        }
    }
}

// ============================================================================
// Health Endpoints
// ============================================================================

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

async fn health_handler() -> impl IntoResponse {
    Json(HealthResponse {
        status: "healthy",
        service: "aura-swarm-scheduler",
    })
}

async fn ready_handler(State(_state): State<AppState>) -> impl IntoResponse {
    // Could add K8s connectivity check here
    (StatusCode::OK, "ready")
}

// ============================================================================
// Agent Pod Management Endpoints
// ============================================================================

/// Request body for scheduling an agent pod.
#[derive(Debug, Deserialize)]
struct ScheduleRequest {
    /// The user ID (hex-encoded) that owns this agent.
    user_id: String,
    /// Resource specification for the agent.
    spec: AgentSpec,
}

/// Error response format.
#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
    code: u16,
}

impl ErrorResponse {
    fn new(error: impl Into<String>, code: u16) -> Self {
        Self {
            error: error.into(),
            code,
        }
    }
}

/// Schedule (create) an agent pod.
///
/// POST /v1/agents/:agent_id/schedule
async fn schedule_handler(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<ScheduleRequest>,
) -> impl IntoResponse {
    let agent_id = match AgentId::from_hex(&agent_id) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(format!("Invalid agent ID: {e}"), 400)),
            )
                .into_response();
        }
    };

    match state
        .scheduler
        .schedule_agent(&agent_id, &req.user_id, &req.spec)
        .await
    {
        Ok(()) => {
            tracing::info!(
                agent_id = %agent_id,
                user_id = %req.user_id,
                "Scheduled agent pod via HTTP API"
            );
            StatusCode::ACCEPTED.into_response()
        }
        Err(e) => {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to schedule agent pod"
            );
            let code = e.http_status_code();
            (
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(ErrorResponse::new(e.to_string(), code)),
            )
                .into_response()
        }
    }
}

/// Terminate an agent pod.
///
/// DELETE /v1/agents/:agent_id
async fn terminate_handler(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    let agent_id = match AgentId::from_hex(&agent_id) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(format!("Invalid agent ID: {e}"), 400)),
            )
                .into_response();
        }
    };

    match state.scheduler.terminate_agent(&agent_id).await {
        Ok(()) => {
            tracing::info!(agent_id = %agent_id, "Terminated agent pod via HTTP API");
            StatusCode::ACCEPTED.into_response()
        }
        Err(e) => {
            tracing::error!(
                agent_id = %agent_id,
                error = %e,
                "Failed to terminate agent pod"
            );
            let code = e.http_status_code();
            (
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(ErrorResponse::new(e.to_string(), code)),
            )
                .into_response()
        }
    }
}

/// Get the status of an agent's pod.
///
/// GET /v1/agents/:agent_id/status
async fn status_handler(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    let agent_id = match AgentId::from_hex(&agent_id) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(format!("Invalid agent ID: {e}"), 400)),
            )
                .into_response();
        }
    };

    match state.scheduler.get_pod_status(&agent_id).await {
        Ok(status) => Json(status).into_response(),
        Err(SchedulerError::PodNotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("Pod not found", 404)),
        )
            .into_response(),
        Err(e) => {
            let code = e.http_status_code();
            (
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(ErrorResponse::new(e.to_string(), code)),
            )
                .into_response()
        }
    }
}

/// Response for endpoint lookup.
#[derive(Debug, Serialize)]
struct EndpointResponse {
    /// The pod endpoint (IP:port), if available.
    endpoint: Option<String>,
}

/// Get the network endpoint for an agent's pod.
///
/// GET /v1/agents/:agent_id/endpoint
async fn endpoint_handler(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    let agent_id = match AgentId::from_hex(&agent_id) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(format!("Invalid agent ID: {e}"), 400)),
            )
                .into_response();
        }
    };

    match state.scheduler.get_pod_endpoint(&agent_id).await {
        Ok(endpoint) => Json(EndpointResponse { endpoint }).into_response(),
        Err(e) => {
            let code = e.http_status_code();
            (
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(ErrorResponse::new(e.to_string(), code)),
            )
                .into_response()
        }
    }
}

// ============================================================================
// Router
// ============================================================================

fn create_router(state: AppState) -> Router {
    Router::new()
        // Health & readiness
        .route("/health", get(health_handler))
        .route("/ready", get(ready_handler))
        // Agent pod management
        .route("/v1/agents/:agent_id/schedule", post(schedule_handler))
        .route("/v1/agents/:agent_id", delete(terminate_handler))
        .route("/v1/agents/:agent_id/status", get(status_handler))
        .route("/v1/agents/:agent_id/endpoint", get(endpoint_handler))
        .with_state(state)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,aura_swarm=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Starting Aura Swarm Scheduler");

    // Load configuration from environment
    let listen_addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let config = SchedulerConfig::from_env();

    tracing::info!(
        namespace = %config.namespace,
        image = %config.image,
        gateway_url = %config.gateway_url,
        "Loaded scheduler configuration"
    );

    // Initialize K8s scheduler
    let scheduler = Arc::new(K8sScheduler::new(config).await?);
    tracing::info!("Connected to Kubernetes cluster");

    // Start the reconciler as a background task
    let reconciler_scheduler = Arc::clone(&scheduler);
    tokio::spawn(async move {
        reconciler_scheduler.run_reconciler().await;
    });
    tracing::info!("Started pod reconciliation loop");

    // Create app state
    let state = AppState { scheduler };

    // Create router
    let app = create_router(state);

    // Start server
    tracing::info!(listen_addr = %listen_addr, "Starting HTTP server");
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
