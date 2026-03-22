//! LLM usage extraction from WebSocket messages.
//!
//! Parses `assistant_message_end` events to extract token usage data.

use serde::Deserialize;

/// Extracted LLM usage information from an assistant message end event.
#[derive(Debug, Clone)]
pub struct ExtractedUsage {
    /// Message ID for idempotency.
    pub message_id: String,
    /// LLM provider (e.g., "anthropic", "openai").
    pub provider: String,
    /// Model name (e.g., "claude-sonnet-4-20250514").
    pub model: String,
    /// Number of input tokens.
    pub input_tokens: u64,
    /// Number of output tokens.
    pub output_tokens: u64,
}

/// Raw message structure for WebSocket text messages.
#[derive(Debug, Deserialize)]
struct WsMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    message_id: Option<String>,
    usage: Option<UsageData>,
    model: Option<String>,
    provider: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageData {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    /// Model name (harness protocol nests this inside usage).
    model: Option<String>,
    /// Provider name (harness protocol nests this inside usage).
    provider: Option<String>,
}

/// Try to extract LLM usage from a WebSocket text message.
///
/// Returns `Some(ExtractedUsage)` if this is an `assistant_message_end` event
/// with token usage data, `None` otherwise.
#[must_use]
pub fn try_extract_usage(text: &str) -> Option<ExtractedUsage> {
    let msg: WsMessage = serde_json::from_str(text).ok()?;

    // Only process assistant_message_end events
    if msg.msg_type.as_deref() != Some("assistant_message_end") {
        return None;
    }

    let message_id = msg.message_id?;
    let usage = msg.usage?;
    let input_tokens = usage.input_tokens.unwrap_or(0);
    let output_tokens = usage.output_tokens.unwrap_or(0);

    // Skip if no tokens were used
    if input_tokens == 0 && output_tokens == 0 {
        return None;
    }

    // Model and provider can be at the top level (legacy) or inside usage (harness).
    let provider = msg
        .provider
        .or(usage.provider)
        .unwrap_or_else(|| "unknown".to_string());
    let model = msg
        .model
        .or(usage.model)
        .unwrap_or_else(|| "unknown".to_string());

    Some(ExtractedUsage {
        message_id,
        provider,
        model,
        input_tokens,
        output_tokens,
    })
}

/// Generate a unique event ID for billing from session and message IDs.
#[must_use]
pub fn make_event_id(session_id: &str, message_id: &str) -> String {
    format!("{session_id}:{message_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_usage_success() {
        let msg = r#"{
            "type": "assistant_message_end",
            "message_id": "msg_123",
            "provider": "anthropic",
            "model": "claude-sonnet-4-20250514",
            "usage": {
                "input_tokens": 1000,
                "output_tokens": 500
            }
        }"#;

        let usage = try_extract_usage(msg).unwrap();
        assert_eq!(usage.message_id, "msg_123");
        assert_eq!(usage.provider, "anthropic");
        assert_eq!(usage.model, "claude-sonnet-4-20250514");
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 500);
    }

    #[test]
    fn extract_usage_wrong_type() {
        let msg = r#"{
            "type": "text_delta",
            "text": "Hello"
        }"#;

        assert!(try_extract_usage(msg).is_none());
    }

    #[test]
    fn extract_usage_no_tokens() {
        let msg = r#"{
            "type": "assistant_message_end",
            "message_id": "msg_123",
            "usage": {}
        }"#;

        assert!(try_extract_usage(msg).is_none());
    }

    #[test]
    fn extract_usage_harness_format() {
        let msg = r#"{
            "type": "assistant_message_end",
            "message_id": "msg_456",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 2000,
                "output_tokens": 800,
                "cumulative_input_tokens": 5000,
                "cumulative_output_tokens": 2000,
                "context_utilization": 0.45,
                "model": "claude-opus-4-6-20250514",
                "provider": "anthropic"
            },
            "files_changed": {
                "created": [],
                "modified": ["src/main.rs"],
                "deleted": []
            }
        }"#;

        let usage = try_extract_usage(msg).unwrap();
        assert_eq!(usage.message_id, "msg_456");
        assert_eq!(usage.model, "claude-opus-4-6-20250514");
        assert_eq!(usage.provider, "anthropic");
        assert_eq!(usage.input_tokens, 2000);
        assert_eq!(usage.output_tokens, 800);
    }

    #[test]
    fn extract_usage_harness_empty_provider() {
        let msg = r#"{
            "type": "assistant_message_end",
            "message_id": "msg_789",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "model": "claude-opus-4-6-20250514",
                "provider": ""
            }
        }"#;

        let usage = try_extract_usage(msg).unwrap();
        assert_eq!(usage.provider, "");
        assert_eq!(usage.model, "claude-opus-4-6-20250514");
    }

    #[test]
    fn make_event_id_format() {
        let id = make_event_id("sess_abc", "msg_123");
        assert_eq!(id, "sess_abc:msg_123");
    }
}
