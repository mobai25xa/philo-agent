//! Answer-prose projection: block roles and baked spans for stateless
//! markdown painting.
//!
//! History repaints arbitrary visible slices every frame, so paint must be
//! a pure function of the row it is handed — renderer memory cannot survive
//! scrolling, resize, or replay. Block structure (code fences, GFM tables)
//! is therefore classified once per answer cell at wrap time: [`classify`]
//! labels each logical line, and [`project_answer`] parses each line into
//! semantically styled [`ProseSpan`]s **once**, wraps them across the
//! width, and stores the fragments in the wrap cache. The render layer only
//! converts semantics to theme tokens (fenced bodies stay raw text so
//! syntect keeps painting them there).
//!
//! Baking spans at projection is also the perf win: markdown parsing costs
//! one pass per width change, not per frame per visible row.
//!
//! Classification is deterministic by construction: fences win first (a
//! ``` ``run`` block owns everything until its matching close), then tables settle
//! only outside fences (a pipe row confirmed by a following delimiter
//! row). Pipe rows never settled by a delimiter stay ordinary prose — the
//! flush rule is implicit in the two-pass shape, with no buffering.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_segmentation::UnicodeSegmentation;

use super::text;

/// One projected display row: a wrapped fragment plus its block role. The
/// wrap cache stores these per cell; every fragment of one logical line
/// repeats that line's role.
///
/// `text` is always the plain fragment text (the copy/selection geometry
/// contract); for styled rows it equals the concatenation of `spans`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectedRow {
    pub(crate) role: BlockRole,
    pub(crate) text: String,
    /// Pre-styled spans when the fragment carries its final presentation.
    /// `None` only for fenced code bodies — syntect paints those at render
    /// time from the role's language tag.
    pub(crate) spans: Option<Vec<ProseSpan>>,
    /// Per-row wash intent (v4.0 P3 diff bodies). `None` everywhere else;
    /// `Some(DiffDel | DiffIns)` gives the shell its background fill.
    pub(crate) tone: Option<super::transcript::Tone>,
    /// v4.0 P4: the right-padded code-fence line-number slot (`" 3"`,
    /// `"42"`, or all spaces on wrapped continuation rows). The render
    /// layer paints it before the BORDER `│` gutter. `None` outside fenced
    /// code bodies.
    pub(crate) code_line: Option<String>,
}

impl ProjectedRow {
    /// A row outside any special block (also every non-answer kind).
    pub(crate) fn plain(text: impl Into<String>) -> Self {
        Self {
            role: BlockRole::Plain,
            text: text.into(),
            spans: None,
            tone: None,
            code_line: None,
        }
    }

    /// A styled row: text derives from the spans so visual width, selection
    /// geometry, and paint never disagree.
    pub(crate) fn styled(role: BlockRole, spans: Vec<ProseSpan>) -> Self {
        Self {
            role,
            text: spans.iter().map(|span| span.text.as_str()).collect(),
            spans: Some(spans),
            tone: None,
            code_line: None,
        }
    }

    /// A styled row carrying a wash tone (diff del/ins bodies).
    pub(crate) fn styled_with_tone(
        role: BlockRole,
        spans: Vec<ProseSpan>,
        tone: super::transcript::Tone,
    ) -> Self {
        Self {
            tone: Some(tone),
            ..Self::styled(role, spans)
        }
    }

    /// A fenced code body: raw text; syntect styles it later. `slot` is the
    /// right-padded line-number slot (or all-spaces continuation gutter).
    fn code(text: String, lang: String, slot: Option<String>) -> Self {
        Self {
            role: BlockRole::FenceBody { lang },
            text,
            spans: None,
            tone: None,
            code_line: slot,
        }
    }
}

/// Semantic color slot of a baked span. App-side vocabulary only — the
/// render layer spends theme tokens to realize it. The tool-card families
/// (v4.0 P3) ride the same span machinery as prose so one wrap cache and
/// one paint path cover every typed row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ProseColor {
    /// Primary foreground.
    #[default]
    Default,
    /// Dim chrome: markers, bars, rules, table frames.
    Meta,
    /// Universal link blue.
    Link,
    /// Brand orange: inline code, document skeleton highlights (headings,
    /// quote bars, checked boxes, language tags, table headers) and the
    /// tool-card edit family.
    Code,
    /// Brand orange alias used by document skeleton highlights.
    Accent,
    /// Dark-gray hints: line numbers, timestamps, card durations.
    DarkGray,
    /// Success green: paths, line deltas, ready state, read/write cards.
    Green,
    /// Warning yellow: warnings, Running state, spinners.
    Yellow,
    /// Error red: failures, deletion, confirm borders.
    Red,
    /// Information blue: model names, function names, hunk heads. Exposed
    /// as a token for tool-card hunk heads and future surfaces.
    #[allow(dead_code)]
    Blue,
    /// Border/separator color: gutter pipes, dot leaders.
    Border,
    /// Bold white (#FFFFFF): H2 rungs and `**emphasis**` body words.
    White,
}

/// Presentation-neutral span style. Covers every prose combination
/// (bold+italic+link…) as flat flags so a wrapped continuation can carry
/// its context verbatim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProseStyle {
    pub(crate) color: ProseColor,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    pub(crate) crossed: bool,
}

impl ProseStyle {
    pub(crate) fn colored(mut self, color: ProseColor) -> Self {
        self.color = color;
        self
    }

    pub(crate) fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub(crate) fn bold_if(mut self, on: bool) -> Self {
        if on {
            self.bold = true;
        }
        self
    }

    fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    fn underlined(mut self) -> Self {
        self.underline = true;
        self
    }

    fn crossed(mut self) -> Self {
        self.crossed = true;
        self
    }
}

/// One baked span of an answer row: text plus semantic style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProseSpan {
    pub(crate) text: String,
    pub(crate) style: ProseStyle,
}

impl ProseSpan {
    pub(crate) fn new(text: impl Into<String>, style: ProseStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    fn raw(text: impl Into<String>) -> Self {
        Self::new(text, ProseStyle::default())
    }

    fn width(&self) -> usize {
        text::width(&self.text)
    }
}

/// Display width of one styled run.
fn spans_width(spans: &[ProseSpan]) -> usize {
    spans.iter().map(ProseSpan::width).sum()
}

/// Block-level role of one answer row (prose-typography plan P0/P2).
///
/// Headings gain a dedicated variant (P4 block-gap support) so the
/// projection can place breathing rows around them without re-parsing. The
/// span styling still flows through pulldown-cmark in `parse_prose`; the
/// variant only marks structure for `block_gap` and any future anchors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BlockRole {
    /// Ordinary prose.
    Plain,
    /// An ATX heading (`#` … `######`), any level.
    Heading,
    /// A fence delimiter line itself (` ``` ` opener or closer): dim chrome.
    FenceEdge,
    /// A body row inside the fence opened with this language tag.
    FenceBody { lang: String },
    /// Header row of a settled GFM table.
    TableHeader,
    /// The `|---|---|` delimiter row of a settled table.
    TableDelim,
    /// A body row of a settled table.
    TableBody,
}

/// One logical line of an answer cell plus its classified role.
pub(crate) struct LogicalLine {
    pub(crate) text: String,
    pub(crate) role: BlockRole,
}

