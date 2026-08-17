//! Per-call display-progress coalescer: bounded live tail, last-wins publish.

use crate::operation::OperationShared;
use crate::{AgentEvent, ToolBatchId, ToolCallId};
use philo_tools::ToolProgressSink;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const PROGRESS_FLUSH_MS: u64 = 50;
pub(crate) const PROGRESS_FLUSH_BYTES: usize = 8 * 1024;
pub(crate) const PROGRESS_MAX_BYTES: usize = 64 * 1024;
pub(crate) const PROGRESS_MAX_LINES: usize = 80;

struct Window {
    text: String,
    pending_bytes: usize,
    last_flush: Option<Instant>,
    dirty: bool,
}

impl Window {
    fn new() -> Self {
        Self {
            text: String::new(),
            pending_bytes: 0,
            last_flush: None,
            dirty: false,
        }
    }

    fn push(&mut self, text: &str) {
        self.text.push_str(text);
        if self.text.len() > PROGRESS_MAX_BYTES
            || bytecount_newlines(&self.text) >= PROGRESS_MAX_LINES
        {
            trim_window(&mut self.text);
        }
        self.pending_bytes = self.pending_bytes.saturating_add(text.len());
        self.dirty = true;
    }

    fn should_flush(&self) -> bool {
        self.dirty
            && (self.last_flush.is_none()
                || self.pending_bytes >= PROGRESS_FLUSH_BYTES
                || self
                    .last_flush
                    .is_some_and(|at| at.elapsed() >= Duration::from_millis(PROGRESS_FLUSH_MS)))
    }

    fn take_tail(&mut self) -> Option<String> {
        if !self.dirty {
            return None;
        }
        self.pending_bytes = 0;
        self.last_flush = Some(Instant::now());
        self.dirty = false;
        Some(self.text.clone())
    }
}

pub(crate) struct ToolProgressBridge {
    state: Arc<Mutex<Window>>,
    shared: Arc<OperationShared>,
    batch_id: ToolBatchId,
    call_id: ToolCallId,
    index: usize,
}

impl ToolProgressBridge {
    pub(crate) fn new(
        shared: Arc<OperationShared>,
        batch_id: ToolBatchId,
        call_id: ToolCallId,
        index: usize,
    ) -> (Self, ToolProgressSink) {
        let state = Arc::new(Mutex::new(Window::new()));
        let sink_state = Arc::clone(&state);
        let sink_shared = Arc::clone(&shared);
        let sink_batch = batch_id.clone();
        let sink_call = call_id.clone();
        let sink = ToolProgressSink::from_fn(move |text| {
            let mut window = sink_state.lock().expect("progress window");
            window.push(text);
            if window.should_flush() {
                if let Some(tail) = window.take_tail() {
                    drop(window);
                    sink_shared.publish_tool_progress(AgentEvent::ToolExecutionProgress {
                        tool_batch_id: sink_batch.clone(),
                        tool_call_id: sink_call.clone(),
                        index,
                        tail,
                    });
                }
            }
        });
        (
            Self {
                state,
                shared,
                batch_id,
                call_id,
                index,
            },
            sink,
        )
    }

    pub(crate) fn finish(self) {
        let mut window = self.state.lock().expect("progress window");
        if let Some(tail) = window.take_tail() {
            drop(window);
            self.shared
                .publish_tool_progress(AgentEvent::ToolExecutionProgress {
                    tool_batch_id: self.batch_id,
                    tool_call_id: self.call_id,
                    index: self.index,
                    tail,
                });
        }
    }
}

fn trim_window(text: &mut String) {
    let newline_count = bytecount_newlines(text);
    let line_count = if text.is_empty() {
        0
    } else if text.ends_with('\n') {
        newline_count
    } else {
        newline_count + 1
    };
    if line_count > PROGRESS_MAX_LINES {
        let skip = line_count - PROGRESS_MAX_LINES;
        let mut seen = 0;
        if let Some(pos) = text.bytes().enumerate().find_map(|(index, byte)| {
            if byte == b'\n' {
                seen += 1;
                if seen == skip { Some(index + 1) } else { None }
            } else {
                None
            }
        }) {
            drain_prefix(text, pos);
        }
    }
    if text.len() > PROGRESS_MAX_BYTES {
        drain_prefix(text, text.len() - PROGRESS_MAX_BYTES);
    }
}

fn bytecount_newlines(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == b'\n').count()
}

fn drain_prefix(text: &mut String, mut bytes: usize) {
    bytes = bytes.min(text.len());
    while bytes < text.len() && !text.is_char_boundary(bytes) {
        bytes += 1;
    }
    text.drain(..bytes);
}
