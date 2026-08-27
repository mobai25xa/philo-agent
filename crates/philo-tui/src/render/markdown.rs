//! Answer-row painter: semantic prose spans to ratatui lines.
//!
//! Projection (`app::prose`) bakes fully styled [`ProseSpan`]s into the
//! wrap cache — markdown parsing happens once per width change, never per
//! frame. This module only realizes semantics through the [`theme`] token
//! set, plus fenced code bodies, which stay raw text so syntect paints
//! them here (their language rides the row's role).

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
) -> Line<'static> {
    match spans {
        Some(spans) => Line::from(spans.iter().map(realize).collect::<Vec<_>>()),
        None => match role {
            BlockRole::FenceBody { lang } => fence_body(text, lang),
            _ => Line::from(Span::raw(text.to_owned())),
        },
    }
}

/// Maps one semantic span onto theme tokens. This is the only place prose
/// semantics become colors.
fn realize(span: &ProseSpan) -> Span<'static> {
    Span::styled(span.text.clone(), resolve(span.style))
}

fn resolve(style: ProseStyle) -> Style {
    let resolved = match style.color {
        ProseColor::Default => Style::default(),
        ProseColor::Meta => theme::meta(),
        ProseColor::Link => theme::link(),
        ProseColor::Code => theme::inline_code(),
        ProseColor::Accent => theme::accent(),
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

/// One fenced body row: guttered code, highlighted when the language is
/// known. An unrecognised language is not an error — the row degrades to
/// the soft code green so the text always reaches the user unchanged.
fn fence_body(text: &str, lang: &str) -> Line<'static> {
    let mut spans = vec![Span::styled("│ ", theme::rule())];
    match CodeHighlighter::for_language(lang).line(text) {
        Some(regions) => spans.extend(
            regions
                .into_iter()
                .map(|(style, fragment)| Span::styled(fragment, style)),
        ),
        None => spans.push(Span::styled(text.to_owned(), theme::inline_code())),
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
                    dump(&answer_row(&row.text, &row.role, row.spans.as_deref())),
                    text = row.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn one(lines: &[&str]) -> Line<'static> {
        let rows = prose::project_answer(&lines.join("\n"), 2000);
        answer_row(&rows[0].text, &rows[0].role, rows[0].spans.as_deref())
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
        let body = answer_row(&rows[1].text, &rows[1].role, rows[1].spans.as_deref());
        assert_eq!(
            body.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "│ # not a heading"
        );
        assert!(
            !body.spans[0].style.add_modifier.contains(Modifier::BOLD)
                && body.spans[0].style.fg == Some(ratatui::style::Color::DarkGray),
            "code keeps its semantic styling"
        );

        let prose_line = one(&["# heading"]);
        assert!(prose_line.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn checked_tasks_strike_and_dim_only_their_own_body() {
        let line = one(&["- [x] done deal"]);
        // The `- ` marker stays meta; the checked `[x] ` lights up accent.
        let texts: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["- ", "[x] ", "done deal"]);
        assert_eq!(line.spans[0].style.fg, Some(ratatui::style::Color::DarkGray));
        let check = &line.spans[1].style;
        assert_eq!(check.fg, Some(theme::accent().fg.expect("accent fg")));
        let body = &line.spans[2].style;
        assert!(
            body.add_modifier.contains(Modifier::CROSSED_OUT)
                && body.fg == Some(ratatui::style::Color::DarkGray),
            "done body is struck and dimmed"
        );

        let open = one(&["- [ ] open item"]);
        // Adjacent meta runs merge: marker and open box share one span.
        let texts: Vec<&str> = open.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["- [ ] ", "open item"]);
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
    fn quotes_keep_an_accent_bar_and_never_steal_the_reasoning_italic() {
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
            Some(theme::accent().fg.expect("accent fg")),
            "quote bar rides brand orange"
        );
        assert!(
            !line.spans[1].style.add_modifier.contains(Modifier::ITALIC),
            "quote bodies stay upright; italic belongs to think blocks"
        );
        assert_eq!(line.spans[1].style.fg, None, "quote body rides primary");
    }

    #[test]
    fn headings_step_down_the_ladder() {
        let accent_fg = theme::accent().fg.expect("accent fg");
        let h1 = one(&["# Title"]);
        assert!(h1.spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(h1.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(h1.spans[0].style.fg, Some(accent_fg), "H1 rides accent");

        let h2 = one(&["## Mid"]);
        assert!(h2.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(!h2.spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(h2.spans[0].style.fg, Some(accent_fg), "H2 rides accent");

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
        assert_eq!(line.spans[0].style.fg, Some(ratatui::style::Color::DarkGray));
    }

    #[test]
    fn inline_code_rides_green_text_and_links_stay_blue() {
        let line = one(&["use `fast_path` now"]);
        assert_eq!(line.spans[1].content.as_ref(), "fast_path");
        assert_eq!(
            line.spans[1].style.fg,
            Some(theme::code_fg()),
            "inline code is soft green text"
        );
        assert_eq!(
            line.spans[1].style.bg, None,
            "code is font color only — no background block"
        );

        let link = one(&["see [docs](https://example.test/guide)"]);
        let docs = link.spans.iter().find(|s| s.content.as_ref() == "docs").expect("link text");
        assert_eq!(docs.style.fg, Some(ratatui::style::Color::Blue));
        assert!(docs.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn tables_paint_an_accent_header_inside_meta_frames() {
        let rows = prose::project_answer("| a | bb |\n|---|---|\n| 1 | 2 |", 80);
        assert_eq!(rows[0].text, "╭───┬────╮");
        let header = answer_row(&rows[1].text, &rows[1].role, rows[1].spans.as_deref());
        assert_eq!(header.spans[0].content.as_ref(), "│ ");
        assert_eq!(header.spans[0].style.fg, Some(ratatui::style::Color::DarkGray));
        assert!(header.spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            header.spans[1].style.fg,
            Some(theme::accent().fg.expect("accent fg")),
            "header text rides brand orange"
        );

        let separator = answer_row(&rows[2].text, &rows[2].role, rows[2].spans.as_deref());
        assert_eq!(
            separator.spans.iter().map(|s| s.content.as_ref()).collect::<String>(),
            "├───┼────┤"
        );
        assert!(separator.spans.iter().all(|s| s.style.fg == Some(ratatui::style::Color::DarkGray)));
    }
}
