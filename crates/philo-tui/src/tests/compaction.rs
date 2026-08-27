//! Manual compaction results and automatic compaction event presentation.

use philo_agent_service::{
    FrontendMaintenance, FrontendMaintenancePhase, FrontendOperationEvent, FrontendTokenUsage,
    FrontendUpdateKind,
};

use crate::app::action::Action;
use crate::app::effect::Effect;
use crate::app::state::App;
use crate::app::status::StatusData;
use crate::app::submit::CancelDispatchResult;
use crate::app::transcript::{InfoLevel, TranscriptLine};
use crate::tests::support::frontend_update;

fn test_app() -> App {
    App::new(StatusData::new("model-a", "s-1", InfoLevel::Default), true)
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.on_action(Action::InsertChar(ch));
    }
}

fn submit_command(app: &mut App, text: &str) -> Vec<Effect> {
    type_text(app, text);
    app.on_action(Action::Submit)
}

fn rendered(lines: &[TranscriptLine]) -> Vec<String> {
    lines
        .iter()
        .map(|line| format!("{:?}: {}", line.kind, line.text))
        .collect()
}

fn appended(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .flat_map(|effect| match effect {
            Effect::Append(lines) => rendered(lines),
            _ => Vec::new(),
        })
        .collect()
}

fn collect_closed_cells(app: &App, seen: &mut usize, output: &mut Vec<String>) {
    let cells = app.cells.cells();
    let end = app.cells.open_index().unwrap_or(cells.len());
    if *seen < end {
        output.extend(rendered(&cells[*seen..end]));
    }
    *seen = end;
}

fn starts_compaction(effects: &[Effect]) -> bool {
    effects
        .iter()
        .any(|effect| matches!(effect, Effect::StartCompaction))
}

/// The composer's top-left state word; `(idle)` once the corner clears.
fn corner_word(app: &App) -> String {
    app.run_state_corner(80)
        .map(|corner| corner.word)
        .unwrap_or_else(|| "(idle)".to_owned())
}

fn settled(message: &str) -> FrontendUpdateKind {
    FrontendUpdateKind::MaintenanceChanged(FrontendMaintenance {
        id: "maint-1".to_owned(),
        phase: FrontendMaintenancePhase::Settled,
        message: Some(message.to_owned()),
    })
}

