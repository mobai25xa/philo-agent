//! One-shot turn execution: assemble, prompt, replay events, map the exit
//! code. The CLI is a pure consumer of public APIs.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use philo_agent_runtime::{
    AgentRuntime, IdSource, OperationHandle, OperationOutcome, SessionId, SettlementDurability,
    UserMessage, UserPart,
};
use philo_coding_profile::CodingProfile;
use philo_model::PhiloModelAdapter;
use philo_session::SessionStore;
use philo_session_jsonl::JsonlSessionStore;

use crate::args::Cli;
use crate::config::{
    self, API_KEY_ENV, UsageError, generate_session_id, parse_reasoning_effort, resolve_data_dir,
};
use crate::image::load_image_part;
use crate::render::{Channel, Output, Renderer, Verbosity};

/// The two ids a single-shot process needs; uniqueness across processes
/// comes from the fresh id embedded per run.
struct CliIdSource {
    run_id: String,
}

impl IdSource for CliIdSource {
    fn next_operation_id(&self) -> philo_agent_runtime::OperationId {
        philo_agent_runtime::OperationId::new(format!("{}-op", self.run_id))
    }
    fn next_turn_id(&self) -> philo_agent_runtime::TurnId {
        philo_agent_runtime::TurnId::new(format!("{}-turn", self.run_id))
    }
}

fn write_outputs(outputs: &[Output]) {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    for output in outputs {
        match output.channel {
            Channel::Stdout => {
                let _ = stdout.write_all(output.text.as_bytes());
                let _ = stdout.flush();
            }
            Channel::Stderr => {
                let _ = stderr.write_all(output.text.as_bytes());
                let _ = stderr.flush();
            }
        }
    }
}

/// Builds the multi-part user message: image parts first, text last.
fn build_message(cli: &Cli, message: &str) -> Result<UserMessage, UsageError> {
    let mut parts = Vec::with_capacity(cli.image.len() + 1);
    for path in &cli.image {
        parts.push(load_image_part(path)?);
    }
    parts.push(UserPart::Text(message.to_owned()));
    UserMessage::from_parts(parts)
        .map_err(|error| UsageError(format!("invalid message: {error:?}")))
}

pub fn run_turn(cli: Cli) -> ExitCode {
    match prepare_and_drive(cli) {
        Ok(code) => code,
        Err(UsageError(message)) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

fn prepare_and_drive(cli: Cli) -> Result<ExitCode, UsageError> {
    let Some(message) = cli.message.clone() else {
        return Err(UsageError("a message argument is required".to_owned()));
    };
    let verbosity = if cli.quiet {
        Verbosity::Quiet
    } else if cli.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Default
    };

    // Resolve everything before any side effect: usage errors exit with 2
    // and the operation never starts.
    let deployment = config::resolve_deployment(cli.model.as_deref())?;
    let data_dir = resolve_data_dir(cli.data_dir.clone())?;
    let reasoning_effort = cli
        .reasoning_effort
        .as_deref()
        .map(parse_reasoning_effort)
        .transpose()?;
    let user_message = build_message(&cli, &message)?;

    let workspace_root = std::env::current_dir()
        .map_err(|error| UsageError(format!("cannot resolve the working directory: {error}")))?;
    let profile = CodingProfile::new(workspace_root);

    // flag > env > profile default.
    let mut runtime_config = profile.runtime_config(&deployment.model);
    if let Some(system) = &cli.system {
        runtime_config.system_prompt = system.clone();
    }
    if let Some(rounds) = cli.max_tool_rounds {
        runtime_config.max_tool_rounds = rounds;
    }
    if reasoning_effort.is_some() {
        runtime_config.generation.reasoning_effort = reasoning_effort;
    }

    let sessions = Arc::new(
        JsonlSessionStore::open(&data_dir)
            .map_err(|error| UsageError(format!("cannot open the session store: {error}")))?,
    );
    let adapter: PhiloModelAdapter = PhiloModelAdapter::builder(
        deployment.provider.clone(),
        deployment.protocol,
        deployment.model.clone(),
        deployment.endpoint.clone(),
    )
    .api_key_env(API_KEY_ENV)
    .build()
    .map_err(|error| UsageError(format!("model assembly failed: {error}")))?;

    let (session_id, is_new_session) = match &cli.session {
        Some(id) => (id.clone(), false),
        None => (generate_session_id(), true),
    };

    let quiet = verbosity == Verbosity::Quiet;
    if is_new_session && !quiet {
        eprintln!("session: {session_id}");
    }

    let runtime = AgentRuntime::with_tools(
        Arc::new(adapter),
        sessions.clone(),
        Arc::new(CliIdSource {
            run_id: generate_session_id(),
        }),
        runtime_config,
        Arc::new(profile.tool_registry()),
    );

    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| UsageError(format!("async runtime assembly failed: {error}")))?;

    Ok(tokio_runtime.block_on(drive(
        &runtime,
        sessions,
        session_id,
        cli.session.is_some(),
        user_message,
        verbosity,
    )))
}

