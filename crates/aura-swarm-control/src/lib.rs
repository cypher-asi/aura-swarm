//! Control plane for aura-swarm agent lifecycle management.
//!
//! This crate provides the core business logic for managing agent lifecycles
//! and sessions. It coordinates between the storage layer and (eventually)
//! the scheduler.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     Gateway (HTTP/WS)                       │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    ControlPlaneService                       │
//! │  ┌─────────────┐ ┌─────────────┐ ┌─────────────────────┐   │
//! │  │   Agent     │ │  Session    │ │    Lifecycle        │   │
//! │  │   CRUD      │ │  Mgmt       │ │    State Machine    │   │
//! │  └─────────────┘ └─────────────┘ └─────────────────────┘   │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!               ┌──────────────┼──────────────┐
//!               ▼              ▼              ▼
//!        ┌──────────┐   ┌──────────┐   ┌──────────┐
//!        │  Store   │   │  Auth    │   │ Scheduler│
//!        │ (RocksDB)│   │  (JWT)   │   │  (K8s)   │
//!        └──────────┘   └──────────┘   └──────────┘
//! ```
//!
//! # Usage
//!
//! ```no_run
//! use std::sync::Arc;
//! use aura_swarm_control::{ControlPlane, ControlPlaneService, CreateAgentRequest};
//! use aura_swarm_store::RocksStore;
//! use aura_swarm_core::UserId;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Initialize store
//! let store = Arc::new(RocksStore::open("/tmp/aura-swarm")?);
//!
//! // Create control plane service
//! let control = ControlPlaneService::with_defaults(store);
//!
//! // Create an agent
//! let user_id = UserId::from_bytes([0u8; 32]);
//! let request = CreateAgentRequest::new("my-agent");
//! let agent = control.create_agent(&user_id, request).await?;
//!
//! println!("Created agent: {}", agent.agent_id);
//! # Ok(())
//! # }
//! ```
//!
//! # State Machine
//!
//! Agents follow a strict state machine with valid transitions:
//!
//! - `Provisioning` → `Running` (pod ready) or `Error`
//! - `Running` → `Idle`, `Hibernating`, `Stopping`, or `Error`
//! - `Idle` → `Running`, `Hibernating`, `Stopping`, or `Error`
//! - `Hibernating` → `Running` (wake), `Stopping`, or `Error`
//! - `Stopping` → `Stopped` or `Error`
//! - `Stopped` → `Provisioning` (restart)
//! - `Error` → `Stopped` or `Provisioning`
//!
//! See the [`lifecycle`] module for transition validation helpers.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]

pub mod error;
pub mod lifecycle;
pub mod scheduler_client;
pub mod service;
pub mod session;
pub mod types;

pub use error::{ControlError, Result};
pub use scheduler_client::{HttpSchedulerClient, NoopSchedulerClient, PodStatusResponse, SchedulerClient};
pub use service::{ControlPlane, ControlPlaneService};
pub use types::{AgentStatus, ControlConfig, CreateAgentRequest, LogOptions};

// Re-export commonly used types from dependencies for convenience
pub use aura_swarm_core::{AgentId, SessionId, UserId};
pub use aura_swarm_store::{Agent, AgentSpec, AgentState, Session, SessionStatus};
