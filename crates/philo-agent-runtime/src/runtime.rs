use crate::driver::{
    OutputAssembler, context_messages, driver, invalid, kernel_user_parts, session_user_parts,
    turn_messages,
};
use crate::operation::{
    Admission, OperationBuilder, OperationHandle, OperationShared, QueueClaim, Scheduler,
};
use crate::tool::{kernel_result, session_result};
use crate::{
    AgentAvailability, AgentError, AgentEvent, AgentFailure, AgentFailureKind, AssistantMessage,
    ModelCallPhase, ModelCallSnapshot, ModelError, ModelEvent, ModelEventStream, ModelMessage,
    ModelPort, OperationId, OperationPhase, RuntimeConfig, SessionId, TurnId, TurnSnapshot,
    UserMessage,
};
use philo_agent_kernel as kernel;
use philo_session as session;
use philo_tools::{RichToolResult, ToolInvocation, ToolPort, ToolRegistry};
use std::sync::Arc;

pub struct AgentRuntime {
    model: Arc<dyn ModelPort>,
    sessions: Arc<dyn session::SessionStore>,
    ids: Arc<dyn crate::IdSource>,
    tools: Arc<dyn ToolPort>,
    config: RuntimeConfig,
    scheduler: Arc<Scheduler>,
}

/// Owned dependencies threaded into an operation engine so queued operations
/// can be driven after `prompt()` has returned.
struct EngineContext {
    model: Arc<dyn ModelPort>,
    sessions: Arc<dyn session::SessionStore>,
    tools: Arc<dyn ToolPort>,
    config: RuntimeConfig,
    scheduler: Arc<Scheduler>,
}

impl AgentRuntime {
    /// M1-compatible constructor with an empty immutable registry.
    pub fn new(
        model: Arc<dyn ModelPort>,
        sessions: Arc<dyn session::SessionStore>,
        ids: Arc<dyn crate::IdSource>,
        config: RuntimeConfig,
    ) -> Self {
        Self::with_tools(
            model,
            sessions,
            ids,
            config,
            Arc::new(ToolRegistry::empty()),
        )
    }

    /// Constructs a runtime with a frozen tool port.
    pub fn with_tools(
        model: Arc<dyn ModelPort>,
        sessions: Arc<dyn session::SessionStore>,
        ids: Arc<dyn crate::IdSource>,
        config: RuntimeConfig,
        tools: Arc<dyn ToolPort>,
    ) -> Self {
        Self {
            model,
            sessions,
            ids,
            tools,
            config,
            scheduler: Scheduler::new(),
        }
    }

    /// Read-only availability observation: `Busy` names the operation the
    /// runtime is actively driving; queued operations are observed through
    /// their own handles.
    pub fn availability(&self) -> AgentAvailability {
        self.scheduler.availability()
    }

    fn engine_context(&self) -> EngineContext {
        EngineContext {
            model: self.model.clone(),
            sessions: self.sessions.clone(),
            tools: self.tools.clone(),
            config: self.config.clone(),
            scheduler: self.scheduler.clone(),
        }
    }

    /// Admits one user prompt and returns its handle immediately (M10:
    /// acceptance never drives the work inside `prompt()`). The operation is
    /// driven while the handle is polled (`next_event` / `wait`); events are
    /// consumable the moment they are published, and `cancel()` is reachable
    /// at any point while the operation runs.
    ///
    /// While another operation is active the new operation queues FIFO with
    /// phase `Queued`; queue entries are never persisted.
    pub async fn prompt(
        &self,
        session_id: SessionId,
        user_message: UserMessage,
    ) -> Result<OperationHandle, AgentError> {
        let operation_id = self.ids.next_operation_id();
        let turn_id = self.ids.next_turn_id();
        match self.scheduler.admit(&operation_id) {
            Admission::Direct => {
                let shared = Arc::new(OperationShared::new(
                    operation_id,
                    turn_id,
                    self.scheduler.clone(),
                    OperationPhase::PreparingTurn,
                ));
                let engine = Box::pin(drive_claimed_owned(
                    self.engine_context(),
                    shared.clone(),
                    session_id,
                    user_message,
                ));
                Ok(OperationHandle::with_engine(shared, engine))
            }
            Admission::Queued => {
                let shared = Arc::new(OperationShared::new(
                    operation_id.clone(),
                    turn_id,
                    self.scheduler.clone(),
                    OperationPhase::Queued,
                ));
                shared.publish(AgentEvent::OperationQueued { operation_id });
                let engine = Box::pin(run_queued(
                    self.engine_context(),
                    shared.clone(),
                    session_id,
                    user_message,
                ));
                Ok(OperationHandle::with_engine(shared, engine))
            }
        }
    }
}

/// Owned-context wrapper so the directly-admitted engine is 'static.
async fn drive_claimed_owned(
    ctx: EngineContext,
    shared: Arc<OperationShared>,
    session_id: SessionId,
    user_message: UserMessage,
) {
    drive_claimed(&ctx, shared, session_id, user_message).await;
}

