//! Usage/cost aggregation over an agent's usage-event log
//! (Swarm TEE upgrade phase 11).
//!
//! Billable intervals are reconstructed by pairing `PodScheduled` /
//! `PodTerminated` events and priced with the hourly rate recorded **on
//! the events** (pricing at event time), so a later re-pricing of a tier
//! never rewrites usage history. zbilling remains the billing source of
//! truth — this is the user-facing stats layer.
//!
//! # Edge-case decisions
//!
//! - **Clipping:** the full per-agent event log drives interval
//!   reconstruction, but every interval is clipped to `[from, to)`. An
//!   interval that started before `from` contributes only its in-range
//!   part; intervals entirely outside the range are dropped.
//! - **Open interval (still running):** a `PodScheduled` with no later
//!   `PodTerminated` is closed at the range end `to` (callers clamp `to`
//!   to "now" so time that has not elapsed is never billed).
//! - **Unpaired `PodScheduled` (crash / lost terminate):** when a second
//!   `PodScheduled` arrives while an interval is open, the open interval
//!   is closed at the second event's timestamp — the old pod cannot have
//!   outlived its replacement, so this may overcount a crashed pod's tail
//!   but never double-counts concurrent pods.
//! - **Unpaired `PodTerminated` (history starts mid-run):** a terminate
//!   with no open interval is ignored — the matching schedule happened
//!   before lifecycle events were emitted, and we undercount
//!   conservatively rather than guess a start time.
//! - **Legacy agents:** intervals without a recorded price count toward
//!   awake time but contribute 0 cents (legacy agents are billed by
//!   cpu/mem-hours via zbilling, not a tier rate).
//! - **Counters** (`wakes`, `triggers_fired`, `tier_changes`) count only
//!   events with `from <= timestamp < to`.

use aura_swarm_store::{UsageEvent, UsageEventKind};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Maximum number of raw events returned alongside an aggregation
/// (`GET /v1/agents/:id/usage` returns at most this many, newest-biased).
pub const RECENT_EVENTS_CAP: usize = 100;

/// One billable interval: a span during which the agent had a pod,
/// priced at the hourly rate recorded when the pod was scheduled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillableInterval {
    /// Interval start (clipped to the query range).
    pub start: DateTime<Utc>,
    /// Interval end (clipped to the query range; equals the range end for
    /// a pod that is still running).
    pub end: DateTime<Utc>,
    /// Tier the pod ran as; `None` for legacy agents.
    pub tier: Option<String>,
    /// Hourly price in cents recorded at schedule time; `None` for legacy.
    pub hourly_price_cents: Option<u32>,
    /// Cost of this interval in cents (0 when no price is recorded),
    /// rounded to the nearest cent.
    pub cost_cents: u64,
}

/// Aggregated usage over a time range for one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageAggregation {
    /// Range start (inclusive).
    pub from: DateTime<Utc>,
    /// Range end (exclusive).
    pub to: DateTime<Utc>,
    /// Total seconds the agent had a pod within the range.
    pub awake_seconds: u64,
    /// Total estimated cost in cents (sum of interval costs).
    pub cost_cents: u64,
    /// Billable intervals, oldest first, clipped to the range.
    pub intervals: Vec<BillableInterval>,
    /// Number of `Woke` events within the range.
    pub wakes: u32,
    /// Number of `TriggerFired` events within the range.
    pub triggers_fired: u32,
    /// Number of `TierChanged` events within the range.
    pub tier_changes: u32,
    /// Unclipped start of the currently open interval (the pod is still
    /// running), derived from the full event history. `None` when the
    /// last pod event was a termination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_interval_started_at: Option<DateTime<Utc>>,
    /// Timestamp of the most recent `Woke` event in the full history
    /// (not restricted to the range).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_wake_at: Option<DateTime<Utc>>,
}

/// Cost in cents of `ms` milliseconds at `price` cents/hour, rounded to
/// the nearest cent. `None` price (legacy agent) costs 0.
fn interval_cost_cents(ms: i64, price: Option<u32>) -> u64 {
    let Some(price) = price else { return 0 };
    let ms = u64::try_from(ms).unwrap_or(0);
    (ms * u64::from(price) + 1_800_000) / 3_600_000
}

