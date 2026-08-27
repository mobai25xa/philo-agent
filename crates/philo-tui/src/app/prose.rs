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
//! ``` run owns everything until its matching close), then tables settle
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
}

impl ProjectedRow {
    /// A row outside any special block (also every non-answer kind).
    pub(crate) fn plain(text: impl Into<String>) -> Self {
        Self {
            role: BlockRole::Plain,
            text: text.into(),
            spans: None,
        }
    }

    /// A styled row: text derives from the spans so visual width, selection
    /// geometry, and paint never disagree.
    fn styled(role: BlockRole, spans: Vec<ProseSpan>) -> Self {
        let text = spans.iter().map(|span| span.text.as_str()).collect();
        Self {
            role,
            text,
            spans: Some(spans),
        }
    }

    /// A fenced code body: raw text; syntect styles it later.
    fn code(text: String, lang: String) -> Self {
        Self {
            role: BlockRole::FenceBody { lang },
            text,
            spans: None,
        }
    }
}

/// Semantic color slot of a baked prose span. App-side vocabulary only —
/// the render layer spends theme tokens to realize it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ProseColor {
    /// Primary foreground.
    #[default]
    Default,
    /// Dim chrome: markers, bars, rules, table frames.
    Meta,
    /// Universal link blue.
    Link,
    /// Soft code green: the universal "this is code" hue, applied to the
    /// text itself — no background block (prose v4, mirrors opencode).
    Code,
    /// Brand orange: document skeleton highlights (headings, quote bars,
    /// checked boxes, language tags, table headers). Prose v3 loosened the
    /// old restraint so element types read apart at a glance.
    Accent,
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
    fn colored(mut self, color: ProseColor) -> Self {
        self.color = color;
        self
    }

    fn bold(mut self) -> Self {
        self.bold = true;
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
    fn new(text: impl Into<String>, style: ProseStyle) -> Self {
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BlockRole {
    /// Ordinary prose.
    Plain,
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
    let mut rows = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = &lines[index];
        match &line.role {
            BlockRole::TableHeader => {
                let end = table_run_end(&lines, index);
                rows.extend(project_table(&lines[index..end], width));
                index = end;
            }
            BlockRole::FenceBody { lang } => {
                rows.extend(
                    text::wrap(&line.text, width)
                        .into_iter()
                        .map(|fragment| ProjectedRow::code(fragment, lang.clone())),
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
                let marker = match ordered {
                    Some(number) => format!("{number}. "),
                    None => "- ".to_owned(),
                };
                spans.push(ProseSpan::new(
                    format!("{indent}{marker}"),
                    ProseStyle::default().colored(ProseColor::Meta),
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
                spans.push(ProseSpan::new(
                    format!("{indent}│ "),
                    accent(),
                ));
            }
            Event::Start(Tag::Emphasis) => {
                styles.push(top(&styles).italic());
            }
            Event::End(TagEnd::Emphasis) => pop(&mut styles),
            Event::Start(Tag::Strong) => styles.push(top(&styles).bold()),
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
            Event::Code(code) => spans.push(ProseSpan::new(code.to_string(), code_style())),
            Event::Text(text) => spans.push(ProseSpan::new(text.to_string(), top(&styles))),
            Event::SoftBreak | Event::HardBreak => spans.push(ProseSpan::raw(" ")),
            Event::Rule => spans.push(ProseSpan::new(
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

/// Inline code: soft green text, no background block.
fn code_style() -> ProseStyle {
    ProseStyle::default().colored(ProseColor::Code)
}

/// The heading ladder spends brand orange on the document skeleton: H1
/// underlined accent, H2 accent, deeper rungs fall back to primary weight.
fn heading_style(level: HeadingLevel) -> ProseStyle {
    let base = ProseStyle::default().bold();
    match level {
        HeadingLevel::H1 => base.colored(ProseColor::Accent).underlined(),
        HeadingLevel::H2 => base.colored(ProseColor::Accent),
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
fn wrap_spans(input: &[ProseSpan], width: usize) -> Vec<Vec<ProseSpan>> {
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
    let mut spans = vec![ProseSpan::new(left, bar())];
    for (column, &width) in widths.iter().enumerate() {
        if column > 0 {
            spans.push(ProseSpan::new(mid, bar()));
        }
        spans.push(ProseSpan::new("─".repeat(width + 2), bar()));
    }
    spans.push(ProseSpan::new(right, bar()));
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
        let mut spans = vec![ProseSpan::new("│ ", bar())];
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
                bar(),
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
            spans.push(ProseSpan::new("|", bar()));
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
            vec![BlockRole::Plain, BlockRole::Plain, BlockRole::Plain]
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
        assert_eq!(
            texts(&rows),
            [
                "before",
                "```",
                "中文二 v",
                "ery long",
                " body li",
                "ne",
                "```"
            ]
        );
        for row in &rows[2..6] {
            assert_eq!(
                row.role,
                BlockRole::FenceBody {
                    lang: String::new()
                },
                "every fragment of one logical line keeps the role"
            );
        }
        assert_eq!(rows[0].role, BlockRole::Plain);
        assert_eq!(rows[1].role, BlockRole::FenceEdge);
    }

    #[test]
    fn projection_preserves_blank_rows() {
        let rows = project_answer("one\n\ntwo", 20);
        assert_eq!(texts(&rows), ["one", "", "two"]);
        assert!(rows.iter().all(|row| row.spans.is_some()));
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

        // Header content rides bold accent; bars and frames ride meta.
        let header = rows[1].spans.as_ref().expect("styled");
        assert!(header[0].style.color == ProseColor::Meta, "bar dims");
        assert!(header[1].style.bold && header[1].style.color == ProseColor::Accent);
        let frame = rows[2].spans.as_ref().expect("styled");
        assert!(frame.iter().all(|span| span.style.color == ProseColor::Meta));
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
                .any(|span| span.text == "|" && span.style.color == ProseColor::Meta)
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
fn probe_wrap_debug() {
    let line = "aaaa **bbbbbbbbbbbbbbbb cccccccccccccccc dddddddddddd** tail";
    for (i, row) in project_answer(line, 20).iter().enumerate() {
        println!("row{i}: {:?}", row.text);
    }
}
}