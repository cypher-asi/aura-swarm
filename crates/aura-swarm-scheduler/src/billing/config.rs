//! Billing configuration for the scheduler.

use serde::{Deserialize, Serialize};

/// Configuration for scheduler billing integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerBillingConfig {
    /// URL of the zbilling service.
    pub url: String,
    /// API key for service authentication.
    pub api_key: String,
    /// Whether billing is enabled.
    pub enabled: bool,
    /// Interval for compute usage reports in seconds.
    pub report_interval_seconds: u64,
    /// Whether to block operations when billing is unavailable.
    pub fail_closed: bool,
}

impl Default for SchedulerBillingConfig {
    fn default() -> Self {
        Self {
            url: "https://z-billing.onrender.com".to_string(),
            api_key: String::new(),
            enabled: true,
            report_interval_seconds: 300, // 5 minutes
            fail_closed: false,
        }
    }
}

impl SchedulerBillingConfig {
    /// Load configuration from environment variables.
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
        if let Ok(val) = std::env::var("Z_BILLING_REPORT_INTERVAL") {
            if let Ok(n) = val.parse() {
                config.report_interval_seconds = n;
            }
        }
        if let Ok(val) = std::env::var("Z_BILLING_FAIL_CLOSED") {
            config.fail_closed = val.parse().unwrap_or(false);
        }

        config
    }

    /// Check if billing is properly configured.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.enabled && !self.api_key.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = SchedulerBillingConfig::default();
        assert_eq!(config.report_interval_seconds, 300);
        assert!(!config.fail_closed);
    }

    #[test]
    fn is_configured_requires_api_key() {
        let mut config = SchedulerBillingConfig::default();
        assert!(!config.is_configured());

        config.api_key = "test-key".to_string();
        assert!(config.is_configured());
    }
}