/// Waits for the queue turn, then drives the operation. Completes without
/// driving when the operation was cancelled while still queued.
async fn run_queued(
    ctx: EngineContext,
    shared: Arc<OperationShared>,
    session_id: SessionId,
    user_message: UserMessage,
) {
    let claimed = std::future::poll_fn(|_| match ctx.scheduler.try_claim_queued(&shared) {
        QueueClaim::Claimed => std::task::Poll::Ready(true),
        QueueClaim::SettledInQueue => std::task::Poll::Ready(false),
        QueueClaim::NotYet => std::task::Poll::Pending,
    })
    .await;
    if claimed {
        drive_claimed(&ctx, shared, session_id, user_message).await;
    }
}

async fn drive_claimed(
    ctx: &EngineContext,
    shared: Arc<OperationShared>,
    session_id: SessionId,
    user_message: UserMessage,
) {
    let operation = OperationBuilder::begin(shared.clone(), ctx.config.operation_timeout);
    drive(ctx, &shared, operation, session_id, user_message).await;
    ctx.scheduler.release(shared.operation_id());
}

/// One consumed step of a cancellable model stream.
enum StreamStep {
    Event(Option<Result<ModelEvent, ModelError>>),
    CancelObserved,
}

/// Polls the next stream event, but observes a pending cancel request first
/// so cancellation cuts in even while the provider stream is quiet.
async fn next_or_cancel(stream: &mut dyn ModelEventStream, shared: &OperationShared) -> StreamStep {
    let mut next = stream.next();
    std::future::poll_fn(move |cx| {
        if shared.is_cancel_requested() {
            return std::task::Poll::Ready(StreamStep::CancelObserved);
        }
        next.as_mut().poll(cx).map(StreamStep::Event)
    })
    .await
}

