//! Manual compaction results and automatic compaction event presentation.

use philo_agent_service::{
    FrontendMaintenance, FrontendMaintenancePhase, FrontendOperationEvent, FrontendTokenUsage,
    FrontendUpdateKind,
};

use crate::app::action::Action;
use crate::app::effect::Effect;
use crate::app::state::App;
use crate::app::status::StatusData;
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
    let waiting = app.status.line();
    let completed = app.apply_update(&frontend_update(
        1,
        settled(r#"Compacted { covers_up_to: "entry-42" }"#),
    ));
    assert!(
        app.status.usage.is_none(),
        "stale pre-compaction usage drops"
    );
    let completed_status = app.status.line();

    let second = submit_command(&mut app, "/compact");
    assert!(starts_compaction(&second));
    let nothing = app.apply_update(&frontend_update(2, settled("NothingToCompact")));

    crate::tests::assert_tui_snapshot!(
        "m13_manual_compaction_results",
        format!(
            "FIRST\n{}\nSTATUS {waiting}\n{}\nSTATUS {}\n\nSECOND\n{}\n{}\nSTATUS {}",
            appended(&first).join("\n"),
            appended(&completed).join("\n"),
            completed_status,
            appended(&second).join("\n"),
            appended(&nothing).join("\n"),
            app.status.line(),
        )
    );
}

#[test]
fn spinner_rejection_and_escape_are_visible() {
    let mut app = test_app();

    let started = submit_command(&mut app, "/compact");
    assert!(starts_compaction(&started));
    let frame_zero = app.status.line();
    app.on_tick();
    let frame_one = app.status.line();

    let duplicate = submit_command(&mut app, "/compact");
    assert!(!starts_compaction(&duplicate));

    let cancelled = app.on_action(Action::Escape);
    assert!(matches!(cancelled.first(), Some(Effect::CancelCompaction)));

    let mut busy = test_app();
    busy.set_busy(true, 0);
    let refused_busy = submit_command(&mut busy, "/compact");
    assert!(!starts_compaction(&refused_busy));

    crate::tests::assert_tui_snapshot!(
        "m13_compaction_spinner_cancel_and_rejection",
        format!(
            "START\n{}\n{frame_zero}\n{frame_one}\n\nDUPLICATE\n{}\n\nESC\n{}\nSTATUS {}\n\nBUSY\n{}",
            appended(&started).join("\n"),
            appended(&duplicate).join("\n"),
            appended(&cancelled).join("\n"),
            app.status.line(),
            appended(&refused_busy).join("\n"),
        )
    );
}

#[test]
fn automatic_events_render_and_update_status_without_breaking_the_flow() {
    let mut app = test_app();
    app.set_busy(true, 0);
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
        output.push(format!("status: {}", app.status.line()));
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
        collect_closed_cells(&app, &mut seen, &mut output);
    }
    output.push(format!(
        "open: {:?}",
        app.cells.open_index().map(|index| {
            let cell = &app.cells.cells()[index];
            (cell.kind, cell.text.as_str())
        })
    ));
    output.push(format!("status: {}", app.status.line()));

    crate::tests::assert_tui_snapshot!("m13_automatic_compaction_events", output.join("\n"));
}
