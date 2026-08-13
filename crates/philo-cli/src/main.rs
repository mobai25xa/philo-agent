//! `philo` — single-shot, read-only coding agent CLI.
//!
//! Composition root only: scenario knowledge lives in
//! `philo-coding-profile`, persistence in `philo-session-jsonl`, model
//! access in `philo-model`. Channel discipline: stdout carries the model's
//! answer text and (for `sessions`) the listing; everything else is stderr.

mod args;
mod config;
mod image;
mod render;
mod run;
mod sessions;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = args::Cli::parse();
    match cli.command {
        Some(args::Command::Sessions { data_dir }) => sessions::run_sessions(data_dir),
        None => run::run_turn(cli),
    }
}
