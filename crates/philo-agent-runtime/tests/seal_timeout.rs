//! RUNTIME-009: the seal protocol for stale unfinished turns, `Interrupted`
//! placeholders for dangling batches of terminated turns, the operation
//! timeout, and reasoned cancellation events.

mod support;

use std::sync::Arc;
use std::time::Duration;

use philo_agent_runtime::{
    AgentEvent, CancelReason, GenerationConfig, ModelMessage, ModelToolResultOutcome,
    OperationOutcome, OperationPhase, OperationStatus, RunningToolBatchPhase, RuntimeConfig,
    SequentialIdSource, SessionId, SettlementDurability, ToolDefinition, TurnId, UserMessage,
};
use philo_session::{
    ContextMessage, MemorySessionStore, SessionAssistantBlock, SessionEntryKind, SessionRevision,
    SessionStore, SessionToolCall, SessionTransaction, SessionUserPart, ToolResultOutcome,
};
use support::failing_session::{FailingSessionStore, FailurePlan};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;

fn config(max_tool_rounds: u32, operation_timeout: Option<Duration>) -> RuntimeConfig {
    RuntimeConfig {
        system_prompt: "sys".to_owned(),
        model_target: "fake".to_owned(),
        generation: GenerationConfig::default(),
        max_tool_rounds,
        max_parallel_tool_calls: 1,
        operation_timeout,
        tool_cancel_grace: std::time::Duration::from_millis(300),
        compaction: Default::default(),
        recovery: Default::default(),
    }
}

async fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_tool_rounds: u32,
    operation_timeout: Option<Duration>,
) -> support::runtime::TestRuntime {
    support::runtime::TestRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        config(max_tool_rounds, operation_timeout),
        tools,
    )
    .await
}

fn no_tools() -> Arc<FakeTool> {
    Arc::new(FakeTool::new([], []))
}

fn echo() -> ToolDefinition {
    ToolDefinition::simple("echo", "echo", philo_agent_runtime::EffectClass::ReadOnly)
}

fn sid() -> SessionId {
    SessionId::new("s")
}

fn stored_sid() -> philo_session::SessionId {
    philo_session::SessionId::new("s")
}

async fn collect_events(handle: &support::runtime::TestOp) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = handle.next_event().await {
        events.push(event);
    }
    events
}

/// Persists the start facts of a turn that never terminated (a crash
/// remnant): OperationStarted + TurnStarted + UserMessage.
async fn commit_remnant_start(store: &dyn SessionStore, revision: u64, suffix: &str) {
    store
        .commit(SessionTransaction::linear(
            stored_sid(),
            SessionRevision::new(revision),
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: philo_session::OperationId::new(format!("stale-op-{suffix}")),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: philo_session::OperationId::new(format!("stale-op-{suffix}")),
                    turn_id: philo_session::TurnId::new(format!("stale-turn-{suffix}")),
                },
                SessionEntryKind::UserMessage {
                    turn_id: philo_session::TurnId::new(format!("stale-turn-{suffix}")),
                    parts: SessionUserPart::text_parts("stranded prompt"),
                },
            ],
        ))
        .await
        .expect("remnant start commits");
}

/// Persists a two-call batch for the given stale turn (B_k durable, no
/// results, no terminal facts — the crash window between B_k and C_k).
async fn commit_remnant_batch(store: &dyn SessionStore, revision: u64, suffix: &str) {
    store
        .commit(SessionTransaction::linear(
            stored_sid(),
            SessionRevision::new(revision),
            vec![SessionEntryKind::AssistantToolCallBatch {
                turn_id: philo_session::TurnId::new(format!("stale-turn-{suffix}")),
                model_call_id: "stale-model-call".to_owned(),
                tool_batch_id: philo_session::ToolBatchId::new(format!("stale-batch-{suffix}")),
                blocks: vec![
                    SessionAssistantBlock::ToolCall(SessionToolCall::new(
                        philo_session::ToolCallId::new(format!("stale-call-{suffix}-1")),
                        "write",
                        r#"{"path":"a.txt"}"#,
                    )),
                    SessionAssistantBlock::ToolCall(SessionToolCall::new(
                        philo_session::ToolCallId::new(format!("stale-call-{suffix}-2")),
                        "shell",
                        r#"{"command":"ls"}"#,
                    )),
                ],
            }],
        ))
        .await
        .expect("remnant batch commits");
}

