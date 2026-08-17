//! Bounded live operation snapshot. This is not a second transcript.

use crate::bounds::{LIVE_REASONING_CHARS_MAX, LIVE_TEXT_CHARS_MAX, LIVE_TOOL_PROGRESS_MAX};
use crate::frontend::snapshot::FrontendTokenUsage;

/// In-flight tool progress row. Latest tail wins per call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveToolProgress {
    /// Batch id.
    pub tool_batch_id: String,
    /// Call id.
    pub tool_call_id: String,
    /// Source index.
    pub index: usize,
    /// Latest tail.
    pub tail: String,
}

/// Bounded projection of the operation that has not yet landed in Session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveOperationSnapshot {
    /// Current or last-accepted operation id.
    pub operation_id: Option<String>,
    /// Current turn id.
    pub turn_id: Option<String>,
    /// Current model call id.
    pub model_call_id: Option<String>,
    /// Merged assistant text for the in-flight call.
    pub text: String,
    /// Merged reasoning text for the in-flight call.
    pub reasoning: String,
    /// True when live text hit [`LIVE_TEXT_CHARS_MAX`].
    pub text_truncated: bool,
    /// True when live reasoning hit [`LIVE_REASONING_CHARS_MAX`].
    pub reasoning_truncated: bool,
    /// True when the live projection is incomplete and needs a Session resync
    /// after settlement (truncation or subscription lag).
    pub needs_resync: bool,
    /// Latest-wins tool progress, capped.
    pub tool_progress: Vec<LiveToolProgress>,
    /// Latest-wins usage for the current model call.
    pub usage: Option<FrontendTokenUsage>,
}

impl LiveOperationSnapshot {
    /// Empty live state.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when no live operation fields are populated.
    pub fn is_empty(&self) -> bool {
        self.operation_id.is_none()
            && self.text.is_empty()
            && self.reasoning.is_empty()
            && self.tool_progress.is_empty()
    }

    pub(crate) fn accept(&mut self, operation_id: impl Into<String>, turn_id: impl Into<String>) {
        self.operation_id = Some(operation_id.into());
        self.turn_id = Some(turn_id.into());
    }

    pub(crate) fn start_operation(&mut self, operation_id: impl Into<String>) {
        self.operation_id = Some(operation_id.into());
    }

    pub(crate) fn start_turn(&mut self, turn_id: impl Into<String>) {
        self.turn_id = Some(turn_id.into());
    }

    pub(crate) fn start_model_call(&mut self, model_call_id: impl Into<String>) {
        self.model_call_id = Some(model_call_id.into());
        self.text.clear();
        self.reasoning.clear();
        self.text_truncated = false;
        self.reasoning_truncated = false;
        self.usage = None;
    }

    pub(crate) fn push_text(&mut self, delta: &str) {
        if append_bounded(&mut self.text, delta, LIVE_TEXT_CHARS_MAX) {
            self.text_truncated = true;
            self.needs_resync = true;
        }
    }

    pub(crate) fn push_reasoning(&mut self, text: &str) {
        if append_bounded(&mut self.reasoning, text, LIVE_REASONING_CHARS_MAX) {
            self.reasoning_truncated = true;
            self.needs_resync = true;
        }
    }

    pub(crate) fn set_usage(&mut self, usage: FrontendTokenUsage) {
        self.usage = Some(usage);
    }

    pub(crate) fn set_tool_progress(&mut self, row: LiveToolProgress) {
        if let Some(existing) = self
            .tool_progress
            .iter_mut()
            .find(|item| item.tool_call_id == row.tool_call_id)
        {
            *existing = row;
            return;
        }
        if self.tool_progress.len() >= LIVE_TOOL_PROGRESS_MAX {
            self.tool_progress.remove(0);
            self.needs_resync = true;
        }
        self.tool_progress.push(row);
    }

    pub(crate) fn complete_tool(&mut self, tool_call_id: &str) {
        self.tool_progress
            .retain(|row| row.tool_call_id != tool_call_id);
    }

    pub(crate) fn mark_lagged(&mut self) {
        self.needs_resync = true;
    }

    /// Durable settlement: drop live buffers. Session view is the fact source.
    pub(crate) fn settle(&mut self) {
        let needs_resync = self.needs_resync;
        *self = Self {
            needs_resync,
            ..Self::default()
        };
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Append `extra` into `buf`, capping at `max_chars`. Returns true if truncated.
fn append_bounded(buf: &mut String, extra: &str, max_chars: usize) -> bool {
    if extra.is_empty() {
        return false;
    }
    let current = buf.chars().count();
    if current >= max_chars {
        return true;
    }
    let room = max_chars - current;
    let extra_chars = extra.chars().count();
    if extra_chars <= room {
        buf.push_str(extra);
        false
    } else {
        buf.extend(extra.chars().take(room));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_capped_and_marked_truncated() {
        let mut live = LiveOperationSnapshot::new();
        live.push_text(&"a".repeat(LIVE_TEXT_CHARS_MAX + 64));
        assert!(live.text_truncated);
        assert!(live.needs_resync);
        assert_eq!(live.text.chars().count(), LIVE_TEXT_CHARS_MAX);
    }

    #[test]
    fn settle_drops_buffers_and_keeps_resync_flag() {
        let mut live = LiveOperationSnapshot::new();
        live.push_text("hello");
        live.mark_lagged();
        live.settle();
        assert!(live.text.is_empty());
        assert!(live.needs_resync);
        assert!(live.operation_id.is_none());
    }

    #[test]
    fn tool_progress_latest_wins() {
        let mut live = LiveOperationSnapshot::new();
        live.set_tool_progress(LiveToolProgress {
            tool_batch_id: "b".into(),
            tool_call_id: "c".into(),
            index: 0,
            tail: "one".into(),
        });
        live.set_tool_progress(LiveToolProgress {
            tool_batch_id: "b".into(),
            tool_call_id: "c".into(),
            index: 0,
            tail: "two".into(),
        });
        assert_eq!(live.tool_progress.len(), 1);
        assert_eq!(live.tool_progress[0].tail, "two");
    }
}
