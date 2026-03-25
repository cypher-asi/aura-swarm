//! API error types and responses.
//!
//! This module defines the standard error format for all API responses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use thiserror::Error;

use aura_swarm_auth::AuthError;
use aura_swarm_control::ControlError;

/// API error type that implements `IntoResponse`.
#[derive(Debug, Error)]
pub enum ApiError {
    /// Missing or invalid authentication token.
    #[error("unauthorized")]
    Unauthorized,

    /// User does not have permission to access this resource.
    #[error("forbidden")]
    Forbidden,

    /// The requested resource was not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// The request conflicts with the current state.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Too many requests, rate limit exceeded.
    #[error("rate limited")]
    RateLimited,

    /// Invalid request body or parameters.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Internal server error.
    #[error("internal error: {0}")]
    Internal(String),

    /// Agent pod is not reachable.
    #[error("agent unavailable")]
    AgentUnavailable,

    /// Insufficient credits for the operation.
    #[error("insufficient credits: balance={balance} cents, required={required} cents")]
    InsufficientCredits {
        /// Current balance in cents.
        balance: i64,
        /// Required amount in cents.
        required: i64,
    },

    /// Billing account not found.
    #[error("billing account not found")]
    BillingAccountNotFound,
}

/// Error response body.
#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: ErrorBody,
}

/// Error details.
#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

impl ApiError {
    /// Get the HTTP status code for this error.
    #[must_use]
    pub const fn status_code(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::AgentUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::InsufficientCredits { .. } | Self::BillingAccountNotFound => {
                StatusCode::PAYMENT_REQUIRED
            }
        }
    }

    /// Get the error code string for this error.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::RateLimited => "rate_limited",
            Self::BadRequest(_) => "bad_request",
            Self::Internal(_) => "internal_error",
            Self::AgentUnavailable => "agent_unavailable",
            Self::InsufficientCredits { .. } => "insufficient_credits",
            Self::BillingAccountNotFound => "billing_account_not_found",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let code = self.code();
        let message = self.to_string();

        let body = ErrorResponse {
            error: ErrorBody { code, message },
        };

        (status, Json(body)).into_response()
    }
}

impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::TokenExpired
            | AuthError::InvalidSignature
            | AuthError::InvalidIssuer
            | AuthError::InvalidAudience
            | AuthError::InvalidUserId
            | AuthError::MissingClaim(_)
            | AuthError::InvalidToken(_) => Self::Unauthorized,
            AuthError::ZosApi { status, .. } => match status {
                401 => Self::Unauthorized,
                403 => Self::Forbidden,
                429 => Self::RateLimited,
                _ => Self::Internal("authentication service error".to_string()),
            },
            AuthError::KeyNotFound(_) | AuthError::JwksFetchFailed(_) | AuthError::Internal(_) => {
                tracing::error!(error = %err, "Auth internal error");
                Self::Internal("authentication service error".to_string())
            }
        }
    }
}

impl From<ControlError> for ApiError {
    fn from(err: ControlError) -> Self {
        match err {
            ControlError::AgentNotFound(id) => Self::NotFound(format!("agent {id}")),
            ControlError::AgentAlreadyExists { agent_id } => {
                Self::Conflict(format!("agent {agent_id} already exists"))
            }
            ControlError::SessionNotFound(id) => Self::NotFound(format!("session {id}")),
            ControlError::QuotaExceeded { limit, .. } => {
                Self::Conflict(format!("agent quota exceeded: limit is {limit}"))
            }
            ControlError::NotOwner { .. } => Self::Forbidden,
            ControlError::InvalidState { from, to, .. } => {
                Self::Conflict(format!("cannot transition from {from:?} to {to:?}"))
            }
            ControlError::AgentNotRunnable(id) => {
                Self::Conflict(format!("agent {id} is not in a runnable state"))
            }
            ControlError::SessionAlreadyActive(id) => {
                Self::Conflict(format!("agent {id} already has an active session"))
            }
            ControlError::InsufficientCredits { balance, required } => {
                Self::InsufficientCredits { balance, required }
            }
            ControlError::BillingAccountNotFound => Self::BillingAccountNotFound,
            ControlError::Auth(auth_err) => Self::from(auth_err),
            ControlError::Store(store_err) => {
                tracing::error!(error = %store_err, "Store error");
                Self::Internal("storage error".to_string())
            }
            ControlError::Internal(msg) => {
                tracing::error!(error = %msg, "Internal error");
                Self::Internal(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_status_codes() {
        assert_eq!(
            ApiError::Unauthorized.status_code(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(ApiError::Forbidden.status_code(), StatusCode::FORBIDDEN);
        assert_eq!(
            ApiError::NotFound("test".into()).status_code(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::Conflict("test".into()).status_code(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            ApiError::RateLimited.status_code(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            ApiError::Internal("test".into()).status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            ApiError::AgentUnavailable.status_code(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn error_codes() {
        assert_eq!(ApiError::Unauthorized.code(), "unauthorized");
        assert_eq!(ApiError::Forbidden.code(), "forbidden");
        assert_eq!(ApiError::NotFound("test".into()).code(), "not_found");
        assert_eq!(ApiError::RateLimited.code(), "rate_limited");
    }
}
