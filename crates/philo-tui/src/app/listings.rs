//! Config and status listing projection. Pure formatting of frontend DTOs.

use philo_agent_service::{FrontendConfigEntry, FrontendStatus, FrontendToolListing};

use super::transcript::{LineKind, TranscriptLine};

fn line(kind: LineKind, text: impl Into<String>) -> TranscriptLine {
    TranscriptLine {
        kind,
        text: text.into(),
        tone: crate::app::transcript::Tone::Plain,
        header: None,
        body: None,
    }
}

pub(crate) fn config_lines(entries: &[FrontendConfigEntry]) -> Vec<TranscriptLine> {
    if entries.is_empty() {
        return vec![line(LineKind::Meta, "config: no effective entries")];
    }
    let width = entries
        .iter()
        .map(|entry| entry.key.len())
        .max()
        .unwrap_or(0);
    let mut lines = vec![line(LineKind::Meta, "config (effective):")];
    lines.extend(entries.iter().map(|entry| {
        line(
            LineKind::Meta,
            format!(
                "  {:width$} = {}  [{}]",
                entry.key, entry.value, entry.source
            ),
        )
    }));
    lines
}

pub(crate) fn status_lines(
    status_line: &str,
    attachment_summary: Option<String>,
    status: &FrontendStatus,
) -> Vec<TranscriptLine> {
    let mut lines = vec![line(LineKind::Meta, status_line.to_owned())];
    if let Some(summary) = attachment_summary {
        lines.push(line(LineKind::Meta, summary));
    }
    if status.tools.is_empty() {
        lines.push(line(LineKind::Meta, "tools: none"));
        return lines;
    }
    lines.push(line(
        LineKind::Meta,
        format!("tools ({}):", status.tools.len()),
    ));
    lines.extend(status.tools.iter().map(tool_line));
    lines
}

fn tool_line(tool: &FrontendToolListing) -> TranscriptLine {
    line(
        LineKind::Meta,
        format!(
            "  {} [{}]",
            tool.name,
            effect_class_label(&tool.effect_class)
        ),
    )
}

fn effect_class_label(class: &str) -> &str {
    match class {
        "read_only" | "ReadOnly" => "read-only",
        other => other,
    }
}
