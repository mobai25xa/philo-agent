//! RUNTIME-005: cancel(), FIFO queue, AgentAvailability, and the five
//! cancellation injection points.

mod support;

use std::sync::Arc;

use philo_agent_runtime::{
    AgentAvailability, AgentEvent, AgentFailureKind, CancelReason, GenerationConfig, ModelCallId,
    ModelCallPhase, OperationId, OperationOutcome, OperationPhase, OperationStatus,
    RunningToolBatchPhase, RuntimeConfig, SequentialIdSource, SessionId, SettlementDurability,
    SettlementRevision, ToolBatchId, ToolCallId, ToolDefinition, TurnId, UserMessage,
};
use philo_session::{
    ContextMessage, MemorySessionStore, SessionRevision, SessionStore, ToolResultOutcome,
};
use support::failing_session::{FailingSessionStore, FailurePlan};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;
use support::gated_session::GatedSessionStore;

async fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_tool_rounds: u32,
) -> support::runtime::TestRuntime {
    support::runtime::TestRuntime::with_tools(
        model,
        sessions,
        Arc::new(SequentialIdSource::new()),
        RuntimeConfig {
            system_prompt: "sys".to_owned(),
            model_target: "fake".to_owned(),
            generation: GenerationConfig::default(),
            max_tool_rounds,
            max_parallel_tool_calls: 1,
            operation_timeout: None,
            tool_cancel_grace: std::time::Duration::from_millis(300),
            compaction: Default::default(),
        },
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

async fn collect_events(handle: &support::runtime::TestOp) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = handle.next_event().await {
        events.push(event);
    }
    events
}

fn settled_count(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, AgentEvent::OperationSettled { .. }))
        .count()
}

/// Occupies the active slot with a suspended warmup operation so follow-up
/// prompts queue. `prompt()` accepts immediately (M10); the suspension now
/// lives in the driving `wait()` future, which the caller resolves after
/// releasing the gate. The future outputs the warmup's `OperationOutcome`.
macro_rules! suspended_warmup {
    ($runtime:expr, $gate:expr) => {{
        let handle = $runtime.prompt(sid(), UserMessage::new("warmup")).await;
        handle.wait_until_busy().await;
        handle
    }};
}

// --- Injection point 1: cancel while queued (M6-005) -------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_cancel_settles_immediately_with_zero_trace() {
    let warmup_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text(&["third"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime(model, sessions.clone(), no_tools(), 1).await;

    let warmup = suspended_warmup!(agent, warmup_gate);
    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    let third = agent.prompt(sid(), UserMessage::new("third")).await;
    assert_eq!(victim.phase().await, OperationPhase::Queued);
    assert_eq!(third.phase().await, OperationPhase::Queued);

    victim.cancel().await;
    assert_eq!(
        victim.phase().await,
        OperationPhase::Settled(OperationStatus::Cancelled)
    );
    assert!(matches!(victim.wait().await, OperationOutcome::Cancelled));

    warmup_gate.release();
    assert!(matches!(
        warmup.wait().await,
        OperationOutcome::Succeeded { .. }
    ));

    // The queue continues past the removed entry.
    assert!(matches!(
        third.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "third"
    ));

    let victim_id = OperationId::new("operation-2");
    assert_eq!(
        collect_events(&victim).await,
        vec![
            AgentEvent::OperationQueued {
                operation_id: victim_id.clone(),
            },
            AgentEvent::CancellationRequested {
                operation_id: victim_id.clone(),
                reason: CancelReason::User,
            },
            AgentEvent::OperationSettled {
                operation_id: victim_id,
                status: OperationStatus::Cancelled,
                durability: SettlementDurability::Confirmed,
                session_revision: SettlementRevision::Unchanged,
            },
        ]
    );

    // Zero persistent trace: only warmup and third committed anything.
    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
    assert_eq!(view.revision(), philo_session::SessionRevision::new(4));
    assert_eq!(view.messages().len(), 4, "two turns, user+assistant each");
}

// --- Injection point 2: cancel before Barrier A persists ---------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_before_barrier_a_leaves_zero_trace() {
    let warmup_gate = Gate::new();
    let read_gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::text_suspending(
        &[],
        &warmup_gate,
        &["done"],
    )]));
    let sessions = Arc::new(GatedSessionStore::memory().gate_context_read_at(2, &read_gate));
    let agent = runtime(model, sessions.clone(), no_tools(), 1).await;

    let warmup = suspended_warmup!(agent, warmup_gate);
    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    warmup_gate.release();
    let _ = warmup.wait().await;

    victim
        .wait_until_phase(|phase| matches!(phase, OperationPhase::PreparingTurn))
        .await;
    assert_eq!(victim.phase().await, OperationPhase::PreparingTurn);
    victim.cancel().await;
    read_gate.release();
    assert!(matches!(victim.wait().await, OperationOutcome::Cancelled));

    let victim_id = OperationId::new("operation-2");
    assert_eq!(
        collect_events(&victim).await,
        vec![
            AgentEvent::OperationQueued {
                operation_id: victim_id.clone(),
            },
            AgentEvent::OperationStarted {
                operation_id: victim_id.clone(),
            },
            AgentEvent::TurnStarted {
                turn_id: TurnId::new("turn-2"),
            },
            AgentEvent::CancellationRequested {
                operation_id: victim_id.clone(),
                reason: CancelReason::User,
            },
            AgentEvent::OperationSettled {
                operation_id: victim_id,
                status: OperationStatus::Cancelled,
                durability: SettlementDurability::Confirmed,
                session_revision: SettlementRevision::Unchanged,
            },
        ],
        "no TurnCancelled: durably no turn ever existed"
    );

    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
    assert_eq!(
        view.revision(),
        philo_session::SessionRevision::new(2),
        "only the warmup turn committed"
    );
}

