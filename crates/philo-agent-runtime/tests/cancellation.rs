//! RUNTIME-005: cancel(), FIFO queue, AgentAvailability, and the five
//! cancellation injection points.

mod support;

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use philo_agent_runtime::{
    AgentAvailability, AgentEvent, AgentFailureKind, AgentRuntime, CancelReason, GenerationConfig,
    ModelCallId, ModelCallPhase, OperationHandle, OperationId, OperationOutcome, OperationPhase,
    OperationStatus, RunningToolBatchPhase, RuntimeConfig, SequentialIdSource, SessionId,
    SettlementDurability, ToolBatchId, ToolCallId, ToolDefinition, TurnId, UserMessage,
};
use philo_session::{ContextMessage, MemorySessionStore, SessionStore, ToolResultOutcome};
use support::failing_session::{FailingSessionStore, FailurePlan};
use support::fake_model::{FakeModel, ModelScript};
use support::fake_tool::{FakeTool, FakeToolResult};
use support::gate::Gate;
use support::gated_session::GatedSessionStore;

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn poll_once<F: Future + ?Sized>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.as_mut().poll(&mut context)
}

fn runtime(
    model: Arc<FakeModel>,
    sessions: Arc<dyn SessionStore>,
    tools: Arc<FakeTool>,
    max_tool_rounds: u32,
) -> AgentRuntime {
    AgentRuntime::with_tools(
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

fn collect_events(mut handle: OperationHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = block_on(handle.next_event()) {
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
        let handle = block_on($runtime.prompt(sid(), UserMessage::new("warmup"))).unwrap();
        let mut warmup = Box::pin(async move { handle.wait().await });
        assert!(poll_once(&mut warmup).is_pending(), "warmup must suspend");
        warmup
    }};
}

// --- Injection point 1: cancel while queued (M6-005) -------------------------

#[test]
fn queued_cancel_settles_immediately_with_zero_trace() {
    let warmup_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text(&["third"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime(model, sessions.clone(), no_tools(), 1);

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    let victim = block_on(agent.prompt(sid(), UserMessage::new("victim"))).unwrap();
    let third = block_on(agent.prompt(sid(), UserMessage::new("third"))).unwrap();
    assert_eq!(victim.phase(), OperationPhase::Queued);
    assert_eq!(third.phase(), OperationPhase::Queued);

    victim.cancel();
    assert_eq!(
        victim.phase(),
        OperationPhase::Settled(OperationStatus::Cancelled)
    );
    assert!(matches!(
        block_on(victim.wait()),
        OperationOutcome::Cancelled
    ));

    warmup_gate.release();
    assert!(matches!(
        block_on(&mut warmup),
        OperationOutcome::Succeeded { .. }
    ));

    // The queue continues past the removed entry.
    assert!(matches!(
        block_on(third.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "third"
    ));

    let victim_id = OperationId::new("operation-2");
    assert_eq!(
        collect_events(victim),
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
            },
        ]
    );

    // Zero persistent trace: only warmup and third committed anything.
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
    assert_eq!(view.revision(), philo_session::SessionRevision::new(4));
    assert_eq!(view.messages().len(), 4, "two turns, user+assistant each");
}

// --- Injection point 2: cancel before Barrier A persists ---------------------

#[test]
fn cancel_before_barrier_a_leaves_zero_trace() {
    let warmup_gate = Gate::new();
    let read_gate = Gate::new();
    let model = Arc::new(FakeModel::new([ModelScript::text_suspending(
        &[],
        &warmup_gate,
        &["done"],
    )]));
    let sessions = Arc::new(GatedSessionStore::memory().gate_context_read_at(2, &read_gate));
    let agent = runtime(model, sessions.clone(), no_tools(), 1);

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    let victim = block_on(agent.prompt(sid(), UserMessage::new("victim"))).unwrap();
    warmup_gate.release();
    let _ = block_on(&mut warmup);

    let mut wait = Box::pin(victim.wait());
    assert!(
        poll_once(&mut wait).is_pending(),
        "suspends in context read"
    );
    assert_eq!(victim.phase(), OperationPhase::PreparingTurn);
    victim.cancel();
    read_gate.release();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));

    let victim_id = OperationId::new("operation-2");
    assert_eq!(
        collect_events(victim),
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
            },
        ],
        "no TurnCancelled: durably no turn ever existed"
    );

    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
    assert_eq!(
        view.revision(),
        philo_session::SessionRevision::new(2),
        "only the warmup turn committed"
    );
}

// --- Injection point 3: cancel during the model stream (M6-001) --------------

