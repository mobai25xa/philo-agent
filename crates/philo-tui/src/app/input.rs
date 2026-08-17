//! Multi-line input editor with grapheme-safe cursor movement.

use unicode_segmentation::UnicodeSegmentation;

use super::text;

/// Multi-line editor state. The cursor is stored as a UTF-8 byte boundary and
/// can only land between extended grapheme clusters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputEditor {
    lines: Vec<String>,
    row: usize,
    byte: usize,
    preferred_cell: Option<usize>,
}

impl Default for InputEditor {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            byte: 0,
            preferred_cell: None,
        }
    }
}

impl InputEditor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Logical row and grapheme index, primarily for state tests.
    #[cfg(test)]
    pub fn cursor(&self) -> (usize, usize) {
        (
            self.row,
            self.lines[self.row][..self.byte].graphemes(true).count(),
        )
    }

    pub(crate) fn cursor_byte(&self) -> (usize, usize) {
        (self.row, self.byte)
    }

    #[cfg(test)]
    pub(crate) fn cursor_cell(&self) -> usize {
        text::width(&self.lines[self.row][..self.byte])
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn take_text(&mut self) -> String {
        let text = self.text();
        *self = Self::default();
        text
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(sanitize).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.byte = self.lines[self.row].len();
        self.preferred_cell = None;
    }

    pub fn insert_char(&mut self, ch: char) {
        if ch.is_control() {
            return;
        }
        self.lines[self.row].insert(self.byte, ch);
        self.byte += ch.len_utf8();
        self.preferred_cell = None;
    }

    /// Inserts text verbatim except for carriage returns and unsafe control
    /// characters; newlines split lines and never submit.
    pub fn insert_str(&mut self, value: &str) {
        for ch in value.chars() {
            match ch {
                '\n' => self.insert_newline(),
                '\r' => {}
                _ if ch.is_control() => {}
                _ => self.insert_char(ch),
            }
        }
    }

    pub fn insert_newline(&mut self) {
        let rest = self.lines[self.row].split_off(self.byte);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.byte = 0;
        self.preferred_cell = None;
    }

    pub fn backspace(&mut self) {
        if self.byte > 0 {
            let previous = previous_boundary(&self.lines[self.row], self.byte);
            self.lines[self.row].drain(previous..self.byte);
            self.byte = previous;
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.byte = self.lines[self.row].len();
            self.lines[self.row].push_str(&current);
        }
        self.preferred_cell = None;
    }

    pub fn delete(&mut self) {
        let line_len = self.lines[self.row].len();
        if self.byte < line_len {
            let next = next_boundary(&self.lines[self.row], self.byte);
            self.lines[self.row].drain(self.byte..next);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
        self.preferred_cell = None;
    }

    pub fn move_left(&mut self) {
        if self.byte > 0 {
            self.byte = previous_boundary(&self.lines[self.row], self.byte);
        } else if self.row > 0 {
            self.row -= 1;
            self.byte = self.lines[self.row].len();
        }
        self.preferred_cell = None;
    }

    pub fn move_right(&mut self) {
        if self.byte < self.lines[self.row].len() {
            self.byte = next_boundary(&self.lines[self.row], self.byte);
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.byte = 0;
        }
        self.preferred_cell = None;
    }

    pub fn move_up(&mut self) -> bool {
        if self.row == 0 {
            return false;
        }
        let cell = *self
            .preferred_cell
            .get_or_insert_with(|| text::width(&self.lines[self.row][..self.byte]));
        self.row -= 1;
        self.byte = boundary_at_cell(&self.lines[self.row], cell);
        true
    }

    pub fn move_down(&mut self) -> bool {
        if self.row + 1 >= self.lines.len() {
            return false;
        }
        let cell = *self
            .preferred_cell
            .get_or_insert_with(|| text::width(&self.lines[self.row][..self.byte]));
        self.row += 1;
        self.byte = boundary_at_cell(&self.lines[self.row], cell);
        true
    }

    pub fn home(&mut self) {
        self.byte = 0;
        self.preferred_cell = None;
    }

    pub fn end(&mut self) {
        self.byte = self.lines[self.row].len();
        self.preferred_cell = None;
    }
}

/// Recalled submissions for Up/Down when the cursor is already on the
/// first or last draft line. Distinct from session replay (`session`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct InputHistory {
    entries: Vec<String>,
    /// Index into `entries` while browsing; `None` when editing fresh text.
    cursor: Option<usize>,
    /// The fresh text stashed while browsing history.
    stash: Option<String>,
}

