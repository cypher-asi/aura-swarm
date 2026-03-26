//! Aura Swarm Gateway - HTTP/WebSocket API Gateway
//!
//! This is the main entry point for the gateway service.
//! The gateway provides the public API for managing agents and sessions,
//! with embedded control plane functionality.
//!
//! # Dev Mode
//!
//! Build with `--features dev-mode` and set `DEV_MODE=true` to use a mock
//! JWT validator that doesn't require network access to zOS.
//! Use tokens in format: `test-token:<identity-uuid>:<namespace-uuid>`
//!
//! # Scheduler Integration
//!
//! Set `SCHEDULER_URL` environment variable to enable scheduler integration.
//! If not set, the gateway operates without scheduler (local-only mode).

use std::sync::Arc;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(feature = "dev-mode")]
use aura_swarm_auth::MockJwtValidator;
#[cfg(not(feature = "dev-mode"))]
use aura_swarm_auth::{AuthConfig, ZosTokenValidator};
use aura_swarm_control::{
    BillingChecker, BillingConfig as ControlBillingConfig, ControlConfig, ControlPlane,
    ControlPlaneService, HttpSchedulerClient,
};
use aura_swarm_gateway::{create_router, GatewayConfig, GatewayState};
use aura_swarm_store::RocksStore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,aura_swarm=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Starting Aura Swarm Gateway");

    let listen_addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data/aura-swarm".into());
    let auth_base_url =
        std::env::var("AUTH_BASE_URL").unwrap_or_else(|_| "https://zosapi.zero.tech".into());
    let auth_audience = std::env::var("AUTH_AUDIENCE").ok();
    let scheduler_url = std::env::var("SCHEDULER_URL").ok();

    tracing::info!(
        listen_addr = %listen_addr,
        data_dir = %data_dir,
        auth_base_url = %auth_base_url,
        auth_audience = ?auth_audience,
        scheduler_url = ?scheduler_url,
        "Gateway configuration loaded"
    );

    tracing::info!(path = %data_dir, "Opening RocksDB store");
    let store = Arc::new(RocksStore::open(&data_dir)?);

    let scheduler_client = match scheduler_url {
        Some(url) => {
            tracing::info!(scheduler_url = %url, "Scheduler integration enabled");
            Some(Arc::new(HttpSchedulerClient::new(url)?))
        }
        None => None,
    };

    if scheduler_client.is_none() {
        tracing::warn!("No SCHEDULER_URL set - running without scheduler integration");
    }

    // Initialize billing checker for credit checks (control plane side only)
    let control_billing = {
        let billing_config = ControlBillingConfig::from_env();
        if billing_config.is_configured() {
            tracing::info!(url = %billing_config.url, "Control plane billing checker enabled");
            Some(Arc::new(BillingChecker::new(billing_config)?))
        } else {
            tracing::info!("Control plane billing checker not configured (no Z_BILLING_API_KEY)");
            None
        }
    };

    let control = Arc::new(ControlPlaneService::with_integrations(
        store,
        ControlConfig::default(),
        scheduler_client,
        control_billing,
    ));

    tracing::info!(
        has_scheduler = control.has_scheduler(),
        has_billing = control.has_billing(),
        "Control plane initialized"
    );

    match control.reconcile_idle_agents().await {
        Ok(0) => tracing::debug!("No stale Running agents to reconcile"),
        Ok(n) => tracing::info!(count = n, "Reconciled stale Running agents → Idle"),
        Err(e) => tracing::warn!(error = %e, "Failed to reconcile idle agents on startup"),
    }

    #[cfg(feature = "dev-mode")]
    let jwt_validator = {
        tracing::warn!("DEV MODE ENABLED - using mock JWT validator");
        tracing::warn!("Use tokens in format: test-token:<user-uuid>");
        Arc::new(MockJwtValidator::default())
    };

    #[cfg(not(feature = "dev-mode"))]
    let jwt_validator = {
        let auth_config = AuthConfig {
            base_url: auth_base_url,
            audience: auth_audience,
            jwks_refresh_seconds: 300,
        };
        let cache_ttl = std::time::Duration::from_secs(60);
        tracing::info!("Using zOS token introspection (cache TTL = {cache_ttl:?})");
        Arc::new(ZosTokenValidator::new(auth_config, cache_ttl)?)
    };
    tracing::info!("JWT validator initialized");

    let gateway_config = GatewayConfig::from_env();
    let state = GatewayState::new(control, jwt_validator, gateway_config);

    let app = create_router(state);
    tracing::info!("Router configured with all API endpoints");

    tracing::info!(listen_addr = %listen_addr, "Starting HTTP server");
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
