//! Integration tests for billing flows.
//!
//! These tests verify:
//! - LLM usage extraction from WebSocket messages
//! - Account caching behavior
//! - BillingService integration with real z-billing service
//!
//! Requires z-billing service running at localhost:8081.

use std::time::Duration;

use aura_swarm_gateway::billing::{
    make_event_id, try_extract_usage, AccountCache, BillingConfig, BillingService,
};
use z_billing_client::LlmUsageEvent;

/// The billing service URL for integration tests.
const BILLING_URL: &str = "http://localhost:8081";

/// The service API key for z-billing.
const SERVICE_API_KEY: &str = "test-service-key";

// ============================================================================
// LLM Usage Extraction Tests (no external dependencies)
// ============================================================================

#[test]
fn extract_usage_from_assistant_message_end() {
    let msg = r#"{
        "type": "assistant_message_end",
        "message_id": "msg_abc123",
        "provider": "anthropic",
        "model": "claude-sonnet-4-20250514",
        "usage": {
            "input_tokens": 1500,
            "output_tokens": 750
        }
    }"#;

    let usage = try_extract_usage(msg).expect("Should extract usage");

    assert_eq!(usage.message_id, "msg_abc123");
    assert_eq!(usage.provider, "anthropic");
    assert_eq!(usage.model, "claude-sonnet-4-20250514");
    assert_eq!(usage.input_tokens, 1500);
    assert_eq!(usage.output_tokens, 750);
}

#[test]
fn extract_usage_ignores_non_end_messages() {
    let messages = [
        r#"{"type": "text_delta", "text": "Hello"}"#,
        r#"{"type": "content_block_start", "index": 0}"#,
        r#"{"type": "message_start", "message": {}}"#,
        r#"{"type": "ping"}"#,
    ];

    for msg in messages {
        assert!(
            try_extract_usage(msg).is_none(),
            "Should ignore message: {msg}"
        );
    }
}

#[test]
fn extract_usage_requires_tokens() {
    let msg = r#"{
        "type": "assistant_message_end",
        "message_id": "msg_no_tokens",
        "usage": {}
    }"#;

    assert!(
        try_extract_usage(msg).is_none(),
        "Should ignore zero token usage"
    );
}

#[test]
fn extract_usage_handles_missing_provider_model() {
    let msg = r#"{
        "type": "assistant_message_end",
        "message_id": "msg_minimal",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50
        }
    }"#;

    let usage = try_extract_usage(msg).expect("Should extract usage");

    assert_eq!(usage.provider, "unknown");
    assert_eq!(usage.model, "unknown");
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 50);
}

#[test]
fn event_id_format_is_idempotent() {
    let event_id = make_event_id("sess_abc123", "msg_xyz789");
    assert_eq!(event_id, "sess_abc123:msg_xyz789");

    // Same inputs produce same output
    assert_eq!(
        make_event_id("sess_abc123", "msg_xyz789"),
        make_event_id("sess_abc123", "msg_xyz789")
    );
}

// ============================================================================
// Account Cache Tests (no external dependencies)
// ============================================================================

#[test]
fn account_cache_basic_operations() {
    let cache = AccountCache::new(Duration::from_secs(60));

    // Initially empty
    assert!(cache.get("user-1").is_none());
    assert!(cache.is_empty());

    // Set exists
    cache.set_exists("user-1");
    assert_eq!(cache.get("user-1"), Some(true));
    assert_eq!(cache.len(), 1);

    // Set not exists
    cache.set_not_exists("user-2");
    assert_eq!(cache.get("user-2"), Some(false));
    assert_eq!(cache.len(), 2);
}

#[test]
fn account_cache_expiry() {
    let cache = AccountCache::new(Duration::from_millis(50));

    cache.set_exists("user-1");
    assert_eq!(cache.get("user-1"), Some(true));

    // Wait for expiry
    std::thread::sleep(Duration::from_millis(100));
    assert!(cache.get("user-1").is_none());
}

#[test]
fn account_cache_cleanup_removes_expired() {
    let cache = AccountCache::new(Duration::from_millis(10));

    cache.set_exists("user-1");
    cache.set_exists("user-2");
    cache.set_exists("user-3");
    assert_eq!(cache.len(), 3);

    std::thread::sleep(Duration::from_millis(50));
    cache.cleanup();

    assert_eq!(cache.len(), 0);
}

#[test]
fn account_cache_overwrite_resets_ttl() {
    let cache = AccountCache::new(Duration::from_millis(100));

    cache.set_exists("user-1");
    std::thread::sleep(Duration::from_millis(60));

    // Overwrite before expiry
    cache.set_exists("user-1");
    std::thread::sleep(Duration::from_millis(60));

    // Should still be valid since TTL was reset
    assert_eq!(cache.get("user-1"), Some(true));
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
    assert_eq!(config.account_cache_ttl_seconds, 300);
}

#[test]
fn billing_config_is_configured() {
    let mut config = BillingConfig::default();
    assert!(!config.is_configured(), "Should require API key");

    config.api_key = "test-key".to_string();
    assert!(config.is_configured());

    config.enabled = false;
    assert!(!config.is_configured(), "Should require enabled");
}

// ============================================================================
// Billing Service Tests (disabled mode - no external dependencies)
// ============================================================================

