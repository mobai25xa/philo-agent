//! Public launch configuration and session outcome.

/// One terminal mode capability acquired during setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalCapability {
    RawMode,
    AlternateScreen,
    MouseCapture,
    BracketedPaste,
    KeyboardEnhancement,
}

impl TerminalCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RawMode => "raw mode",
            Self::AlternateScreen => "alternate screen",
            Self::MouseCapture => "mouse capture",
            Self::BracketedPaste => "bracketed paste",
            Self::KeyboardEnhancement => "keyboard enhancement",
        }
    }
}

/// One restore step that failed while releasing a held capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreFailure {
    pub capability: TerminalCapability,
    pub message: String,
}

/// Structured result of an explicit finish/restore. Errors are collected
/// rather than swallowed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// True when this call actually restored the matching live session.
    pub restored: bool,
    /// True when a stale token was ignored so a newer session stays intact.
    pub skipped_stale: bool,
    /// Capabilities this call attempted to release (capability-driven order).
    pub attempted: Vec<TerminalCapability>,
    /// Capabilities successfully released in this call.
    pub restored_caps: Vec<TerminalCapability>,
    /// Restore steps that failed. Empty on a clean restore or skip.
    pub failures: Vec<RestoreFailure>,
}

/// One attachment preserved when a TUI instance cannot dispatch a submit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuiRecoveryAttachment {
    /// A path registered by `/image`; the next TUI resolves it on submit.
    Path(String),
    /// Image bytes already decoded by the previous TUI instance.
    Image {
        media_type: String,
        bytes: Vec<u8>,
        /// Original path or clipboard label shown in the composer.
        origin: String,
    },
}

/// Editable composer contents carried across a frontend restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiRecovery {
    pub draft: String,
    pub attachments: Vec<TuiRecoveryAttachment>,
}

impl TuiRecovery {
    pub fn is_empty(&self) -> bool {
        self.draft.is_empty() && self.attachments.is_empty()
    }
}

/// One TUI run: the session outcome plus the owner-thread restore report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiRunReport {
    /// How the interactive session ended.
    pub outcome: TuiOutcome,
    /// Structured terminal restore result. Never silently discarded.
    pub restore: RestoreReport,
    /// Composer contents that were not accepted by the Service.
    pub recovery: Option<TuiRecovery>,
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
    /// Terminal background RGB detected by the composition root (OSC 11),
    /// used to derive band and diff surface colors relative to the theme.
    /// `None` keeps stable fallback surfaces; the TUI never queries the
    /// terminal itself.
    pub terminal_palette: Option<(u8, u8, u8)>,
    /// Supervisor-owned Ctrl+C pulse counter. `None` in tests that do not
    /// inject signals. The TUI never writes the terminal or exits the process.
    pub interrupt: Option<tokio::sync::watch::Receiver<u64>>,
    /// One-shot composer contents from the previous TUI instance.
    pub recovery: Option<TuiRecovery>,
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
