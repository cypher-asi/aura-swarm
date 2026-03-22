//! Billing integration for the gateway.
//!
//! This module provides integration with the z-billing service for:
//! - Account auto-creation on first authenticated request
//! - LLM usage tracking from WebSocket streams
//! - Account existence caching to reduce API calls

mod account_cache;
mod config;
mod middleware;
mod service;
mod usage;

pub use account_cache::AccountCache;
pub use config::BillingConfig;
pub use middleware::{BillingAccountLayer, BillingAccountMiddleware, UserId};
pub use service::{BillingService, BillingServiceError};
pub use usage::{make_event_id, try_extract_usage, ExtractedUsage};
