//! Markdown projection for answer text.
//!
//! The transcript is line-oriented and append-only, so rendering is too: a
//! committed line is styled once and never revisited. Only fenced code
//! blocks need memory across lines, and that is the whole of this
//! renderer's state. Answer lines go through markdown; every other line
//! kind keeps its semantic styling.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::transcript::{LineKind, TranscriptLine};

use super::highlight::{CodeHighlighter, code_style};
use super::line::styled_line;
use super::theme;

/// An open fenced code block.
struct Fence {
    marker: char,
    language: String,
    #[cfg(test)]
    highlighter: CodeHighlighter,
}

/// Renders transcript lines, remembering only the open code fence.
#[derive(Default)]
pub(crate) struct MarkdownRenderer {
    fence: Option<Fence>,
}

impl MarkdownRenderer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Drops block state at a session boundary, so an unterminated fence
    /// cannot bleed into the next session's answers.
    pub(crate) fn reset(&mut self) {
        self.fence = None;
    }

    /// Renders a line that is now part of history, advancing block state.
    /// History paint uses [`Self::preview`]; this stays for markdown tests.
    #[cfg(test)]
    pub(crate) fn commit(&mut self, line: &TranscriptLine) -> Line<'static> {
        if line.kind != LineKind::Answer {
            return styled_line(line);
        }
        let text = line.text.as_str();
        if let Some(fence) = self.fence.as_mut() {
            if closes(text, fence.marker) {
                self.fence = None;
                return delimiter(text);
            }
            let mut spans = vec![Span::styled("│ ", theme::rule())];
            match fence.highlighter.line(text) {
                Some(regions) => spans.extend(
                    regions
                        .into_iter()
                        .map(|(style, fragment)| Span::styled(fragment, style)),
                ),
                None => spans.push(Span::styled(text.to_owned(), code_style())),
            }
            return Line::from(spans);
        }
        match opens(text) {
            Some((marker, language)) => {
                self.fence = Some(Fence {
                    marker,
                    language: language.clone(),
                    highlighter: CodeHighlighter::for_language(&language),
                });
                delimiter(text)
            }
            None => inline(text),
        }
    }

    /// Renders the streaming line without touching block state: it is still
    /// being written and will be committed again once complete.
    pub(crate) fn preview(&self, line: &TranscriptLine) -> Line<'static> {
        if line.kind != LineKind::Answer {
            return styled_line(line);
        }
        let text = line.text.as_str();
        if let Some(fence) = self.fence.as_ref() {
            if closes(text, fence.marker) {
                return delimiter(text);
            }
            let mut spans = vec![Span::styled("│ ", theme::rule())];
            match CodeHighlighter::preview_line(&fence.language, text) {
                Some(regions) => spans.extend(
                    regions
                        .into_iter()
                        .map(|(style, fragment)| Span::styled(fragment, style)),
                ),
                None => spans.push(Span::styled(text.to_owned(), code_style())),
            }
            return Line::from(spans);
        }
        if opens(text).is_some() {
            return delimiter(text);
        }
        inline(text)
    }
}

/// The fence line itself, shown as the dim marker it is.
fn delimiter(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        Style::default().fg(Color::DarkGray),
    ))
}

/// A fence opener and its info string, if this line is one.
fn opens(text: &str) -> Option<(char, String)> {
    let (marker, rest) = fence_run(text)?;
    Some((marker, rest.trim().to_owned()))
}

/// Whether this line closes a fence opened with `marker` (a closing fence
/// carries no info string).
fn closes(text: &str, marker: char) -> bool {
    match fence_run(text) {
        Some((found, rest)) => found == marker && rest.trim().is_empty(),
        None => false,
    }
}

/// Splits a fence line into its marker and info string.
fn fence_run(text: &str) -> Option<(char, &str)> {
    let trimmed = text.trim_start();
    let marker = trimmed
        .chars()
        .next()
        .filter(|ch| *ch == '`' || *ch == '~')?;
    let run = trimmed.chars().take_while(|ch| *ch == marker).count();
    if run < 3 {
        return None;
    }
    Some((marker, &trimmed[run..]))
}

