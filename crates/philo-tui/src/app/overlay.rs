//! Overlay state and its pure frame projection.
//!
//! Three overlays exist: the session picker (`/sessions`), the model picker
//! (`/models`), and the approval prompt fed by `ConfirmationRequested`. All
//! project to an [`OverlayFrame`] of typed rows plus a content width, so the
//! shell can paint them as rounded float panels without measuring text
//! itself (redesign §3.6–3.9): pickers render as fixed-size centered dialogs
//! (v0.44 §4.2) and approvals stay content-sized; titles embed into the top
//! border. Overlays are transient float content: they never touch the
//! scrollback and never intercept frontend updates.

use std::collections::HashMap;

use super::text;

/// Visual weight of the overlay title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayTone {
    /// Regular pickers.
    Normal,
    /// The approval prompt.
    Warning,
}

/// Padding cells between the borders and the text zone, per side.
pub(crate) const PANEL_PAD: usize = 1;

/// One paintable overlay row, already fitted to the panel's text zone.
/// Structure stays presentation-only; the shell derives styles from the
/// variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayRow {
    /// Plain body text (approval prompts, filter echo, blank padding).
    Text(String),
    /// A dim small-caps section header (`/models` provider groups).
    Group(String),
    /// A selectable row: marker column, uniform-width primary cell, then
    /// `tail` carries the gap plus the right-aligned secondary meta.
    Entry {
        marked: bool,
        primary: String,
        tail: String,
    },
    /// An indented secondary line under the highlighted entry (previews).
    Detail(String),
    /// A quiet placeholder row ("no matches").
    Empty(String),
}

impl OverlayRow {
    /** Display width of one row inside the text zone (test helper). */
    #[cfg(test)]
    fn fitted_width(&self) -> usize {
        match self {
            OverlayRow::Text(value)
            | OverlayRow::Group(value)
            | OverlayRow::Detail(value)
            | OverlayRow::Empty(value) => text::width(value),
            OverlayRow::Entry { primary, tail, .. } => 2 + text::width(primary) + text::width(tail),
        }
    }
}

/// A windowed row before the panel width is known: entries still carry
/// their raw secondary meta.
enum RawRow {
    Group(String),
    Entry {
        marked: bool,
        primary: String,
        secondary: String,
    },
    Detail(String),
}

/// One projected row plus its selection state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayLine {
    pub row: OverlayRow,
    pub selected: bool,
}

/// Rendered overlay content: an embedded title, typed body rows, and one
/// footer of hints. `width` is the panel's inner content width (text zone
/// plus one padding cell per side); the shell adds the two border columns,
/// sizes the rounded panel from it, and centers the result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayFrame {
    pub title: String,
    pub tone: OverlayTone,
    pub width: usize,
    pub body: Vec<OverlayLine>,
    pub footer: String,
}

impl OverlayFrame {
    /// Flat text rendering of the whole panel (snapshot form).
    #[cfg(test)]
    pub fn to_text(&self) -> String {
        let outer = self.width + 2;
        let zone = self.width - PANEL_PAD * 2;
        let title = text::truncate(&self.title, outer.saturating_sub(6));
        let mut top = format!("╭─ {title} ");
        top.push_str(&"─".repeat(outer.saturating_sub(5 + text::width(&title))));
        top.push('╮');
        let mut out = String::from(&top);
        for line in &self.body {
            out.push_str("\n│ ");
            out.push_str(&flat_row(line, zone));
            out.push_str(" │");
        }
        out.push_str(&format!(
            "\n│ {} │",
            text::pad(&text::truncate(&self.footer, zone), zone)
        ));
        out.push_str(&format!("\n╰{}╯", "─".repeat(outer.saturating_sub(2))));
        out
    }
}

/// Flat rendering of one body row inside the panel's text zone.
#[cfg(test)]
fn flat_row(line: &OverlayLine, zone: usize) -> String {
    let body = match &line.row {
        OverlayRow::Text(value)
        | OverlayRow::Group(value)
        | OverlayRow::Detail(value)
        | OverlayRow::Empty(value) => value.clone(),
        OverlayRow::Entry {
            marked,
            primary,
            tail,
        } => {
            let marker = if line.selected {
                "›"
            } else if *marked {
                "•"
            } else {
                " "
            };
            format!("{marker} {primary}{tail}")
        }
    };
    text::pad(&text::truncate(&body, zone), zone)
}

/// Preview state of one session in the picker (loaded lazily, once).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preview {
    Loading,
    Ready(Vec<String>),
    Failed(String),
}

