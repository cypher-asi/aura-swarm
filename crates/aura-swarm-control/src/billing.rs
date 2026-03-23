//! Billing integration for the control plane.
//!
//! Provides credit balance checks before agent and session creation.

use std::sync::Arc;

use z_billing_client::{ClientOptions, ZBillingClient};

/// Configuration for billing integration.
#[derive(Debug, Clone)]
pub struct BillingConfig {
    /// URL of the z-billing service.
    pub url: String,
    /// API key for service authentication.
    pub api_key: String,
    /// Whether billing is enabled.
    pub enabled: bool,
    /// Minimum credit balance required for agent creation (in cents).
    pub min_credits_for_agent: i64,
    /// Minimum credit balance required for session creation (in cents).
    pub min_credits_for_session: i64,
    /// Whether to block operations when billing is unavailable.
    pub fail_closed: bool,
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            url: "http://z-billing:8080".to_string(),
            api_key: String::new(),
            enabled: true,
            min_credits_for_agent: 100,  // $1.00 minimum
            min_credits_for_session: 10, // $0.10 minimum
            fail_closed: false,
        }
    }
}

impl BillingConfig {
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
        if let Ok(val) = std::env::var("Z_BILLING_MIN_CREDITS") {
            if let Ok(n) = val.parse() {
                config.min_credits_for_agent = n;
            }
        }
        if let Ok(val) = std::env::var("Z_BILLING_MIN_SESSION_CREDITS") {
            if let Ok(n) = val.parse() {
                config.min_credits_for_session = n;
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

/// Billing checker for credit balance verification.
pub struct BillingChecker {
    client: ZBillingClient,
    config: BillingConfig,
}

impl BillingChecker {
    /// Create a new billing checker from configuration.
    ///
    /// # Errors
    ///
    /// Returns `BillingCheckError::ServiceError` if the HTTP client cannot be created.
    pub fn new(config: BillingConfig) -> Result<Self, BillingCheckError> {
        let options = ClientOptions::with_service_name("aura-control");
        let client = ZBillingClient::with_options(&config.url, &config.api_key, options)
            .map_err(|e| BillingCheckError::ServiceError(format!("failed to create billing client: {e}")))?;
        Ok(Self { client, config })
    }

    /// Create a billing checker wrapped in an Arc.
    ///
    /// # Errors
    ///
    /// Returns `BillingCheckError::ServiceError` if the HTTP client cannot be created.
    pub fn new_shared(config: BillingConfig) -> Result<Arc<Self>, BillingCheckError> {
        Ok(Arc::new(Self::new(config)?))
    }

    /// Check if billing is enabled and configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.is_configured()
    }

    /// Check if the user has sufficient credits for agent creation.
    ///
    /// # Errors
    ///
    /// Returns error if user has insufficient credits or billing service fails.
    pub async fn check_agent_credits(&self, user_id: &str) -> Result<(), BillingCheckError> {
        self.check_balance(user_id, self.config.min_credits_for_agent)
            .await
    }

    /// Check if the user has sufficient credits for session creation.
    ///
    /// # Errors
    ///
    /// Returns error if user has insufficient credits or billing service fails.
    pub async fn check_session_credits(&self, user_id: &str) -> Result<(), BillingCheckError> {
        self.check_balance(user_id, self.config.min_credits_for_session)
            .await
    }

    /// Check if the user has sufficient credits.
    async fn check_balance(&self, user_id: &str, required: i64) -> Result<(), BillingCheckError> {
        if !self.is_enabled() {
            return Ok(());
        }

        match self.client.check_balance(user_id, required).await {
            Ok(response) => {
                if response.sufficient {
                    Ok(())
                } else {
                    Err(BillingCheckError::InsufficientCredits {
                        balance: response.balance_cents,
                        required: response.required_cents,
                    })
                }
            }
            Err(z_billing_client::ClientError::InsufficientCredits { balance, required }) => {
                Err(BillingCheckError::InsufficientCredits { balance, required })
            }
            Err(z_billing_client::ClientError::AccountNotFound { user_id }) => {
                Err(BillingCheckError::AccountNotFound { user_id })
            }
            Err(e) if !self.config.fail_closed => {
                tracing::warn!(
                    user_id = %user_id,
                    error = %e,
                    "Failed to check billing balance, continuing (fail_closed=false)"
                );
                Ok(())
            }
            Err(e) => Err(BillingCheckError::ServiceError(e.to_string())),
        }
    }
}

/// Errors from billing checks.
#[derive(Debug, thiserror::Error)]
pub enum BillingCheckError {
    /// User has insufficient credits.
    #[error("insufficient credits: balance={balance} cents, required={required} cents")]
    InsufficientCredits {
        /// Current balance in cents.
        balance: i64,
        /// Required amount in cents.
        required: i64,
    },

    /// User's billing account was not found.
    #[error("billing account not found: {user_id}")]
    AccountNotFound {
        /// The user ID.
        user_id: String,
    },

    /// Billing service error.
    #[error("billing service error: {0}")]
    ServiceError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = BillingConfig::default();
        assert_eq!(config.min_credits_for_agent, 100);
        assert_eq!(config.min_credits_for_session, 10);
        assert!(!config.fail_closed);
    }

    #[test]
    fn is_configured_requires_api_key() {
        let mut config = BillingConfig::default();
        assert!(!config.is_configured());

        config.api_key = "test-key".to_string();
        assert!(config.is_configured());
    }
}
