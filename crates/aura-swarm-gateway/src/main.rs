//! Aura Swarm Gateway - HTTP/WebSocket API Gateway
//!
//! This is the main entry point for the gateway service.
//! The gateway provides the public API for managing agents and sessions,
//! with embedded control plane functionality.
//!
//! # Dev Mode
//!
//! Build with `--features dev-mode` and set `DEV_MODE=true` to use a mock
//! JWT validator that doesn't require network access to Zero-ID.
//! Use tokens in format: `test-token:<identity-uuid>:<namespace-uuid>`
//!
//! # Scheduler Integration
//!
//! Set `SCHEDULER_URL` environment variable to enable scheduler integration.
//! If not set, the gateway operates without scheduler (local-only mode).

use std::sync::Arc;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(not(feature = "dev-mode"))]
use aura_swarm_auth::{AuthConfig, JwksValidator};
#[cfg(feature = "dev-mode")]
use aura_swarm_auth::MockJwtValidator;
use aura_swarm_control::{ControlConfig, ControlPlaneService, HttpSchedulerClient};
use aura_swarm_gateway::{create_router, GatewayConfig, GatewayState};
use aura_swarm_store::RocksStore;

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

    tracing::info!("Starting Aura Swarm Gateway");

    // Load configuration from environment
    let listen_addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data/aura-swarm".into());
    let auth_base_url =
        std::env::var("AUTH_BASE_URL").unwrap_or_else(|_| "https://zid.zero.tech".into());
    let auth_audience = std::env::var("AUTH_AUDIENCE").unwrap_or_else(|_| "zero-vault".into());
    let scheduler_url = std::env::var("SCHEDULER_URL").ok();

    tracing::info!(
        listen_addr = %listen_addr,
        data_dir = %data_dir,
        auth_base_url = %auth_base_url,
        auth_audience = %auth_audience,
        scheduler_url = ?scheduler_url,
        "Gateway configuration loaded"
    );

    // Initialize RocksDB store
    tracing::info!(path = %data_dir, "Opening RocksDB store");
    let store = Arc::new(RocksStore::open(&data_dir)?);

    // Initialize control plane service with optional scheduler integration
    let scheduler_client = scheduler_url.map(|url| {
        tracing::info!(scheduler_url = %url, "Scheduler integration enabled");
        Arc::new(HttpSchedulerClient::new(url))
    });

    if scheduler_client.is_none() {
        tracing::warn!("No SCHEDULER_URL set - running without scheduler integration");
    }

    let control = Arc::new(ControlPlaneService::with_optional_scheduler(
        store,
        ControlConfig::default(),
        scheduler_client,
    ));

    tracing::info!(
        has_scheduler = control.has_scheduler(),
        "Control plane initialized"
    );

    // Initialize JWT validator
    #[cfg(feature = "dev-mode")]
    let jwt_validator = {
        tracing::warn!("DEV MODE ENABLED - using mock JWT validator");
        tracing::warn!("Use tokens in format: test-token:<identity-uuid>:<namespace-uuid>");
        Arc::new(MockJwtValidator::default())
    };

    #[cfg(not(feature = "dev-mode"))]
    let jwt_validator = {
        let auth_config = AuthConfig {
            base_url: auth_base_url,
            audience: auth_audience,
            jwks_refresh_seconds: 300,
        };
        Arc::new(JwksValidator::new(auth_config))
    };
    tracing::info!("JWT validator initialized");

    // Build gateway state and configuration
    let gateway_config = GatewayConfig::default();
    let state = GatewayState::new(control, jwt_validator, gateway_config);

    // Create the full router with all API endpoints
    let app = create_router(state);
    tracing::info!("Router configured with all API endpoints");

    // Start HTTP server
    tracing::info!(listen_addr = %listen_addr, "Starting HTTP server");
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
