//! Domain types stored in the database.
//!
//! These types represent the persisted state of agents, sessions, and users.

use aura_swarm_core::{AgentId, SessionId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// An agent record stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// Unique identifier for the agent.
    pub agent_id: AgentId,
    /// Owner user ID (from zOS).
    pub user_id: UserId,
    /// Human-readable name.
    pub name: String,
    /// Current lifecycle state.
    pub status: AgentState,
    /// Resource specification.
    pub spec: AgentSpec,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last modification timestamp.
    pub updated_at: DateTime<Utc>,
    /// Last heartbeat from the agent runtime.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Error message when agent is in Error state (e.g., provisioning failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Resource specification for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    /// CPU allocation in millicores.
    pub cpu_millicores: u32,
    /// Memory allocation in megabytes.
    pub memory_mb: u32,
    /// Aura runtime version.
    pub runtime_version: String,
    /// Isolation level for the agent runtime.
    /// If not specified, uses the scheduler's default.
    #[serde(default)]
    pub isolation: Option<IsolationLevel>,
    /// Box tier this spec was derived from (e.g. "standard").
    ///
    /// Required since R3 of the TEE upgrade: every live record carries a
    /// tier (the R2 migration rewrote legacy records). The only decode
    /// path for pre-tier record shapes lives in `migrations.rs`.
    pub tier: String,
    /// Storage encryption mode for the agent's persistent state.
    ///
    /// Required since R3 of the TEE upgrade: every agent's state is
    /// sealed. The only decode path for pre-sealed record shapes lives
    /// in `migrations.rs`.
    pub storage_encryption: StorageEncryption,
}

/// Storage encryption mode for an agent's persistent state volume.
///
/// Every agent's state is `Sealed`; the enum exists so future modes
/// (e.g. key rotation schemes) stay representable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageEncryption {
    /// State is encrypted inside the guest with a per-agent data encryption
    /// key. For confidential VMs the key is released by the KBS only after
    /// successful attestation; the host/EFS only ever sees ciphertext.
    Sealed {
        /// KBS key identifier for the per-agent state DEK
        /// (see [`StorageEncryption::state_key_id`]).
        key_id: String,
    },
}

impl StorageEncryption {
    /// Deterministic KBS key id for an agent's state DEK:
    /// `swarm/agents/{agent_id}/state-key`.
    #[must_use]
    pub fn state_key_id(agent_id: &AgentId) -> String {
        format!("swarm/agents/{agent_id}/state-key")
    }

    /// Build the sealed-storage descriptor for an agent, using the
    /// deterministic per-agent key id.
    #[must_use]
    pub fn sealed_for(agent_id: &AgentId) -> Self {
        Self::Sealed {
            key_id: Self::state_key_id(agent_id),
        }
    }

    /// The KBS key id backing this encryption mode.
    #[must_use]
    pub fn key_id(&self) -> &str {
        match self {
            Self::Sealed { key_id } => key_id,
        }
    }
}

/// Box tiers: the predefined VM SKUs a user can choose when creating an agent.
///
/// Every tier is a confidential SEV-SNP VM with sealed per-agent storage —
/// tiers differ only in size and hourly price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BoxTier {
    /// 500m CPU / 1Gi memory.
    Small,
    /// 1000m CPU / 2Gi memory. The default tier.
    #[default]
    Standard,
    /// 2000m CPU / 4Gi memory.
    Pro,
}

impl BoxTier {
    /// All known tiers, smallest first.
    pub const ALL: [Self; 3] = [Self::Small, Self::Standard, Self::Pro];

