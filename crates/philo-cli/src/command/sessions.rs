//! The `sessions` subcommand: read-only enumeration through the store's
//! public interface, with advisory display titles when the store knows one.

use std::path::PathBuf;
use std::process::ExitCode;

use philo_session::{SessionStore, SessionSummary};
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
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: cannot start the runtime: {error}");
            return Ok(ExitCode::from(1));
        }
    };
    let listed = runtime.block_on(SessionStore::list_session_summaries(&store));
    let _ = store.shutdown();
    match listed {
        Ok(mut sessions) => {
            sessions.sort_by(|a, b| a.session_id.as_str().cmp(b.session_id.as_str()));
            for session in sessions {
                println!("{}", summary_line(&session));
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            eprintln!("error: cannot list sessions: {error:?}");
            Ok(ExitCode::from(1))
        }
    }
}

/// `{id}` alone, or `{id}  {title}` when a title exists. Titles are
/// advisory; the id always leads so output stays machine-greppable.
fn summary_line(summary: &SessionSummary) -> String {
    let Some(title) = &summary.title else {
        return summary.session_id.as_str().to_owned();
    };
    format!("{}  {}", summary.session_id.as_str(), title)
}