#[test]
fn manual_compaction_success_and_nothing_to_compact() {
    let mut app = test_app();
    app.status.usage = Some(FrontendTokenUsage {
        input_tokens: Some(8_000),
        output_tokens: Some(200),
        ..FrontendTokenUsage::default()
    });

    let first = submit_command(&mut app, "/compact");
    assert!(starts_compaction(&first));
    let waiting = corner_word(&app);
    let completed = app.apply_update(&frontend_update(
        1,
        settled(r#"Compacted { covers_up_to: "entry-42" }"#),
    ));
    assert!(
        app.status.usage.is_none(),
        "stale pre-compaction usage drops"
    );
    let completed_corner = corner_word(&app);

    let second = submit_command(&mut app, "/compact");
    assert!(starts_compaction(&second));
    let nothing = app.apply_update(&frontend_update(2, settled("NothingToCompact")));

    crate::tests::assert_tui_snapshot!(
        "m13_manual_compaction_results",
        format!(
            "FIRST\n{}\nCORNER {waiting}\n{}\nCORNER {completed_corner}\n\nSECOND\n{}\n{}\nCORNER {}",
            appended(&first).join("\n"),
            appended(&completed).join("\n"),
            appended(&second).join("\n"),
            appended(&nothing).join("\n"),
            corner_word(&app),
        )
    );
}

#[test]
fn spinner_rejection_and_escape_are_visible() {
    let mut app = test_app();

    let started = submit_command(&mut app, "/compact");
    assert!(starts_compaction(&started));
    let frame_zero = app
        .run_state_corner(80)
        .map(|corner| (corner.spinner, corner.word))
        .expect("manual compaction owns the corner");
    app.on_tick(std::time::Duration::from_millis(100));
    let frame_one = app
        .run_state_corner(80)
        .map(|corner| (corner.spinner, corner.word))
        .expect("the corner stays through the tick");
    assert_ne!(
        frame_zero.0, frame_one.0,
        "the run-state spinner advances on ticks"
    );

    let duplicate = submit_command(&mut app, "/compact");
    assert!(!starts_compaction(&duplicate));

    let cancelled = app.on_action(Action::Escape);
    assert!(matches!(cancelled.first(), Some(Effect::CancelCompaction)));

    let mut busy = test_app();
    busy.set_busy(true);
    let refused_busy = submit_command(&mut busy, "/compact");
    assert!(!starts_compaction(&refused_busy));

    crate::tests::assert_tui_snapshot!(
        "m13_compaction_spinner_cancel_and_rejection",
        format!(
            "START\n{}\n{frame_zero:?}\n{frame_one:?}\n\nDUPLICATE\n{}\n\nESC\n{}\nCORNER {}\n\nBUSY\n{}",
            appended(&started).join("\n"),
            appended(&duplicate).join("\n"),
            appended(&cancelled).join("\n"),
            corner_word(&app),
            appended(&refused_busy).join("\n"),
        )
    );
}

#[test]
fn compaction_escape_does_not_clear_before_dispatch() {
    let mut app = test_app();
    assert!(starts_compaction(&submit_command(&mut app, "/compact")));
    let effects = app.on_action(Action::Escape);
    assert_eq!(effects, vec![Effect::CancelCompaction]);
    assert!(app.status.compacting);
    assert!(
        !appended(&effects)
            .iter()
            .any(|line| line.contains("cancelled"))
    );
}

#[test]
fn compaction_cancel_backpressured_stays_compacting() {
    let mut app = test_app();
    assert!(starts_compaction(&submit_command(&mut app, "/compact")));
    let effects = app.on_action(Action::CompactionCancelDispatchFinished {
        result: CancelDispatchResult::Backpressured,
    });
    assert!(app.status.compacting);
    assert!(
        appended(&effects)
            .iter()
            .any(|line| line.contains("取消请求未发送"))
    );
    assert!(
        !appended(&effects)
            .iter()
            .any(|line| line.contains("cancelled"))
    );
}

#[test]
fn compaction_cancel_enqueued_then_clears() {
    let mut app = test_app();
    assert!(starts_compaction(&submit_command(&mut app, "/compact")));
    let effects = app.on_action(Action::CompactionCancelDispatchFinished {
        result: CancelDispatchResult::Enqueued(philo_agent_service::FrontendRequestId::new(3)),
    });
    assert!(!app.status.compacting);
    assert!(
        appended(&effects)
            .iter()
            .any(|line| line.contains("context compaction cancelled"))
    );
}

#[test]
fn automatic_events_render_and_update_status_without_breaking_the_flow() {
    let mut app = test_app();
    app.set_busy(true);
    app.status.usage = Some(FrontendTokenUsage {
        input_tokens: Some(9_000),
        ..FrontendTokenUsage::default()
    });
    let mut output = Vec::new();
    let mut seen = 0;

    for event in [
        FrontendOperationEvent::ContextCompactionStarted,
        FrontendOperationEvent::ContextCompactionCompleted {
            covers_up_to: "entry-42".to_owned(),
        },
    ] {
        app.on_operation_event(&event);
        collect_closed_cells(&app, &mut seen, &mut output);
        output.push(format!("corner: {}", corner_word(&app)));
    }
    assert!(app.status.usage.is_none());

    app.on_operation_event(&FrontendOperationEvent::ModelUsageUpdated {
        model_call_id: "call-2".to_owned(),
        usage: FrontendTokenUsage {
            input_tokens: Some(4_000),
            ..FrontendTokenUsage::default()
        },
    });
    for event in [
        FrontendOperationEvent::ContextCompactionStarted,
        FrontendOperationEvent::ContextCompactionFailed {
            message: "summary model unavailable".to_owned(),
        },
        FrontendOperationEvent::TextDelta {
            delta: "turn continues".to_owned(),
        },
    ] {
        app.on_operation_event(&event);
        app.flush_stream();
        collect_closed_cells(&app, &mut seen, &mut output);
    }
    output.push(format!(
        "open: {:?}",
        app.cells.open_index().map(|index| {
            let cell = &app.cells.cells()[index];
            (cell.kind, cell.text.as_str())
        })
    ));
    output.push(format!("corner: {}", corner_word(&app)));

    crate::tests::assert_tui_snapshot!("m13_automatic_compaction_events", output.join("\n"));
}