    /// Short tier name used in APIs ("small" / "standard" / "pro").
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Standard => "standard",
            Self::Pro => "pro",
        }
    }

    /// Stable billing SKU identifier (e.g. "swarm.small").
    #[must_use]
    pub const fn sku(&self) -> &'static str {
        match self {
            Self::Small => "swarm.small",
            Self::Standard => "swarm.standard",
            Self::Pro => "swarm.pro",
        }
    }

    /// Resolve a tier from its short name (case-insensitive).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "small" => Some(Self::Small),
            "standard" => Some(Self::Standard),
            "pro" => Some(Self::Pro),
            _ => None,
        }
    }

    /// Map a raw resource request to the nearest tier:
    /// cpu <= 500m -> small, <= 1000m -> standard, else pro.
    ///
    /// Used only by the v1 -> v2 store migration (`migrations.rs`) when
    /// rewriting pre-tier records from straggler DBs restored from backup.
    #[must_use]
    pub const fn nearest_for_cpu(cpu_millicores: u32) -> Self {
        if cpu_millicores <= 500 {
            Self::Small
        } else if cpu_millicores <= 1000 {
            Self::Standard
        } else {
            Self::Pro
        }
    }

    /// CPU allocation in millicores.
    #[must_use]
    pub const fn cpu_millis(&self) -> u32 {
        match self {
            Self::Small => 500,
            Self::Standard => 1000,
            Self::Pro => 2000,
        }
    }

    /// Memory allocation in megabytes.
    #[must_use]
    pub const fn memory_mb(&self) -> u32 {
        match self {
            Self::Small => 1024,
            Self::Standard => 2048,
            Self::Pro => 4096,
        }
    }

    /// Isolation level: always a confidential SEV-SNP VM.
    #[must_use]
    pub const fn isolation(&self) -> IsolationLevel {
        IsolationLevel::ConfidentialVM
    }

    /// Platform price per hour of pod runtime, in cents.
    #[must_use]
    pub const fn hourly_price_cents(&self) -> u32 {
        match self {
            Self::Small => 4,
            Self::Standard => 8,
            Self::Pro => 15,
        }
    }

    /// Build the full resource spec for an agent created on this tier:
    /// tier-sized resources, confidential isolation, and sealed storage
    /// keyed by the agent's deterministic state-key id.
    #[must_use]
    pub fn to_spec(self, agent_id: &AgentId, runtime_version: impl Into<String>) -> AgentSpec {
        AgentSpec {
            cpu_millicores: self.cpu_millis(),
            memory_mb: self.memory_mb(),
            runtime_version: runtime_version.into(),
            isolation: Some(self.isolation()),
            tier: self.as_str().to_string(),
            storage_encryption: StorageEncryption::sealed_for(agent_id),
        }
    }
}

impl std::fmt::Display for BoxTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for BoxTier {
    type Err = UnknownBoxTier;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::from_name(s).ok_or_else(|| UnknownBoxTier(s.to_string()))
    }
}

/// Error returned when parsing an unknown tier name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownBoxTier(pub String);

impl std::fmt::Display for UnknownBoxTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown box tier: {} (expected small/standard/pro)", self.0)
    }
}

impl std::error::Error for UnknownBoxTier {}

/// Isolation level for agent execution.
///
/// Determines whether the agent runs in a lightweight container
/// or a confidential (TEE) VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    /// Run in a standard container (shared kernel).
    /// Faster startup, lower overhead, less isolation.
    /// Survives strictly for local dev-mode.
    Container,
    /// Run in a confidential SEV-SNP VM (`CoCo`: Kata + QEMU) with
    /// attestation-gated sealed storage. The level for all agents.
    #[default]
    #[serde(rename = "confidential_vm")]
    ConfidentialVM,
}

impl IsolationLevel {
    /// Get the Kubernetes `RuntimeClass` name for this isolation level.
    ///
    /// Returns `None` for container isolation (uses default runtime) or
    /// `Some("kata-remote")` for confidential VM isolation (Peer Pods /
    /// Cloud API Adaptor: a per-agent AWS-managed SEV-SNP pod VM).
    #[must_use]
    pub const fn runtime_class(&self) -> Option<&'static str> {
        match self {
            Self::Container => None, // Use default container runtime
            Self::ConfidentialVM => Some("kata-remote"),
        }
    }
}

/// Lifecycle states for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum AgentState {
    /// Pod is being created, Aura initializing.
    Provisioning = 1,
    /// Agent is active and accepting sessions.
    Running = 2,
    /// No active sessions, still running.
    Idle = 3,
    /// State saved, pod terminated, instant wake.
    Hibernating = 4,
    /// Graceful shutdown in progress.
    Stopping = 5,
    /// Pod terminated, state preserved.
    Stopped = 6,
    /// Health check failed or crash.
    Error = 7,
}

impl AgentState {
    /// Convert the state to its numeric representation.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Try to convert a numeric value to an `AgentState`.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Provisioning),
            2 => Some(Self::Running),
            3 => Some(Self::Idle),
            4 => Some(Self::Hibernating),
            5 => Some(Self::Stopping),
            6 => Some(Self::Stopped),
            7 => Some(Self::Error),
            _ => None,
        }
    }
}

