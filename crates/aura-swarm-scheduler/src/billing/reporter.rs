//! Compute usage reporter for billing.
//!
//! Periodically collects pod resource metrics and reports to the zbilling service.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_swarm_store::BoxTier;
use chrono::Utc;
use parking_lot::RwLock;
use reqwest::Client;
use serde::Serialize;

use super::config::SchedulerBillingConfig;

/// Information about a pod's usage since last report.
#[derive(Debug, Clone)]
pub struct PodUsageInfo {
    /// User ID who owns the agent.
    pub user_id: String,
    /// Agent ID.
    pub agent_id: String,
    /// Last report time.
    pub last_report: chrono::DateTime<Utc>,
    /// CPU millicores allocated.
    pub cpu_millicores: u32,
    /// Memory MB allocated.
    pub memory_mb: u32,
}

struct PodTrackingInfo {
    user_id: String,
    last_report_at: Instant,
    cpu_millicores: u32,
    memory_mb: u32,
    /// Box tier for tiered (confidential) agents. `None` for legacy agents,
    /// which keep the plain cpu/mem-hour payload. Kept in the registration
    /// data so re-registration (e.g. pod recreate on tier change) reports
    /// under the right SKU.
    tier: Option<BoxTier>,
}

#[derive(Serialize)]
struct ComputeUsageRequest {
    event_id: String,
    user_id: String,
    agent_id: Option<String>,
    cpu_hours: f64,
    memory_gb_hours: f64,
    /// Billing SKU for tiered agents (e.g. "swarm.standard").
    /// Omitted entirely for legacy agents so their payload shape is
    /// exactly the pre-TEE-upgrade one.
    #[serde(skip_serializing_if = "Option::is_none")]
    sku: Option<String>,
    /// Platform price per hour of pod runtime, in cents. Omitted for
    /// legacy agents (see `sku`).
    #[serde(skip_serializing_if = "Option::is_none")]
    hourly_price_cents: Option<u32>,
}

/// Compute usage reporter that tracks pod metrics and reports to billing.
pub struct ComputeUsageReporter {
    http: Client,
    config: SchedulerBillingConfig,
    /// Tracks last report time per agent.
    pod_tracking: RwLock<HashMap<String, PodTrackingInfo>>,
}

