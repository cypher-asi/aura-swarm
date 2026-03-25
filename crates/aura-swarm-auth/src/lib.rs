//! JWT authentication for aura-swarm.
//!
//! This crate provides JWT validation with zOS integration, including:
//!
//! - JWKS (JSON Web Key Set) fetching and caching
//! - EdDSA and RS256 signature validation
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
//!                          │   zOS            │
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
//!     base_url: "https://zosapi.zero.tech".to_string(),
//!     audience: None,
//!     jwks_refresh_seconds: 300,
//! };
//!
//! let validator = JwksValidator::new(config)?;
//!
//! // In a request handler:
//! let token = "eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCJ9...";
//! let claims = validator.validate(token).await?;
//!
//! println!("User ID: {}", claims.user_id);
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

pub use client::{ZosClient, ZosLoginRequest, ZosLoginResponse};
pub use error::{AuthError, Result};
pub use jwt::{JwksValidator, JwtValidator, ValidatedClaims, ZosTokenValidator};

#[cfg(any(test, feature = "test-utils"))]
pub use jwt::MockJwtValidator;

/// Configuration for authentication with zOS.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Base URL for the zOS API (e.g., `https://zosapi.zero.tech`).
    pub base_url: String,
    /// Expected JWT audience (`aud` claim). If `None`, audience is not validated.
    pub audience: Option<String>,
    /// How often to refresh the JWKS cache, in seconds.
    pub jwks_refresh_seconds: u64,
}

impl AuthConfig {
    /// Get the JWKS endpoint URL.
    #[must_use]
    pub fn jwks_url(&self) -> String {
        format!("{}/.well-known/jwks.json", self.base_url)
    }

    /// Get the login endpoint URL.
    #[must_use]
    pub fn login_url(&self) -> String {
        format!("{}/api/v2/accounts/login", self.base_url)
    }

    /// Get the user info endpoint URL.
    #[must_use]
    pub fn user_info_url(&self) -> String {
        format!("{}/api/users/current", self.base_url)
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
            base_url: "https://zosapi.zero.tech".to_string(),
            audience: None,
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
        assert_eq!(config.base_url, "https://zosapi.zero.tech");
        assert!(config.audience.is_none());
        assert_eq!(config.jwks_refresh_seconds, 300);
    }

    #[test]
    fn config_urls() {
        let config = AuthConfig::default();
        assert_eq!(
            config.jwks_url(),
            "https://zosapi.zero.tech/.well-known/jwks.json"
        );
        assert_eq!(
            config.login_url(),
            "https://zosapi.zero.tech/api/v2/accounts/login"
        );
        assert_eq!(
            config.user_info_url(),
            "https://zosapi.zero.tech/api/users/current"
        );
        assert_eq!(config.issuer(), "https://zosapi.zero.tech");
    }

    #[test]
    fn auth_error_status_codes() {
        assert_eq!(AuthError::TokenExpired.http_status_code(), 401);
        assert_eq!(AuthError::InvalidSignature.http_status_code(), 401);
        assert_eq!(
            AuthError::ZosApi {
                status: 403,
                code: "FORBIDDEN".into(),
                message: "forbidden".into()
            }
            .http_status_code(),
            403
        );
        assert_eq!(
            AuthError::ZosApi {
                status: 429,
                code: "RATE_LIMITED".into(),
                message: "too many requests".into()
            }
            .http_status_code(),
            429
        );
        assert_eq!(
            AuthError::JwksFetchFailed("test".into()).http_status_code(),
            500
        );
    }

    #[test]
    fn auth_error_retriable() {
        assert!(AuthError::TokenExpired.is_retriable());
        assert!(AuthError::JwksFetchFailed("test".into()).is_retriable());
        assert!(!AuthError::InvalidSignature.is_retriable());
        assert!(!AuthError::ZosApi {
            status: 429,
            code: "RATE_LIMITED".into(),
            message: "too many requests".into()
        }
        .is_retriable());
    }
}
