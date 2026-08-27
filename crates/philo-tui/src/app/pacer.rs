//! Stream pacer: the smoothing valve between model deltas and the
//! transcript (v2.2, plan T4.11).
//!
//! Deltas land in a backlog instead of writing cells directly; animation
//! ticks release characters at a rate proportional to the backlog
//! (`rate = clamp(backlog / TARGET_LATENCY, MIN, MAX)`), so a burst from
//! the model unspools evenly instead of landing as one block while a slow
//! trickle still advances visibly every tick. Pure tick counting — no wall
//! clock — keeps snapshots deterministic (plan risk 7).
//!
//! Order and kind are preserved: entries replay through the same reducer
//! paths as live events, so think/answer transitions close cells exactly
//! as unpaced delivery would. Structural events flush the backlog first,
//! which makes cancellation (`esc`) reveal everything instantly.

use std::collections::VecDeque;
use std::time::Duration;

use super::transcript::LineKind;

/// Backlog size at which the drain rate reaches ~1× the backlog per
/// latency window: smaller backlogs drain slower, larger ones faster.
pub(crate) const TARGET_LATENCY: Duration = Duration::from_millis(350);
/// Drain floor (chars/s) so a trickle still advances visibly each tick.
pub(crate) const MIN_CHARS_PER_SEC: usize = 120;
/// Drain ceiling (chars/s) so huge bursts never lag meaningfully behind.
pub(crate) const MAX_CHARS_PER_SEC: usize = 6000;

/// One paced streaming piece: the transcript kind it belongs to plus the
/// characters released for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PacedPiece {
    pub(crate) kind: LineKind,
    pub(crate) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Chunk {
    kind: LineKind,
    chars: Vec<char>,
}

/// Backlog of not-yet-displayed stream characters.
///
/// The rate controller is pure integer arithmetic (micros of char budget):
/// identical event + tick sequences release identical character counts on
/// every platform, which is what makes the paced snapshots deterministic.
#[derive(Debug, Default)]
pub(crate) struct StreamPacer {
    queue: VecDeque<Chunk>,
    /// Unspent budget carried across ticks, in millionths of a character.
    owed_micro_chars: u64,
}

