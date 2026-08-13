use crate::{
    EffectId, KernelPhaseView, KernelToolCall, ModelCallId, ToolBatchId, TurnId, TurnMessage,
    TurnOutcome,
};
use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub struct KernelState {
    pub(crate) inner: State,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    ExpectingTurnStart,
    ExpectingModelCompletion {
        effect_id: EffectId,
        model_call_id: ModelCallId,
        /// Tool batches accepted so far in this turn.
        used_rounds: u32,
        max_tool_rounds: u32,
        turn_id: TurnId,
        transcript: Vec<TurnMessage>,
    },
    ExpectingToolBatchCompletion {
        effect_id: EffectId,
        tool_batch_id: ToolBatchId,
        /// One-based round of the outstanding batch.
        round: u32,
        max_tool_rounds: u32,
        turn_id: TurnId,
        transcript: Vec<TurnMessage>,
        calls: Vec<KernelToolCall>,
    },
    Terminated {
        outcome: TurnOutcome,
        completed_effect_id: EffectId,
    },
}

impl fmt::Debug for KernelState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KernelState")
            .field("phase", &phase(self))
            .finish_non_exhaustive()
    }
}

pub fn initial_state() -> KernelState {
    KernelState {
        inner: State::ExpectingTurnStart,
    }
}
pub fn phase(state: &KernelState) -> KernelPhaseView {
    match &state.inner {
        State::ExpectingTurnStart => KernelPhaseView::ExpectingTurnStart,
        State::ExpectingModelCompletion {
            effect_id,
            model_call_id,
            ..
        } => KernelPhaseView::ExpectingModelCompletion {
            effect_id: effect_id.clone(),
            model_call_id: model_call_id.clone(),
        },
        State::ExpectingToolBatchCompletion {
            effect_id,
            tool_batch_id,
            ..
        } => KernelPhaseView::ExpectingToolBatchCompletion {
            effect_id: effect_id.clone(),
            tool_batch_id: tool_batch_id.clone(),
        },
        State::Terminated { outcome, .. } => KernelPhaseView::Terminated { outcome: *outcome },
    }
}