fn interrupted_count(messages: &[ContextMessage]) -> usize {
    messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ContextMessage::ToolResult {
                    outcome: ToolResultOutcome::Interrupted,
                    ..
                }
            )
        })
        .count()
}

// ---------------------------------------------------------------- seal 端到端

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seal_completes_the_stranded_batch_before_the_new_turn() {
    let sessions = Arc::new(MemorySessionStore::new());
    commit_remnant_start(sessions.as_ref(), 0, "a").await;
    commit_remnant_batch(sessions.as_ref(), 1, "a").await;

    let model = Arc::new(FakeModel::succeeds(&["done"]));
    let agent = runtime(model.clone(), sessions.clone(), no_tools(), 0, None).await;
    let handle = agent.prompt(sid(), UserMessage::new("continue")).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "done"
    ));

    // Event order: PriorTurnSealed sits after OperationStarted, before
    // TurnStarted, and does not use the cancellation event channel.
    let events = collect_events(&handle).await;
    let started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::OperationStarted { .. }))
        .expect("operation started");
    let sealed = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::PriorTurnSealed { turn_id } if turn_id == &TurnId::new("stale-turn-a")
            )
        })
        .expect("prior turn sealed");
    let turn_started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TurnStarted { .. }))
        .expect("turn started");
    assert!(started < sealed && sealed < turn_started);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::CancellationRequested { .. } | AgentEvent::TurnCancelled { .. }
        )),
        "sealing never uses the cancellation event channel"
    );

    // Durable: the batch completed with Interrupted marks and the session
    // has no open turns left.
    let view = sessions.context_view(&stored_sid()).await.expect("view");
    assert!(view.open_turns().is_empty());
    assert_eq!(interrupted_count(view.messages()), 2);

    // The model request carries fully paired tool messages.
    let request = &model.calls()[0];
    let interrupted_in_request = request
        .messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ModelMessage::ToolResult {
                    outcome: ModelToolResultOutcome::Interrupted,
                    ..
                }
            )
        })
        .count();
    assert_eq!(interrupted_in_request, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seal_without_a_stranded_batch_commits_terminal_facts_only() {
    let sessions = Arc::new(MemorySessionStore::new());
    commit_remnant_start(sessions.as_ref(), 0, "a").await;

    let model = Arc::new(FakeModel::succeeds(&["done"]));
    let agent = runtime(model, sessions.clone(), no_tools(), 0, None).await;
    let handle = agent.prompt(sid(), UserMessage::new("continue")).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));

    let view = sessions.context_view(&stored_sid()).await.expect("view");
    assert!(view.open_turns().is_empty());
    assert_eq!(interrupted_count(view.messages()), 0, "no completion marks");
    // remnant(1) + seal(1) + new turn start(1) + settlement(1)
    assert_eq!(view.revision(), SessionRevision::new(4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_remnants_seal_in_source_order_with_independent_transactions() {
    let sessions = Arc::new(MemorySessionStore::new());
    commit_remnant_start(sessions.as_ref(), 0, "a").await;
    commit_remnant_start(sessions.as_ref(), 1, "b").await;
    commit_remnant_batch(sessions.as_ref(), 2, "b").await;

    let model = Arc::new(FakeModel::succeeds(&["done"]));
    let agent = runtime(model, sessions.clone(), no_tools(), 0, None).await;
    let handle = agent.prompt(sid(), UserMessage::new("continue")).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));

    let events = collect_events(&handle).await;
    let sealed: Vec<&TurnId> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::PriorTurnSealed { turn_id } => Some(turn_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        sealed,
        [&TurnId::new("stale-turn-a"), &TurnId::new("stale-turn-b")],
        "seals follow source order"
    );

    let view = sessions.context_view(&stored_sid()).await.expect("view");
    assert!(view.open_turns().is_empty());
    // remnants(3) + two independent seals(2) + new turn(2)
    assert_eq!(view.revision(), SessionRevision::new(7));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seal_commit_failure_fails_the_operation_and_retries_idempotently() {
    let memory = Arc::new(MemorySessionStore::new());
    commit_remnant_start(memory.as_ref(), 0, "a").await;
    commit_remnant_batch(memory.as_ref(), 1, "a").await;

    // The first commit the runtime issues is the seal transaction: fail it.
    let failing = Arc::new(FailingSessionStore::around(
        memory.clone(),
        FailurePlan::commit_at(1),
    ));
    let model = Arc::new(FakeModel::succeeds(&["done"]));
    let agent = runtime(model, failing, no_tools(), 0, None).await;
    let handle = agent.prompt(sid(), UserMessage::new("continue")).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Failed {
            durability: SettlementDurability::Unconfirmed,
            ..
        }
    ));
    let events = collect_events(&handle).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnStarted { .. })),
        "the new turn never starts when sealing fails"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PriorTurnSealed { .. })),
        "no seal notification without a committed seal"
    );

    // The remnant is still there; the next prompt reseals and proceeds.
    let view = memory.context_view(&stored_sid()).await.expect("view");
    assert_eq!(view.open_turns().len(), 1);

    let model = Arc::new(FakeModel::succeeds(&["done"]));
    let agent = runtime(model, memory.clone(), no_tools(), 0, None).await;
    let handle = agent.prompt(sid(), UserMessage::new("continue")).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));
    let view = memory.context_view(&stored_sid()).await.expect("view");
    assert!(view.open_turns().is_empty(), "idempotent progress");
}