#[allow(clippy::too_many_lines)]
async fn drive(
    ctx: &EngineContext,
    shared: &Arc<OperationShared>,
    mut operation: OperationBuilder,
    session_id: SessionId,
    user_message: UserMessage,
) {
    let stored_session_id = session::SessionId::new(session_id.as_str());
    let mut context = match ctx.sessions.context_view(&stored_session_id).await {
        Ok(context) => context,
        Err(error) => {
            operation.fail_unconfirmed(session_failure("reading session context", &error));
            return;
        }
    };
    // M11 seal step: every stale unfinished turn is sealed — one independent
    // transaction each, in source order — before this turn starts. A seal
    // commit failure fails this operation with the new turn durably absent;
    // the next prompt resumes from the remaining remnants (idempotent
    // progress). Sealing bypasses the kernel: the stale turn has no living
    // KernelState, so the runtime constructs the session transaction
    // directly and the validation core rules on its shape.
    if !context.open_turns().is_empty() {
        let mut seal_revision = context.revision();
        for open in context.open_turns() {
            let mut entries = Vec::new();
            if let Some(batch) = open.unfilled_batch() {
                // C_k atomicity: a stranded batch has zero durable results,
                // so every unfilled call is completed as Interrupted
                // ("execution state unknown" — side effects may have
                // happened), never as Cancelled ("never ran").
                for call_id in batch.unfilled_call_ids() {
                    entries.push(session::SessionEntryKind::ToolResult {
                        turn_id: open.turn_id().clone(),
                        tool_batch_id: batch.tool_batch_id().clone(),
                        result: session::SessionToolResult::interrupted(call_id.clone()),
                    });
                }
            }
            let reason = session::CancelReason::Abandoned;
            entries.push(session::SessionEntryKind::TurnTerminated {
                turn_id: open.turn_id().clone(),
                outcome: session::TurnOutcome::Cancelled { reason },
            });
            entries.push(session::SessionEntryKind::OperationSettled {
                operation_id: open.operation_id().clone(),
                outcome: session::OperationOutcome::Cancelled { reason },
            });
            match ctx
                .sessions
                .commit(session::SessionTransaction::linear(
                    stored_session_id.clone(),
                    seal_revision,
                    entries,
                ))
                .await
            {
                Ok(commit) => {
                    seal_revision = commit.revision();
                    operation.prior_turn_sealed(TurnId::new(open.turn_id().as_str()));
                }
                Err(error) => {
                    operation.fail_unconfirmed(session_failure("sealing stale turn", &error));
                    return;
                }
            }
        }
        // The snapshot must see the sealed facts: re-read the view.
        context = match ctx.sessions.context_view(&stored_session_id).await {
            Ok(context) => context,
            Err(error) => {
                operation.fail_unconfirmed(session_failure("re-reading sealed context", &error));
                return;
            }
        };
    }
    operation.turn_started();
    let turn = TurnSnapshot {
        session_id: session_id.clone(),
        session_revision: context.revision(),
        context_messages: context_messages(&context),
        system_prompt: ctx.config.system_prompt.clone(),
        model_target: ctx.config.model_target.clone(),
        generation: ctx.config.generation.clone(),
        tools: ctx.tools.definitions(),
        max_tool_rounds: ctx.config.max_tool_rounds,
    };
    // Explicit runtime -> kernel mapping; both layers enforce the same
    // structural rules, so a constructed UserMessage always converts.
    let kernel_user = match kernel::UserMessage::from_parts(kernel_user_parts(user_message.parts()))
    {
        Ok(user) => user,
        Err(_) => {
            operation.fail_unconfirmed(driver("kernel rejected user message parts"));
            return;
        }
    };
    let initial = kernel::initial_state();
    let started = match kernel::transition(
        &initial,
        kernel::KernelInput::BeginTurn {
            turn_id: kernel::TurnId::new(operation.turn_id().as_str()),
            user_message: kernel_user,
            max_tool_rounds: turn.max_tool_rounds,
        },
    ) {
        Ok(value) => value,
        Err(_) => {
            operation.fail_unconfirmed(driver("kernel rejected BeginTurn"));
            return;
        }
    };
    let entries = match start_entries(
        &started.observations,
        operation.operation_id(),
        operation.turn_id(),
    ) {
        Ok(entries) => entries,
        Err(failure) => {
            operation.fail_unconfirmed(failure);
            return;
        }
    };
    // Injection point: cancel before Barrier A persists ends with zero
    // persistent trace; no turn exists durably.
    if shared.is_cancel_requested() {
        operation.cancellation_observed();
        operation.cancel_zero_trace();
        return;
    }
    let start_commit = match ctx
        .sessions
        .commit(session::SessionTransaction::linear(
            stored_session_id.clone(),
            context.revision(),
            entries,
        ))
        .await
    {
        Ok(commit) => commit,
        Err(error) => {
            operation.fail_unconfirmed(session_failure("committing turn start", &error));
            return;
        }
    };
    let mut state = started.next_state;
    let mut revision = start_commit.revision();
    let mut effect = started.effect.expect("BeginTurn always requests model");
    let mut model_call_index: u32 = 0;
    loop {
        match effect {
            kernel::KernelEffect::RequestModel {
                effect_id,
                model_call_id,
                turn_messages: messages,
                tools_allowed,
            } => {
                // Injection point: between barriers, before the next model
                // call starts. The latest batch is already fully committed,
                // so the cancellation transaction is terminal-only.
                if shared.is_cancel_requested() {
                    operation.cancellation_observed();
                    cancel_started_turn(
                        ctx,
                        operation,
                        &stored_session_id,
                        revision,
                        &state,
                        effect_id,
                        Vec::new(),
                        Vec::new(),
                    )
                    .await;
                    return;
                }
                model_call_index += 1;
                operation.set_phase(OperationPhase::RunningModelCall(ModelCallPhase::Starting));
                operation.push(AgentEvent::ModelCallStarted {
                    model_call_id: crate::ModelCallId::new(model_call_id.as_str()),
                });
                let request = ModelCallSnapshot {
                    operation_id: operation.operation_id().clone(),
                    turn_id: operation.turn_id().clone(),
                    model_call_id: crate::ModelCallId::new(model_call_id.as_str()),
                    model_call_index,
                    session_revision: turn.session_revision,
                    messages: build_messages(&turn, messages),
                    tools: if tools_allowed {
                        turn.tools.clone()
                    } else {
                        Vec::new()
                    },
                    model_target: turn.model_target.clone(),
                    generation: turn.generation.clone(),
                };
                let mut stream = match ctx.model.start(request).await {
                    Ok(stream) => stream,
                    Err(error) => {
                        fail_started_turn(
                            ctx,
                            operation,
                            &stored_session_id,
                            revision,
                            &state,
                            effect_id,
                            AgentFailure::new(AgentFailureKind::ModelCall, error.message()),
                        )
                        .await;
                        return;
                    }
                };
                operation.set_phase(OperationPhase::RunningModelCall(
                    ModelCallPhase::WaitingForFirstOutput,
                ));
                let mut assembler = OutputAssembler::default();
                let mut completed_seen = false;
                let mut response_started_seen = false;
                loop {
                    match next_or_cancel(stream.as_mut(), shared).await {
                        // Injection point: cancel during the model stream.
                        // Dropping the stream is the ModelPort cancellation
                        // signal; published text deltas stay transient facts.
                        StreamStep::CancelObserved => {
                            drop(stream);
                            operation.cancellation_observed();
                            cancel_started_turn(
                                ctx,
                                operation,
                                &stored_session_id,
                                revision,
                                &state,
                                effect_id,
                                Vec::new(),
                                Vec::new(),
                            )
                            .await;
                            return;
                        }
                        StreamStep::Event(Some(Ok(ModelEvent::ResponseStarted {
                            response_model,
                            response_id,
                        }))) => {
                            if completed_seen {
                                fail_started_turn(
                                    ctx,
                                    operation,
                                    &stored_session_id,
                                    revision,
                                    &state,
                                    effect_id,
                                    invalid("model stream emitted output after Completed"),
                                )
                                .await;
                                return;
                            }
                            if response_started_seen {
                                fail_started_turn(
                                    ctx,
                                    operation,
                                    &stored_session_id,
                                    revision,
                                    &state,
                                    effect_id,
                                    invalid("model stream emitted ResponseStarted more than once"),
                                )
                                .await;
                                return;
                            }
                            response_started_seen = true;
                            operation.push(AgentEvent::ModelResponseStarted {
                                model_call_id: crate::ModelCallId::new(model_call_id.as_str()),
                                response_model,
                                response_id,
                            });
                        }
                        StreamStep::Event(Some(Ok(ModelEvent::TextDelta(delta)))) => {
                            if completed_seen {
                                fail_started_turn(
                                    ctx,
                                    operation,
                                    &stored_session_id,
                                    revision,
                                    &state,
                                    effect_id,
                                    invalid("model stream emitted output after Completed"),
                                )
                                .await;
                                return;
                            }
                            operation.set_phase(OperationPhase::RunningModelCall(
                                ModelCallPhase::Streaming,
                            ));
                            assembler.text(&delta);
                            operation.push(AgentEvent::TextDelta { delta });
                        }
                        // Transient observations: forwarded as-is, never part
                        // of the assembled output, never written to Session.
                        StreamStep::Event(Some(Ok(ModelEvent::ReasoningDelta { text }))) => {
                            if completed_seen {
                                fail_started_turn(
                                    ctx,
                                    operation,
                                    &stored_session_id,
                                    revision,
                                    &state,
                                    effect_id,
                                    invalid("model stream emitted output after Completed"),
                                )
                                .await;
                                return;
                            }
                            operation.push(AgentEvent::ReasoningDelta {
                                model_call_id: crate::ModelCallId::new(model_call_id.as_str()),
                                text,
                            });
                        }
                        StreamStep::Event(Some(Ok(ModelEvent::UsageUpdated { usage }))) => {
                            if completed_seen {
                                fail_started_turn(
                                    ctx,
                                    operation,
                                    &stored_session_id,
                                    revision,
                                    &state,
                                    effect_id,
                                    invalid("model stream emitted output after Completed"),
                                )
                                .await;
                                return;
                            }
                            operation.push(AgentEvent::ModelUsageUpdated {
                                model_call_id: crate::ModelCallId::new(model_call_id.as_str()),
                                usage,
                            });
                        }
                        StreamStep::Event(Some(Ok(ModelEvent::ToolCallDelta {
                            index,
                            id,
                            name,
                            arguments,
                        }))) => {
                            if completed_seen {
                                fail_started_turn(
                                    ctx,
                                    operation,
                                    &stored_session_id,
                                    revision,
                                    &state,
                                    effect_id,
                                    invalid("model stream emitted output after Completed"),
                                )
                                .await;
                                return;
                            }
                            assembler.tool(crate::ToolCallDelta {
                                index,
                                id,
                                name,
                                arguments,
                            });
                        }
                        StreamStep::Event(Some(Ok(ModelEvent::Completed))) if completed_seen => {
                            fail_started_turn(
                                ctx,
                                operation,
                                &stored_session_id,
                                revision,
                                &state,
                                effect_id,
                                invalid("model stream emitted Completed more than once"),
                            )
                            .await;
                            return;
                        }
                        StreamStep::Event(Some(Ok(ModelEvent::Completed))) => completed_seen = true,
                        StreamStep::Event(Some(Err(error))) => {
                            fail_started_turn(
                                ctx,
                                operation,
                                &stored_session_id,
                                revision,
                                &state,
                                effect_id,
                                AgentFailure::new(AgentFailureKind::ModelCall, error.message()),
                            )
                            .await;
                            return;
                        }
                        StreamStep::Event(None) if completed_seen => break,
                        StreamStep::Event(None) => {
                            fail_started_turn(
                                ctx,
                                operation,
                                &stored_session_id,
                                revision,
                                &state,
                                effect_id,
                                driver("model stream ended before Completed"),
                            )
                            .await;
                            return;
                        }
                    }
                }
                let (text, calls) = match assembler.finish() {
                    Ok(value) => value,
                    Err(failure) => {
                        fail_started_turn(
                            ctx,
                            operation,
                            &stored_session_id,
                            revision,
                            &state,
                            effect_id,
                            failure,
                        )
                        .await;
                        return;
                    }
                };
                let output = if calls.is_empty() {
                    kernel::AssistantOutput::final_text(&text)
                } else {
                    kernel::AssistantOutput::tool_calls(calls)
                };
                let completed = match kernel::transition(
                    &state,
                    kernel::KernelInput::ModelCallCompleted {
                        effect_id: effect_id.clone(),
                        output,
                    },
                ) {
                    Ok(value) => value,
                    Err(_) => {
                        fail_started_turn(
                            ctx,
                            operation,
                            &stored_session_id,
                            revision,
                            &state,
                            effect_id,
                            invalid("kernel rejected model output"),
                        )
                        .await;
                        return;
                    }
                };
                if !completed.observations.iter().any(|observation| {
                    matches!(
                        observation,
                        kernel::KernelObservation::AssistantToolCallsAccepted { .. }
                    )
                }) {
                    // The kernel made its final decision: cancellation can no
                    // longer take effect on this operation.
                    let final_entries = match success_entries(
                        &completed.observations,
                        operation.operation_id(),
                        operation.turn_id(),
                    ) {
                        Ok(entries) => entries,
                        Err(failure) => {
                            fail_started_turn(
                                ctx,
                                operation,
                                &stored_session_id,
                                revision,
                                &state,
                                effect_id,
                                failure,
                            )
                            .await;
                            return;
                        }
                    };
                    operation.set_phase(OperationPhase::Finalizing);
                    if let Err(error) = ctx
                        .sessions
                        .commit(session::SessionTransaction::linear(
                            stored_session_id.clone(),
                            revision,
                            final_entries,
                        ))
                        .await
                    {
                        fail_started_turn(
                            ctx,
                            operation,
                            &stored_session_id,
                            revision,
                            &state,
                            effect_id,
                            session_failure("committing successful settlement", &error),
                        )
                        .await;
                        return;
                    }
                    operation.succeed(AssistantMessage { content: text });
                    return;
                }
                let (tool_effect_id, batch_id, calls) = match completed.effect.as_ref() {
                    Some(kernel::KernelEffect::ExecuteToolBatch {
                        effect_id,
                        tool_batch_id,
                        calls,
                    }) => (effect_id.clone(), tool_batch_id.clone(), calls.clone()),
                    _ => {
                        fail_started_turn(
                            ctx,
                            operation,
                            &stored_session_id,
                            revision,
                            &state,
                            effect_id,
                            driver("tool-call transition omitted tool effect"),
                        )
                        .await;
                        return;
                    }
                };
                let batch_entry = session::SessionEntryKind::AssistantToolCallBatch {
                    turn_id: session::TurnId::new(operation.turn_id().as_str()),
                    model_call_id: model_call_id.as_str().to_owned(),
                    tool_batch_id: session::ToolBatchId::new(batch_id.as_str()),
                    calls: calls
                        .iter()
                        .map(|call| {
                            session::SessionToolCall::new(
                                session::ToolCallId::new(call.id().as_str()),
                                call.name(),
                                call.arguments(),
                            )
                        })
                        .collect(),
                };
                operation.set_phase(OperationPhase::RunningToolBatch(
                    crate::RunningToolBatchPhase::Preparing,
                ));
                let batch_commit = match ctx
                    .sessions
                    .commit(session::SessionTransaction::linear(
                        stored_session_id.clone(),
                        revision,
                        vec![batch_entry],
                    ))
                    .await
                {
                    Ok(commit) => commit,
                    Err(error) => {
                        fail_started_turn(
                            ctx,
                            operation,
                            &stored_session_id,
                            revision,
                            &state,
                            effect_id,
                            session_failure("committing tool calls", &error),
                        )
                        .await;
                        return;
                    }
                };
                revision = batch_commit.revision();
                state = completed.next_state;
                operation.push(AgentEvent::ToolBatchRequested {
                    tool_batch_id: crate::ToolBatchId::new(batch_id.as_str()),
                    call_count: calls.len(),
                });
                let mut results: Vec<(kernel::KernelToolCall, RichToolResult)> = Vec::new();
                for (index, call) in calls.iter().enumerate() {
                    // Injection point: barrier between tool calls (and before
                    // the first). The executing call always runs to completion;
                    // later calls never start.
                    if shared.is_cancel_requested() {
                        operation.cancellation_observed();
                        let (marks, executed_events) =
                            cancellation_batch(operation.turn_id(), &batch_id, &calls, &results);
                        cancel_started_turn(
                            ctx,
                            operation,
                            &stored_session_id,
                            revision,
                            &state,
                            tool_effect_id,
                            marks,
                            executed_events,
                        )
                        .await;
                        return;
                    }
                    operation.set_phase(OperationPhase::RunningToolBatch(
                        crate::RunningToolBatchPhase::Executing { index },
                    ));
                    operation.push(AgentEvent::ToolExecutionStarted {
                        tool_batch_id: crate::ToolBatchId::new(batch_id.as_str()),
                        tool_call_id: crate::ToolCallId::new(call.id().as_str()),
                        index,
                        tool_name: call.name().to_owned(),
                        arguments: call.arguments().to_owned(),
                    });
                    let result = match ctx
                        .tools
                        .invoke(ToolInvocation::new(
                            call.id().as_str(),
                            call.name(),
                            call.arguments(),
                        ))
                        .await
                    {
                        Ok(result) => result,
                        Err(error) => {
                            fail_started_turn(
                                ctx,
                                operation,
                                &stored_session_id,
                                revision,
                                &state,
                                tool_effect_id.clone(),
                                AgentFailure::new(AgentFailureKind::ToolExecution, error.message()),
                            )
                            .await;
                            return;
                        }
                    };
                    results.push((call.clone(), result));
                }
                // Injection point: after the last call finished, before the
                // results barrier commits: all-real completion, empty suffix.
                if shared.is_cancel_requested() {
                    operation.cancellation_observed();
                    let (marks, executed_events) =
                        cancellation_batch(operation.turn_id(), &batch_id, &calls, &results);
                    cancel_started_turn(
                        ctx,
                        operation,
                        &stored_session_id,
                        revision,
                        &state,
                        tool_effect_id,
                        marks,
                        executed_events,
                    )
                    .await;
                    return;
                }
                operation.set_phase(OperationPhase::RunningToolBatch(
                    crate::RunningToolBatchPhase::CommittingResults,
                ));
                // Model channel only: display never enters the Session.
                let result_entries = results
                    .iter()
                    .map(|(call, rich)| session::SessionEntryKind::ToolResult {
                        turn_id: session::TurnId::new(operation.turn_id().as_str()),
                        tool_batch_id: session::ToolBatchId::new(batch_id.as_str()),
                        result: session_result(call, rich.result()),
                    })
                    .collect();
                let results_commit = match ctx
                    .sessions
                    .commit(session::SessionTransaction::linear(
                        stored_session_id.clone(),
                        revision,
                        result_entries,
                    ))
                    .await
                {
                    Ok(commit) => commit,
                    Err(error) => {
                        fail_started_turn(
                            ctx,
                            operation,
                            &stored_session_id,
                            revision,
                            &state,
                            tool_effect_id.clone(),
                            session_failure("committing tool results", &error),
                        )
                        .await;
                        return;
                    }
                };
                for (index, (call, rich)) in results.iter().enumerate() {
                    operation.push(AgentEvent::ToolExecutionCompleted {
                        tool_batch_id: crate::ToolBatchId::new(batch_id.as_str()),
                        tool_call_id: crate::ToolCallId::new(call.id().as_str()),
                        index,
                        tool_name: call.name().to_owned(),
                        result: rich.result().clone(),
                        display: rich.display().cloned(),
                    });
                }
                revision = results_commit.revision();
                let kernel_results = results
                    .iter()
                    .map(|(call, rich)| kernel_result(call, rich.result()))
                    .collect();
                let next = match kernel::transition(
                    &state,
                    kernel::KernelInput::ToolBatchCompleted {
                        effect_id: tool_effect_id,
                        results: kernel_results,
                    },
                ) {
                    Ok(value) => value,
                    Err(_) => {
                        operation.fail_unconfirmed(invalid("kernel rejected tool results"));
                        return;
                    }
                };
                state = next.next_state;
                effect = next.effect.expect("tool completion requests model");
            }
            kernel::KernelEffect::ExecuteToolBatch { .. } => {
                operation
                    .fail_unconfirmed(driver("runtime cannot execute uncommitted tool effect"));
                return;
            }
        }
    }
}

