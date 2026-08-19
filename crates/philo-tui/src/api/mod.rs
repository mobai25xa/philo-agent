//! Stable public API for the composition root.

pub mod types;

pub use types::{
    RestoreFailure, RestoreReport, TerminalCapability, TuiLaunchConfig, TuiOutcome, TuiRecovery,
    TuiRecoveryAttachment, TuiRunReport, TuiScreen,
};
