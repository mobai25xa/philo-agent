//! Crossterm key-event interpretation. Semantic actions belong to app state.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::app::action::Action;

/// Wrapped rows moved by one wheel tick. Same path as PageUp/PageDown.
pub(crate) const WHEEL_ROWS: isize = 3;

/// Maps one key event. Release/repeat events are ignored (kitty protocol
/// reports them when enhanced keys are active).
pub fn interpret(key: &KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Enter if shift => Action::InsertNewline,
        KeyCode::Enter => Action::Submit,
        KeyCode::Char('j' | 'J') if ctrl => Action::InsertNewline,
        KeyCode::Char('c' | 'C') if ctrl => Action::CtrlC,
        KeyCode::Char('d' | 'D') if ctrl => Action::CtrlD,
        KeyCode::Char('o' | 'O') if ctrl => Action::ToggleLevel,
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
        KeyCode::PageUp => Action::PageTranscriptUp,
        KeyCode::PageDown => Action::PageTranscriptDown,
        KeyCode::Tab => Action::Complete,
        KeyCode::Char(ch) if !ctrl => Action::InsertChar(ch),
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

    #[test]
    fn newline_mixed_strategy() {
        assert_eq!(
            interpret(&key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit
        );
        assert_eq!(
            interpret(&key(KeyCode::Enter, KeyModifiers::SHIFT)),
            Action::InsertNewline,
            "capability-detected terminals report Shift+Enter"
        );
        assert_eq!(
            interpret(&key(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Action::InsertNewline,
            "Ctrl+J is the always-available fallback"
        );
    }

    #[test]
    fn control_chords_map_to_their_actions() {
        assert_eq!(
            interpret(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::CtrlC
        );
        assert_eq!(
            interpret(&key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Action::CtrlD
        );
        assert_eq!(
            interpret(&key(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Action::ToggleLevel
        );
        assert_eq!(
            interpret(&key(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            Action::Redraw
        );
        assert_eq!(
            interpret(&key(KeyCode::Char('v'), KeyModifiers::CONTROL)),
            Action::Paste,
            "terminals that keep Ctrl+V for themselves send a paste event instead"
        );
    }

    #[test]
    fn home_and_end_map_to_composer_or_transcript_jumps() {
        assert_eq!(
            interpret(&key(KeyCode::Home, KeyModifiers::NONE)),
            Action::Home
        );
        assert_eq!(
            interpret(&key(KeyCode::End, KeyModifiers::NONE)),
            Action::End
        );
    }

    #[test]
    fn page_up_down_map_to_transcript_scroll() {
        assert_eq!(
            interpret(&key(KeyCode::PageUp, KeyModifiers::NONE)),
            Action::PageTranscriptUp
        );
        assert_eq!(
            interpret(&key(KeyCode::PageDown, KeyModifiers::NONE)),
            Action::PageTranscriptDown
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
        assert_eq!(interpret(&released), Action::None);
    }
}
