//! Single-shot turn orchestration.

pub(crate) mod drive;
mod message;

use std::process::ExitCode;

use crate::args::Cli;
use crate::assembly;
use crate::config::{LoadedConfig, Verbosity};
use crate::error::UsageError;
use crate::ids::fresh_session_id;

pub async fn run(cli: Cli) -> Result<ExitCode, UsageError> {
    let message_text = cli
        .message
        .as_deref()
        .ok_or_else(|| UsageError::new("a message argument is required"))?;

    // Resolve all usage-sensitive input before assembling side-effecting
    // adapters or admitting an operation.
    let config = LoadedConfig::load()?;
    let settings = config.resolve_run(&cli)?;
    if settings.verbosity != Verbosity::Quiet {
        for warning in config.warnings() {
            eprintln!("warning: {warning}");
        }
    }
    let user_message = message::build(&cli, message_text)?;

    let bootstrap = assembly::bootstrap(&cli, settings)?;
    let (session_id, is_new_session) = match &cli.session {
        Some(id) => (id.clone(), false),
        None => (fresh_session_id(), true),
    };
    if is_new_session && bootstrap.settings.verbosity != Verbosity::Quiet {
        eprintln!("session: {session_id}");
    }

    let verbosity = bootstrap.settings.verbosity;
    let show_reasoning = bootstrap.settings.show_reasoning;
    let (interrupt_tx, interrupt_rx) = tokio::sync::watch::channel(0u64);
    tokio::spawn(crate::command::ctrl_c::forward_os_ctrl_c(interrupt_tx));
    let report = drive::run(drive::Request {
        client: bootstrap.client.clone(),
        sessions: Some(bootstrap.sessions.clone()),
        session_id,
        continues_existing: cli.session.is_some(),
        user_message: Some(user_message),
        verbosity,
        show_reasoning,
        success_exit: 0,
        interrupt: interrupt_rx,
    })
    .await;
    if report.forced {
        drop(bootstrap);
        Ok(report.exit_code())
    } else {
        assembly::shutdown(bootstrap).await;
        Ok(report.exit_code())
    }
}