/// Labels every logical line of one answer cell.
///
/// Two deterministic passes over the cell's `\n`-split lines:
///
/// 1. **Fences**: a ```/~~~ run opens until a matching bare close; its
///    edges are [`BlockRole::FenceEdge`], its interior
///    [`BlockRole::FenceBody`] carrying the opener's language tag. An
///    unclosed fence runs to the end of the cell — the streaming case.
/// 2. **Tables** (outside fences only): a pipe row settled by a following
///    delimiter row becomes a table header; the delimiter and subsequent
///    pipe rows become the table body until a non-pipe row ends it.
pub(crate) fn classify(answer: &str) -> Vec<LogicalLine> {
    let mut lines: Vec<LogicalLine> = answer
        .split('\n')
        .map(|text| LogicalLine {
            text: text.to_owned(),
            role: BlockRole::Plain,
        })
        .collect();

    let mut fence: Option<(char, String)> = None;
    for line in &mut lines {
        match &fence {
            None => {
                if let Some((marker, lang)) = fence_run(&line.text) {
                    line.role = BlockRole::FenceEdge;
                    fence = Some((marker, lang));
                } else if is_atx_heading(&line.text) {
                    line.role = BlockRole::Heading;
                }
            }
            Some((marker, lang)) => {
                if closes_fence(&line.text, *marker) {
                    line.role = BlockRole::FenceEdge;
                    fence = None;
                } else {
                    line.role = BlockRole::FenceBody { lang: lang.clone() };
                }
            }
        }
    }

    let mut index = 0;
    while index + 1 < lines.len() {
        if lines[index].role != BlockRole::Plain
            || !is_pipe_row(&lines[index].text)
            || lines[index + 1].role != BlockRole::Plain
            || !is_delim_row(&lines[index + 1].text)
        {
            index += 1;
            continue;
        }
        lines[index].role = BlockRole::TableHeader;
        lines[index + 1].role = BlockRole::TableDelim;
        index += 2;
        while index < lines.len()
            && lines[index].role == BlockRole::Plain
            && is_pipe_row(&lines[index].text)
        {
            lines[index].role = BlockRole::TableBody;
            index += 1;
        }
    }

    lines
}

/// Wraps an answer cell into display rows at `width`, baking styled spans
/// into each fragment. Pure: same cell text and width, same rows.
pub(crate) fn project_answer(answer: &str, width: usize) -> Vec<ProjectedRow> {
    if width == 0 {
        return Vec::new();
    }
    let lines = classify(answer);
    // v4.0 P4 §3.1: fence bodies number 1..N within their fence, right
    // padded to the widest number so the `│` gutters line up. One pre-pass
    // pairs every body line with its slot (this is a presentation fact).
    let mut code_line: Vec<Option<String>> = vec![None; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        if lines[index].role != BlockRole::FenceEdge {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        while end < lines.len() && lines[end].role != BlockRole::FenceEdge {
            end += 1;
        }
        let total = (end - index - 1).max(1);
        let digits = total.to_string().len();
        for (offset, body) in ((index + 1)..end).enumerate() {
            code_line[body] = Some(format!("{:>digits$}", offset + 1));
        }
        index = end;
    }

    let mut rows = Vec::new();
    let mut index = 0;
    let mut prev_role: Option<BlockRole> = None;
    while index < lines.len() {
        let line = &lines[index];
        if let Some(n) = block_gap_rows(prev_role.as_ref(), &line.role) {
            // Idempotent: don't amplify a blank the author already wrote.
            let last_blank = rows
                .last()
                .map(|row: &ProjectedRow| row.text.is_empty())
                .unwrap_or(true);
            if !last_blank {
                for _ in 0..n {
                    rows.push(ProjectedRow::plain(String::new()));
                }
            }
        }
        prev_role = Some(line.role.clone());
        match &line.role {
            BlockRole::TableHeader => {
                let end = table_run_end(&lines, index);
                rows.extend(project_table(&lines[index..end], width));
                index = end;
            }
            BlockRole::FenceBody { lang } => {
                let slot = code_line[index].clone();
                let gutter = slot.as_ref().map(String::len).unwrap_or(0) + 2;
                let content_width = width.saturating_sub(gutter).max(1);
                rows.extend(
                    text::wrap(&line.text, content_width)
                        .into_iter()
                        .enumerate()
                        .map(|(fragment, text)| {
                            let slot = match (&slot, fragment) {
                                (Some(slot), 0) => Some(slot.clone()),
                                (Some(slot), _) => Some(" ".repeat(slot.len())),
                                (None, _) => None,
                            };
                            ProjectedRow::code(text, lang.clone(), slot)
                        }),
                );
                index += 1;
            }
            BlockRole::FenceEdge => {
                // The raw marker run stays chrome; a non-empty info string
                // reads as the language chip. Text stays verbatim either way.
                let indent: String = line
                    .text
                    .chars()
                    .take_while(|ch| ch.is_whitespace())
                    .collect();
                let rest = &line.text[indent.len()..];
                let marker = rest.chars().next();
                let run_bytes = if matches!(marker, Some('`') | Some('~')) {
                    rest.find(|ch: char| ch != marker.unwrap())
                        .unwrap_or(rest.len())
                } else {
                    0
                };
                let (run_text, tail) = rest.split_at(run_bytes);
                let mut spans = Vec::new();
                if !indent.is_empty() {
                    spans.push(ProseSpan::new(indent, bar()));
                }
                spans.push(ProseSpan::new(run_text, bar()));
                if !tail.trim().is_empty() {
                    spans.push(ProseSpan::new(tail, accent()));
                } else if !tail.is_empty() {
                    spans.push(ProseSpan::new(tail, bar()));
                }
                rows.extend(
                    wrap_spans(&spans, width)
                        .into_iter()
                        .map(|row_spans| ProjectedRow::styled(line.role.clone(), row_spans)),
                );
                index += 1;
            }
            _ => {
                rows.extend(
                    wrap_spans(&parse_prose(&line.text), width)
                        .into_iter()
                        .map(|spans| ProjectedRow::styled(line.role.clone(), spans)),
                );
                index += 1;
            }
        }
    }
    rows
}

/// Collapses a [`BlockRole`] into a coarse structural category so the
/// fence and the table each read as one block (no internal gaps even though
/// their edge/body/delim rows carry distinct variants).
fn block_category(role: &BlockRole) -> &'static str {
    match role {
        BlockRole::Plain => "plain",
        BlockRole::Heading => "heading",
        BlockRole::FenceEdge | BlockRole::FenceBody { .. } => "fence",
        BlockRole::TableHeader | BlockRole::TableDelim | BlockRole::TableBody => "table",
    }
}

/// How many `GAP_BLOCK` blank rows to insert between `prev` and `cur`.
/// Returns `None` (= 0) for same-category transitions and the very first
/// row — so consecutive prose lines, fence interiors, and table interiors
/// stay tight. Cross-category transitions (prose↔heading, prose↔fence,
/// prose↔table, heading↔table, …) get one breathing row.
///
/// The caller enforces idempotence over source blank lines (it skips the
/// insert when the last emitted row was already blank), so an author's
/// `\n\n` is never amplified into two blanks.
fn block_gap_rows(prev: Option<&BlockRole>, cur: &BlockRole) -> Option<usize> {
    let prev = prev?;
    if block_category(prev) == block_category(cur) {
        return None;
    }
    Some(crate::render::theme::GAP_BLOCK)
}

// ---------------------------------------------------------------------------
// Inline prose parsing (semantic spans)
// ---------------------------------------------------------------------------

