//! Single-shot turn orchestration.

mod drive;
mod message;

use std::process::ExitCode;

use philo_agent_runtime::AgentRuntime;

use crate::args::Cli;
use crate::assembly::RunAssembly;
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

    let assembly = RunAssembly::prepare(&cli, settings)?;
    let (session_id, is_new_session) = match &cli.session {
        Some(id) => (id.clone(), false),
        None => (fresh_session_id(), true),
    };
    if is_new_session && assembly.settings.verbosity != Verbosity::Quiet {
        eprintln!("session: {session_id}");
    }

    let verbosity = assembly.settings.verbosity;
    let show_reasoning = assembly.settings.show_reasoning;
    let sessions = assembly.sessions;
    let runtime = AgentRuntime::with_tools(
        assembly.model,
        sessions.clone(),
        assembly.ids,
        assembly.runtime_config,
        assembly.tools,
    );

    Ok(drive::run(drive::Request {
        runtime,
        sessions,
        session_id,
        continues_existing: cli.session.is_some(),
        user_message,
        verbosity,
        show_reasoning,
    })
    .await)
}
