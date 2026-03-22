//! Compute usage reporter for billing.
//!
//! Periodically collects pod resource metrics and reports to z-billing.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use z_billing_client::{ClientOptions, ComputeUsageEvent, ZBillingClient};

use super::config::SchedulerBillingConfig;

/// Information about a pod's usage since last report.
#[derive(Debug, Clone)]
pub struct PodUsageInfo {
    /// User ID who owns the agent.
    pub user_id: String,
    /// Agent ID.
    pub agent_id: String,
    /// Last report time.
    pub last_report: DateTime<Utc>,
    /// CPU millicores allocated.
    pub cpu_millicores: u32,
    /// Memory MB allocated.
    pub memory_mb: u32,
}

/// Compute usage reporter that tracks pod metrics and reports to billing.
pub struct ComputeUsageReporter {
    client: ZBillingClient,
    config: SchedulerBillingConfig,
    /// Tracks last report time per agent.
    pod_tracking: RwLock<HashMap<String, PodTrackingInfo>>,
}

struct PodTrackingInfo {
    user_id: String,
    last_report_at: Instant,
    cpu_millicores: u32,
    memory_mb: u32,
}

impl ComputeUsageReporter {
    /// Create a new compute usage reporter.
    #[must_use]
    pub fn new(config: SchedulerBillingConfig) -> Self {
        let options = ClientOptions::with_service_name("aura-scheduler");
        let client = ZBillingClient::with_options(&config.url, &config.api_key, options)
            .expect("failed to create billing client");

        Self {
            client,
            config,
            pod_tracking: RwLock::new(HashMap::new()),
        }
    }

    /// Create a reporter wrapped in an Arc.
    #[must_use]
    pub fn new_shared(config: SchedulerBillingConfig) -> Arc<Self> {
        Arc::new(Self::new(config))
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
    pub fn register_pod(&self, agent_id: &str, user_id: &str, cpu_millicores: u32, memory_mb: u32) {
        let mut tracking = self.pod_tracking.write();
        tracking.insert(
            agent_id.to_string(),
            PodTrackingInfo {
                user_id: user_id.to_string(),
                last_report_at: Instant::now(),
                cpu_millicores,
                memory_mb,
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

        let event = ComputeUsageEvent {
            event_id: format!("compute:{}:{}", agent_id, Utc::now().timestamp_millis()),
            user_id: info.user_id.clone(),
            agent_id: Some(agent_id.to_string()),
            cpu_hours,
            memory_gb_hours,
            metadata: None,
        };

        match self.client.report_compute_usage(event).await {
            Ok(response) => {
                tracing::debug!(
                    agent_id = %agent_id,
                    cpu_hours = %cpu_hours,
                    memory_gb_hours = %memory_gb_hours,
                    cost_cents = response.cost_cents,
                    "Reported compute usage"
                );
                self.update_last_report(agent_id);
                Ok(())
            }
            Err(z_billing_client::ClientError::DuplicateEvent { .. }) => {
                self.update_last_report(agent_id);
                Ok(())
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
        let reporter = ComputeUsageReporter::new(config);

        reporter.register_pod("agent-1", "user-1", 500, 512);
        assert_eq!(reporter.tracked_pod_count(), 1);

        reporter.unregister_pod("agent-1");
        assert_eq!(reporter.tracked_pod_count(), 0);
    }

    #[test]
    fn report_interval() {
        let mut config = SchedulerBillingConfig::default();
        config.report_interval_seconds = 60;
        let reporter = ComputeUsageReporter::new(config);

        assert_eq!(reporter.report_interval(), Duration::from_secs(60));
    }
}
