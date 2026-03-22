//! Gateway application state.
//!
//! This module defines the shared state that is available to all request handlers.

use std::sync::Arc;

use aura_swarm_auth::JwtValidator;
use aura_swarm_control::ControlPlane;

use crate::billing::BillingService;
use crate::config::GatewayConfig;

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
    /// Optional billing service for z-billing integration.
    pub billing: Option<Arc<BillingService>>,
}

impl<C, V> GatewayState<C, V>
where
    C: ControlPlane,
    V: JwtValidator,
{
    /// Create a new gateway state without billing.
    #[must_use]
    pub fn new(control: Arc<C>, jwt_validator: Arc<V>, config: GatewayConfig) -> Self {
        Self {
            control,
            jwt_validator,
            config,
            billing: None,
        }
    }

    /// Create a new gateway state with billing service.
    #[must_use]
    pub fn with_billing(
        control: Arc<C>,
        jwt_validator: Arc<V>,
        config: GatewayConfig,
        billing: Arc<BillingService>,
    ) -> Self {
        Self {
            control,
            jwt_validator,
            config,
            billing: Some(billing),
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
            billing: self.billing.clone(),
        }
    }
}
