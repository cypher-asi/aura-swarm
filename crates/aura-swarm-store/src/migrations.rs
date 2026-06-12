//! One-time store schema migrations (Swarm TEE upgrade R2).
//!
//! The database carries a `schema_version` key (big-endian `u32`) in the
//! `meta` column family. A database without the key is **schema v1** —
//! the pre-TEE-upgrade shape where agent records may lack `tier` /
//! `storage_encryption` (legacy agents).
//!
//! # v1 → v2 (R2 fleet migration)
//!
//! Rewrites every legacy agent record (`spec.tier == None`) to the
//! all-TEE architecture:
//!
//! * `tier` — the requested resources map to the nearest tier
//!   (cpu ≤ 500m → small, ≤ 1000m → standard, else pro), and the spec's
//!   resources are normalized to that tier's sizes.
//! * `isolation = ConfidentialVM` (`kata-qemu-snp`).
//! * `storage_encryption = Sealed` with the deterministic per-agent key
//!   id `swarm/agents/{agent_id}/state-key`.
//!
//! Records that already carry a tier (everything created after R1, plus
//! legacy agents early-migrated via the tier endpoint) are untouched, so
//! the rewrite is idempotent and safe to rerun. Writes go through
//! `WriteBatch` chunks; the `schema_version` bump to 2 rides in the final
//! batch, so a crash mid-migration leaves the version at 1 and the next
//! startup simply redoes the (idempotent) pass.
//!
//! The migration is invoked automatically by the gateway at startup,
//! before the API starts serving (see `aura-swarm-gateway/src/main.rs`).
//!
//! # Legacy decode boundary
//!
//! This module is the **only** place allowed to decode the legacy (v1)
//! CBOR record shape. Today the legacy shape deserializes through the
//! current [`Agent`] struct because the new fields are `#[serde(default)]`
//! `Option`s; in R3 those fields become required on the live structs and
//! the frozen legacy-shaped structs move **here** (the migration module
//! retains the only legacy decode path, for straggler DBs restored from
//! backup).

use rocksdb::WriteBatch;

use crate::error::{Result, StoreError};
use crate::rocks::RocksStore;
use crate::schema::cf;
use crate::types::{Agent, BoxTier, IsolationLevel, StorageEncryption};

/// Key in the `meta` CF holding the schema version (big-endian `u32`).
pub const SCHEMA_VERSION_KEY: &[u8] = b"schema_version";

/// The implicit version of a database without a `schema_version` key.
pub const SCHEMA_VERSION_V1: u32 = 1;

/// The schema version this build writes and expects.
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// Agent records rewritten per `WriteBatch`. Chunking keeps individual
/// batches small; idempotency (not batch atomicity) is the crash-safety
/// mechanism, since the version bump only lands in the final batch.
const MIGRATION_BATCH_SIZE: usize = 128;

/// Outcome of a [`run`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationSummary {
    /// Schema version found on disk before the pass.
    pub from_version: u32,
    /// Schema version after the pass (always [`CURRENT_SCHEMA_VERSION`]).
    pub to_version: u32,
    /// Number of legacy agent records rewritten in this pass.
    pub agents_migrated: u32,
}

/// Run all pending schema migrations on the store.
///
/// Must be called once at startup, after the database is opened and
/// before any request is served. Safe to call on an already-current
/// database (no-op) and safe to rerun after a crash mid-migration.
///
/// # Errors
///
/// Returns an error if the database cannot be read/written, if a stored
/// record fails to decode, or if the on-disk version is **newer** than
/// this build understands (refusing to run old code against a future
/// schema).
pub fn run(store: &RocksStore) -> Result<MigrationSummary> {
    let from_version = store.read_schema_version()?;

    if from_version > CURRENT_SCHEMA_VERSION {
        return Err(StoreError::Database(format!(
            "database schema version {from_version} is newer than this build's \
             supported version {CURRENT_SCHEMA_VERSION}; refusing to start"
        )));
    }

    if from_version == CURRENT_SCHEMA_VERSION {
        return Ok(MigrationSummary {
            from_version,
            to_version: CURRENT_SCHEMA_VERSION,
            agents_migrated: 0,
        });
    }

    let agents_migrated = migrate_v1_to_v2(store)?;

    Ok(MigrationSummary {
        from_version,
        to_version: CURRENT_SCHEMA_VERSION,
        agents_migrated,
    })
}

