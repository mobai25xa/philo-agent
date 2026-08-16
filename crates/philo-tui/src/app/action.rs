//! Semantic input actions consumed by the pure app state machine.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    InsertChar(char),
    InsertNewline,
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    Home,
    End,
    Submit,
    /// `Esc`: cancel while running, close overlays.
    Escape,
    /// `Ctrl+C`: clear input / cancel / two-step exit, by context.
    CtrlC,
    /// `Ctrl+D`: exit when the input is empty.
    CtrlD,
    /// `Ctrl+O`: toggle the information tier.
    ToggleLevel,
    /// `Ctrl+L`: force a full redraw.
    Redraw,
    /// `Tab`: slash-command completion.
    Complete,
    /// `Ctrl+V` reached the app: the terminal did not turn it into a
    /// bracketed paste, so the clipboard has to be read directly.
    Paste,
    /// Composition-root config reload notice. Not produced by the keymap.
    ConfigReload(crate::api::types::ConfigReloadNotice),
    None,
}
