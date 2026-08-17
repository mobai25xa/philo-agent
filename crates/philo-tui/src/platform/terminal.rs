//! Crossterm terminal ownership: raw mode, optional alternate screen, mouse
//! capture for wheel scroll, bracketed paste, and the restore obligation
//! (normal exit, error exit, and panic all restore the terminal state).
//!
//! Alternate mode owns the isolated alternate buffer. Inline mode draws an
//! inline viewport on the main buffer and never enters the alternate screen.
//! Native main-buffer scrollback dump on exit is not implemented here.

use std::io::{Stdout, stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::api::types::TuiScreen;

/// Set while a session owns the terminal so the panic hook knows to
/// restore before the default hook prints.
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);
static ENHANCED_KEYS: AtomicBool = AtomicBool::new(false);
static ALTERNATE_SCREEN: AtomicBool = AtomicBool::new(false);
static MOUSE_CAPTURE: AtomicBool = AtomicBool::new(false);

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
    if MOUSE_CAPTURE.swap(false, Ordering::SeqCst) {
        let _ = crossterm::execute!(out, DisableMouseCapture);
    }
    if ALTERNATE_SCREEN.swap(false, Ordering::SeqCst) {
        let _ = crossterm::execute!(out, LeaveAlternateScreen);
    }
    let _ = crossterm::execute!(out, DisableBracketedPaste);
    let _ = disable_raw_mode();
    println!();
}

/// One terminal session: raw mode plus either an isolated alternate screen
/// or an inline viewport on the main buffer.
/// Dropping the guard restores the terminal.
pub struct TerminalSession {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
    /// Whether `Shift+Enter` is deliverable (Windows native or kitty
    /// enhancement); the hint line adapts.
    pub shift_enter: bool,
    /// Screen mode chosen at enter; the session never switches.
    pub screen: TuiScreen,
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
    pub fn enter(screen: TuiScreen) -> std::io::Result<Self> {
        enable_raw_mode()?;
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        install_panic_hook();
        let setup_guard = SetupGuard;

        let mut out = stdout();
        if matches!(screen, TuiScreen::Alternate) {
            crossterm::execute!(out, EnterAlternateScreen)?;
            ALTERNATE_SCREEN.store(true, Ordering::SeqCst);
        }
        crossterm::execute!(out, EnableMouseCapture)?;
        MOUSE_CAPTURE.store(true, Ordering::SeqCst);
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
        let terminal = match screen {
            TuiScreen::Alternate => Terminal::new(backend)?,
            TuiScreen::Inline => {
                let (_, height) = crossterm::terminal::size()?;
                Terminal::with_options(
                    backend,
                    TerminalOptions {
                        viewport: Viewport::Inline(height),
                    },
                )?
            }
        };
        std::mem::forget(setup_guard);
        Ok(Self {
            terminal,
            shift_enter,
            screen,
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
