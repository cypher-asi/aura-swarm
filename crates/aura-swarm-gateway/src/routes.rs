//! Router configuration.
//!
//! This module sets up the Axum router with all routes and middleware.

use std::sync::Arc;
use std::time::Duration;

use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;

use crate::handlers::{agents, health, sessions, ws};
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
/// - `POST /v1/agents/:agent_id:start` - Start agent
/// - `POST /v1/agents/:agent_id:stop` - Stop agent
/// - `POST /v1/agents/:agent_id:restart` - Restart agent
/// - `POST /v1/agents/:agent_id:hibernate` - Hibernate agent
/// - `POST /v1/agents/:agent_id:wake` - Wake agent
/// - `GET /v1/agents/:agent_id/logs` - Get agent logs
/// - `GET /v1/agents/:agent_id/status` - Get agent status
///
/// ## Sessions (authenticated)
/// - `POST /v1/agents/:agent_id/sessions` - Create session
/// - `GET /v1/agents/:agent_id/sessions` - List sessions
/// - `GET /v1/sessions/:session_id` - Get session
/// - `DELETE /v1/sessions/:session_id` - Close session
/// - `GET /v1/sessions/:session_id/ws` - WebSocket connection
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
            "/v1/agents/{agent_id}",
            get(agents::get_agent::<C, V>).delete(agents::delete_agent::<C, V>),
        )
        // Agent lifecycle (using :action suffix pattern)
        .route(
            "/v1/agents/{agent_id}:start",
            post(agents::start_agent::<C, V>),
        )
        .route(
            "/v1/agents/{agent_id}:stop",
            post(agents::stop_agent::<C, V>),
        )
        .route(
            "/v1/agents/{agent_id}:restart",
            post(agents::restart_agent::<C, V>),
        )
        .route(
            "/v1/agents/{agent_id}:hibernate",
            post(agents::hibernate_agent::<C, V>),
        )
        .route(
            "/v1/agents/{agent_id}:wake",
            post(agents::wake_agent::<C, V>),
        )
        // Agent observability
        .route("/v1/agents/{agent_id}/logs", get(agents::get_logs::<C, V>))
        .route(
            "/v1/agents/{agent_id}/status",
            get(agents::get_status::<C, V>),
        )
        // Sessions
        .route(
            "/v1/agents/{agent_id}/sessions",
            post(sessions::create_session::<C, V>).get(sessions::list_sessions::<C, V>),
        )
        .route(
            "/v1/sessions/{session_id}",
            get(sessions::get_session::<C, V>).delete(sessions::close_session::<C, V>),
        )
        // WebSocket
        .route(
            "/v1/sessions/{session_id}/ws",
            get(ws::websocket_handler::<C, V>),
        )
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