/// Builds the completion marks and post-commit events for a mid-batch
/// cancellation: executed calls keep their real results (a source-order
/// prefix) with full event payloads, never-executed calls get `Cancelled`
/// marks (the suffix) and still publish no execution events (M6 semantics).
fn cancellation_batch(
    turn_id: &TurnId,
    batch_id: &kernel::ToolBatchId,
    calls: &[kernel::KernelToolCall],
    executed: &[(kernel::KernelToolCall, RichToolResult)],
) -> (Vec<session::SessionEntryKind>, Vec<AgentEvent>) {
    let turn = session::TurnId::new(turn_id.as_str());
    let batch = session::ToolBatchId::new(batch_id.as_str());
    let mut marks = Vec::new();
    let mut executed_events = Vec::new();
    for (index, (call, rich)) in executed.iter().enumerate() {
        marks.push(session::SessionEntryKind::ToolResult {
            turn_id: turn.clone(),
            tool_batch_id: batch.clone(),
            result: session_result(call, rich.result()),
        });
        executed_events.push(AgentEvent::ToolExecutionCompleted {
            tool_batch_id: crate::ToolBatchId::new(batch_id.as_str()),
            tool_call_id: crate::ToolCallId::new(call.id().as_str()),
            index,
            tool_name: call.name().to_owned(),
            result: rich.result().clone(),
            display: rich.display().cloned(),
        });
    }
    for call in &calls[executed.len()..] {
        marks.push(session::SessionEntryKind::ToolResult {
            turn_id: turn.clone(),
            tool_batch_id: batch.clone(),
            result: session::SessionToolResult::cancelled(session::ToolCallId::new(
                call.id().as_str(),
            )),
        });
    }
    (marks, executed_events)
}

