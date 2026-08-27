//! Answer-row painter: semantic prose spans to ratatui lines.
//!
//! Projection (`app::prose`) bakes fully styled [`ProseSpan`]s into the
//! wrap cache — markdown parsing happens once per width change, never per
//! frame. This module only realizes semantics through the [`theme`] token
//! set, plus fenced code bodies, which stay raw text so syntect paints
//! them here (their language rides the row's role, and the v4.0 P4 gutter
//! carries the line-number slot the projection numbered).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::prose::{BlockRole, ProseColor, ProseSpan, ProseStyle};

use super::highlight::CodeHighlighter;
use super::theme;

/// Paints one projected answer row. Pure: same inputs, same line.
pub(crate) fn answer_row(
    text: &str,
    role: &BlockRole,
    spans: Option<&[ProseSpan]>,
    code_line: Option<&str>,
) -> Line<'static> {
    match spans {
        Some(spans) => Line::from(spans.iter().map(realize).collect::<Vec<_>>()),
        None => match role {
            BlockRole::FenceBody { lang } => fence_body(text, lang, code_line),
            _ => Line::from(Span::raw(text.to_owned())),
        },
    }
}

/// Maps one semantic span onto theme tokens. This is the only place prose
/// semantics become colors.
fn realize(span: &ProseSpan) -> Span<'static> {
    Span::styled(span.text.clone(), resolve(span.style))
}

/// Realizes a baked span run into a painted line (tool-card rows reuse the
/// prose span machinery so one semantic → token table covers both).
pub(crate) fn line_from_spans(spans: &[ProseSpan]) -> Line<'static> {
    Line::from(spans.iter().map(realize).collect::<Vec<_>>())
}

fn resolve(style: ProseStyle) -> Style {
    // The bold-gain rule (anti-glare): a bold run wearing the saturated
    // accent family steps down a color rung — the theme damps its
    // saturation/lightness while keeping the hue.
    let damped = style.bold && matches!(style.color, ProseColor::Code | ProseColor::Accent);
    let resolved = match style.color {
        ProseColor::Default => Style::default(),
        ProseColor::Meta => theme::meta(),
        ProseColor::Link => theme::link(),
        ProseColor::Code if damped => theme::bold_accent(),
        ProseColor::Code => theme::inline_code(),
        ProseColor::Accent if damped => theme::bold_accent(),
        ProseColor::Accent => theme::accent(),
        ProseColor::DarkGray => theme::corner_meta(),
        ProseColor::Green => theme::ok(),
        ProseColor::Yellow => theme::warn(),
        ProseColor::Red => theme::err(),
        ProseColor::Blue => theme::info(),
        ProseColor::Border => Style::default().fg(theme::border_color()),
        ProseColor::White => theme::bold_white(),
    };
    let mut modifiers = Modifier::empty();
    if style.bold {
        modifiers |= Modifier::BOLD;
    }
    if style.italic {
        modifiers |= Modifier::ITALIC;
    }
    if style.underline {
        modifiers |= Modifier::UNDERLINED;
    }
    if style.crossed {
        modifiers |= Modifier::CROSSED_OUT;
    }
    resolved.add_modifier(modifiers)
}

