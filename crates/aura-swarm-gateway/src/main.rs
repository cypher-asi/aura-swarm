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
//!
//! # KBS Integration (state DEK lifecycle)
//!
//! Set `KBS_URL` and `KBS_ADMIN_KEY_PATH` (Ed25519 admin private key, PEM)
//! to provision/revoke per-agent state DEKs in the Trustee KBS. `KBS_ENABLED`
//! (default true) disables the integration; without an admin key path a
//! no-op client is used (dev mode).

use std::sync::Arc;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(feature = "dev-mode")]
use aura_swarm_auth::MockJwtValidator;
#[cfg(not(feature = "dev-mode"))]
use aura_swarm_auth::{AuthConfig, ZosTokenValidator};
use aura_swarm_control::{
    BillingChecker, BillingConfig as ControlBillingConfig, ControlConfig, ControlPlane,
    ControlPlaneService, CronServiceConfig, HttpKbsClient, HttpPodTriggerClient,
    HttpSchedulerClient, KbsClient, KbsConfig, NoopKbsClient, ProcessCronService,
};
use aura_swarm_gateway::{create_router, GatewayConfig, GatewayState};
use aura_swarm_store::RocksStore;

/// Build the KBS client for the per-agent state DEK lifecycle (provision at
/// confidential-agent create, revoke at destroy). Without an admin key
/// configured (dev mode) a no-op client is used.
fn build_kbs_client() -> Result<Arc<dyn KbsClient>, Box<dyn std::error::Error>> {
    let kbs_config = KbsConfig::from_env();
    if kbs_config.is_configured() {
        tracing::info!(
            kbs_url = %kbs_config.url,
            "KBS DEK lifecycle enabled"
        );
        Ok(Arc::new(HttpKbsClient::new(&kbs_config)?))
    } else {
        tracing::warn!(
            "KBS not configured (no KBS_ADMIN_KEY_PATH or KBS_ENABLED=false) - \
             using no-op KBS client; state DEKs will NOT be provisioned"
        );
        Ok(Arc::new(NoopKbsClient::new()))
    }
}

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

    // Swarm TEE upgrade R2: run pending schema migrations (v1 -> v2
    // rewrites legacy agent records to tiered/sealed) before anything
    // is served. A failure here aborts startup — serving against an
    // unmigrated store would mix legacy and tiered behavior.
    let migration = aura_swarm_store::migrations::run(&store)?;
    if migration.agents_migrated > 0 {
        tracing::info!(
            from_version = migration.from_version,
            to_version = migration.to_version,
            agents_migrated = migration.agents_migrated,
            "Store schema migration complete"
        );
    } else {
        tracing::info!(
            schema_version = migration.to_version,
            "Store schema is current"
        );
    }

    let scheduler_client = match scheduler_url {
        Some(url) => {
            tracing::info!(scheduler_url = %url, "Scheduler integration enabled");
            // The scheduler enforces the shared INTERNAL_TOKEN bearer on
            // its /v1 API when configured; send it on every request.
            let token = std::env::var("INTERNAL_TOKEN").ok().filter(|t| !t.is_empty());
            Some(Arc::new(HttpSchedulerClient::new(url)?.with_token(token)))
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

    let kbs_client = build_kbs_client()?;

    let mut control = ControlPlaneService::with_integrations(
        Arc::clone(&store),
        ControlConfig::default(),
        scheduler_client,
        control_billing,
    );
    control.set_kbs(kbs_client);
    let control = Arc::new(control);

    tracing::info!(
        has_scheduler = control.has_scheduler(),
        has_billing = control.has_billing(),
        has_kbs = control.has_kbs(),
        "Control plane initialized"
    );

    match control.reconcile_idle_agents().await {
        Ok(0) => tracing::debug!("No stale Running agents to reconcile"),
        Ok(n) => tracing::info!(count = n, "Reconciled stale Running agents → Idle"),
        Err(e) => tracing::warn!(error = %e, "Failed to reconcile idle agents on startup"),
    }

    // R2 post-migration reconciliation: agents the v1 -> v2 migration
    // marked as sealed have no DEK in the KBS yet — provision the
    // missing ones (put-if-absent; an existing DEK is never touched).
    // Failures are non-fatal: the pass reruns on every startup.
    match control.backfill_missing_deks().await {
        Ok(summary) if summary.provisioned > 0 || summary.failed > 0 => tracing::info!(
            sealed_agents = summary.sealed_agents,
            provisioned = summary.provisioned,
            already_present = summary.already_present,
            failed = summary.failed,
            "DEK backfill pass complete"
        ),
        Ok(summary) => tracing::debug!(
            sealed_agents = summary.sealed_agents,
            "DEK backfill: nothing to do"
        ),
        Err(e) => tracing::warn!(error = %e, "DEK backfill pass failed; will retry next startup"),
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
    #[cfg(not(feature = "dev-mode"))]
    if gateway_config.internal_token.is_none() {
        return Err("INTERNAL_TOKEN must be set to protect /internal gateway endpoints".into());
    }

    // ProcessCronService: fire due process triggers (wake -> ready ->
    // POST trigger to pod) and auto-hibernate long-idle agents. The pod
    // trigger client authenticates with the platform INTERNAL_TOKEN —
    // the same value the scheduler injects into confidential pods as
    // AURA_SWARM_INTERNAL_TOKEN.
    let cron_config = CronServiceConfig::from_env();
    let pod_trigger_client = Arc::new(HttpPodTriggerClient::new(
        gateway_config.internal_token.clone(),
    ));
    let cron_service = Arc::new(ProcessCronService::new(
        Arc::clone(&store),
        Arc::clone(&control),
        pod_trigger_client,
        cron_config,
    ));
    tokio::spawn(Arc::clone(&cron_service).run_cron_loop());
    tokio::spawn(Arc::clone(&cron_service).run_auto_hibernate_loop());
    tracing::info!("Started ProcessCronService cron and auto-hibernate loops");

    let state = GatewayState::new(control, jwt_validator, gateway_config);

    let app = create_router(state);
    tracing::info!("Router configured with all API endpoints");

    tracing::info!(listen_addr = %listen_addr, "Starting HTTP server");
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
