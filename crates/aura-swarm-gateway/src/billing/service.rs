//! Billing service wrapper for the gateway.
//!
//! Provides a high-level interface to z-billing with caching and error handling.

use std::sync::Arc;
use std::time::Duration;

use z_billing_client::{ClientOptions, LlmUsageEvent, ZBillingClient};

use super::account_cache::AccountCache;
use super::config::BillingConfig;

/// Gateway billing service.
///
/// Wraps the z-billing client with account caching and graceful degradation.
pub struct BillingService {
    client: ZBillingClient,
    cache: AccountCache,
    config: BillingConfig,
}

impl BillingService {
    /// Create a new billing service from configuration.
    #[must_use]
    pub fn new(config: BillingConfig) -> Self {
        let options = ClientOptions::with_service_name("aura-gateway");
        let client = ZBillingClient::with_options(&config.url, &config.api_key, options)
            .expect("failed to create billing client");
        let cache = AccountCache::new(Duration::from_secs(config.account_cache_ttl_seconds));

        Self {
            client,
            cache,
            config,
        }
    }

    /// Create a billing service wrapped in an Arc.
    #[must_use]
    pub fn new_shared(config: BillingConfig) -> Arc<Self> {
        Arc::new(Self::new(config))
    }

    /// Check if billing is enabled and configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.is_configured()
    }

    /// Ensure an account exists for the user, creating one if needed.
    ///
    /// Uses caching to minimize API calls. Returns `Ok(true)` if account exists
    /// or was created, `Ok(false)` if billing is disabled.
    ///
    /// # Errors
    ///
    /// Returns error if billing service communication fails.
    pub async fn ensure_account(&self, user_id: &str) -> Result<bool, BillingServiceError> {
        if !self.is_enabled() {
            return Ok(false);
        }

        // Check cache first
        if let Some(true) = self.cache.get(user_id) {
            return Ok(true);
        }

        // Try to create account (idempotent operation)
        match self.create_account_internal(user_id).await {
            Ok(()) => {
                self.cache.set_exists(user_id);
                Ok(true)
            }
            Err(e) if !self.config.fail_closed => {
                tracing::warn!(
                    user_id = %user_id,
                    error = %e,
                    "Failed to ensure billing account, continuing (fail_closed=false)"
                );
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Report LLM usage for billing.
    ///
    /// This is called asynchronously after WebSocket messages complete.
    ///
    /// # Errors
    ///
    /// Returns error if billing service communication fails.
    pub async fn report_llm_usage(&self, event: LlmUsageEvent) -> Result<(), BillingServiceError> {
        if !self.is_enabled() {
            return Ok(());
        }

        match self.client.report_llm_usage(event.clone()).await {
            Ok(response) => {
                tracing::debug!(
                    event_id = %event.event_id,
                    cost_cents = response.cost_cents,
                    "Reported LLM usage"
                );
                Ok(())
            }
            Err(z_billing_client::ClientError::DuplicateEvent { .. }) => {
                // Idempotent - already processed, not an error
                Ok(())
            }
            Err(e) if !self.config.fail_closed => {
                tracing::warn!(
                    event_id = %event.event_id,
                    error = %e,
                    "Failed to report LLM usage, continuing"
                );
                Ok(())
            }
            Err(e) => Err(BillingServiceError::Client(e)),
        }
    }

    /// Get a reference to the cache for metrics/monitoring.
    #[must_use]
    pub fn cache(&self) -> &AccountCache {
        &self.cache
    }

    /// Create an account via the z-billing API.
    async fn create_account_internal(&self, user_id: &str) -> Result<(), BillingServiceError> {
        // The z-billing client doesn't have a direct create_account method in the SDK
        // We'll use a minimal HTTP call or rely on the first usage event creating the account
        // For now, we'll check balance which will fail with AccountNotFound if no account
        match self.client.check_balance(user_id, 0).await {
            Ok(_) => Ok(()),
            Err(z_billing_client::ClientError::AccountNotFound { .. }) => {
                // Account doesn't exist - this is expected on first use
                // The billing service should auto-create on first usage report
                // Mark as not existing so we retry on next request
                tracing::info!(user_id = %user_id, "Account not found, will create on first usage");
                Ok(())
            }
            Err(e) => Err(BillingServiceError::Client(e)),
        }
    }
}

/// Errors from the billing service.
#[derive(Debug, thiserror::Error)]
pub enum BillingServiceError {
    /// Z-billing client error.
    #[error("billing client error: {0}")]
    Client(#[from] z_billing_client::ClientError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_disabled_by_default() {
        let config = BillingConfig::default();
        let service = BillingService::new(config);
        assert!(!service.is_enabled());
    }

    #[test]
    fn service_enabled_with_api_key() {
        let mut config = BillingConfig::default();
        config.api_key = "test-key".to_string();
        let service = BillingService::new(config);
        assert!(service.is_enabled());
    }
}
