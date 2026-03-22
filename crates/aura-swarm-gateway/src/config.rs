//! Gateway configuration types.
//!
//! This module defines configuration structures for the HTTP/WebSocket gateway.

use std::time::Duration;

use serde::Deserialize;

use crate::billing::BillingConfig;

/// Configuration for the gateway service.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    /// Listen address (e.g., "0.0.0.0:8080").
    #[serde(default = "GatewayConfig::default_listen_addr")]
    pub listen_addr: String,

    /// Allowed CORS origins.
    #[serde(default)]
    pub cors_origins: Vec<String>,

    /// Rate limit (requests per second per user).
    #[serde(default = "GatewayConfig::default_rate_limit")]
    pub rate_limit_rps: u32,

    /// WebSocket idle timeout in seconds.
    #[serde(default = "GatewayConfig::default_ws_timeout")]
    pub websocket_timeout_seconds: u64,

    /// Maximum request body size in bytes.
    #[serde(default = "GatewayConfig::default_max_body")]
    pub max_body_bytes: usize,

    /// Request timeout in seconds.
    #[serde(default = "GatewayConfig::default_request_timeout")]
    pub request_timeout_seconds: u64,

    /// Billing configuration for z-billing integration.
    #[serde(default)]
    pub billing: BillingConfig,
}

impl GatewayConfig {
    fn default_listen_addr() -> String {
        "0.0.0.0:8080".to_string()
    }

    const fn default_rate_limit() -> u32 {
        100
    }

    const fn default_ws_timeout() -> u64 {
        300 // 5 minutes
    }

    const fn default_max_body() -> usize {
        1024 * 1024 // 1 MB
    }

    const fn default_request_timeout() -> u64 {
        30
    }

    /// Get the WebSocket timeout as a `Duration`.
    #[must_use]
    pub fn websocket_timeout(&self) -> Duration {
        Duration::from_secs(self.websocket_timeout_seconds)
    }

    /// Get the request timeout as a `Duration`.
    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_seconds)
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen_addr: Self::default_listen_addr(),
            cors_origins: vec!["*".to_string()],
            rate_limit_rps: Self::default_rate_limit(),
            websocket_timeout_seconds: Self::default_ws_timeout(),
            max_body_bytes: Self::default_max_body(),
            request_timeout_seconds: Self::default_request_timeout(),
            billing: BillingConfig::default(),
        }
    }
}

impl GatewayConfig {
    /// Load configuration from environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(val) = std::env::var("LISTEN_ADDR") {
            config.listen_addr = val;
        }
        if let Ok(val) = std::env::var("CORS_ORIGINS") {
            config.cors_origins = val.split(',').map(str::trim).map(String::from).collect();
        }
        if let Ok(val) = std::env::var("RATE_LIMIT_RPS") {
            if let Ok(n) = val.parse() {
                config.rate_limit_rps = n;
            }
        }
        if let Ok(val) = std::env::var("WS_TIMEOUT_SECONDS") {
            if let Ok(n) = val.parse() {
                config.websocket_timeout_seconds = n;
            }
        }
        if let Ok(val) = std::env::var("MAX_BODY_BYTES") {
            if let Ok(n) = val.parse() {
                config.max_body_bytes = n;
            }
        }

        // Load billing config from environment
        config.billing = BillingConfig::from_env();

        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = GatewayConfig::default();
        assert_eq!(config.listen_addr, "0.0.0.0:8080");
        assert_eq!(config.rate_limit_rps, 100);
        assert_eq!(config.websocket_timeout_seconds, 300);
        assert_eq!(config.max_body_bytes, 1024 * 1024);
    }

    #[test]
    fn timeout_duration() {
        let config = GatewayConfig::default();
        assert_eq!(config.websocket_timeout(), Duration::from_secs(300));
        assert_eq!(config.request_timeout(), Duration::from_secs(30));
    }
}
