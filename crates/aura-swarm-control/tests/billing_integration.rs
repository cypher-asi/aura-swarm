//! Integration tests for control plane billing.
//!
//! Tests balance checking before agent and session creation.
//!
//! Requires z-billing service running at localhost:8081.

use std::sync::Arc;

use aura_swarm_control::{
    BillingCheckError, BillingChecker, BillingConfig, ControlConfig, ControlPlane,
    ControlPlaneService, CreateAgentRequest, NoopSchedulerClient,
};
use aura_swarm_core::IdentityId;
use aura_swarm_store::{RocksStore, Store};
use serde_json::json;
use tempfile::TempDir;

/// The billing service URL for integration tests.
const BILLING_URL: &str = "http://localhost:8081";

/// The service API key for z-billing.
const SERVICE_API_KEY: &str = "test-service-key";

// ============================================================================
// Test Setup
// ============================================================================

fn create_identity_id() -> IdentityId {
    IdentityId::from_uuid(uuid::Uuid::new_v4())
}

/// Generate a unique UUID for each test (z-billing uses UUID format).
fn unique_user_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Generate a unique aura-swarm IdentityId and return both the IdentityId and its string representation.
/// The string is what gets passed to billing, so tests should create accounts with it.
fn unique_identity_id_and_str() -> (IdentityId, String) {
    let identity_id = IdentityId::from_uuid(uuid::Uuid::new_v4());
    let identity_str = identity_id.to_string();
    (identity_id, identity_str)
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

    assert_eq!(config.url, "http://z-billing:8080");
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
    let checker = BillingChecker::new(config);

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

    let identity_id = create_identity_id();
    let request = CreateAgentRequest::new("test-agent");

    let result = service.create_agent(&identity_id, request).await;
    assert!(result.is_ok());
}

// ============================================================================
// Live Integration Tests (require z-billing at localhost:8081)
// ============================================================================

mod live {
    use super::*;
    use aura_swarm_store::AgentState;

    /// Helper to create a funded test account via the billing service.
    async fn create_funded_account(user_uuid: &str, balance_cents: i64) -> reqwest::Result<()> {
        let client = reqwest::Client::new();

        // Create account
        client
            .post(format!("{}/v1/accounts", BILLING_URL))
            .header("authorization", format!("Bearer test-token:{user_uuid}"))
            .json(&json!({}))
            .send()
            .await?
            .error_for_status()?;

        // Add credits if needed
        if balance_cents > 0 {
            client
                .post(format!("{}/v1/credits/add", BILLING_URL))
                .json(&json!({
                    "user_id": user_uuid,
                    "amount_cents": balance_cents,
                    "reason": "Test funding"
                }))
                .send()
                .await?
                .error_for_status()?;
        }

        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires z-billing service at localhost:8081"]
    async fn billing_checker_sufficient_balance() {
        let user_uuid = unique_user_uuid();

        // Create funded account
        create_funded_account(&user_uuid, 10000)
            .await
            .expect("Failed to create test account");

        let config = BillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            min_credits_for_agent: 100,
            min_credits_for_session: 10,
            fail_closed: true,
        };

        let checker = BillingChecker::new(config);
        assert!(checker.is_enabled());

        // Should succeed with sufficient balance
        let result = checker.check_agent_credits(&user_uuid).await;
        assert!(result.is_ok(), "check_agent_credits failed: {result:?}");

        let result = checker.check_session_credits(&user_uuid).await;
        assert!(result.is_ok(), "check_session_credits failed: {result:?}");
    }

    #[tokio::test]
    #[ignore = "requires z-billing service at localhost:8081"]
    async fn billing_checker_insufficient_balance() {
        let user_uuid = unique_user_uuid();

        // Create account with low balance
        create_funded_account(&user_uuid, 50)
            .await
            .expect("Failed to create test account");

        let config = BillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            min_credits_for_agent: 100,
            min_credits_for_session: 10,
            fail_closed: true,
        };

        let checker = BillingChecker::new(config);

