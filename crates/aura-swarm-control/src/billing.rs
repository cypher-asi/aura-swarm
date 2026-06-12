//! Billing integration for the control plane.
//!
//! Provides credit balance checks before agent and session creation.
//! Calls the external zbilling service via its REST API.

use std::sync::Arc;

use aura_swarm_store::BoxTier;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Configuration for billing integration.
#[derive(Debug, Clone)]
pub struct BillingConfig {
    /// URL of the zbilling service.
    pub url: String,
    /// API key for service authentication.
    pub api_key: String,
    /// Whether billing is enabled.
    pub enabled: bool,
    /// Legacy flat minimum credit balance for agent creation (in cents).
    /// Tier-based creates use `agent_runway_hours` x the tier's hourly
    /// price instead; this floor only backs the legacy flat check.
    pub min_credits_for_agent: i64,
    /// Minimum credit balance required for session creation (in cents).
    pub min_credits_for_session: i64,
    /// Hours of runway (at the tier's hourly rate) a user must be able to
    /// afford before an agent is created on that tier.
    pub agent_runway_hours: i64,
    /// Whether to block operations when billing is unavailable.
    pub fail_closed: bool,
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            url: "https://z-billing.onrender.com".to_string(),
            api_key: String::new(),
            enabled: true,
            min_credits_for_agent: 100,  // $1.00 minimum
            min_credits_for_session: 10, // $0.10 minimum
            agent_runway_hours: 2,
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
        if let Ok(val) = std::env::var("Z_BILLING_AGENT_RUNWAY_HOURS") {
            if let Ok(n) = val.parse() {
                config.agent_runway_hours = n;
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

#[derive(Serialize)]
struct BalanceCheckRequest<'a> {
    user_id: &'a str,
    required_cents: i64,
}

#[derive(Deserialize)]
struct BalanceCheckResponse {
    sufficient: bool,
    balance_cents: i64,
    required_cents: i64,
}

/// Billing checker for credit balance verification.
pub struct BillingChecker {
    http: Client,
    config: BillingConfig,
}

impl BillingChecker {
    /// Create a new billing checker from configuration.
    ///
    /// # Errors
    ///
    /// Returns `BillingCheckError::ServiceError` if the HTTP client cannot be created.
    pub fn new(config: BillingConfig) -> Result<Self, BillingCheckError> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| {
                BillingCheckError::ServiceError(format!("failed to create HTTP client: {e}"))
            })?;
        Ok(Self { http, config })
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

    /// Check if the user has sufficient credits for agent creation
    /// (legacy flat-amount check).
    ///
    /// Prefer [`Self::check_agent_credits_for_tier`], which scales with the
    /// tier's hourly price.
    ///
    /// # Errors
    ///
    /// Returns error if user has insufficient credits or billing service fails.
    pub async fn check_agent_credits(&self, user_id: &str) -> Result<(), BillingCheckError> {
        self.check_balance(user_id, self.config.min_credits_for_agent)
            .await
    }

    /// Check if the user can afford the configured runway (default ~2 hours)
    /// at the tier's hourly rate before creating an agent on that tier.
    ///
    /// # Errors
    ///
    /// Returns error if user has insufficient credits or billing service fails.
    pub async fn check_agent_credits_for_tier(
        &self,
        user_id: &str,
        tier: BoxTier,
    ) -> Result<(), BillingCheckError> {
        let required = i64::from(tier.hourly_price_cents()) * self.config.agent_runway_hours;
        self.check_balance(user_id, required).await
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

        let url = format!("{}/v1/usage/check", self.config.url);
        let body = BalanceCheckRequest {
            user_id,
            required_cents: required,
        };

        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.config.api_key)
            .header("x-service-name", "aura-control")
            .json(&body)
            .send()
            .await;

        match resp {
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
                Err(BillingCheckError::AccountNotFound {
                    user_id: user_id.to_string(),
                })
            }
            Ok(r) if r.status().is_success() => {
                let check: BalanceCheckResponse = r.json().await.map_err(|e| {
                    BillingCheckError::ServiceError(format!("invalid response: {e}"))
                })?;
                if check.sufficient {
                    Ok(())
                } else {
                    Err(BillingCheckError::InsufficientCredits {
                        balance: check.balance_cents,
                        required: check.required_cents,
                    })
                }
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                let msg = format!("billing service returned {status}: {body}");
                if self.config.fail_closed {
                    Err(BillingCheckError::ServiceError(msg))
                } else {
                    tracing::warn!(
                        user_id = %user_id,
                        error = %msg,
                        "Failed to check billing balance, continuing (fail_closed=false)"
                    );
                    Ok(())
                }
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
        assert_eq!(config.agent_runway_hours, 2);
        assert!(!config.fail_closed);
    }

    #[test]
    fn tier_check_required_amount_scales_with_price() {
        // 2 hours of runway at the tier rate.
        let config = BillingConfig::default();
        for tier in BoxTier::ALL {
            let required = i64::from(tier.hourly_price_cents()) * config.agent_runway_hours;
            assert_eq!(required, i64::from(tier.hourly_price_cents()) * 2);
        }
        // Sanity: standard tier needs 16 cents, not the flat $1.00.
        assert_eq!(i64::from(BoxTier::Standard.hourly_price_cents()) * 2, 16);
    }

    #[test]
    fn is_configured_requires_api_key() {
        let mut config = BillingConfig::default();
        assert!(!config.is_configured());

        config.api_key = "test-key".to_string();
        assert!(config.is_configured());
    }
}