/// Parses one logical prose line into semantic spans. This is the whole of
/// the answer body's markdown vocabulary — block structure arrived via
/// [`BlockRole`], so only inline constructs are handled here.
fn parse_prose(text: &str) -> Vec<ProseSpan> {
    let indent: String = text.chars().take_while(|ch| *ch == ' ').collect();
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let mut spans: Vec<ProseSpan> = Vec::new();
    let mut styles = vec![ProseStyle::default()];
    let mut ordered: Option<u64> = None;
    // Set while inside a checked task item's body; cleared at item end.
    let mut struck_task = false;

    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let style = heading_style(level);
                spans.push(ProseSpan::new("▍ ", style));
                styles.push(style);
            }
            Event::End(TagEnd::Heading(_)) => pop(&mut styles),
            Event::Start(Tag::List(start)) => ordered = start,
            Event::End(TagEnd::List(_)) => ordered = None,
            Event::Start(Tag::Item) => {
                struck_task = false;
                // v4.0 §6: bullet `•` and ordered numbers ride brand
                // orange (demo `::before` bolds the bullet).
                let marker = match ordered {
                    Some(number) => format!("{number}. "),
                    None => "• ".to_owned(),
                };
                spans.push(ProseSpan::new(
                    format!("{indent}{marker}"),
                    ProseStyle::default()
                        .colored(ProseColor::Code)
                        .bold_if(ordered.is_none()),
                ));
            }
            Event::End(TagEnd::Item) => {
                if struck_task {
                    pop(&mut styles);
                    struck_task = false;
                }
            }
            // The literal `[x]` / `[ ]` glyphs stay (width safety); done
            // rows strike and dim their remaining body, and the checked
            // box itself lights up accent.
            Event::TaskListMarker(checked) => {
                spans.push(ProseSpan::new(
                    format!("[{}] ", if checked { "x" } else { " " }),
                    if checked { accent() } else { bar() },
                ));
                if checked {
                    styles.push(ProseStyle::default().colored(ProseColor::Meta).crossed());
                    struck_task = true;
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                // v4.0 §6: the quote bar is a dark-gray `│` hairline; the
                // body stays primary.
                spans.push(ProseSpan::new(
                    format!("{indent}│ "),
                    ProseStyle::default().colored(ProseColor::DarkGray),
                ));
            }
            Event::Start(Tag::Emphasis) => {
                styles.push(top(&styles).italic());
            }
            Event::End(TagEnd::Emphasis) => pop(&mut styles),
            Event::Start(Tag::Strong) => {
                // v4.0 §6: `**emphasis**` lifts to bold white. Inside a
                // colored host (headings, links, table headers) the host
                // color wins and only the weight changes.
                let base = top(&styles);
                let style = if base.color == ProseColor::Default {
                    base.colored(ProseColor::White).bold()
                } else {
                    base.bold()
                };
                styles.push(style);
            }
            Event::End(TagEnd::Strong) => pop(&mut styles),
            Event::Start(Tag::Strikethrough) => styles.push(top(&styles).crossed()),
            Event::End(TagEnd::Strikethrough) => pop(&mut styles),
            Event::Start(Tag::Link { .. }) => {
                styles.push(top(&styles).colored(ProseColor::Link).underlined())
            }
            Event::End(TagEnd::Link) => pop(&mut styles),
            // Indented code: the code hue, without a language tag.
            Event::Start(Tag::CodeBlock(_)) => styles.push(top(&styles).colored(ProseColor::Code)),
            Event::End(TagEnd::CodeBlock) => pop(&mut styles),
            Event::Code(code) => {
                spans.push(ProseSpan::new(code.to_string(), code_style()));
            }
            Event::Text(text) => {
                spans.extend(colorize(&text, top(&styles)));
            }
            Event::SoftBreak | Event::HardBreak => spans.push(ProseSpan::raw(" ")),
            Event::Rule => spans.push(ProseSpan::new(
                // v4.0 §2: the rule run is annotation gray.
                "─".repeat(24),
                ProseStyle::default().colored(ProseColor::Meta),
            )),
            _ => {}
        }
    }
    if spans.is_empty() {
        return vec![ProseSpan::raw(text)];
    }
    spans
}

/// Inline code: helper green foreground, no background block (v4.0 §6 paths
/// are green; the user ruled inline code to a single uniform green — no
/// path-shaped probing, no density rationing). The `Code` hue itself stays
/// orange for list markers and tool-card Edit states.
fn code_style() -> ProseStyle {
    ProseStyle::default().colored(ProseColor::Green)
}

// ---------------------------------------------------------------------------
// Bare-path detection (v4.0 §6 / P4 §4)
// ---------------------------------------------------------------------------

/// Splits a text event into spans, lifting bare file paths to helper green
/// (`src/utils/jwt.ts`, `C:\x\y.rs`). Inline code and link text are skipped
/// (their own semantics own the coloring); URLs are never flagged.
fn colorize(text: &str, style: ProseStyle) -> Vec<ProseSpan> {
    if matches!(style.color, ProseColor::Link | ProseColor::Code) {
        return vec![ProseSpan::new(text.to_owned(), style)];
    }
    let mut out: Vec<ProseSpan> = Vec::new();
    let mut buffer = String::new();
    let flush = |buffer: &mut String, style: ProseStyle, out: &mut Vec<ProseSpan>| {
        if !buffer.is_empty() {
            out.push(ProseSpan::new(std::mem::take(buffer), style));
        }
    };
    for token in text.split_inclusive(char::is_whitespace) {
        let body = token.trim_end();
        let trailing = &token[body.len()..];
        if is_path_like(body) {
            flush(&mut buffer, style, &mut out);
            out.push(ProseSpan::new(
                body.to_owned(),
                style.colored(ProseColor::Green),
            ));
            if !trailing.is_empty() {
                out.push(ProseSpan::new(trailing.to_owned(), style));
            }
        } else {
            buffer.push_str(token);
        }
    }
    flush(&mut buffer, style, &mut out);
    out
}

/// Narrow path probe: a contiguous token carrying `/` or `\` and ending in a
/// file-like extension (`src/utils/jwt.ts`, `C:\x\y.rs`). URLs and plain
/// words are rejected.
fn is_path_like(token: &str) -> bool {
    if token.is_empty() || token.contains("://") {
        return false;
    }
    let core = token.trim_matches(|c: char| {
        matches!(
            c,
            '.' | '(' | ')' | '[' | ']' | ',' | ';' | ':' | '"' | '\''
                | '`' | '，' | '。' | '、' | '；' | '：' | '（' | '）' | '」' | '』' | '『' | '「'
        )
    });
    if !(core.contains('/') || core.contains('\\')) {
        return false;
    }
    match core.rsplit_once('.') {
        Some((_, extension)) => {
            !extension.is_empty()
                && !extension.contains('/')
                && !extension.contains('\\')
                && extension
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        }
        None => false,
    }
}

/// The heading ladder (v4.0 §6 / decision D10): `▍ ` hangs every rung; H1
/// is orange bold without the old underline, H2 lifts to bold white, deeper
/// rungs fall back to primary weight.
fn heading_style(level: HeadingLevel) -> ProseStyle {
    let base = ProseStyle::default().bold();
    match level {
        HeadingLevel::H1 => base.colored(ProseColor::Accent),
        HeadingLevel::H2 => base.colored(ProseColor::White),
        _ => base,
    }
}

fn top(styles: &[ProseStyle]) -> ProseStyle {
    styles.last().copied().unwrap_or_default()
}

