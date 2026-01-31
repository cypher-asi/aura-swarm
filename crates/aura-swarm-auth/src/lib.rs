//! JWT authentication for aura-swarm.
//!
//! This crate provides JWT validation with Zero-ID integration, including:
//!
//! - JWKS (JSON Web Key Set) fetching and caching
//! - Ed25519 (`EdDSA`) signature validation
//! - Claims extraction and validation
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────┐     ┌──────────────────┐
//! │   Gateway        │────▶│   JwtValidator   │
//! │   (HTTP/WS)      │     │   (trait)        │
//! └──────────────────┘     └────────┬─────────┘
//!                                   │
//!                          ┌────────▼─────────┐
//!                          │  JwksValidator   │
//!                          │  (impl)          │
//!                          └────────┬─────────┘
//!                                   │
//!                          ┌────────▼─────────┐
//!                          │  JwksProvider    │
//!                          │  (key cache)     │
//!                          └────────┬─────────┘
//!                                   │ HTTPS
//!                          ┌────────▼─────────┐
//!                          │   Zero-ID        │
//!                          │   JWKS endpoint  │
//!                          └──────────────────┘
//! ```
//!
//! # Example
//!
//! ```no_run
//! use aura_swarm_auth::{AuthConfig, JwksValidator, JwtValidator};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = AuthConfig {
//!     base_url: "https://auth.zero.tech".to_string(),
//!     audience: "swarm-platform".to_string(),
//!     jwks_refresh_seconds: 300,
//! };
//!
//! let validator = JwksValidator::new(config);
//!
//! // In a request handler:
//! let token = "eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCJ9...";
//! let claims = validator.validate(token).await?;
//!
//! println!("Identity ID: {}", claims.identity_id);
//! println!("Namespace ID: {}", claims.namespace_id);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]

pub mod client;
pub mod error;
pub mod jwks;
pub mod jwt;

pub use client::{LoginRequest, LoginResponse, RefreshRequest, ZidClient};
pub use error::{AuthError, Result};
pub use jwt::{JwksValidator, JwtValidator, ValidatedClaims};

#[cfg(any(test, feature = "test-utils"))]
pub use jwt::MockJwtValidator;

/// Configuration for authentication with Zero-ID.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Base URL for ZID (e.g., `https://auth.zero.tech`).
    pub base_url: String,
    /// Expected JWT audience (`aud` claim).
    pub audience: String,
    /// How often to refresh the JWKS cache, in seconds.
    pub jwks_refresh_seconds: u64,
}

impl AuthConfig {
    /// Get the JWKS endpoint URL.
    #[must_use]
    pub fn jwks_url(&self) -> String {
        format!("{}/.well-known/jwks.json", self.base_url)
    }

    /// Get the email login endpoint URL.
    #[must_use]
    pub fn login_url(&self) -> String {
        format!("{}/v1/auth/login/email", self.base_url)
    }

    /// Get the token refresh endpoint URL.
    #[must_use]
    pub fn refresh_url(&self) -> String {
        format!("{}/v1/auth/refresh", self.base_url)
    }

    /// Get the expected JWT issuer.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.base_url
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            base_url: "https://auth.zero.tech".to_string(),
            audience: "swarm-platform".to_string(),
            jwks_refresh_seconds: 300,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = AuthConfig::default();
        assert_eq!(config.base_url, "https://auth.zero.tech");
        assert_eq!(config.audience, "swarm-platform");
        assert_eq!(config.jwks_refresh_seconds, 300);
    }

    #[test]
    fn config_urls() {
        let config = AuthConfig::default();
        assert_eq!(
            config.jwks_url(),
            "https://auth.zero.tech/.well-known/jwks.json"
        );
        assert_eq!(
            config.login_url(),
            "https://auth.zero.tech/v1/auth/login/email"
        );
        assert_eq!(
            config.refresh_url(),
            "https://auth.zero.tech/v1/auth/refresh"
        );
        assert_eq!(config.issuer(), "https://auth.zero.tech");
    }

    #[test]
    fn auth_error_status_codes() {
        assert_eq!(AuthError::TokenExpired.http_status_code(), 401);
        assert_eq!(AuthError::InvalidSignature.http_status_code(), 401);
        assert_eq!(AuthError::MfaRequired.http_status_code(), 403);
        assert_eq!(AuthError::IdentityFrozen.http_status_code(), 403);
        assert_eq!(AuthError::RateLimited.http_status_code(), 429);
        assert_eq!(
            AuthError::JwksFetchFailed("test".into()).http_status_code(),
            500
        );
    }

    #[test]
    fn auth_error_retriable() {
        assert!(AuthError::TokenExpired.is_retriable());
        assert!(AuthError::JwksFetchFailed("test".into()).is_retriable());
        assert!(AuthError::RateLimited.is_retriable());
        assert!(!AuthError::InvalidSignature.is_retriable());
        assert!(!AuthError::MfaRequired.is_retriable());
    }
}