// --- Injection point 3: cancel during the model stream (M6-001) --------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_model_stream_discards_output_and_commits_terminal_facts() {
    let warmup_gate = Gate::new();
    let victim_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text_suspending(&["par"], &victim_gate, &["tial"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime(model, sessions.clone(), no_tools(), 1).await;

    let warmup = suspended_warmup!(agent, warmup_gate);
    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    warmup_gate.release();
    let _ = warmup.wait().await;

    victim
        .wait_until_phase(|phase| {
            matches!(
                phase,
                OperationPhase::RunningModelCall(ModelCallPhase::Streaming)
            )
        })
        .await;
    assert_eq!(
        victim.phase().await,
        OperationPhase::RunningModelCall(ModelCallPhase::Streaming)
    );
    victim.cancel().await;
    // The victim gate is never released: the stream is dropped instead.
    assert!(matches!(victim.wait().await, OperationOutcome::Cancelled));

    let victim_id = OperationId::new("operation-2");
    let events = collect_events(&victim).await;
    assert_eq!(
        events,
        vec![
            AgentEvent::OperationQueued {
                operation_id: victim_id.clone(),
            },
            AgentEvent::OperationStarted {
                operation_id: victim_id.clone(),
            },
            AgentEvent::TurnStarted {
                turn_id: TurnId::new("turn-2"),
            },
            AgentEvent::ModelCallStarted {
                model_call_id: ModelCallId::new("turn-2:model-call:1"),
            },
            AgentEvent::TextDelta {
                delta: "par".to_owned(),
            },
            AgentEvent::CancellationRequested {
                operation_id: victim_id.clone(),
                reason: CancelReason::User,
            },
            AgentEvent::TurnCancelled {
                turn_id: TurnId::new("turn-2"),
                reason: CancelReason::User,
            },
            AgentEvent::OperationSettled {
                operation_id: victim_id,
                status: OperationStatus::Cancelled,
                durability: SettlementDurability::Confirmed,
                session_revision: SettlementRevision::Committed(SessionRevision::new(4)),
            },
        ]
    );
    assert_eq!(settled_count(&events), 1);

    // No AssistantMessage; the cancellation transaction advanced the session.
    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
    assert_eq!(view.revision(), philo_session::SessionRevision::new(4));
    assert!(matches!(
        view.messages().last(),
        Some(ContextMessage::User { parts })
            if parts == &philo_session::SessionUserPart::text_parts("victim")
    ));
}

// --- Injection point 3 variant: between rounds after C_k (M6-003) ------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_between_rounds_commits_terminal_entries_only() {
    let warmup_gate = Gate::new();
    let commit_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
    ]));
    // Warmup: commits 1..2. Victim: A=3, batch=4, results=5 (gated).
    let sessions = Arc::new(GatedSessionStore::memory().gate_commit_at(5, &commit_gate));
    let tools = Arc::new(FakeTool::new([echo()], [FakeToolResult::success("one")]));
    let agent = runtime(model.clone(), sessions.clone(), tools, 2).await;

    let warmup = suspended_warmup!(agent, warmup_gate);
    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    warmup_gate.release();
    let _ = warmup.wait().await;

    victim
        .wait_until_phase(|phase| {
            matches!(
                phase,
                OperationPhase::RunningToolBatch(RunningToolBatchPhase::CommittingResults)
            )
        })
        .await;
    victim.cancel().await;
    commit_gate.release();
    assert!(matches!(victim.wait().await, OperationOutcome::Cancelled));

    // C_k succeeded, so the executed call keeps its real persisted result and
    // the cancellation transaction carries only the two terminal entries.
    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
    let result_messages: Vec<_> = view
        .messages()
        .iter()
        .filter_map(|message| match message {
            ContextMessage::ToolResult { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(result_messages.len(), 1, "no Cancelled completion marks");
    assert!(matches!(
        result_messages[0],
        ToolResultOutcome::Success { content } if content == "one"
    ));

    let events = collect_events(&victim).await;
    let completed_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolExecutionCompleted { .. }))
        .expect("executed call completes");
    let requested_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::CancellationRequested { .. }))
        .expect("cancellation accepted");
    assert!(
        completed_index < requested_index,
        "cancel was observed after the round completed"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCancelled { .. }))
    );
    assert_eq!(settled_count(&events), 1);
    assert_eq!(model.call_count(), 2, "round-2 model call never starts");
}

