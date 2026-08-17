//! Helpers for reopening a JSONL root after a runtime epoch ends.

use std::path::Path;
use std::time::Duration;

use philo_session_jsonl::{JsonlOpenError, JsonlSessionStore};

/// Open `path` after the previous writer has released its session locks.
///
/// Dropping `TestRuntime` only starts coordinator shutdown; the store lock
/// is released when that task actually drops.
pub async fn reopen(path: impl AsRef<Path>) -> JsonlSessionStore {
    let path = path.as_ref();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match JsonlSessionStore::open(path) {
            Ok(store) => return store,
            Err(JsonlOpenError::Locked { .. }) => {
                if tokio::time::Instant::now() >= deadline {
                    panic!(
                        "timed out waiting to reopen jsonl store at {}",
                        path.display()
                    );
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(error) => panic!("re-open jsonl store: {error}"),
        }
    }
}