fn pop(styles: &mut Vec<ProseStyle>) {
    if styles.len() > 1 {
        styles.pop();
    }
}

// ---------------------------------------------------------------------------
// Span-aware wrapping
// ---------------------------------------------------------------------------

/// Soft-wraps styled spans into display rows of at most `width` cells,
/// never splitting a grapheme. Breaks at the last space of the row when
/// there is one (words stay whole — `index.html` must never shatter into
/// `inde/x.ht/ml`); falls back to a hard break for spaceless runs (CJK
/// wraps per character, as it should). Style context survives every
/// break: a fragment continues its source span's style verbatim.
pub(crate) fn wrap_spans(input: &[ProseSpan], width: usize) -> Vec<Vec<ProseSpan>> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows: Vec<Vec<ProseSpan>> = Vec::new();
    let mut current: Vec<ProseSpan> = Vec::new();
    let mut used = 0usize;

    for span in input {
        for grapheme in span.text.graphemes(true) {
            let grapheme_width = text::width(grapheme);
            if used > 0 && used + grapheme_width > width {
                match word_break(&mut current) {
                    Some((tail, tail_used)) => {
                        rows.push(std::mem::take(&mut current));
                        current = tail;
                        used = tail_used;
                    }
                    None => {
                        rows.push(std::mem::take(&mut current));
                        used = 0;
                    }
                }
            }
            match current.last_mut() {
                Some(last) if last.style == span.style => last.text.push_str(grapheme),
                _ => current.push(ProseSpan::new(grapheme, span.style)),
            }
            used += grapheme_width;
        }
    }
    rows.push(current);
    rows
}

/// Splits the accumulated row at its last space: the space stays as a
/// (visually inert) trailing cell, the post-space graphemes seed the next
/// row. Returns the tail spans and their display width, or `None` when the
/// row has no interior space to break at.
fn word_break(current: &mut Vec<ProseSpan>) -> Option<(Vec<ProseSpan>, usize)> {
    for index in (0..current.len()).rev() {
        let text = current[index].text.as_str();
        if let Some(byte_pos) = text.rfind(' ') {
            if byte_pos == 0 && current[..index].iter().all(|s| s.text.is_empty()) {
                return None;
            }
            let style = current[index].style;
            let tail_text = text[byte_pos + 1..].to_owned();
            // The break space itself stays on the row (a trailing cell).
            current[index].text.truncate(byte_pos + 1);
            let mut tail = Vec::new();
            if !tail_text.is_empty() {
                tail.push(ProseSpan::new(tail_text, style));
            }
            tail.extend(current[index + 1..].iter().cloned());
            // The moved spans leave the head — they live in the tail now.
            current.truncate(index + 1);
            let tail_used = tail.iter().map(|span| text::width(&span.text)).sum();
            return Some((tail, tail_used));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// GFM tables
// ---------------------------------------------------------------------------

/// End (exclusive) of the contiguous table run starting at `start`.
fn table_run_end(lines: &[LogicalLine], start: usize) -> usize {
    let mut end = start + 1;
    while end < lines.len()
        && matches!(
            lines[end].role,
            BlockRole::TableDelim | BlockRole::TableBody | BlockRole::TableHeader
        )
    {
        end += 1;
    }
    end
}

/// Splits one pipe row into trimmed cell texts. Outer pipes are optional
/// (GFM tolerates their absence); excess cells beyond the header count are
/// dropped by the caller per the GFM rule.
fn split_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);
    trimmed.split('|').map(|cell| cell.trim().to_owned()).collect()
}

/// Projects a settled table run into a full-frame gridded table (or the
/// flow fallback when even one cell per column cannot fit). Column widths
/// are the max natural width over **every** row's cells (prose v4: the old
/// header-only rule crushed wide body content into sliver columns next to
/// a half-empty screen). When the natural grid overflows the column, the
/// widest columns shrink first (water-filling) so narrow columns keep
/// their content on one line; only the squeezed columns wrap internally.
fn project_table(rows: &[LogicalLine], width: usize) -> Vec<ProjectedRow> {
    let column_count = split_cells(&rows[0].text).len();
    let mut naturals = vec![0usize; column_count];
    for row in rows {
        if row.role == BlockRole::TableDelim {
            continue;
        }
        for (column, cell) in split_cells(&row.text).into_iter().enumerate().take(column_count) {
            naturals[column] = naturals[column].max(text::width(&cell).max(1));
        }
    }
    // "│ c1 │ c2 │": bars + single-space padding around every column.
    let frame_overhead = 3 * column_count + 1;
    let budget = width.saturating_sub(frame_overhead);
    let widths = match budget >= column_count {
        true => waterfill(&naturals, budget),
        false => {
            // Not even one cell per column: the whole run degrades to the
            // dim pipe flow, frames included.
            let mut out = Vec::new();
            for row in rows {
                out.extend(flow_rows(&row.text, width, row.role.clone()));
            }
            return out;
        }
    };
    let mut out = vec![frame_row(&widths, "╭", "┬", "╮")];
    for row in rows {
        let role = row.role.clone();
        if role == BlockRole::TableDelim {
            out.push(frame_row(&widths, "├", "┼", "┤"));
        } else {
            let is_header = role == BlockRole::TableHeader;
            out.extend(grid_rows(&split_cells(&row.text), &widths, is_header, role));
        }
    }
    out.push(frame_row(&widths, "╰", "┴", "╯"));
    out
}

/// Distributes `budget` display cells over the columns. Natural widths
/// win when they fit outright; otherwise columns are processed widest
/// first and each takes `min(natural, remaining / columns_left)`, so the
/// burden falls on the columns with room to wrap. Stable and deterministic.
fn waterfill(naturals: &[usize], budget: usize) -> Vec<usize> {
    let count = naturals.len();
    if naturals.iter().sum::<usize>() <= budget {
        return naturals.to_vec();
    }
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(naturals[i]));
    let mut widths = vec![0usize; count];
    let mut remaining = budget;
    for (rank, &column) in order.iter().enumerate() {
        let share = (remaining / (count - rank)).max(1);
        let taken = naturals[column].min(share);
        widths[column] = taken;
        remaining -= taken;
    }
    widths
}

/// One full-frame border row (`╭─┬─╮`, `├─┼─┤`, `╰─┴─╯`). Each segment
/// spans its column plus the single-space padding so junctions line up
/// with the data-row bars exactly.
fn frame_row(widths: &[usize], left: &str, mid: &str, right: &str) -> ProjectedRow {
    let mut spans = vec![ProseSpan::new(left, border())];
    for (column, &width) in widths.iter().enumerate() {
        if column > 0 {
            spans.push(ProseSpan::new(mid, border()));
        }
        spans.push(ProseSpan::new("─".repeat(width + 2), border()));
    }
    spans.push(ProseSpan::new(right, border()));
    ProjectedRow::styled(BlockRole::TableDelim, spans)
}

