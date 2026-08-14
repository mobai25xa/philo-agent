//! JSONL durable backend for the `philo-session` store contract.
//!
//! Layout: one directory per session (`{root}/{session_dir}/`) holding an
//! append-only `log.jsonl` transaction log and an advisory `lock` file. One
//! line is one committed `SessionTransaction` (schema v1 envelope); `commit`
//! returns only after the line is fsynced, so `Confirmed` means on disk.
//!
//! Recovery replays complete transaction lines through the shared
//! `SessionProjection` validation core: a physically incomplete tail line is
//! truncated as crash residue, mid-log corruption refuses to open, and the
//! recovery point always lands on a committed transaction boundary
//! (`adr/ADR-0001-jsonl-format-and-recovery.md`).
//!
//! Image bytes are persisted as content-addressed artifact files under
//! `{session_dir}/artifacts/{sha256}`; the log line carries only the
//! reference. Newly referenced artifacts are fsynced before the transaction
//! line is appended, orphan artifacts are tolerated and reported, and a
//! missing or mismatching referenced artifact refuses to open
//! (`adr/ADR-0002-image-artifact-persistence.md`).

mod artifact;
mod error;
mod schema;
mod store;

pub use error::JsonlOpenError;
pub use store::{JsonlSessionStore, RecoveryReport};
