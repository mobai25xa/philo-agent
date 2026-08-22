//! Command surface. Usage errors exit with code 2 (clap's default).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "philo",
    version,
    about = "Coding agent over the Philo runtime; bare `philo` opens an interactive session"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// The user message for this turn; omit it to open an interactive session.
    pub message: Option<String>,

    /// Continue this session; an unknown id starts a new session under it.
    #[arg(long)]
    pub session: Option<String>,

    /// Model name; defaults to PHILO_MODEL.
    #[arg(long)]
    pub model: Option<String>,

    /// Session root directory; defaults to PHILO_DATA_DIR, then ~/.philo/sessions.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Override the profile's coding system prompt.
    #[arg(long)]
    pub system: Option<String>,

    /// Override the profile's tool-round upper bound.
    #[arg(long)]
    pub max_tool_rounds: Option<u32>,

    /// Reasoning effort: minimal|low|medium|high|xhigh|max.
    #[arg(long)]
    pub reasoning_effort: Option<String>,

    /// Attach an image to the message (repeatable). Media type is inferred
    /// from the file extension.
    #[arg(long)]
    pub image: Vec<PathBuf>,

    /// Tool and diagnostic details.
    #[arg(long, conflicts_with = "quiet")]
    pub verbose: bool,

    /// Silence everything except the answer (stderr keeps errors only).
    #[arg(long)]
    pub quiet: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List the session ids present in the data directory.
    Sessions {
        /// Session root directory; defaults to PHILO_DATA_DIR, then ~/.philo/sessions.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}
