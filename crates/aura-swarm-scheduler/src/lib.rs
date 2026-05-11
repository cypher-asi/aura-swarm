//! Kubernetes pod scheduler for aura-swarm agent management.
//!
//! This crate provides the [`Scheduler`] trait and [`K8sScheduler`] implementation
//! for managing agent pods in a Kubernetes cluster. It handles:
//!
//! - Pod creation with Kata Containers runtime for microVM isolation
//! - Pod lifecycle management (start, stop, health checks)
//! - Endpoint caching for fast routing
//! - Status reconciliation with the control plane
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      Control Plane                               │
//! └─────────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                       K8sScheduler                               │
//! │  ┌─────────────┐ ┌─────────────┐ ┌─────────────────────────┐   │
//! │  │  Schedule   │ │  Terminate  │ │    Reconciliation       │   │
//! │  │  Agent      │ │  Agent      │ │    Loop                 │   │
//! │  └─────────────┘ └─────────────┘ └─────────────────────────┘   │
//! │                         │                                       │
//! │               ┌─────────┴─────────┐                            │
//! │               ▼                   ▼                            │
//! │        ┌───────────┐       ┌───────────┐                       │
//! │        │ Endpoint  │       │   Pod     │                       │
//! │        │ Cache     │       │   Builder │                       │
//! │        └───────────┘       └───────────┘                       │
//! └─────────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                    Kubernetes API Server                         │
//! └─────────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                    MicroVM Pods (Kata + Firecracker)             │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```no_run
//! use aura_swarm_scheduler::{K8sScheduler, Scheduler, SchedulerConfig};
//! use aura_swarm_core::{AgentId, UserId};
//! use aura_swarm_store::AgentSpec;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create scheduler with default config
//! let config = SchedulerConfig::default();
//! let scheduler = K8sScheduler::new(config).await?;
//!
//! // Schedule an agent
//! let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
//! let agent_id = AgentId::generate();
//! let spec = AgentSpec::default();
//!
//! scheduler.schedule_agent(&agent_id, &user_id.to_string(), "my-agent", &spec).await?;
//!
//! // Check if it's ready
//! let status = scheduler.get_pod_status(&agent_id).await?;
//! println!("Pod phase: {:?}, ready: {}", status.phase, status.ready);
//!
//! // Get the endpoint for routing
//! if let Some(endpoint) = scheduler.get_pod_endpoint(&agent_id).await? {
//!     println!("Agent endpoint: {}", endpoint);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Testing
//!
//! For testing without a real Kubernetes cluster, enable the `test-utils` feature
//! and use the mock scheduler:
//!
//! ```text
//! use aura_swarm_scheduler::{Scheduler, MockScheduler};
//! use aura_swarm_core::{AgentId, UserId};
//! use aura_swarm_store::AgentSpec;
//!
//! async fn example() -> Result<(), Box<dyn std::error::Error>> {
//!     let scheduler = MockScheduler::new();
//!
//!     let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
//!     let agent_id = AgentId::generate();
//!     let spec = AgentSpec::default();
//!
//!     scheduler.schedule_agent(&agent_id, &user_id.to_string(), "test-agent", &spec).await?;
//!     assert_eq!(scheduler.pod_count(), 1);
//!     Ok(())
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]

pub mod billing;
pub mod cache;
pub mod error;
pub mod k8s;
#[cfg(any(test, feature = "test-utils"))]
pub mod mock_scheduler;
pub mod pod;
pub mod types;

pub use billing::{ComputeUsageReporter, PodUsageInfo, SchedulerBillingConfig};
pub use error::{Result, SchedulerError};
pub use k8s::{verify_gateway_auth, GatewayAuthError, K8sScheduler, Scheduler};
pub use types::{PodInfo, PodPhase, PodStatus, SchedulerConfig};

#[cfg(any(test, feature = "test-utils"))]
pub use mock_scheduler::MockScheduler;
