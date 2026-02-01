//! Aura Swarm Control Plane - Agent Lifecycle Management Service
//!
//! This is the main entry point for the control plane service.
//! It provides internal APIs for agent and session management.

use std::sync::Arc;

use aura_swarm_control::ControlPlaneService;
use aura_swarm_store::RocksStore;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Application state shared across handlers.
struct AppState<S: aura_swarm_store::Store> {
    control: Arc<ControlPlaneService<S>>,
}

impl<S: aura_swarm_store::Store> Clone for AppState<S> {
    fn clone(&self) -> Self {
        Self {
            control: Arc::clone(&self.control),
        }
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

async fn health_handler() -> impl IntoResponse {
    Json(HealthResponse {
        status: "healthy",
        service: "aura-swarm-control",
    })
}

async fn ready_handler<S: aura_swarm_store::Store + 'static>(
    State(_state): State<AppState<S>>,
) -> impl IntoResponse {
    // Could add store health check here
    (StatusCode::OK, "ready")
}

fn create_router<S: aura_swarm_store::Store + 'static>(state: AppState<S>) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/ready", get(ready_handler::<S>))
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

    tracing::info!("Starting Aura Swarm Control Plane");

    // Load configuration from environment
    let listen_addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());

    // Initialize store
    let store = Arc::new(RocksStore::open(&data_dir)?);
    tracing::info!(data_dir = %data_dir, "Initialized RocksDB store");

    // Initialize control plane service
    let control = Arc::new(ControlPlaneService::with_defaults(store));

    // Create app state
    let state = AppState { control };

    // Create router
    let app = create_router(state);

    // Start server
    tracing::info!(listen_addr = %listen_addr, "Starting HTTP server");
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
