//! Integration tests for scheduler billing.
//!
//! Tests compute usage reporting.
//!
//! Requires z-billing service running at localhost:8081.

use std::time::Duration;

use aura_swarm_scheduler::{ComputeUsageReporter, SchedulerBillingConfig};
use serde_json::json;

/// The billing service URL for integration tests.
const BILLING_URL: &str = "http://localhost:8081";

/// The service API key for z-billing.
const SERVICE_API_KEY: &str = "test-service-key";

/// Generate a unique UUID for each test (z-billing uses UUID format).
fn unique_user_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ============================================================================
// Config Tests (no external dependencies)
// ============================================================================

#[test]
fn scheduler_billing_config_defaults() {
    let config = SchedulerBillingConfig::default();

    assert_eq!(config.url, "http://z-billing:8080");
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
    let reporter = ComputeUsageReporter::new(config);

    assert!(!reporter.is_enabled());
}

#[test]
fn reporter_enabled_with_api_key() {
    let mut config = SchedulerBillingConfig::default();
    config.api_key = "test-key".to_string();
    let reporter = ComputeUsageReporter::new(config);

    assert!(reporter.is_enabled());
}

#[test]
fn reporter_tracks_pods() {
    let config = SchedulerBillingConfig::default();
    let reporter = ComputeUsageReporter::new(config);

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
    let reporter = ComputeUsageReporter::new(config);

    assert_eq!(reporter.report_interval(), Duration::from_secs(60));
}

#[tokio::test]
async fn reporter_disabled_reports_zero() {
    let config = SchedulerBillingConfig::default(); // No API key
    let reporter = ComputeUsageReporter::new(config);

    reporter.register_pod("agent-1", "user-1", 500, 512);

    // Should return 0 when disabled
    let count = reporter.report_all_usage().await;
    assert_eq!(count, 0);
}

// ============================================================================
// Live Integration Tests (require z-billing at localhost:8081)
// ============================================================================

mod live {
    use super::*;

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
    async fn reporter_reports_compute_usage() {
        let user_uuid = unique_user_uuid();
        let agent_uuid = unique_user_uuid(); // Agent ID must be a valid UUID

        // Create funded account
        create_funded_account(&user_uuid, 10000)
            .await
            .expect("Failed to create test account");

        let config = SchedulerBillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            report_interval_seconds: 0, // Report immediately
            fail_closed: true,
        };

        let reporter = ComputeUsageReporter::new(config);

        // Register pod
        reporter.register_pod(&agent_uuid, &user_uuid, 500, 512);

        // Wait a moment for interval check
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Report usage
        let count = reporter.report_all_usage().await;
        assert_eq!(count, 1, "Should report 1 pod");
    }

    #[tokio::test]
    #[ignore = "requires z-billing service at localhost:8081"]
    async fn reporter_reports_multiple_pods() {
        let user_uuid_1 = unique_user_uuid();
        let user_uuid_2 = unique_user_uuid();
        let agent_uuid_1 = unique_user_uuid(); // Agent IDs must be valid UUIDs
        let agent_uuid_2 = unique_user_uuid();

        // Create funded accounts
        create_funded_account(&user_uuid_1, 10000)
            .await
            .expect("Failed to create test account 1");
        create_funded_account(&user_uuid_2, 10000)
            .await
            .expect("Failed to create test account 2");

        let config = SchedulerBillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            report_interval_seconds: 0,
            fail_closed: true,
        };

        let reporter = ComputeUsageReporter::new(config);

        // Register multiple pods
        reporter.register_pod(&agent_uuid_1, &user_uuid_1, 500, 512);
        reporter.register_pod(&agent_uuid_2, &user_uuid_2, 1000, 1024);

        tokio::time::sleep(Duration::from_millis(10)).await;

        // Report usage
        let count = reporter.report_all_usage().await;
        assert_eq!(count, 2, "Should report 2 pods");
    }

    #[tokio::test]
    #[ignore = "requires z-billing service at localhost:8081"]
    async fn full_compute_usage_flow() {
        let user_uuid = unique_user_uuid();
        let agent_uuid_1 = unique_user_uuid(); // Agent IDs must be valid UUIDs
        let agent_uuid_2 = unique_user_uuid();

        // Create funded account
        create_funded_account(&user_uuid, 10000)
            .await
            .expect("Failed to create test account");

        let config = SchedulerBillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            report_interval_seconds: 0,
            fail_closed: true,
        };

        let reporter = ComputeUsageReporter::new(config);

        // Simulate pod lifecycle
        // 1. Pod starts
        reporter.register_pod(&agent_uuid_1, &user_uuid, 500, 512);
        reporter.register_pod(&agent_uuid_2, &user_uuid, 1000, 1024);
        assert_eq!(reporter.tracked_pod_count(), 2);

        tokio::time::sleep(Duration::from_millis(10)).await;

        // 2. Report usage
        let count = reporter.report_all_usage().await;
        assert_eq!(count, 2, "Should report 2 pods");

        // 3. One pod terminates
        reporter.unregister_pod(&agent_uuid_1);
        assert_eq!(reporter.tracked_pod_count(), 1);

        tokio::time::sleep(Duration::from_millis(10)).await;

        // 4. Report again (only one pod)
        let count = reporter.report_all_usage().await;
        assert_eq!(count, 1, "Should report 1 pod");

        // 5. All pods terminate
        reporter.unregister_pod(&agent_uuid_2);
        assert_eq!(reporter.tracked_pod_count(), 0);
    }
}
