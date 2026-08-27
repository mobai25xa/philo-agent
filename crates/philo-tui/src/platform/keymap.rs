//! Crossterm key-event interpretation. Semantic actions belong to app state.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::app::action::Action;
use crate::app::state::FocusMode;

/// Wrapped rows moved by one wheel tick. Same path as PageUp/PageDown.
pub(crate) const WHEEL_ROWS: isize = 3;

/// Maps one key event against the current focus owner (P5 §1): the composer
/// and history browse mode read entirely different chords — `k/j/o/i` are
/// literal characters while typing but browse-mode shortcuts while reading
/// history. Release/repeat events are ignored (kitty protocol reports them
/// when enhanced keys are active).
pub fn interpret(key: &KeyEvent, focus: FocusMode) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    match focus {
        FocusMode::Browse => interpret_browse(key),
        FocusMode::Input => interpret_input(key),
    }
}

/// Composer-focus key table (P5 §1 P3): text editing plus the browse-mode
/// entry keys. PgDn no longer scrolls directly — it only has meaning once
/// browse mode owns the focus (decision: PgUp/Ctrl+U enter browse).
fn interpret_input(key: &KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Enter if shift => Action::InsertNewline,
        KeyCode::Enter => Action::Submit,
        KeyCode::Char('j' | 'J') if ctrl => Action::InsertNewline,
        KeyCode::Char('c' | 'C') if ctrl => Action::CtrlC,
        KeyCode::Char('d' | 'D') if ctrl => Action::CtrlD,
        KeyCode::Char('o' | 'O') if ctrl => Action::ToggleLevel,
        KeyCode::Char('u' | 'U') if ctrl => Action::EnterBrowse,
        KeyCode::Char('l' | 'L') if ctrl => Action::Redraw,
        KeyCode::Char('v' | 'V') if ctrl => Action::Paste,
        KeyCode::Esc => Action::Escape,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left => Action::MoveLeft,
        KeyCode::Right => Action::MoveRight,
        KeyCode::Up => Action::MoveUp,
        KeyCode::Down => Action::MoveDown,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        KeyCode::PageUp => Action::EnterBrowse,
        KeyCode::Tab => Action::Complete,
        KeyCode::Char(ch) if !ctrl => Action::InsertChar(ch),
        _ => Action::None,
    }
}

/// History browse-mode key table (P5 §2.2): `k/↑`, `j/↓`, `PgUp/PgDn`,
/// `Space/o`, `Home/End`, `i` and `Esc`. Enter submits the preserved draft
/// from browse mode — the composer returns to follow-bottom with the turn.
/// `Ctrl+C` keeps its cancel/exit semantics; anything else is inert so no
/// stray keystroke edits the draft or leaves the modal.
fn interpret_browse(key: &KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('k' | 'K') if !ctrl => Action::BrowseStep(-1),
        KeyCode::Char('j' | 'J') if !ctrl => Action::BrowseStep(1),
        KeyCode::Up => Action::BrowseStep(-1),
        KeyCode::Down => Action::BrowseStep(1),
        KeyCode::PageUp => Action::BrowsePage(-1),
        KeyCode::PageDown => Action::BrowsePage(1),
        KeyCode::Char(' ') => Action::BrowseToggleFold,
        KeyCode::Char('o' | 'O') if !ctrl => Action::BrowseToggleFold,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        KeyCode::Enter => Action::Submit,
        KeyCode::Char('i' | 'I') if !ctrl => Action::ExitBrowse,
        KeyCode::Esc => Action::Escape,
        KeyCode::Char('c' | 'C') if ctrl => Action::CtrlC,
        _ => Action::None,
    }
}