/// Commits the cancellation of a turn whose start already persisted:
/// `completion_marks` (batch completion, possibly empty) plus the two
/// terminal entries in one atomic transaction. On commit failure the
/// `Terminated(Cancelled)` decision is discarded and the persistence error
/// takes the established failure path over the old outstanding effect —
/// the operation must never report `Cancelled` without the durable fact.
#[allow(clippy::too_many_arguments)]
async fn cancel_started_turn(
    ctx: &EngineContext,
    mut operation: OperationBuilder,
    session_id: &session::SessionId,
    revision: session::SessionRevision,
    state: &kernel::KernelState,
    effect_id: kernel::EffectId,
    completion_marks: Vec<session::SessionEntryKind>,
    executed_events: Vec<AgentEvent>,
) {
    if kernel::transition(
        state,
        kernel::KernelInput::CancelRequested {
            effect_id: effect_id.clone(),
        },
    )
    .is_err()
    {
        fail_started_turn(
            ctx,
            operation,
            session_id,
            revision,
            state,
            effect_id,
            driver("kernel rejected CancelRequested"),
        )
        .await;
        return;
    }
    // The accepted reason (user request or operation timeout) becomes part
    // of the durable terminal facts; the kernel never sees it.
    let reason = operation.cancel_reason();
    let mut entries = completion_marks;
    entries.push(session::SessionEntryKind::TurnTerminated {
        turn_id: session::TurnId::new(operation.turn_id().as_str()),
        outcome: session::TurnOutcome::Cancelled { reason },
    });
    entries.push(session::SessionEntryKind::OperationSettled {
        operation_id: session::OperationId::new(operation.operation_id().as_str()),
        outcome: session::OperationOutcome::Cancelled { reason },
    });
    match ctx
        .sessions
        .commit(session::SessionTransaction::linear(
            session_id.clone(),
            revision,
            entries,
        ))
        .await
    {
        Ok(_) => {
            for event in executed_events {
                operation.push(event);
            }
            operation.cancel_committed();
        }
        Err(error) => {
            fail_started_turn(
                ctx,
                operation,
                session_id,
                revision,
                state,
                effect_id,
                session_failure("committing cancellation", &error),
            )
            .await;
        }
    }
}