// --- Injection point 4: cancel mid tool batch (M6-002) -----------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_mid_batch_completes_the_executing_call_and_marks_the_rest() {
    let warmup_gate = Gate::new();
    let tool_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::tool_calls(&[(0, "call-1", "echo", "{}"), (1, "call-2", "echo", "{}")]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [
            FakeToolResult::gated_success(&tool_gate, "one"),
            FakeToolResult::success("never"),
        ],
    ));
    let agent = runtime(model, sessions.clone(), tools.clone(), 1).await;

    let warmup = suspended_warmup!(agent, warmup_gate);
    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    warmup_gate.release();
    let _ = warmup.wait().await;

    victim
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
    assert_eq!(
        victim.phase().await,
        OperationPhase::RunningToolBatch(RunningToolBatchPhase::Executing {
            in_flight: 1,
            completed: 0,
        })
    );
    victim.cancel().await;
    tool_gate.release();
    assert!(matches!(victim.wait().await, OperationOutcome::Cancelled));

    assert_eq!(tools.invocation_count(), 1, "call-2 never executes");

    let victim_id = OperationId::new("operation-2");
    let batch_id = ToolBatchId::new("turn-2:tool-batch:1");
    let events = collect_events(&victim).await;
    assert_eq!(
        events,
        vec![
            AgentEvent::OperationQueued {
                operation_id: victim_id.clone(),
            },
            AgentEvent::OperationStarted {
                operation_id: victim_id.clone(),
            },
            AgentEvent::TurnStarted {
                turn_id: TurnId::new("turn-2"),
            },
            AgentEvent::ModelCallStarted {
                model_call_id: ModelCallId::new("turn-2:model-call:1"),
            },
            AgentEvent::ToolBatchRequested {
                tool_batch_id: batch_id.clone(),
                call_count: 2,
            },
            AgentEvent::ToolExecutionStarted {
                tool_batch_id: batch_id.clone(),
                tool_call_id: ToolCallId::new("call-1"),
                index: 0,
                tool_name: "echo".to_owned(),
                arguments: "{}".to_owned(),
            },
            AgentEvent::CancellationRequested {
                operation_id: victim_id.clone(),
                reason: CancelReason::User,
            },
            AgentEvent::ToolExecutionCompleted {
                tool_batch_id: batch_id,
                tool_call_id: ToolCallId::new("call-1"),
                index: 0,
                tool_name: "echo".to_owned(),
                result: philo_agent_runtime::ToolResult::success("one"),
                display: None,
            },
            AgentEvent::TurnCancelled {
                turn_id: TurnId::new("turn-2"),
                reason: CancelReason::User,
            },
            AgentEvent::OperationSettled {
                operation_id: victim_id,
                status: OperationStatus::Cancelled,
                durability: SettlementDurability::Confirmed,
                session_revision: SettlementRevision::Committed(SessionRevision::new(5)),
            },
        ]
    );

    // Real prefix + Cancelled suffix persisted atomically with the terminals.
    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
    assert_eq!(view.revision(), philo_session::SessionRevision::new(5));
    let outcomes: Vec<_> = view
        .messages()
        .iter()
        .filter_map(|message| match message {
            ContextMessage::ToolResult {
                tool_call_id,
                outcome,
            } => Some((tool_call_id.as_str(), outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes.len(), 2);
    assert!(matches!(
        outcomes[0],
        ("call-1", ToolResultOutcome::Success { content }) if content == "one"
    ));
    assert!(matches!(
        outcomes[1],
        ("call-2", ToolResultOutcome::Cancelled)
    ));
}

// --- Injection point 4 variant: zero executed calls (M6-003) ------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_after_batch_commit_marks_every_call_cancelled() {
    let warmup_gate = Gate::new();
    let commit_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::tool_calls(&[(0, "call-1", "echo", "{}"), (1, "call-2", "echo", "{}")]),
    ]));
    // Warmup: commits 1..2. Victim: A=3, batch commit B_k=4 (gated).
    let sessions = Arc::new(GatedSessionStore::memory().gate_commit_at(4, &commit_gate));
    let tools = Arc::new(FakeTool::new([echo()], []));
    let agent = runtime(model, sessions.clone(), tools.clone(), 1).await;

    let warmup = suspended_warmup!(agent, warmup_gate);
    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    warmup_gate.release();
    let _ = warmup.wait().await;

    victim
        .wait_until_phase(|phase| {
            matches!(
                phase,
                OperationPhase::RunningToolBatch(RunningToolBatchPhase::Preparing)
            )
        })
        .await;
    victim.cancel().await;
    commit_gate.release();
    assert!(matches!(victim.wait().await, OperationOutcome::Cancelled));

    assert_eq!(tools.invocation_count(), 0, "no call ever executes");

    let events = collect_events(&victim).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolExecutionStarted { .. })),
        "zero execution events"
    );
    assert_eq!(settled_count(&events), 1);

    let view = sessions
        .context_view(&philo_session::SessionId::new("s"))
        .await
        .unwrap();
    let outcomes: Vec<_> = view
        .messages()
        .iter()
        .filter_map(|message| match message {
            ContextMessage::ToolResult { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes.len(), 2, "the whole batch is marked");
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, ToolResultOutcome::Cancelled))
    );
}