/// One fenced body row: `{slot} │ {code}`, where `slot` is the numbered
/// gutter (v4.0 P4 §3.1) — dark-gray right-padded line number (or blank
/// continuation cells) followed by a BORDER `│`. The code is highlighted
/// when the language is known; an unrecognised language is not an error —
/// the row degrades to plain code so the text always reaches the user.
fn fence_body(text: &str, lang: &str, slot: Option<&str>) -> Line<'static> {
    let mut spans = Vec::with_capacity(3);
    let number = slot.unwrap_or_default();
    if number.is_empty() {
        spans.push(Span::raw("  "));
    } else if number.chars().all(|ch| ch == ' ') {
        spans.push(Span::raw(format!("{number} ")));
    } else {
        spans.push(Span::styled(
            format!("{number} "),
            theme::corner_meta(),
        ));
    }
    spans.push(Span::styled("│ ", Style::default().fg(theme::border_color())));
    match CodeHighlighter::for_language(lang).line(text) {
        Some(regions) => spans.extend(
            regions
                .into_iter()
                .map(|(style, fragment)| Span::styled(fragment, style)),
        ),
        None => spans.push(Span::styled(text.to_owned(), theme::primary())),
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::prose;

    /// Compact dump of a rendered line: `text{styles}` per span.
    fn dump(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| {
                let mut marks: Vec<String> = Vec::new();
                if let Some(color) = span.style.fg {
                    marks.push(format!("{color:?}").to_lowercase());
                }
                if let Some(color) = span.style.bg {
                    marks.push(format!("on {color:?}").to_lowercase());
                }
                for (modifier, name) in [
                    (Modifier::BOLD, "bold"),
                    (Modifier::ITALIC, "italic"),
                    (Modifier::UNDERLINED, "underlined"),
                    (Modifier::CROSSED_OUT, "crossed"),
                ] {
                    if span.style.add_modifier.contains(modifier) {
                        marks.push(name.to_owned());
                    }
                }
                if marks.is_empty() {
                    format!("{:?}", span.content)
                } else {
                    format!("{:?}{{{}}}", span.content, marks.join(","))
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Projects the joined lines exactly as production would (wide enough
    /// to skip wrapping), then paints each row.
    fn render(lines: &[&str]) -> String {
        prose::project_answer(&lines.join("\n"), 2000)
            .iter()
            .map(|row| {
                format!(
                    "{text}\n  -> {}",
                    dump(&answer_row(
                        &row.text,
                        &row.role,
                        row.spans.as_deref(),
                        row.code_line.as_deref(),
                    )),
                    text = row.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn one(lines: &[&str]) -> Line<'static> {
        let rows = prose::project_answer(&lines.join("\n"), 2000);
        answer_row(
            &rows[0].text,
            &rows[0].role,
            rows[0].spans.as_deref(),
            rows[0].code_line.as_deref(),
        )
    }

    #[test]
    fn element_matrix_snapshot() {
        crate::tests::assert_tui_snapshot!(
            "markdown_elements",
            render(&[
                "# Title",
                "## Middle",
                "### Smaller",
                "plain paragraph text",
                "- first item",
                "  - nested item",
                "1. ordered item",
                "- [x] done deal",
                "- [ ] open item",
                "> quoted remark",
                "text with **bold**, *italic*, ~~struck~~ and `inline code`",
                "see [docs](https://example.test/guide)",
                "open src/utils/jwt.ts now",
                "---",
                "",
            ])
        );
    }

    #[test]
    fn fenced_code_snapshot() {
        crate::tests::assert_tui_snapshot!(
            "markdown_code_fence",
            render(&[
                "```rust",
                "fn main() {}",
                "```",
                "between blocks",
                "```not-a-language",
                "* not a list *",
                "```",
                "after",
            ])
        );
    }

    #[test]
    fn table_grid_snapshot() {
        crate::tests::assert_tui_snapshot!(
            "markdown_table_grid",
            render(&[
                "| plan | latency | notes |",
                "|---|---|---|",
                "| fast | 2ms | cached path |",
                "| slow | 200ms | hits the network every single time |",
                "",
                "back to prose",
            ])
        );
    }

    #[test]
    fn fence_bodies_are_code_not_markdown() {
        let rows = prose::project_answer("```\n# not a heading\n```", 80);
        let body = answer_row(
            &rows[1].text,
            &rows[1].role,
            rows[1].spans.as_deref(),
            rows[1].code_line.as_deref(),
        );
        assert_eq!(
            body.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "1 │ # not a heading",
            "the P4 gutter leads with the dark-gray number slot and a border bar"
        );
        assert_eq!(
            body.spans[0].style.fg,
            Some(theme::corner_meta().fg.expect("dark fg")),
            "line numbers are dark gray hints"
        );
        assert_eq!(
            body.spans[1].style.fg,
            Some(theme::border_color()),
            "the gutter bar rides the BORDER token"
        );
        assert!(
            !body.spans[2].style.add_modifier.contains(Modifier::BOLD),
            "code keeps its semantic styling"
        );

        let prose_line = one(&["# heading"]);
        assert!(prose_line.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn unclosed_streaming_fence_paints_no_background_bleed() {
        // The streaming intermediate frame: the closing ``` has not arrived
        // yet, so the whole remainder is one fence body. The virtual
        // closure must not smear a block background across the screen —
        // body cells stay foreground-only (P4 §3.1).
        let rows = prose::project_answer("```rust\nlet answer = 42;\n", 80);
        assert_eq!(rows[1].role, crate::app::prose::BlockRole::FenceBody { lang: "rust".to_owned() });
        let body = answer_row(
            &rows[1].text,
            &rows[1].role,
            rows[1].spans.as_deref(),
            rows[1].code_line.as_deref(),
        );
        assert_eq!(
            body.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "1 │ let answer = 42;"
        );
        assert!(
            body.spans.iter().all(|span| span.style.bg.is_none()),
            "a streamed fence body never paints a background block: {:?}",
            body.spans
        );
    }

    #[test]
    fn checked_tasks_strike_and_dim_only_their_own_body() {
        let line = one(&["- [x] done deal"]);
        // v4.0: the bullet is bold accent; the checked `[x] ` lights up the
        // damped accent; the done body stays struck meta.
        let texts: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["• ", "[x] ", "done deal"]);
        assert_eq!(line.spans[0].style.fg, Some(theme::code_fg()));
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
        let check = &line.spans[1].style;
        assert_eq!(check.fg, theme::accent().fg, "the box stays regular accent");
        let body = &line.spans[2].style;
        assert!(
            body.add_modifier.contains(Modifier::CROSSED_OUT)
                && body.fg == Some(theme::meta().fg.expect("meta fg")),
            "done body is struck and dimmed"
        );

        let open = one(&["- [ ] open item"]);
        // The bold bullet stays separate from the regular-weight box run.
        let texts: Vec<&str> = open.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["• ", "[ ] ", "open item"]);
        assert!(
            !open.spans[1].style.add_modifier.contains(Modifier::CROSSED_OUT),
            "open tasks stay primary"
        );

        // Strike state cannot leak across logical lines.
        let done = one(&["- [x] done"]);
        assert!(
            done.spans
                .last()
                .expect("body span")
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        let next = one(&["plain tail"]);
        let last = next.spans.last().expect("tail span");
        assert_eq!(last.content.as_ref(), "plain tail");
        assert!(!last.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn quotes_keep_a_dark_gray_bar_and_never_steal_the_reasoning_italic() {
        let line = one(&["> quoted remark"]);
        assert_eq!(
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            "│ quoted remark"
        );
        assert_eq!(
            line.spans[0].style.fg,
            Some(theme::corner_meta().fg.expect("dark fg")),
            "the quote bar is a dark-gray hairline (v4.0 §6)"
        );
        assert!(
            !line.spans[1].style.add_modifier.contains(Modifier::ITALIC),
            "quote bodies stay upright; italic belongs to think blocks"
        );
        assert_eq!(line.spans[1].style.fg, None, "quote body rides primary");
    }

    #[test]
    fn headings_step_down_the_ladder() {
        let white_fg = theme::bold_white().fg.expect("bold white fg");
        let h1 = one(&["# Title"]);
        assert!(
            !h1.spans[0].style.add_modifier.contains(Modifier::UNDERLINED),
            "P4 removes the H1 underline"
        );
        assert!(h1.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            h1.spans[0].style.fg,
            theme::bold_accent().fg,
            "H1 rides the damped bold accent"
        );

        let h2 = one(&["## Mid"]);
        assert!(h2.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(!h2.spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(h2.spans[0].style.fg, Some(white_fg), "H2 lifts to bold white");

        let h3 = one(&["### Small"]);
        assert!(h3.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(h3.spans[0].style.fg, None, "H3+ falls back to primary weight");
    }

    #[test]
    fn rules_become_a_dim_horizontal_run() {
        let line = one(&["---"]);
        assert_eq!(
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            "────────────────────────"
        );
        assert_eq!(line.spans[0].style.fg, Some(theme::meta().fg.expect("meta fg")));
    }

    #[test]
    fn inline_code_rides_uniform_green_and_links_stay_blue() {
        let line = one(&["use `fast_path` now"]);
        assert_eq!(line.spans[1].content.as_ref(), "fast_path");
        assert_eq!(
            line.spans[1].style.fg,
            Some(theme::ok().fg.expect("green fg")),
            "inline code rides the uniform helper green"
        );
        assert_eq!(
            line.spans[1].style.bg, None,
            "code is font color only — no background block"
        );

        let link = one(&["see [docs](https://example.test/guide)"]);
        let docs = link.spans.iter().find(|s| s.content.as_ref() == "docs").expect("link text");
        assert_eq!(docs.style.fg, Some(theme::link().fg.expect("link fg")));
        assert!(docs.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn tables_paint_an_accent_header_inside_border_frames() {
        let rows = prose::project_answer("| a | bb |\n|---|---|\n| 1 | 2 |", 80);
        assert_eq!(rows[0].text, "╭───┬────╮");
        let header = answer_row(
            &rows[1].text,
            &rows[1].role,
            rows[1].spans.as_deref(),
            rows[1].code_line.as_deref(),
        );
        assert_eq!(header.spans[0].content.as_ref(), "│ ");
        assert_eq!(
            header.spans[0].style.fg,
            Some(theme::border_color()),
            "table gutters ride the BORDER token"
        );
        assert!(header.spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            header.spans[1].style.fg,
            theme::bold_accent().fg,
            "header text rides the damped bold accent"
        );

        let separator = answer_row(
            &rows[2].text,
            &rows[2].role,
            rows[2].spans.as_deref(),
            rows[2].code_line.as_deref(),
        );
        assert_eq!(
            separator.spans.iter().map(|s| s.content.as_ref()).collect::<String>(),
            "├───┼────┤"
        );
        assert!(
            separator
                .spans
                .iter()
                .all(|s| s.style.fg == Some(theme::border_color()))
        );
    }

    #[test]
    fn bare_paths_paint_helper_green_outside_code_and_links() {
        let line = one(&["open src/utils/jwt.ts now"]);
        // The trailing space coalesces into the next plain run during wrap.
        let texts: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["open ", "src/utils/jwt.ts", " now"]);
        assert_eq!(
            line.spans[1].style.fg,
            Some(theme::ok().fg.expect("green fg")),
            "the bare path lifts to helper green"
        );
        assert_eq!(line.spans[2].style.fg, None, "surrounding words stay primary");
    }
}
