//! Public launch configuration and session outcome.

/// Structured result of an explicit restore. Errors are collected rather
/// than swallowed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// True when this call actually restored the matching live session.
    pub restored: bool,
    /// True when a stale token was ignored so a newer session stays intact.
    pub skipped_stale: bool,
    /// Restore steps that failed. Empty on a clean restore or skip.
    pub errors: Vec<String>,
}

/// One TUI run: the session outcome plus the owner-thread restore report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiRunReport {
    /// How the interactive session ended.
    pub outcome: TuiOutcome,
    /// Structured terminal restore result. Never silently discarded.
    pub restore: RestoreReport,
}

/// Alternate screen versus an inline viewport on the main buffer.
/// Chosen by the composition root; a session never switches modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiScreen {
    Alternate,
    Inline,
}

/// Deployment inputs for one interactive session (from the composition
/// root's configuration chain).
#[derive(Clone, Debug)]
pub struct TuiLaunchConfig {
    pub session_id: String,
    pub model_name: String,
    /// Initial information tier (`--verbose` maps to verbose rendering).
    pub verbose: bool,
    /// Whether visible reasoning reaches the transcript at all.
    pub show_reasoning: bool,
    /// Context-budget hint for the status bar.
    pub context_window: Option<u64>,
    /// Screen mode for this session. Hot reload must not change it.
    pub screen: TuiScreen,
    /// Supervisor-owned Ctrl+C pulse counter. `None` in tests that do not
    /// inject signals. The TUI never writes the terminal or exits the process.
    pub interrupt: Option<tokio::sync::watch::Receiver<u64>>,
}

/// How the interactive session ended. Process exit codes belong to the
/// supervisor, not this crate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuiOutcome {
    /// Idle user quit (`/quit`, idle Ctrl+C, empty Ctrl+D).
    UserExit,
    /// Terminal or frontend input fault; supervisor may restart the TUI.
    FrontendRestartRequested { fault: String },
    /// Consecutive faults exceeded budget; supervisor should fall back.
    FallbackRequested { fault: String },
    /// User asked to shut the process down while work was still active.
    ProcessShutdownRequested,
    /// Supervisor-requested forced process exit after restore.
    ForcedExitRequested { code: u8 },
}
