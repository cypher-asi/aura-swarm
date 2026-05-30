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
//! use aura_swarm_client::SwarmClient;
//! use aura_swarm_client::ws::connect_run_stream;
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
//! // Start a run, then attach to its event stream
//! let run = client.create_run(&agent.agent_id).await?;
//! let ws_url = client.run_ws_url(&agent.agent_id, &run.run_id);
//! let (sender, mut events) = connect_run_stream(&ws_url, client.token()).await?;
//!
//! sender.send_user_message("Hello, agent!").await?;
//! while let Some(event) = events.recv().await {
//!     println!("event: {event:?}");
//! }
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
pub mod ws;

pub use client::SwarmClient;
pub use error::SwarmClientError;
pub use ws::{connect_run_stream, RunStreamSender};
pub use types::{
    CreateRemoteAgentRequest, CreateSessionRequest, CreateSessionResponse,
    ListRemoteAgentsResponse, ListSessionsResponse, RemoteAgent, RemoteAgentSpec, RemoteAgentState,
    RemoteAgentStateResponse, RemoteIsolationLevel, SessionConfig, SessionResponse, SessionStatus,
};

// Re-export the protocol message types so consumers can drive a run stream
// without taking a direct dependency on `aura-swarm-protocol`.
pub use aura_swarm_protocol::{
    InboundMessage, OutboundMessage, RuntimeRunResponse, ToolCallbackRequest, ToolCallbackResponse,
};