/// Aggregate an agent's usage events (time-ordered, full history) into
/// billable intervals and counters over `[from, to)`.
///
/// See the module docs for the edge-case decisions.
#[must_use]
#[allow(clippy::cast_sign_loss)]
pub fn aggregate(
    events: &[UsageEvent],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> UsageAggregation {
    let mut intervals = Vec::new();
    let mut awake_ms: u64 = 0;
    let mut cost_cents: u64 = 0;
    let mut wakes = 0u32;
    let mut triggers_fired = 0u32;
    let mut tier_changes = 0u32;
    let mut last_wake_at = None;

    // Close an interval, clip it to the range, and account for it.
    let mut close = |start: DateTime<Utc>,
                     end: DateTime<Utc>,
                     tier: Option<String>,
                     price: Option<u32>| {
        let clipped_start = start.max(from);
        let clipped_end = end.min(to);
        if clipped_start >= clipped_end {
            return;
        }
        let ms = (clipped_end - clipped_start).num_milliseconds();
        let cost = interval_cost_cents(ms, price);
        awake_ms += ms as u64;
        cost_cents += cost;
        intervals.push(BillableInterval {
            start: clipped_start,
            end: clipped_end,
            tier,
            hourly_price_cents: price,
            cost_cents: cost,
        });
    };

    // The currently open interval: (unclipped start, tier, price).
    let mut open: Option<(DateTime<Utc>, Option<String>, Option<u32>)> = None;
    let in_range = |ts: DateTime<Utc>| ts >= from && ts < to;

    for event in events {
        let ts = event.timestamp;
        match &event.kind {
            UsageEventKind::PodScheduled {
                tier,
                hourly_price_cents,
            } => {
                // An already-open interval means the previous pod's
                // terminate event was lost (crash): close it here.
                if let Some((start, t, p)) = open.take() {
                    close(start, ts, t, p);
                }
                open = Some((ts, tier.clone(), *hourly_price_cents));
            }
            UsageEventKind::PodTerminated { .. } => {
                // Pricing comes from the interval's opening event; a
                // leading unpaired terminate is ignored (see module docs).
                if let Some((start, t, p)) = open.take() {
                    close(start, ts, t, p);
                }
            }
            UsageEventKind::Woke => {
                last_wake_at = Some(ts);
                if in_range(ts) {
                    wakes += 1;
                }
            }
            UsageEventKind::TriggerFired { .. } => {
                if in_range(ts) {
                    triggers_fired += 1;
                }
            }
            UsageEventKind::TierChanged { .. } => {
                if in_range(ts) {
                    tier_changes += 1;
                }
            }
            UsageEventKind::Hibernated => {}
        }
    }

    // Pod still running: close the open interval at the range end.
    let open_interval_started_at = open.as_ref().map(|(start, _, _)| *start);
    if let Some((start, t, p)) = open {
        close(start, to, t, p);
    }

    UsageAggregation {
        from,
        to,
        awake_seconds: awake_ms / 1000,
        cost_cents,
        intervals,
        wakes,
        triggers_fired,
        tier_changes,
        open_interval_started_at,
        last_wake_at,
    }
}

/// Usage report for one agent: the aggregation plus the most recent raw
/// events within the range (capped at [`RECENT_EVENTS_CAP`]).
#[derive(Debug, Clone, Serialize)]
pub struct AgentUsage {
    /// Aggregated intervals, totals, and counters.
    pub aggregation: UsageAggregation,
    /// Raw events within the range, oldest first, capped to the most
    /// recent [`RECENT_EVENTS_CAP`].
    pub recent_events: Vec<UsageEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_swarm_core::AgentId;
    use chrono::TimeZone;

    fn t(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap() + chrono::Duration::minutes(minutes)
    }

    fn event(agent_id: &AgentId, at: DateTime<Utc>, kind: UsageEventKind) -> UsageEvent {
        UsageEvent {
            event_id: uuid::Uuid::new_v4(),
            agent_id: *agent_id,
            timestamp: at,
            kind,
        }
    }

    fn scheduled(agent_id: &AgentId, at: DateTime<Utc>, tier: &str, price: u32) -> UsageEvent {
        event(
            agent_id,
            at,
            UsageEventKind::PodScheduled {
                tier: Some(tier.to_string()),
                hourly_price_cents: Some(price),
            },
        )
    }

    fn terminated(agent_id: &AgentId, at: DateTime<Utc>, reason: &str) -> UsageEvent {
        event(
            agent_id,
            at,
            UsageEventKind::PodTerminated {
                tier: Some("standard".to_string()),
                hourly_price_cents: Some(8),
                reason: reason.to_string(),
            },
        )
    }

    #[test]
    fn empty_history_is_all_zero() {
        let agg = aggregate(&[], t(0), t(60));
        assert_eq!(agg.awake_seconds, 0);
        assert_eq!(agg.cost_cents, 0);
        assert!(agg.intervals.is_empty());
        assert_eq!((agg.wakes, agg.triggers_fired, agg.tier_changes), (0, 0, 0));
        assert!(agg.open_interval_started_at.is_none());
    }

    #[test]
    fn paired_interval_is_priced_from_the_schedule_event() {
        let aid = AgentId::generate();
        // 30 minutes at 8 c/h = 4 cents.
        let events = vec![
            scheduled(&aid, t(10), "standard", 8),
            terminated(&aid, t(40), "stop"),
        ];
        let agg = aggregate(&events, t(0), t(60));
        assert_eq!(agg.awake_seconds, 30 * 60);
        assert_eq!(agg.cost_cents, 4);
        assert_eq!(agg.intervals.len(), 1);
        let iv = &agg.intervals[0];
        assert_eq!((iv.start, iv.end), (t(10), t(40)));
        assert_eq!(iv.tier.as_deref(), Some("standard"));
        assert_eq!(iv.hourly_price_cents, Some(8));
        assert_eq!(iv.cost_cents, 4);
        assert!(agg.open_interval_started_at.is_none());
    }

    #[test]
    fn open_interval_closes_at_range_end() {
        let aid = AgentId::generate();
        let events = vec![scheduled(&aid, t(30), "pro", 15)];
        let agg = aggregate(&events, t(0), t(60));
        // 30 minutes at 15 c/h = 7.5 → rounds to 8.
        assert_eq!(agg.awake_seconds, 30 * 60);
        assert_eq!(agg.cost_cents, 8);
        assert_eq!(agg.intervals[0].end, t(60));
        assert_eq!(agg.open_interval_started_at, Some(t(30)));
    }

    #[test]
    fn intervals_are_clipped_to_the_range() {
        let aid = AgentId::generate();
        // Runs from t(-60) to t(30): only [t(0), t(30)) is in range.
        let events = vec![
            scheduled(&aid, t(-60), "standard", 8),
            terminated(&aid, t(30), "stop"),
        ];
        let agg = aggregate(&events, t(0), t(60));
        assert_eq!(agg.awake_seconds, 30 * 60);
        assert_eq!(agg.intervals[0].start, t(0));
        assert_eq!(agg.intervals[0].end, t(30));

        // Entirely before the range: dropped.
        let agg = aggregate(&events, t(40), t(60));
        assert!(agg.intervals.is_empty());
        assert_eq!(agg.awake_seconds, 0);
        assert_eq!(agg.cost_cents, 0);
    }

    #[test]
    fn unpaired_schedule_closes_at_next_schedule() {
        let aid = AgentId::generate();
        // First pod crashed without a terminate event; the second
        // schedule closes the first interval.
        let events = vec![
            scheduled(&aid, t(0), "standard", 8),
            scheduled(&aid, t(30), "standard", 8),
            terminated(&aid, t(45), "stop"),
        ];
        let agg = aggregate(&events, t(0), t(60));
        assert_eq!(agg.intervals.len(), 2);
        assert_eq!((agg.intervals[0].start, agg.intervals[0].end), (t(0), t(30)));
        assert_eq!((agg.intervals[1].start, agg.intervals[1].end), (t(30), t(45)));
        assert_eq!(agg.awake_seconds, 45 * 60);
    }

    #[test]
    fn leading_unpaired_terminate_is_ignored() {
        let aid = AgentId::generate();
        let events = vec![
            terminated(&aid, t(10), "stop"),
            scheduled(&aid, t(20), "standard", 8),
            terminated(&aid, t(50), "stop"),
        ];
        let agg = aggregate(&events, t(0), t(60));
        assert_eq!(agg.intervals.len(), 1);
        assert_eq!(agg.intervals[0].start, t(20));
        assert_eq!(agg.awake_seconds, 30 * 60);
    }

    #[test]
    fn tier_change_mid_range_prices_each_interval_at_its_own_rate() {
        let aid = AgentId::generate();
        // 1h standard (8c) then 1h pro (15c): the recreate emits a
        // terminate/schedule pair, splitting the cost exactly.
        let events = vec![
            scheduled(&aid, t(0), "standard", 8),
            terminated(&aid, t(60), "tier_change"),
            event(
                &aid,
                t(60),
                UsageEventKind::TierChanged {
                    from: Some("standard".to_string()),
                    to: "pro".to_string(),
                    from_hourly_price_cents: Some(8),
                    to_hourly_price_cents: 15,
                },
            ),
            scheduled(&aid, t(60), "pro", 15),
            terminated(&aid, t(120), "stop"),
        ];
        let agg = aggregate(&events, t(0), t(120));
        assert_eq!(agg.intervals.len(), 2);
        assert_eq!(agg.intervals[0].cost_cents, 8);
        assert_eq!(agg.intervals[1].cost_cents, 15);
        assert_eq!(agg.cost_cents, 23);
        assert_eq!(agg.awake_seconds, 120 * 60);
        assert_eq!(agg.tier_changes, 1);
    }

    #[test]
    fn re_pricing_does_not_rewrite_history() {
        let aid = AgentId::generate();
        // The event recorded 8 c/h even if the tier's price later changes:
        // aggregation must use the recorded 8, not any current price.
        let events = vec![
            scheduled(&aid, t(0), "standard", 8),
            terminated(&aid, t(60), "stop"),
        ];
        let agg = aggregate(&events, t(0), t(120));
        assert_eq!(agg.cost_cents, 8);
    }

    #[test]
    fn legacy_intervals_count_time_but_cost_zero() {
        let aid = AgentId::generate();
        let events = vec![
            event(
                &aid,
                t(0),
                UsageEventKind::PodScheduled {
                    tier: None,
                    hourly_price_cents: None,
                },
            ),
            event(
                &aid,
                t(60),
                UsageEventKind::PodTerminated {
                    tier: None,
                    hourly_price_cents: None,
                    reason: "stop".to_string(),
                },
            ),
        ];
        let agg = aggregate(&events, t(0), t(120));
        assert_eq!(agg.awake_seconds, 3600);
        assert_eq!(agg.cost_cents, 0);
        assert_eq!(agg.intervals[0].hourly_price_cents, None);
    }

    #[test]
    fn counters_only_count_in_range_events() {
        let aid = AgentId::generate();
        let events = vec![
            event(&aid, t(-10), UsageEventKind::Woke),
            event(&aid, t(5), UsageEventKind::Woke),
            event(&aid, t(10), UsageEventKind::Hibernated),
            event(
                &aid,
                t(15),
                UsageEventKind::TriggerFired {
                    process_id: "p1".to_string(),
                },
            ),
            event(
                &aid,
                t(70),
                UsageEventKind::TriggerFired {
                    process_id: "p2".to_string(),
                },
            ),
        ];
        let agg = aggregate(&events, t(0), t(60));
        assert_eq!(agg.wakes, 1);
        assert_eq!(agg.triggers_fired, 1);
        // last_wake_at tracks the full history, not just the range.
        assert_eq!(agg.last_wake_at, Some(t(5)));
    }

    #[test]
    fn sub_cent_interval_rounds_to_zero() {
        let aid = AgentId::generate();
        // 1 minute at 8 c/h ≈ 0.13 cents → 0.
        let events = vec![
            scheduled(&aid, t(0), "standard", 8),
            terminated(&aid, t(1), "stop"),
        ];
        let agg = aggregate(&events, t(0), t(60));
        assert_eq!(agg.cost_cents, 0);
        assert_eq!(agg.awake_seconds, 60);
    }
}