/// One listed row: a stable identity plus its display facts.
/// Sessions show the title (falling back to the id) with a relative time;
/// models show the composite id with the owning provider as meta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PickerEntry {
    pub(crate) id: String,
    pub(crate) primary: String,
    pub(crate) secondary: String,
    /// Grouping key (`""` disables grouping).
    pub(crate) group: String,
    /// Whether this entry is the current session / current model.
    pub(crate) marked: bool,
    /// Declared reasoning tiers (models only), light to heavy. Empty means
    /// the model has no reasoning capability; sessions never set this.
    pub(crate) tiers: Vec<String>,
}

impl PickerEntry {
    #[cfg(test)]
    pub(crate) fn untitled(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            primary: id.clone(),
            id,
            secondary: String::new(),
            group: String::new(),
            marked: false,
            tiers: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_secondary(mut self, secondary: impl Into<String>) -> Self {
        self.secondary = secondary.into();
        self
    }
}

/// Vertical chrome of a dialog body: blank padding above and below the
/// footer row (the borders themselves are the shell's business).
const DIALOG_BLANKS: usize = 2;

/// The `/sessions` and `/models` overlay: a live-filtered list with a
/// selection cursor and (sessions only) lazily loaded previews rendered as
/// detail rows under the highlight. Model pickers are two-level: `tab`
/// switches between the model list and the highlighted model's reasoning
/// tiers (v0.37 §4.2).
#[derive(Clone, Debug)]
pub struct Picker {
    title: &'static str,
    grouped: bool,
    entries: Vec<PickerEntry>,
    /// Case-insensitive substring query typed while the picker is open.
    filter: String,
    /// Index into [`Picker::visible`], not into `entries`.
    selected: usize,
    /// Selection held across filter edits: remembered when the filter first
    /// hides the highlighted entry, restored once it matches again.
    restore: Option<String>,
    previews: HashMap<String, Preview>,
    /// Whether the reasoning-tier level is active (model pickers only).
    tier_mode: bool,
    /// Highlight index into the current model's tiers.
    tier_selected: usize,
    /// Lowercase label of the effective effort, marked in the tier list.
    current_effort: Option<String>,
}

impl Picker {
    pub(crate) fn new(title: &'static str, grouped: bool, entries: Vec<PickerEntry>) -> Self {
        debug_assert!(!entries.is_empty(), "the picker needs at least one entry");
        Self {
            title,
            grouped,
            entries,
            filter: String::new(),
            selected: 0,
            restore: None,
            previews: HashMap::new(),
            tier_mode: false,
            tier_selected: 0,
            current_effort: None,
        }
    }

    /// Marks the effective effort in the tier level (lowercase labels).
    pub(crate) fn set_current_effort(&mut self, effort: Option<String>) {
        self.current_effort = effort.map(|label| label.to_ascii_lowercase());
    }

