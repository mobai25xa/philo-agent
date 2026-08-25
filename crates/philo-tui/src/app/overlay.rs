//! Overlay state and its pure frame projection.
//!
//! Three overlays exist: the session picker (`/sessions`), the model picker
//! (`/models`), and the approval prompt fed by `ConfirmationRequested`. All
//! project to an [`OverlayFrame`] of typed rows so the content is snapshot-
//! testable and the terminal shell only paints it. Overlays are transient
//! bottom-panel content: they never touch the scrollback and never
//! intercept frontend updates.

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

/// One paintable overlay row. Structure stays presentation-only; the shell
/// derives styles from the variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayRow {
    /// Plain body text (approval prompts).
    Text(String),
    /// A dim section header (`/models` provider groups).
    Group(String),
    /// A selectable row: accent mark column, primary label, right-aligned
    /// secondary meta.
    Entry {
        marked: bool,
        primary: String,
        secondary: String,
    },
    /// An indented secondary line under the highlighted entry (previews).
    Detail(String),
    /// A quiet placeholder row ("no matches").
    Empty(String),
}

/// One projected row plus its selection state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayLine {
    pub row: OverlayRow,
    pub selected: bool,
}

/// Rendered overlay content: a title, typed body rows, and one footer of hints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayFrame {
    pub title: String,
    pub tone: OverlayTone,
    pub body: Vec<OverlayLine>,
    pub footer: String,
}

impl OverlayFrame {
    /// Flat text rendering (snapshot form).
    #[cfg(test)]
    pub fn to_text(&self) -> String {
        let mut text = String::from(&self.title);
        for line in &self.body {
            text.push('\n');
            match &line.row {
                OverlayRow::Text(value)
                | OverlayRow::Group(value)
                | OverlayRow::Detail(value)
                | OverlayRow::Empty(value) => text.push_str(value.trim_end()),
                OverlayRow::Entry {
                    marked,
                    primary,
                    secondary,
                } => {
                    let marker = if *marked && line.selected { ">" } else { " " };
                    text.push_str(&format!("{marker} {primary}  {secondary}"));
                }
            }
        }
        text.push('\n');
        text.push_str(&self.footer);
        text
    }
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
        }
    }
}