#[tokio::test]
async fn billing_service_disabled_returns_ok() {
    let config = BillingConfig::default(); // No API key = disabled
    let service = BillingService::new(config);

    // All operations should succeed when disabled
    let result = service.ensure_account("user-1").await;
    assert!(result.is_ok());
    assert!(!result.unwrap()); // Returns false when disabled

    let event = LlmUsageEvent {
        event_id: "evt_test".to_string(),
        user_id: "user-1".to_string(),
        agent_id: Some("agent-1".to_string()),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-20250514".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        metadata: None,
    };
    let result = service.report_llm_usage(event).await;
    assert!(result.is_ok());
}

// ============================================================================
// Billing Service Integration Tests (require z-billing at localhost:8081)
// ============================================================================

mod live {
    use super::*;
    use std::sync::Arc;

    /// Helper to create a funded test account via the billing service.
    async fn create_funded_account(user_uuid: &str, balance_cents: i64) -> reqwest::Result<()> {
        let client = reqwest::Client::new();

        // Create account
        client
            .post(format!("{}/v1/accounts", BILLING_URL))
            .header("authorization", format!("Bearer test-token:{user_uuid}"))
            .json(&serde_json::json!({}))
            .send()
            .await?
            .error_for_status()?;

        // Add credits if needed
        if balance_cents > 0 {
            client
                .post(format!("{}/v1/credits/add", BILLING_URL))
                .json(&serde_json::json!({
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

    /// Generate a unique UUID for each test to avoid conflicts.
    fn unique_user_uuid() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[tokio::test]
    #[ignore = "requires z-billing service at localhost:8081"]
    async fn billing_service_ensure_account() {
        let user_uuid = unique_user_uuid();

        let config = BillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            fail_closed: true,
            account_cache_ttl_seconds: 60,
        };

        let service = BillingService::new(config);

        // First call should create account
        let result = service.ensure_account(&user_uuid).await;
        assert!(result.is_ok(), "ensure_account failed: {result:?}");

        // Second call should hit cache
        let result = service.ensure_account(&user_uuid).await;
        assert!(result.is_ok());
        assert!(!service.cache().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires z-billing service at localhost:8081"]
    async fn billing_service_report_llm_usage() {
        let user_uuid = unique_user_uuid();
        let agent_uuid = unique_user_uuid(); // Agent ID must also be a valid UUID

        // Create funded account first
        create_funded_account(&user_uuid, 10000)
            .await
            .expect("Failed to create test account");

        let config = BillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            fail_closed: true,
            account_cache_ttl_seconds: 60,
        };

        let service = BillingService::new(config);

        let event = LlmUsageEvent {
            event_id: format!("evt_{user_uuid}_1"),
            user_id: user_uuid.clone(),
            agent_id: Some(agent_uuid),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            input_tokens: 1000,
            output_tokens: 500,
            metadata: None,
        };

        let result = service.report_llm_usage(event).await;
        assert!(result.is_ok(), "report_llm_usage failed: {result:?}");
    }

    #[tokio::test]
    #[ignore = "requires z-billing service at localhost:8081"]
    async fn billing_service_caches_accounts() {
        let user_uuid = unique_user_uuid();

        let config = BillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            fail_closed: false,
            account_cache_ttl_seconds: 60,
        };

        let service = BillingService::new(config);

        // First call
        service.ensure_account(&user_uuid).await.unwrap();
        assert_eq!(service.cache().len(), 1);

        // Second call - should use cache
        service.ensure_account(&user_uuid).await.unwrap();
        assert_eq!(service.cache().len(), 1);

        // Different user
        let user_uuid_2 = unique_user_uuid();
        service.ensure_account(&user_uuid_2).await.unwrap();
        assert_eq!(service.cache().len(), 2);
    }

    #[tokio::test]
    #[ignore = "requires z-billing service at localhost:8081"]
    async fn full_llm_usage_flow() {
        let user_uuid = unique_user_uuid();
        let agent_uuid = unique_user_uuid(); // Agent ID must also be a valid UUID

        // Create funded account
        create_funded_account(&user_uuid, 10000)
            .await
            .expect("Failed to create test account");

        let config = BillingConfig {
            url: BILLING_URL.to_string(),
            api_key: SERVICE_API_KEY.to_string(),
            enabled: true,
            fail_closed: true,
            account_cache_ttl_seconds: 60,
        };

        let service = Arc::new(BillingService::new(config));

        // Simulate WebSocket message processing
        let ws_message = r#"{
            "type": "assistant_message_end",
            "message_id": "msg_flow_test",
            "provider": "anthropic",
            "model": "claude-sonnet-4-20250514",
            "usage": {
                "input_tokens": 2000,
                "output_tokens": 1000
            }
        }"#;

        // Extract usage
        let usage = try_extract_usage(ws_message).expect("Should extract usage");

        // Create billing event
        let event_id = make_event_id(&format!("sess_{user_uuid}"), &usage.message_id);
        let event = LlmUsageEvent {
            event_id,
            user_id: user_uuid,
            agent_id: Some(agent_uuid),
            provider: usage.provider,
            model: usage.model,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            metadata: None,
        };

        // Report to billing
        let result = service.report_llm_usage(event).await;
        assert!(result.is_ok(), "report_llm_usage failed: {result:?}");
    }
}
