//! Hard caps from the Wave 1 runtime/TUI refactor contract.
//!
//! Full channels return an explicit error, merge, or `needs_resync`. They
//! never grow into unbounded containers.

pub use philo_agent_runtime::{RUNTIME_RELIABLE_STAGING_CAP, TRANSIENT_KIND_COUNT};

/// Runtime command-lane capacity (service does not own this channel).
pub const RUNTIME_COMMAND_CAP: usize = 32;
/// Runtime control-lane capacity (service does not own this channel).
pub const RUNTIME_CONTROL_CAP: usize = 16;
/// Runtime subscription capacity (service does not own this channel).
pub const RUNTIME_EVENT_CAP: usize = 256;
/// Maximum queued operations mirrored into the live snapshot.
pub const RUNTIME_QUEUE_MAX: usize = 32;
/// Maximum driver events drained per actor turn after the first `recv`.
pub const RUNTIME_DRIVER_EVENT_BUDGET: usize = 32;
/// Maximum in-flight service child tasks (submit, store view, install, cancel, compaction).
/// Shutdown always gets an extra slot beyond this cap.
pub const STORE_COMMAND_CAP: usize = 64;
/// Blocking-tool queue cap from the shared contract (owned by tools-std).
pub const BLOCKING_TOOL_QUEUE: usize = 32;
/// Service → frontend update lane.
pub const FRONTEND_UPDATE_CAP: usize = 64;
/// Frontend command lane (submit, queries, install).
pub const FRONTEND_COMMAND_CAP: usize = 32;
/// Frontend control lane (cancel, confirmation, shutdown).
pub const FRONTEND_CONTROL_CAP: usize = 16;
/// Supervisor lifecycle lane (attach, detach). Independent of submit/list traffic.
pub const FRONTEND_SUPERVISOR_CAP: usize = 4;
/// Reserved snapshot-request lane so resync cannot be starved by submit.
pub const FRONTEND_SNAPSHOT_CAP: usize = 1;
/// Maximum live assistant text retained for the current operation.
pub const LIVE_TEXT_CHARS_MAX: usize = 65536;
/// Maximum live reasoning text retained for the current operation.
pub const LIVE_REASONING_CHARS_MAX: usize = 65536;
/// Supervisor restart budget (owned by CLI; recorded here for the contract).
pub const FRONTEND_RESTART_BUDGET: u32 = 3;
/// Supervisor restart window (owned by CLI; recorded here for the contract).
pub const FRONTEND_RESTART_WINDOW_SECS: u64 = 60;
/// Adjacent live delta merge chunk.
pub const DELTA_MERGE_CHUNK_MAX: usize = 4096;
/// In-flight tool-progress rows retained in the live snapshot.
pub const LIVE_TOOL_PROGRESS_MAX: usize = 32;
/// Pending confirmation map capacity. Overflow auto-denies the new request.
pub const CONFIRMATION_MAP_CAP: usize = 16;
