//! Configuration module.
//!
//! Its interface hides TOML discovery/merging and precedence resolution.
//! Callers either resolve all run settings or only the session directory
//! needed by the read-only `sessions` command.

mod file;
pub(crate) mod resolve;
mod watch;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use crate::args::Cli;
use crate::error::UsageError;

pub use resolve::{Credential, Deployment, Settings, Verbosity, deployment_for};
pub use watch::{ResolveFlags, WatchIntervals, WatchTask, spawn};

/// Loaded global/project file layers. Raw keys and source bookkeeping remain
/// private to the configuration implementation.
pub struct LoadedConfig {
    file: file::FileConfig,
}

impl LoadedConfig {
    pub fn load() -> Result<Self, UsageError> {
        Ok(Self {
            file: file::load()?,
        })
    }

    pub fn warnings(&self) -> &[String] {
        &self.file.warnings
    }

    pub fn resolve_run(&self, cli: &Cli) -> Result<Settings, UsageError> {
        resolve::resolve(cli, &self.file)
    }

    pub fn resolve_data_dir(
        &self,
        flag: Option<PathBuf>,
    ) -> Result<(PathBuf, &'static str), UsageError> {
        resolve::resolve_data_dir(flag, &self.file)
    }
}