/// Rewrite every legacy agent record to the tiered/sealed v2 shape and
/// stamp `schema_version = 2`.
fn migrate_v1_to_v2(store: &RocksStore) -> Result<u32> {
    let cf_agents = store.cf(cf::AGENTS)?;
    let cf_meta = store.cf(cf::META)?;

    // Collect rewrites first (iterator borrows the DB), then apply in
    // chunked WriteBatches. Fleet sizes are far below memory limits.
    let mut rewrites: Vec<(Box<[u8]>, Vec<u8>)> = Vec::new();

    let iter = store
        .db
        .iterator_cf(&cf_agents, rocksdb::IteratorMode::Start);
    for item in iter {
        let (key, value) = item.map_err(|e| StoreError::Database(e.to_string()))?;

        // Legacy (v1) records decode through the current `Agent` struct
        // because `tier` / `storage_encryption` are `#[serde(default)]`.
        // R3 note: when those fields become required, the frozen legacy
        // struct definitions move into this module and this is the only
        // place that may decode them.
        let mut agent: Agent = ciborium::from_reader(value.as_ref())
            .map_err(|e| StoreError::Serialization(format!("decoding v1 agent record: {e}")))?;

        if agent.spec.tier.is_some() {
            // Already on the new architecture (post-R1 create or early
            // migration via the tier endpoint) — idempotent skip.
            continue;
        }

        let tier = BoxTier::nearest_for_cpu(agent.spec.cpu_millicores);
        agent.spec.cpu_millicores = tier.cpu_millis();
        agent.spec.memory_mb = tier.memory_mb();
        agent.spec.isolation = Some(IsolationLevel::ConfidentialVM);
        agent.spec.tier = Some(tier.as_str().to_string());
        agent.spec.storage_encryption = Some(StorageEncryption::sealed_for(&agent.agent_id));
        agent.updated_at = chrono::Utc::now();

        let mut buf = Vec::new();
        ciborium::into_writer(&agent, &mut buf)
            .map_err(|e| StoreError::Serialization(e.to_string()))?;
        rewrites.push((key, buf));
    }

    let migrated = u32::try_from(rewrites.len()).unwrap_or(u32::MAX);

    // The user/status indexes key on user_id/status, neither of which
    // changes here, so only the primary records need rewriting.
    let mut chunks = rewrites.chunks(MIGRATION_BATCH_SIZE).peekable();
    let mut wrote_version = false;
    while let Some(chunk) = chunks.next() {
        let mut batch = WriteBatch::default();
        for (key, value) in chunk {
            batch.put_cf(&cf_agents, key, value);
        }
        if chunks.peek().is_none() {
            // Version bump rides in the last batch: a crash before this
            // point leaves the DB at v1 and the next startup reruns the
            // idempotent pass.
            batch.put_cf(
                &cf_meta,
                SCHEMA_VERSION_KEY,
                CURRENT_SCHEMA_VERSION.to_be_bytes(),
            );
            wrote_version = true;
        }
        store
            .db
            .write(batch)
            .map_err(|e| StoreError::Database(e.to_string()))?;
    }

    if !wrote_version {
        // No legacy records at all — just stamp the version.
        store
            .db
            .put_cf(
                &cf_meta,
                SCHEMA_VERSION_KEY,
                CURRENT_SCHEMA_VERSION.to_be_bytes(),
            )
            .map_err(|e| StoreError::Database(e.to_string()))?;
    }

    Ok(migrated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentState;
    use crate::Store;
    use aura_swarm_core::{AgentId, UserId};
    use chrono::{DateTime, Utc};
    use serde::Serialize;
    use tempfile::TempDir;

    fn create_test_store() -> (RocksStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = RocksStore::open(dir.path()).unwrap();
        (store, dir)
    }

    /// Exact pre-upgrade (v1) record shape, serialized without the
    /// `tier` / `storage_encryption` / `error_message` fields — the same
    /// frozen shape the phase-1 regression test uses.
    #[derive(Serialize)]
    struct LegacyAgentSpec {
        cpu_millicores: u32,
        memory_mb: u32,
        runtime_version: String,
        isolation: Option<IsolationLevel>,
    }

    #[derive(Serialize)]
    struct LegacyAgent {
        agent_id: AgentId,
        user_id: UserId,
        name: String,
        status: AgentState,
        spec: LegacyAgentSpec,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        last_heartbeat_at: Option<DateTime<Utc>>,
    }

    /// Write a raw legacy-CBOR agent record straight into the agents CF
    /// (bypassing `put_agent`, which would serialize the modern shape),
    /// plus its index entries like the v1 code did.
    fn put_legacy_agent(store: &RocksStore, cpu_millicores: u32, status: AgentState) -> AgentId {
        let agent_id = AgentId::generate();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let now = Utc::now();
        let legacy = LegacyAgent {
            agent_id,
            user_id,
            name: format!("legacy-{cpu_millicores}m"),
            status,
            spec: LegacyAgentSpec {
                cpu_millicores,
                memory_mb: 512,
                runtime_version: "v1".to_string(),
                isolation: Some(IsolationLevel::MicroVM),
            },
            created_at: now,
            updated_at: now,
            last_heartbeat_at: None,
        };

        let mut buf = Vec::new();
        ciborium::into_writer(&legacy, &mut buf).unwrap();
        let cf_agents = store.cf(cf::AGENTS).unwrap();
        store
            .db
            .put_cf(&cf_agents, crate::keys::agent_key(&agent_id), buf)
            .unwrap();
        let cf_by_user = store.cf(cf::AGENTS_BY_USER).unwrap();
        store
            .db
            .put_cf(&cf_by_user, crate::keys::user_agent_key(&user_id, &agent_id), [])
            .unwrap();
        let cf_by_status = store.cf(cf::AGENTS_BY_STATUS).unwrap();
        store
            .db
            .put_cf(
                &cf_by_status,
                crate::keys::status_agent_key(status.as_u8(), &agent_id),
                [],
            )
            .unwrap();
        agent_id
    }

    fn put_modern_agent(store: &RocksStore, tier: BoxTier) -> AgentId {
        let agent_id = AgentId::generate();
        let now = Utc::now();
        let agent = Agent {
            agent_id,
            user_id: UserId::from_uuid(uuid::Uuid::new_v4()),
            name: "modern".to_string(),
            status: AgentState::Running,
            spec: tier.to_spec(&agent_id, "latest"),
            created_at: now,
            updated_at: now,
            last_heartbeat_at: None,
            error_message: None,
        };
        store.put_agent(&agent).unwrap();
        agent_id
    }

    #[test]
    fn fresh_database_reports_v1_until_migrated() {
        let (store, _dir) = create_test_store();
        assert_eq!(store.read_schema_version().unwrap(), SCHEMA_VERSION_V1);

        let summary = run(&store).unwrap();
        assert_eq!(summary.from_version, SCHEMA_VERSION_V1);
        assert_eq!(summary.to_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(summary.agents_migrated, 0);
        assert_eq!(store.read_schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn legacy_records_rewritten_with_nearest_tier_mapping() {
        let (store, _dir) = create_test_store();
        let small = put_legacy_agent(&store, 250, AgentState::Hibernating);
        let small_edge = put_legacy_agent(&store, 500, AgentState::Running);
        let standard = put_legacy_agent(&store, 1000, AgentState::Stopped);
        let pro = put_legacy_agent(&store, 2000, AgentState::Idle);

        let summary = run(&store).unwrap();
        assert_eq!(summary.agents_migrated, 4);
        assert_eq!(store.read_schema_version().unwrap(), CURRENT_SCHEMA_VERSION);

        for (agent_id, expected_tier) in [
            (small, BoxTier::Small),
            (small_edge, BoxTier::Small),
            (standard, BoxTier::Standard),
            (pro, BoxTier::Pro),
        ] {
            let agent = store.get_agent(&agent_id).unwrap().unwrap();
            assert_eq!(agent.spec.tier.as_deref(), Some(expected_tier.as_str()));
            assert_eq!(agent.spec.cpu_millicores, expected_tier.cpu_millis());
            assert_eq!(agent.spec.memory_mb, expected_tier.memory_mb());
            assert_eq!(agent.spec.isolation, Some(IsolationLevel::ConfidentialVM));
            assert_eq!(
                agent.spec.storage_encryption,
                Some(StorageEncryption::sealed_for(&agent_id)),
                "sealed key id must be the deterministic per-agent id"
            );
        }
    }

    #[test]
    fn migration_preserves_identity_status_and_indexes() {
        let (store, _dir) = create_test_store();
        let agent_id = put_legacy_agent(&store, 1000, AgentState::Hibernating);
        run(&store).unwrap();

        let agent = store.get_agent(&agent_id).unwrap().unwrap();
        assert_eq!(agent.agent_id, agent_id, "AgentId must be preserved");
        assert_eq!(agent.status, AgentState::Hibernating);

        // Index reads still resolve the rewritten record.
        let by_status = store.list_agents_by_status(AgentState::Hibernating).unwrap();
        assert_eq!(by_status.len(), 1);
        assert_eq!(by_status[0].agent_id, agent_id);
        let by_user = store.list_agents_by_user(&agent.user_id).unwrap();
        assert_eq!(by_user.len(), 1);
    }

    #[test]
    fn already_tiered_records_are_skipped() {
        let (store, _dir) = create_test_store();
        let modern_id = put_modern_agent(&store, BoxTier::Pro);
        let before = store.get_agent(&modern_id).unwrap().unwrap();
        put_legacy_agent(&store, 100, AgentState::Running);

        let summary = run(&store).unwrap();
        assert_eq!(summary.agents_migrated, 1, "only the legacy record migrates");

        let after = store.get_agent(&modern_id).unwrap().unwrap();
        assert_eq!(after.spec.tier.as_deref(), Some("pro"));
        assert_eq!(after.updated_at, before.updated_at, "modern record untouched");
    }

    #[test]
    fn rerun_is_idempotent_and_already_v2_is_noop() {
        let (store, _dir) = create_test_store();
        put_legacy_agent(&store, 750, AgentState::Running);

        let first = run(&store).unwrap();
        assert_eq!(first.agents_migrated, 1);

        let agents_after_first = store.list_all_agents().unwrap();

        let second = run(&store).unwrap();
        assert_eq!(second.from_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(second.agents_migrated, 0, "already-v2 DB must be a no-op");

        let agents_after_second = store.list_all_agents().unwrap();
        assert_eq!(agents_after_first.len(), agents_after_second.len());
        for (a, b) in agents_after_first.iter().zip(&agents_after_second) {
            assert_eq!(a.updated_at, b.updated_at, "no-op rerun must not rewrite");
        }
    }

    /// Crash-safety model: an interrupted pass (version still v1, some
    /// records already rewritten) reruns cleanly without disturbing the
    /// already-migrated records' tier assignment.
    #[test]
    fn interrupted_pass_reruns_safely() {
        let (store, _dir) = create_test_store();
        let migrated_early = put_legacy_agent(&store, 100, AgentState::Running);
        put_legacy_agent(&store, 2000, AgentState::Running);

        // Simulate "crashed after rewriting one record, before the
        // version bump": run a full pass, then knock the version back.
        run(&store).unwrap();
        let cf_meta = store.cf(cf::META).unwrap();
        store.db.delete_cf(&cf_meta, SCHEMA_VERSION_KEY).unwrap();
        assert_eq!(store.read_schema_version().unwrap(), SCHEMA_VERSION_V1);

        let summary = run(&store).unwrap();
        assert_eq!(summary.agents_migrated, 0, "rewritten records are skipped");
        assert_eq!(store.read_schema_version().unwrap(), CURRENT_SCHEMA_VERSION);

        let agent = store.get_agent(&migrated_early).unwrap().unwrap();
        assert_eq!(agent.spec.tier.as_deref(), Some("small"));
    }

    #[test]
    fn future_schema_version_refuses_to_run() {
        let (store, _dir) = create_test_store();
        let cf_meta = store.cf(cf::META).unwrap();
        store
            .db
            .put_cf(&cf_meta, SCHEMA_VERSION_KEY, 99u32.to_be_bytes())
            .unwrap();

        let err = run(&store).unwrap_err();
        assert!(err.to_string().contains("newer"), "got: {err}");
    }

    #[test]
    fn schema_version_survives_reopen() {
        let dir = TempDir::new().unwrap();
        {
            let store = RocksStore::open(dir.path()).unwrap();
            run(&store).unwrap();
        }
        let store = RocksStore::open(dir.path()).unwrap();
        assert_eq!(store.read_schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    }
}
