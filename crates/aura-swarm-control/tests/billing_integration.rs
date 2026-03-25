//! Integration tests for control plane billing against the live zbilling service.
//!
//! Environment variables:
//!   Z_BILLING_URL       - Service URL (default: https://z-billing.onrender.com)
//!   Z_BILLING_API_KEY   - Service-to-service API key (default: test-service-key)
//!   Z_BILLING_ADMIN_KEY - Admin API key for test account setup

use std::sync::Arc;

use aura_swarm_control::{
    BillingCheckError, BillingChecker, BillingConfig, ControlConfig, ControlPlane,
    ControlPlaneService, CreateAgentRequest, NoopSchedulerClient,
};
use aura_swarm_core::UserId;
use aura_swarm_store::{RocksStore, Store};
use serde_json::json;
use tempfile::TempDir;

fn billing_url() -> String {
    std::env::var("Z_BILLING_URL")
        .unwrap_or_else(|_| "https://z-billing.onrender.com".to_string())
}

fn service_api_key() -> String {
    std::env::var("Z_BILLING_API_KEY").unwrap_or_else(|_| "test-service-key".to_string())
}

fn admin_api_key() -> String {
    std::env::var("Z_BILLING_ADMIN_KEY").unwrap_or_else(|_| "test-admin-key".to_string())
}

// ============================================================================
// Test Setup
// ============================================================================

fn create_user_id() -> UserId {
    UserId::from_uuid(uuid::Uuid::new_v4())
}

/// Generate a unique UUID for each test (zbilling uses UUID format).
fn unique_user_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Generate a unique aura-swarm UserId and return both the UserId and its string representation.
fn unique_user_id_and_str() -> (UserId, String) {
    let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
    let user_str = user_id.to_string();
    (user_id, user_str)
}

fn setup_store() -> (Arc<RocksStore>, TempDir) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let store = Arc::new(RocksStore::open(temp_dir.path()).expect("Failed to open store"));
    (store, temp_dir)
}

// ============================================================================
// Billing Config Tests (no external dependencies)
// ============================================================================

#[test]
fn billing_config_defaults() {
    let config = BillingConfig::default();

    assert_eq!(config.url, "https://z-billing.onrender.com");
    assert!(config.enabled);
    assert!(!config.fail_closed);
    assert_eq!(config.min_credits_for_agent, 100);
    assert_eq!(config.min_credits_for_session, 10);
}

#[test]
fn billing_config_is_configured() {
    let mut config = BillingConfig::default();
    assert!(!config.is_configured());

    config.api_key = "test-key".to_string();
    assert!(config.is_configured());

    config.enabled = false;
    assert!(!config.is_configured());
}

// ============================================================================
// Billing Checker Tests (disabled mode - no external dependencies)
// ============================================================================

#[tokio::test]
async fn billing_checker_disabled_allows_all() {
    let config = BillingConfig::default(); // No API key = disabled
    let checker = BillingChecker::new(config).unwrap();

    assert!(!checker.is_enabled());

    // Should succeed when disabled
    let result = checker.check_agent_credits("user-1").await;
    assert!(result.is_ok());

    let result = checker.check_session_credits("user-1").await;
    assert!(result.is_ok());
}

// ============================================================================
// Control Plane Tests (no external billing dependencies)
// ============================================================================

#[tokio::test]
async fn control_plane_without_billing_allows_agent_creation() {
    let (store, _temp_dir) = setup_store();
    let config = ControlConfig::default();
    let service = ControlPlaneService::new(store, config);

    let user_id = create_user_id();
    let request = CreateAgentRequest::new("test-agent");

    let result = service.create_agent(&user_id, request).await;
    assert!(result.is_ok());
}

// ============================================================================
// Live Integration Tests (z-billing at Z_BILLING_URL)
// ============================================================================

mod live {
    use super::*;
    use aura_swarm_store::AgentState;

    /// Returns true if the live billing keys are configured.
    fn has_billing_keys() -> bool {
        std::env::var("Z_BILLING_API_KEY").map_or(false, |v| !v.is_empty())
    }

    fn test_billing_config() -> BillingConfig {
        BillingConfig {
            url: billing_url(),
            api_key: service_api_key(),
            enabled: true,
            min_credits_for_agent: 100,
            min_credits_for_session: 10,
            fail_closed: true,
        }
    }

