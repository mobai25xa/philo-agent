//! `philo` — coding agent CLI: interactive session, or one-shot turn.
//!
//! Composition root: parse flags, construct an explicit multi-thread Tokio
//! runtime, inject real adapters into `AgentService`, then either consume a
//! oneshot frontend or hand the main thread to `ProcessSupervisor`.
//!
//! Process exit codes come only from returning [`ExitCode`]. Workers must
//! never call `process::exit`.

mod args;
mod assembly;
mod command;
mod config;
mod error;
mod ids;
mod render;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let mut cli = args::Cli::parse();
    let result = match cli.command.take() {
        Some(args::Command::Sessions { data_dir }) => command::sessions::run(data_dir),
        None if cli.message.is_some() => {
            multi_thread_runtime().and_then(|rt| rt.block_on(command::oneshot::run(cli)))
        }
        None => multi_thread_runtime().and_then(|rt| command::interactive::run(rt, cli)),
    };
    match result {
        Ok(code) => code,
        Err(error::UsageError(message)) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

fn multi_thread_runtime() -> Result<tokio::runtime::Runtime, error::UsageError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error::UsageError::new(format!("cannot start the runtime: {error}")))
}