async fn drive(
    runtime: &AgentRuntime,
    sessions: Arc<JsonlSessionStore>,
    session_id: String,
    continues_existing: bool,
    user_message: UserMessage,
    verbosity: Verbosity,
) -> ExitCode {
    let quiet = verbosity == Verbosity::Quiet;

    // Heuristic continuation notice: presentation-only, reads nothing but
    // the public context view.
    if continues_existing && !quiet {
        let stored_id = philo_session::SessionId::new(session_id.as_str());
        if let Ok(view) = sessions.context_view(&stored_id).await {
            let unfinished = view.messages().last().is_some_and(|message| {
                !matches!(message, philo_session::ContextMessage::Assistant { .. })
            });
            if unfinished {
                eprintln!(
                    "note: the previous turn did not finish normally; its partial \
                     trajectory remains in the context"
                );
            }
        }
    }

    // M10: acceptance returns the handle immediately; events stream in real
    // time while the handle is polled, and cancel() is reachable mid-run.
    let mut handle: OperationHandle = match runtime
        .prompt(SessionId::new(session_id.as_str()), user_message)
        .await
    {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("error: prompt rejected: {error:?}");
            return ExitCode::from(1);
        }
    };

    // Real-time event loop with two-level Ctrl+C: the first requests an
    // orderly cancellation (the cancel commit lands, exit 130), the second
    // forces an exit with an unconfirmed-state notice.
    let mut renderer = Renderer::new(verbosity);
    let mut interrupts: u32 = 0;
    loop {
        enum Step {
            Event(Option<philo_agent_runtime::AgentEvent>),
            Interrupt,
        }
        let step = tokio::select! {
            maybe = handle.next_event() => Step::Event(maybe),
            _ = tokio::signal::ctrl_c() => Step::Interrupt,
        };
        match step {
            Step::Event(Some(event)) => write_outputs(&renderer.render(&event)),
            Step::Event(None) => break,
            Step::Interrupt => {
                interrupts += 1;
                if interrupts == 1 {
                    eprintln!(
                        "\ncancelling: waiting for the orderly settlement \
                         (press Ctrl+C again to force quit)"
                    );
                    handle.cancel();
                } else {
                    eprintln!("forced exit: the session state may be unconfirmed");
                    std::process::exit(130);
                }
            }
        }
    }

    match handle.wait().await {
        OperationOutcome::Succeeded { .. } => ExitCode::SUCCESS,
        OperationOutcome::Failed { durability, .. } => {
            // The renderer already reported the failure and any UNCONFIRMED
            // warning from the settled event; keep the exit path silent.
            let _ = durability == SettlementDurability::Unconfirmed;
            ExitCode::from(1)
        }
        OperationOutcome::Cancelled => ExitCode::from(130),
    }
}
