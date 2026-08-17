//! Interactive command startup and ProcessSupervisor handoff.

mod supervisor;

use std::io::IsTerminal;
use std::process::ExitCode;

use crate::args::Cli;
use crate::assembly;
use crate::config::{LoadedConfig, ResolveFlags, Verbosity, WatchIntervals};
use crate::error::UsageError;
use crate::ids::fresh_session_id;

use self::supervisor::ProcessSupervisor;

pub fn run(runtime: tokio::runtime::Runtime, cli: Cli) -> Result<ExitCode, UsageError> {
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
    let flags = ResolveFlags::from_cli(&cli);
    // AgentRuntime::start spawns on Handle::try_current(). The interactive
    // path owns the Runtime value but has not yet block_on'd, so enter first.
    let bootstrap = {
        let _enter = runtime.enter();
        assembly::bootstrap(&cli, settings)?
    };
    let watch_client = bootstrap.client.clone();
    let watch_assembler = bootstrap.assembler.clone();
    let watch = crate::config::spawn(
        flags,
        WatchIntervals::default(),
        move |result| assembly::apply_config_reload(&watch_client, &watch_assembler, result),
        || {},
    )?;

    ProcessSupervisor::new(runtime, bootstrap, watch).run(session_id)
}

#[cfg(test)]
mod tests {
    use philo_agent_runtime::{AgentRuntime, RuntimeDeps};

    #[test]
    fn agent_runtime_start_requires_the_cli_runtime_to_be_entered() {
        match AgentRuntime::start(RuntimeDeps::default()) {
            Err(error) => assert!(
                error.message().contains("Tokio runtime"),
                "{}",
                error.message()
            ),
            Ok(_) => panic!("AgentRuntime::start must fail without a Tokio context"),
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let started = {
            let _enter = runtime.enter();
            AgentRuntime::start(RuntimeDeps::default())
        };
        assert!(
            started.is_ok(),
            "{}",
            started.err().map(|error| error.message().to_owned()).unwrap_or_default()
        );
    }
}
