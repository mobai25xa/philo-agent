//! Run-state word machine: the single ephemeral word in the composer's
//! top-left corner (redesign §2.5, tui.md §8).
//!
//! One word, one timer. Events map straight onto phases with no debounce;
//! tool gaps may pass through `Waiting` briefly — determinism beats gloss.
//! The projection never enters the transcript or the Session.

use std::time::Duration;

use philo_agent_service::FrontendOperationEvent;
use unicode_segmentation::UnicodeSegmentation;

use super::text;
use crate::render::theme::{ELLIPSIS, SPINNER_FRAMES};

/// The seven run phases behind the state word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Waiting,
    Thinking,
    Writing,
    Running,
    Retrying,
    Compacting,
    Cancelling,
}

impl Phase {
    /// Sticky priority chain: `Cancelling > Compacting > Running >
    /// Thinking/Writing > Waiting/Retrying`. A lower phase never overrides
    /// a higher one; equal levels replace (last event wins).
    fn priority(self) -> u8 {
        match self {
            Self::Cancelling => 5,
            Self::Compacting => 4,
            Self::Running => 3,
            Self::Thinking | Self::Writing => 2,
            Self::Waiting | Self::Retrying => 1,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Waiting => "Waiting",
            Self::Thinking => "Thinking",
            Self::Writing => "Writing",
            Self::Running => "Running",
            Self::Retrying => "Retrying",
            Self::Compacting => "Compacting",
            Self::Cancelling => "Cancelling",
        }
    }
}

/// Turn timer: starts when an operation first occupies the corner, survives
/// every phase change, and stops at settlement. Production reads the wall
/// clock; tests freeze it for deterministic snapshots (plan T3.4).
#[derive(Debug, Default)]
struct RunClock {
    started_at: Option<std::time::Instant>,
    #[cfg(test)]
    frozen: Option<Duration>,
}

impl RunClock {
    fn start(&mut self) {
        if self.started_at.is_none() {
            self.started_at = Some(std::time::Instant::now());
        }
    }

    fn stop(&mut self) {
        self.started_at = None;
        #[cfg(test)]
        {
            self.frozen = None;
        }
    }

    fn elapsed(&self) -> Option<Duration> {
        #[cfg(test)]
        if self.frozen.is_some() {
            return self.frozen;
        }
        self.started_at.map(|started| started.elapsed())
    }

    #[cfg(test)]
    fn freeze(&mut self, elapsed: Duration) {
        self.frozen = Some(elapsed);
    }
}

/// Ephemeral operation projection driving the top-left corner.
#[derive(Debug, Default)]
pub(crate) struct RunState {
    phase: Option<Phase>,
    clock: RunClock,
    spinner: usize,
    /// Tool executions opened but not yet completed; the last completion
    /// opens the contract's `Waiting` gap between batches.
    live_tools: Vec<usize>,
}

impl RunState {
    pub(crate) fn is_active(&self) -> bool {
        self.phase.is_some()
    }

    /// Terminal events and session switches clear the word and the timer.
    pub(crate) fn clear(&mut self) {
        self.phase = None;
        self.live_tools.clear();
        self.clock.stop();
        self.spinner = 0;
    }

    /// Braille spinner frame — the only animated element in the UI.
    pub(crate) fn advance_spinner(&mut self) {
        if self.is_active() {
            self.spinner = (self.spinner + 1) % SPINNER_FRAMES.len();
        }
    }

    /// Manual `/compact` takes the corner unconditionally (it is only
    /// reachable while idle).
    pub(crate) fn start_manual_compaction(&mut self) {
        if self.phase.is_none() {
            self.clock.start();
        }
        self.live_tools.clear();
        self.phase = Some(Phase::Compacting);
    }

    /// Busy with no word yet (availability flip): show the baseline unless
    /// a sticky higher phase already owns the corner.
    pub(crate) fn ensure_waiting(&mut self) {
        self.set(Phase::Waiting);
    }

    /// Manual compaction settles to an empty corner; automatic compaction
    /// instead returns to `Waiting` via [`Self::on_event`].
    pub(crate) fn finish_manual_compaction(&mut self) {
        if self.phase == Some(Phase::Compacting) {
            self.clear();
        }
    }