// ------------------------------------------------------------ 占位映射（已终态）

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dangling_batch_of_a_terminated_turn_maps_to_placeholders_without_durable_change() {
    let sessions = Arc::new(MemorySessionStore::new());
    commit_remnant_start(sessions.as_ref(), 0, "a").await;
    commit_remnant_batch(sessions.as_ref(), 1, "a").await;
    // Terminate the turn the M6 failure way: the failure transaction never
    // completes the batch.
    sessions
        .commit(SessionTransaction::linear(
            stored_sid(),
            SessionRevision::new(2),
            vec![
                SessionEntryKind::TurnFailure {
                    turn_id: philo_session::TurnId::new("stale-turn-a"),
                    failure: philo_session::TurnFailure::new(
                        philo_session::TurnFailureKind::ModelCall,
                        "provider offline",
                    ),
                },
                SessionEntryKind::TurnTerminated {
                    turn_id: philo_session::TurnId::new("stale-turn-a"),
                    outcome: philo_session::TurnOutcome::Failed,
                },
                SessionEntryKind::OperationSettled {
                    operation_id: philo_session::OperationId::new("stale-op-a"),
                    outcome: philo_session::OperationOutcome::Failed,
                    usage: None,
                    generation: None,
                },
            ],
        ))
        .await
        .expect("failure transaction commits");

    let model = Arc::new(FakeModel::succeeds(&["done"]));
    let agent = runtime(model.clone(), sessions.clone(), no_tools(), 0, None).await;
    let handle = agent.prompt(sid(), UserMessage::new("continue")).await;
    assert!(matches!(
        handle.wait().await,
        OperationOutcome::Succeeded { .. }
    ));

    // No seal happened: the turn was already terminated.
    let events = collect_events(&handle).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PriorTurnSealed { .. })),
        "terminated turns are never sealed"
    );

    // The request carries synthesized placeholders for the dangling calls.
    let request = &model.calls()[0];
    let placeholders = request
        .messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ModelMessage::ToolResult {
                    outcome: ModelToolResultOutcome::Interrupted,
                    ..
                }
            )
        })
        .count();
    assert_eq!(placeholders, 2);

    // Durable facts did not change: the batch still has zero results.
    let view = sessions.context_view(&stored_sid()).await.expect("view");
    assert_eq!(interrupted_count(view.messages()), 0);
    // remnants(2) + failure(1) + new turn(2): placeholders added nothing.
    assert_eq!(view.revision(), SessionRevision::new(5));
}