// --- Injection point 5: cancel after the kernel's final decision (M6-006) ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_after_final_decision_is_a_no_op() {
    let warmup_gate = Gate::new();
    let commit_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text(&["fin"]),
    ]));
    // Warmup: commits 1..2. Victim: A=3, final settlement=4 (gated).
    let sessions = Arc::new(GatedSessionStore::memory().gate_commit_at(4, &commit_gate));
    let agent = runtime(model, sessions.clone(), no_tools(), 1).await;

    let warmup = suspended_warmup!(agent, warmup_gate);
    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    warmup_gate.release();
    let _ = warmup.wait().await;

    victim
        .wait_until_phase(|phase| matches!(phase, OperationPhase::Finalizing))
        .await;
    assert_eq!(victim.phase().await, OperationPhase::Finalizing);
    victim.cancel().await;
    commit_gate.release();
    assert!(matches!(
        victim.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "fin"
    ));

    let events = collect_events(&victim).await;
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::CancellationRequested { .. } | AgentEvent::TurnCancelled { .. }
        )),
        "an ineffective cancel publishes nothing"
    );
    assert!(matches!(
        sessions.context_view(&philo_session::SessionId::new("s")).await
            .unwrap()
            .messages()
            .last(),
        Some(ContextMessage::Assistant { blocks })
            if matches!(
                blocks.as_slice(),
                [philo_session::SessionAssistantBlock::Text { text }] if text == "fin"
            )
    ));
}

