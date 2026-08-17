//! RUNTIME-001: Direct Answer 受理、事件、提交失败

mod support;

use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, AgentRuntime, DEFAULT_MAX_TOOL_ROUNDS, GenerationConfig,
    ModelMessage, OperationHandle, OperationOutcome, OperationPhase, OperationStatus,
    RuntimeConfig, SequentialIdSource, SessionId, SettlementDurability, UserMessage, UserPart,
};
use philo_session::{
    ContextMessage, MemorySessionStore, SessionCommit, SessionContextView, SessionError,
    SessionFuture, SessionStore, SessionTransaction, SessionUserPart,
};
use support::fake_model::FakeModel;

fn block_on<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn config() -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "answer directly".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        compaction: Default::default(),
    }
}

fn runtime(model: Arc<FakeModel>, sessions: Arc<dyn SessionStore>) -> AgentRuntime {
    AgentRuntime::new(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(),
    )
}

fn collect_events(mut handle: philo_agent_runtime::OperationHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    events
}

fn drain(mut handle: OperationHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    events
}

#[test]
fn direct_answer_success() {
    let model = Arc::new(FakeModel::succeeds(&["hel", "lo"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let mut handle = block_on(
        runtime(model.clone(), sessions.clone())
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    // M10: acceptance returns immediately; waiting drives to settled.
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "hello"
    ));
    assert_eq!(
        handle.phase(),
        &OperationPhase::Settled(OperationStatus::Succeeded)
    );
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
        events.push(event);
    }
    assert!(matches!(events[0], AgentEvent::OperationStarted { .. }));
    assert!(matches!(events[1], AgentEvent::TurnStarted { .. }));
    assert!(matches!(events[2], AgentEvent::ModelCallStarted { .. }));
    assert!(matches!(
        events[5],
        AgentEvent::AssistantMessageCompleted { .. }
    ));
    assert!(matches!(events[6], AgentEvent::OperationSettled { .. }));

    let calls = model.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].messages,
        vec![
            ModelMessage::System {
                content: "answer directly".to_owned(),
            },
            ModelMessage::User {
                parts: vec![UserPart::Text("hi".to_owned())],
            },
        ]
    );
    let stored_id = philo_session::SessionId::new("session");
    assert_eq!(
        block_on(sessions.context_view(&stored_id))
            .unwrap()
            .messages(),
        &[
            ContextMessage::User {
                parts: SessionUserPart::text_parts("hi"),
            },
            ContextMessage::Assistant {
                content: "hello".to_owned(),
            },
        ]
    );
}

#[test]
fn start_commit_failure_prevents_model_call() {
    let model = Arc::new(FakeModel::succeeds(&["unused"]));
    let sessions = Arc::new(FailingStore::always());
    let handle = block_on(
        runtime(model.clone(), sessions).prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(model.calls().is_empty());
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
}

#[test]
fn model_error_settles_failed() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(model, sessions).prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            failure,
            durability: SettlementDurability::Confirmed,
        } if failure.kind() == AgentFailureKind::ModelCall
    ));
}

#[test]
fn final_commit_failure_prevents_success() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(FailingStore::after_successful_commits(1));
    let handle = block_on(
        runtime(model, sessions).prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            failure,
            durability: SettlementDurability::Unconfirmed,
        } if failure.kind() == AgentFailureKind::Persistence
    ));
    let events = collect_events(handle);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
    );
}

#[test]
fn delta_ordering_preserved() {
    let model = Arc::new(FakeModel::succeeds(&["a", "b", "c"]));
    let handle = block_on(
        runtime(model, Arc::new(MemorySessionStore::new()))
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    let deltas = collect_events(handle)
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { delta } => Some(delta),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deltas, ["a", "b", "c"]);
}

#[test]
fn terminal_event_emitted_once() {
    let model = Arc::new(FakeModel::stream_fails_after(&["partial"], "broken"));
    let handle = block_on(
        runtime(model, Arc::new(MemorySessionStore::new()))
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    let terminal_count = collect_events(handle)
        .iter()
        .filter(|event| matches!(event, AgentEvent::OperationSettled { .. }))
        .count();
    assert_eq!(terminal_count, 1);
}

#[test]
fn model_failure_is_durably_settled() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(model, sessions.clone()).prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    let events = collect_events(handle);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFailed { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::OperationSettled {
            status: OperationStatus::Failed,
            durability: SettlementDurability::Confirmed,
            ..
        }
    )));
    let stored_id = philo_session::SessionId::new("session");
    assert_eq!(
        block_on(sessions.context_view(&stored_id))
            .unwrap()
            .revision(),
        philo_session::SessionRevision::new(2)
    );
}

#[test]
fn persistent_session_failure_settles_unconfirmed() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let sessions = Arc::new(FailingStore::after_successful_commits(1));
    let handle = block_on(
        runtime(model, sessions).prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
    let events = collect_events(handle);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFailed { .. }))
    );
}

