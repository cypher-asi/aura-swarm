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
use std::time::Duration;

use aura_swarm_core::AgentId;
use aura_swarm_scheduler::{
    ComputeUsageReporter, GatewayAuthError, K8sScheduler, Scheduler, SchedulerBillingConfig,
    SchedulerConfig, SchedulerError,
};
use aura_swarm_store::AgentSpec;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tower_http::timeout::TimeoutLayer;
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
    /// Human-readable agent name for pod naming and display.
    agent_name: String,
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
/// `POST /v1/agents/:agent_id/schedule`
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
        .schedule_agent(&agent_id, &req.user_id, &req.agent_name, &req.spec)
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
/// `DELETE /v1/agents/:agent_id`
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
/// `GET /v1/agents/:agent_id/status`
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
/// `GET /v1/agents/:agent_id/endpoint`
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
        .layer(TimeoutLayer::new(Duration::from_secs(30)))
        .with_state(state)
}

/// Maximum number of retries for transient transport errors when probing the
/// gateway's `/internal/health` endpoint at boot. The gateway and scheduler
/// are usually rolled together so a brief unavailability window is normal;
/// 401/403 / unexpected statuses are NOT retried (see [`GatewayAuthError`]).
const GATEWAY_AUTH_MAX_RETRIES: u32 = 5;

/// Verify the scheduler can authenticate to the gateway's internal API at
/// boot, with bounded retries on transient transport errors. Hard failures
/// (token rejected, unexpected status) bail immediately so the deployment
/// crashloops with a clear, operator-actionable error message instead of
/// silently mis-running.
///
/// In dev mode (`INTERNAL_TOKEN` and `GATEWAY_TOKEN` both unset) the check
/// is skipped with a warning, matching the gateway's pre-2cc329c behavior.
async fn ensure_gateway_auth(scheduler: &K8sScheduler) -> Result<(), Box<dyn std::error::Error>> {
    if scheduler.gateway_token().is_empty() {
        tracing::warn!(
            "INTERNAL_TOKEN is not set; skipping gateway auth check (dev mode). \
             Production must set INTERNAL_TOKEN so that scheduler->gateway \
             internal callbacks succeed; otherwise new agents will silently \
             stay in Provisioning."
        );
        return Ok(());
    }

    let mut attempt: u32 = 0;
    loop {
        match scheduler.verify_gateway_auth().await {
            Ok(()) => {
                tracing::info!(
                    "Verified scheduler can authenticate to gateway /internal/health"
                );
                return Ok(());
            }
            Err(err) if err.is_retriable() && attempt < GATEWAY_AUTH_MAX_RETRIES => {
                attempt = attempt.saturating_add(1);
                let backoff = Duration::from_secs(1u64 << attempt.min(5));
                tracing::warn!(
                    error = %err,
                    attempt,
                    max_retries = GATEWAY_AUTH_MAX_RETRIES,
                    backoff_secs = backoff.as_secs(),
                    "Gateway auth check failed transiently; retrying"
                );
                tokio::time::sleep(backoff).await;
            }
            Err(err @ GatewayAuthError::InvalidToken { .. }) => {
                tracing::error!(
                    error = %err,
                    "Gateway rejected the scheduler's INTERNAL_TOKEN. This is almost \
                     always token drift after a redeploy: re-apply aura-swarm-secrets \
                     and roll both the gateway and scheduler so they share the same \
                     INTERNAL_TOKEN. Refusing to start so the regression is loud."
                );
                return Err(err.into());
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "Gateway auth check failed (non-retriable or retries exhausted); \
                     refusing to start to avoid silently stuck-Provisioning agents"
                );
                return Err(err.into());
            }
        }
    }
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

    // Initialize billing reporter if configured
    let billing_config = SchedulerBillingConfig::from_env();
    let billing_reporter = if billing_config.is_configured() {
        let reporter = Arc::new(ComputeUsageReporter::new(billing_config.clone())?);
        tracing::info!(
            url = %billing_config.url,
            report_interval_seconds = billing_config.report_interval_seconds,
            "Billing reporter enabled"
        );
        Some(reporter)
    } else {
        tracing::info!("Billing reporter not configured (no Z_BILLING_API_KEY)");
        None
    };

    // Initialize K8s scheduler with optional billing
    let scheduler = match &billing_reporter {
        Some(reporter) => Arc::new(K8sScheduler::with_billing(config, Arc::clone(reporter)).await?),
        None => Arc::new(K8sScheduler::new(config).await?),
    };
    tracing::info!("Connected to Kubernetes cluster");

    // Verify the scheduler can authenticate to the gateway's internal API
    // before we start the reconciler. If INTERNAL_TOKEN drifts between the
    // scheduler and gateway across a redeploy, the scheduler can still
    // successfully create pods (it talks directly to the K8s API) but every
    // notify_status_change PATCH would 401, so brand-new agents silently get
    // stuck in `Provisioning` and the stuck-pod escalation in
    // handle_pod_update can't surface it either. Crashing here makes that
    // misconfiguration loud at deploy time.
    ensure_gateway_auth(&scheduler).await?;

    // Start the reconciler as a background task
    let reconciler_scheduler = Arc::clone(&scheduler);
    tokio::spawn(async move {
        reconciler_scheduler.run_reconciler().await;
    });
    tracing::info!("Started pod reconciliation loop");

    // Start billing reporter background task if configured
    if let Some(reporter) = billing_reporter {
        let report_interval = reporter.report_interval();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(report_interval);
            loop {
                interval.tick().await;
                let count = reporter.report_all_usage().await;
                if count > 0 {
                    tracing::debug!(count, "Reported compute usage for pods");
                }
            }
        });
        tracing::info!("Started billing reporter background task");
    }

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
