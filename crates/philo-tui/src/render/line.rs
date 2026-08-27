//! Semantic transcript lines mapped to Ratatui styles.
//!
//! The pre-redesign glyph family (`▸` tool headers, `•` verb bullets,
//! `│` think gutter, `└` detail rows) is gone: tool cards carry their
//! structure as [`Tone`]s (M5), user strips and answers render bare
//! (M4), and think bodies hang from a `│ ` gutter that lives only in
//! wrap/paint.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::app::transcript::{LineKind, Tone, TranscriptLine};

use super::theme;

pub(crate) fn styled_line(line: &TranscriptLine) -> Line<'static> {
    match line.kind {
        LineKind::User => user_line(&line.text),
        LineKind::Reasoning => reasoning_line(&line.text),
        LineKind::Tool => tool_line(line),
        LineKind::Answer => Line::styled(line.text.clone(), theme::primary()),
        // v4.0 §6.1: semantic status rows are two-tone — the symbol+tag
        // prefix carries the kind color (bold), the summary rides primary.
        // Prefixes pad to the 11-column tag gutter so the four templates
        // align (`✔ [Success]` is the widest at exactly 11).
        LineKind::Notice => {
            system_row("⚠ [Warn]   ", theme::warn().add_modifier(Modifier::BOLD), &line.text)
        }
        LineKind::Error => system_row("✖ [Error]  ", theme::error(), &line.text),
        LineKind::Meta => Line::styled(line.text.clone(), theme::meta()),
    }
}

/// One semantic status row: `{symbol} [Tag]` in the kind color plus the
/// plain summary (v4.0 §6.1).
fn system_row(prefix: &str, prefix_style: ratatui::style::Style, summary: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(prefix.to_owned(), prefix_style),
        Span::styled(summary.to_owned(), theme::primary()),
    ])
}

/// User strip rows carry no prefix and no own background here: the frame
/// pre-paints the full-width surface band and the accent bar under them.
fn user_line(text: &str) -> Line<'static> {
    if text.is_empty() {
        return Line::from("");
    }
    Line::styled(text.to_owned(), theme::primary())
}

fn reasoning_line(text: &str) -> Line<'static> {
    if text == "think" || text.starts_with("think · ") {
        return Line::styled(
            text.to_owned(),
            theme::reasoning().add_modifier(Modifier::BOLD),
        );
    }
    let rest = text
        .strip_prefix("│ ")
        .or_else(|| text.strip_prefix("  "))
        .unwrap_or(text);
    Line::from(vec![
        Span::styled("│ ".to_owned(), theme::reasoning()),
        Span::styled(rest.to_owned(), theme::reasoning()),
    ])
}

/// Tool cards paint by tone: the header's leading display name is accent
/// bold, `↳` details are meta, failures red, diff rows carry their wash
/// style for the history layer to fill across the content column.
fn tool_line(line: &TranscriptLine) -> Line<'static> {
    match line.tone {
        Tone::Title => title_line(&line.text),
        Tone::Detail => detail_line(&line.text),
        Tone::Failure => Line::styled(line.text.clone(), theme::err()),
        Tone::DiffDel => Line::styled(line.text.clone(), theme::diff_del()),
        Tone::DiffIns => Line::styled(line.text.clone(), theme::diff_add()),
        Tone::Plain => plain_tool_line(&line.text),
    }
}

/// `{title} {rest}` with the display name in the damped bold accent;
/// wrapped continuation rows fall back to plain default foreground.
fn title_line(text: &str) -> Line<'static> {
    let Some((title, rest)) = text.split_once(' ') else {
        return Line::styled(text.to_owned(), theme::bold_accent());
    };
    if rest.is_empty() {
        return Line::from(vec![
            Span::styled(title.to_owned(), theme::bold_accent()),
            Span::styled(" ".to_owned(), theme::primary()),
        ]);
    }
    Line::from(vec![
        Span::styled(title.to_owned(), theme::bold_accent()),
        Span::styled(format!(" {rest}"), theme::primary()),
    ])
}

fn detail_line(text: &str) -> Line<'static> {
    // v4.0 retires the `↳` detail glyph: detail rows render as plain
    // indented meta text until P3 rebuilds the card body.
    Line::styled(text.to_owned(), theme::meta())
}

/// Card body rows (output / locs / context lines) read as normal content.
fn plain_tool_line(text: &str) -> Line<'static> {
    if text.is_empty() {
        return Line::from("");
    }
    Line::styled(text.to_owned(), theme::primary())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::transcript::line;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn toned(kind: LineKind, text: &str, tone: Tone) -> TranscriptLine {
        TranscriptLine {
            kind,
            text: text.to_owned(),
            tone,
            header: None,
            body: None,
        }
    }

    #[test]
    fn think_block_paints_bar_on_body_not_header() {
        let header = styled_line(&line(LineKind::Reasoning, "think"));
        assert_eq!(line_text(&header), "think");
        assert!(!line_text(&header).contains('│'));

        let body = styled_line(&line(LineKind::Reasoning, "  hello"));
        assert_eq!(line_text(&body), "│ hello");

        let wrapped = styled_line(&line(LineKind::Reasoning, "│ more"));
        assert_eq!(line_text(&wrapped), "│ more");

        let answer = styled_line(&line(LineKind::Answer, "• the actual answer"));
        assert!(line_text(&answer).starts_with("• "));
        assert!(!line_text(&answer).contains('│'));
    }

    #[test]
    fn card_headers_accent_the_display_name_only() {
        let spans = styled_line(&toned(LineKind::Tool, "Grep 1 search", Tone::Title)).spans;
        assert_eq!(spans[0].style, theme::bold_accent());
        assert_eq!(spans[0].content.as_ref(), "Grep");
        assert_eq!(spans[1].content.as_ref(), " 1 search");

        let bare = styled_line(&toned(LineKind::Tool, "read_file", Tone::Title));
        assert_eq!(line_text(&bare), "read_file");
        assert_eq!(bare.style, theme::bold_accent());
    }

    #[test]
    fn detail_rows_and_failures_wear_their_tones() {
        let detail = styled_line(&toned(LineKind::Tool, "  pnpm test", Tone::Detail));
        assert_eq!(line_text(&detail), "  pnpm test");
        assert_eq!(detail.style, theme::meta());

        let failure = styled_line(&toned(
            LineKind::Tool,
            "  Failed. not_unique · 3 matches",
            Tone::Failure,
        ));
        assert_eq!(failure.style, theme::err());
    }

    #[test]
    fn diff_rows_carry_their_wash_styles() {
        let del = styled_line(&toned(
            LineKind::Tool,
            "    6 | const limit = page * 10;",
            Tone::DiffDel,
        ));
        assert_eq!(del.style, theme::diff_del());

        let ins = styled_line(&toned(
            LineKind::Tool,
            "    7 | const offset = page * limit;",
            Tone::DiffIns,
        ));
        assert_eq!(ins.style, theme::diff_add());

        let context = styled_line(&toned(
            LineKind::Tool,
            "    8 | return paginate(page);",
            Tone::Plain,
        ));
        assert_ne!(context.style, theme::diff_add());
        assert_ne!(context.style, theme::diff_del());
    }

    #[test]
    fn session_summaries_stay_plain_tool_rows() {
        let summary = styled_line(&line(LineKind::Tool, "ok · fn main() {}"));
        assert_eq!(summary.style, theme::primary());
        assert!(!line_text(&summary).contains('↳'));
    }
}
