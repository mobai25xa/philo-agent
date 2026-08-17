//! One scheduling round's terminal writes: at most one scrollback insert and
//! one bottom-panel draw.

use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::text::Line;
use tokio::time::Instant;

use crate::app::state::App;
use crate::app::transcript::TranscriptLine;
use crate::render::{frame, markdown::MarkdownRenderer};

use super::scheduler::FrameScheduler;
use super::scrollback;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FlushReport {
    pub(crate) clears: usize,
    pub(crate) inserts: usize,
    pub(crate) draws: usize,
}

#[derive(Default)]
pub(crate) struct PendingOutput {
    history: Vec<Line<'static>>,
}

impl PendingOutput {
    pub(crate) fn append(&mut self, markdown: &mut MarkdownRenderer, lines: Vec<TranscriptLine>) {
        self.history
            .extend(lines.iter().map(|line| markdown.commit(line)));
    }

    /// Flushes only when the scheduler grants a frame. Completed history
    /// accumulated since the previous frame is inserted as one batch before
    /// the bottom panel is repainted once.
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
        if !self.history.is_empty() {
            let rendered = self.take_history_batch(usize::from(u16::MAX));
            scrollback::append_history(terminal, &rendered)?;
            report.inserts = 1;
            if !self.history.is_empty() {
                // Ratatui's insertion height is u16. Preserve an oversized
                // queue and grant it one append-only batch on a later frame.
                scheduler.invalidate_background(now);
            }
        }
        terminal.draw(|terminal_frame| {
            frame::draw(terminal_frame, app, markdown, shift_enter);
        })?;
        report.draws = 1;
        Ok(report)
    }

    fn take_history_batch(&mut self, limit: usize) -> Vec<Line<'static>> {
        let end = std::cmp::min(limit, self.history.len());
        self.history.drain(..end).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use philo_agent_runtime::AgentEvent;
    use ratatui::backend::{Backend, TestBackend};
    use ratatui::layout::Position;
    use ratatui::{TerminalOptions, Viewport};

    use crate::app::effect::Effect;
    use crate::app::status::StatusData;
    use crate::app::transcript::{InfoLevel, LineKind};

    use super::*;

    fn line(text: &str) -> TranscriptLine {
        TranscriptLine {
            kind: LineKind::Meta,
            text: text.to_owned(),
        }
    }

    fn test_terminal() -> Terminal<TestBackend> {
        let mut backend = TestBackend::new(80, 20);
        backend
            .set_cursor_position(Position::new(0, 12))
            .expect("test cursor");
        Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(crate::render::frame::VIEWPORT_HEIGHT),
            },
        )
        .expect("test terminal")
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
    fn completed_lines_are_batched_into_one_insert_and_one_draw() {
        let start = Instant::now();
        let mut terminal = test_terminal();
        let app = App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true);
        let mut markdown = MarkdownRenderer::new();
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput::default();

        output.append(
            &mut markdown,
            vec![line("first completed"), line("second completed")],
        );
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
                inserts: 1,
                draws: 1,
            }
        );

        let first_delta = start + Duration::from_millis(1);
        output.append(&mut markdown, vec![line("third completed")]);
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

        output.append(&mut markdown, vec![line("fourth completed")]);
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
                inserts: 1,
                draws: 1,
            }
        );

        let screen = screen_text(&terminal);
        assert!(screen.contains("third completed"));
        assert!(screen.contains("fourth completed"));
        assert!(screen.contains("model model-a · session s-1 · idle"));
    }

    #[test]
    fn oversized_history_batches_keep_the_remainder_in_order() {
        let mut markdown = MarkdownRenderer::new();
        let mut output = PendingOutput::default();
        output.append(&mut markdown, vec![line("one"), line("two"), line("three")]);

        let first = output.take_history_batch(2);
        let second = output.take_history_batch(2);
        let text = |lines: Vec<Line<'static>>| {
            lines
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(text(first), ["one", "two"]);
        assert_eq!(text(second), ["three"]);
    }

    #[test]
    fn ordinary_frames_never_clear_but_explicit_recovery_does() {
        let start = Instant::now();
        let mut terminal = test_terminal();
        let app = App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true);
        let mut markdown = MarkdownRenderer::new();
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput::default();
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
    }

    #[test]
    fn delta_flood_keeps_exact_text_without_exceeding_the_frame_budget() {
        let start = Instant::now();
        let mut terminal = test_terminal();
        let mut app = App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true);
        let mut markdown = MarkdownRenderer::new();
        let mut scheduler = FrameScheduler::new(start);
        let mut output = PendingOutput::default();
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
            let effects = app.on_agent_event(&AgentEvent::TextDelta {
                delta: "x".to_owned(),
            });
            for effect in effects {
                if let Effect::Append(lines) = effect {
                    output.append(&mut markdown, lines);
                }
            }
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
        assert!(screen.contains("answer"), "{screen}");
        assert!(
            screen.contains(&"x".repeat(80)),
            "the live band should show the tail of the stream\n{screen}"
        );
    }
}