/// A session record stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique identifier for the session.
    pub session_id: SessionId,
    /// Agent this session is connected to.
    pub agent_id: AgentId,
    /// User who owns this session.
    pub user_id: UserId,
    /// Current session status.
    pub status: SessionStatus,
    /// Per-session configuration for the harness runtime.
    #[serde(default)]
    pub config: SessionConfig,
    /// Harness `run_id` this session is attached to, set once the gateway has
    /// created the run via `POST /v1/run`. The WS-attach path resolves the
    /// owning session from this id so the Running -> Idle transition still
    /// fires when the run's stream closes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// When the session was closed (if closed).
    pub closed_at: Option<DateTime<Utc>>,
}

/// Status of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SessionStatus {
    /// Session is active and can receive messages.
    Active = 1,
    /// Session has been closed.
    Closed = 2,
}

/// Per-session configuration for the harness runtime.
///
/// Stored alongside the session record. With the migration to the
/// `POST /v1/run` contract the gateway populates a
/// [`aura_swarm_protocol::RuntimeRequest`] from the run request body rather
/// than from this struct, so these fields are advisory metadata only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionConfig {
    /// System prompt override for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Model identifier override (e.g., "claude-opus-4-6-20250514").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Maximum tokens per model response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Maximum agentic steps per turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Workspace configuration for file operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceConfig>,
}

/// Workspace configuration for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Git repository URL to clone into the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_repo_url: Option<String>,
    /// Git branch to check out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
}

impl SessionStatus {
    /// Convert the status to its numeric representation.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Trigger metadata registered by an agent for one of its processes
/// (Swarm TEE upgrade phase 8, "trigger outside, data inside").
///
/// # Trust boundary
///
/// This record is the **only** process-derived data the control plane
/// ever persists. The process prompt, config, and run history stay
/// sealed inside the agent VM; what lives here is just enough for the
/// external cron service to fire a content-free trigger: which
/// process, on what schedule, whether it is active, and when it fires
/// next. Registration paths must never widen this shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessTrigger {
    /// Agent that owns the process (the VM the trigger fires into).
    pub agent_id: AgentId,
    /// Process id (opaque off-VM; assigned inside the agent).
    pub process_id: String,
    /// Cron expression (UTC) — schedule only, no payload.
    pub cron: String,
    /// Whether the cron service should fire this trigger.
    pub enabled: bool,
    /// Next fire time computed inside the agent, if the schedule has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<DateTime<Utc>>,
    /// When the control-plane cron service last fired this trigger.
    /// Owned by the gateway side; preserved across re-registrations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<DateTime<Utc>>,
    /// When this trigger was first registered (preserved across
    /// re-registrations of the same `(agent_id, process_id)`).
    pub registered_at: DateTime<Utc>,
    /// When this trigger was last (re-)registered or updated.
    pub updated_at: DateTime<Utc>,
}

/// A usage/cost event recorded for an agent (Swarm TEE upgrade phase 10+).
///
/// Events live in the `usage_events` CF keyed by
/// `agent_id || timestamp_millis` (big-endian) so a per-agent prefix scan
/// yields a time-ordered log. Lifecycle transitions append events at the
/// control plane; the usage aggregation layer pairs `PodScheduled` /
/// `PodTerminated` into billable intervals priced at event time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// Unique event id.
    pub event_id: uuid::Uuid,
    /// Agent the event belongs to.
    pub agent_id: AgentId,
    /// When the event occurred.
    pub timestamp: DateTime<Utc>,
    /// What happened.
    pub kind: UsageEventKind,
}

impl UsageEvent {
    /// Build a new event for `agent_id`, stamped now with a fresh id.
    #[must_use]
    pub fn new(agent_id: AgentId, kind: UsageEventKind) -> Self {
        Self {
            event_id: uuid::Uuid::new_v4(),
            agent_id,
            timestamp: Utc::now(),
            kind,
        }
    }

    /// The event timestamp as non-negative Unix millis (the storage-key
    /// timestamp component).
    #[must_use]
    pub fn timestamp_millis(&self) -> u64 {
        u64::try_from(self.timestamp.timestamp_millis()).unwrap_or(0)
    }
}

