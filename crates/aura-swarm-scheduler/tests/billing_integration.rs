//! Integration tests for scheduler billing against the live zbilling service.
//!
//! Environment variables:
//!   Z_BILLING_URL       - Service URL (default: https://z-billing.onrender.com)
//!   Z_BILLING_API_KEY   - Service-to-service API key (default: test-service-key)
//!   Z_BILLING_ADMIN_KEY - Admin API key for test account setup

use std::time::Duration;

use aura_swarm_scheduler::{ComputeUsageReporter, SchedulerBillingConfig};
use serde_json::json;

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

/// Generate a unique UUID for each test (zbilling uses UUID format).
fn unique_user_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ============================================================================
// Config Tests (no external dependencies)
// ============================================================================

#[test]
fn scheduler_billing_config_defaults() {
    let config = SchedulerBillingConfig::default();

    assert_eq!(config.url, "https://z-billing.onrender.com");
    assert!(config.enabled);
    assert!(!config.fail_closed);
    assert_eq!(config.report_interval_seconds, 300);
}

#[test]
fn scheduler_billing_config_is_configured() {
    let mut config = SchedulerBillingConfig::default();
    assert!(!config.is_configured());

    config.api_key = "test-key".to_string();
    assert!(config.is_configured());

    config.enabled = false;
    assert!(!config.is_configured());
}

// ============================================================================
// Compute Usage Reporter Tests (disabled mode - no external dependencies)
// ============================================================================

#[test]
fn reporter_disabled_when_no_api_key() {
    let config = SchedulerBillingConfig::default();
    let reporter = ComputeUsageReporter::new(config).unwrap();

    assert!(!reporter.is_enabled());
}

#[test]
fn reporter_enabled_with_api_key() {
    let mut config = SchedulerBillingConfig::default();
    config.api_key = "test-key".to_string();
    let reporter = ComputeUsageReporter::new(config).unwrap();

    assert!(reporter.is_enabled());
}

#[test]
fn reporter_tracks_pods() {
    let config = SchedulerBillingConfig::default();
    let reporter = ComputeUsageReporter::new(config).unwrap();

    assert_eq!(reporter.tracked_pod_count(), 0);

    reporter.register_pod("agent-1", "user-1", 500, 512);
    assert_eq!(reporter.tracked_pod_count(), 1);

    reporter.register_pod("agent-2", "user-1", 1000, 1024);
    assert_eq!(reporter.tracked_pod_count(), 2);

    reporter.unregister_pod("agent-1");
    assert_eq!(reporter.tracked_pod_count(), 1);

    reporter.unregister_pod("agent-2");
    assert_eq!(reporter.tracked_pod_count(), 0);
}

#[test]
fn reporter_report_interval() {
    let mut config = SchedulerBillingConfig::default();
    config.report_interval_seconds = 60;
    let reporter = ComputeUsageReporter::new(config).unwrap();

    assert_eq!(reporter.report_interval(), Duration::from_secs(60));
}

#[tokio::test]
async fn reporter_disabled_reports_zero() {
    let config = SchedulerBillingConfig::default(); // No API key
    let reporter = ComputeUsageReporter::new(config).unwrap();

    reporter.register_pod("agent-1", "user-1", 500, 512);

    // Should return 0 when disabled
    let count = reporter.report_all_usage().await;
    assert_eq!(count, 0);
}

// ============================================================================
// Live Integration Tests (require zbilling service)
// ============================================================================

mod live {
    use super::*;

    fn has_billing_keys() -> bool {
        std::env::var("Z_BILLING_API_KEY").map_or(false, |v| !v.is_empty())
    }

    fn test_billing_config() -> SchedulerBillingConfig {
        SchedulerBillingConfig {
            url: billing_url(),
            api_key: service_api_key(),
            enabled: true,
            report_interval_seconds: 0,
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
            .header("x-service-name", "aura-scheduler-test")
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
    async fn reporter_reports_compute_usage() {
        if !has_billing_keys() { println!("skipped: Z_BILLING_API_KEY not set"); return; }
        let user_uuid = unique_user_uuid();
        let agent_uuid = unique_user_uuid();
        create_funded_account(&user_uuid, 10000)
            .await
            .expect("Failed to create test account");

        let reporter = ComputeUsageReporter::new(test_billing_config()).unwrap();
        reporter.register_pod(&agent_uuid, &user_uuid, 500, 512);
        tokio::time::sleep(Duration::from_millis(10)).await;

        let count = reporter.report_all_usage().await;
        assert_eq!(count, 1, "Should report 1 pod");
    }

    #[tokio::test]
    async fn reporter_reports_multiple_pods() {
        if !has_billing_keys() { println!("skipped: Z_BILLING_API_KEY not set"); return; }
        let user_uuid_1 = unique_user_uuid();
        let user_uuid_2 = unique_user_uuid();
        let agent_uuid_1 = unique_user_uuid();
        let agent_uuid_2 = unique_user_uuid();

        create_funded_account(&user_uuid_1, 10000)
            .await
            .expect("Failed to create test account 1");
        create_funded_account(&user_uuid_2, 10000)
            .await
            .expect("Failed to create test account 2");

        let reporter = ComputeUsageReporter::new(test_billing_config()).unwrap();
        reporter.register_pod(&agent_uuid_1, &user_uuid_1, 500, 512);
        reporter.register_pod(&agent_uuid_2, &user_uuid_2, 1000, 1024);
        tokio::time::sleep(Duration::from_millis(10)).await;

        let count = reporter.report_all_usage().await;
        assert_eq!(count, 2, "Should report 2 pods");
    }

    #[tokio::test]
    async fn full_compute_usage_flow() {
        if !has_billing_keys() { println!("skipped: Z_BILLING_API_KEY not set"); return; }
        let user_uuid = unique_user_uuid();
        let agent_uuid_1 = unique_user_uuid();
        let agent_uuid_2 = unique_user_uuid();
        create_funded_account(&user_uuid, 10000)
            .await
            .expect("Failed to create test account");

        let reporter = ComputeUsageReporter::new(test_billing_config()).unwrap();

        reporter.register_pod(&agent_uuid_1, &user_uuid, 500, 512);
        reporter.register_pod(&agent_uuid_2, &user_uuid, 1000, 1024);
        assert_eq!(reporter.tracked_pod_count(), 2);

        tokio::time::sleep(Duration::from_millis(10)).await;

        let count = reporter.report_all_usage().await;
        assert_eq!(count, 2, "Should report 2 pods");

        reporter.unregister_pod(&agent_uuid_1);
        assert_eq!(reporter.tracked_pod_count(), 1);

        tokio::time::sleep(Duration::from_millis(10)).await;

        let count = reporter.report_all_usage().await;
        assert_eq!(count, 1, "Should report 1 pod");

        reporter.unregister_pod(&agent_uuid_2);
        assert_eq!(reporter.tracked_pod_count(), 0);
    }
}
