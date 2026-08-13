//! Crossterm terminal ownership: raw mode, bracketed paste, optional keyboard
//! enhancement, and the restore obligation (normal exit, error exit, and
//! panic all restore the terminal state).

use std::io::{Stdout, stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

/// Set while a session owns the terminal so the panic hook knows to
/// restore before the default hook prints.
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);
static ENHANCED_KEYS: AtomicBool = AtomicBool::new(false);

/// Restores the terminal to its normal state; idempotent and safe to call
/// from the panic hook.
pub fn restore_terminal() {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = stdout();
    if ENHANCED_KEYS.swap(false, Ordering::SeqCst) {
        let _ = crossterm::execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(out, DisableBracketedPaste);
    let _ = disable_raw_mode();
    println!();
}

/// One terminal session: raw mode plus an inline viewport. Dropping the
/// guard restores the terminal.
pub struct TerminalSession {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
    /// Whether `Shift+Enter` is deliverable (Windows native or kitty
    /// enhancement); the hint line adapts.
    pub shift_enter: bool,
}

struct SetupGuard;

impl Drop for SetupGuard {
    fn drop(&mut self) {
        // `enter` can fail after raw mode is enabled; restore that partial
        // setup just like the steady-state session guard.
        restore_terminal();
    }
}

impl TerminalSession {
    /// Takes terminal ownership and installs the panic-restore hook.
    pub fn enter(viewport_height: u16) -> std::io::Result<Self> {
        enable_raw_mode()?;
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        install_panic_hook();
        let setup_guard = SetupGuard;

        let mut out = stdout();
        crossterm::execute!(out, EnableBracketedPaste)?;

        // Capability probe: kitty-protocol terminals disambiguate
        // Shift+Enter once enhancement flags are pushed; Windows delivers
        // modifiers natively.
        let mut shift_enter = cfg!(windows);
        if !cfg!(windows) && matches!(supports_keyboard_enhancement(), Ok(true)) {
            crossterm::execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
            ENHANCED_KEYS.store(true, Ordering::SeqCst);
            shift_enter = true;
        }

        let backend = CrosstermBackend::new(stdout());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(viewport_height),
            },
        )?;
        std::mem::forget(setup_guard);
        Ok(Self {
            terminal,
            shift_enter,
        })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn install_panic_hook() {
    static HOOKED: AtomicBool = AtomicBool::new(false);
    if HOOKED.swap(true, Ordering::SeqCst) {
        return;
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
}
