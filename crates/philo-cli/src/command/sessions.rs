//! The `sessions` subcommand: read-only enumeration through the store's
//! public interface.

use std::path::PathBuf;
use std::process::ExitCode;

use philo_session_jsonl::JsonlSessionStore;

use crate::config::LoadedConfig;
use crate::error::UsageError;

pub fn run(data_dir_flag: Option<PathBuf>) -> Result<ExitCode, UsageError> {
    let config = LoadedConfig::load()?;
    for warning in config.warnings() {
        eprintln!("warning: {warning}");
    }
    let (data_dir, _source) = config.resolve_data_dir(data_dir_flag)?;
    if !data_dir.exists() {
        // Listing an absent root is a successful empty result.
        return Ok(ExitCode::SUCCESS);
    }
    let store = match JsonlSessionStore::open(&data_dir) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("error: cannot open the session store: {error}");
            return Ok(ExitCode::from(1));
        }
    };
    match store.list_sessions() {
        Ok(mut sessions) => {
            sessions.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            for session in sessions {
                println!("{}", session.as_str());
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            eprintln!("error: cannot list sessions: {error}");
            Ok(ExitCode::from(1))
        }
    }
}
