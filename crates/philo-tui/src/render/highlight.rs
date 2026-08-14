//! Syntax highlighting for fenced code blocks.
//!
//! syntect is built on fancy-regex (no C dependency). An unrecognised
//! language is not an error: the block degrades to plain code styling, so
//! the text always reaches the user unchanged.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Plain styling for code that has no highlighter.
pub(crate) fn code_style() -> Style {
    Style::default().fg(Color::LightGreen)
}

struct Assets {
    syntaxes: SyntaxSet,
    theme: Theme,
}

/// The default syntax and theme sets, loaded once on the first code block
/// (they cost a few megabytes and most sessions never need them).
fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .or_else(|| themes.themes.values().next().cloned())
            .unwrap_or_default();
        Assets {
            syntaxes: SyntaxSet::load_defaults_nonewlines(),
            theme,
        }
    })
}

/// One code block's highlighting state (syntect carries parser state from
/// line to line, which is why this outlives a single line).
pub(crate) struct CodeHighlighter {
    inner: Option<HighlightLines<'static>>,
}

impl CodeHighlighter {
    /// Builds a highlighter for the fence's info string; an unknown or
    /// missing language yields the plain-text fallback.
    pub(crate) fn for_language(language: &str) -> Self {
        let token = language.split_whitespace().next().unwrap_or_default();
        if token.is_empty() {
            return Self { inner: None };
        }
        let assets = assets();
        let syntax = assets
            .syntaxes
            .find_syntax_by_token(token)
            .or_else(|| assets.syntaxes.find_syntax_by_extension(token));
        Self {
            inner: syntax.map(|syntax| HighlightLines::new(syntax, &assets.theme)),
        }
    }

    /// Whether this block has a real highlighter behind it.
    #[cfg(test)]
    pub(crate) fn is_highlighted(&self) -> bool {
        self.inner.is_some()
    }

    /// Styled fragments for one line, or `None` when the caller should fall
    /// back to plain code styling.
    pub(crate) fn line(&mut self, text: &str) -> Option<Vec<(Style, String)>> {
        let highlighter = self.inner.as_mut()?;
        let regions = highlighter.highlight_line(text, &assets().syntaxes).ok()?;
        Some(
            regions
                .into_iter()
                .map(|(style, fragment)| (convert(style), fragment.to_owned()))
                .collect(),
        )
    }
}

fn convert(style: syntect::highlighting::Style) -> Style {
    let mut converted = Style::default().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        converted = converted.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        converted = converted.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        converted = converted.add_modifier(Modifier::UNDERLINED);
    }
    converted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_language_produces_several_coloured_regions() {
        let mut highlighter = CodeHighlighter::for_language("rust");
        assert!(highlighter.is_highlighted());
        let regions = highlighter.line("fn main() {}").expect("highlights");
        assert!(regions.len() > 1, "keywords and names differ: {regions:?}");
        let text: String = regions.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(text, "fn main() {}", "text survives verbatim");
    }

    #[test]
    fn aliases_resolve_and_unknown_languages_degrade() {
        assert!(CodeHighlighter::for_language("rs").is_highlighted());
        assert!(CodeHighlighter::for_language("py").is_highlighted());

        let mut plain = CodeHighlighter::for_language("not-a-language");
        assert!(!plain.is_highlighted());
        assert!(plain.line("anything").is_none());

        let mut bare = CodeHighlighter::for_language("");
        assert!(!bare.is_highlighted());
        assert!(bare.line("anything").is_none());
    }
}