#[test]
fn cancel_during_model_stream_discards_output_and_commits_terminal_facts() {
    let warmup_gate = Gate::new();
    let victim_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text_suspending(&["par"], &victim_gate, &["tial"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime(model, sessions.clone(), no_tools(), 1);

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    let victim = block_on(agent.prompt(sid(), UserMessage::new("victim"))).unwrap();
    warmup_gate.release();
    let _ = block_on(&mut warmup);

    let mut wait = Box::pin(victim.wait());
    assert!(poll_once(&mut wait).is_pending(), "suspends mid-stream");
    assert_eq!(
        victim.phase(),
        OperationPhase::RunningModelCall(ModelCallPhase::Streaming)
    );
    victim.cancel();
    // The victim gate is never released: the stream is dropped instead.
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));

    let victim_id = OperationId::new("operation-2");
    let events = collect_events(victim);
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
            },
        ]
    );
    assert_eq!(settled_count(&events), 1);

    // No AssistantMessage; the cancellation transaction advanced the session.
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
    assert_eq!(view.revision(), philo_session::SessionRevision::new(4));
    assert!(matches!(
        view.messages().last(),
        Some(ContextMessage::User { parts })
            if parts == &philo_session::SessionUserPart::text_parts("victim")
    ));
}

// --- Injection point 3 variant: between rounds after C_k (M6-003) ------------

#[test]
fn cancel_between_rounds_commits_terminal_entries_only() {
    let warmup_gate = Gate::new();
    let commit_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::tool_call(0, Some("call-1"), Some("echo"), &["{}"]),
    ]));
    // Warmup: commits 1..2. Victim: A=3, batch=4, results=5 (gated).
    let sessions = Arc::new(GatedSessionStore::memory().gate_commit_at(5, &commit_gate));
    let tools = Arc::new(FakeTool::new([echo()], [FakeToolResult::success("one")]));
    let agent = runtime(model.clone(), sessions.clone(), tools, 2);

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    let victim = block_on(agent.prompt(sid(), UserMessage::new("victim"))).unwrap();
    warmup_gate.release();
    let _ = block_on(&mut warmup);

    let mut wait = Box::pin(victim.wait());
    assert!(poll_once(&mut wait).is_pending(), "suspends in C_k commit");
    victim.cancel();
    commit_gate.release();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));

    // C_k succeeded, so the executed call keeps its real persisted result and
    // the cancellation transaction carries only the two terminal entries.
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
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

    let events = collect_events(victim);
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

#[test]
fn cancel_mid_batch_completes_the_executing_call_and_marks_the_rest() {
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
    let agent = runtime(model, sessions.clone(), tools.clone(), 1);

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    let victim = block_on(agent.prompt(sid(), UserMessage::new("victim"))).unwrap();
    warmup_gate.release();
    let _ = block_on(&mut warmup);

    let mut wait = Box::pin(victim.wait());
    assert!(poll_once(&mut wait).is_pending(), "call-1 is executing");
    assert_eq!(
        victim.phase(),
        OperationPhase::RunningToolBatch(RunningToolBatchPhase::Executing {
            in_flight: 1,
            completed: 0,
        })
    );
    victim.cancel();
    tool_gate.release();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));

    assert_eq!(tools.invocation_count(), 1, "call-2 never executes");

    let victim_id = OperationId::new("operation-2");
    let batch_id = ToolBatchId::new("turn-2:tool-batch:1");
    let events = collect_events(victim);
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
            },
        ]
    );

    // Real prefix + Cancelled suffix persisted atomically with the terminals.
    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
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

#[test]
fn cancel_after_batch_commit_marks_every_call_cancelled() {
    let warmup_gate = Gate::new();
    let commit_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::tool_calls(&[(0, "call-1", "echo", "{}"), (1, "call-2", "echo", "{}")]),
    ]));
    // Warmup: commits 1..2. Victim: A=3, batch commit B_k=4 (gated).
    let sessions = Arc::new(GatedSessionStore::memory().gate_commit_at(4, &commit_gate));
    let tools = Arc::new(FakeTool::new([echo()], []));
    let agent = runtime(model, sessions.clone(), tools.clone(), 1);

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    let victim = block_on(agent.prompt(sid(), UserMessage::new("victim"))).unwrap();
    warmup_gate.release();
    let _ = block_on(&mut warmup);

    let mut wait = Box::pin(victim.wait());
    assert!(poll_once(&mut wait).is_pending(), "suspends in B_k commit");
    victim.cancel();
    commit_gate.release();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));

    assert_eq!(tools.invocation_count(), 0, "no call ever executes");

    let events = collect_events(victim);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolExecutionStarted { .. })),
        "zero execution events"
    );
    assert_eq!(settled_count(&events), 1);

    let view = block_on(sessions.context_view(&philo_session::SessionId::new("s"))).unwrap();
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

