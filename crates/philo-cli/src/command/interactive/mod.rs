//! Interactive command startup and TUI handoff.

mod host;
mod runtime_control;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;

use philo_tui::{TuiConfig, TuiExit};

use self::host::CliHost;
use crate::args::Cli;
use crate::assembly::RunAssembly;
use crate::config::{LoadedConfig, Verbosity};
use crate::error::UsageError;
use crate::ids::fresh_session_id;

pub async fn run(cli: Cli) -> Result<ExitCode, UsageError> {
    // Configuration errors deliberately precede terminal validation so both
    // execution modes validate the same effective settings.
    let config = LoadedConfig::load()?;
    let settings = config.resolve_run(&cli)?;
    if settings.verbosity != Verbosity::Quiet {
        for warning in config.warnings() {
            eprintln!("warning: {warning}");
        }
    }
    if !cli.image.is_empty() {
        eprintln!("warning: --image applies to single-shot mode; attach images with /image here");
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(UsageError::new(
            "the interactive session needs a terminal; pass a message argument for \
             single-shot mode",
        ));
    }

    let session_id = cli.session.clone().unwrap_or_else(fresh_session_id);
    let tui_config = TuiConfig {
        session_id,
        model_name: settings.deployment.model.clone(),
        verbose: settings.verbosity == Verbosity::Verbose,
        show_reasoning: settings.show_reasoning,
        context_window: settings.context_window,
    };
    let assembly = RunAssembly::prepare(&cli, settings)?;
    let host = Arc::new(CliHost::new(assembly));

    match philo_tui::run(host, tui_config).await {
        Ok(TuiExit::Normal) => Ok(ExitCode::SUCCESS),
        Err(error) => {
            eprintln!("error: the interactive session ended abnormally: {error}");
            Ok(ExitCode::from(1))
        }
    }
}
