//! Crossterm key-event interpretation. Semantic actions belong to app state.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::action::Action;

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
        KeyCode::Esc => Action::Escape,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left => Action::MoveLeft,
        KeyCode::Right => Action::MoveRight,
        KeyCode::Up => Action::MoveUp,
        KeyCode::Down => Action::MoveDown,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        KeyCode::Tab => Action::Complete,
        KeyCode::Char(ch) if !ctrl => Action::InsertChar(ch),
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
    }

    #[test]
    fn release_events_are_ignored() {
        let mut released = key(KeyCode::Char('a'), KeyModifiers::NONE);
        released.kind = KeyEventKind::Release;
        assert_eq!(interpret(&released), Action::None);
    }
}