    /// Indices of entries matching the active filter.
    fn visible(&self) -> Vec<usize> {
        let query = self.filter.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                query.is_empty()
                    || entry.primary.to_lowercase().contains(&query)
                    || entry.secondary.to_lowercase().contains(&query)
                    || entry.group.to_lowercase().contains(&query)
                    || entry.id.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// The highlighted entry's stable identity (empty when nothing matches).
    pub fn selected(&self) -> &str {
        let visible = self.visible();
        match visible.get(self.selected.min(visible.len().saturating_sub(1))) {
            Some(index) => &self.entries[*index].id,
            None => "",
        }
    }

    /// Whether this picker lists models (grouped rows, no previews).
    pub(crate) fn is_models(&self) -> bool {
        self.grouped
    }

    /// Whether the reasoning-tier level is currently shown.
    pub(crate) fn in_tier_mode(&self) -> bool {
        self.tier_mode
    }

    /// `tab`: flips between the model list and the highlighted model's
    /// reasoning tiers. Entering needs a model that declares tiers; leaving
    /// always works. Returns whether the level changed.
    pub(crate) fn toggle_tier_mode(&mut self) -> bool {
        if !self.grouped {
            return false;
        }
        if self.tier_mode {
            self.tier_mode = false;
            return true;
        }
        let Some(model) = self.highlighted_entry() else {
            return false;
        };
        if model.tiers.is_empty() {
            return false;
        }
        // The default tier is the middle one (even counts take the upper
        // median), matching the resolution default.
        self.tier_selected = model.tiers.len() / 2;
        self.tier_mode = true;
        true
    }

    /// The entry under the highlight (models level).
    fn highlighted_entry(&self) -> Option<&PickerEntry> {
        let visible = self.visible();
        visible
            .get(self.selected.min(visible.len().saturating_sub(1)))
            .map(|index| &self.entries[*index])
    }

    /// Whether the highlighted model declares reasoning tiers.
    pub(crate) fn highlighted_has_tiers(&self) -> bool {
        self.highlighted_entry().is_some_and(|model| !model.tiers.is_empty())
    }

    /// Whether the highlighted entry is the current session / model.
    pub(crate) fn selected_is_current(&self) -> bool {
        self.highlighted_entry().is_some_and(|entry| entry.marked)
    }

    /// Highlighted model id plus tier label while the tier level is open.
    pub(crate) fn selected_tier(&self) -> Option<(String, String)> {
        let model = self.highlighted_entry()?;
        let tier = model.tiers.get(self.tier_selected)?;
        Some((model.id.clone(), tier.clone()))
    }

    /// Whether the tier level's highlight sits on the effective effort.
    pub(crate) fn selected_tier_is_current(&self) -> bool {
        match self.selected_tier() {
            Some((_, tier)) => {
                Some(tier.to_ascii_lowercase()).as_deref() == self.current_effort.as_deref()
            }
            None => false,
        }
    }

    /// Moves the tier-level highlight; returns whether it actually moved.
    pub(crate) fn move_tier(&mut self, up: bool) -> bool {
        let len = self.highlighted_entry().map_or(0, |model| model.tiers.len());
        if len == 0 {
            return false;
        }
        let next = if up {
            self.tier_selected.saturating_sub(1)
        } else {
            (self.tier_selected + 1).min(len - 1)
        };
        if next == self.tier_selected {
            return false;
        }
        self.tier_selected = next;
        true
    }

    /// Jumps the tier-level highlight to the first or last tier.
    pub(crate) fn jump_tier(&mut self, to_top: bool) -> bool {
        let len = self.highlighted_entry().map_or(0, |model| model.tiers.len());
        if len == 0 {
            return false;
        }
        let target = if to_top { 0 } else { len - 1 };
        if target == self.tier_selected {
            return false;
        }
        self.tier_selected = target;
        true
    }

    /// Moves the highlight; returns whether it actually moved.
    pub(crate) fn move_up(&mut self) -> bool {
        if self.selected == 0 {
            return false;
        }
        self.selected -= 1;
        true
    }

    /// Moves the highlight; returns whether it actually moved.
    pub(crate) fn move_down(&mut self) -> bool {
        if self.selected + 1 >= self.visible().len() {
            return false;
        }
        self.selected += 1;
        true
    }

    /// Jumps the highlight to the first or last visible row.
    pub(crate) fn jump(&mut self, to_top: bool) -> bool {
        let len = self.visible().len();
        let target = if to_top { 0 } else { len.saturating_sub(1) };
        if self.selected == target {
            return false;
        }
        self.selected = target;
        true
    }

    /// Appends one filter character; keeps the current entry selected when
    /// it still matches, otherwise resets the highlight to the top.
    pub(crate) fn push_filter(&mut self, ch: char) {
        self.filter.push(ch);
        self.reselect();
    }

    /// Removes the last filter character, with the same keep-selection rule.
    pub(crate) fn pop_filter(&mut self) {
        if self.filter.pop().is_some() {
            self.reselect();
        }
    }

    fn reselect(&mut self) {
        let current = self.selected().to_owned();
        if !current.is_empty() {
            self.restore.get_or_insert(current);
        }
        let anchor = self.restore.clone();
        self.selected = anchor
            .and_then(|id| {
                self.visible()
                    .iter()
                    .position(|index| self.entries[*index].id == id)
            })
            .unwrap_or(0);
    }

    /// Claims the preview load for the current selection: yields the id
    /// the first time it is needed and marks it loading, so a preview is
    /// fetched at most once per session per overlay.
    pub(crate) fn claim_preview(&mut self) -> Option<String> {
        let id = self.selected().to_owned();
        if id.is_empty() || self.previews.contains_key(&id) {
            return None;
        }
        self.previews.insert(id.clone(), Preview::Loading);
        Some(id)
    }

    /// Records a finished preview load.
    pub(crate) fn set_preview(&mut self, id: &str, preview: Preview) {
        self.previews.insert(id.to_owned(), preview);
    }

    fn has_preview_pane(&self) -> bool {
        self.previews.contains_key(self.selected())
    }

    /// Projects the overlay onto the fixed-size dialog (v0.44 §4.2): the
    /// caller passes the clamped outer targets (`height` outer rows,
    /// `outer_width` outer columns — theme constants capped by the live
    /// band). Rows truncate into the constant text zone and short lists pad
    /// with blanks, so the dialog never changes size with its content.
    #[cfg(test)]
    pub(crate) fn frame(&self, height: usize) -> OverlayFrame {
        // 88 = the proportional picker's width cap; kept literal to avoid a
        // render dep (v0.37 §4.2 sizing).
        self.frame_for(height, 88)
    }

    pub(crate) fn frame_for(&self, height: usize, outer_width: usize) -> OverlayFrame {
        if self.tier_mode {
            return self.tier_frame_for(height, outer_width);
        }
        let visible = self.visible();
        let total = visible.len();
        let highlighted = self.selected.min(total.saturating_sub(1));

        // Raw windowed sequence; fitting happens after the panel is sized.
        let mut sequence: Vec<(RawRow, usize)> = Vec::new();
        let mut last_group = String::new();
        for (position, index) in visible.iter().enumerate() {
            let entry = &self.entries[*index];
            if self.grouped && entry.group != last_group {
                last_group = entry.group.clone();
                sequence.push((
                    RawRow::Group(format!("  {}", entry.group.to_uppercase())),
                    position,
                ));
            }
            sequence.push((
                RawRow::Entry {
                    marked: entry.marked,
                    primary: entry.primary.clone(),
                    secondary: entry.secondary.clone(),
                },
                position,
            ));
            if position == highlighted && self.has_preview_pane() {
                for line in self.detail_rows() {
                    sequence.push((RawRow::Detail(format!("    {line}")), position));
                }
            }
        }
        let selected_row = sequence
            .iter()
            .position(|(_, position)| *position == highlighted)
            .unwrap_or(0);

        // Fixed outer width (v0.44 §4.2): the dialog no longer follows its
        // widest row; `outer_width` is the exact outer target (borders
        // included) and content truncates into the constant text zone.
        let inner = outer_width.saturating_sub(2).max(3);
        let text_zone = inner - PANEL_PAD * 2;

        let title = format!(
            "{} · {}/{}",
            self.title,
            usize::from(total > 0) * (highlighted + 1),
            total
        );
        let footer = if self.grouped {
            "enter select · tab reasoning · ↑↓ move · type filter · esc close"
        } else {
            "enter select · ↑↓ move · type filter · esc close"
        };
        let filter_text = if self.filter.is_empty() {
            None
        } else {
            Some(format!("filter {}", self.filter))
        };

        // Entry column layout inside the text zone: marker+gap (2), a
        // uniform primary cell, then the secondary ending at the right edge.
        let sec_max = sequence
            .iter()
            .filter_map(|(row, _)| match row {
                RawRow::Entry { secondary, .. } if !secondary.is_empty() => {
                    Some(text::width(secondary))
                }
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let primary_budget = text_zone
            .saturating_sub(2 + sec_max.min(text_zone.saturating_sub(2)) + 1)
            .max(1);
        let primary_cell = sequence
            .iter()
            .filter_map(|(row, _)| match row {
                RawRow::Entry { primary, .. } => Some(text::width(primary)),
                _ => None,
            })
            .max()
            .map_or(primary_budget, |widest| widest.min(primary_budget));

        let fit = |row: &RawRow| -> OverlayRow {
            match row {
                RawRow::Group(value) => OverlayRow::Group(value.clone()),
                RawRow::Detail(value) => OverlayRow::Detail(value.clone()),
                RawRow::Entry {
                    marked,
                    primary,
                    secondary,
                } => {
                    let secondary_width = text_zone.saturating_sub(2 + primary_cell);
                    let secondary = text::truncate(secondary, secondary_width);
                    let gap = secondary_width.saturating_sub(text::width(&secondary));
                    OverlayRow::Entry {
                        marked: *marked,
                        primary: text::pad(&text::truncate(primary, primary_cell), primary_cell),
                        tail: format!("{}{secondary}", " ".repeat(gap)),
                    }
                }
            }
        };

        // Vertical budget: two blank paddings plus the footer sit around
        // the windowed body; on tiny panels the blanks give way first.
        let inner_budget = height.saturating_sub(2).max(1);
        let filter_rows = usize::from(!self.filter.is_empty());
        let show_blanks = inner_budget >= DIALOG_BLANKS + 2;
        let capacity = inner_budget
            .saturating_sub(usize::from(show_blanks) * DIALOG_BLANKS + 1 + filter_rows)
            .max(1);

        let detail_lines = self.detail_rows();
        let rows_below_highlight = usize::from(self.has_preview_pane()) * detail_lines.len();
        let keep_above = capacity.saturating_sub(1 + rows_below_highlight);
        let mut start = selected_row.saturating_sub(keep_above);
        // Never lead with an orphaned group header.
        while start < sequence.len()
            && matches!(sequence[start].0, RawRow::Group(_))
            && start < selected_row
        {
            start += 1;
        }

        // Body rows fill the windowed capacity exactly: short lists pad
        // with blanks so the fixed dialog height never shrinks (v0.44).
        let mut body: Vec<OverlayLine> = Vec::new();
        if let Some(filter) = &filter_text {
            body.push(OverlayLine {
                row: OverlayRow::Text(text::truncate(filter, text_zone)),
                selected: false,
            });
        }
        for (row, position) in sequence.iter().skip(start).take(capacity) {
            body.push(OverlayLine {
                row: fit(row),
                // Only entries carry the highlight; a leading group header
                // shares the entry's window position but never its tint.
                selected: matches!(row, RawRow::Entry { .. }) && *position == highlighted,
            });
        }
        if total == 0 {
            body.push(OverlayLine {
                row: OverlayRow::Empty("no matches".to_owned()),
                selected: false,
            });
        }
        while body.len() < capacity {
            body.push(blank_line());
        }

        let mut lines: Vec<OverlayLine> = Vec::new();
        if show_blanks {
            lines.push(blank_line());
        }
        lines.extend(body);
        if show_blanks {
            lines.push(blank_line());
        }

        OverlayFrame {
            title: text::truncate(&title, inner.saturating_sub(5)),
            tone: OverlayTone::Normal,
            width: inner,
            body: lines,
            footer: text::truncate(footer, inner),
        }
    }

    /// The reasoning-tier level projection: one row per declared tier of
    /// the highlighted model, with the effective effort marked. Same fixed
    /// dialog geometry as the model level (v0.37 §4.2).
    fn tier_frame_for(&self, height: usize, outer_width: usize) -> OverlayFrame {
        let inner = outer_width.saturating_sub(2).max(3);
        let text_zone = inner - PANEL_PAD * 2;

        let tiers = self
            .highlighted_entry()
            .map(|model| model.tiers.as_slice())
            .unwrap_or_default();
        let title = match self.highlighted_entry() {
            Some(model) => format!("Reasoning · {}", model.primary),
            None => "Reasoning".to_owned(),
        };

        // Vertical budget mirrors the model level: blanks and footer around
        // a windowed body that keeps the highlight visible.
        let inner_budget = height.saturating_sub(2).max(1);
        let show_blanks = inner_budget >= DIALOG_BLANKS + 2;
        let capacity = inner_budget
            .saturating_sub(usize::from(show_blanks) * DIALOG_BLANKS + 1)
            .max(1);

        let total = tiers.len();
        let highlighted = self.tier_selected.min(total.saturating_sub(1));
        // Keep the highlight visible; short lists anchor at the top.
        let start = highlighted.saturating_sub(capacity.saturating_sub(1));

        // Uniform primary cell across tiers, like the model level's column.
        let marker_width = 2;
        let primary_cell = tiers
            .iter()
            .map(|tier| text::width(tier))
            .max()
            .unwrap_or(0)
            .min(text_zone.saturating_sub(marker_width))
            .max(1);

        let mut body: Vec<OverlayLine> = Vec::new();
        for (index, tier) in tiers.iter().enumerate().skip(start).take(capacity) {
            let marked =
                Some(tier.to_ascii_lowercase()).as_deref() == self.current_effort.as_deref();
            body.push(OverlayLine {
                row: fit_tier_row(
                    marked,
                    tier,
                    if marked { "current" } else { "" },
                    primary_cell,
                    text_zone,
                ),
                selected: index == highlighted,
            });
        }
        while body.len() < capacity {
            body.push(blank_line());
        }

        let mut lines: Vec<OverlayLine> = Vec::new();
        if show_blanks {
            lines.push(blank_line());
        }
        lines.extend(body);
        if show_blanks {
            lines.push(blank_line());
        }

        OverlayFrame {
            title: text::truncate(&title, inner.saturating_sub(5)),
            tone: OverlayTone::Normal,
            width: inner,
            body: lines,
            footer: text::truncate(
                "enter confirm · tab models · ↑↓ move · esc back",
                inner,
            ),
        }
    }

    fn detail_rows(&self) -> Vec<String> {
        match self.previews.get(self.selected()) {
            None | Some(Preview::Loading) => vec!["loading preview...".to_owned()],
            Some(Preview::Ready(lines)) => lines.iter().take(4).cloned().collect(),
            Some(Preview::Failed(message)) => vec![format!("preview unavailable: {message}")],
        }
    }
}

fn blank_line() -> OverlayLine {
    OverlayLine {
        row: OverlayRow::Text(String::new()),
        selected: false,
    }
}

/// Fits one tier row into the text zone: marker column, uniform-width
/// primary cell, then the right-aligned `current` meta (v0.37 §4.2).
fn fit_tier_row(
    marked: bool,
    primary: &str,
    secondary: &str,
    primary_cell: usize,
    text_zone: usize,
) -> OverlayRow {
    let secondary_width = text_zone.saturating_sub(2 + primary_cell);
    let secondary = text::truncate(secondary, secondary_width);
    let gap = secondary_width.saturating_sub(text::width(&secondary));
    OverlayRow::Entry {
        marked,
        primary: text::pad(&text::truncate(primary, primary_cell), primary_cell),
        tail: format!("{}{secondary}", " ".repeat(gap)),
    }
}

/// The approval overlay: one confirmation request at a time (FIFO), with a
/// binary answer. Approval semantics live in the service; this only carries
/// the question in and the decision out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmPrompt {
    pub(crate) id: u64,
    pub(crate) title: String,
    pub(crate) body: String,
}

impl ConfirmPrompt {
    pub(crate) fn new(id: u64, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            body: body.into(),
        }
    }

    /// The question's title, echoed with the decision.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Projects the approval into a rounded panel of at most `height`
    /// outer rows (borders included) and `max_width` outer columns.
    #[cfg(test)]
    pub(crate) fn frame(&self, height: usize) -> OverlayFrame {
        self.frame_for(height, 80)
    }

    pub(crate) fn frame_for(&self, height: usize, max_width: usize) -> OverlayFrame {
        const BORDER_TITLE: &str = "Approval required";
        const FOOTER: &str = "y allow · n / esc deny";
        // The question title leads the body; the service body follows.
        let content: Vec<String> = std::iter::once(self.title.clone())
            .chain(self.body.lines().map(str::to_owned))
            .collect();

        let natural = content
            .iter()
            .map(|line| text::width(line))
            .chain([text::width(BORDER_TITLE), text::width(FOOTER)])
            .max()
            .unwrap_or(0);
        // `max_width` caps the outer panel (borders included).
        let inner = (natural + PANEL_PAD * 2).clamp(3, max_width.saturating_sub(2).max(1));
        let text_zone = inner - PANEL_PAD * 2;

        let inner_budget = height.saturating_sub(2).max(1);
        let show_blanks = inner_budget >= DIALOG_BLANKS + 2;
        let capacity = inner_budget
            .saturating_sub(usize::from(show_blanks) * DIALOG_BLANKS + 1)
            .max(1);

        let mut lines: Vec<OverlayLine> = Vec::new();
        if show_blanks {
            lines.push(blank_line());
        }
        for line in content.iter().take(capacity) {
            lines.push(OverlayLine {
                row: OverlayRow::Text(text::truncate(line, text_zone)),
                selected: false,
            });
        }
        if show_blanks {
            lines.push(blank_line());
        }

        OverlayFrame {
            title: text::truncate(BORDER_TITLE, inner.saturating_sub(5)),
            tone: OverlayTone::Warning,
            width: inner,
            body: lines,
            footer: text::truncate(FOOTER, inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> Picker {
        Picker::new(
            "Sessions",
            false,
            vec![
                PickerEntry::untitled("s-1"),
                PickerEntry::untitled("s-2"),
                PickerEntry::untitled("s-3"),
            ],
        )
    }

    #[test]
    fn previews_are_claimed_once_per_session() {
        let mut picker = picker();
        assert_eq!(picker.claim_preview(), Some("s-1".to_owned()));
        assert_eq!(picker.claim_preview(), None, "already loading");
        picker.set_preview("s-1", Preview::Ready(vec!["hello".to_owned()]));
        assert_eq!(picker.claim_preview(), None, "already loaded");
        assert!(picker.move_down());
        assert_eq!(picker.claim_preview(), Some("s-2".to_owned()));
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        let mut picker = picker();
        assert!(!picker.move_up(), "already at the top");
        assert!(picker.move_down());
        assert!(picker.move_down());
        assert!(!picker.move_down(), "already at the bottom");
        assert_eq!(picker.selected(), "s-3");
    }

    #[test]
    fn the_window_follows_the_selection() {
        let mut picker = picker();
        picker.move_down();
        picker.move_down();
        // 7 outer rows: borders + blank padding + footer + two entry rows.
        let frame = picker.frame(7);
        let OverlayRow::Entry { primary, .. } = &frame.body[1].row else {
            panic!("entry row expected: {frame:?}");
        };
        assert!(primary.contains("s-2"), "{frame:?}");
        assert!(!frame.body[1].selected);
        assert!(frame.body[2].selected);
    }

    #[test]
    fn session_picker_frame_snapshot() {
        let mut picker = Picker::new(
            "Sessions",
            false,
            vec![
                PickerEntry {
                    id: "s-flaky".to_owned(),
                    primary: "fix the flaky test on windows".to_owned(),
                    secondary: "now".to_owned(),
                    group: String::new(),
                    marked: true,
                    tiers: Vec::new(),
                },
                PickerEntry::untitled("s-auth").with_secondary("3m"),
                PickerEntry::untitled("s-image").with_secondary("2d"),
            ],
        );
        picker.claim_preview();
        picker.set_preview(
            "s-flaky",
            Preview::Ready(vec![
                "last: refactor the auth middleware to…".to_owned(),
                "one file".to_owned(),
            ]),
        );
        crate::tests::assert_tui_snapshot!("session_picker_overlay", picker.frame(11).to_text());
    }

    #[test]
    fn model_picker_groups_providers_and_marks_current() {
        let picker = Picker::new(
            "Models",
            true,
            vec![
                PickerEntry {
                    id: "anthropic/claude-sonnet-4-5".to_owned(),
                    primary: "claude-sonnet-4-5".to_owned(),
                    secondary: "current".to_owned(),
                    group: "anthropic".to_owned(),
                    marked: true,
                    tiers: Vec::new(),
                },
                PickerEntry {
                    id: "openai/gpt-5.2".to_owned(),
                    primary: "gpt-5.2".to_owned(),
                    secondary: String::new(),
                    group: "openai".to_owned(),
                    marked: false,
                    tiers: Vec::new(),
                },
            ],
        );
        let frame = picker.frame(12);
        assert!(
            frame.body.iter().any(
                |line| matches!(&line.row, OverlayRow::Group(name) if name.contains("ANTHROPIC"))
            ),
            "provider groups render as small caps: {frame:?}"
        );
        let marked = frame
            .body
            .iter()
            .find(|line| line.selected)
            .map(|line| &line.row)
            .expect("an entry is selected");
        match marked {
            OverlayRow::Entry { tail, .. } => {
                assert!(tail.trim_end().ends_with("current"), "{marked:?}");
            }
            other => panic!("entry row expected, got {other:?}"),
        }
        crate::tests::assert_tui_snapshot!("model_picker_overlay", frame.to_text());
    }

    #[test]
    fn failed_preview_renders_the_error_in_the_pane() {
        let mut picker = picker();
        picker.claim_preview();
        picker.set_preview("s-1", Preview::Failed("store busy".to_owned()));
        let frame = picker.frame(8);
        assert!(
            format!("{:?}", frame.body).contains("preview unavailable: store busy"),
            "{frame:?}"
        );
    }

    fn model_picker() -> Picker {
        Picker::new(
            "Models",
            true,
            vec![
                PickerEntry {
                    id: "openai/gpt-5.2".to_owned(),
                    primary: "openai/gpt-5.2".to_owned(),
                    secondary: String::new(),
                    group: "openai".to_owned(),
                    marked: false,
                    tiers: vec![
                        "low".to_owned(),
                        "medium".to_owned(),
                        "high".to_owned(),
                        "xhigh".to_owned(),
                    ],
                },
                PickerEntry {
                    id: "deepseek/deepseek-chat".to_owned(),
                    primary: "deepseek/deepseek-chat".to_owned(),
                    secondary: "current".to_owned(),
                    group: "deepseek".to_owned(),
                    marked: true,
                    tiers: Vec::new(),
                },
            ],
        )
    }

    #[test]
    fn tier_level_toggles_with_a_middle_default_and_marks_current() {
        let mut picker = model_picker();
        assert!(!picker.in_tier_mode());

        // The tiered model under the initial highlight accepts the toggle
        // and lands on the middle tier (even counts take the upper median).
        assert_eq!(picker.selected(), "openai/gpt-5.2");
        assert!(picker.toggle_tier_mode());
        assert!(picker.in_tier_mode());
        let (id, tier) = picker.selected_tier().expect("tier under the cursor");
        assert_eq!(id, "openai/gpt-5.2");
        assert_eq!(tier, "high", "middle of [low, medium, high, xhigh]");
        assert!(!picker.selected_tier_is_current());

        // `tab` round-trips back to the models level, keeping the selection.
        assert!(picker.toggle_tier_mode());
        assert!(!picker.in_tier_mode());

        // A tier-less model refuses to enter the tier level.
        assert!(picker.move_down());
        assert_eq!(picker.selected(), "deepseek/deepseek-chat");
        assert!(!picker.toggle_tier_mode(), "no tiers on the highlight");

        // Tier navigation clamps at both ends.
        assert!(picker.move_up());
        assert!(picker.toggle_tier_mode());
        assert!(picker.move_tier(true), "moves up from the middle");
        assert_eq!(
            picker.selected_tier().expect("tier").1,
            "medium",
            "one step above the middle"
        );
        picker.jump_tier(false);
        assert!(!picker.move_tier(false), "already at the bottom");
        picker.jump_tier(true);
        assert!(!picker.move_tier(true), "already at the top");
    }

    #[test]
    fn tier_level_marks_the_effective_effort() {
        let mut picker = model_picker();
        picker.set_current_effort(Some("XHigh".to_owned()));
        assert!(picker.toggle_tier_mode());
        // The mark is case-insensitive on labels.
        while picker
            .selected_tier()
            .is_some_and(|(_, tier)| tier != "xhigh")
        {
            assert!(picker.move_tier(false));
        }
        assert!(picker.selected_tier_is_current());
    }

    #[test]
    fn session_pickers_ignore_the_tier_toggle() {
        let mut picker = picker();
        assert!(!picker.toggle_tier_mode(), "sessions have no tiers");
        assert!(!picker.in_tier_mode());
    }

    #[test]
    fn tier_frame_snapshot() {
        let mut picker = model_picker();
        picker.set_current_effort(Some("medium".to_owned()));
        picker.toggle_tier_mode();
        crate::tests::assert_tui_snapshot!("model_tier_overlay", picker.frame(11).to_text());
    }

    #[test]
    fn confirm_frame_snapshot() {
        let prompt = ConfirmPrompt::new(
            1,
            "run_command",
            "cargo test -p philo-tui\nworking directory: /repo",
        );
        crate::tests::assert_tui_snapshot!("confirmation_overlay", prompt.frame(9).to_text());
    }

    #[test]
    fn long_ids_truncate_to_fit_the_row() {
        let picker = Picker::new(
            "Sessions",
            false,
            vec![PickerEntry::untitled(
                "session-with-a-very-long-identifier-indeed",
            )],
        );
        // The 40-column cap includes the borders.
        let frame = picker.frame_for(7, 40);
        assert!(frame.width + 2 <= 40, "{frame:?}");
        let text_zone = frame.width - 2;
        assert!(
            frame
                .body
                .iter()
                .all(|line| line.row.fitted_width() <= text_zone),
            "{frame:?}"
        );
        let OverlayRow::Entry { primary, .. } = &frame.body[1].row else {
            panic!("entry row expected: {frame:?}");
        };
        assert!(
            primary.starts_with("session-with-a-very-long-ident"),
            "{primary}"
        );
    }

    #[test]
    fn titled_sessions_render_the_title_in_the_column() {
        let mut picker = Picker::new(
            "Sessions",
            false,
            vec![
                PickerEntry {
                    id: "sess-1982ab3-41".to_owned(),
                    primary: "fix the login bug".to_owned(),
                    secondary: "3h".to_owned(),
                    group: String::new(),
                    marked: true,
                    tiers: Vec::new(),
                },
                PickerEntry::untitled("sess-1982cd7-42"),
            ],
        );
        picker.move_down();
        let frame = picker.frame(7);
        assert!(
            frame.body.iter().any(|line| line.selected
                && matches!(
                    &line.row,
                    OverlayRow::Entry { primary, .. } if primary.contains("sess-1982cd7-42")
                )),
            "{frame:?}"
        );
    }

    #[test]
    fn narrow_picker_keeps_rows_within_cell_width() {
        let picker = Picker::new(
            "Sessions",
            false,
            vec![PickerEntry::untitled("中文-session-name")],
        );
        let frame = picker.frame_for(7, 20);
        assert!(
            frame
                .body
                .iter()
                .all(|line| line.row.fitted_width() <= frame.width - 2),
            "{frame:?}"
        );
    }

    #[test]
    fn filtering_narrows_and_restores_selection_on_clearing() {
        let mut picker = picker();
        picker.move_down();
        picker.move_down();
        assert_eq!(picker.selected(), "s-3");
        for ch in "-2".chars() {
            picker.push_filter(ch);
        }
        assert_eq!(picker.selected(), "s-2", "kept the highlighted entry");
        picker.pop_filter();
        picker.pop_filter();
        assert_eq!(picker.selected(), "s-3", "selection restored");
    }

    #[test]
    fn filtered_views_reset_to_top_when_the_entry_vanishes() {
        let mut picker = picker();
        picker.move_down();
        for ch in "-3".chars() {
            picker.push_filter(ch);
        }
        assert_eq!(picker.selected(), "s-3");
        picker.pop_filter();
        for ch in "-9".chars() {
            picker.push_filter(ch);
        }
        let frame = picker.frame(6);
        assert!(
            frame
                .body
                .iter()
                .any(|line| line.row == OverlayRow::Empty("no matches".to_owned())),
            "{frame:?}"
        );
    }
}
