//! Cancellable model-stream polling and output assembly.

use crate::operation::OperationShared;
use crate::{AgentFailure, ModelError, ModelEvent, ModelEventStream, ToolCallDelta};
use philo_agent_kernel as kernel;
use std::collections::{HashMap, HashSet};

/// One consumed step of a cancellable model stream.
pub(super) enum StreamStep {
    Event(Option<Result<ModelEvent, ModelError>>),
    CancelObserved,
}

/// Polls the next stream event, but observes a pending cancel request first
/// so cancellation cuts in even while the provider stream is quiet.
pub(super) async fn next_or_cancel(
    stream: &mut dyn ModelEventStream,
    shared: &OperationShared,
) -> StreamStep {
    tokio::select! {
        biased;
        _ = shared.wait_until_cancelled() => StreamStep::CancelObserved,
        event = stream.next() => StreamStep::Event(event),
    }
}

pub(super) async fn next_or_maintenance_cancel(
    stream: &mut dyn ModelEventStream,
    ctx: &super::EngineContext,
) -> StreamStep {
    tokio::select! {
        biased;
        _ = ctx.wait_maintenance_cancel() => StreamStep::CancelObserved,
        event = stream.next() => StreamStep::Event(event),
    }
}

/// Accumulates streamed deltas for live UI and diagnostics. The
/// authoritative assistant output is `ModelEvent::Completed.blocks`, not
/// this assembler.
#[derive(Default)]
pub(super) struct OutputAssembler {
    text: String,
    calls: HashMap<usize, CallParts>,
    order: Vec<usize>,
}
#[derive(Default)]
struct CallParts {
    id: String,
    name: String,
    arguments: String,
}
impl OutputAssembler {
    pub fn text(&mut self, delta: &str) {
        self.text.push_str(delta);
    }
    pub fn tool(&mut self, delta: ToolCallDelta) {
        if !self.calls.contains_key(&delta.index) {
            self.order.push(delta.index);
        }
        let parts = self.calls.entry(delta.index).or_default();
        if let Some(id) = delta.id {
            parts.id.push_str(&id);
        }
        if let Some(name) = delta.name {
            parts.name.push_str(&name);
        }
        parts.arguments.push_str(&delta.arguments);
    }
    #[allow(dead_code)]
    pub fn finish(self) -> Result<(String, Vec<kernel::KernelToolCall>), AgentFailure> {
        let mut ids = HashSet::new();
        let mut calls = Vec::new();
        for index in self.order {
            let parts = match self.calls.get(&index) {
                Some(parts) => parts,
                None => {
                    return Err(AgentFailure::invalid_model_output(
                        "assembler missing recorded call index",
                    ));
                }
            };
            if parts.id.is_empty() || parts.name.trim().is_empty() || !ids.insert(parts.id.clone())
            {
                return Err(AgentFailure::invalid_model_output(
                    "model produced incomplete or duplicate tool calls",
                ));
            }
            calls.push(kernel::KernelToolCall::new(
                kernel::ToolCallId::new(&parts.id),
                &parts.name,
                &parts.arguments,
            ));
        }
        Ok((self.text, calls))
    }
}
