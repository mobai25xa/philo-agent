//! Same-turn reasoning replay side channel.
//!
//! Providers with signed or opaque reasoning require later model calls of the
//! same turn to replay the earlier calls' reasoning state. That state has no
//! place in the kernel transcript or the session: the adapter keeps it in a
//! turn-scoped in-memory cache. The cache rotates when a call for a different
//! turn arrives, so state never leaks across turns or operations, and a turn
//! that ended in any terminal state (success, failure, cancellation) is
//! dropped wholesale. No I/O, no global state.

use std::sync::Mutex;

use philo::api::stable as sdk;

/// One reasoning block captured from a finished call, replayed verbatim.
#[derive(Clone)]
pub(crate) struct CachedReasoning {
    pub kind: sdk::ReasoningKind,
    pub text: Option<String>,
    pub replay_requirement: sdk::ReplayRequirement,
    pub replay_token: Option<sdk::ReplayToken>,
}

struct ChannelState {
    /// Identifies the turn the cache belongs to (operation + turn ids).
    key: Option<String>,
    /// `calls[i]` holds the reasoning captured by logical call `i + 1`.
    calls: Vec<Vec<CachedReasoning>>,
}

/// Turn-scoped reasoning replay cache shared between the adapter and its
/// in-flight normalized stream.
pub(crate) struct ReplayChannel {
    state: Mutex<ChannelState>,
}

impl ReplayChannel {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ChannelState {
                key: None,
                calls: Vec::new(),
            }),
        }
    }

    /// Rotates the cache to the given turn (dropping any other turn's state)
    /// and returns the reasoning captured by this turn's earlier calls.
    pub fn begin_call(&self, key: &str) -> Vec<Vec<CachedReasoning>> {
        let mut state = self.state.lock().expect("replay channel mutex");
        if state.key.as_deref() != Some(key) {
            state.key = Some(key.to_owned());
            state.calls.clear();
        }
        state.calls.clone()
    }

    /// Records the reasoning captured by logical call `call_index` (1-based)
    /// of the given turn. Stale writes from another turn are discarded.
    pub fn record(&self, key: &str, call_index: u32, items: Vec<CachedReasoning>) {
        let mut state = self.state.lock().expect("replay channel mutex");
        if state.key.as_deref() != Some(key) {
            return;
        }
        let slot = call_index.saturating_sub(1) as usize;
        if state.calls.len() <= slot {
            state.calls.resize_with(slot + 1, Vec::new);
        }
        state.calls[slot] = items;
    }
}