impl ComputeUsageReporter {
    /// Create a new compute usage reporter.
    ///
    /// # Errors
    ///
    /// Returns `ReportError::Client` if the HTTP client cannot be created.
    pub fn new(config: SchedulerBillingConfig) -> Result<Self, ReportError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ReportError::Client(format!("failed to create HTTP client: {e}")))?;

        Ok(Self {
            http,
            config,
            pod_tracking: RwLock::new(HashMap::new()),
        })
    }

    /// Create a reporter wrapped in an Arc.
    ///
    /// # Errors
    ///
    /// Returns `ReportError::Client` if the billing client cannot be created.
    pub fn new_shared(config: SchedulerBillingConfig) -> Result<Arc<Self>, ReportError> {
        Ok(Arc::new(Self::new(config)?))
    }

    /// Check if billing is enabled and configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.is_configured()
    }

    /// Get the report interval as a Duration.
    #[must_use]
    pub fn report_interval(&self) -> Duration {
        Duration::from_secs(self.config.report_interval_seconds)
    }

    /// Register a pod for usage tracking.
    ///
    /// `tier` is the box tier for tiered (confidential) agents and `None`
    /// for legacy agents; it determines whether usage reports carry the
    /// `sku` / `hourly_price_cents` fields.
    pub fn register_pod(
        &self,
        agent_id: &str,
        user_id: &str,
        cpu_millicores: u32,
        memory_mb: u32,
        tier: Option<BoxTier>,
    ) {
        let mut tracking = self.pod_tracking.write();
        tracking.insert(
            agent_id.to_string(),
            PodTrackingInfo {
                user_id: user_id.to_string(),
                last_report_at: Instant::now(),
                cpu_millicores,
                memory_mb,
                tier,
            },
        );
    }

    /// Unregister a pod from usage tracking.
    pub fn unregister_pod(&self, agent_id: &str) {
        let mut tracking = self.pod_tracking.write();
        tracking.remove(agent_id);
    }

    /// Report compute usage for all tracked pods.
    ///
    /// Returns the number of successful reports.
    pub async fn report_all_usage(&self) -> usize {
        if !self.is_enabled() {
            return 0;
        }

        let pods_to_report = self.collect_pods_for_report();
        let mut success_count = 0;

        for (agent_id, info) in pods_to_report {
            if self.report_pod_usage(&agent_id, &info).await.is_ok() {
                success_count += 1;
            }
        }

        success_count
    }

    /// Collect pods that need usage reported.
    fn collect_pods_for_report(&self) -> Vec<(String, PodTrackingInfo)> {
        let tracking = self.pod_tracking.read();
        let interval = self.report_interval();

        tracking
            .iter()
            .filter(|(_, info)| info.last_report_at.elapsed() >= interval)
            .map(|(id, info)| {
                (
                    id.clone(),
                    PodTrackingInfo {
                        user_id: info.user_id.clone(),
                        last_report_at: info.last_report_at,
                        cpu_millicores: info.cpu_millicores,
                        memory_mb: info.memory_mb,
                        tier: info.tier,
                    },
                )
            })
            .collect()
    }

    /// Report usage for a single pod.
    async fn report_pod_usage(
        &self,
        agent_id: &str,
        info: &PodTrackingInfo,
    ) -> Result<(), ReportError> {
        let elapsed_hours = info.last_report_at.elapsed().as_secs_f64() / 3600.0;
        let cpu_hours = (f64::from(info.cpu_millicores) / 1000.0) * elapsed_hours;
        let memory_gb_hours = (f64::from(info.memory_mb) / 1024.0) * elapsed_hours;

        let url = format!("{}/v1/usage/compute", self.config.url);
        let body = ComputeUsageRequest {
            event_id: format!("compute:{}:{}", agent_id, Utc::now().timestamp_millis()),
            user_id: info.user_id.clone(),
            agent_id: Some(agent_id.to_string()),
            cpu_hours,
            memory_gb_hours,
            sku: info.tier.map(|t| t.sku().to_string()),
            hourly_price_cents: info.tier.map(|t| t.hourly_price_cents()),
        };

        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.config.api_key)
            .header("x-service-name", "aura-scheduler")
            .json(&body)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                tracing::debug!(
                    agent_id = %agent_id,
                    cpu_hours = %cpu_hours,
                    memory_gb_hours = %memory_gb_hours,
                    "Reported compute usage"
                );
                self.update_last_report(agent_id);
                Ok(())
            }
            Ok(r) if r.status() == reqwest::StatusCode::CONFLICT => {
                // Duplicate event — treat as success
                self.update_last_report(agent_id);
                Ok(())
            }
            Ok(r) if !self.config.fail_closed => {
                let status = r.status();
                tracing::warn!(
                    agent_id = %agent_id,
                    status = %status,
                    "Failed to report compute usage, continuing"
                );
                Ok(())
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                Err(ReportError::Client(format!(
                    "billing service returned {status}: {body}"
                )))
            }
            Err(e) if !self.config.fail_closed => {
                tracing::warn!(
                    agent_id = %agent_id,
                    error = %e,
                    "Failed to report compute usage, continuing"
                );
                Ok(())
            }
            Err(e) => Err(ReportError::Client(e.to_string())),
        }
    }

    /// Update the last report time for an agent.
    fn update_last_report(&self, agent_id: &str) {
        let mut tracking = self.pod_tracking.write();
        if let Some(info) = tracking.get_mut(agent_id) {
            info.last_report_at = Instant::now();
        }
    }

    /// Get the number of tracked pods.
    #[must_use]
    pub fn tracked_pod_count(&self) -> usize {
        self.pod_tracking.read().len()
    }
}