async fn fail_started_turn(
    ctx: &EngineContext,
    operation: OperationBuilder,
    session_id: &session::SessionId,
    revision: session::SessionRevision,
    state: &kernel::KernelState,
    effect_id: kernel::EffectId,
    failure: AgentFailure,
) {
    let termination = match kernel::transition(
        state,
        kernel::KernelInput::TerminationRequested {
            effect_id,
            failure: kernel_failure(&failure),
        },
    ) {
        Ok(value) => value,
        Err(_) => {
            operation.fail_unconfirmed(failure);
            return;
        }
    };
    let entries = match failure_entries(
        &termination.observations,
        operation.operation_id(),
        operation.turn_id(),
    ) {
        Ok(value) => value,
        Err(_) => {
            operation.fail_unconfirmed(failure);
            return;
        }
    };
    match ctx
        .sessions
        .commit(session::SessionTransaction::linear(
            session_id.clone(),
            revision,
            entries,
        ))
        .await
    {
        Ok(_) => operation.fail_confirmed(failure),
        Err(error) => operation.fail_unconfirmed(AgentFailure::new(
            failure.kind(),
            format!(
                "{}; failure settlement unconfirmed: {}",
                failure.message(),
                describe_session_error(&error)
            ),
        )),
    }
}

fn build_messages(turn: &TurnSnapshot, messages: Vec<kernel::TurnMessage>) -> Vec<ModelMessage> {
    let mut output = vec![ModelMessage::System {
        content: turn.system_prompt.clone(),
    }];
    output.extend(turn.context_messages.clone());
    output.extend(turn_messages(messages));
    output
}

