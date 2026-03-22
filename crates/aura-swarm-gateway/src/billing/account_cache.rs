//! In-memory cache for account existence checks.
//!
//! Caches "account exists" state to avoid repeated z-billing API calls.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// Cache for tracking which user accounts exist in z-billing.
///
/// Uses a TTL to periodically re-verify accounts exist.
pub struct AccountCache {
    entries: RwLock<HashMap<String, CacheEntry>>,
    ttl: Duration,
}

struct CacheEntry {
    exists: bool,
    inserted_at: Instant,
}

impl AccountCache {
    /// Create a new account cache with the specified TTL.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Check if an account is cached as existing.
    ///
    /// Returns `Some(true)` if cached and exists, `Some(false)` if cached and
    /// doesn't exist, `None` if not cached or expired.
    #[must_use]
    pub fn get(&self, user_id: &str) -> Option<bool> {
        let entries = self.entries.read().ok()?;
        let entry = entries.get(user_id)?;

        if entry.inserted_at.elapsed() > self.ttl {
            return None;
        }

        Some(entry.exists)
    }

    /// Mark an account as existing in the cache.
    pub fn set_exists(&self, user_id: &str) {
        if let Ok(mut entries) = self.entries.write() {
            entries.insert(
                user_id.to_string(),
                CacheEntry {
                    exists: true,
                    inserted_at: Instant::now(),
                },
            );
        }
    }

    /// Mark an account as not existing (for negative caching).
    pub fn set_not_exists(&self, user_id: &str) {
        if let Ok(mut entries) = self.entries.write() {
            entries.insert(
                user_id.to_string(),
                CacheEntry {
                    exists: false,
                    inserted_at: Instant::now(),
                },
            );
        }
    }

    /// Remove expired entries from the cache.
    ///
    /// Call this periodically to prevent unbounded memory growth.
    pub fn cleanup(&self) {
        if let Ok(mut entries) = self.entries.write() {
            entries.retain(|_, entry| entry.inserted_at.elapsed() <= self.ttl);
        }
    }

    /// Get the number of entries in the cache (for metrics).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().map(|e| e.len()).unwrap_or(0)
    }

    /// Check if the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_set_and_get() {
        let cache = AccountCache::new(Duration::from_secs(60));

        assert!(cache.get("user-1").is_none());

        cache.set_exists("user-1");
        assert_eq!(cache.get("user-1"), Some(true));

        cache.set_not_exists("user-2");
        assert_eq!(cache.get("user-2"), Some(false));
    }

    #[test]
    fn cache_expiry() {
        let cache = AccountCache::new(Duration::from_millis(10));

        cache.set_exists("user-1");
        assert_eq!(cache.get("user-1"), Some(true));

        std::thread::sleep(Duration::from_millis(20));
        assert!(cache.get("user-1").is_none());
    }

    #[test]
    fn cache_cleanup() {
        let cache = AccountCache::new(Duration::from_millis(10));

        cache.set_exists("user-1");
        cache.set_exists("user-2");
        assert_eq!(cache.len(), 2);

        std::thread::sleep(Duration::from_millis(20));
        cache.cleanup();
        assert_eq!(cache.len(), 0);
    }
}
