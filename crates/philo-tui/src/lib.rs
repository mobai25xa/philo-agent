//! Interactive TUI presentation layer for the philo coding agent.
//!
//! `philo-tui` talks only to [`philo_agent_service::FrontendClient`]. It
//! holds no scenario knowledge (tool lineups, prompts) and no composition
//! knowledge (models, stores, profiles). App reducers do not perform I/O
//! and do not call Runtime.
//!
//! Behavioral contract: `current/tui.md`.

mod api;
mod app;
mod driver;
mod platform;
mod render;

#[cfg(test)]
mod tests;

pub use api::{
    RestoreFailure, RestoreReport, TerminalCapability, TuiLaunchConfig, TuiOutcome, TuiRecovery,
    TuiRecoveryAttachment, TuiRunReport, TuiScreen,
};
pub use driver::{run, run_async};
pub use platform::input::{TerminalInput, TerminalInputFault, TerminalInputSource};