/// Renders one markdown line: block prefix plus inline styling.
fn inline(text: &str) -> Line<'static> {
    let indent: String = text.chars().take_while(|ch| *ch == ' ').collect();
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut styles = vec![Style::default()];
    let mut ordered: Option<u64> = None;

    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                spans.push(Span::styled("▍ ", heading_style(level)));
                styles.push(heading_style(level));
            }
            Event::End(TagEnd::Heading(_)) => pop(&mut styles),
            Event::Start(Tag::List(start)) => ordered = start,
            Event::Start(Tag::Item) => {
                let marker = match ordered {
                    Some(number) => format!("{number}. "),
                    None => "- ".to_owned(),
                };
                spans.push(Span::styled(
                    format!("{indent}{marker}"),
                    Style::default().fg(Color::Green),
                ));
            }
            Event::Start(Tag::BlockQuote(_)) => {
                spans.push(Span::styled(
                    format!("{indent}| "),
                    Style::default().fg(Color::DarkGray),
                ));
                styles.push(top(&styles).fg(Color::Gray).add_modifier(Modifier::ITALIC));
            }
            Event::End(TagEnd::BlockQuote(_)) => pop(&mut styles),
            Event::Start(Tag::Emphasis) => {
                styles.push(top(&styles).add_modifier(Modifier::ITALIC));
            }
            Event::End(TagEnd::Emphasis) => pop(&mut styles),
            Event::Start(Tag::Strong) => styles.push(top(&styles).add_modifier(Modifier::BOLD)),
            Event::End(TagEnd::Strong) => pop(&mut styles),
            Event::Start(Tag::Strikethrough) => {
                styles.push(top(&styles).add_modifier(Modifier::CROSSED_OUT));
            }
            Event::End(TagEnd::Strikethrough) => pop(&mut styles),
            Event::Start(Tag::Link { .. }) => {
                styles.push(
                    top(&styles)
                        .fg(Color::Blue)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Event::End(TagEnd::Link) => pop(&mut styles),
            // Indented code: styled like a fenced block, without a language.
            Event::Start(Tag::CodeBlock(_)) => styles.push(code_style()),
            Event::End(TagEnd::CodeBlock) => pop(&mut styles),
            Event::Code(code) => spans.push(Span::styled(code.to_string(), code_style())),
            Event::Text(text) => spans.push(Span::styled(text.to_string(), top(&styles))),
            Event::SoftBreak | Event::HardBreak => spans.push(Span::raw(" ")),
            Event::Rule => spans.push(Span::styled(
                "-".repeat(24),
                Style::default().fg(Color::DarkGray),
            )),
            _ => {}
        }
    }
    if spans.is_empty() {
        return Line::from(Span::raw(text.to_owned()));
    }
    Line::from(spans)
}

fn heading_style(level: HeadingLevel) -> Style {
    let base = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => base.add_modifier(Modifier::UNDERLINED),
        _ => base,
    }
}

fn top(styles: &[Style]) -> Style {
    styles.last().copied().unwrap_or_default()
}

fn pop(styles: &mut Vec<Style>) {
    if styles.len() > 1 {
        styles.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(text: &str) -> TranscriptLine {
        TranscriptLine {
            kind: LineKind::Answer,
            text: text.to_owned(),
        }
    }

    /// Compact dump of a rendered line: `text{styles}` per span.
    fn dump(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| {
                let mut marks: Vec<String> = Vec::new();
                if let Some(color) = span.style.fg {
                    marks.push(format!("{color:?}").to_lowercase());
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

    fn render(lines: &[&str]) -> String {
        let mut renderer = MarkdownRenderer::new();
        lines
            .iter()
            .map(|text| format!("{text}\n  -> {}", dump(&renderer.commit(&answer(text)))))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn element_matrix_snapshot() {
        crate::tests::assert_tui_snapshot!(
            "markdown_elements",
            render(&[
                "# Title",
                "### Smaller",
                "plain paragraph text",
                "- first item",
                "  - nested item",
                "1. ordered item",
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
    fn a_fence_keeps_markdown_from_touching_its_body() {
        let mut renderer = MarkdownRenderer::new();
        renderer.commit(&answer("```"));
        let inside = renderer.commit(&answer("# not a heading"));
        assert_eq!(
            inside
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "│ # not a heading"
        );
        assert!(
            !inside.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "code is not styled as markdown"
        );
        renderer.commit(&answer("```"));
        let outside = renderer.commit(&answer("# heading again"));
        assert!(outside.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn an_unterminated_fence_does_not_survive_a_session_switch() {
        let mut renderer = MarkdownRenderer::new();
        renderer.commit(&answer("```rust"));
        renderer.reset();
        let after = renderer.commit(&answer("# heading"));
        assert!(after.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn the_preview_renders_without_advancing_block_state() {
        let mut renderer = MarkdownRenderer::new();
        let preview = renderer.preview(&answer("```rust"));
        assert_eq!(preview.spans[0].content.as_ref(), "```rust");
        // Still outside a fence: the streaming line was not committed.
        let heading = renderer.commit(&answer("# heading"));
        assert!(heading.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn other_line_kinds_keep_their_semantic_styling() {
        let mut renderer = MarkdownRenderer::new();
        let notice = TranscriptLine {
            kind: LineKind::Notice,
            text: "# not markdown".to_owned(),
        };
        assert_eq!(dump(&renderer.commit(&notice)), dump(&styled_line(&notice)));
    }
}
