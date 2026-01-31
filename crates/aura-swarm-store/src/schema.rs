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

    /// User records (synced from Zero-ID), keyed by `user_id`.
    pub const USERS: &str = "users";
}

/// Returns all column family names for database initialization.
#[must_use]
pub fn all_column_families() -> Vec<&'static str> {
    vec![
        cf::AGENTS,
        cf::AGENTS_BY_STATUS,
        cf::AGENTS_BY_USER,
        cf::SESSIONS,
        cf::SESSIONS_BY_AGENT,
        cf::USERS,
    ]
}
