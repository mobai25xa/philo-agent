//! Interactive TUI presentation layer for the philo coding agent.
//!
//! `philo-tui` is a pure event consumer: it holds no scenario knowledge
//! (tool lineups, prompts) and no composition knowledge (models, stores,
//! profiles). Everything arrives through the [`TuiHost`] interface the
//! composition root implements; runtime interaction goes through the
//! operation handles that interface returns.
//!
//! Behavioral contract: `current/tui.md` (key table, slash commands,
//! rendering discipline, confirmation channel, terminal obligations).

mod api;
mod app;
mod driver;
mod platform;
mod render;

#[cfg(test)]
mod tests;

pub use api::{
    ConfigEntry, ConfigReloadNotice, ConfirmationChannel, ConfirmationRequest,
    ConfirmationResponse, HostError, TuiConfig, TuiExit, TuiHost, TuiScreen,
};
pub use driver::run;