#[test]
fn single_turn_success() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(model, sessions.clone()).prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "hello"
    ));
    let events = drain(handle);
    assert!(matches!(
        events.as_slice(),
        [
            AgentEvent::OperationStarted { .. },
            AgentEvent::TurnStarted { .. },
            AgentEvent::ModelCallStarted { .. },
            AgentEvent::TextDelta { delta },
            AgentEvent::AssistantMessageCompleted { .. },
            AgentEvent::OperationSettled {
                status: OperationStatus::Succeeded,
                ..
            }
        ] if delta == "hello"
    ));
    let stored_id = philo_session::SessionId::new("session");
    assert_eq!(
        block_on(sessions.context_view(&stored_id))
            .unwrap()
            .messages(),
        &[
            ContextMessage::User {
                parts: SessionUserPart::text_parts("hi"),
            },
            ContextMessage::Assistant {
                content: "hello".to_owned(),
            },
        ]
    );
}

#[test]
fn start_commit_failure_skips_model() {
    let model = Arc::new(FakeModel::succeeds(&["unused"]));
    let handle = block_on(
        runtime(model.clone(), Arc::new(FailingStore::after(0)))
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(model.calls().is_empty());
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
}

#[test]
fn final_commit_failure_never_publishes_success() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let handle = block_on(
        runtime(model, Arc::new(FailingStore::after(1)))
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed { .. }
    ));
    let events = drain(handle);
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::AssistantMessageCompleted { .. }
            | AgentEvent::OperationSettled {
                status: OperationStatus::Succeeded,
                ..
            }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::OperationSettled { .. }))
            .count(),
        1
    );
}

#[test]
fn second_turn_contains_first_turn_context() {
    let model = Arc::new(FakeModel::succeeds_sequence(vec![
        vec!["first"],
        vec!["second"],
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let runtime = runtime(model.clone(), sessions.clone());
    let first =
        block_on(runtime.prompt(SessionId::new("session"), UserMessage::new("one"))).unwrap();
    assert!(matches!(
        block_on(first.wait()),
        OperationOutcome::Succeeded { .. }
    ));
    let second =
        block_on(runtime.prompt(SessionId::new("session"), UserMessage::new("two"))).unwrap();
    assert!(matches!(
        block_on(second.wait()),
        OperationOutcome::Succeeded { .. }
    ));

    let calls = model.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1].messages,
        vec![
            ModelMessage::System {
                content: "answer directly".to_owned(),
            },
            ModelMessage::User {
                parts: vec![UserPart::Text("one".to_owned())],
            },
            ModelMessage::Assistant {
                content: "first".to_owned(),
            },
            ModelMessage::User {
                parts: vec![UserPart::Text("two".to_owned())],
            },
        ]
    );
    let stored_id = philo_session::SessionId::new("session");
    assert_eq!(
        block_on(sessions.context_view(&stored_id))
            .unwrap()
            .messages(),
        &[
            ContextMessage::User {
                parts: SessionUserPart::text_parts("one"),
            },
            ContextMessage::Assistant {
                content: "first".to_owned(),
            },
            ContextMessage::User {
                parts: SessionUserPart::text_parts("two"),
            },
            ContextMessage::Assistant {
                content: "second".to_owned(),
            },
        ]
    );
}

#[test]
fn model_failure_is_confirmed_and_durable() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let sessions = Arc::new(MemorySessionStore::new());
    let handle = block_on(
        runtime(model, sessions.clone()).prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            durability: SettlementDurability::Confirmed,
            ..
        }
    ));
    let events = drain(handle);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFailed { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::OperationSettled {
            status: OperationStatus::Failed,
            durability: SettlementDurability::Confirmed,
            ..
        })
    ));
    let stored_id = philo_session::SessionId::new("session");
    assert_eq!(
        block_on(sessions.context_view(&stored_id))
            .unwrap()
            .revision(),
        philo_session::SessionRevision::new(2)
    );
}

#[test]
fn persistent_session_failure_is_unconfirmed() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let handle = block_on(
        runtime(model, Arc::new(FailingStore::after(1)))
            .prompt(SessionId::new("session"), UserMessage::new("hi")),
    )
    .unwrap();
    assert!(matches!(
        block_on(handle.wait()),
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
    let events = drain(handle);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFailed { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::OperationSettled {
            status: OperationStatus::Failed,
            durability: SettlementDurability::Unconfirmed,
            ..
        })
    ));
}

struct FailingStore {
    inner: MemorySessionStore,
    successful_commits_left: Mutex<usize>,
}

impl FailingStore {
    fn always() -> Self {
        Self::after_successful_commits(0)
    }

    fn after(successful_commits: usize) -> Self {
        Self::after_successful_commits(successful_commits)
    }

    fn after_successful_commits(count: usize) -> Self {
        Self {
            inner: MemorySessionStore::new(),
            successful_commits_left: Mutex::new(count),
        }
    }
}

impl SessionStore for FailingStore {
    fn context_view<'a>(
        &'a self,
        session_id: &'a philo_session::SessionId,
    ) -> SessionFuture<'a, Result<SessionContextView, SessionError>> {
        self.inner.context_view(session_id)
    }

    fn commit<'a>(
        &'a self,
        transaction: SessionTransaction,
    ) -> SessionFuture<'a, Result<SessionCommit, SessionError>> {
        Box::pin(async move {
            let should_succeed = {
                let mut commits = self.successful_commits_left.lock().unwrap();
                if *commits > 0 {
                    *commits -= 1;
                    true
                } else {
                    false
                }
            };
            if should_succeed {
                self.inner.commit(transaction).await
            } else {
                Err(SessionError::store_unavailable("scripted commit failure"))
            }
        })
    }
}
