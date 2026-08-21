//! Process-wide per-path mutation locks shared by `edit` and `write`.
//!
//! Both tools perform read-modify-write cycles; under a concurrent executor
//! two calls targeting the same file could otherwise interleave and the
//! later write would clobber the earlier one based on stale content.
//! Serializing on the resolved target path keeps same-file operations
//! ordered while distinct files stay parallel (pi precedent:
//! `file-mutation-queue.ts`). Registry entries are intentionally never
//! removed: each costs a few dozen bytes and the set is bounded by the
//! distinct files touched in the process lifetime.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Runs `operation` while holding the mutation lock for `target`. The lock
/// is a std sync lock because every filesystem step inside the critical
/// section is synchronous; handlers never await while holding it.
pub(crate) fn with_file_mutation<T>(target: &Path, operation: impl FnOnce() -> T) -> T {
    let key = target.to_path_buf();
    let lock = Arc::clone(
        registry()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(key)
            .or_default(),
    );
    let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
    operation()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn same_path_is_serialized_and_distinct_paths_are_independent() {
        static ACTIVE: AtomicUsize = AtomicUsize::new(0);
        static MAX_ACTIVE: AtomicUsize = AtomicUsize::new(0);
        static ENTERED_OTHER: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::temp_dir();
        let busy = base.join("philo-mutation-busy");
        let other = base.join("philo-mutation-other");

        let mut handles = Vec::new();
        for _ in 0..4 {
            let busy = busy.clone();
            handles.push(std::thread::spawn(move || {
                with_file_mutation(&busy, || {
                    let now = ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
                    MAX_ACTIVE.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    ACTIVE.fetch_sub(1, Ordering::SeqCst);
                });
            }));
        }
        handles.push(std::thread::spawn(move || {
            with_file_mutation(&other, || {
                ENTERED_OTHER.fetch_add(1, Ordering::SeqCst);
            });
        }));
        for handle in handles {
            handle.join().expect("test thread");
        }
        assert_eq!(MAX_ACTIVE.load(Ordering::SeqCst), 1, "same path serialized");
        assert_eq!(
            ENTERED_OTHER.load(Ordering::SeqCst),
            1,
            "distinct path not blocked"
        );
    }
}
