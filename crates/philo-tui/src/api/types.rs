//! Public launch configuration and exit result.

/// Deployment inputs for one interactive session (from the composition
/// root's configuration chain).
#[derive(Clone, Debug)]
pub struct TuiConfig {
    pub session_id: String,
    pub model_name: String,
    /// Initial information tier (`--verbose` maps to verbose rendering).
    pub verbose: bool,
    /// Whether visible reasoning reaches the transcript at all.
    pub show_reasoning: bool,
    /// Context-budget hint for the status bar.
    pub context_window: Option<u64>,
}

/// How the interactive session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiExit {
    /// User-requested exit: process exit code 0.
    Normal,
}