    pub(crate) fn on_event(&mut self, event: &FrontendOperationEvent) {
        match event {
            FrontendOperationEvent::OperationQueued { .. } => {
                // queued 不展示: enqueue hints live only in transcript meta.
            }
            FrontendOperationEvent::OperationStarted { .. }
            | FrontendOperationEvent::TurnStarted { .. }
            | FrontendOperationEvent::ModelCallStarted { .. }
            | FrontendOperationEvent::ModelResponseStarted { .. }
            | FrontendOperationEvent::AssistantMessageCompleted { .. } => {
                // Contract-mandated `Waiting…` entry points: structural
                // boundaries demote streaming/tool words, but never pull
                // the corner off an active Compacting or sticky Cancelling.
                self.demote_to_waiting();
            }
            FrontendOperationEvent::ReasoningDelta { .. } => self.set(Phase::Thinking),
            FrontendOperationEvent::TextDelta { .. } => self.set(Phase::Writing),
            FrontendOperationEvent::ToolBatchRequested { .. } => {
                // Batch announcements never move the word.
            }
            FrontendOperationEvent::ToolExecutionStarted { index, .. } => {
                if !self.live_tools.contains(index) {
                    self.live_tools.push(*index);
                }
                self.set(Phase::Running);
            }
            // Progress produces no display and no state change (§2.5).
            FrontendOperationEvent::ToolExecutionProgress { .. } => {}
            FrontendOperationEvent::ToolExecutionCompleted { index, .. } => {
                self.live_tools.retain(|live| live != index);
                // The gap after the final tool demotes explicitly, but never
                // past a sticky `Cancelling`.
                if self.live_tools.is_empty() && self.phase == Some(Phase::Running) {
                    self.replace(Phase::Waiting);
                }
            }
            FrontendOperationEvent::ContextCompactionStarted => self.set(Phase::Compacting),
            FrontendOperationEvent::ContextCompactionCompleted { .. }
            | FrontendOperationEvent::ContextCompactionFailed { .. } => {
                // Automatic compaction settles back to the baseline.
                if self.phase == Some(Phase::Compacting) {
                    self.replace(Phase::Waiting);
                }
            }
            FrontendOperationEvent::CancellationRequested { .. }
            | FrontendOperationEvent::TurnCancelled { .. } => {
                self.replace(Phase::Cancelling);
            }
            FrontendOperationEvent::ModelRetryScheduled { .. } => self.set(Phase::Retrying),
            FrontendOperationEvent::TurnFailed { .. }
            | FrontendOperationEvent::OperationSettled { .. } => self.clear(),
            FrontendOperationEvent::ModelUsageUpdated { .. }
            | FrontendOperationEvent::PriorTurnSealed { .. } => {}
        }
    }

    /// Priority-guarded transition; starts the turn clock on first use.
    fn set(&mut self, next: Phase) {
        if self
            .phase
            .is_some_and(|current| current.priority() > next.priority())
        {
            return;
        }
        if self.phase.is_none() {
            self.clock.start();
        }
        self.phase = Some(next);
    }

    /// Unguarded replacement for explicit demotions (tool gaps, compaction
    /// settlement) and the sticky cancel takeover.
    fn replace(&mut self, next: Phase) {
        if self.phase.is_none() {
            self.clock.start();
        }
        self.phase = Some(next);
    }

    /// Structural boundaries (`OperationStarted`…`AssistantMessageCompleted`)
    /// land the corner on the baseline unless a higher sticky phase owns it.
    fn demote_to_waiting(&mut self) {
        if self
            .phase
            .is_some_and(|current| current.priority() <= Phase::Running.priority())
        {
            self.replace(Phase::Waiting);
        } else if self.phase.is_none() {
            self.set(Phase::Waiting);
        }
    }

