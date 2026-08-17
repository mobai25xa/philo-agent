//! One scheduling round's terminal writes: optional hard clear and at most
//! one draw of the isolated alternate screen. Sealed history lives in
//! `App.cells`, not here.

use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::time::Instant;

use crate::app::state::App;
use crate::render::{frame, markdown::MarkdownRenderer};

use super::scheduler::FrameScheduler;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FlushReport {
    pub(crate) clears: usize,
    pub(crate) inserts: usize,
    pub(crate) draws: usize,
}

#[derive(Default)]
pub(crate) struct PendingOutput;

impl PendingOutput {
    /// Flushes only when the scheduler grants a frame. History is painted
    /// from `App.cells` inside `frame::draw`; this type never writes
    /// `insert_before`.
    pub(crate) fn flush<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &App,
        markdown: &mut MarkdownRenderer,
        shift_enter: bool,
        scheduler: &mut FrameScheduler,
        now: Instant,
    ) -> Result<FlushReport, B::Error> {
        let Some(plan) = scheduler.take_frame(now) else {
            return Ok(FlushReport::default());
        };

        let mut report = FlushReport::default();
        if plan.hard_clear {
            terminal.clear()?;
            report.clears = 1;
        }
        terminal.draw(|terminal_frame| {
            frame::draw(terminal_frame, app, markdown, shift_enter);
        })?;
        report.draws = 1;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use philo_agent_runtime::AgentEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::effect::Effect;
    use crate::app::status::StatusData;
    use crate::app::transcript::{InfoLevel, LineKind, TranscriptLine};

    use super::*;

    fn line(text: &str) -> TranscriptLine {
        TranscriptLine {
            kind: LineKind::Meta,
            text: text.to_owned(),
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
        let mut markdown = MarkdownRenderer::new();
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput;

        app.ingest_appends(vec![Effect::Append(vec![
            line("first completed"),
            line("second completed"),
        ])]);
        let initial = output
            .flush(
                &mut terminal,
                &app,
                &mut markdown,
                false,
                &mut scheduler,
                start,
            )
            .expect("initial flush");
        assert_eq!(
            initial,
            FlushReport {
                clears: 0,
                inserts: 0,
                draws: 1,
            }
        );

        let first_delta = start + Duration::from_millis(1);
        app.ingest_appends(vec![Effect::Append(vec![line("third completed")])]);
        scheduler.invalidate_background(first_delta);
        assert_eq!(
            output
                .flush(
                    &mut terminal,
                    &app,
                    &mut markdown,
                    false,
                    &mut scheduler,
                    first_delta,
                )
                .expect("deferred flush"),
            FlushReport::default()
        );

        app.ingest_appends(vec![Effect::Append(vec![line("fourth completed")])]);
        scheduler.invalidate_background(start + Duration::from_millis(2));
        let batched = output
            .flush(
                &mut terminal,
                &app,
                &mut markdown,
                false,
                &mut scheduler,
                start + super::super::scheduler::FRAME_INTERVAL,
            )
            .expect("batched flush");
        assert_eq!(
            batched,
            FlushReport {
                clears: 0,
                inserts: 0,
                draws: 1,
            }
        );

        let screen = screen_text(&terminal);
        assert!(screen.contains("third completed"), "{screen}");
        assert!(screen.contains("fourth completed"), "{screen}");
        assert!(screen.contains("model-a"), "{screen}");
        assert!(screen.contains("s-1"), "{screen}");
    }

    #[test]
    fn tall_history_keeps_the_visible_tail_in_order() {
        let start = Instant::now();
        let mut terminal = test_terminal();
        let mut app = App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true);
        let mut markdown = MarkdownRenderer::new();
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput;

        app.cells.append((0..40).map(|i| line(&format!("row-{i}"))));
        output
            .flush(
                &mut terminal,
                &app,
                &mut markdown,
                false,
                &mut scheduler,
                start,
            )
            .expect("flush");

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
        let mut markdown = MarkdownRenderer::new();
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput;
        app.ingest_appends(vec![Effect::Append(vec![line("keep-me")])]);
        output
            .flush(
                &mut terminal,
                &app,
                &mut markdown,
                false,
                &mut scheduler,
                start,
            )
            .expect("initial flush");

        let background = start + super::super::scheduler::FRAME_INTERVAL;
        scheduler.invalidate_background(background);
        let ordinary = output
            .flush(
                &mut terminal,
                &app,
                &mut markdown,
                false,
                &mut scheduler,
                background,
            )
            .expect("ordinary flush");
        assert_eq!(ordinary.clears, 0);

        let recovery = background + Duration::from_millis(1);
        scheduler.request_hard_redraw(recovery);
        let explicit = output
            .flush(
                &mut terminal,
                &app,
                &mut markdown,
                false,
                &mut scheduler,
                recovery,
            )
            .expect("recovery flush");
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
        let mut markdown = MarkdownRenderer::new();
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput;
        let mut operations = output
            .flush(
                &mut terminal,
                &app,
                &mut markdown,
                false,
                &mut scheduler,
                start,
            )
            .expect("initial flush");

        let expected = "x".repeat(1_000);
        for millis in 1..=1_000 {
            let _effects = app.on_agent_event(&AgentEvent::TextDelta {
                delta: "x".to_owned(),
            });
            let now = start + Duration::from_millis(millis);
            scheduler.invalidate_background(now);
            let report = output
                .flush(
                    &mut terminal,
                    &app,
                    &mut markdown,
                    false,
                    &mut scheduler,
                    now,
                )
                .expect("stream frame");
            operations.clears += report.clears;
            operations.inserts += report.inserts;
            operations.draws += report.draws;
        }

        let final_deadline = scheduler.frame_deadline().expect("final dirty frame");
        let report = output
            .flush(
                &mut terminal,
                &app,
                &mut markdown,
                false,
                &mut scheduler,
                final_deadline,
            )
            .expect("final stream frame");
        operations.clears += report.clears;
        operations.inserts += report.inserts;
        operations.draws += report.draws;

        assert_eq!(
            app.transcript.partial().map(|(_, text)| text),
            Some(expected.as_str())
        );
        assert_eq!(operations.clears, 0);
        assert_eq!(operations.inserts, 0);
        assert!(operations.draws <= 31);
        let screen = screen_text(&terminal);
        assert!(
            screen.contains(&"x".repeat(80)),
            "the history band should show the tail of the stream\n{screen}"
        );
    }
}
