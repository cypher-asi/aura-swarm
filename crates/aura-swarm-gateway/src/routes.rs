//! Router configuration.
//!
//! This module sets up the Axum router with all routes and middleware.

use std::sync::Arc;
use std::time::Duration;

use axum::routing::{get, patch, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;

use crate::handlers::{
    agents, automaton, files, health, internal, process_triggers, processes, run, secrets,
    sessions, terminal, usage, ws,
};
use crate::state::GatewayState;

/// Create the gateway router with all routes and middleware.
///
/// # Routes
///
/// ## Public
/// - `GET /health` - Health check
///
/// ## Agents (authenticated)
/// - `GET /v1/agents` - List agents
/// - `POST /v1/agents` - Create agent
/// - `GET /v1/agents/:agent_id` - Get agent
/// - `DELETE /v1/agents/:agent_id` - Delete agent
/// - `POST /v1/agents/:agent_id/start` - Start agent
/// - `POST /v1/agents/:agent_id/stop` - Stop agent
/// - `POST /v1/agents/:agent_id/restart` - Restart agent
/// - `POST /v1/agents/:agent_id/hibernate` - Hibernate agent
/// - `POST /v1/agents/:agent_id/wake` - Wake agent
/// - `POST /v1/agents/:agent_id/tier` - Change box tier (upgrade/downgrade)
/// - `GET /v1/agents/:agent_id/logs?tail&since` - VM/platform logs (live tail + termination snapshots)
/// - `GET /v1/agents/:agent_id/status` - Get agent status (real usage-derived metrics)
/// - `GET /v1/agents/:agent_id/state` - Get remote agent state (lifecycle only)
///
/// ## Usage / cost stats (authenticated; zbilling stays the billing source of truth)
/// - `GET /v1/agents/:agent_id/usage?from&to` - Billable intervals + counters + recent events
/// - `GET /v1/usage?from&to` - Per-agent summaries + grand total for the caller's agents
///
/// ## Terminal (authenticated, proxied to agent pod)
/// - `GET /v1/agents/:agent_id/terminal/ws` - Terminal WebSocket (spawn/IO/kill)
///
/// ## Files (authenticated, proxied to agent pod)
/// - `POST /v1/agents/:agent_id/files` - List directory contents
/// - `POST /v1/agents/:agent_id/read-file` - Read file contents
///
/// ## Secrets (authenticated, proxied to the in-TEE vault on the pod;
/// values are never persisted, cached, or logged by the gateway)
/// - `GET /v1/agents/:agent_id/secrets` - List secret names + metadata
/// - `GET /v1/agents/:agent_id/secrets/:name` - Get metadata (`?reveal=true` for the value)
/// - `PUT /v1/agents/:agent_id/secrets/:name` - Create/update a secret
/// - `DELETE /v1/agents/:agent_id/secrets/:name` - Delete a secret
///
/// ## Sessions (authenticated, CRUD/observability only)
/// - `POST /v1/agents/:agent_id/sessions` - Create session
/// - `GET /v1/agents/:agent_id/sessions` - List sessions
/// - `GET /v1/sessions/:session_id` - Get session
/// - `DELETE /v1/sessions/:session_id` - Close session
///
/// ## Runs (authenticated, proxied to pod `POST /v1/run` contract)
/// - `POST /v1/agents/:agent_id/run` - Start a run (chat / dev-loop / task)
/// - `GET /v1/agents/:agent_id/run/list` - List runs
/// - `GET /v1/agents/:agent_id/run/:run_id/status` - Get run status
/// - `POST /v1/agents/:agent_id/run/:run_id/pause` - Pause a run
/// - `POST /v1/agents/:agent_id/run/:run_id/stop` - Stop a run
/// - `GET /v1/agents/:agent_id/stream/:run_id` - Attach to a run's event stream (WS)
/// - `GET /v1/agents/:agent_id/workspace/resolve` - Resolve a workspace path
///
/// ## Process triggers (Swarm TEE phase 8 — metadata only, never payloads)
/// - `GET /v1/agents/:agent_id/process-triggers` - Owner read of registered trigger metadata
///
/// ## Internal (service-token authenticated, cluster-only)
/// - `PATCH /internal/agents/:agent_id/status` - Update agent status (scheduler callback)
/// - `POST /internal/agents/:agent_id/log-snapshot` - Store a pod-log termination snapshot (scheduler)
/// - `GET /internal/agents/active` - List agents expected to have pods (scheduler reconciler)
/// - `GET /internal/agents/all` - List all persisted agents (deploy verification)
/// - `PUT /internal/agents/:agent_id/process-triggers` - Replace-sync trigger metadata (harness)
/// - `DELETE /internal/agents/:agent_id/process-triggers/:process_id` - Unregister one trigger
/// - `GET /internal/health` - Internal health check
pub fn create_router<C, V>(state: GatewayState<C, V>) -> Router
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    // Extract config values before moving state
    let cors_origins = state.config.cors_origins.clone();
    let max_body_bytes = state.config.max_body_bytes;
    let request_timeout_seconds = state.config.request_timeout_seconds;

    // Build CORS layer
    let cors = build_cors_layer(&cors_origins);

    // Build the router
    let state = Arc::new(state);

    Router::new()
        // Health (public)
        .route("/health", get(health::health))
        // Agents
        .route(
            "/v1/agents",
            get(agents::list_agents::<C, V>).post(agents::create_agent::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id",
            get(agents::get_agent::<C, V>).delete(agents::delete_agent::<C, V>),
        )
        // Agent lifecycle
        .route(
            "/v1/agents/:agent_id/start",
            post(agents::start_agent::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/stop",
            post(agents::stop_agent::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/restart",
            post(agents::restart_agent::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/hibernate",
            post(agents::hibernate_agent::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/wake",
            post(agents::wake_agent::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/tier",
            post(agents::change_tier::<C, V>),
        )
        // Agent observability
        .route("/v1/agents/:agent_id/logs", get(agents::get_logs::<C, V>))
        .route(
            "/v1/agents/:agent_id/status",
            get(agents::get_status::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/state",
            get(agents::get_agent_state::<C, V>),
        )
        // Usage / cost stats (user-facing aggregation over usage events)
        .route(
            "/v1/agents/:agent_id/usage",
            get(usage::get_agent_usage::<C, V>),
        )
        .route("/v1/usage", get(usage::get_user_usage::<C, V>))
        // Terminal proxy (single WS — spawn/IO/kill all flow as protocol messages)
        .route(
            "/v1/agents/:agent_id/terminal/ws",
            get(terminal::terminal_ws::<C, V>),
        )
        // File proxies (HTTP — forward to pod /api/files and /api/read-file)
        .route(
            "/v1/agents/:agent_id/files",
            post(files::list_files::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/read-file",
            post(files::read_file::<C, V>),
        )
        // Secrets vault pass-through (HTTP — forward to the pod's /secrets
        // routes; pure proxy, no control-plane persistence or body logging)
        .route(
            "/v1/agents/:agent_id/secrets",
            get(secrets::list_secrets::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/secrets/:name",
            get(secrets::get_secret::<C, V>)
                .put(secrets::put_secret::<C, V>)
                .delete(secrets::delete_secret::<C, V>),
        )
        // Sessions (CRUD + observability; the chat/automaton stream is driven
        // by the run endpoints below)
        .route(
            "/v1/agents/:agent_id/sessions",
            post(sessions::create_session::<C, V>).get(sessions::list_sessions::<C, V>),
        )
        .route(
            "/v1/sessions/:session_id",
            get(sessions::get_session::<C, V>).delete(sessions::close_session::<C, V>),
        )
        // Run proxy (harness POST /v1/run + WS /stream/:run_id contract)
        .route("/v1/agents/:agent_id/run", post(run::run_start::<C, V>))
        .route(
            "/v1/agents/:agent_id/run/list",
            get(run::run_list::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/run/:run_id/status",
            get(run::run_status::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/run/:run_id/pause",
            post(run::run_pause::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/run/:run_id/stop",
            post(run::run_stop::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/stream/:run_id",
            get(ws::run_attach_handler::<C, V>),
        )
        .route(
            "/v1/agents/:agent_id/workspace/resolve",
            get(automaton::workspace_resolve::<C, V>),
        )
        // Process API pass-through (owner create/list scheduled processes on the
        // in-VM harness; peer-pods can't be reached via kubectl port-forward, so
        // this authenticated pod proxy is the supported path).
        .route(
            "/v1/agents/:agent_id/processes",
            get(processes::list_processes::<C, V>).post(processes::create_process::<C, V>),
        )
        // Process-trigger metadata (owner read; registration is internal)
        .route(
            "/v1/agents/:agent_id/process-triggers",
            get(process_triggers::list_triggers::<C, V>),
        )
        // Internal endpoints (service-token authenticated; not user API)
        .route(
            "/internal/agents/:agent_id/process-triggers",
            axum::routing::put(process_triggers::replace_triggers::<C, V>),
        )
        .route(
            "/internal/agents/:agent_id/process-triggers/:process_id",
            axum::routing::delete(process_triggers::delete_trigger::<C, V>),
        )
        .route(
            "/internal/agents/:agent_id/status",
            patch(internal::update_agent_status::<C, V>),
        )
        .route(
            "/internal/agents/:agent_id/log-snapshot",
            post(internal::store_log_snapshot::<C, V>),
        )
        .route(
            "/internal/agents/active",
            get(internal::list_active_agents::<C, V>),
        )
        .route(
            "/internal/agents/all",
            get(internal::list_all_agents::<C, V>),
        )
        .route("/internal/health", get(internal::internal_health::<C, V>))
        // Middleware
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .layer(RequestBodyLimitLayer::new(max_body_bytes))
        .layer(TimeoutLayer::new(Duration::from_secs(
            request_timeout_seconds,
        )))
        .with_state(state)
}

/// Build the CORS layer from configured origins.
fn build_cors_layer(origins: &[String]) -> CorsLayer {
    if origins.iter().any(|o| o == "*") {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        // For specific origins, parse them
        let origins: Vec<_> = origins.iter().filter_map(|o| o.parse().ok()).collect();

        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(Any)
            .allow_headers(Any)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_any_origin() {
        let origins = vec!["*".to_string()];
        let _layer = build_cors_layer(&origins);
        // Just verify it doesn't panic
    }

    #[test]
    fn cors_specific_origins() {
        let origins = vec![
            "http://localhost:3000".to_string(),
            "https://app.example.com".to_string(),
        ];
        let _layer = build_cors_layer(&origins);
    }
}
