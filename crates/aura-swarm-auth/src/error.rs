//! Authentication error types.

use thiserror::Error;

/// A result type using `AuthError`.
pub type Result<T> = std::result::Result<T, AuthError>;

/// Errors that can occur during authentication.
#[derive(Debug, Error)]
pub enum AuthError {
    /// The JWT has expired.
    #[error("token expired")]
    TokenExpired,

    /// The JWT signature is invalid.
    #[error("invalid signature")]
    InvalidSignature,

    /// The JWT issuer does not match the expected value.
    #[error("invalid issuer")]
    InvalidIssuer,

    /// The JWT audience does not match the expected value.
    #[error("invalid audience")]
    InvalidAudience,

    /// The user ID in the token is malformed.
    #[error("invalid user ID format")]
    InvalidUserId,

    /// A required claim is missing from the token.
    #[error("missing required claim: {0}")]
    MissingClaim(String),

    /// Failed to fetch JWKS from the authentication server.
    #[error("JWKS fetch failed: {0}")]
    JwksFetchFailed(String),

    /// The key ID specified in the token was not found.
    #[error("key not found: {0}")]
    KeyNotFound(String),

    /// The token format is invalid.
    #[error("invalid token format: {0}")]
    InvalidToken(String),

    /// An error returned by the zOS API.
    #[error("zOS API error ({status}): [{code}] {message}")]
    ZosApi {
        /// HTTP status code from the zOS API.
        status: u16,
        /// Error code from the response body.
        code: String,
        /// Human-readable error message.
        message: String,
    },

    /// An internal error occurred.
    #[error("internal error: {0}")]
    Internal(String),
}

impl AuthError {
    /// Returns `true` if this error indicates the client should retry with a new token.
    #[must_use]
    pub const fn is_retriable(&self) -> bool {
        matches!(self, Self::TokenExpired | Self::JwksFetchFailed(_))
    }

    /// Returns the appropriate HTTP status code for this error.
    #[must_use]
    pub const fn http_status_code(&self) -> u16 {
        match self {
            Self::TokenExpired
            | Self::InvalidSignature
            | Self::InvalidIssuer
            | Self::InvalidAudience
            | Self::InvalidUserId
            | Self::MissingClaim(_)
            | Self::InvalidToken(_) => 401,
            Self::ZosApi { status, .. } => *status,
            Self::KeyNotFound(_) | Self::JwksFetchFailed(_) | Self::Internal(_) => 500,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_error_variants_status_codes() {
        assert_eq!(AuthError::TokenExpired.http_status_code(), 401);
        assert_eq!(AuthError::InvalidSignature.http_status_code(), 401);
        assert_eq!(AuthError::InvalidIssuer.http_status_code(), 401);
        assert_eq!(AuthError::InvalidAudience.http_status_code(), 401);
        assert_eq!(AuthError::InvalidUserId.http_status_code(), 401);
        assert_eq!(AuthError::MissingClaim("x".into()).http_status_code(), 401);
        assert_eq!(AuthError::InvalidToken("x".into()).http_status_code(), 401);
        assert_eq!(
            AuthError::ZosApi {
                status: 403,
                code: "forbidden".into(),
                message: "no access".into(),
            }
            .http_status_code(),
            403
        );
        assert_eq!(AuthError::KeyNotFound("k".into()).http_status_code(), 500);
        assert_eq!(
            AuthError::JwksFetchFailed("f".into()).http_status_code(),
            500
        );
        assert_eq!(AuthError::Internal("i".into()).http_status_code(), 500);
    }

    #[test]
    fn all_error_variants_retriable() {
        assert!(AuthError::TokenExpired.is_retriable());
        assert!(AuthError::JwksFetchFailed("fail".into()).is_retriable());

        assert!(!AuthError::InvalidSignature.is_retriable());
        assert!(!AuthError::InvalidIssuer.is_retriable());
        assert!(!AuthError::InvalidAudience.is_retriable());
        assert!(!AuthError::InvalidUserId.is_retriable());
        assert!(!AuthError::MissingClaim("x".into()).is_retriable());
        assert!(!AuthError::InvalidToken("x".into()).is_retriable());
        assert!(!AuthError::KeyNotFound("k".into()).is_retriable());
        assert!(!AuthError::Internal("i".into()).is_retriable());
        assert!(!AuthError::ZosApi {
            status: 500,
            code: "err".into(),
            message: "msg".into(),
        }
        .is_retriable());
    }
}
