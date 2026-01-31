//! ZID authentication client for login and token refresh.
//!
//! This module provides a client for interacting with the Zero-ID authentication API,
//! including email/password login and token refresh functionality.

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use aura_swarm_core::SessionId;

use crate::error::{AuthError, Result};
use crate::AuthConfig;

/// Request payload for email/password login.
#[derive(Debug, Clone, Serialize)]
pub struct LoginRequest {
    /// User's email address.
    pub email: String,
    /// User's password.
    pub password: String,
    /// Optional MFA code if MFA is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mfa_code: Option<String>,
}

/// Response from a successful login or token refresh.
#[derive(Debug, Clone)]
pub struct LoginResponse {
    /// JWT access token.
    pub access_token: String,
    /// Refresh token for obtaining new access tokens.
    pub refresh_token: String,
    /// Session ID for this login session.
    pub session_id: SessionId,
    /// When the access token expires.
    pub expires_at: DateTime<Utc>,
}

/// Request payload for refreshing an access token.
#[derive(Debug, Clone, Serialize)]
pub struct RefreshRequest {
    /// The refresh token obtained from login.
    pub refresh_token: String,
    /// The current session ID.
    pub session_id: SessionId,
    /// Machine identifier for device tracking.
    pub machine_id: String,
}

/// Raw response from ZID login/refresh endpoints.
#[derive(Debug, Deserialize)]
struct RawLoginResponse {
    access_token: String,
    refresh_token: String,
    session_id: String,
    expires_in: u64,
}

/// Error response from ZID API.
#[derive(Debug, Deserialize)]
struct ZidErrorResponse {
    code: String,
    #[allow(dead_code)]
    message: Option<String>,
}

/// Client for interacting with Zero-ID authentication API.
pub struct ZidClient {
    config: AuthConfig,
    client: reqwest::Client,
}

impl ZidClient {
    /// Create a new ZID client with the given configuration.
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client cannot be created (should never happen with default TLS).
    #[must_use]
    pub fn new(config: AuthConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to create HTTP client");

        Self { config, client }
    }

    /// Authenticate with email and password.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The credentials are invalid (`LoginFailed`)
    /// - MFA is required but not provided (`MfaRequired`)
    /// - The identity is frozen (`IdentityFrozen`)
    /// - Rate limit is exceeded (`RateLimited`)
    /// - Network or server error occurs
    pub async fn login(&self, req: LoginRequest) -> Result<LoginResponse> {
        let url = self.config.login_url();

        let response = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| AuthError::Internal(format!("request failed: {e}")))?;

        self.handle_response(response).await
    }

    /// Refresh an access token using a refresh token.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The refresh token is invalid or expired (`LoginFailed`)
    /// - The session has been invalidated (`LoginFailed`)
    /// - Rate limit is exceeded (`RateLimited`)
    /// - Network or server error occurs
    pub async fn refresh(&self, req: RefreshRequest) -> Result<LoginResponse> {
        let url = self.config.refresh_url();

        let response = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| AuthError::Internal(format!("request failed: {e}")))?;

        self.handle_response(response).await
    }

    /// Handle the HTTP response and convert to `LoginResponse`.
    async fn handle_response(&self, response: reqwest::Response) -> Result<LoginResponse> {
        let status = response.status();

        if status.is_success() {
            let raw: RawLoginResponse = response
                .json()
                .await
                .map_err(|e| AuthError::Internal(format!("invalid response: {e}")))?;

            let session_id = SessionId::from_str(&raw.session_id)
                .map_err(|_| AuthError::Internal("invalid session_id in response".to_string()))?;

            let expires_in_secs = i64::try_from(raw.expires_in).unwrap_or(i64::MAX);
            let expires_at = Utc::now() + chrono::Duration::seconds(expires_in_secs);

            return Ok(LoginResponse {
                access_token: raw.access_token,
                refresh_token: raw.refresh_token,
                session_id,
                expires_at,
            });
        }

        // Try to parse error response
        let error_response: Option<ZidErrorResponse> = response.json().await.ok();

        match error_response {
            Some(err) => match err.code.as_str() {
                "UNAUTHORIZED" => Err(AuthError::LoginFailed("invalid credentials".to_string())),
                "MFA_REQUIRED" => Err(AuthError::MfaRequired),
                "IDENTITY_FROZEN" => Err(AuthError::IdentityFrozen),
                "RATE_LIMITED" => Err(AuthError::RateLimited),
                code => Err(AuthError::LoginFailed(format!("error code: {code}"))),
            },
            None => {
                // Fallback based on status code
                match status.as_u16() {
                    401 => Err(AuthError::LoginFailed("unauthorized".to_string())),
                    403 => Err(AuthError::IdentityFrozen),
                    429 => Err(AuthError::RateLimited),
                    _ => Err(AuthError::Internal(format!("HTTP {status}"))),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_request_serializes() {
        let req = LoginRequest {
            email: "user@example.com".to_string(),
            password: "secret".to_string(),
            mfa_code: None,
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("email"));
        assert!(json.contains("password"));
        // mfa_code should be omitted when None
        assert!(!json.contains("mfa_code"));
    }

    #[test]
    fn login_request_with_mfa_serializes() {
        let req = LoginRequest {
            email: "user@example.com".to_string(),
            password: "secret".to_string(),
            mfa_code: Some("123456".to_string()),
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("mfa_code"));
        assert!(json.contains("123456"));
    }

    #[test]
    fn refresh_request_serializes() {
        let req = RefreshRequest {
            refresh_token: "token123".to_string(),
            session_id: SessionId::generate(),
            machine_id: "machine-abc".to_string(),
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("refresh_token"));
        assert!(json.contains("session_id"));
        assert!(json.contains("machine_id"));
    }

    #[test]
    fn client_creation() {
        let config = AuthConfig::default();
        let _client = ZidClient::new(config);
        // Just verify it doesn't panic
    }
}
