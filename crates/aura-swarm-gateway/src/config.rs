//! Gateway configuration types.
//!
//! This module defines configuration structures for the HTTP/WebSocket gateway.

use std::time::Duration;

use serde::Deserialize;

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

    /// WebSocket *connect* timeout in seconds.
    ///
    /// This bounds only the initial `connect_async` handshake to the
    /// agent pod (see `handlers::ws` / `handlers::terminal`). It is NOT
    /// an idle timeout on an established stream — long-lived agent
    /// sessions are intentionally not capped here. Keepalive of an
    /// established stream is handled separately via
    /// [`Self::websocket_keepalive`].
    #[serde(default = "GatewayConfig::default_ws_timeout")]
    pub websocket_timeout_seconds: u64,

    /// Interval in seconds between gateway-originated WebSocket keepalive
    /// pings on an established proxy stream. `0` disables keepalive.
    ///
    /// tokio-tungstenite/axum do not send pings on their own, so a quiet
    /// agent session (model thinking, user reading) can be silently
    /// reaped by an intermediary NAT/load balancer. The proxy sends a
    /// periodic Ping toward both the client and the agent to keep the
    /// path warm independent of client behavior.
    #[serde(default = "GatewayConfig::default_ws_keepalive")]
    pub ws_keepalive_seconds: u64,

    /// Maximum request body size in bytes.
    #[serde(default = "GatewayConfig::default_max_body")]
    pub max_body_bytes: usize,

    /// Request timeout in seconds.
    #[serde(default = "GatewayConfig::default_request_timeout")]
    pub request_timeout_seconds: u64,

    /// Bearer token for internal endpoints (scheduler callbacks).
    /// When set, internal endpoints require this token in the Authorization header.
    #[serde(default)]
    pub internal_token: Option<String>,
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

    const fn default_ws_keepalive() -> u64 {
        20 // keep NAT/LB paths warm without flooding the stream
    }

    const fn default_max_body() -> usize {
        1024 * 1024 // 1 MB
    }

    const fn default_request_timeout() -> u64 {
        30
    }

    /// Get the WebSocket connect timeout as a `Duration`.
    #[must_use]
    pub fn websocket_timeout(&self) -> Duration {
        Duration::from_secs(self.websocket_timeout_seconds)
    }

    /// Get the WebSocket keepalive ping interval as a `Duration`, or
    /// `None` when keepalive is disabled (`ws_keepalive_seconds == 0`).
    #[must_use]
    pub fn websocket_keepalive(&self) -> Option<Duration> {
        match self.ws_keepalive_seconds {
            0 => None,
            secs => Some(Duration::from_secs(secs)),
        }
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
            ws_keepalive_seconds: Self::default_ws_keepalive(),
            max_body_bytes: Self::default_max_body(),
            request_timeout_seconds: Self::default_request_timeout(),
            internal_token: std::env::var("INTERNAL_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
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
        if let Ok(val) = std::env::var("WS_KEEPALIVE_SECONDS") {
            if let Ok(n) = val.parse() {
                config.ws_keepalive_seconds = n;
            }
        }
        if let Ok(val) = std::env::var("MAX_BODY_BYTES") {
            if let Ok(n) = val.parse() {
                config.max_body_bytes = n;
            }
        }

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
        assert_eq!(config.ws_keepalive_seconds, 20);
        assert_eq!(config.max_body_bytes, 1024 * 1024);
    }

    #[test]
    fn timeout_duration() {
        let config = GatewayConfig::default();
        assert_eq!(config.websocket_timeout(), Duration::from_secs(300));
        assert_eq!(config.request_timeout(), Duration::from_secs(30));
        assert_eq!(config.websocket_keepalive(), Some(Duration::from_secs(20)));
    }

    #[test]
    fn keepalive_disabled_when_zero() {
        let mut config = GatewayConfig::default();
        config.ws_keepalive_seconds = 0;
        assert_eq!(config.websocket_keepalive(), None);
    }
}
