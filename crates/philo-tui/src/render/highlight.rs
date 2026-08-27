//! Syntax highlighting for fenced code blocks.
//!
//! syntect stays as the parser (grammar definitions reused), but the
//! base16-ocean theme is gone: the P4 remap owns a hand-written brand token
//! table (v4.0 §6 / P4 §3.2), so every region is painted by scope onto the
//! fixed v4.0 palette — keyword orange bold, strings green, function names
//! blue, comments gray, numbers yellow. An unrecognised language is not an
//! error: the block degrades to plain code styling, so the text always
//! reaches the user unchanged.
//!
//! State is kept per block (parser state plus the running scope stack), so
//! multi-line constructs highlight correctly across `line()` calls.

use std::sync::OnceLock;

use ratatui::style::Style;
use syntect::easy::ScopeRegionIterator;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use super::theme;

struct Assets {
    syntaxes: SyntaxSet,
}

/// The default syntax set, loaded once on the first code block (it costs a
/// few megabytes and most sessions never need it).
fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| Assets {
        syntaxes: SyntaxSet::load_defaults_nonewlines(),
    })
}

/// One code block's highlighting state (syntect carries parser state and
/// the scope stack from line to line, which is why this outlives a line).
pub(crate) struct CodeHighlighter {
    inner: Option<State>,
}

struct State {
    parse: ParseState,
    stack: ScopeStack,
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
            inner: syntax.map(|syntax| State {
                parse: ParseState::new(syntax),
                stack: ScopeStack::new(),
            }),
        }
    }

    /// Whether this block has a real highlighter behind it.
    #[cfg(test)]
    pub(crate) fn is_highlighted(&self) -> bool {
        self.inner.is_some()
    }

    /// Brand-styled fragments for one line, or `None` when the caller
    /// should fall back to plain code styling.
    pub(crate) fn line(&mut self, text: &str) -> Option<Vec<(Style, String)>> {
        let state = self.inner.as_mut()?;
        let ops = state.parse.parse_line(text, &assets().syntaxes).ok()?;
        let mut regions = Vec::new();
        for (fragment, op) in ScopeRegionIterator::new(&ops, text) {
            if state.stack.apply(op).is_err() {
                continue;
            }
            if fragment.is_empty() {
                continue;
            }
            regions.push((map_scope(&state.stack), fragment.to_owned()));
        }
        Some(regions)
    }
}

/// Maps the innermost matching scope of the running stack onto the fixed
/// brand palette. Unmapped scopes fall back to the primary body tone.
fn map_scope(stack: &ScopeStack) -> Style {
    for scope in stack.as_slice().iter().rev() {
        if let Some(style) = brand_token(scope) {
            return style;
        }
    }
    theme::primary()
}

/// One scope name → brand token. Operators explicitly stay out so `= + -`
/// do not get painted; the demo keeps them plain.
fn brand_token(scope: &Scope) -> Option<Style> {
    let name = scope.to_string();
    if name.contains("comment") {
        return Some(theme::meta());
    }
    if name.contains("string") {
        return Some(theme::ok());
    }
    // The old syntect theme painted operators; the v4.0 brand canvas keeps
    // them quiet unless a more specific scope above this one claims them.
    if name.contains("keyword.operator") || name.contains("punctuation") {
        return None;
    }
    if name.contains("keyword")
        || name.contains("storage")
        || name.contains("variable.language")
    {
        return Some(theme::keyword_style());
    }
    if name.contains("entity.name.function") {
        return Some(theme::bold_info());
    }
    if name.contains("entity.name.type") || name.contains("support.type") {
        return Some(theme::info());
    }
    if name.contains("constant") {
        return Some(theme::warn());
    }
    None
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

    #[test]
    fn typescript_tokens_map_onto_the_same_brand_palette() {
        // Probe the default set for whatever TypeScript token resolves; the
        // golden below then runs against it.
        let ss = syntect::parsing::SyntaxSet::load_defaults_nonewlines();
        let token = ["typescript", "ts", "TypeScript", "tsx"]
            .into_iter()
            .find(|candidate| {
                ss.find_syntax_by_token(candidate).is_some()
                    || ss.find_syntax_by_extension(candidate).is_some()
            })
            .unwrap_or("js");
        let mut highlighter = CodeHighlighter::for_language(token);
        assert!(highlighter.is_highlighted(), "no TS token resolved");
        // The anti-glare default (Recommended tier) sheds the keyword
        // weight: keywords ride plain accent (new-color.md §3.1).
        let keyword = theme::keyword_style();
        let green = theme::ok().fg;
        let gray = theme::meta().fg;
        let yellow = theme::warn().fg;

        let head = highlighter.line("export function load() {").expect("highlights");
        assert!(
            head.iter()
                .any(|(style, text)| text.contains("export") && *style == keyword),
            "TS keywords ride the theme keyword style: {head:?}"
        );

        let body = highlighter
            .line("    return fs.readFile(\"a.ts\"); // sync")
            .expect("highlights");
        assert!(
            body.iter()
                .any(|(style, text)| text.contains("return") && *style == keyword),
            "TS keywords ride the theme keyword style: {body:?}"
        );
        assert!(
            body.iter()
                .any(|(style, text)| text.contains("a.ts") && style.fg == green),
            "TS strings ride helper green: {body:?}"
        );
        assert!(
            body.iter()
                .any(|(style, text)| text.contains("sync") && style.fg == gray),
            "TS comments ride annotation gray: {body:?}"
        );
        let number = highlighter
            .line("    const n = 42; // count")
            .expect("highlights");
        assert!(
            number
                .iter()
                .any(|(style, text)| text.contains("42") && style.fg == yellow),
            "TS numbers ride warning yellow: {number:?}"
        );
    }

    #[test]
    fn rust_tokens_map_onto_the_brand_palette() {
        let mut highlighter = CodeHighlighter::for_language("rust");
        let keyword = theme::keyword_style();
        let blue_bold = theme::bold_info();
        let green = theme::ok().fg;
        let gray = theme::meta().fg;
        let yellow = theme::warn().fg;

        let head = highlighter.line("fn main() {").expect("highlights");
        assert!(
            head.iter()
                .any(|(style, text)| text.contains("fn") && *style == keyword),
            "keywords ride the theme keyword style: {head:?}"
        );
        assert!(
            head.iter()
                .any(|(style, text)| text.contains("main") && *style == blue_bold),
            "function names ride blue bold: {head:?}"
        );

        let body = highlighter
            .line("    let s = \"hi\"; // note")
            .expect("highlights");
        assert!(
            body.iter()
                .any(|(style, text)| text.contains("hi") && style.fg == green),
            "strings ride helper green: {body:?}"
        );
        assert!(
            body.iter()
                .any(|(style, text)| text.contains("note") && style.fg == gray),
            "comments ride annotation gray: {body:?}"
        );

        let number = highlighter.line("    let n = 42;").expect("highlights");
        assert!(
            number
                .iter()
                .any(|(style, text)| text.contains("42") && style.fg == yellow),
            "numbers ride warning yellow: {number:?}"
        );
    }
}