/// What a [`UsageEvent`] records.
///
/// Internally tagged (`kind`) so the enum can keep growing without
/// breaking CBOR decoding of already-persisted events: decoding
/// dispatches on the stored tag string, never on variant order.
///
/// Pricing fields capture the tier + hourly price **at event time**, so a
/// later re-pricing of a tier never rewrites usage history. Legacy agents
/// (no tier) get events with `tier: None` / `hourly_price_cents: None` —
/// they are billed by cpu/mem-hours via zbilling, not a tier rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageEventKind {
    /// A pod was scheduled for the agent (create, start, wake, or the
    /// schedule half of a tier-change recreate). Opens a billable interval.
    PodScheduled {
        /// Tier of the spec the pod was scheduled with; `None` for legacy.
        tier: Option<String>,
        /// Hourly price of that tier in cents at event time.
        hourly_price_cents: Option<u32>,
    },
    /// The agent's pod was terminated. Closes the open billable interval.
    PodTerminated {
        /// Tier the pod was running as; `None` for legacy.
        tier: Option<String>,
        /// Hourly price of that tier in cents at event time.
        hourly_price_cents: Option<u32>,
        /// Why the pod went away, as known by the call site:
        /// `hibernate` / `stop` / `tier_change` / `crash`.
        reason: String,
    },
    /// The agent was put into hibernation (counter; the paired
    /// `PodTerminated { reason: "hibernate" }` carries the pricing).
    Hibernated,
    /// The agent was woken from hibernation or a stopped state (counter;
    /// the paired `PodScheduled` carries the pricing).
    Woke,
    /// The control-plane cron service delivered a process trigger to the
    /// agent's pod.
    TriggerFired {
        /// The process the trigger fired into (opaque off-VM).
        process_id: String,
    },
    /// The agent's box tier changed (upgrade, downgrade, or legacy-agent
    /// early migration). Both hourly prices are captured at event time so
    /// cost intervals split exactly at the change.
    TierChanged {
        /// Tier before the change; `None` for a legacy agent that was
        /// assigned its first tier (per-agent early migration).
        from: Option<String>,
        /// Tier after the change.
        to: String,
        /// Hourly price of the old tier in cents; `None` for legacy agents
        /// (they were billed by cpu/mem-hours, not a tier rate).
        from_hourly_price_cents: Option<u32>,
        /// Hourly price of the new tier in cents.
        to_hourly_price_cents: u32,
    },
}

/// A single pod-log line with the timestamp parsed from the Kubernetes
/// log timestamp prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    /// When the line was emitted (from the K8s `timestamps=true` prefix).
    pub timestamp: DateTime<Utc>,
    /// The raw log line (timestamp prefix stripped).
    pub line: String,
}

/// A pod-log tail snapshot captured on pod termination (Swarm TEE upgrade
/// phase 12).
///
/// Pod stdout vanishes with the pod, so the scheduler captures a final
/// tail on every termination path and ships it to the gateway, which
/// stores it in the `agent_logs` CF keyed by
/// `agent_id || captured_at_millis` (big-endian). Per-agent storage is
/// capped — inserting a snapshot prunes the oldest beyond the cap.
///
/// Trust boundary: these are VM/platform logs (boot, attestation, health,
/// harness lifecycle) that are host-visible by design. Detailed in-VM
/// agent logs stay sealed inside the guest and never reach this store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLogSnapshot {
    /// Agent whose pod the snapshot was captured from.
    pub agent_id: AgentId,
    /// When the scheduler captured the tail.
    pub captured_at: DateTime<Utc>,
    /// Why the pod was terminated, as known by the scheduler
    /// (e.g. `terminated` / `stale_image`) or the control plane
    /// (`hibernate` / `stop` / `tier_change` / `destroy`).
    pub reason: String,
    /// The captured tail, oldest line first.
    pub entries: Vec<LogLine>,
}

impl AgentLogSnapshot {
    /// The capture timestamp as non-negative Unix millis (the storage-key
    /// timestamp component).
    #[must_use]
    pub fn captured_at_millis(&self) -> u64 {
        u64::try_from(self.captured_at.timestamp_millis()).unwrap_or(0)
    }
}

