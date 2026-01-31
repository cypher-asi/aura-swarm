//! Endpoint cache for fast agent routing.
//!
//! This module provides a simple in-memory cache for pod endpoints,
//! avoiding repeated Kubernetes API calls for frequently accessed agents.

use aura_swarm_core::AgentId;
use parking_lot::RwLock;
use std::collections::HashMap;

/// A cache for agent pod endpoints.
///
/// The cache stores IP:port strings for agents, enabling fast routing
/// without hitting the Kubernetes API on every request.
#[derive(Debug, Default)]
pub struct EndpointCache {
    cache: RwLock<HashMap<AgentId, String>>,
}

impl EndpointCache {
    /// Create a new empty endpoint cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the endpoint for an agent, if cached.
    #[must_use]
    pub fn get(&self, agent_id: &AgentId) -> Option<String> {
        self.cache.read().get(agent_id).cloned()
    }

    /// Insert or update an endpoint for an agent.
    pub fn insert(&self, agent_id: AgentId, endpoint: String) {
        self.cache.write().insert(agent_id, endpoint);
    }

    /// Remove an endpoint from the cache.
    pub fn remove(&self, agent_id: &AgentId) -> Option<String> {
        self.cache.write().remove(agent_id)
    }

    /// Check if an agent has a cached endpoint.
    #[must_use]
    pub fn contains(&self, agent_id: &AgentId) -> bool {
        self.cache.read().contains_key(agent_id)
    }

    /// Get the number of cached endpoints.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cache.read().len()
    }

    /// Check if the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cache.read().is_empty()
    }

    /// Clear all cached endpoints.
    pub fn clear(&self) {
        self.cache.write().clear();
    }

    /// Get all cached agent IDs.
    #[must_use]
    pub fn agent_ids(&self) -> Vec<AgentId> {
        self.cache.read().keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent_id() -> AgentId {
        AgentId::from_bytes([1u8; 32])
    }

    #[test]
    fn cache_insert_and_get() {
        let cache = EndpointCache::new();
        let agent_id = test_agent_id();

        assert!(cache.get(&agent_id).is_none());
        assert!(!cache.contains(&agent_id));

        cache.insert(agent_id.clone(), "10.0.0.1:8080".to_string());

        assert_eq!(cache.get(&agent_id), Some("10.0.0.1:8080".to_string()));
        assert!(cache.contains(&agent_id));
    }

    #[test]
    fn cache_update() {
        let cache = EndpointCache::new();
        let agent_id = test_agent_id();

        cache.insert(agent_id.clone(), "10.0.0.1:8080".to_string());
        cache.insert(agent_id.clone(), "10.0.0.2:8080".to_string());

        assert_eq!(cache.get(&agent_id), Some("10.0.0.2:8080".to_string()));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_remove() {
        let cache = EndpointCache::new();
        let agent_id = test_agent_id();

        cache.insert(agent_id.clone(), "10.0.0.1:8080".to_string());
        let removed = cache.remove(&agent_id);

        assert_eq!(removed, Some("10.0.0.1:8080".to_string()));
        assert!(cache.get(&agent_id).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_clear() {
        let cache = EndpointCache::new();

        cache.insert(AgentId::from_bytes([1u8; 32]), "10.0.0.1:8080".to_string());
        cache.insert(AgentId::from_bytes([2u8; 32]), "10.0.0.2:8080".to_string());

        assert_eq!(cache.len(), 2);

        cache.clear();

        assert!(cache.is_empty());
    }
}