        // Should fail for agent (requires 100)
        let result = checker.check_agent_credits(&user_uuid).await;
        assert!(
            matches!(result, Err(BillingCheckError::InsufficientCredits { .. })),
            "Expected InsufficientCredits, got: {result:?}"
        );

        // Should succeed for session (requires 10)
        let result = checker.check_session_credits(&user_uuid).await;
        assert!(
            result.is_ok(),
            "check_session_credits should pass: {result:?}"
        );
    }

    #[tokio::test]
    #[ignore = "requires z-billing service at localhost:8081"]
    async fn control_plane_with_billing_checks_balance() {
        // Generate IdentityId and its string representation (what control plane sends to billing)
        let (identity_id, identity_str) = unique_identity_id_and_str();

        // Create funded account using the identity ID string (what billing will receive)
        create_funded_account(&identity_str, 10000)
            .await
            .expect("Failed to create test account");

        let billing_config = BillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            min_credits_for_agent: 100,
            min_credits_for_session: 10,
            fail_closed: true,
        };

        let billing = Arc::new(BillingChecker::new(billing_config));
        let (store, _temp_dir) = setup_store();
        let config = ControlConfig::default();

        let service: ControlPlaneService<RocksStore, NoopSchedulerClient> =
            ControlPlaneService::with_integrations(store, config, None, Some(billing));

        let request = CreateAgentRequest::new("test-agent");

        // Should succeed with sufficient balance
        let result = service.create_agent(&identity_id, request).await;
        assert!(result.is_ok(), "create_agent failed: {result:?}");
    }

    #[tokio::test]
    #[ignore = "requires z-billing service at localhost:8081"]
    async fn control_plane_rejects_agent_with_insufficient_balance() {
        // Generate IdentityId and its string representation
        let (identity_id, identity_str) = unique_identity_id_and_str();

        // Create account with low balance
        create_funded_account(&identity_str, 50)
            .await
            .expect("Failed to create test account");

        let billing_config = BillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            min_credits_for_agent: 100,
            min_credits_for_session: 10,
            fail_closed: true,
        };

        let billing = Arc::new(BillingChecker::new(billing_config));
        let (store, _temp_dir) = setup_store();
        let config = ControlConfig::default();

        let service: ControlPlaneService<RocksStore, NoopSchedulerClient> =
            ControlPlaneService::with_integrations(store, config, None, Some(billing));

        let request = CreateAgentRequest::new("test-agent");

        // Should fail due to insufficient balance
        let result = service.create_agent(&identity_id, request).await;
        assert!(result.is_err(), "Expected error for insufficient balance");

        // Verify error type
        let err = result.unwrap_err();
        assert_eq!(err.http_status_code(), 402); // Payment Required
    }

    #[tokio::test]
    #[ignore = "requires z-billing service at localhost:8081"]
    async fn control_plane_session_checks_balance() {
        // Generate IdentityId and its string representation
        let (identity_id, identity_str) = unique_identity_id_and_str();

        // Create funded account
        create_funded_account(&identity_str, 10000)
            .await
            .expect("Failed to create test account");

        let billing_config = BillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            min_credits_for_agent: 100,
            min_credits_for_session: 10,
            fail_closed: true,
        };

        let billing = Arc::new(BillingChecker::new(billing_config));
        let (store, _temp_dir) = setup_store();
        let config = ControlConfig::default();

        let service: ControlPlaneService<RocksStore, NoopSchedulerClient> =
            ControlPlaneService::with_integrations(store.clone(), config, None, Some(billing));

        // Create agent first
        let request = CreateAgentRequest::new("test-agent");
        let agent = service.create_agent(&identity_id, request).await.unwrap();

        // Simulate agent is running
        Store::update_agent_status(&*store, &agent.agent_id, AgentState::Running).unwrap();

        // Create session (should check balance)
        let result = service
            .create_session(
                &identity_id,
                &agent.agent_id,
                aura_swarm_store::SessionConfig::default(),
            )
            .await;
        assert!(result.is_ok(), "create_session failed: {result:?}");
    }
}
