//! Typed HTTP client for the aura-swarm gateway.
//!
//! This crate provides [`SwarmClient`], a remote-agent-oriented client that
//! wraps the gateway REST API. All agent operations use `remote_agent` naming
//! to establish a clear boundary between a local agent (e.g. in aura-os) and
//! the backing VM managed by the swarm platform.
//!
//! # Usage
//!
//! ```no_run
//! use aura_swarm_client::{SwarmClient, CreateSessionRequest};
//!
//! # async fn example() -> Result<(), aura_swarm_client::SwarmClientError> {
//! let client = SwarmClient::new("http://localhost:8080", "my-jwt-token")?;
//!
//! // Create a remote agent with caller-supplied ID for identity parity
//! let agent = client
//!     .create_remote_agent("my-agent", None, Some("local-agent-id"))
//!     .await?;
//!
//! // Query VM state
//! let state = client.get_remote_agent_state(&agent.agent_id).await?;
//! println!("Agent state: {:?}", state.state);
//!
//! // Open a session
//! let session = client
//!     .create_session(&agent.agent_id, CreateSessionRequest::default())
//!     .await?;
//! println!("WebSocket URL: {}", client.ws_url(&session.session_id));
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]

pub mod client;
pub mod error;
pub mod types;

pub use client::SwarmClient;
pub use error::SwarmClientError;
pub use types::{
    CreateRemoteAgentRequest, CreateSessionRequest, CreateSessionResponse,
    ListRemoteAgentsResponse, ListSessionsResponse, RemoteAgent, RemoteAgentSpec, RemoteAgentState,
    RemoteAgentStateResponse, RemoteIsolationLevel, SessionConfig, SessionResponse, SessionStatus,
};
