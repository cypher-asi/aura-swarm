//! Caches for fast agent routing and state-change deduplication.
//!
//! This module provides simple in-memory caches:
//! - `EndpointCache` — pod endpoints, avoiding repeated K8s API calls.
//! - `StateCache` — last-pushed `AgentState` per agent, so the scheduler only
//!   notifies the gateway when the mapped state actually changes.

use aura_swarm_core::AgentId;
use aura_swarm_store::AgentState;
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

/// Tracks the last `AgentState` pushed to the gateway for each agent.
///
/// The scheduler uses this to avoid redundant HTTP notifications when the
/// K8s watcher fires repeated events that map to the same logical state.
#[derive(Debug, Default)]
pub struct StateCache {
    cache: RwLock<HashMap<AgentId, AgentState>>,
}

impl StateCache {
    /// Create a new empty state cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `state` and return `true` if it differs from the previously
    /// recorded state (i.e. this is a genuine change that should be pushed).
    pub fn update_if_changed(&self, agent_id: AgentId, state: AgentState) -> bool {
        let mut map = self.cache.write();
        let prev = map.insert(agent_id, state);
        prev.map_or(true, |p| p != state)
    }

    /// Remove the cached state for an agent (e.g. on pod deletion).
    pub fn remove(&self, agent_id: &AgentId) {
        self.cache.write().remove(agent_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_swarm_core::UserId;

    fn test_agent_id() -> AgentId {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        AgentId::generate_deterministic(&user_id, "test", 42)
    }

    #[test]
    fn cache_insert_and_get() {
        let cache = EndpointCache::new();
        let agent_id = test_agent_id();

        assert!(cache.get(&agent_id).is_none());
        assert!(!cache.contains(&agent_id));

        cache.insert(agent_id, "10.0.0.1:8080".to_string());

        assert_eq!(cache.get(&agent_id), Some("10.0.0.1:8080".to_string()));
        assert!(cache.contains(&agent_id));
    }

    #[test]
    fn cache_update() {
        let cache = EndpointCache::new();
        let agent_id = test_agent_id();

        cache.insert(agent_id, "10.0.0.1:8080".to_string());
        cache.insert(agent_id, "10.0.0.2:8080".to_string());

        assert_eq!(cache.get(&agent_id), Some("10.0.0.2:8080".to_string()));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_remove() {
        let cache = EndpointCache::new();
        let agent_id = test_agent_id();

        cache.insert(agent_id, "10.0.0.1:8080".to_string());
        let removed = cache.remove(&agent_id);

        assert_eq!(removed, Some("10.0.0.1:8080".to_string()));
        assert!(cache.get(&agent_id).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_clear() {
        let cache = EndpointCache::new();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());

        cache.insert(
            AgentId::generate_deterministic(&user_id, "a1", 1),
            "10.0.0.1:8080".to_string(),
        );
        cache.insert(
            AgentId::generate_deterministic(&user_id, "a2", 2),
            "10.0.0.2:8080".to_string(),
        );

        assert_eq!(cache.len(), 2);

        cache.clear();

        assert!(cache.is_empty());
    }

    #[test]
    fn agent_ids_returns_all_keys() {
        let cache = EndpointCache::new();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());

        let a1 = AgentId::generate_deterministic(&user_id, "a1", 1);
        let a2 = AgentId::generate_deterministic(&user_id, "a2", 2);
        let a3 = AgentId::generate_deterministic(&user_id, "a3", 3);

        cache.insert(a1, "10.0.0.1:8080".to_string());
        cache.insert(a2, "10.0.0.2:8080".to_string());
        cache.insert(a3, "10.0.0.3:8080".to_string());

        let ids = cache.agent_ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&a1));
        assert!(ids.contains(&a2));
        assert!(ids.contains(&a3));
    }

    #[test]
    fn len_and_is_empty() {
        let cache = EndpointCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);

        let agent_id = test_agent_id();
        cache.insert(agent_id, "10.0.0.1:8080".to_string());

        assert!(!cache.is_empty());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let cache = EndpointCache::new();
        let agent_id = test_agent_id();
        assert!(cache.get(&agent_id).is_none());
    }

    // =========================================================================
    // StateCache tests
    // =========================================================================

    #[test]
    fn state_cache_first_insert_is_change() {
        let cache = StateCache::new();
        let agent_id = test_agent_id();
        assert!(cache.update_if_changed(agent_id, AgentState::Running));
    }

    #[test]
    fn state_cache_same_state_is_not_change() {
        let cache = StateCache::new();
        let agent_id = test_agent_id();
        assert!(cache.update_if_changed(agent_id, AgentState::Running));
        assert!(!cache.update_if_changed(agent_id, AgentState::Running));
    }

    #[test]
    fn state_cache_different_state_is_change() {
        let cache = StateCache::new();
        let agent_id = test_agent_id();
        assert!(cache.update_if_changed(agent_id, AgentState::Provisioning));
        assert!(cache.update_if_changed(agent_id, AgentState::Running));
        assert!(cache.update_if_changed(agent_id, AgentState::Idle));
    }

    #[test]
    fn state_cache_remove_resets() {
        let cache = StateCache::new();
        let agent_id = test_agent_id();
        assert!(cache.update_if_changed(agent_id, AgentState::Running));
        cache.remove(&agent_id);
        assert!(cache.update_if_changed(agent_id, AgentState::Running));
    }
}
