//! Database schema definitions and column families.
//!
//! This module defines the column families used in `RocksDB` storage.

/// Column family names for the `RocksDB` database.
pub mod cf {
    /// Primary agent records, keyed by `agent_id`.
    pub const AGENTS: &str = "agents";

    /// Index: agents by status, keyed by `status || agent_id`.
    pub const AGENTS_BY_STATUS: &str = "agents_by_status";

    /// Index: agents by user, keyed by `user_id || agent_id`.
    pub const AGENTS_BY_USER: &str = "agents_by_user";

    /// Primary session records, keyed by `session_id`.
    pub const SESSIONS: &str = "sessions";

    /// Index: sessions by agent, keyed by `agent_id || session_id`.
    pub const SESSIONS_BY_AGENT: &str = "sessions_by_agent";

    /// User records (synced from zOS), keyed by `user_id`.
    pub const USERS: &str = "users";

    /// Process trigger metadata registered by agents, keyed by
    /// `agent_id || process_id`. Holds only `(process_id, cron, enabled)`
    /// style metadata — process payloads stay sealed inside the agent.
    pub const PROCESS_TRIGGERS: &str = "process_triggers";

    /// Usage events for cost/usage aggregation, keyed by
    /// `agent_id || timestamp_millis` (big-endian for time-ordered scans).
    pub const USAGE_EVENTS: &str = "usage_events";

    /// Pod-log snapshots captured by the scheduler on pod termination,
    /// keyed by `agent_id || captured_at_millis` (big-endian). Capped per
    /// agent (oldest snapshots pruned on insert) — see
    /// `LOG_SNAPSHOTS_PER_AGENT_CAP` in the rocks implementation.
    pub const AGENT_LOGS: &str = "agent_logs";
}

/// Returns all column family names for database initialization.
///
/// Column families listed here are auto-created on database open
/// (`create_missing_column_families`), so adding a new CF requires no
/// explicit migration.
#[must_use]
pub fn all_column_families() -> Vec<&'static str> {
    vec![
        cf::AGENTS,
        cf::AGENTS_BY_STATUS,
        cf::AGENTS_BY_USER,
        cf::SESSIONS,
        cf::SESSIONS_BY_AGENT,
        cf::USERS,
        cf::PROCESS_TRIGGERS,
        cf::USAGE_EVENTS,
        cf::AGENT_LOGS,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_column_families_complete() {
        let cfs = all_column_families();
        assert_eq!(
            cfs,
            vec![
                cf::AGENTS,
                cf::AGENTS_BY_STATUS,
                cf::AGENTS_BY_USER,
                cf::SESSIONS,
                cf::SESSIONS_BY_AGENT,
                cf::USERS,
                cf::PROCESS_TRIGGERS,
                cf::USAGE_EVENTS,
                cf::AGENT_LOGS,
            ]
        );
    }
}