/// One grid line per wrapped row of the tallest column; other columns pad
/// or blank to keep the columns aligned.
fn grid_rows(
    cells: &[String],
    widths: &[usize],
    is_header: bool,
    role: BlockRole,
) -> Vec<ProjectedRow> {
    let content_style = |style: ProseStyle| {
        if is_header {
            // Header text carries the accent, mirroring tool-card titles.
            style.bold().colored(ProseColor::Accent)
        } else {
            style
        }
    };
    let wrapped_columns: Vec<Vec<Vec<ProseSpan>>> = widths
        .iter()
        .enumerate()
        .map(|(column, &width)| {
            let cell = cells.get(column).map(String::as_str).unwrap_or("");
            let spans: Vec<ProseSpan> = parse_prose(cell)
                .into_iter()
                .map(|span| ProseSpan::new(span.text, content_style(span.style)))
                .collect();
            wrap_spans(&spans, width)
        })
        .collect();
    let height = wrapped_columns.iter().map(Vec::len).max().unwrap_or(1);

    let mut out = Vec::with_capacity(height);
    for row_index in 0..height {
        let mut spans = vec![ProseSpan::new("│ ", border())];
        for (column, &width) in widths.iter().enumerate() {
            let line = wrapped_columns[column].get(row_index);
            if let Some(line) = line {
                spans.extend(line.iter().cloned());
            }
            let used = line.map(Vec::as_slice).map(spans_width).unwrap_or(0);
            if used < width {
                spans.push(ProseSpan::raw(" ".repeat(width - used)));
            }
            spans.push(ProseSpan::new(
                if column + 1 == widths.len() { " │" } else { " │ " },
                border(),
            ));
        }
        out.push(ProjectedRow::styled(role.clone(), spans));
    }
    out
}

/// Flow fallback: the row keeps its exact text, with pipes dimmed so the
/// tabular intent still reads on narrow screens.
fn flow_rows(text: &str, width: usize, role: BlockRole) -> Vec<ProjectedRow> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    for ch in text.chars() {
        if ch == '|' {
            if !plain.is_empty() {
                spans.push(ProseSpan::raw(std::mem::take(&mut plain)));
            }
            spans.push(ProseSpan::new("|", border()));
        } else {
            plain.push(ch);
        }
    }
    if !plain.is_empty() {
        spans.push(ProseSpan::raw(plain));
    }
    wrap_spans(&spans, width)
        .into_iter()
        .map(|row_spans| ProjectedRow::styled(role.clone(), row_spans))
        .collect()
}

fn bar() -> ProseStyle {
    ProseStyle::default().colored(ProseColor::Meta)
}

/// Brand-orange highlight for document skeleton elements.
fn accent() -> ProseStyle {
    ProseStyle::default().colored(ProseColor::Accent)
}

/// v4.0 §6: GFM table frame / gutter lines ride the BORDER token instead
/// of the old meta gray.
fn border() -> ProseStyle {
    ProseStyle::default().colored(ProseColor::Border)
}

/// A fence opener and its info string, if this line is one. Leading
/// whitespace is allowed; three or more marker chars are required.
fn fence_run(text: &str) -> Option<(char, String)> {
    let trimmed = text.trim_start();
    let marker = trimmed
        .chars()
        .next()
        .filter(|ch| *ch == '`' || *ch == '~')?;
    let run = trimmed.chars().take_while(|ch| *ch == marker).count();
    if run < 3 {
        return None;
    }
    Some((marker, trimmed[run..].trim().to_owned()))
}

/// Whether this line closes a fence opened with `marker`: the same marker
/// run carrying no info string.
fn closes_fence(text: &str, marker: char) -> bool {
    matches!(fence_run(text), Some((found, rest)) if found == marker && rest.is_empty())
}

/// ATX heading detection for block-gap structure: 1–6 leading `#` followed
/// by a space or end-of-line (`# Title`, `## Mid`, `#### `). Setext
/// underlines (`===`/`---`) are intentionally not flagged — `---` also means
/// a thematic break, and pulldown-cmark already emits the heading span via
/// `parse_prose`, so we only need the structural marker for `block_gap`.
fn is_atx_heading(text: &str) -> bool {
    let trimmed = text.trim_start();
    let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&hashes) {
        return false;
    }
    let rest = trimmed[hashes..].chars().next();
    matches!(rest, None | Some(' ') | Some('\t'))
}

fn is_pipe_row(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty() && trimmed.contains('|')
}