// --- Idempotency (M6-009) ------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_is_idempotent_and_late_cancels_are_no_ops() {
    let warmup_gate = Gate::new();
    let victim_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text_suspending(&["x"], &victim_gate, &["y"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime(model, sessions, no_tools(), 1).await;

    // Keep the warmup handle borrowable: the settled no-op check needs it.
    let warmup = agent.prompt(sid(), UserMessage::new("warmup")).await;
    warmup.wait_until_busy().await;
    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    warmup_gate.release();
    assert!(matches!(
        warmup.wait().await,
        OperationOutcome::Succeeded { .. }
    ));

    // Cancelling a settled (succeeded) operation is a no-op.
    warmup.cancel().await;
    assert!(matches!(
        warmup.wait().await,
        OperationOutcome::Succeeded { .. }
    ));

    victim
        .wait_until_phase(|phase| {
            matches!(
                phase,
                OperationPhase::RunningModelCall(ModelCallPhase::Streaming)
            )
        })
        .await;
    victim.cancel().await;
    victim.cancel().await;
    victim.cancel().await;
    assert!(matches!(victim.wait().await, OperationOutcome::Cancelled));

    // Cancel after settlement: no state change, no extra events.
    victim.cancel().await;
    assert_eq!(
        victim.phase().await,
        OperationPhase::Settled(OperationStatus::Cancelled)
    );
    let events = collect_events(&victim).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::CancellationRequested { .. }))
            .count(),
        1,
        "CancellationRequested at most once per operation"
    );
    assert_eq!(settled_count(&events), 1);
}

// --- Cancel commit failure takes the failure path (M6-007) --------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_commit_failure_settles_failed_confirmed() {
    let warmup_gate = Gate::new();
    let victim_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text_suspending(&["x"], &victim_gate, &["y"]),
    ]));
    // Warmup: commits 1..2. Victim: A=3, cancel transaction=4 fails once,
    // failure settlement=5 succeeds.
    let sessions = Arc::new(FailingSessionStore::memory(FailurePlan::commit_at(4)));
    let agent = runtime(model, sessions, no_tools(), 1).await;

    let warmup = suspended_warmup!(agent, warmup_gate);
    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    warmup_gate.release();
    let _ = warmup.wait().await;

    victim
        .wait_until_phase(|phase| {
            matches!(
                phase,
                OperationPhase::RunningModelCall(ModelCallPhase::Streaming)
            )
        })
        .await;
    victim.cancel().await;
    let outcome = victim.wait().await;
    let OperationOutcome::Failed {
        failure,
        durability,
    } = outcome
    else {
        panic!("cancel commit failure must settle Failed, got {outcome:?}");
    };
    assert_eq!(failure.kind(), AgentFailureKind::Persistence);
    assert!(failure.message().contains("committing cancellation"));
    assert_eq!(durability, SettlementDurability::Confirmed);

    let events = collect_events(&victim).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFailed { .. })),
        "the failure path settles durably"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCancelled { .. })),
        "never report Cancelled without the durable fact"
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        AgentEvent::OperationSettled {
            status: OperationStatus::Cancelled,
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_commit_persistent_failure_settles_unconfirmed() {
    let warmup_gate = Gate::new();
    let victim_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text_suspending(&["x"], &victim_gate, &["y"]),
    ]));
    // Every commit from the cancel transaction on fails.
    let sessions = Arc::new(FailingSessionStore::memory(
        FailurePlan::persistent_commit_at(4),
    ));
    let agent = runtime(model, sessions, no_tools(), 1).await;

    let warmup = suspended_warmup!(agent, warmup_gate);
    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    warmup_gate.release();
    let _ = warmup.wait().await;

    victim
        .wait_until_phase(|phase| {
            matches!(
                phase,
                OperationPhase::RunningModelCall(ModelCallPhase::Streaming)
            )
        })
        .await;
    victim.cancel().await;
    assert!(matches!(
        victim.wait().await,
        OperationOutcome::Failed {
            failure,
            durability: SettlementDurability::Unconfirmed,
        } if failure.kind() == AgentFailureKind::Persistence
    ));
}

