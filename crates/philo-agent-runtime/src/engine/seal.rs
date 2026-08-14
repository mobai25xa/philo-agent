//! M11 seal step: closing stale unfinished turns before a new turn starts.

use super::EngineContext;
use crate::TurnId;
use crate::mapping::failure::session_failure;
use crate::operation::OperationPublisher;
use philo_session as session;

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
    if context.open_turns().is_empty() {
        return SealOutcome::Sealed(operation, context);
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
        });
        let commit = ctx
            .sessions
            .commit(session::SessionTransaction::linear(
                session_id.clone(),
                seal_revision,
                entries,
            ))
            .await;
        match commit {
            Ok(commit) => {
                seal_revision = commit.revision();
                operation.prior_turn_sealed(TurnId::new(open.turn_id().as_str()));
            }
            Err(error) => {
                operation.fail_unconfirmed(session_failure("sealing stale turn", &error));
                return SealOutcome::Settled;
            }
        }
    }
    // The snapshot must see the sealed facts: re-read the view.
    let context = ctx.sessions.context_view(session_id).await;
    match context {
        Ok(context) => SealOutcome::Sealed(operation, context),
        Err(error) => {
            operation.fail_unconfirmed(session_failure("re-reading sealed context", &error));
            SealOutcome::Settled
        }
    }
}
