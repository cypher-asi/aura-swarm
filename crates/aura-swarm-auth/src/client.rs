//! zOS authentication client for login and user info.
//!
//! This module provides a client for interacting with the zOS authentication API,
//! including email/password login and user info retrieval.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{AuthError, Result};
use crate::AuthConfig;

/// Request payload for email/password login.
#[derive(Debug, Clone, Serialize)]
pub struct ZosLoginRequest {
    /// User's email address.
    pub email: String,
    /// User's password.
    pub password: String,
}

/// Response from a successful login.
#[derive(Debug, Clone)]
pub struct ZosLoginResponse {
    /// JWT access token.
    pub access_token: String,
}

/// Raw response from the zOS login endpoint.
///
/// zOS returns camelCase JSON (`accessToken`, `identityToken`).
#[derive(Debug, Deserialize)]
struct RawZosLoginResponse {
    #[serde(rename = "accessToken")]
    access_token: String,
}

/// Error response from the zOS API.
#[derive(Debug, Deserialize)]
struct ZosErrorResponse {
    code: Option<String>,
    message: Option<String>,
}

/// Client for interacting with the zOS authentication API.
pub struct ZosClient {
    config: AuthConfig,
    client: reqwest::Client,
}

impl ZosClient {
    /// Create a new zOS client with the given configuration.
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
    /// Returns `AuthError::ZosApi` if the zOS API returns an error, or
    /// `AuthError::Internal` on network/parsing failures.
    pub async fn login(&self, email: &str, password: &str) -> Result<ZosLoginResponse> {
        let url = self.config.login_url();

        let body = ZosLoginRequest {
            email: email.to_owned(),
            password: password.to_owned(),
        };

        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AuthError::Internal(format!("request failed: {e}")))?;

        let status = response.status();

        if status.is_success() {
            let raw: RawZosLoginResponse = response
                .json()
                .await
                .map_err(|e| AuthError::Internal(format!("invalid response: {e}")))?;

            return Ok(ZosLoginResponse {
                access_token: raw.access_token,
            });
        }

        Err(self.map_error_response(status.as_u16(), response).await)
    }

    /// Fetch information about the currently authenticated user.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::ZosApi` if the zOS API returns an error, or
    /// `AuthError::Internal` on network/parsing failures.
    pub async fn fetch_user_info(&self, token: &str) -> Result<serde_json::Value> {
        let url = self.config.user_info_url();

        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| AuthError::Internal(format!("request failed: {e}")))?;

        let status = response.status();

        if status.is_success() {
            let value: serde_json::Value = response
                .json()
                .await
                .map_err(|e| AuthError::Internal(format!("invalid response: {e}")))?;

            return Ok(value);
        }

        Err(self.map_error_response(status.as_u16(), response).await)
    }

    async fn map_error_response(
        &self,
        status: u16,
        response: reqwest::Response,
    ) -> AuthError {
        let error_body: Option<ZosErrorResponse> = response.json().await.ok();
        let code = error_body
            .as_ref()
            .and_then(|e| e.code.clone())
            .unwrap_or_else(|| "UNKNOWN".to_string());
        let message = error_body
            .as_ref()
            .and_then(|e| e.message.clone())
            .unwrap_or_else(|| format!("HTTP {status}"));

        AuthError::ZosApi {
            status,
            code,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_request_serializes() {
        let req = ZosLoginRequest {
            email: "user@example.com".to_string(),
            password: "secret".to_string(),
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("email"));
        assert!(json.contains("password"));
    }

    #[test]
    fn client_creation() {
        let config = AuthConfig::default();
        let _client = ZosClient::new(config);
    }
}
