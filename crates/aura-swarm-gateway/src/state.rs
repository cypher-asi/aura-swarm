//! Gateway application state.
//!
//! This module defines the shared state that is available to all request handlers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;

use crate::config::GatewayConfig;

/// Cache of harness build info (`git_sha` from the pod `/health` endpoint),
/// keyed by agent endpoint (`ip:port`). A pod restart yields a new endpoint,
/// which naturally invalidates stale entries. `None` means the pod was
/// reachable but reported no `git_sha` (older harness image).
pub type HarnessInfoCache = Arc<Mutex<HashMap<String, Option<String>>>>;

/// Shared application state for the gateway.
///
/// This struct holds references to all services needed by the HTTP handlers.
pub struct GatewayState<C, V>
where
    C: ControlPlane,
    V: JwtValidator,
{
    /// The control plane for agent lifecycle operations.
    pub control: Arc<C>,
    /// The JWT validator for authentication.
    pub jwt_validator: Arc<V>,
    /// Gateway configuration.
    pub config: GatewayConfig,
    /// Per-endpoint cache of harness git SHAs reported by pod `/health`.
    pub harness_info_cache: HarnessInfoCache,
}

impl<C, V> GatewayState<C, V>
where
    C: ControlPlane,
    V: JwtValidator,
{
    /// Create a new gateway state.
    #[must_use]
    pub fn new(control: Arc<C>, jwt_validator: Arc<V>, config: GatewayConfig) -> Self {
        Self {
            control,
            jwt_validator,
            config,
            harness_info_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<C, V> Clone for GatewayState<C, V>
where
    C: ControlPlane,
    V: JwtValidator,
{
    fn clone(&self) -> Self {
        Self {
            control: Arc::clone(&self.control),
            jwt_validator: Arc::clone(&self.jwt_validator),
            config: self.config.clone(),
            harness_info_cache: Arc::clone(&self.harness_info_cache),
        }
    }
}