fn start_entries(
    observations: &[kernel::KernelObservation],
    operation_id: &OperationId,
    turn_id: &TurnId,
) -> Result<Vec<session::SessionEntryKind>, AgentFailure> {
    let Some((observed, user)) = observations
        .iter()
        .find_map(|observation| match observation {
            kernel::KernelObservation::TurnBegan {
                turn_id,
                user_message,
            } => Some((turn_id, user_message)),
            _ => None,
        })
    else {
        return Err(driver("missing TurnBegan observation"));
    };
    if observed.as_str() != turn_id.as_str() {
        return Err(driver("unexpected TurnBegan turn id"));
    }
    Ok(vec![
        session::SessionEntryKind::OperationStarted {
            operation_id: session::OperationId::new(operation_id.as_str()),
        },
        session::SessionEntryKind::TurnStarted {
            operation_id: session::OperationId::new(operation_id.as_str()),
            turn_id: session::TurnId::new(turn_id.as_str()),
        },
        session::SessionEntryKind::UserMessage {
            turn_id: session::TurnId::new(turn_id.as_str()),
            parts: session_user_parts(user.parts()),
        },
    ])
}

fn success_entries(
    observations: &[kernel::KernelObservation],
    operation_id: &OperationId,
    turn_id: &TurnId,
) -> Result<Vec<session::SessionEntryKind>, AgentFailure> {
    let Some(output) = observations
        .iter()
        .find_map(|observation| match observation {
            kernel::KernelObservation::AssistantOutputAccepted { output, .. } => Some(output),
            _ => None,
        })
    else {
        return Err(driver("missing accepted assistant output"));
    };
    if !observations.iter().any(|observation| {
        matches!(
            observation,
            kernel::KernelObservation::TurnTerminated {
                outcome: kernel::TurnOutcome::Succeeded
            }
        )
    }) {
        return Err(driver("missing successful termination"));
    }
    Ok(vec![
        session::SessionEntryKind::AssistantMessage {
            turn_id: session::TurnId::new(turn_id.as_str()),
            content: output.text().to_owned(),
        },
        session::SessionEntryKind::TurnTerminated {
            turn_id: session::TurnId::new(turn_id.as_str()),
            outcome: session::TurnOutcome::Succeeded,
        },
        session::SessionEntryKind::OperationSettled {
            operation_id: session::OperationId::new(operation_id.as_str()),
            outcome: session::OperationOutcome::Succeeded,
        },
    ])
}

