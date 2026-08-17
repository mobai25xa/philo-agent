//! RUNTIME-001: Direct Answer 受理、事件、提交失败

mod support;

use std::sync::{Arc, Mutex};

use philo_agent_runtime::{
    AgentEvent, AgentFailureKind, DEFAULT_MAX_TOOL_ROUNDS, GenerationConfig, ModelAssistantBlock,
    ModelMessage, OperationOutcome, OperationStatus, RuntimeConfig, SettlementDurability, UserPart,
};
use philo_session::{
    ContextMessage, MemorySessionStore, SessionAssistantBlock, SessionCommit, SessionContextView,
    SessionError, SessionFuture, SessionStore, SessionTransaction, SessionUserPart,
};
use support::fake_model::FakeModel;
use support::runtime::{Harness, empty_tools};

fn config() -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "answer directly".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        max_parallel_tool_calls: 1,
        operation_timeout: None,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction: Default::default(),
    }
}

async fn launch(model: Arc<FakeModel>, sessions: Arc<dyn SessionStore>) -> Harness {
    Harness::launch_default(model, sessions, empty_tools(), config()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_answer_success() {
    let model = Arc::new(FakeModel::succeeds(&["hel", "lo"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness = launch(model.clone(), sessions.clone()).await;
    let (events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(
        outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "hello"
    ));
    assert!(matches!(events[0], AgentEvent::OperationStarted { .. }));
    assert!(matches!(events[1], AgentEvent::TurnStarted { .. }));
    assert!(matches!(events[2], AgentEvent::ModelCallStarted { .. }));
    let completed = events
        .iter()
        .position(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
        .expect("assistant completed");
    let settled = events
        .iter()
        .position(|event| matches!(event, AgentEvent::OperationSettled { .. }))
        .expect("settled");
    assert!(completed > 2 && completed < settled);
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello");

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
        sessions.context_view(&stored_id).await.unwrap().messages(),
        &[
            ContextMessage::User {
                parts: SessionUserPart::text_parts("hi"),
            },
            ContextMessage::Assistant {
                blocks: vec![SessionAssistantBlock::Text {
                    text: "hello".to_owned(),
                }],
            },
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_commit_failure_prevents_model_call() {
    let model = Arc::new(FakeModel::succeeds(&["unused"]));
    let sessions = Arc::new(FailingStore::always());
    let mut harness = launch(model.clone(), sessions).await;
    let (_, outcome) = harness.run("session", "hi").await;
    assert!(model.calls().is_empty());
    assert!(matches!(
        outcome,
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_error_settles_failed() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness = launch(model, sessions).await;
    let (_, outcome) = harness.run("session", "hi").await;
    assert!(matches!(
        outcome,
        OperationOutcome::Failed {
            failure,
            durability: SettlementDurability::Confirmed,
        } if failure.kind() == AgentFailureKind::ModelCall
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_commit_failure_prevents_success() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(FailingStore::after_successful_commits(1));
    let mut harness = launch(model, sessions).await;
    let (events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(
        outcome,
        OperationOutcome::Failed {
            failure,
            durability: SettlementDurability::Unconfirmed,
        } if failure.kind() == AgentFailureKind::Persistence
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delta_ordering_preserved() {
    let model = Arc::new(FakeModel::succeeds(&["a", "b", "c"]));
    let mut harness = launch(model, Arc::new(MemorySessionStore::new())).await;
    let (events, _) = harness.run("session", "hi").await;
    let deltas = events
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { delta } => Some(delta),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deltas.concat(), "abc");
    assert!(!deltas.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_event_emitted_once() {
    let model = Arc::new(FakeModel::stream_fails_after(&["partial"], "broken"));
    let mut harness = launch(model, Arc::new(MemorySessionStore::new())).await;
    let (events, _) = harness.run("session", "hi").await;
    let terminal_count = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::OperationSettled { .. }))
        .count();
    assert_eq!(terminal_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_failure_is_durably_settled() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness = launch(model, sessions.clone()).await;
    let (events, _) = harness.run("session", "hi").await;
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
        sessions.context_view(&stored_id).await.unwrap().revision(),
        philo_session::SessionRevision::new(2)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_session_failure_settles_unconfirmed() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let sessions = Arc::new(FailingStore::after_successful_commits(1));
    let mut harness = launch(model, sessions).await;
    let (events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(
        outcome,
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFailed { .. }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_turn_success() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness = launch(model, sessions.clone()).await;
    let (events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(
        outcome,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "hello"
    ));
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello");
    assert!(matches!(
        events.first(),
        Some(AgentEvent::OperationStarted { .. })
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::OperationSettled {
            status: OperationStatus::Succeeded,
            ..
        }
    )));
    let stored_id = philo_session::SessionId::new("session");
    assert_eq!(
        sessions.context_view(&stored_id).await.unwrap().messages(),
        &[
            ContextMessage::User {
                parts: SessionUserPart::text_parts("hi"),
            },
            ContextMessage::Assistant {
                blocks: vec![SessionAssistantBlock::Text {
                    text: "hello".to_owned(),
                }],
            },
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_commit_failure_skips_model() {
    let model = Arc::new(FakeModel::succeeds(&["unused"]));
    let mut harness = launch(model.clone(), Arc::new(FailingStore::after(0))).await;
    let (_, outcome) = harness.run("session", "hi").await;
    assert!(model.calls().is_empty());
    assert!(matches!(
        outcome,
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_commit_failure_never_publishes_success() {
    let model = Arc::new(FakeModel::succeeds(&["hello"]));
    let mut harness = launch(model, Arc::new(FailingStore::after(1))).await;
    let (events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(outcome, OperationOutcome::Failed { .. }));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_turn_contains_first_turn_context() {
    let model = Arc::new(FakeModel::succeeds_sequence(vec![
        vec!["first"],
        vec!["second"],
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness = launch(model.clone(), sessions.clone()).await;
    let (_, first) = harness.run("session", "one").await;
    assert!(matches!(first, OperationOutcome::Succeeded { .. }));
    let (_, second) = harness.run("session", "two").await;
    assert!(matches!(second, OperationOutcome::Succeeded { .. }));

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
                blocks: vec![ModelAssistantBlock::Text {
                    text: "first".to_owned(),
                }],
            },
            ModelMessage::User {
                parts: vec![UserPart::Text("two".to_owned())],
            },
        ]
    );
    let stored_id = philo_session::SessionId::new("session");
    assert_eq!(
        sessions.context_view(&stored_id).await.unwrap().messages(),
        &[
            ContextMessage::User {
                parts: SessionUserPart::text_parts("one"),
            },
            ContextMessage::Assistant {
                blocks: vec![SessionAssistantBlock::Text {
                    text: "first".to_owned(),
                }],
            },
            ContextMessage::User {
                parts: SessionUserPart::text_parts("two"),
            },
            ContextMessage::Assistant {
                blocks: vec![SessionAssistantBlock::Text {
                    text: "second".to_owned(),
                }],
            },
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_failure_is_confirmed_and_durable() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let sessions = Arc::new(MemorySessionStore::new());
    let mut harness = launch(model, sessions.clone()).await;
    let (events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(
        outcome,
        OperationOutcome::Failed {
            durability: SettlementDurability::Confirmed,
            ..
        }
    ));
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
        sessions.context_view(&stored_id).await.unwrap().revision(),
        philo_session::SessionRevision::new(2)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_session_failure_is_unconfirmed() {
    let model = Arc::new(FakeModel::start_fails("offline"));
    let mut harness = launch(model, Arc::new(FailingStore::after(1))).await;
    let (events, outcome) = harness.run("session", "hi").await;
    assert!(matches!(
        outcome,
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
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

    fn list_sessions(
        &self,
    ) -> SessionFuture<'_, Result<Vec<philo_session::SessionId>, SessionError>> {
        self.inner.list_sessions()
    }
}