// --- FIFO queue and start timing (M6-004) -------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_is_fifo_and_operations_start_at_dequeue() {
    let warmup_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text(&["alpha"]),
        ModelScript::text(&["beta"]),
    ]));
    let agent = runtime(
        model.clone(),
        Arc::new(MemorySessionStore::new()),
        no_tools(),
        1,
    )
    .await;

    let warmup = suspended_warmup!(agent, warmup_gate);
    let first = agent.prompt(sid(), UserMessage::new("first")).await;
    let second = agent.prompt(sid(), UserMessage::new("second")).await;
    assert_eq!(first.phase().await, OperationPhase::Queued);
    assert_eq!(second.phase().await, OperationPhase::Queued);
    assert_eq!(
        second.phase().await,
        OperationPhase::Queued,
        "no queue jumping"
    );

    warmup_gate.release();
    let _ = warmup.wait().await;

    // Scripts are consumed in FIFO order: first gets "alpha", second "beta".
    assert!(matches!(
        first.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "alpha"
    ));
    assert!(matches!(
        second.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "beta"
    ));

    // OperationStarted publishes at dequeue: it follows OperationQueued.
    let events = collect_events(&first).await;
    assert!(matches!(events[0], AgentEvent::OperationQueued { .. }));
    assert!(matches!(events[1], AgentEvent::OperationStarted { .. }));
}

// --- AgentAvailability (M6-004) -------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn availability_reflects_the_actively_driven_operation() {
    let warmup_gate = Gate::new();
    let victim_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text_suspending(&[], &victim_gate, &["late"]),
    ]));
    let agent = runtime(model, Arc::new(MemorySessionStore::new()), no_tools(), 1).await;

    assert_eq!(agent.availability().await, AgentAvailability::Idle);

    let warmup = suspended_warmup!(agent, warmup_gate);
    assert_eq!(
        agent.availability().await,
        AgentAvailability::Busy {
            operation_id: OperationId::new("operation-1"),
        }
    );

    let follow_up = agent.prompt(sid(), UserMessage::new("follow")).await;
    assert_eq!(
        agent.availability().await,
        AgentAvailability::Busy {
            operation_id: OperationId::new("operation-1"),
        },
        "queued operations do not change the active observation"
    );

    warmup_gate.release();
    let _ = warmup.wait().await;
    follow_up.wait_until_busy().await;
    assert_eq!(
        agent.availability().await,
        AgentAvailability::Busy {
            operation_id: OperationId::new("operation-2"),
        }
    );

    victim_gate.release();
    assert!(matches!(
        follow_up.wait().await,
        OperationOutcome::Succeeded { .. }
    ));
    assert_eq!(agent.availability().await, AgentAvailability::Idle);
}

// --- Cancelled results replay into later turns ---------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_marks_replay_into_the_next_turn_context() {
    let warmup_gate = Gate::new();
    let tool_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::tool_calls(&[(0, "call-1", "echo", "{}"), (1, "call-2", "echo", "{}")]),
        ModelScript::text(&["next"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let tools = Arc::new(FakeTool::new(
        [echo()],
        [FakeToolResult::gated_success(&tool_gate, "one")],
    ));
    let agent = runtime(model.clone(), sessions, tools, 1).await;

    let warmup = suspended_warmup!(agent, warmup_gate);
    let victim = agent.prompt(sid(), UserMessage::new("victim")).await;
    warmup_gate.release();
    let _ = warmup.wait().await;

    victim
        .wait_until_phase(|phase| {
            matches!(
                phase,
                OperationPhase::RunningToolBatch(RunningToolBatchPhase::Executing { .. })
            )
        })
        .await;
    victim.cancel().await;
    tool_gate.release();
    assert!(matches!(victim.wait().await, OperationOutcome::Cancelled));

    // The next turn's model call replays the cancelled turn's partial tool
    // trajectory, including the Cancelled outcome mirror.
    let next = agent.prompt(sid(), UserMessage::new("continue")).await;
    assert!(matches!(
        next.wait().await,
        OperationOutcome::Succeeded { assistant } if assistant.content() == "next"
    ));
    let calls = model.calls();
    let final_messages = &calls.last().expect("three calls").messages;
    let cancelled_mirrors = final_messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                philo_agent_runtime::ModelMessage::ToolResult {
                    outcome: philo_agent_runtime::ModelToolResultOutcome::Cancelled,
                    ..
                }
            )
        })
        .count();
    assert_eq!(cancelled_mirrors, 1, "call-2's Cancelled mark is replayed");
}