#[test]
fn cancel_after_final_decision_is_a_no_op() {
    let warmup_gate = Gate::new();
    let commit_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text(&["fin"]),
    ]));
    // Warmup: commits 1..2. Victim: A=3, final settlement=4 (gated).
    let sessions = Arc::new(GatedSessionStore::memory().gate_commit_at(4, &commit_gate));
    let agent = runtime(model, sessions.clone(), no_tools(), 1);

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    let victim = block_on(agent.prompt(sid(), UserMessage::new("victim"))).unwrap();
    warmup_gate.release();
    let _ = block_on(&mut warmup);

    let mut wait = Box::pin(victim.wait());
    assert!(
        poll_once(&mut wait).is_pending(),
        "suspends in final commit"
    );
    assert_eq!(victim.phase(), OperationPhase::Finalizing);
    victim.cancel();
    commit_gate.release();
    assert!(matches!(
        block_on(wait),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "fin"
    ));

    let events = collect_events(victim);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::CancellationRequested { .. } | AgentEvent::TurnCancelled { .. }
        )),
        "an ineffective cancel publishes nothing"
    );
    assert!(matches!(
        block_on(sessions.context_view(&philo_session::SessionId::new("s")))
            .unwrap()
            .messages()
            .last(),
        Some(ContextMessage::Assistant { content }) if content == "fin"
    ));
}

// --- Idempotency (M6-009) ------------------------------------------------------

#[test]
fn cancel_is_idempotent_and_late_cancels_are_no_ops() {
    let warmup_gate = Gate::new();
    let victim_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text_suspending(&["x"], &victim_gate, &["y"]),
    ]));
    let sessions = Arc::new(MemorySessionStore::new());
    let agent = runtime(model, sessions, no_tools(), 1);

    // Keep the warmup handle borrowable: the settled no-op check needs it.
    let warmup = block_on(agent.prompt(sid(), UserMessage::new("warmup"))).unwrap();
    let mut warmup_wait = Box::pin(warmup.wait());
    assert!(
        poll_once(&mut warmup_wait).is_pending(),
        "warmup must suspend"
    );
    let victim = block_on(agent.prompt(sid(), UserMessage::new("victim"))).unwrap();
    warmup_gate.release();
    assert!(matches!(
        block_on(&mut warmup_wait),
        OperationOutcome::Succeeded { .. }
    ));
    drop(warmup_wait);

    // Cancelling a settled (succeeded) operation is a no-op.
    warmup.cancel();
    assert!(matches!(
        block_on(warmup.wait()),
        OperationOutcome::Succeeded { .. }
    ));

    let mut wait = Box::pin(victim.wait());
    assert!(poll_once(&mut wait).is_pending());
    victim.cancel();
    victim.cancel();
    victim.cancel();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));

    // Cancel after settlement: no state change, no extra events.
    victim.cancel();
    assert_eq!(
        victim.phase(),
        OperationPhase::Settled(OperationStatus::Cancelled)
    );
    let events = collect_events(victim);
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

#[test]
fn cancel_commit_failure_settles_failed_confirmed() {
    let warmup_gate = Gate::new();
    let victim_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text_suspending(&["x"], &victim_gate, &["y"]),
    ]));
    // Warmup: commits 1..2. Victim: A=3, cancel transaction=4 fails once,
    // failure settlement=5 succeeds.
    let sessions = Arc::new(FailingSessionStore::memory(FailurePlan::commit_at(4)));
    let agent = runtime(model, sessions, no_tools(), 1);

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    let victim = block_on(agent.prompt(sid(), UserMessage::new("victim"))).unwrap();
    warmup_gate.release();
    let _ = block_on(&mut warmup);

    let mut wait = Box::pin(victim.wait());
    assert!(poll_once(&mut wait).is_pending());
    victim.cancel();
    let outcome = block_on(wait);
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

    let events = collect_events(victim);
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

