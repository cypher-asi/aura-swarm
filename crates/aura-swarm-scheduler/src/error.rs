//! Error types for the scheduler crate.

use thiserror::Error;

/// Errors that can occur during scheduling operations.
#[derive(Error, Debug)]
pub enum SchedulerError {
    /// Kubernetes API error.
    #[error("Kubernetes API error: {0}")]
    KubeApi(#[from] kube::Error),

    /// Pod not found in the cluster.
    #[error("Pod not found: {0}")]
    PodNotFound(String),

    /// Pod creation failed.
    #[error("Pod creation failed: {0}")]
    PodCreationFailed(String),

    /// Timeout waiting for pod to be ready.
    #[error("Timeout waiting for pod: {0}")]
    Timeout(String),

    /// Agent ID parsing error.
    #[error("Invalid agent ID: {0}")]
    InvalidAgentId(String),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Store error.
    #[error("Store error: {0}")]
    Store(#[from] aura_swarm_store::StoreError),

    /// Health check failed.
    #[error("Health check failed: {0}")]
    HealthCheckFailed(String),
}

impl SchedulerError {
    /// Check if this error is retriable.
    #[must_use]
    pub fn is_retriable(&self) -> bool {
        matches!(
            self,
            Self::KubeApi(_) | Self::Timeout(_) | Self::HealthCheckFailed(_)
        )
    }

    /// Get the HTTP status code for this error.
    #[must_use]
    pub fn http_status_code(&self) -> u16 {
        match self {
            Self::PodNotFound(_) => 404,
            Self::InvalidAgentId(_) | Self::Config(_) => 400,
            Self::PodCreationFailed(_) => 500,
            Self::KubeApi(_) | Self::Timeout(_) | Self::Store(_) | Self::HealthCheckFailed(_) => {
                503
            }
        }
    }
}

/// A specialized Result type for scheduler operations.
pub type Result<T> = std::result::Result<T, SchedulerError>;
