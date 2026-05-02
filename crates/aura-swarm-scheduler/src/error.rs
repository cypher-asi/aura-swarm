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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_kube_api_error() -> SchedulerError {
        let resp = kube::core::ErrorResponse {
            status: "Failure".into(),
            code: 500,
            message: "internal error".into(),
            reason: "InternalError".into(),
        };
        SchedulerError::KubeApi(kube::Error::Api(resp))
    }

    fn make_store_error() -> SchedulerError {
        SchedulerError::Store(aura_swarm_store::StoreError::NotFound)
    }

    #[test]
    fn all_error_variants_status_codes() {
        assert_eq!(make_kube_api_error().http_status_code(), 503);
        assert_eq!(
            SchedulerError::PodNotFound("x".into()).http_status_code(),
            404
        );
        assert_eq!(
            SchedulerError::PodCreationFailed("x".into()).http_status_code(),
            500
        );
        assert_eq!(SchedulerError::Timeout("x".into()).http_status_code(), 503);
        assert_eq!(
            SchedulerError::InvalidAgentId("x".into()).http_status_code(),
            400
        );
        assert_eq!(SchedulerError::Config("x".into()).http_status_code(), 400);
        assert_eq!(make_store_error().http_status_code(), 503);
        assert_eq!(
            SchedulerError::HealthCheckFailed("x".into()).http_status_code(),
            503
        );
    }

    #[test]
    fn is_retriable_all_variants() {
        assert!(make_kube_api_error().is_retriable());
        assert!(!SchedulerError::PodNotFound("x".into()).is_retriable());
        assert!(!SchedulerError::PodCreationFailed("x".into()).is_retriable());
        assert!(SchedulerError::Timeout("x".into()).is_retriable());
        assert!(!SchedulerError::InvalidAgentId("x".into()).is_retriable());
        assert!(!SchedulerError::Config("x".into()).is_retriable());
        assert!(!make_store_error().is_retriable());
        assert!(SchedulerError::HealthCheckFailed("x".into()).is_retriable());
    }

    #[test]
    fn display_messages() {
        assert_eq!(
            SchedulerError::PodNotFound("agent-abc".into()).to_string(),
            "Pod not found: agent-abc"
        );
        assert_eq!(
            SchedulerError::Config("bad config".into()).to_string(),
            "Configuration error: bad config"
        );
        assert_eq!(
            SchedulerError::Timeout("30s".into()).to_string(),
            "Timeout waiting for pod: 30s"
        );
    }
}