#[test]
fn cancel_commit_persistent_failure_settles_unconfirmed() {
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
    let agent = runtime(model, sessions, no_tools(), 1);

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    let victim = block_on(agent.prompt(sid(), UserMessage::new("victim"))).unwrap();
    warmup_gate.release();
    let _ = block_on(&mut warmup);

    let mut wait = Box::pin(victim.wait());
    assert!(poll_once(&mut wait).is_pending());
    victim.cancel();
    assert!(matches!(
        block_on(wait),
        OperationOutcome::Failed {
            failure,
            durability: SettlementDurability::Unconfirmed,
        } if failure.kind() == AgentFailureKind::Persistence
    ));
}

// --- FIFO queue and start timing (M6-004) -------------------------------------

#[test]
fn queue_is_fifo_and_operations_start_at_dequeue() {
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
    );

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    let first = block_on(agent.prompt(sid(), UserMessage::new("first"))).unwrap();
    let second = block_on(agent.prompt(sid(), UserMessage::new("second"))).unwrap();
    assert_eq!(first.phase(), OperationPhase::Queued);
    assert_eq!(second.phase(), OperationPhase::Queued);

    // The queue is strictly FIFO: the second operation cannot start while the
    // first still waits, even when polled.
    let mut second_wait = Box::pin(second.wait());
    assert!(poll_once(&mut second_wait).is_pending());
    assert_eq!(second.phase(), OperationPhase::Queued, "no queue jumping");

    warmup_gate.release();
    let _ = block_on(&mut warmup);

    assert!(poll_once(&mut second_wait).is_pending());
    assert_eq!(
        second.phase(),
        OperationPhase::Queued,
        "still blocked behind the first queued operation"
    );

    // Scripts are consumed in FIFO order: first gets "alpha", second "beta".
    assert!(matches!(
        block_on(first.wait()),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "alpha"
    ));
    assert!(matches!(
        block_on(second_wait),
        OperationOutcome::Succeeded { assistant } if assistant.content() == "beta"
    ));

    // OperationStarted publishes at dequeue: it follows OperationQueued.
    let events = collect_events(first);
    assert!(matches!(events[0], AgentEvent::OperationQueued { .. }));
    assert!(matches!(events[1], AgentEvent::OperationStarted { .. }));
}

// --- AgentAvailability (M6-004) -------------------------------------------------

#[test]
fn availability_reflects_the_actively_driven_operation() {
    let warmup_gate = Gate::new();
    let victim_gate = Gate::new();
    let model = Arc::new(FakeModel::new([
        ModelScript::text_suspending(&[], &warmup_gate, &["done"]),
        ModelScript::text_suspending(&[], &victim_gate, &["late"]),
    ]));
    let agent = runtime(model, Arc::new(MemorySessionStore::new()), no_tools(), 1);

    assert_eq!(agent.availability(), AgentAvailability::Idle);

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    assert_eq!(
        agent.availability(),
        AgentAvailability::Busy {
            operation_id: OperationId::new("operation-1"),
        }
    );

    let follow_up = block_on(agent.prompt(sid(), UserMessage::new("follow"))).unwrap();
    assert_eq!(
        agent.availability(),
        AgentAvailability::Busy {
            operation_id: OperationId::new("operation-1"),
        },
        "queued operations do not change the active observation"
    );

    warmup_gate.release();
    let _ = block_on(&mut warmup);
    assert_eq!(agent.availability(), AgentAvailability::Idle);

    let mut wait = Box::pin(follow_up.wait());
    assert!(poll_once(&mut wait).is_pending());
    assert_eq!(
        agent.availability(),
        AgentAvailability::Busy {
            operation_id: OperationId::new("operation-2"),
        }
    );

    victim_gate.release();
    assert!(matches!(block_on(wait), OperationOutcome::Succeeded { .. }));
    assert_eq!(agent.availability(), AgentAvailability::Idle);
}

// --- Cancelled results replay into later turns ---------------------------------

#[test]
fn cancelled_marks_replay_into_the_next_turn_context() {
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
    let agent = runtime(model.clone(), sessions, tools, 1);

    let mut warmup = suspended_warmup!(agent, warmup_gate);
    let victim = block_on(agent.prompt(sid(), UserMessage::new("victim"))).unwrap();
    warmup_gate.release();
    let _ = block_on(&mut warmup);

    let mut wait = Box::pin(victim.wait());
    assert!(poll_once(&mut wait).is_pending());
    victim.cancel();
    tool_gate.release();
    assert!(matches!(block_on(wait), OperationOutcome::Cancelled));

    // The next turn's model call replays the cancelled turn's partial tool
    // trajectory, including the Cancelled outcome mirror.
    let next = block_on(agent.prompt(sid(), UserMessage::new("continue"))).unwrap();
    assert!(matches!(
        block_on(next.wait()),
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
