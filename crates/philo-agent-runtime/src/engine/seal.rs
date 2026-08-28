//! M11 seal step: closing stale unfinished turns before a new turn starts.

use super::EngineContext;
use crate::TurnId;
use crate::mapping::failure::{commit_failure, session_failure};
use crate::operation::OperationPublisher;
use crate::RetryDisposition;
use philo_session as session;

// The view variant intentionally carries the full SessionContextView by
// value; the title field pushed it past clippy's default size threshold.
#[allow(clippy::large_enum_variant)]
pub(super) enum SealOutcome {
    /// Every stale turn is sealed; carries the refreshed context view.
    Sealed(OperationPublisher, session::SessionContextView),
    /// A seal transaction failed and the operation settled.
    Settled,
}

/// Seals every stale unfinished turn — one independent transaction each, in
/// source order — before this turn starts (M11). A seal commit failure
/// fails this operation with the new turn durably absent; the next prompt
/// resumes from the remaining remnants (idempotent progress). Sealing
/// bypasses the kernel: the stale turn has no living KernelState, so the
/// runtime constructs the session transaction directly and the validation
/// core rules on its shape.
pub(super) async fn seal_stale_turns(
    ctx: &EngineContext,
    operation: OperationPublisher,
    session_id: &session::SessionId,
    context: session::SessionContextView,
) -> SealOutcome {
    let mut sealed = Vec::new();
    match seal_stale_turns_with(ctx, session_id, context, |turn_id| {
        sealed.push(turn_id.clone());
    })
    .await
    {
        Ok(context) => {
            for turn_id in sealed {
                operation
                    .prior_turn_sealed(TurnId::new(turn_id.as_str()))
                    .await;
            }
            SealOutcome::Sealed(operation, context)
        }
        Err(SealFailure::Commit(error)) => {
            operation
                .fail_unconfirmed(commit_failure(
                    "engine.seal_commit_failed",
                    RetryDisposition::Safe { retry_after_ms: None },
                    "sealing stale turn",
                    &error,
                ))
                .await;
            SealOutcome::Settled
        }
        Err(SealFailure::Refresh(error)) => {
            operation
                .fail_unconfirmed(session_failure("re-reading sealed context", &error))
                .await;
            SealOutcome::Settled
        }
    }
}

/// Manual maintenance uses the same seal protocol without operation events.
pub(super) async fn seal_stale_turns_for_maintenance(
    ctx: &EngineContext,
    session_id: &session::SessionId,
    context: session::SessionContextView,
) -> Result<session::SessionContextView, SealFailure> {
    seal_stale_turns_with(ctx, session_id, context, |_| {}).await
}

pub(super) enum SealFailure {
    Commit(session::SessionError),
    Refresh(session::SessionError),
}

async fn seal_stale_turns_with(
    ctx: &EngineContext,
    session_id: &session::SessionId,
    context: session::SessionContextView,
    mut on_sealed: impl FnMut(&session::TurnId),
) -> Result<session::SessionContextView, SealFailure> {
    if context.open_turns().is_empty() {
        return Ok(context);
    }
    let mut seal_revision = context.revision();
    for open in context.open_turns() {
        let mut entries = Vec::new();
        if let Some(batch) = open.unfilled_batch() {
            // C_k atomicity: a stranded batch has zero durable results, so
            // every unfilled call is completed as Interrupted ("execution
            // state unknown" — side effects may have happened), never as
            // Cancelled ("never ran").
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
            usage: None,
        });
        let commit = ctx
            .sessions
            .commit(session::SessionTransaction::linear(
                session_id.clone(),
                seal_revision,
                entries,
            ))
            .await;
        let commit = commit.map_err(SealFailure::Commit)?;
        seal_revision = commit.revision();
        on_sealed(open.turn_id());
    }
    // The snapshot must see the sealed facts: re-read the view.
    ctx.sessions
        .context_view(session_id)
        .await
        .map_err(SealFailure::Refresh)
}
