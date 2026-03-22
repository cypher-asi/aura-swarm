//! Billing configuration for the gateway.

use serde::Deserialize;

/// Configuration for z-billing integration.
#[derive(Debug, Clone, Deserialize)]
pub struct BillingConfig {
    /// URL of the z-billing service.
    #[serde(default = "BillingConfig::default_url")]
    pub url: String,

    /// API key for service authentication.
    #[serde(default)]
    pub api_key: String,

    /// Whether billing is enabled.
    #[serde(default = "BillingConfig::default_enabled")]
    pub enabled: bool,

    /// Whether to block operations when billing is unavailable.
    /// If false, operations continue with a warning log.
    #[serde(default)]
    pub fail_closed: bool,

    /// Cache TTL for account existence checks in seconds.
    #[serde(default = "BillingConfig::default_cache_ttl")]
    pub account_cache_ttl_seconds: u64,
}

impl BillingConfig {
    fn default_url() -> String {
        "http://z-billing:8080".to_string()
    }

    const fn default_enabled() -> bool {
        true
    }

    const fn default_cache_ttl() -> u64 {
        300 // 5 minutes
    }

    /// Load configuration from environment variables.
    ///
    /// Supported variables:
    /// - `Z_BILLING_URL`: Service URL
    /// - `Z_BILLING_API_KEY`: Service API key
    /// - `Z_BILLING_ENABLED`: Whether billing is enabled (default: true)
    /// - `Z_BILLING_FAIL_CLOSED`: Block on billing errors (default: false)
    /// - `Z_BILLING_CACHE_TTL`: Account cache TTL in seconds (default: 300)
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(val) = std::env::var("Z_BILLING_URL") {
            config.url = val;
        }
        if let Ok(val) = std::env::var("Z_BILLING_API_KEY") {
            config.api_key = val;
        }
        if let Ok(val) = std::env::var("Z_BILLING_ENABLED") {
            config.enabled = val.parse().unwrap_or(true);
        }
        if let Ok(val) = std::env::var("Z_BILLING_FAIL_CLOSED") {
            config.fail_closed = val.parse().unwrap_or(false);
        }
        if let Ok(val) = std::env::var("Z_BILLING_CACHE_TTL") {
            if let Ok(n) = val.parse() {
                config.account_cache_ttl_seconds = n;
            }
        }

        config
    }

    /// Check if billing is properly configured.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.enabled && !self.api_key.is_empty()
    }
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            url: Self::default_url(),
            api_key: String::new(),
            enabled: Self::default_enabled(),
            fail_closed: false,
            account_cache_ttl_seconds: Self::default_cache_ttl(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = BillingConfig::default();
        assert_eq!(config.url, "http://z-billing:8080");
        assert!(config.enabled);
        assert!(!config.fail_closed);
        assert_eq!(config.account_cache_ttl_seconds, 300);
    }

    #[test]
    fn is_configured_requires_api_key() {
        let mut config = BillingConfig::default();
        assert!(!config.is_configured());

        config.api_key = "test-key".to_string();
        assert!(config.is_configured());

        config.enabled = false;
        assert!(!config.is_configured());
    }
}