/// A user record stored in the database (synced from zOS).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// Unique identifier for the user (from zOS).
    pub user_id: UserId,
    /// User's email address.
    pub email: String,
    /// Whether the email has been verified.
    pub email_verified: bool,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last login timestamp.
    pub last_login_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_state_as_u8_roundtrip() {
        let variants = [
            AgentState::Provisioning,
            AgentState::Running,
            AgentState::Idle,
            AgentState::Hibernating,
            AgentState::Stopping,
            AgentState::Stopped,
            AgentState::Error,
        ];
        for state in variants {
            let roundtripped = AgentState::from_u8(state.as_u8());
            assert_eq!(roundtripped, Some(state));
        }
    }

    #[test]
    fn agent_state_from_invalid_u8() {
        assert_eq!(AgentState::from_u8(0), None);
        assert_eq!(AgentState::from_u8(255), None);
    }

    #[test]
    fn session_status_serde_roundtrip() {
        for status in [SessionStatus::Active, SessionStatus::Closed] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: SessionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn isolation_level_runtime_class() {
        assert_eq!(IsolationLevel::Container.runtime_class(), None);
        assert_eq!(
            IsolationLevel::ConfidentialVM.runtime_class(),
            Some("kata-remote")
        );
    }

    #[test]
    fn isolation_level_serde_names() {
        let json = serde_json::to_string(&IsolationLevel::ConfidentialVM).unwrap();
        assert_eq!(json, "\"confidential_vm\"");
        let parsed: IsolationLevel = serde_json::from_str("\"confidential_vm\"").unwrap();
        assert_eq!(parsed, IsolationLevel::ConfidentialVM);
    }

    #[test]
    fn agent_serde_json_roundtrip() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent_id = AgentId::generate();
        let now = chrono::Utc::now();

        let agent = Agent {
            agent_id,
            user_id,
            name: "my-agent".to_string(),
            status: AgentState::Running,
            spec: AgentSpec {
                cpu_millicores: 1000,
                memory_mb: 2048,
                runtime_version: "v2".to_string(),
                isolation: Some(IsolationLevel::ConfidentialVM),
                tier: "standard".to_string(),
                storage_encryption: StorageEncryption::sealed_for(&agent_id),
            },
            created_at: now,
            updated_at: now,
            last_heartbeat_at: Some(now),
            error_message: Some("test error".to_string()),
        };

        let json = serde_json::to_string(&agent).unwrap();
        let parsed: Agent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.agent_id, agent.agent_id);
        assert_eq!(parsed.user_id, agent.user_id);
        assert_eq!(parsed.name, "my-agent");
        assert_eq!(parsed.status, AgentState::Running);
        assert_eq!(parsed.spec.cpu_millicores, 1000);
        assert_eq!(parsed.spec.memory_mb, 2048);
        assert_eq!(parsed.spec.runtime_version, "v2");
        assert_eq!(parsed.error_message, Some("test error".to_string()));
    }

    #[test]
    fn box_tier_lineup() {
        assert_eq!(BoxTier::ALL, [BoxTier::Small, BoxTier::Standard, BoxTier::Pro]);
        assert_eq!(BoxTier::default(), BoxTier::Standard);

        assert_eq!(BoxTier::Small.cpu_millis(), 500);
        assert_eq!(BoxTier::Small.memory_mb(), 1024);
        assert_eq!(BoxTier::Small.hourly_price_cents(), 4);
        assert_eq!(BoxTier::Small.sku(), "swarm.small");

        assert_eq!(BoxTier::Standard.cpu_millis(), 1000);
        assert_eq!(BoxTier::Standard.memory_mb(), 2048);
        assert_eq!(BoxTier::Standard.hourly_price_cents(), 8);
        assert_eq!(BoxTier::Standard.sku(), "swarm.standard");

        assert_eq!(BoxTier::Pro.cpu_millis(), 2000);
        assert_eq!(BoxTier::Pro.memory_mb(), 4096);
        assert_eq!(BoxTier::Pro.hourly_price_cents(), 15);
        assert_eq!(BoxTier::Pro.sku(), "swarm.pro");

        // Every tier is confidential.
        for tier in BoxTier::ALL {
            assert_eq!(tier.isolation(), IsolationLevel::ConfidentialVM);
        }
    }

    #[test]
    fn box_tier_parse_display_serde() {
        for tier in BoxTier::ALL {
            // Display + FromStr roundtrip
            let parsed: BoxTier = tier.to_string().parse().unwrap();
            assert_eq!(parsed, tier);
            // Case-insensitive parse
            assert_eq!(BoxTier::from_name(&tier.as_str().to_uppercase()), Some(tier));
            // Lowercase serde
            let json = serde_json::to_string(&tier).unwrap();
            assert_eq!(json, format!("\"{tier}\""));
            let roundtripped: BoxTier = serde_json::from_str(&json).unwrap();
            assert_eq!(roundtripped, tier);
        }
        assert!("mega".parse::<BoxTier>().is_err());
        assert_eq!(BoxTier::from_name("mega"), None);
    }

    #[test]
    fn box_tier_nearest_for_cpu() {
        assert_eq!(BoxTier::nearest_for_cpu(100), BoxTier::Small);
        assert_eq!(BoxTier::nearest_for_cpu(500), BoxTier::Small);
        assert_eq!(BoxTier::nearest_for_cpu(501), BoxTier::Standard);
        assert_eq!(BoxTier::nearest_for_cpu(1000), BoxTier::Standard);
        assert_eq!(BoxTier::nearest_for_cpu(1001), BoxTier::Pro);
        assert_eq!(BoxTier::nearest_for_cpu(8000), BoxTier::Pro);
    }

    #[test]
    fn box_tier_to_spec_is_confidential_and_sealed() {
        let agent_id = AgentId::generate();
        let spec = BoxTier::Standard.to_spec(&agent_id, "latest");

        assert_eq!(spec.cpu_millicores, 1000);
        assert_eq!(spec.memory_mb, 2048);
        assert_eq!(spec.isolation, Some(IsolationLevel::ConfidentialVM));
        assert_eq!(spec.tier, "standard");

        let enc = spec.storage_encryption;
        assert_eq!(enc.key_id(), format!("swarm/agents/{agent_id}/state-key"));
        assert_eq!(enc, StorageEncryption::sealed_for(&agent_id));
    }

    #[test]
    fn storage_encryption_key_id_is_deterministic() {
        let agent_id = AgentId::generate();
        assert_eq!(
            StorageEncryption::state_key_id(&agent_id),
            format!("swarm/agents/{agent_id}/state-key")
        );
        assert_eq!(
            StorageEncryption::sealed_for(&agent_id),
            StorageEncryption::sealed_for(&agent_id)
        );
    }

    #[test]
    fn usage_event_cbor_roundtrip() {
        let event = UsageEvent::new(
            AgentId::generate(),
            UsageEventKind::TierChanged {
                from: Some("standard".to_string()),
                to: "pro".to_string(),
                from_hourly_price_cents: Some(8),
                to_hourly_price_cents: 15,
            },
        );

        let mut buf = Vec::new();
        ciborium::into_writer(&event, &mut buf).unwrap();
        let parsed: UsageEvent = ciborium::from_reader(buf.as_slice()).unwrap();
        assert_eq!(parsed, event);
    }

    /// Every phase-11 event kind survives a CBOR roundtrip.
    #[test]
    fn usage_event_kind_phase11_cbor_roundtrips() {
        let kinds = [
            UsageEventKind::PodScheduled {
                tier: Some("standard".to_string()),
                hourly_price_cents: Some(8),
            },
            UsageEventKind::PodScheduled {
                tier: None,
                hourly_price_cents: None,
            },
            UsageEventKind::PodTerminated {
                tier: Some("pro".to_string()),
                hourly_price_cents: Some(15),
                reason: "hibernate".to_string(),
            },
            UsageEventKind::Hibernated,
            UsageEventKind::Woke,
            UsageEventKind::TriggerFired {
                process_id: "proc-1".to_string(),
            },
        ];
        for kind in kinds {
            let event = UsageEvent::new(AgentId::generate(), kind);
            let mut buf = Vec::new();
            ciborium::into_writer(&event, &mut buf).unwrap();
            let parsed: UsageEvent = ciborium::from_reader(buf.as_slice()).unwrap();
            assert_eq!(parsed, event);
        }
    }

    /// Forward-compatibility: phase 11 extends `UsageEventKind` with more
    /// variants. Decoding must dispatch on the serde `kind` tag, so an
    /// extended enum still reads today's persisted `TierChanged` records.
    #[test]
    fn usage_event_kind_decodes_under_extended_enum() {
        #[derive(Debug, PartialEq, Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum FutureUsageEventKind {
            PodScheduled {
                tier: String,
            },
            TierChanged {
                from: Option<String>,
                to: String,
                from_hourly_price_cents: Option<u32>,
                to_hourly_price_cents: u32,
            },
            Hibernated,
        }

        let current = UsageEventKind::TierChanged {
            from: None,
            to: "standard".to_string(),
            from_hourly_price_cents: None,
            to_hourly_price_cents: 8,
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&current, &mut buf).unwrap();

        let parsed: FutureUsageEventKind = ciborium::from_reader(buf.as_slice()).unwrap();
        assert_eq!(
            parsed,
            FutureUsageEventKind::TierChanged {
                from: None,
                to: "standard".to_string(),
                from_hourly_price_cents: None,
                to_hourly_price_cents: 8,
            }
        );
    }

    // The legacy-CBOR deserialization regression tests live in
    // `migrations.rs` since R3: the live structs require `tier` /
    // `storage_encryption`, and the migration module owns the only
    // frozen legacy decode path.
}