impl StreamPacer {
    /// Queues one delta. Callers forward only live streaming kinds here;
    /// reasoning deltas dropped by `[ui].show_reasoning=false` never reach
    /// the pacer.
    pub(crate) fn push(&mut self, kind: LineKind, text: &str) {
        if text.is_empty() {
            return;
        }
        debug_assert!(
            matches!(kind, LineKind::Answer | LineKind::Reasoning),
            "the pacer carries only streaming kinds"
        );
        self.queue.push_back(Chunk {
            kind,
            chars: text.chars().collect(),
        });
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Total buffered characters — the rate controller's input.
    pub(crate) fn backlog_chars(&self) -> usize {
        self.queue.iter().map(|chunk| chunk.chars.len()).sum()
    }

    /// Releases characters due after `dt` of pacing, preserving order and
    /// splitting only the head chunk. Empty result means nothing to paint.
    pub(crate) fn drain(&mut self, dt: Duration) -> Vec<PacedPiece> {
        let backlog = self.backlog_chars();
        if backlog == 0 || dt.is_zero() {
            self.owed_micro_chars = 0;
            return Vec::new();
        }
        let latency = (TARGET_LATENCY.as_millis() as u64).max(1); // ms
        let dt_ms = dt.as_millis() as u64;
        let cps = (backlog as u64 * 1000 / latency)
            .clamp(MIN_CHARS_PER_SEC as u64, MAX_CHARS_PER_SEC as u64);
        // Budget in millionths of a character: cps * dt_ms * 1000.
        let budget = self.owed_micro_chars + cps.saturating_mul(dt_ms).saturating_mul(1000);
        let whole_chars = (budget / 1_000_000) as usize;

        let mut pieces: Vec<PacedPiece> = Vec::new();
        let mut left = whole_chars;
        while left > 0 {
            let Some(head) = self.queue.front_mut() else {
                self.owed_micro_chars = 0;
                return pieces;
            };
            let take = head.chars.len().min(left);
            let rest = head.chars.split_off(take);
            let text: String = std::mem::replace(&mut head.chars, rest)
                .into_iter()
                .collect();
            let kind = head.kind;
            left -= take;
            if head.chars.is_empty() {
                self.queue.pop_front();
            }
            pieces.push(PacedPiece { kind, text });
        }
        self.owed_micro_chars = if self.queue.is_empty() {
            0
        } else {
            budget % 1_000_000
        };
        pieces
    }

    /// Emits everything immediately in queue order (structural boundary,
    /// cancellation, session switch teardown upstream).
    pub(crate) fn flush(&mut self) -> Vec<PacedPiece> {
        self.owed_micro_chars = 0;
        self.queue
            .drain(..)
            .map(|chunk| PacedPiece {
                kind: chunk.kind,
                text: chunk.chars.into_iter().collect(),
            })
            .collect()
    }

    /// Drops the backlog without emitting (session switches: the store is
    /// cleared with it).
    pub(crate) fn clear(&mut self) {
        self.queue.clear();
        self.owed_micro_chars = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drained_text(pieces: &[PacedPiece]) -> String {
        pieces.iter().map(|piece| piece.text.as_str()).collect()
    }

    #[test]
    fn bursts_release_proportional_chunks_instead_of_landing_as_a_block() {
        let mut pacer = StreamPacer::default();
        pacer.push(LineKind::Answer, &"x".repeat(1000));
        let tick = Duration::from_millis(100);
        // First tick: 1000 backlog / 350ms latency → 2857 cps → 285 chars,
        // with the sub-character remainder carried as micro-char credit.
        let first = drained_text(&pacer.drain(tick));
        assert_eq!(first.chars().count(), 285);

        // The controller retargets each tick, so the rate tapers with the
        // backlog while never stalling above the floor…
        let mut counts = vec![first.chars().count()];
        while !pacer.is_empty() {
            counts.push(drained_text(&pacer.drain(tick)).chars().count());
        }
        assert!(
            counts.windows(2).all(|window| window[0] >= window[1]),
            "releases taper monotonically: {counts:?}"
        );
        // …and nothing is lost or duplicated.
        assert_eq!(counts.iter().sum::<usize>(), 1000);
    }

    #[test]
    fn trickles_advance_at_the_floor_rate() {
        let mut pacer = StreamPacer::default();
        pacer.push(LineKind::Answer, "abcdef");
        // 6 backlog / 0.35s ≈ 17 cps → floor 120 cps → 12 chars/s… per
        // 100ms tick that is 12 chars of credit minus none: the whole
        // six-char chunk fits under one tick's floor release.
        let pieces = pacer.drain(Duration::from_millis(100));
        assert_eq!(drained_text(&pieces), "abcdef");
        assert!(pacer.is_empty());
    }

    #[test]
    fn order_and_kind_survive_mixed_streams() {
        let mut pacer = StreamPacer::default();
        pacer.push(LineKind::Reasoning, "th ink ");
        pacer.push(LineKind::Answer, "hi");

        let mut seen = Vec::new();
        while !pacer.is_empty() {
            for piece in pacer.drain(Duration::from_millis(50)) {
                seen.push(piece.kind);
            }
        }
        assert_eq!(
            seen.first(),
            Some(&LineKind::Reasoning),
            "think drains before the answer"
        );
        assert_eq!(seen.last(), Some(&LineKind::Answer));
    }

    #[test]
    fn flush_emits_everything_in_order_and_resets_credit() {
        let mut pacer = StreamPacer::default();
        pacer.push(LineKind::Reasoning, "think");
        pacer.push(LineKind::Answer, "answer");
        let pieces = pacer.flush();
        assert_eq!(pieces.len(), 2);
        assert_eq!(
            (pieces[0].kind, pieces[0].text.clone()),
            (LineKind::Reasoning, "think".to_owned())
        );
        assert_eq!(pieces[1].text, "answer");
        assert!(pacer.is_empty());
        assert_eq!(pacer.backlog_chars(), 0);
    }

    #[test]
    fn clear_drops_without_emitting() {
        let mut pacer = StreamPacer::default();
        pacer.push(LineKind::Answer, "text");
        pacer.clear();
        assert!(pacer.is_empty());
        assert!(pacer.drain(Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn empty_and_zero_dt_are_noops() {
        let mut pacer = StreamPacer::default();
        assert!(pacer.drain(Duration::from_secs(1)).is_empty());
        pacer.push(LineKind::Answer, "abc");
        assert!(pacer.drain(Duration::ZERO).is_empty());
        assert_eq!(pacer.backlog_chars(), 3, "nothing was consumed");
    }
}