/// Wheel ticks scroll the sealed transcript. Left-button drag selects it.
/// `Moved` is only a drag while a selection is already in progress, so
/// idle pointer motion does not invalidate frames.
pub fn interpret_mouse(mouse: &MouseEvent, selecting: bool) -> Action {
    match mouse.kind {
        MouseEventKind::ScrollUp => Action::ScrollTranscript(-WHEEL_ROWS),
        MouseEventKind::ScrollDown => Action::ScrollTranscript(WHEEL_ROWS),
        MouseEventKind::Down(MouseButton::Left) => Action::SelectStart {
            x: mouse.column,
            y: mouse.row,
        },
        MouseEventKind::Drag(MouseButton::Left) => Action::SelectDrag {
            x: mouse.column,
            y: mouse.row,
        },
        MouseEventKind::Up(MouseButton::Left) => Action::SelectEnd {
            x: mouse.column,
            y: mouse.row,
        },
        MouseEventKind::Moved if selecting => Action::SelectDrag {
            x: mouse.column,
            y: mouse.row,
        },
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn input_key(code: KeyCode, modifiers: KeyModifiers) -> Action {
        interpret(&key(code, modifiers), FocusMode::Input)
    }

    fn browse_key(code: KeyCode, modifiers: KeyModifiers) -> Action {
        interpret(&key(code, modifiers), FocusMode::Browse)
    }

    #[test]
    fn newline_mixed_strategy() {
        assert_eq!(
            input_key(KeyCode::Enter, KeyModifiers::NONE),
            Action::Submit
        );
        assert_eq!(
            input_key(KeyCode::Enter, KeyModifiers::SHIFT),
            Action::InsertNewline,
            "capability-detected terminals report Shift+Enter"
        );
        assert_eq!(
            input_key(KeyCode::Char('j'), KeyModifiers::CONTROL),
            Action::InsertNewline,
            "Ctrl+J is the always-available fallback"
        );
    }

    #[test]
    fn control_chords_map_to_their_actions() {
        assert_eq!(
            input_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Action::CtrlC
        );
        assert_eq!(
            input_key(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Action::CtrlD
        );
        assert_eq!(
            input_key(KeyCode::Char('o'), KeyModifiers::CONTROL),
            Action::ToggleLevel
        );
        assert_eq!(
            input_key(KeyCode::Char('l'), KeyModifiers::CONTROL),
            Action::Redraw
        );
        assert_eq!(
            input_key(KeyCode::Char('v'), KeyModifiers::CONTROL),
            Action::Paste,
            "terminals that keep Ctrl+V for themselves send a paste event instead"
        );
    }

    #[test]
    fn home_and_end_map_to_composer_or_transcript_jumps() {
        assert_eq!(
            input_key(KeyCode::Home, KeyModifiers::NONE),
            Action::Home
        );
        assert_eq!(
            input_key(KeyCode::End, KeyModifiers::NONE),
            Action::End
        );
    }

    #[test]
    fn page_up_enters_browse_while_page_down_is_inert_in_input_mode() {
        assert_eq!(
            input_key(KeyCode::PageUp, KeyModifiers::NONE),
            Action::EnterBrowse,
            "PgUp leaves the composer and enters browse mode"
        );
        assert_eq!(
            input_key(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Action::EnterBrowse,
            "Ctrl+U is the alternate browse entry key"
        );
        assert_eq!(
            input_key(KeyCode::PageDown, KeyModifiers::NONE),
            Action::None,
            "non-modal PgDn scrolling was revoked; the key means nothing in the composer"
        );
    }

    #[test]
    fn browse_mode_rebinds_the_navigation_chord() {
        assert_eq!(
            browse_key(KeyCode::Char('k'), KeyModifiers::NONE),
            Action::BrowseStep(-1)
        );
        assert_eq!(
            browse_key(KeyCode::Char('j'), KeyModifiers::NONE),
            Action::BrowseStep(1)
        );
        assert_eq!(
            browse_key(KeyCode::Up, KeyModifiers::NONE),
            Action::BrowseStep(-1)
        );
        assert_eq!(
            browse_key(KeyCode::Down, KeyModifiers::NONE),
            Action::BrowseStep(1)
        );
        assert_eq!(
            browse_key(KeyCode::PageUp, KeyModifiers::NONE),
            Action::BrowsePage(-1)
        );
        assert_eq!(
            browse_key(KeyCode::PageDown, KeyModifiers::NONE),
            Action::BrowsePage(1)
        );
        assert_eq!(
            browse_key(KeyCode::Char(' '), KeyModifiers::NONE),
            Action::BrowseToggleFold
        );
        assert_eq!(
            browse_key(KeyCode::Char('o'), KeyModifiers::NONE),
            Action::BrowseToggleFold
        );
        assert_eq!(
            browse_key(KeyCode::Char('i'), KeyModifiers::NONE),
            Action::ExitBrowse
        );
        assert_eq!(
            browse_key(KeyCode::Esc, KeyModifiers::NONE),
            Action::Escape,
            "Esc in browse mode keeps its action; the app exits the modal on it"
        );
        assert_eq!(
            browse_key(KeyCode::Enter, KeyModifiers::NONE),
            Action::Submit,
            "Enter in browse mode sends the preserved draft and returns to the tail (P5 §2.2)"
        );
    }

    #[test]
    fn browse_mode_keeps_literals_inert() {
        // Typing letters is impossible while browsing: the draft survives
        // the round-trip untouched.
        for ch in ['a', 'b', 'x', 'z', '1', '9'] {
            assert_eq!(
                browse_key(KeyCode::Char(ch), KeyModifiers::NONE),
                Action::None,
                "{ch} is inert in browse mode"
            );
        }
        assert_eq!(
            browse_key(KeyCode::Char('k'), KeyModifiers::CONTROL),
            Action::None,
            "Ctrl chords stay reserved for the composer"
        );
    }

    #[test]
    fn wheel_maps_to_transcript_scroll() {
        let event = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            interpret_mouse(&event(MouseEventKind::ScrollUp), false),
            Action::ScrollTranscript(-WHEEL_ROWS)
        );
        assert_eq!(
            interpret_mouse(&event(MouseEventKind::ScrollDown), false),
            Action::ScrollTranscript(WHEEL_ROWS)
        );
        assert_eq!(
            interpret_mouse(&event(MouseEventKind::Moved), false),
            Action::None
        );
        assert_eq!(
            interpret_mouse(&event(MouseEventKind::Moved), true),
            Action::SelectDrag { x: 0, y: 0 }
        );
        assert_eq!(
            interpret_mouse(&event(MouseEventKind::Down(MouseButton::Left)), false),
            Action::SelectStart { x: 0, y: 0 }
        );
        assert_eq!(
            interpret_mouse(&event(MouseEventKind::Drag(MouseButton::Left)), false),
            Action::SelectDrag { x: 0, y: 0 }
        );
        assert_eq!(
            interpret_mouse(&event(MouseEventKind::Up(MouseButton::Left)), false),
            Action::SelectEnd { x: 0, y: 0 }
        );
    }

    #[test]
    fn release_events_are_ignored() {
        let mut released = key(KeyCode::Char('a'), KeyModifiers::NONE);
        released.kind = KeyEventKind::Release;
        assert_eq!(
            interpret(&released, FocusMode::Input),
            Action::None
        );
        assert_eq!(
            interpret(&released, FocusMode::Browse),
            Action::None
        );
    }
}