// ---------------------------------------------------------------- 超时取消

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operation_timeout_cancels_mid_batch_with_reason_timeout() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[
        (0, "call-1", "echo", "{}"),
        (1, "call-2", "echo", "{}"),
    ])]));
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [
            FakeToolResult::gated_success(&gate, "one"),
            FakeToolResult::success("never"),
        ],
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime(
        model.clone(),
        sessions.clone(),
        tools.clone(),
        2,
        Some(Duration::from_millis(50)),
    )
    .await;

    let handle = agent.prompt(sid(), UserMessage::new("go")).await;
    handle
        .wait_until_phase(|phase| {
            matches!(
                phase,
                OperationPhase::RunningToolBatch(RunningToolBatchPhase::Executing {
                    in_flight: 1,
                    completed: 0,
                })
            )
        })
        .await;
    // Let the deadline expire while the call is executing, then let the
    // call return: the injection point picks the timeout up.
    tokio::time::sleep(Duration::from_millis(80)).await;
    gate.release();
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));

    let events = collect_events(&handle).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::CancellationRequested {
            reason: CancelReason::Timeout,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnCancelled {
            reason: CancelReason::Timeout,
            ..
        }
    )));
    // The executing call ran to completion and kept its real result.
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionCompleted { result, .. }
            if result == &philo_agent_runtime::ToolResult::success("one")
    )));
    assert_eq!(tools.invocation_count(), 1, "call-2 never starts");
    assert_eq!(model.call_count(), 1, "round-2 model call never starts");

    // Durable: real prefix + Cancelled suffix, and the session continues.
    let view = sessions.context_view(&stored_sid()).await.expect("view");
    assert!(view.open_turns().is_empty());
    assert!(view.messages().iter().any(|message| matches!(
        message,
        ContextMessage::ToolResult {
            outcome: ToolResultOutcome::Cancelled,
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_cancel_wins_the_race_and_keeps_reason_user() {
    let gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::tool_calls(&[(
        0, "call-1", "echo", "{}",
    )])]));
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [FakeToolResult::gated_success(&gate, "one")],
    ));
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime(model, sessions, tools, 1, Some(Duration::from_millis(50))).await;

    let handle = agent.prompt(sid(), UserMessage::new("go")).await;
    handle
        .wait_until_phase(|phase| {
            matches!(
                phase,
                OperationPhase::RunningToolBatch(RunningToolBatchPhase::Executing {
                    in_flight: 1,
                    completed: 0,
                })
            )
        })
        .await;
    // The user cancel arrives first; the timeout expires later but the
    // first accepted reason wins.
    handle.cancel().await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    gate.release();
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));

    let events = collect_events(&handle).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::CancellationRequested {
            reason: CancelReason::User,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnCancelled {
            reason: CancelReason::User,
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_before_barrier_a_cancels_with_zero_trace() {
    let sessions = Arc::new(MemorySessionStore::new());
    let model = Arc::new(FakeModel::succeeds(&["never"]));
    let agent = runtime(
        model.clone(),
        sessions.clone(),
        no_tools(),
        0,
        Some(Duration::ZERO),
    )
    .await;

    let handle = agent.prompt(sid(), UserMessage::new("go")).await;
    assert!(matches!(handle.wait().await, OperationOutcome::Cancelled));

    let events = collect_events(&handle).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::CancellationRequested {
            reason: CancelReason::Timeout,
            ..
        }
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCancelled { .. })),
        "zero-trace cancellation has no durable turn to cancel"
    );
    assert_eq!(model.call_count(), 0);
    let view = sessions.context_view(&stored_sid()).await.expect("view");
    assert_eq!(
        view.revision(),
        SessionRevision::ZERO,
        "zero persistent trace"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_cancel_publishes_reason_user() {
    let warmup_gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::text_suspending(
        &[],
        &warmup_gate,
        &["done"],
    )]));
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime(model, sessions, no_tools(), 0, None).await;

    let warmup = agent.prompt(sid(), UserMessage::new("warmup")).await;
    warmup.wait_until_busy().await;

    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    victim
        .wait_until_phase(|phase| matches!(phase, OperationPhase::Queued))
        .await;
    assert_eq!(victim.phase().await, OperationPhase::Queued);
    victim.cancel().await;
    assert_eq!(
        victim.phase().await,
        OperationPhase::Settled(OperationStatus::Cancelled)
    );
    let events = collect_events(&victim).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::CancellationRequested {
            reason: CancelReason::User,
            ..
        }
    )));

    warmup_gate.release();
    let _ = warmup.wait().await;
}
