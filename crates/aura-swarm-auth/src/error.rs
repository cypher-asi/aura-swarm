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

    /// The identity ID in the token is malformed.
    #[error("invalid identity ID format")]
    InvalidIdentityId,

    /// The namespace ID in the token is malformed.
    #[error("invalid namespace ID format")]
    InvalidNamespaceId,

    /// The session ID in the token is malformed.
    #[error("invalid session ID format")]
    InvalidSessionId,

    /// MFA is required to complete authentication.
    #[error("MFA required")]
    MfaRequired,

    /// The identity is frozen and cannot authenticate.
    #[error("identity frozen")]
    IdentityFrozen,

    /// Too many authentication attempts, rate limited.
    #[error("rate limited")]
    RateLimited,

    /// Login failed with a specific reason.
    #[error("login failed: {0}")]
    LoginFailed(String),

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

    /// An internal error occurred.
    #[error("internal error: {0}")]
    Internal(String),
}

impl AuthError {
    /// Returns `true` if this error indicates the client should retry with a new token.
    #[must_use]
    pub const fn is_retriable(&self) -> bool {
        matches!(
            self,
            Self::TokenExpired | Self::JwksFetchFailed(_) | Self::RateLimited
        )
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
            | Self::InvalidIdentityId
            | Self::InvalidNamespaceId
            | Self::InvalidSessionId
            | Self::MissingClaim(_)
            | Self::InvalidToken(_)
            | Self::LoginFailed(_) => 401,
            Self::MfaRequired | Self::IdentityFrozen => 403,
            Self::RateLimited => 429,
            Self::KeyNotFound(_) | Self::JwksFetchFailed(_) | Self::Internal(_) => 500,
        }
    }
}