impl InputHistory {
    pub(crate) fn push(&mut self, text: String) {
        self.entries.push(text);
        self.cursor = None;
        self.stash = None;
    }

    pub(crate) fn reset_browse(&mut self) {
        self.cursor = None;
    }

    /// Moves to an older submission. Always yields text when the list is
    /// non-empty, including when already on the oldest entry.
    pub(crate) fn prev(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let next_index = match self.cursor {
            None => {
                self.stash = Some(current.to_owned());
                self.entries.len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.cursor = Some(next_index);
        Some(self.entries[next_index].clone())
    }

    /// Moves toward newer submissions, or restores the stashed draft.
    pub(crate) fn next(&mut self) -> Option<String> {
        let index = self.cursor?;
        if index + 1 < self.entries.len() {
            self.cursor = Some(index + 1);
            Some(self.entries[index + 1].clone())
        } else {
            self.cursor = None;
            Some(self.stash.take().unwrap_or_default())
        }
    }
}

fn previous_boundary(line: &str, byte: usize) -> usize {
    line[..byte]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_boundary(line: &str, byte: usize) -> usize {
    line[byte..]
        .grapheme_indices(true)
        .nth(1)
        .map_or(line.len(), |(index, _)| byte + index)
}

fn boundary_at_cell(line: &str, target: usize) -> usize {
    let mut cells = 0;
    for (byte, grapheme) in line.grapheme_indices(true) {
        let next = cells + text::width(grapheme);
        if next > target {
            return byte;
        }
        cells = next;
    }
    line.len()
}

fn sanitize(line: &str) -> String {
    line.chars().filter(|ch| !ch.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_round_trip_with_multibyte_chars() {
        let mut editor = InputEditor::new();
        editor.insert_str("héllo");
        editor.move_left();
        editor.backspace();
        editor.insert_char('中');
        assert_eq!(editor.text(), "hél中o");
    }

    #[test]
    fn newline_split_and_join() {
        let mut editor = InputEditor::new();
        editor.insert_str("ab");
        editor.move_left();
        editor.insert_newline();
        assert_eq!(editor.lines(), ["a", "b"]);
        assert_eq!(editor.cursor(), (1, 0));
        editor.backspace();
        assert_eq!(editor.text(), "ab");
        assert_eq!(editor.cursor(), (0, 1));
    }

    #[test]
    fn vertical_movement_uses_terminal_cells() {
        let mut editor = InputEditor::new();
        editor.insert_str("中文x\nabcdef\n中y");
        editor.home();
        editor.move_right();
        assert_eq!(editor.cursor_cell(), 2);
        assert!(editor.move_up());
        assert_eq!(editor.cursor(), (1, 2));
        assert!(editor.move_up());
        assert_eq!(editor.cursor(), (0, 1));
    }

    #[test]
    fn combining_and_emoji_sequences_move_and_delete_as_graphemes() {
        let mut editor = InputEditor::new();
        editor.insert_str("Ae\u{301}👩‍💻中");
        assert_eq!(editor.cursor(), (0, 4));
        editor.move_left();
        editor.backspace();
        assert_eq!(editor.text(), "Ae\u{301}中");
        editor.backspace();
        assert_eq!(editor.text(), "A中");
        editor.backspace();
        assert_eq!(editor.text(), "中");
    }

    #[test]
    fn delete_never_splits_a_grapheme() {
        let mut editor = InputEditor::new();
        editor.insert_str("e\u{301}👨‍👩‍👧‍👦x");
        editor.home();
        editor.delete();
        assert_eq!(editor.text(), "👨‍👩‍👧‍👦x");
        editor.delete();
        assert_eq!(editor.text(), "x");
    }

    #[test]
    fn paste_filters_terminal_control_characters() {
        let mut editor = InputEditor::new();
        editor.insert_str("a\u{1b}[31m\tb\r\nc");
        assert_eq!(editor.text(), "a[31mb\nc");
    }
}