/// The `/sessions` and `/models` overlay: a live-filtered list with a
/// selection cursor and (sessions only) lazily loaded previews rendered as
/// detail rows under the highlight.
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
        }
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

    /// Projects the overlay content for a body of `height` rows.
    #[cfg(test)]
    pub(crate) fn frame(&self, height: usize) -> OverlayFrame {
        self.frame_for(height, 80)
    }

    pub(crate) fn frame_for(&self, height: usize, width: usize) -> OverlayFrame {
        let visible = self.visible();
        let total = visible.len();
        let filter_rows = usize::from(!self.filter.is_empty());
        // Detail rows that follow the highlighted entry (previews).
        let detail_lines = self.detail_rows();
        let detail_rows = usize::from(self.has_preview_pane()) * detail_lines.len();

        // Window start: keep every row above the highlight on screen when
        // possible; otherwise slide so the highlight stays visible.
        let mut sequence: Vec<(OverlayRow, usize)> = Vec::new();
        let mut last_group = String::new();
        for (position, index) in visible.iter().enumerate() {
            let entry = &self.entries[*index];
            if self.grouped && entry.group != last_group {
                last_group = entry.group.clone();
                sequence.push((OverlayRow::Group(entry.group.clone()), position));
            }
            sequence.push((
                OverlayRow::Entry {
                    marked: entry.marked,
                    primary: entry.primary.clone(),
                    secondary: entry.secondary.clone(),
                },
                position,
            ));
            if position == self.selected.min(total.saturating_sub(1)) && detail_rows > 0 {
                for line in &detail_lines {
                    sequence.push((OverlayRow::Detail(line.clone()), position));
                }
            }
        }
        let selected_row = sequence
            .iter()
            .position(|(_, position)| *position == self.selected.min(total.saturating_sub(1)))
            .unwrap_or(0);
        let capacity = height.max(1).saturating_sub(filter_rows);
        let rows_below_highlight = detail_rows;
        let keep_above = capacity.saturating_sub(1 + rows_below_highlight);
        let mut start = selected_row.saturating_sub(keep_above);
        // Never lead with an orphaned group header.
        while start < sequence.len()
            && matches!(sequence[start].0, OverlayRow::Group(_))
            && start < selected_row
        {
            start += 1;
        }

        let mut lines: Vec<OverlayLine> = Vec::new();
        if filter_rows > 0 {
            lines.push(OverlayLine {
                row: OverlayRow::Text(format!("filter {}", self.filter)),
                selected: false,
            });
        }
        for (row, position) in sequence.iter().skip(start).take(capacity) {
            lines.push(OverlayLine {
                row: pad_entry(row.clone(), width),
                selected: *position == self.selected.min(total.saturating_sub(1)),
            });
        }
        if total == 0 {
            lines.push(OverlayLine {
                row: OverlayRow::Empty("no matches".to_owned()),
                selected: false,
            });
        }
        let highlighted = self.selected.min(total.saturating_sub(1));
        OverlayFrame {
            title: text::truncate(
                &format!(
                    "{}  {}/{}",
                    self.title,
                    usize::from(total > 0) * (highlighted + 1),
                    total
                ),
                width,
            ),
            tone: OverlayTone::Normal,
            body: lines,
            footer: text::truncate(
                "enter select  ·  ↑↓ move  ·  type to filter  ·  esc close",
                width,
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

/// Fits selectable entries to the row budget: the secondary meta shrinks (and
/// finally the primary truncates) so full-row highlights fill but never
/// overflow the panel width.
fn pad_entry(row: OverlayRow, width: usize) -> OverlayRow {
    match row {
        OverlayRow::Entry {
            marked,
            primary,
            secondary,
        } => {
            // marker column + gap + primary + gap.
            let budget = width.saturating_sub(4);
            let mut primary = primary;
            if text::width(&primary) > budget {
                primary = text::truncate(&primary, budget);
            }
            let secondary_budget = budget.saturating_sub(text::width(&primary));
            OverlayRow::Entry {
                marked,
                primary,
                secondary: text::truncate(
                    &text::pad(&secondary, secondary_budget),
                    secondary_budget,
                ),
            }
        }
        other => other,
    }
}

/// Flat display width of one row (test helper).
#[cfg(test)]
fn row_width(row: OverlayRow) -> usize {
    match row {
        OverlayRow::Text(value)
        | OverlayRow::Group(value)
        | OverlayRow::Detail(value)
        | OverlayRow::Empty(value) => text::width(&value),
        OverlayRow::Entry {
            primary, secondary, ..
        } => text::width(&format!("  {primary}  {secondary}")),
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

    /// Projects the overlay content for a body of `height` rows.
    #[cfg(test)]
    pub(crate) fn frame(&self, height: usize) -> OverlayFrame {
        self.frame_for(height, 80)
    }

    pub(crate) fn frame_for(&self, height: usize, width: usize) -> OverlayFrame {
        let body = self
            .body
            .lines()
            .take(height.max(1))
            .map(|line| OverlayLine {
                row: OverlayRow::Text(text::truncate(line, width)),
                selected: false,
            })
            .collect();
        OverlayFrame {
            title: text::truncate(&format!("approval required  ·  {}", self.title), width),
            tone: OverlayTone::Warning,
            body,
            footer: text::truncate("y allow  ·  n / esc deny", width),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> Picker {
        Picker::new(
            "sessions",
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
        let frame = picker.frame(2);
        let OverlayRow::Entry { primary, .. } = &frame.body[0].row else {
            panic!("entry row expected");
        };
        assert!(primary.contains("s-2"), "{frame:?}");
        assert!(!frame.body[0].selected);
        assert!(frame.body[1].selected);
    }

    #[test]
    fn session_picker_frame_snapshot() {
        let mut picker = picker();
        picker.claim_preview();
        picker.set_preview(
            "s-1",
            Preview::Ready(vec!["> count the files".to_owned(), "one file".to_owned()]),
        );
        crate::tests::assert_tui_snapshot!("session_picker_overlay", picker.frame(5).to_text());
    }

    #[test]
    fn failed_preview_renders_the_error_in_the_pane() {
        let mut picker = picker();
        picker.claim_preview();
        picker.set_preview("s-1", Preview::Failed("store busy".to_owned()));
        let frame = picker.frame(2);
        assert!(
            format!("{:?}", frame.body).contains("preview unavailable: store busy"),
            "{frame:?}"
        );
    }

    #[test]
    fn confirm_frame_snapshot() {
        let prompt = ConfirmPrompt::new(
            1,
            "run_command",
            "cargo test -p philo-tui\nworking directory: /repo",
        );
        crate::tests::assert_tui_snapshot!("confirmation_overlay", prompt.frame(5).to_text());
    }

    #[test]
    fn long_ids_truncate_to_fit_the_row() {
        let picker = Picker::new(
            "sessions",
            false,
            vec![PickerEntry::untitled(
                "session-with-a-very-long-identifier-indeed",
            )],
        );
        let frame = picker.frame_for(1, 40);
        let OverlayRow::Entry { primary, .. } = &frame.body[0].row else {
            panic!("entry row expected");
        };
        assert!(
            primary.starts_with("session-with-a-very-long-identifi"),
            "{primary}"
        );
        assert!(row_width(frame.body[0].row.clone()) <= 40, "{frame:?}");
    }

    #[test]
    fn titled_sessions_render_the_title_in_the_column() {
        let mut picker = Picker::new(
            "sessions",
            false,
            vec![
                PickerEntry {
                    id: "sess-1982ab3-41".to_owned(),
                    primary: "fix the login bug".to_owned(),
                    secondary: "3h".to_owned(),
                    group: String::new(),
                    marked: true,
                },
                PickerEntry::untitled("sess-1982cd7-42"),
            ],
        );
        picker.move_down();
        let frame = picker.frame(2);
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
            "sessions",
            false,
            vec![PickerEntry::untitled("中文-session-name")],
        );
        let frame = picker.frame_for(2, 20);
        assert!(
            frame
                .body
                .iter()
                .all(|line| row_width(line.row.clone()) <= 20),
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
        let frame = picker.frame(3);
        assert!(
            frame
                .body
                .iter()
                .any(|line| line.row == OverlayRow::Empty("no matches".to_owned())),
            "{frame:?}"
        );
    }
}