/// A GFM delimiter row: pipes and dashes (colons allowed for alignment),
/// at least one dash, nothing else.
fn is_delim_row(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.contains('|')
        && trimmed.contains('-')
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '|' | ':' | '-' | ' '))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(answer: &str) -> Vec<BlockRole> {
        classify(answer).into_iter().map(|line| line.role).collect()
    }

    fn texts(rows: &[ProjectedRow]) -> Vec<&str> {
        rows.iter().map(|row| row.text.as_str()).collect()
    }

    #[test]
    fn prose_without_blocks_stays_plain() {
        assert_eq!(
            roles("# Title\nbody text\n- item"),
            vec![BlockRole::Heading, BlockRole::Plain, BlockRole::Plain]
        );
    }

    #[test]
    fn fences_pair_and_capture_the_language() {
        assert_eq!(
            roles("```rust\nlet x = 1;\n```"),
            [
                BlockRole::FenceEdge,
                BlockRole::FenceBody {
                    lang: "rust".to_owned()
                },
                BlockRole::FenceEdge,
            ]
        );
    }

    #[test]
    fn unknown_languages_still_classify_as_a_body() {
        assert_eq!(
            roles("```not-a-language\n* not a list *\n```"),
            [
                BlockRole::FenceEdge,
                BlockRole::FenceBody {
                    lang: "not-a-language".to_owned()
                },
                BlockRole::FenceEdge,
            ]
        );
    }

    #[test]
    fn tilde_fences_match_their_own_marker_only() {
        assert_eq!(
            roles("~~~\ntext\n```\nstill inside\n~~~"),
            [
                BlockRole::FenceEdge,
                BlockRole::FenceBody {
                    lang: String::new()
                },
                BlockRole::FenceBody {
                    lang: String::new()
                },
                BlockRole::FenceBody {
                    lang: String::new()
                },
                BlockRole::FenceEdge,
            ]
        );
    }

    #[test]
    fn an_unclosed_fence_runs_to_the_cell_end() {
        assert_eq!(
            roles("before\n```python\nx = 1\n# never closed"),
            [
                BlockRole::Plain,
                BlockRole::FenceEdge,
                BlockRole::FenceBody {
                    lang: "python".to_owned()
                },
                BlockRole::FenceBody {
                    lang: "python".to_owned()
                },
            ]
        );
    }

    #[test]
    fn blank_lines_inside_a_fence_stay_body() {
        assert!(matches!(
            roles("```\n\n```")[1],
            BlockRole::FenceBody { .. }
        ));
    }

    #[test]
    fn indented_fences_still_classify() {
        assert_eq!(roles("   ```sh\necho hi")[0], BlockRole::FenceEdge);
    }

    #[test]
    fn short_marker_runs_are_prose() {
        assert_eq!(
            roles("`` inline``\n`code`"),
            vec![BlockRole::Plain, BlockRole::Plain]
        );
    }

    #[test]
    fn pipes_inside_a_fence_are_never_a_table() {
        assert_eq!(
            roles("```\n| a | b |\n|---|---|\n```"),
            [
                BlockRole::FenceEdge,
                BlockRole::FenceBody {
                    lang: String::new()
                },
                BlockRole::FenceBody {
                    lang: String::new()
                },
                BlockRole::FenceEdge,
            ]
        );
    }

    #[test]
    fn a_delimiter_row_settles_a_table() {
        assert_eq!(
            roles("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |"),
            [
                BlockRole::TableHeader,
                BlockRole::TableDelim,
                BlockRole::TableBody,
                BlockRole::TableBody,
            ]
        );
    }

    #[test]
    fn alignment_colons_keep_the_delimiter_valid() {
        assert_eq!(
            roles("left | right\n:--- | ---:\n1 | 2")[..2],
            [BlockRole::TableHeader, BlockRole::TableDelim]
        );
    }

    #[test]
    fn unsettled_pipe_rows_flush_to_prose() {
        assert_eq!(
            roles("| just | words |\n| more | words |"),
            vec![BlockRole::Plain, BlockRole::Plain],
            "no delimiter row means no table"
        );
    }

    #[test]
    fn a_table_ends_at_the_first_non_pipe_row() {
        assert_eq!(
            roles("| a | b |\n|---|---|\n| 1 | 2 |\nplain again\n| 3 | 4 |"),
            [
                BlockRole::TableHeader,
                BlockRole::TableDelim,
                BlockRole::TableBody,
                BlockRole::Plain,
                BlockRole::Plain,
            ],
            "a later pipe row does not rejoin a settled table"
        );
    }

    #[test]
    fn non_delimiter_followers_leave_the_header_candidate_plain() {
        assert_eq!(
            roles("| a | b |\nnot a delim"),
            vec![BlockRole::Plain, BlockRole::Plain]
        );
    }

    #[test]
    fn fragments_carry_their_role_across_wraps() {
        let rows = project_answer("before\n```\n中文二 very long body line\n```", 8);
        // The one-digit number slot plus `│ ` gutter reserve 3 cells, so the
        // body wraps inside 5. A block-gap now separates `before` (plain)
        // from the fence opener (cross-category transition, GAP_BLOCK).
        assert_eq!(
            texts(&rows),
            [
                "before", "", "```", "中文", "二 ve", "ry lo", "ng bo", "dy li", "ne", "```"
            ]
        );
        for row in &rows[3..9] {
            assert_eq!(
                row.role,
                BlockRole::FenceBody {
                    lang: String::new()
                },
                "every fragment of one logical line keeps the role"
            );
        }
        assert_eq!(rows[3].code_line.as_deref(), Some("1"), "head carries the number");
        for row in &rows[4..9] {
            assert_eq!(
                row.code_line.as_deref(),
                Some(" "),
                "wrapped continuations blank the number slot"
            );
        }
        assert_eq!(rows[0].role, BlockRole::Plain);
        assert_eq!(rows[1].role, BlockRole::Plain);
        assert_eq!(rows[2].role, BlockRole::FenceEdge);
    }

    #[test]
    fn projection_preserves_blank_rows() {
        let rows = project_answer("one\n\ntwo", 20);
        assert_eq!(texts(&rows), ["one", "", "two"]);
        assert!(rows.iter().all(|row| row.spans.is_some()));
    }

    /// Headings classify into [`BlockRole::Heading`] (P4 block-gap
    /// support) instead of staying Plain — the structural marker the gap
    /// strategy keys off of, while `parse_prose` still owns the span
    /// styling.
    #[test]
    fn atx_headings_classify_into_the_heading_role() {
        assert_eq!(roles("# H1"), vec![BlockRole::Heading]);
        assert_eq!(roles("###### Deepest"), vec![BlockRole::Heading]);
        assert_eq!(roles("####### Too many"), vec![BlockRole::Plain], "7 hashes is not a heading");
        assert_eq!(roles("no space after#hash"), vec![BlockRole::Plain]);
        // Setext and thematic breaks stay Plain (pulldown-cmark still
        // styles the heading span; we only need the structural marker).
        assert_eq!(roles("---"), vec![BlockRole::Plain]);
        // Inside a fence, a `#` line is a code body, never a heading.
        assert_eq!(
            roles("```rust\n# comment\n```"),
            [
                BlockRole::FenceEdge,
                BlockRole::FenceBody { lang: "rust".to_owned() },
                BlockRole::FenceEdge,
            ]
        );
    }

    /// Cross-category transitions insert one GAP_BLOCK blank row between
    /// structural blocks; same-category rows stay tight. This is the
    /// density fix — without it, headings, fences, and tables hugged the
    /// surrounding prose.
    #[test]
    fn block_gap_inserts_between_structural_categories() {
        // Plain → Heading → Plain: gaps on both sides of the heading.
        let rows = project_answer("intro\n# Title\nafter", 80);
        assert_eq!(texts(&rows), ["intro", "", "▍ Title", "", "after"]);

        // Plain → Fence (and back): gaps around the fence. The fence body's
        // line-number slot rides `code_line`, not `text`, so `texts()`
        // returns just the code.
        let rows = project_answer("before\n```\nx\n```\nafter", 80);
        assert_eq!(texts(&rows), ["before", "", "```", "x", "```", "", "after"]);

        // Plain → Table (and back): gaps around the table. The table
        // projects 5 rows (top, header, sep, body, bottom).
        let rows = project_answer("lead\n| a | b |\n|---|---|\n| 1 | 2 |\ntail", 80);
        let rendered = texts(&rows);
        assert_eq!(rendered[0], "lead");
        assert_eq!(rendered[1], "", "gap before the table");
        assert_eq!(rendered[6], "╰───┴───╯", "table bottom frame");
        assert_eq!(rendered[7], "", "gap after the table");
        assert_eq!(rendered[8], "tail");
    }

    /// The block gap is idempotent over source blank lines: an author's
    /// `\n\n` is never amplified into two blanks. The projection skips the
    /// insert when the last emitted row is already blank.
    #[test]
    fn block_gap_is_idempotent_over_author_blank_lines() {
        // `para\n\n# H` — the author already wrote a blank line; the gap
        // strategy does not add another.
        let rows = project_answer("para\n\n# H", 80);
        assert_eq!(texts(&rows), ["para", "", "▍ H"]);

        // Two author blanks stay two blanks (the strategy never adds on
        // top of an existing blank; it does not collapse the author's
        // own spacing either — idempotence means "no amplification").
        let rows = project_answer("para\n\n\n# H", 80);
        let rendered = texts(&rows);
        let blanks = rendered.iter().filter(|r| r.is_empty()).count();
        assert_eq!(blanks, 2, "two author blanks preserved, none added: {rendered:?}");

        // Back-to-back headings share a category, so the strategy keeps
        // them tight (the gap fires on category *changes*, not between
        // same-category rows — consecutive headings stay a tight ladder).
        let rows = project_answer("# A\n# B", 80);
        let rendered = texts(&rows);
        let blanks = rendered.iter().filter(|r| r.is_empty()).count();
        assert_eq!(blanks, 0, "same-category headings stay tight: {rendered:?}");
    }

    // -- P2: baked spans ---------------------------------------------------

    #[test]
    fn styled_row_text_equals_the_concatenation_of_its_spans() {
        let rows = project_answer("plain and **bold**", 80);
        for row in rows {
            let spans = row.spans.as_ref().expect("styled");
            assert_eq!(
                row.text,
                spans.iter().map(|span| span.text.as_str()).collect::<String>()
            );
        }
    }

    #[test]
    fn bold_context_survives_a_wrap_break() {
        let line = "aaaa **bbbbbbbbbbbbbbbb cccccccccccccccc dddddddddddd** tail";
        let rows = project_answer(line, 20);
        // The bold phrase wraps; every fragment inside it keeps the bold
        // flag, including continuations that start mid-word.
        let bold_fragments: Vec<&str> = rows
            .iter()
            .filter(|row| {
                row.spans
                    .as_ref()
                    .expect("styled")
                    .iter()
                    .any(|span| span.style.bold)
            })
            .map(|row| row.text.as_str())
            .collect();
        assert!(
            bold_fragments.len() >= 3,
            "the bold run must span several fragments: {bold_fragments:?}"
        );
        // The joined text is lossless.
        let joined: String = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(joined.replace('\n', ""), line.replace("**", ""));
    }

    #[test]
    fn wrap_spans_never_splits_a_grapheme_or_style_run() {
        let spans = vec![ProseSpan::new("中文中文", ProseStyle::default().bold())];
        let rows = wrap_spans(&spans, 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 1, "same-style graphemes merge into one span");
        assert_eq!(rows[0][0].text, "中文");
        assert_eq!(rows[1][0].text, "中文");
        assert!(rows[0][0].style.bold && rows[1][0].style.bold);

        // Adjacent different styles stay separate even mid-row.
        let mixed = vec![ProseSpan::raw("ab"), ProseSpan::new("cd", code_style())];
        let rows = wrap_spans(&mixed, 10);
        assert_eq!(rows[0].len(), 2);
    }

    #[test]
    fn fenced_bodies_stay_raw_for_syntect() {
        let rows = project_answer("```rust\nlet x = 1;\n```", 40);
        assert!(matches!(rows[0].role, BlockRole::FenceEdge));
        let body = &rows[1];
        assert!(matches!(body.role, BlockRole::FenceBody { .. }));
        assert!(body.spans.is_none(), "code bodies paint via syntect later");
        assert_eq!(body.text, "let x = 1;");
        assert_eq!(body.code_line.as_deref(), Some("1"));
    }

    #[test]
    fn a_settled_table_grids_on_its_widest_content() {
        let table = "| plan | latency |\n|---|---|\n| fast | 2ms |";
        let rows = project_answer(table, 80);
        assert_eq!(rows.len(), 5, "top, header, separator, body, bottom");

        // Widths are the max over every cell: 4 and 7. Every line — frames
        // included (segments span column + padding) — is 18 cells wide.
        for row in &rows {
            assert_eq!(text::width(&row.text), 18, "{}", row.text);
        }
        assert_eq!(rows[0].text, "╭──────┬─────────╮");
        assert_eq!(rows[1].text, "│ plan │ latency │");
        assert_eq!(rows[2].text, "├──────┼─────────┤");
        assert_eq!(rows[3].text, "│ fast │ 2ms     │");
        assert_eq!(rows[4].text, "╰──────┴─────────╯");

        // Header content rides bold accent; bars and frames ride BORDER.
        let header = rows[1].spans.as_ref().expect("styled");
        assert!(header[0].style.color == ProseColor::Border, "bar is the frame token");
        assert!(header[1].style.bold && header[1].style.color == ProseColor::Accent);
        let frame = rows[2].spans.as_ref().expect("styled");
        assert!(frame.iter().all(|span| span.style.color == ProseColor::Border));
    }

    #[test]
    fn body_content_widens_the_grid_instead_of_crushing() {
        // The old header-only rule wrapped "index.html" inside a 4-cell
        // column; now the body cell widens its column and nothing wraps.
        let table = "| 文件 | 说明 |\n|---|---|\n| index.html | 页面结构 |";
        let rows = project_answer(table, 80);
        assert_eq!(rows.len(), 5, "every cell fits its column — one line each");
        assert_eq!(rows[0].text, "╭────────────┬──────────╮");
        assert_eq!(rows[1].text, "│ 文件       │ 说明     │");
        assert_eq!(rows[2].text, "├────────────┼──────────┤");
        assert_eq!(rows[3].text, "│ index.html │ 页面结构 │");
        assert_eq!(rows[4].text, "╰────────────┴──────────╯");
        for row in &rows {
            assert_eq!(text::width(&row.text), 25, "{}", row.text);
        }
    }

    #[test]
    fn cjk_cells_measure_in_terminal_cells() {
        let table = "| 名前 | 値 |\n|---|---|\n| 中文値 | 1 |";
        let rows = project_answer(table, 80);
        // Natural widths: 中文値=6, 値=2 → the body widens column one.
        assert_eq!(rows.len(), 5, "no wrap: columns fit their widest cell");
        for row in &rows {
            assert_eq!(text::width(&row.text), 15, "{}", row.text);
        }
        assert_eq!(rows[0].text, "╭────────┬────╮");
        assert_eq!(rows[1].text, "│ 名前   │ 値 │");
        assert_eq!(rows[3].text, "│ 中文値 │ 1  │");
        assert_eq!(rows[4].text, "╰────────┴────╯");
    }

    #[test]
    fn a_tall_cell_wraps_inside_its_column_and_blanks_its_neighbours() {
        let table = "| col | b |\n|---|---|\n| one two three | x |";
        // Natural widths 13+1=14 fit budget 73 at width 80 — no wrap.
        let wide = project_answer(table, 80);
        assert_eq!(wide.len(), 5);
        assert_eq!(wide[3].text, "│ one two three │ x │");

        // At width 20 the budget is 13 < 14: the wide column squeezes to 6
        // and its cell wraps at word boundaries inside the column; the
        // narrow one stays intact.
        let rows = project_answer(table, 20);
        assert_eq!(rows.len(), 7, "two frames + header + separator + three grid lines");
        assert_eq!(rows[0].text, "╭────────┬───╮");
        assert_eq!(rows[1].text, "│ col    │ b │");
        assert_eq!(rows[2].text, "├────────┼───┤");
        assert_eq!(rows[3].text, "│ one    │ x │");
        assert_eq!(rows[4].text, "│ two    │   │");
        assert_eq!(rows[5].text, "│ three  │   │");
        assert_eq!(rows[6].text, "╰────────┴───╯");
        assert!(matches!(rows[5].role, BlockRole::TableBody));
        assert!(matches!(rows[0].role, BlockRole::TableDelim));
    }

    #[test]
    fn excess_cells_drop_and_missing_cells_pad_per_gfm() {
        let table = "| name | qty |\n|---|---|\n| xy | 9 | extra |\n| abcdefghij |";
        let rows = project_answer(table, 80);
        // Natural widths: name=10 (abcdefghij), qty=3.
        assert_eq!(rows[0].text, "╭────────────┬─────╮");
        assert_eq!(rows[1].text, "│ name       │ qty │");
        assert_eq!(rows[3].text, "│ xy         │ 9   │", "excess cell dropped");
        assert_eq!(rows[4].text, "│ abcdefghij │     │", "missing cell pads blank");
        assert_eq!(rows[5].text, "╰────────────┴─────╯");
    }

    #[test]
    fn alignment_colons_are_absorbed_by_the_delimiter_render() {
        let table = "left | right\n:--- | ---:\n1 | 2";
        let rows = project_answer(table, 80);
        assert_eq!(rows[0].text, "╭──────┬───────╮");
        assert_eq!(rows[2].text, "├──────┼───────┤");
    }

    #[test]
    fn an_oversized_table_squeezes_before_it_flows() {
        let wide_cell = "x".repeat(30);
        let table = format!("| {wide_cell} | b |\n|---|---|\n| 1 | 2 |");
        // Budget 24-7 = 17 < natural 31: the wide column squeezes to 8 and
        // wraps; the table still grids — no premature flow degradation.
        let rows = project_answer(&table, 24);
        assert!(rows[0].text.starts_with('╭'));
        assert!(rows.iter().any(|row| row.text.contains('│')));
        assert!(rows.iter().any(|row| row.text.starts_with('╰')));
        let joined: String = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(
            joined.chars().filter(|ch| *ch == 'x').count(),
            30,
            "every x survives the in-column wrap"
        );

        let header_spans = rows[1].spans.as_ref().expect("styled");
        assert!(
            header_spans
                .iter()
                .any(|span| span.style.bold && span.style.color == ProseColor::Accent)
        );

        // Below one cell per column (width < 4N+1 = 9) the whole run
        // degrades to the dim pipe flow, frames included.
        let flowed = project_answer(&table, 8);
        assert!(flowed.iter().all(|row| !row.text.contains('╭')));
        assert!(flowed.iter().all(|row| !row.text.contains('│')));
        let joined: String = flowed.iter().map(|row| row.text.as_str()).collect();
        assert!(joined.contains('|') && joined.contains(&wide_cell));
        let flow_spans = flowed[0].spans.as_ref().expect("styled");
        assert!(
            flow_spans
                .iter()
                .any(|span| span.text == "|" && span.style.color == ProseColor::Border)
        );
    }

    #[test]
    fn flow_rows_preserve_the_exact_source_text() {
        let source = "| a | bbb |";
        let role = BlockRole::TableBody;
        let rows = flow_rows(source, 8, role);
        let joined: String = rows.iter().map(|row| row.text.as_str()).collect();
        // Word wrapping may relocate a break space to a row end, but every
        // source character must survive.
        let mut source_chars: Vec<char> = source.chars().collect();
        let mut joined_chars: Vec<char> = joined.chars().collect();
        source_chars.sort_unstable();
        joined_chars.sort_unstable();
        assert_eq!(joined_chars, source_chars);
    }
