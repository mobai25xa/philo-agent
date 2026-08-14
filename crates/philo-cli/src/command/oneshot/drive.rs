//! Real-time one-shot operation driving: continuation notice, event output,
//! Ctrl+C policy, and terminal outcome mapping.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use philo_agent_runtime::{
    AgentRuntime, OperationHandle, OperationOutcome, SessionId, UserMessage,
};
use philo_session::SessionStore;
use philo_session_jsonl::JsonlSessionStore;

use crate::config::Verbosity;
use crate::render::{Channel, Output, Renderer};

pub(super) struct Request {
    pub runtime: AgentRuntime,
    pub sessions: Arc<JsonlSessionStore>,
    pub session_id: String,
    pub continues_existing: bool,
    pub user_message: UserMessage,
    pub verbosity: Verbosity,
    pub show_reasoning: bool,
}

pub(super) async fn run(request: Request) -> ExitCode {
    let quiet = request.verbosity == Verbosity::Quiet;

    // Presentation-only heuristic over the public context view.
    if request.continues_existing && !quiet {
        let stored_id = philo_session::SessionId::new(request.session_id.as_str());
        if let Ok(view) = request.sessions.context_view(&stored_id).await {
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

    let mut handle: OperationHandle = match request
        .runtime
        .prompt(
            SessionId::new(request.session_id.as_str()),
            request.user_message,
        )
        .await
    {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("error: prompt rejected: {error:?}");
            return ExitCode::from(1);
        }
    };

    // The first Ctrl+C requests orderly cancellation; the second forces an
    // immediate process exit because durable state can no longer be promised.
    let mut renderer = Renderer::new(request.verbosity).with_reasoning(request.show_reasoning);
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
        OperationOutcome::Failed { .. } => ExitCode::from(1),
        OperationOutcome::Cancelled => ExitCode::from(130),
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
