//! HTTP and WebSocket gateway for the aura-swarm agent platform.
//!
//! This crate provides the public-facing API for managing agents and sessions.
//! It handles:
//!
//! - JWT authentication with Zero-ID integration
//! - REST HTTP endpoints for agent management
//! - WebSocket proxying to agent pods
//! - Rate limiting and request validation
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                        Clients                               │
//! │                   (HTTP / WebSocket)                        │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    aura-swarm-gateway                        │
//! │  ┌─────────────┐ ┌─────────────┐ ┌─────────────────────┐   │
//! │  │   Auth      │ │   Router    │ │    WebSocket        │   │
//! │  │  Extractor  │ │  + Handlers │ │    Proxy            │   │
//! │  └─────────────┘ └─────────────┘ └─────────────────────┘   │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!               ┌──────────────┼──────────────┐
//!               ▼              ▼              ▼
//!        ┌──────────┐   ┌──────────┐   ┌──────────┐
//!        │ Control  │   │  Auth    │   │  Agent   │
//!        │ Plane    │   │ (JWT)    │   │  Pods    │
//!        └──────────┘   └──────────┘   └──────────┘
//! ```
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use aura_swarm_gateway::{GatewayConfig, GatewayState, create_router};
//! use aura_swarm_control::{ControlPlaneService};
//! use aura_swarm_auth::{JwksValidator, AuthConfig};
//! use aura_swarm_store::RocksStore;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Initialize dependencies
//! let store = Arc::new(RocksStore::open("/tmp/aura-swarm")?);
//! let control = Arc::new(ControlPlaneService::with_defaults(store));
//! let jwt_validator = Arc::new(JwksValidator::new(AuthConfig::default()));
//!
//! // Create gateway state
//! let config = GatewayConfig::default();
//! let state = GatewayState::new(control, jwt_validator, config);
//!
//! // Create router
//! let app = create_router(state);
//!
//! // Run server
//! let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
//! axum::serve(listener, app).await?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod routes;
pub mod state;

pub use config::GatewayConfig;
pub use error::ApiError;
pub use routes::create_router;
pub use state::GatewayState;

// Re-export key types for convenience
pub use auth::AuthUser;
