//! Per-session generation cache with a hand-written LRU.
//!
//! The workspace keeps zero external cache dependencies; this crate follows
//! the same pattern (plain `HashMap` + manual eviction). The cache holds at
//! most `capacity` hot sessions; eviction drops the least-recently-used
//! `Arc<RuntimeGeneration>` (its `Drop` releases the model port connection
//! pool via `Arc` reference counting). Cold sessions rebuild lazily from
//! the persisted `SessionGenerationChoice`.

use std::collections::HashMap;
use std::sync::Arc;

use philo_agent_runtime::RuntimeGeneration;

/// Per-session `RuntimeGeneration` cache with LRU eviction.
///
/// `get` updates the access order; `put` inserts/updates and evicts the
/// least-recently-used entry when the capacity is exceeded. The capacity is
/// small (default 8) so eviction is O(n) over a tiny map.
pub(crate) struct SessionGenerationCache {
    entries: HashMap<String, (u64, Arc<RuntimeGeneration>)>,
    capacity: usize,
    tick: u64,
}

impl SessionGenerationCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
            tick: 0,
        }
    }

    /// Returns a hot session's generation, refreshing its access order.
    pub(crate) fn get(&mut self, session_id: &str) -> Option<Arc<RuntimeGeneration>> {
        let tick = self.next_tick();
        match self.entries.get_mut(session_id) {
            Some((stamp, generation)) => {
                *stamp = tick;
                Some(generation.clone())
            }
            None => None,
        }
    }

    /// Inserts or replaces a session's generation, evicting the LRU entry
    /// when the capacity is exceeded.
    #[allow(clippy::map_entry)]
    pub(crate) fn put(&mut self, session_id: String, generation: Arc<RuntimeGeneration>) {
        if self.entries.contains_key(&session_id) {
            let tick = self.next_tick();
            self.entries.insert(session_id, (tick, generation));
            return;
        }
        if self.entries.len() >= self.capacity {
            self.evict_lru();
        }
        let tick = self.next_tick();
        self.entries.insert(session_id, (tick, generation));
    }

    /// Explicitly removes a session's entry (session close).
    #[allow(dead_code)]
    pub(crate) fn remove(&mut self, session_id: &str) {
        self.entries.remove(session_id);
    }

    fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.saturating_add(1);
        self.tick
    }

    fn evict_lru(&mut self) {
        let lru_key = match self
            .entries
            .iter()
            .min_by_key(|(_, (stamp, _))| *stamp)
            .map(|(key, _)| key.clone())
        {
            Some(key) => key,
            None => return,
        };
        self.entries.remove(&lru_key);
    }
}
