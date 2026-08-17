//! Public launch configuration and exit result.

/// A composition-root notice about config reload. The TUI never opens
/// files or environment variables; it only displays these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigReloadNotice {
    Applied {
        show_reasoning: bool,
        verbose: bool,
        context_window: Option<u64>,
        model_name: String,
        runtime_pending: bool,
        warnings: Vec<String>,
    },
    Failed {
        message: String,
        clear_pending: bool,
    },
    Pending,
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
#[derive(Debug)]
pub struct TuiConfig {
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
    /// Optional notices from the composition-root watcher.
    pub config_notices: Option<tokio::sync::mpsc::UnboundedReceiver<ConfigReloadNotice>>,
}

/// How the interactive session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiExit {
    /// User-requested exit: process exit code 0.
    Normal,
}