    /// Composer top-left corner content (§2.4): `⠹ {State}… {elapsed} · esc
    /// cancel`, degrading deterministically — drop `· esc cancel` first,
    /// then truncate the word (§3.10). `approval` swaps in the overlay flag
    /// word without touching the underlying phase.
    pub(crate) fn corner(&self, max_width: usize, approval: bool) -> Option<CornerWord> {
        let phase = self.phase?;
        let spinner = SPINNER_FRAMES[self.spinner % SPINNER_FRAMES.len()].to_owned();
        let word = if approval {
            format!("Approval{ELLIPSIS}")
        } else {
            format!("{}{ELLIPSIS}", phase.word())
        };
        let timing = match self.clock.elapsed() {
            Some(elapsed) => format!("{} · esc cancel", format_elapsed(elapsed)),
            None => String::new(),
        };
        let mut corner = CornerWord {
            spinner,
            word,
            timing,
            warning: phase == Phase::Cancelling,
        };
        corner.degrade(max_width);
        Some(corner)
    }

    /// Current turn-elapsed reading, if a turn owns the clock. The App peeks
    /// this just before terminal events stop the clock so the settlement
    /// line can cite the duration (design §2.4).
    pub(crate) fn elapsed(&self) -> Option<Duration> {
        self.clock.elapsed()
    }

    /// Pins the rendered elapsed so snapshots stay deterministic (T3.4).
    #[cfg(test)]
    pub(crate) fn freeze_elapsed(&mut self, elapsed: Duration) {
        self.clock.freeze(elapsed);
    }
}

/// Width-aware top-left corner projection consumed by the composer band.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CornerWord {
    pub(crate) spinner: String,
    pub(crate) word: String,
    /// `{elapsed} · esc cancel`; empty once degradation drops it.
    pub(crate) timing: String,
    /// `Cancelling…` wears warn; every other word stays primary.
    pub(crate) warning: bool,
}

impl CornerWord {
    fn width(&self) -> usize {
        let mut width = text::width(&self.spinner) + 1 + text::width(&self.word);
        if !self.timing.is_empty() {
            width += 1 + text::width(&self.timing);
        }
        width
    }

    fn degrade(&mut self, max_width: usize) {
        if self.width() <= max_width {
            return;
        }
        self.timing = String::new();
        if text::width(&self.spinner) + 1 + text::width(&self.word) <= max_width {
            return;
        }
        self.word = truncate_word(&self.word, max_width.saturating_sub(2));
    }

    pub(crate) fn painted_width(&self) -> usize {
        self.width()
    }
}

/// `≥1m` renders as `1m23s`; below that a bare `42s`. Shared with the
/// transcript's think headers and settlement duration lines.
pub(crate) fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Think-header durations (v2.2): sub-second spans render as milliseconds
/// (`850ms`) so short blocks never collapse into a meaningless `0s`; from a
/// second up, the shared whole-second form applies (`8s`, `1m23s`). Only
/// the think header uses this — corner timer and settlement lines stay
/// whole-second.
pub(crate) fn format_think_elapsed(elapsed: Duration) -> String {
    if elapsed.as_secs() == 0 {
        format!("{}ms", elapsed.as_millis())
    } else {
        format_elapsed(elapsed)
    }
}