    /// Create a funded test account via the zbilling API.
    ///
    /// Triggers auto-account-creation via a usage check, then adds credits
    /// through the admin endpoint.
    async fn create_funded_account(user_uuid: &str, balance_cents: i64) -> reqwest::Result<()> {
        let url = billing_url();
        let api_key = service_api_key();
        let client = reqwest::Client::new();

        // Usage check auto-creates the account with zero balance
        client
            .post(format!("{url}/v1/usage/check"))
            .header("x-api-key", &api_key)
            .header("x-service-name", "aura-control-test")
            .json(&json!({ "user_id": user_uuid, "required_cents": 0 }))
            .send()
            .await?
            .error_for_status()?;

        if balance_cents > 0 {
            client
                .post(format!("{url}/v1/credits/add"))
                .header("x-admin-key", admin_api_key())
                .json(&json!({
                    "user_id": user_uuid,
                    "amount_cents": balance_cents,
                    "reason": "Integration test funding"
                }))
                .send()
                .await?
                .error_for_status()?;
        }

        Ok(())
    }

    #[tokio::test]
    async fn billing_checker_sufficient_balance() {
        if !has_billing_keys() { println!("skipped: Z_BILLING_API_KEY not set"); return; }
        let user_uuid = unique_user_uuid();
        create_funded_account(&user_uuid, 10000).await.expect("Failed to create test account");

        let checker = BillingChecker::new(test_billing_config()).unwrap();
        assert!(checker.is_enabled());

        let result = checker.check_agent_credits(&user_uuid).await;
        assert!(result.is_ok(), "check_agent_credits failed: {result:?}");

        let result = checker.check_session_credits(&user_uuid).await;
        assert!(result.is_ok(), "check_session_credits failed: {result:?}");
    }

    #[tokio::test]
    async fn billing_checker_insufficient_balance() {
        if !has_billing_keys() { println!("skipped: Z_BILLING_API_KEY not set"); return; }
        let user_uuid = unique_user_uuid();
        create_funded_account(&user_uuid, 50).await.expect("Failed to create test account");

        let checker = BillingChecker::new(test_billing_config()).unwrap();

        let result = checker.check_agent_credits(&user_uuid).await;
        assert!(
            matches!(result, Err(BillingCheckError::InsufficientCredits { .. })),
            "Expected InsufficientCredits, got: {result:?}"
        );

        let result = checker.check_session_credits(&user_uuid).await;
        assert!(result.is_ok(), "check_session_credits should pass: {result:?}");
    }

    #[tokio::test]
    async fn control_plane_with_billing_checks_balance() {
        if !has_billing_keys() { println!("skipped: Z_BILLING_API_KEY not set"); return; }
        let (user_id, user_str) = unique_user_id_and_str();
        create_funded_account(&user_str, 10000).await.expect("Failed to create test account");

        let billing = Arc::new(BillingChecker::new(test_billing_config()).unwrap());
        let (store, _temp_dir) = setup_store();

        let service: ControlPlaneService<RocksStore, NoopSchedulerClient> =
            ControlPlaneService::with_integrations(store, ControlConfig::default(), None, Some(billing));

        let result = service.create_agent(&user_id, CreateAgentRequest::new("test-agent")).await;
        assert!(result.is_ok(), "create_agent failed: {result:?}");
    }

    #[tokio::test]
    async fn control_plane_rejects_agent_with_insufficient_balance() {
        if !has_billing_keys() { println!("skipped: Z_BILLING_API_KEY not set"); return; }
        let (user_id, user_str) = unique_user_id_and_str();
        create_funded_account(&user_str, 50).await.expect("Failed to create test account");

        let billing = Arc::new(BillingChecker::new(test_billing_config()).unwrap());
        let (store, _temp_dir) = setup_store();

        let service: ControlPlaneService<RocksStore, NoopSchedulerClient> =
            ControlPlaneService::with_integrations(store, ControlConfig::default(), None, Some(billing));

        let result = service.create_agent(&user_id, CreateAgentRequest::new("test-agent")).await;
        assert!(result.is_err(), "Expected error for insufficient balance");
        assert_eq!(result.unwrap_err().http_status_code(), 402);
    }

    #[tokio::test]
    async fn control_plane_session_checks_balance() {
        if !has_billing_keys() { println!("skipped: Z_BILLING_API_KEY not set"); return; }
        let (user_id, user_str) = unique_user_id_and_str();
        create_funded_account(&user_str, 10000).await.expect("Failed to create test account");

        let billing = Arc::new(BillingChecker::new(test_billing_config()).unwrap());
        let (store, _temp_dir) = setup_store();

        let service: ControlPlaneService<RocksStore, NoopSchedulerClient> =
            ControlPlaneService::with_integrations(store.clone(), ControlConfig::default(), None, Some(billing));

        let request = CreateAgentRequest::new("test-agent");
        let agent = service.create_agent(&user_id, request).await.unwrap();
        Store::update_agent_status(&*store, &agent.agent_id, AgentState::Running).unwrap();

        let result = service
            .create_session(&user_id, &agent.agent_id, aura_swarm_store::SessionConfig::default())
            .await;
        assert!(result.is_ok(), "create_session failed: {result:?}");
    }
}
