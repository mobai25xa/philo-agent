//! v4.0 P3 live tool-card tracking: a default-mode `ToolBatchRequested`
//! batch becomes one card cell (single tool) or a tree cell (batch > 1),
//! rewritten in place as started/progress/completed events land. The
//! transcript keeps its older line-based projection for verbose mode.

use std::time::{Duration, Instant};

use philo_agent_service::{FrontendToolDisplay, FrontendToolResult};

use super::run_state::format_card_elapsed;
use super::tool_card;
use super::transcript::{card_cell, CardBody, CardHeader, HeaderPiece, SegColor, SegSpan, TranscriptLine};

/// One in-flight batch. `cell_index` is the card cell (single) or the tree
/// cell (batch > 1); the tree cell is created at `ToolBatchRequested`, the
/// single card at its first `ToolExecutionStarted`.
#[derive(Debug)]
pub(crate) struct LiveBatch {
    pub(crate) total: usize,
    pub(crate) started_at: Instant,
    pub(crate) slots: Vec<LiveSlot>,
    pub(crate) cell_index: Option<usize>,
    #[cfg(test)]
    pub(crate) frozen: Option<Duration>,
}

#[derive(Debug)]
pub(crate) struct LiveSlot {
    pub(crate) index: usize,
    pub(crate) tool_name: String,
    pub(crate) arguments: String,
    pub(crate) started_at: Instant,
    pub(crate) output: String,
    pub(crate) truncated: bool,
    pub(crate) settled: Option<SlotSettle>,
}

#[derive(Debug)]
pub(crate) struct SlotSettle {
    pub(crate) result: FrontendToolResult,
    pub(crate) display: Option<FrontendToolDisplay>,
    /// Cancelled slots settle to `✗ cancelled` (highest priority, §2).
    pub(crate) cancelled: bool,
}

impl LiveBatch {
    pub(crate) fn new(total: usize) -> Self {
        Self {
            total,
            started_at: Instant::now(),
            slots: Vec::new(),
            cell_index: None,
            #[cfg(test)]
            frozen: None,
        }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        #[cfg(test)]
        if let Some(frozen) = self.frozen {
            return frozen;
        }
        self.started_at.elapsed()
    }

    pub(crate) fn slot_elapsed(&self, slot: &LiveSlot) -> Duration {
        #[cfg(test)]
        if let Some(frozen) = self.frozen {
            return frozen;
        }
        slot.started_at.elapsed()
    }

    pub(crate) fn slot(&self, index: usize) -> Option<&LiveSlot> {
        self.slots.iter().find(|slot| slot.index == index)
    }

    pub(crate) fn slot_mut(&mut self, index: usize) -> Option<&mut LiveSlot> {
        self.slots.iter_mut().find(|slot| slot.index == index)
    }

    /// Every announced tool has started and settled (or was cancelled).
    pub(crate) fn all_settled(&self) -> bool {
        self.total > 0
            && self.slots.len() >= self.total
            && self.slots.iter().all(|slot| slot.settled.is_some())
    }

    pub(crate) fn any_failed(&self) -> bool {
        self.slots.iter().any(|slot| {
            slot.settled
                .as_ref()
                .is_some_and(|settle| settle.cancelled || matches!(settle.result, FrontendToolResult::Error { .. }))
        })
    }

    /// Pins the rendered elapsed so snapshots stay deterministic.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn freeze(&mut self, elapsed: Duration) {
        self.frozen = Some(elapsed);
    }
}

/// The live card cell: the single running card or the concurrent tree.
pub(crate) fn live_cell(batch: &LiveBatch, spinner: &str) -> TranscriptLine {
    if batch.total == 1 {
        let slot = batch.slots.first().expect("single batch has one slot");
        tool_card::running_cell(
            &slot.tool_name,
            &slot.arguments,
            &slot.output,
            slot.truncated,
            spinner,
            batch.slot_elapsed(slot),
        )
    } else {
        tree_cell(batch, spinner)
    }
}

/// The concurrent tree, one cell (§5): parent header plus child mini-card
/// lines, foldable as a whole after the batch ends.
fn tree_cell(batch: &LiveBatch, spinner: &str) -> TranscriptLine {
    let children = batch
        .slots
        .iter()
        .map(|slot| child_row(slot, batch, spinner))
        .collect::<Vec<_>>();
    let child_count = children.len();
    let (bar, status) = if batch.all_settled() {
        if batch.any_failed() {
            (SegColor::Red, "✗ failed")
        } else {
            (SegColor::Green, "✓ done")
        }
    } else {
        (SegColor::Yellow, spinner)
    };
    let header = CardHeader {
        bar: HeaderPiece {
            text: "▎".to_owned(),
            color: bar,
            bold: false,
        },
        action: HeaderPiece {
            text: format!("Parallel Task ({} operations)", batch.total),
            color: SegColor::Gray,
            bold: true,
        },
        target: None,
        stats: None,
        status: HeaderPiece {
            text: status.to_owned(),
            color: bar,
            bold: false,
        },
        time: Some(HeaderPiece {
            text: format_card_elapsed(batch.elapsed()),
            color: SegColor::DarkGray,
            bold: false,
        }),
    };
    card_cell(
        header,
        CardBody {
            lines: children,
            threshold: 1,
            fold_default_collapsed: false,
            fold_count: child_count,
            fold_label: "operations 已折叠".to_owned(),
            fold_hint: false,
            fold_all: true,
        },
    )
}

/// One child mini-card row: `├─ ▎ {action} {target} {status} {time}` (last
/// child wears `└─`). The `▎` bar and status are colored by child state.
fn child_row(slot: &LiveSlot, batch: &LiveBatch, spinner: &str) -> Vec<SegSpan> {
    let last = batch.slots.last().is_some_and(|last| last.index == slot.index);
    let (bar, status, status_color, time) = match &slot.settled {
        Some(settle) if settle.cancelled => (
            SegColor::Red,
            "✗ cancelled".to_owned(),
            SegColor::Red,
            None,
        ),
        Some(settle) => {
            let (color, word, _) = tool_card::state_for(&settle.result, settle.display.as_ref());
            (
                color,
                word.to_owned(),
                color,
                Some(format_card_elapsed(batch.slot_elapsed(slot))),
            )
        }
        None => (
            SegColor::Yellow,
            spinner.to_owned(),
            SegColor::Yellow,
            Some(format_card_elapsed(batch.slot_elapsed(slot))),
        ),
    };
    let mut segs = vec![
        SegSpan::colored(if last { "└─ " } else { "├─ " }, SegColor::Gray),
        SegSpan::colored("▎ ", bar),
        SegSpan {
            text: slot.tool_name.clone(),
            color: SegColor::Gray,
            bold: true,
            tone: None,
        },
    ];
    if let Some((target, kind)) = tool_card::preview_target(&slot.arguments) {
        segs.push(SegSpan::plain(" "));
        segs.push(SegSpan::colored(target, kind));
    }
    if !status.is_empty() {
        segs.push(SegSpan::plain(" "));
        segs.push(SegSpan::colored(status, status_color));
    }
    if let Some(time) = time {
        segs.push(SegSpan::plain(" "));
        segs.push(SegSpan::colored(time, SegColor::DarkGray));
    }
    segs
}