//! Pure, deterministic turn-decision state machine for Philo Agent.

mod protocol;
mod state;
mod tool_loop;
mod transition;

pub use protocol::{
    AssistantBlock, AssistantOutput, DurabilityRequirement, EffectId, InvalidAssistantOutput,
    InvalidUserMessage, KernelEffect, KernelInput, KernelInputRejection,
    KernelInputRejectionReason, KernelObservation, KernelPhaseView, KernelToolCall,
    KernelToolResult, KernelToolResultOutcome, KernelTransition, ModelCallId, ToolBatchId,
    ToolCallId, TurnFailure, TurnId, TurnMessage, TurnOutcome, UserMessage, UserPart,
};
pub use state::{KernelState, initial_state, phase};
pub use transition::transition;
