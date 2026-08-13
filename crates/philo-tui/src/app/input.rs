//! Multi-line input editor: a plain, char-indexed text buffer with cursor
//! movement. Pure state, fully unit-testable; no terminal knowledge.

/// Char-indexed multi-line editor state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputEditor {
    lines: Vec<String>,
    row: usize,
    /// Cursor position within the row, counted in chars.
    col: usize,
}

impl Default for InputEditor {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
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

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// The whole buffer joined with newlines.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Clears the buffer and returns its previous content.
    pub fn take_text(&mut self) -> String {
        let text = self.text();
        *self = Self::default();
        text
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Replaces the whole buffer (input-history recall) and puts the
    /// cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_owned).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = char_count(&self.lines[self.row]);
    }

    pub fn insert_char(&mut self, ch: char) {
        let line = &mut self.lines[self.row];
        let byte = byte_index(line, self.col);
        line.insert(byte, ch);
        self.col += 1;
    }

    /// Inserts text verbatim; newlines split lines (bracketed paste never
    /// submits).
    pub fn insert_str(&mut self, text: &str) {
        for ch in text.chars() {
            match ch {
                '\n' => self.insert_newline(),
                '\r' => {}
                _ => self.insert_char(ch),
            }
        }
    }

    pub fn insert_newline(&mut self) {
        let line = &mut self.lines[self.row];
        let byte = byte_index(line, self.col);
        let rest = line.split_off(byte);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let remove_at = byte_index(line, self.col - 1);
            line.remove(remove_at);
            self.col -= 1;
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = char_count(&self.lines[self.row]);
            self.lines[self.row].push_str(&current);
        }
    }

    pub fn delete(&mut self) {
        let line_chars = char_count(&self.lines[self.row]);
        if self.col < line_chars {
            let line = &mut self.lines[self.row];
            let remove_at = byte_index(line, self.col);
            line.remove(remove_at);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = char_count(&self.lines[self.row]);
        }
    }

    pub fn move_right(&mut self) {
        if self.col < char_count(&self.lines[self.row]) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Moves up one line; returns false when already on the first line
    /// (the caller may recall input history instead).
    pub fn move_up(&mut self) -> bool {
        if self.row == 0 {
            return false;
        }
        self.row -= 1;
        self.col = self.col.min(char_count(&self.lines[self.row]));
        true
    }

    /// Moves down one line; returns false when already on the last line.
    pub fn move_down(&mut self) -> bool {
        if self.row + 1 >= self.lines.len() {
            return false;
        }
        self.row += 1;
        self.col = self.col.min(char_count(&self.lines[self.row]));
        true
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = char_count(&self.lines[self.row]);
    }
}

fn char_count(line: &str) -> usize {
    line.chars().count()
}

fn byte_index(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map_or(line.len(), |(byte, _)| byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_round_trip_with_multibyte_chars() {
        let mut editor = InputEditor::new();
        editor.insert_str("héllo");
        assert_eq!(editor.text(), "héllo");
        editor.move_left();
        editor.backspace(); // removes the second 'l'
        assert_eq!(editor.text(), "hélo");
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
        editor.backspace(); // joins back
        assert_eq!(editor.text(), "ab");
        assert_eq!(editor.cursor(), (0, 1));
    }

    #[test]
    fn vertical_movement_clamps_and_reports_boundaries() {
        let mut editor = InputEditor::new();
        editor.insert_str("long line\nx");
        assert_eq!(editor.cursor(), (1, 1));
        assert!(!editor.move_down(), "already on the last line");
        assert!(editor.move_up());
        assert_eq!(editor.cursor(), (0, 1), "column clamps to source col");
        assert!(!editor.move_up(), "first line reports the boundary");
    }

    #[test]
    fn paste_never_submits_and_take_text_resets() {
        let mut editor = InputEditor::new();
        editor.insert_str("line1\nline2\r\nline3");
        assert_eq!(editor.lines().len(), 3);
        let taken = editor.take_text();
        assert_eq!(taken, "line1\nline2\nline3");
        assert!(editor.is_empty());
    }
}