#[test]
    fn path_probe_accepts_only_separator_plus_extension_tokens() {
        assert!(is_path_like("src/utils/jwt.ts"));
        assert!(is_path_like("C:\\x\\y.rs"));
        assert!(is_path_like("src/a.ts."), "trailing punctuation trims off");
        assert!(is_path_like("(src/a.ts)"));
        assert!(!is_path_like("config.yaml"), "no separator");
        assert!(!is_path_like("src/utils/"), "no extension");
        assert!(!is_path_like("https://example.com/a.ts"), "URLs never flag");
        assert!(!is_path_like("http://x/y"), "URLs never flag");
        assert!(!is_path_like("plain"), "ordinary words stay put");
        assert!(!is_path_like("src/a."), "empty extension");
        assert!(!is_path_like(""), "empty token");
    }

    #[test]
    fn colorize_lifts_bare_paths_but_not_inline_code_or_links() {
        let default = ProseStyle::default();
        let spans = colorize("from src/utils/jwt.ts extract", default);
        let texts: Vec<&str> = spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(texts, ["from ", "src/utils/jwt.ts", " ", "extract"]);
        assert_eq!(spans[1].style.color, ProseColor::Green);
        assert_eq!(spans[3].style.color, ProseColor::Default);

        let code = colorize("use `src/a.ts` now", default.colored(ProseColor::Code));
        assert_eq!(code.len(), 1, "inline code keeps one span");
        assert_eq!(code[0].style.color, ProseColor::Code);

        let link = colorize("see src/a.ts", default.colored(ProseColor::Link));
        assert_eq!(link.len(), 1, "link text is never re-colored");
        assert_eq!(link[0].style.color, ProseColor::Link);
    }

    #[test]
    fn inline_code_rides_uniform_green_everywhere() {
        // Every backtick fragment paints helper green — paths, commands,
        // identifiers, single or clustered, no density rationing.
        let spans = parse_prose("edit `src/utils/jwt.ts` then run `npm test` and `fast_path` ok");
        for name in ["src/utils/jwt.ts", "npm test", "fast_path"] {
            assert_eq!(
                spans
                    .iter()
                    .find(|span| span.text == name)
                    .unwrap_or_else(|| panic!("{name} span"))
                    .style
                    .color,
                ProseColor::Green,
                "`{name}` rides the uniform green"
            );
        }
        // Multiple chips on one line: all stay green (the carpet rule is
        // retired with the orange chip).
        for name in ["a/b.rs", "c/d.rs"] {
            assert!(parse_prose(&format!("touch `{name}` files"))
                .iter()
                .any(|span| span.text == name && span.style.color == ProseColor::Green));
        }
        // Code chips on structural rows keep their green too; markers are
        // untouched chrome.
        let list = parse_prose("- `Viewport::FullScreen` ⇔ 备用屏幕");
        assert!(
            list.iter().any(|span| span.text == "• " && span.style.bold),
            "the bullet marker survives"
        );
        assert_eq!(
            list.iter()
                .find(|span| span.text == "Viewport::FullScreen")
                .expect("code chip")
                .style
                .color,
            ProseColor::Green
        );
        let heading = parse_prose("# `ratatui` 框架知识");
        assert_eq!(
            heading
                .iter()
                .find(|span| span.text == "ratatui")
                .expect("heading chip")
                .style
                .color,
            ProseColor::Green
        );
    }

    #[test]
    fn multi_digit_slots_pad_to_the_widest_number_in_the_fence() {
        let code = "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\nfn five() {}\nfn six() {}\nfn seven() {}\nfn eight() {}\nfn nine() {}\nfn ten() {}";
        let rows = project_answer(&format!("```rust\n{code}\n```"), 80);
        let heads: Vec<Option<&str>> = rows
            .iter()
            .filter(|row| matches!(row.role, BlockRole::FenceBody { .. }))
            .map(|row| row.code_line.as_deref())
            .collect();
        assert_eq!(
            heads,
            [
                Some(" 1"),
                Some(" 2"),
                Some(" 3"),
                Some(" 4"),
                Some(" 5"),
                Some(" 6"),
                Some(" 7"),
                Some(" 8"),
                Some(" 9"),
                Some("10"),
            ]
        );
        assert!(heads.iter().all(|slot| slot.is_some()));
    }

    #[test]
    fn bold_words_lift_to_white_inside_body_text() {
        let rows = project_answer("plain **word** tail", 80);
        let spans = rows[0].spans.as_ref().expect("styled");
        let bold = spans
            .iter()
            .find(|span| span.text == "word" && span.style.bold)
            .expect("the strong span");
        assert_eq!(bold.style.color, ProseColor::White);
        let plain = spans
            .iter()
            .find(|span| span.text.contains("plain"))
            .expect("body span");
        assert_eq!(plain.style.color, ProseColor::Default);
    }

    #[test]
    fn quote_bar_rides_dark_gray_and_headers_step_white() {
        let rows = project_answer("> quoted\n## Sub", 80);
        // Plain→Heading opens a block gap, so `## Sub` lands at row 2 (the
        // inserted blank rides GAP_BLOCK).
        let quote_spans = rows[0].spans.as_ref().expect("styled");
        assert_eq!(quote_spans[0].style.color, ProseColor::DarkGray);
        assert_eq!(rows[1].text, "");
        let sub_spans = rows[2].spans.as_ref().expect("styled");
        assert!(sub_spans[0].style.bold);
        assert_eq!(sub_spans[0].style.color, ProseColor::White, "H2 is bold white");
    }

#[test]
fn probe_wrap_debug() {
    let line = "aaaa **bbbbbbbbbbbbbbbb cccccccccccccccc dddddddddddd** tail";
    for (i, row) in project_answer(line, 20).iter().enumerate() {
        println!("row{i}: {:?}", row.text);
    }
}
}