//! `philo` — coding agent CLI: interactive session, or one-shot turn.
//!
//! Composition root only: scenario knowledge lives in
//! `philo-coding-profile`, persistence in `philo-session-jsonl`, model
//! access in `philo-model`, presentation of the interactive session in
//! `philo-tui`. Channel discipline for the single-shot path: stdout carries
//! the model's answer text and (for `sessions`) the listing; everything else
//! is stderr.

mod args;
mod assembly;
mod command;
mod config;
mod error;
mod ids;
mod render;

use std::process::ExitCode;

use clap::Parser;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut cli = args::Cli::parse();
    let result = match cli.command.take() {
        Some(args::Command::Sessions { data_dir }) => command::sessions::run(data_dir),
        // A message argument means single-shot; a bare `philo` opens the
        // interactive session.
        None if cli.message.is_some() => command::oneshot::run(cli).await,
        None => command::interactive::run(cli).await,
    };
    match result {
        Ok(code) => code,
        Err(error::UsageError(message)) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}
