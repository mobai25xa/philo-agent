//! The `sessions` subcommand: read-only enumeration through the store's
//! public API. The CLI never parses directory names itself.

use std::path::PathBuf;
use std::process::ExitCode;

use philo_session_jsonl::JsonlSessionStore;

use crate::config::{UsageError, resolve_data_dir};

pub fn run_sessions(data_dir_flag: Option<PathBuf>) -> ExitCode {
    let data_dir = match resolve_data_dir(data_dir_flag) {
        Ok(dir) => dir,
        Err(UsageError(message)) => {
            eprintln!("error: {message}");
            return ExitCode::from(2);
        }
    };
    if !data_dir.exists() {
        // No data directory means no sessions; listing an absent root is
        // not an error for a read-only command.
        return ExitCode::SUCCESS;
    }
    let store = match JsonlSessionStore::open(&data_dir) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("error: cannot open the session store: {error}");
            return ExitCode::from(1);
        }
    };
    match store.list_sessions() {
        Ok(mut sessions) => {
            sessions.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            for session in sessions {
                println!("{}", session.as_str());
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: cannot list sessions: {error}");
            ExitCode::from(1)
        }
    }
}