fn failure_entries(
    observations: &[kernel::KernelObservation],
    operation_id: &OperationId,
    turn_id: &TurnId,
) -> Result<Vec<session::SessionEntryKind>, AgentFailure> {
    let Some(failure) = observations
        .iter()
        .find_map(|observation| match observation {
            kernel::KernelObservation::TurnFailureAccepted { failure, .. } => Some(failure),
            _ => None,
        })
    else {
        return Err(driver("missing accepted failure"));
    };
    Ok(vec![
        session::SessionEntryKind::TurnFailure {
            turn_id: session::TurnId::new(turn_id.as_str()),
            failure: session_failure_from_kernel(failure),
        },
        session::SessionEntryKind::TurnTerminated {
            turn_id: session::TurnId::new(turn_id.as_str()),
            outcome: session::TurnOutcome::Failed,
        },
        session::SessionEntryKind::OperationSettled {
            operation_id: session::OperationId::new(operation_id.as_str()),
            outcome: session::OperationOutcome::Failed,
        },
    ])
}

fn kernel_failure(failure: &AgentFailure) -> kernel::TurnFailure {
    match failure.kind() {
        AgentFailureKind::ModelCall => kernel::TurnFailure::ModelCallFailed {
            message: failure.message().to_owned(),
        },
        AgentFailureKind::InvalidModelOutput => kernel::TurnFailure::InvalidModelOutput {
            message: failure.message().to_owned(),
        },
        AgentFailureKind::ToolExecution => kernel::TurnFailure::ToolExecutionFailed {
            message: failure.message().to_owned(),
        },
        AgentFailureKind::Persistence => kernel::TurnFailure::PersistenceFailed {
            message: failure.message().to_owned(),
        },
        AgentFailureKind::RuntimeDriver => kernel::TurnFailure::RuntimeDriverFailed {
            message: failure.message().to_owned(),
        },
    }
}
fn session_failure_from_kernel(failure: &kernel::TurnFailure) -> session::TurnFailure {
    match failure {
        kernel::TurnFailure::ModelCallFailed { message } => {
            session::TurnFailure::new(session::TurnFailureKind::ModelCall, message)
        }
        kernel::TurnFailure::InvalidModelOutput { message } => {
            session::TurnFailure::new(session::TurnFailureKind::InvalidModelOutput, message)
        }
        kernel::TurnFailure::ToolExecutionFailed { message } => {
            session::TurnFailure::new(session::TurnFailureKind::ToolExecution, message)
        }
        kernel::TurnFailure::PersistenceFailed { message } => {
            session::TurnFailure::new(session::TurnFailureKind::Persistence, message)
        }
        kernel::TurnFailure::RuntimeDriverFailed { message } => {
            session::TurnFailure::new(session::TurnFailureKind::RuntimeDriver, message)
        }
    }
}
fn session_failure(context: &str, error: &session::SessionError) -> AgentFailure {
    AgentFailure::new(
        AgentFailureKind::Persistence,
        format!("{context}: {}", describe_session_error(error)),
    )
}
fn describe_session_error(error: &session::SessionError) -> String {
    format!("{error:?}")
}