/// Head-keeping truncation with the design's `…` marker.
fn truncate_word(word: &str, max_width: usize) -> String {
    if text::width(word) <= max_width {
        return word.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width.saturating_sub(text::width(ELLIPSIS));
    let mut head = String::new();
    let mut used = 0;
    for grapheme in word.graphemes(true) {
        let width = text::width(grapheme);
        if used + width > budget {
            break;
        }
        head.push_str(grapheme);
        used += width;
    }
    format!("{head}{ELLIPSIS}")
}

#[cfg(test)]
mod tests {
    use philo_agent_service::FrontendFailure;

    use super::*;

    fn op_started() -> FrontendOperationEvent {
        FrontendOperationEvent::OperationStarted {
            operation_id: "op".to_owned(),
        }
    }

    fn reasoning() -> FrontendOperationEvent {
        FrontendOperationEvent::ReasoningDelta {
            model_call_id: "call".to_owned(),
            text: "t".to_owned(),
        }
    }

    fn writing() -> FrontendOperationEvent {
        FrontendOperationEvent::TextDelta {
            delta: "t".to_owned(),
        }
    }

    fn tool_started(index: usize) -> FrontendOperationEvent {
        FrontendOperationEvent::ToolExecutionStarted {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: format!("call-{index}"),
            index,
            tool_name: "read".to_owned(),
            arguments: "{}".to_owned(),
        }
    }

    fn tool_progress(index: usize) -> FrontendOperationEvent {
        FrontendOperationEvent::ToolExecutionProgress {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: format!("call-{index}"),
            index,
            tail: "out".to_owned(),
        }
    }

    fn tool_completed(index: usize) -> FrontendOperationEvent {
        FrontendOperationEvent::ToolExecutionCompleted {
            tool_batch_id: "batch".to_owned(),
            tool_call_id: format!("call-{index}"),
            index,
            tool_name: "read".to_owned(),
            result: philo_agent_service::FrontendToolResult::Success {
                content: "ok".to_owned(),
            },
            display: None,
        }
    }

    fn cancelled() -> FrontendOperationEvent {
        FrontendOperationEvent::CancellationRequested {
            operation_id: "op".to_owned(),
            reason: "User".to_owned(),
        }
    }

    fn settled() -> FrontendOperationEvent {
        FrontendOperationEvent::OperationSettled {
            operation_id: "op".to_owned(),
            session_id: "s".to_owned(),
            status: "Cancelled".to_owned(),
            durability: "Confirmed".to_owned(),
            session_revision: philo_agent_service::SettlementRevision::Unchanged,
        }
    }

    fn retry() -> FrontendOperationEvent {
        FrontendOperationEvent::ModelRetryScheduled {
            model_call_id: "call".to_owned(),
            attempt: 1,
            max_retries: 3,
            delay_ms: 2000,
            failure: failure(),
        }
    }

    fn failure() -> FrontendFailure {
        FrontendFailure {
            code: "network.timeout".to_owned(),
            domain: "network".to_owned(),
            stage: "model-port".to_owned(),
            retry: "safe".to_owned(),
            summary: "upstream timed out".to_owned(),
            diagnostic: String::new(),
        }
    }

    fn word_of(state: &RunState, approval: bool) -> Option<String> {
        state.corner(120, approval).map(|corner| corner.word)
    }

    #[test]
    fn queued_never_activates_the_corner() {
        let mut state = RunState::default();
        state.on_event(&FrontendOperationEvent::OperationQueued {
            operation_id: "op".to_owned(),
        });
        assert!(!state.is_active());
        assert!(word_of(&state, false).is_none(), "idle stays empty");
    }

    #[test]
    fn baseline_words_follow_the_last_event() {
        let mut state = RunState::default();
        state.on_event(&op_started());
        assert_eq!(word_of(&state, false).as_deref(), Some("Waiting…"));
        state.on_event(&reasoning());
        assert_eq!(word_of(&state, false).as_deref(), Some("Thinking…"));
        state.on_event(&writing());
        assert_eq!(word_of(&state, false).as_deref(), Some("Writing…"));
        state.on_event(&FrontendOperationEvent::AssistantMessageCompleted {
            turn_id: "turn".to_owned(),
            content: "done".to_owned(),
        });
        assert_eq!(word_of(&state, false).as_deref(), Some("Waiting…"));
    }

    #[test]
    fn tools_run_as_one_word_and_progress_is_invisible() {
        let mut state = RunState::default();
        state.on_event(&op_started());
        state.on_event(&FrontendOperationEvent::ToolBatchRequested {
            tool_batch_id: "batch".to_owned(),
            call_count: 2,
        });
        assert_eq!(
            word_of(&state, false).as_deref(),
            Some("Waiting…"),
            "batch requests keep the current word"
        );

        state.on_event(&tool_started(0));
        assert_eq!(word_of(&state, false).as_deref(), Some("Running…"));
        state.on_event(&tool_progress(0));
        assert_eq!(
            word_of(&state, false).as_deref(),
            Some("Running…"),
            "progress changes nothing"
        );
        state.on_event(&tool_progress(1));
        state.on_event(&tool_started(1));
        assert_eq!(word_of(&state, false).as_deref(), Some("Running…"));

        state.on_event(&tool_completed(0));
        assert_eq!(
            word_of(&state, false).as_deref(),
            Some("Running…"),
            "one tool still runs"
        );
        state.on_event(&tool_completed(1));
        assert_eq!(
            word_of(&state, false).as_deref(),
            Some("Waiting…"),
            "the gap after all tools lands on Waiting"
        );
        state.on_event(&tool_completed(0));
        assert_eq!(
            word_of(&state, false).as_deref(),
            Some("Waiting…"),
            "stray completions do not loop the demotion"
        );
    }

    #[test]
    fn retry_sits_with_waiting_and_loses_to_running() {
        let mut state = RunState::default();
        state.on_event(&op_started());
        state.on_event(&retry());
        assert_eq!(word_of(&state, false).as_deref(), Some("Retrying…"));
        state.on_event(&FrontendOperationEvent::ModelCallStarted {
            model_call_id: "call".to_owned(),
        });
        assert_eq!(
            word_of(&state, false).as_deref(),
            Some("Waiting…"),
            "equal levels replace"
        );

        state.on_event(&tool_started(0));
        state.on_event(&retry());
        assert_eq!(
            word_of(&state, false).as_deref(),
            Some("Running…"),
            "retry cannot beat running"
        );
    }

    #[test]
    fn streaming_loses_to_running_but_compacting_beats_both() {
        let mut state = RunState::default();
        state.on_event(&op_started());
        state.on_event(&tool_started(0));
        state.on_event(&writing());
        assert_eq!(
            word_of(&state, false).as_deref(),
            Some("Running…"),
            "writing cannot beat running"
        );

        state.on_event(&FrontendOperationEvent::ContextCompactionStarted);
        assert_eq!(word_of(&state, false).as_deref(), Some("Compacting…"));
        state.on_event(&writing());
        assert_eq!(
            word_of(&state, false).as_deref(),
            Some("Compacting…"),
            "streaming cannot beat compacting"
        );
        state.on_event(&FrontendOperationEvent::ContextCompactionCompleted {
            covers_up_to: "entry-42".to_owned(),
        });
        assert_eq!(word_of(&state, false).as_deref(), Some("Waiting…"));

        state.on_event(&reasoning());
        state.on_event(&FrontendOperationEvent::ContextCompactionStarted);
        state.on_event(&FrontendOperationEvent::ContextCompactionFailed {
            message: "no".to_owned(),
        });
        assert_eq!(word_of(&state, false).as_deref(), Some("Waiting…"));
    }

    #[test]
    fn cancelling_is_sticky_until_settlement() {
        let mut state = RunState::default();
        state.on_event(&op_started());
        state.on_event(&writing());
        state.on_event(&cancelled());
        assert_eq!(word_of(&state, false).as_deref(), Some("Cancelling…"));

        for event in [
            writing(),
            reasoning(),
            op_started(),
            tool_started(3),
            tool_completed(3),
            retry(),
            FrontendOperationEvent::ContextCompactionStarted,
            FrontendOperationEvent::ContextCompactionCompleted {
                covers_up_to: "x".to_owned(),
            },
            FrontendOperationEvent::TurnCancelled {
                turn_id: "turn".to_owned(),
                reason: "User".to_owned(),
            },
        ] {
            state.on_event(&event);
            assert_eq!(
                word_of(&state, false).as_deref(),
                Some("Cancelling…"),
                "nothing overrides a sticky cancel: {event:?}"
            );
        }

        state.on_event(&settled());
        assert!(!state.is_active(), "settlement clears even Cancelling");
    }

    #[test]
    fn terminal_events_clear_the_word_and_timer() {
        let mut state = RunState::default();
        state.on_event(&op_started());
        state.freeze_elapsed(Duration::from_secs(5));
        state.on_event(&settled());
        assert!(word_of(&state, false).is_none());

        state.on_event(&FrontendOperationEvent::TurnFailed {
            turn_id: "turn".to_owned(),
            failure: failure(),
        });
        assert!(!state.is_active());

        state.on_event(&op_started());
        let corner = state.corner(120, false).expect("restarted");
        assert_eq!(corner.timing, "0s · esc cancel");
    }

    #[test]
    fn manual_compaction_takes_and_gives_the_corner() {
        let mut state = RunState::default();
        state.start_manual_compaction();
        assert_eq!(word_of(&state, false).as_deref(), Some("Compacting…"));
        state.advance_spinner();
        state.finish_manual_compaction();
        assert!(!state.is_active(), "manual compaction settles empty");

        state.finish_manual_compaction();
        assert!(!state.is_active(), "finishing while idle is inert");
    }

    #[test]
    fn the_turn_clock_spans_phase_changes_and_stops_once() {
        let mut state = RunState::default();
        state.on_event(&op_started());
        state.freeze_elapsed(Duration::from_secs(42));
        state.on_event(&tool_started(0));
        let corner = state.corner(120, false).expect("running");
        assert_eq!(corner.timing, "42s · esc cancel");

        state.on_event(&cancelled());
        let corner = state.corner(120, false).expect("cancelling");
        assert_eq!(corner.timing, "42s · esc cancel");
        assert!(corner.warning, "Cancelling… wears warn");

        state.on_event(&settled());
        assert!(state.corner(120, false).is_none());
    }

    #[test]
    fn approval_overlays_the_word_without_touching_the_phase() {
        let mut state = RunState::default();
        state.on_event(&op_started());
        state.on_event(&tool_started(0));
        let corner = state.corner(120, true).expect("approval overlay");
        assert_eq!(corner.word, "Approval…");
        assert!(!corner.warning);

        let bare = state.corner(120, false).expect("underlying");
        assert_eq!(bare.word, "Running…", "the flag hides, not replaces");
    }

    #[test]
    fn elapsed_formats_seconds_then_minutes() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(42)), "42s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(83)), "1m23s");
        assert_eq!(format_elapsed(Duration::from_secs(600)), "10m0s");
    }

    #[test]
    fn think_elapsed_wears_milliseconds_below_a_second() {
        assert_eq!(
            format_think_elapsed(Duration::from_millis(850)),
            "850ms",
            "short blocks never collapse to 0s"
        );
        assert_eq!(format_think_elapsed(Duration::from_millis(0)), "0ms");
        assert_eq!(format_think_elapsed(Duration::from_secs(8)), "8s");
        assert_eq!(format_think_elapsed(Duration::from_secs(83)), "1m23s");
    }

    #[test]
    fn the_corner_degrades_timing_first_then_truncates_the_word() {
        let mut state = RunState::default();
        state.on_event(&op_started());
        state.on_event(&writing());
        state.freeze_elapsed(Duration::from_secs(42));

        let full = state.corner(80, false).expect("wide");
        assert_eq!(full.spinner, "⠋");
        assert_eq!(full.word, "Writing…");
        assert_eq!(full.timing, "42s · esc cancel");
        assert_eq!(
            full.painted_width(),
            text::width("⠹") + 1 + text::width("Writing…") + 1 + text::width("42s · esc cancel")
        );

        let squeezed = state
            .corner(full.painted_width() - 1, false)
            .expect("squeezed");
        assert!(squeezed.timing.is_empty(), "timing drops first");
        assert_eq!(squeezed.word, "Writing…");

        let tighter = state.corner(6, false).expect("tight");
        assert!(tighter.timing.is_empty());
        assert!(
            tighter.painted_width() <= 6,
            "the word truncates into budget: {:?}",
            tighter.word
        );
        assert!(tighter.word.ends_with(ELLIPSIS));

        let floor = state.corner(2, false).expect("floor");
        assert_eq!(floor.word, "", "a two-cell budget leaves the spinner");
    }

    #[test]
    fn the_spinner_advances_only_while_active_and_wraps() {
        let mut state = RunState::default();
        state.advance_spinner();
        assert_eq!(
            state.corner(80, false).map(|corner| corner.spinner),
            None,
            "idle has no frames to advance"
        );

        state.on_event(&op_started());
        let first = state.corner(80, false).expect("active").spinner;
        assert_eq!(first, SPINNER_FRAMES[0]);
        for expected in 1..=SPINNER_FRAMES.len() {
            state.advance_spinner();
            let frame = state.corner(80, false).expect("active").spinner;
            assert_eq!(frame, SPINNER_FRAMES[expected % SPINNER_FRAMES.len()]);
        }

        state.on_event(&settled());
        assert_eq!(state.corner(80, false), None);
    }
}