/// Errors from compute usage reporting.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// Billing client error.
    #[error("client error: {0}")]
    Client(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_unregister_pod() {
        let config = SchedulerBillingConfig::default();
        let reporter = ComputeUsageReporter::new(config).unwrap();

        reporter.register_pod("agent-1", "user-1", 500, 512, None);
        assert_eq!(reporter.tracked_pod_count(), 1);

        reporter.unregister_pod("agent-1");
        assert_eq!(reporter.tracked_pod_count(), 0);
    }

    /// Legacy agents (no tier) must report exactly the pre-TEE-upgrade
    /// payload shape: no `sku` / `hourly_price_cents` keys at all.
    #[test]
    fn legacy_usage_payload_has_no_tier_fields() {
        let body = ComputeUsageRequest {
            event_id: "compute:agent-1:1".to_string(),
            user_id: "user-1".to_string(),
            agent_id: Some("agent-1".to_string()),
            cpu_hours: 0.5,
            memory_gb_hours: 0.25,
            sku: None,
            hourly_price_cents: None,
        };

        let json = serde_json::to_value(&body).unwrap();
        // serde_json::Value keys are alphabetically ordered.
        let keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            vec![
                "agent_id",
                "cpu_hours",
                "event_id",
                "memory_gb_hours",
                "user_id"
            ],
            "legacy payload shape must be unchanged"
        );
    }

    /// Tiered agents report the legacy fields plus `sku` and
    /// `hourly_price_cents`.
    #[test]
    fn tiered_usage_payload_carries_sku_and_price() {
        let tier = aura_swarm_store::BoxTier::Standard;
        let body = ComputeUsageRequest {
            event_id: "compute:agent-1:1".to_string(),
            user_id: "user-1".to_string(),
            agent_id: Some("agent-1".to_string()),
            cpu_hours: 0.5,
            memory_gb_hours: 0.25,
            sku: Some(tier.sku().to_string()),
            hourly_price_cents: Some(tier.hourly_price_cents()),
        };

        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["sku"], "swarm.standard");
        assert_eq!(json["hourly_price_cents"], 8);
        // Legacy fields are still present alongside the tier fields.
        assert_eq!(json["cpu_hours"], 0.5);
        assert_eq!(json["memory_gb_hours"], 0.25);
    }

    /// The per-pod registration data must carry the tier so that the
    /// reported payload (and any re-registration) uses the right SKU.
    #[test]
    fn registration_keeps_tier_for_reregistration() {
        let config = SchedulerBillingConfig::default();
        let reporter = ComputeUsageReporter::new(config).unwrap();

        reporter.register_pod("agent-1", "user-1", 1000, 2048, Some(BoxTier::Standard));
        {
            let tracking = reporter.pod_tracking.read();
            assert_eq!(tracking["agent-1"].tier, Some(BoxTier::Standard));
        }

        // Re-registration under a new tier (e.g. tier change pod recreate).
        reporter.register_pod("agent-1", "user-1", 2000, 4096, Some(BoxTier::Pro));
        {
            let tracking = reporter.pod_tracking.read();
            assert_eq!(tracking["agent-1"].tier, Some(BoxTier::Pro));
        }
        assert_eq!(reporter.tracked_pod_count(), 1);
    }

    #[test]
    fn report_interval() {
        let mut config = SchedulerBillingConfig::default();
        config.report_interval_seconds = 60;
        let reporter = ComputeUsageReporter::new(config).unwrap();

        assert_eq!(reporter.report_interval(), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn disabled_reporter_report_returns_zero() {
        let config = SchedulerBillingConfig::default();
        let reporter = ComputeUsageReporter::new(config).unwrap();
        assert!(!reporter.is_enabled());

        let count = reporter.report_all_usage().await;
        assert_eq!(count, 0);
    }

    #[test]
    fn register_duplicate_pod() {
        let config = SchedulerBillingConfig::default();
        let reporter = ComputeUsageReporter::new(config).unwrap();

        reporter.register_pod("agent-1", "user-1", 500, 512, None);
        reporter.register_pod("agent-1", "user-1", 1000, 1024, None);
        assert_eq!(reporter.tracked_pod_count(), 1);
    }

    #[test]
    fn unregister_nonexistent() {
        let config = SchedulerBillingConfig::default();
        let reporter = ComputeUsageReporter::new(config).unwrap();

        reporter.unregister_pod("never-registered");
        assert_eq!(reporter.tracked_pod_count(), 0);
    }
}
