//! One scheduling round's terminal writes: optional hard clear and at most
//! one draw of the isolated alternate screen. History lives in `App.cells`,
//! not here.

use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::time::Instant;

use crate::app::state::App;
use crate::render::frame;

use super::scheduler::FrameScheduler;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FlushReport {
    pub(crate) clears: usize,
    pub(crate) inserts: usize,
    pub(crate) draws: usize,
    pub(crate) failed: bool,
}

#[derive(Default)]
pub(crate) struct PendingOutput;

impl PendingOutput {
    /// Flushes only when the scheduler grants a frame. History is painted
    /// from `App.cells` inside `frame::draw`; this type never writes
    /// `insert_before`. Draw errors keep dirty and never abort the process.
    pub(crate) fn flush<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &App,
        shift_enter: bool,
        scheduler: &mut FrameScheduler,
        now: Instant,
    ) -> FlushReport {
        let Some(permit) = scheduler.prepare_frame(now) else {
            return FlushReport::default();
        };

        let mut report = FlushReport::default();
        // Backend draws move the physical cursor through every changed cell.
        // Keep it hidden until ratatui restores the composer cursor at the end
        // of this frame, otherwise streaming diffs visibly drag it around.
        if let Err(error) = terminal.hide_cursor() {
            scheduler.retry_frame(permit, error);
            report.failed = true;
            return report;
        }
        if permit.hard_clear {
            if let Err(error) = terminal.clear() {
                scheduler.retry_frame(permit, error);
                report.failed = true;
                return report;
            }
            report.clears = 1;
        }
        match terminal.draw(|terminal_frame| {
            frame::draw(terminal_frame, app, shift_enter);
        }) {
            Ok(_) => {
                scheduler.commit_frame(permit, now);
                report.draws = 1;
            }
            Err(error) => {
                scheduler.retry_frame(permit, error);
                report.failed = true;
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use philo_agent_service::FrontendOperationEvent;
    use ratatui::Terminal;
    use std::ops::Range;

    use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};

    use crate::app::effect::Effect;
    use crate::app::state::App;
    use crate::app::status::StatusData;
    use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};

    use super::*;

    fn line(text: &str) -> TranscriptLine {
        TranscriptLine {
            kind: LineKind::Meta,
            text: text.to_owned(),
            tone: crate::app::transcript::Tone::Plain,
            header: None,
            body: None,
        }
    }

    fn test_terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(80, 24)).expect("test terminal")
    }

    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn completed_lines_draw_from_cells_without_insert_before() {
        let start = Instant::now();
        let mut terminal = test_terminal();
        let mut app = App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true);
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput;

        app.ingest_appends(vec![Effect::Append(vec![
            line("first completed"),
            line("second completed"),
        ])]);
        let initial = output.flush(&mut terminal, &app, false, &mut scheduler, start);
        assert_eq!(
            initial,
            FlushReport {
                clears: 0,
                inserts: 0,
                draws: 1,
                failed: false,
            }
        );

        let first_delta = start + Duration::from_millis(1);
        app.ingest_appends(vec![Effect::Append(vec![line("third completed")])]);
        scheduler.invalidate_background(first_delta);
        assert_eq!(
            output.flush(&mut terminal, &app, false, &mut scheduler, first_delta,),
            FlushReport::default()
        );

        app.ingest_appends(vec![Effect::Append(vec![line("fourth completed")])]);
        scheduler.invalidate_background(start + Duration::from_millis(2));
        let batched = output.flush(
            &mut terminal,
            &app,
            false,
            &mut scheduler,
            start + super::super::scheduler::FRAME_INTERVAL,
        );
        assert_eq!(
            batched,
            FlushReport {
                clears: 0,
                inserts: 0,
                draws: 1,
                failed: false,
            }
        );

        let screen = screen_text(&terminal);
        assert!(screen.contains("third completed"), "{screen}");
        assert!(screen.contains("fourth completed"), "{screen}");
        assert!(screen.contains("Ask anything"), "{screen}");
    }

    #[test]
    fn tall_history_keeps_the_visible_tail_in_order() {
        let start = Instant::now();
        let mut terminal = test_terminal();
        let mut app = App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true);
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput;

        app.cells
            .push_closed((0..40).map(|i| line(&format!("row-{i}"))));
        output.flush(&mut terminal, &app, false, &mut scheduler, start);

        let screen = screen_text(&terminal);
        assert!(screen.contains("row-39"), "{screen}");
        assert!(
            !screen.contains("row-0"),
            "the oldest row should have scrolled off\n{screen}"
        );
    }

    #[test]
    fn ordinary_frames_never_clear_but_explicit_recovery_keeps_cells() {
        let start = Instant::now();
        let mut terminal = test_terminal();
        let mut app = App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true);
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput;
        app.ingest_appends(vec![Effect::Append(vec![line("keep-me")])]);
        output.flush(&mut terminal, &app, false, &mut scheduler, start);

        let background = start + super::super::scheduler::FRAME_INTERVAL;
        scheduler.invalidate_background(background);
        let ordinary = output.flush(&mut terminal, &app, false, &mut scheduler, background);
        assert_eq!(ordinary.clears, 0);

        let recovery = background + Duration::from_millis(1);
        scheduler.request_hard_redraw(recovery);
        let explicit = output.flush(&mut terminal, &app, false, &mut scheduler, recovery);
        assert_eq!(explicit.clears, 1);
        assert_eq!(explicit.draws, 1);
        assert_eq!(explicit.inserts, 0);
        let screen = screen_text(&terminal);
        assert!(
            screen.contains("keep-me"),
            "hard clear must rebuild from cells\n{screen}"
        );
    }

    #[test]
    fn delta_flood_keeps_exact_text_without_exceeding_the_frame_budget() {
        let start = Instant::now();
        let mut terminal = test_terminal();
        let mut app = App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true);
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput;
        let mut operations = output.flush(&mut terminal, &app, false, &mut scheduler, start);

        let expected = "x".repeat(1_000);
        for millis in 1..=1_000 {
            let _effects = app.on_operation_event(&FrontendOperationEvent::TextDelta {
                delta: "x".to_owned(),
            });
            let now = start + Duration::from_millis(millis);
            scheduler.invalidate_background(now);
            let report = output.flush(&mut terminal, &app, false, &mut scheduler, now);
            operations.clears += report.clears;
            operations.inserts += report.inserts;
            operations.draws += report.draws;
        }

        // The pacer held the flood; a structural boundary (here, the test)
        // flushes it so the final frame carries the whole tail.
        assert!(app.flush_stream(), "the flood buffered in the pacer");
        scheduler.invalidate_background(start + Duration::from_millis(1001));
        let final_deadline = scheduler.frame_deadline().expect("final dirty frame");
        let report = output.flush(&mut terminal, &app, false, &mut scheduler, final_deadline);
        operations.clears += report.clears;
        operations.inserts += report.inserts;
        operations.draws += report.draws;

        let open = app.cells.open_index().expect("stream still open");
        assert_eq!(app.cells.cells()[open].text, expected);
        assert_eq!(operations.clears, 0);
        assert_eq!(operations.inserts, 0);
        assert!(operations.draws <= 31);
        let screen = screen_text(&terminal);
        assert!(
            screen.contains(&"x".repeat(70)),
            "the history band should show the tail of the stream\n{screen}"
        );
    }

    struct FlakyBackend {
        inner: TestBackend,
        fail_draws: usize,
        cursor_ops: Vec<&'static str>,
    }

    impl Backend for FlakyBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.cursor_ops.push("draw");
            if self.fail_draws > 0 {
                self.fail_draws -= 1;
                return Err(io::Error::other("flaky draw"));
            }
            self.inner.draw(content).map_err(|error| match error {})
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.cursor_ops.push("hide");
            self.inner.hide_cursor().map_err(|error| match error {})
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.cursor_ops.push("show");
            self.inner.show_cursor().map_err(|error| match error {})
        }

        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.inner
                .get_cursor_position()
                .map_err(|error| match error {})
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.cursor_ops.push("set");
            self.inner
                .set_cursor_position(position)
                .map_err(|error| match error {})
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            self.inner.clear().map_err(|error| match error {})
        }

        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
            self.inner
                .clear_region(clear_type)
                .map_err(|error| match error {})
        }

        fn size(&self) -> Result<Size, Self::Error> {
            self.inner.size().map_err(|error| match error {})
        }

        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.inner.window_size().map_err(|error| match error {})
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            self.inner.flush().map_err(|error| match error {})
        }

        fn scroll_region_up(
            &mut self,
            region: Range<u16>,
            line_count: u16,
        ) -> Result<(), Self::Error> {
            self.inner
                .scroll_region_up(region, line_count)
                .map_err(|error| match error {})
        }

        fn scroll_region_down(
            &mut self,
            region: Range<u16>,
            line_count: u16,
        ) -> Result<(), Self::Error> {
            self.inner
                .scroll_region_down(region, line_count)
                .map_err(|error| match error {})
        }
    }

    #[test]
    fn first_draw_failure_then_success_paints_the_latest_frame() {
        let start = Instant::now();
        let mut terminal = Terminal::new(FlakyBackend {
            inner: TestBackend::new(80, 24),
            fail_draws: 1,
            cursor_ops: Vec::new(),
        })
        .expect("flaky terminal");
        let mut app = App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true);
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput;
        app.ingest_appends(vec![Effect::Append(vec![line("first-pass")])]);

        let failed = output.flush(&mut terminal, &app, false, &mut scheduler, start);
        assert!(failed.failed);
        assert_eq!(failed.draws, 0);
        assert!(scheduler.prepare_frame(start).is_some(), "dirty is kept");

        app.ingest_appends(vec![Effect::Append(vec![line("latest-pass")])]);
        scheduler.invalidate_immediate(start);
        let ok = output.flush(&mut terminal, &app, false, &mut scheduler, start);
        assert!(!ok.failed);
        assert_eq!(ok.draws, 1);

        let screen = terminal
            .backend()
            .inner
            .buffer()
            .content
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("latest-pass"), "{screen}");
    }

    #[test]
    fn frame_hides_cursor_while_writing_the_diff() {
        let start = Instant::now();
        let mut terminal = Terminal::new(FlakyBackend {
            inner: TestBackend::new(80, 24),
            fail_draws: 0,
            cursor_ops: Vec::new(),
        })
        .expect("tracking terminal");
        terminal.backend_mut().cursor_ops.clear();
        let app = App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true);
        let mut scheduler = FrameScheduler::new(start);

        let report = PendingOutput.flush(&mut terminal, &app, false, &mut scheduler, start);

        assert_eq!(report.draws, 1);
        assert_eq!(
            terminal.backend().cursor_ops,
            vec!["hide", "draw", "show", "set"]
        );
    }
}
