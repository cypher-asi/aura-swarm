//! Process-trigger registration types and validation (Swarm TEE
//! upgrade phase 8, "trigger outside, data inside").
//!
//! Agents export trigger metadata — and **only** trigger metadata —
//! to the control plane so the external cron service knows *when* to
//! fire a process, never *what* it does. [`TriggerRegistration`] is
//! the explicit DTO for that boundary: it structurally cannot carry a
//! prompt, config, or run data, and any extra fields a caller sends
//! are stripped during deserialization.

use aura_swarm_core::AgentId;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::str::FromStr;

use crate::error::{ControlError, Result};

/// Maximum length of a registered process id.
pub const MAX_PROCESS_ID_LEN: usize = 128;

/// Maximum number of triggers a single agent may register.
pub const MAX_TRIGGERS_PER_AGENT: usize = 256;

/// One trigger in an agent's desired registration set.
///
/// # Trust boundary
///
/// This is the **only** shape in which process-derived data may enter
/// the control plane. The harness sends exactly
/// `(process_id, cron, enabled, next_run_at)`; serde silently drops
/// anything else (no `deny_unknown_fields` — a harness that
/// accidentally widens its payload must not break registration, and
/// stripping is the safe behavior for data we must not hold).
#[derive(Debug, Clone, Deserialize)]
pub struct TriggerRegistration {
    /// Process id (opaque to the control plane).
    pub process_id: String,
    /// Cron expression (UTC); validated server-side.
    pub cron: String,
    /// Whether the cron service should fire this trigger.
    pub enabled: bool,
    /// Next fire time computed inside the agent.
    #[serde(default)]
    pub next_run_at: Option<DateTime<Utc>>,
}

impl TriggerRegistration {
    /// Validate the registration and convert it into a store record.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::InvalidTrigger`] when the process id or
    /// cron expression is invalid.
    pub fn into_trigger(self, agent_id: &AgentId) -> Result<aura_swarm_store::ProcessTrigger> {
        validate_process_id(&self.process_id)?;
        validate_cron(&self.cron)?;
        let now = Utc::now();
        Ok(aura_swarm_store::ProcessTrigger {
            agent_id: *agent_id,
            process_id: self.process_id,
            cron: self.cron,
            enabled: self.enabled,
            next_run_at: self.next_run_at,
            last_run_at: None,
            registered_at: now,
            updated_at: now,
        })
    }
}

/// Validate a process id before it becomes part of a store key
/// (`agent_id || process_id`) or a URL path segment.
///
/// Allows the same conservative charset as other proxied identifiers
/// (`[A-Za-z0-9._-]`, bounded length); harness process ids are UUID
/// strings and fit comfortably.
///
/// # Errors
///
/// Returns [`ControlError::InvalidTrigger`] for empty, oversized, or
/// out-of-charset ids.
pub fn validate_process_id(process_id: &str) -> Result<()> {
    if process_id.is_empty()
        || process_id.len() > MAX_PROCESS_ID_LEN
        || !process_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(ControlError::InvalidTrigger(format!(
            "invalid process id: {process_id}"
        )));
    }
    Ok(())
}

/// Validate a cron expression with the same semantics as the harness
/// (`aura-store-db`): standard 5-field expressions are normalized by
/// prepending a `0` seconds field for the `cron` crate; 6/7-field
/// expressions pass through unchanged.
///
/// # Errors
///
/// Returns [`ControlError::InvalidTrigger`] when the expression does
/// not parse.
pub fn validate_cron(expr: &str) -> Result<()> {
    parse_cron(expr).map(|_| ())
}

/// Parse a cron expression after harness-compatible normalization
/// (5-field expressions get a `0` seconds field prepended).
fn parse_cron(expr: &str) -> Result<cron::Schedule> {
    if expr.trim().is_empty() {
        return Err(ControlError::InvalidTrigger(
            "cron expression must not be empty".into(),
        ));
    }
    let normalized = if expr.split_whitespace().count() == 5 {
        format!("0 {}", expr.trim())
    } else {
        expr.trim().to_string()
    };
    cron::Schedule::from_str(&normalized)
        .map_err(|e| ControlError::InvalidTrigger(format!("invalid cron expression: {e}")))
}

/// Compute the next occurrence of `expr` strictly after `after` (UTC).
///
/// Returns `Ok(None)` when the schedule has no future occurrence (e.g.
/// a year-bound expression entirely in the past).
///
/// # Errors
///
/// Returns [`ControlError::InvalidTrigger`] when the expression does
/// not parse.
pub fn next_occurrence_after(
    expr: &str,
    after: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let schedule = parse_cron(expr)?;
    Ok(schedule.after(&after).next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_standard_and_extended_cron() {
        assert!(validate_cron("*/5 * * * *").is_ok());
        assert!(validate_cron("0 0 * * 1").is_ok());
        assert!(validate_cron("0 */10 * * * *").is_ok()); // 6-field
        assert!(validate_cron("0 0 0 1 1 ? 2030").is_ok()); // 7-field
    }

    #[test]
    fn rejects_bad_cron() {
        assert!(validate_cron("").is_err());
        assert!(validate_cron("not a cron").is_err());
        assert!(validate_cron("99 * * * *").is_err());
    }

    #[test]
    fn process_id_charset() {
        assert!(validate_process_id("0d9af1f2-1c2b-4f4e-9a51-0c8e2f8b1a23").is_ok());
        assert!(validate_process_id("proc_1.x").is_ok());
        assert!(validate_process_id("").is_err());
        assert!(validate_process_id("a/b").is_err());
        assert!(validate_process_id("a b").is_err());
        assert!(validate_process_id(&"a".repeat(MAX_PROCESS_ID_LEN + 1)).is_err());
    }

    #[test]
    fn next_occurrence_is_strictly_after() {
        use chrono::TimeZone;
        let after = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let next = next_occurrence_after("*/5 * * * *", after).unwrap().unwrap();
        assert!(next > after);
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 1, 0, 5, 0).unwrap());
    }

    #[test]
    fn next_occurrence_none_when_exhausted() {
        use chrono::TimeZone;
        // Year-bound schedule entirely in the past.
        let after = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let next = next_occurrence_after("0 0 0 1 1 ? 2020", after).unwrap();
        assert!(next.is_none());
    }

    #[test]
    fn registration_strips_unknown_fields() {
        // Extra fields (e.g. a prompt that must never cross the trust
        // boundary) are silently dropped by the explicit DTO.
        let json = serde_json::json!({
            "process_id": "p1",
            "cron": "*/5 * * * *",
            "enabled": true,
            "next_run_at": null,
            "prompt": "SECRET — must never be stored",
            "config": {"k": "v"}
        });
        let reg: TriggerRegistration = serde_json::from_value(json).unwrap();
        let trigger = reg.into_trigger(&AgentId::generate()).unwrap();
        let stored = serde_json::to_string(&trigger).unwrap();
        assert!(!stored.contains("SECRET"));
        assert!(!stored.contains("prompt"));
    }
}
