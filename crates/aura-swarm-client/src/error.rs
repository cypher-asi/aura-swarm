//! Error types for the SwarmClient.

use crate::types::ApiErrorResponse;

/// Error type for `SwarmClient` operations.
#[derive(Debug, thiserror::Error)]
pub enum SwarmClientError {
    /// HTTP transport failure (connection, timeout, TLS, etc.).
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// The gateway returned a structured API error.
    #[error("API error ({status}): {message}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Error code from the gateway (e.g. "not_found").
        code: String,
        /// Human-readable message.
        message: String,
    },

    /// Failed to deserialize the response body.
    #[error("Failed to parse response: {0}")]
    Parse(String),
}

impl SwarmClientError {
    /// Build an `Api` variant from a gateway error response.
    pub(crate) fn from_api_response(status: u16, resp: ApiErrorResponse) -> Self {
        Self::Api {
            status,
            code: resp.error.code,
            message: resp.error.message,
        }
    }

    /// Build an `Api` variant when the error body couldn't be parsed.
    pub(crate) fn from_status(status: u16) -> Self {
        Self::Api {
            status,
            code: String::new(),
            message: "Unknown error".to_string(),
        }
    }

    /// Whether this error represents a 404 Not Found.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Api { status: 404, .. })
    }

    /// Whether this error represents an authentication failure.
    #[must_use]
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Api { status: 401, .. })
    }
}
