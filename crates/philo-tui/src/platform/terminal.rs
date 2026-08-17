//! Crossterm terminal ownership: raw mode, optional alternate screen, mouse
//! capture, bracketed paste, and owner-thread restore.
//!
//! Alternate mode owns the isolated alternate buffer. Inline mode draws an
//! inline viewport on the main buffer and never enters the alternate screen.
//! Native main-buffer scrollback dump on exit is not implemented here.

use std::io::{Stdout, stdout};
use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::ThreadId;

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

use crate::api::types::{RestoreReport, TuiScreen};

#[derive(Clone, Copy, Debug)]
struct SessionFlags {
    token: u64,
    owner: ThreadId,
    enhanced_keys: bool,
    alternate: bool,
    mouse: bool,
}

struct Registry {
    next: AtomicU64,
    active: Mutex<Option<SessionFlags>>,
}

impl Registry {
    const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            active: Mutex::new(None),
        }
    }

    fn allocate(&self, _owner: ThreadId) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    fn activate(&self, flags: SessionFlags) {
        if let Ok(mut active) = self.active.lock() {
            *active = Some(flags);
        }
    }

    fn snapshot(&self) -> Option<SessionFlags> {
        self.active.lock().ok().and_then(|guard| *guard)
    }

    fn take_if(&self, token: u64) -> Option<SessionFlags> {
        let mut active = self.active.lock().ok()?;
        match *active {
            Some(flags) if flags.token == token => active.take(),
            _ => None,
        }
    }
}

static REGISTRY: Registry = Registry::new();

/// One terminal session: raw mode plus either an isolated alternate screen
/// or an inline viewport on the main buffer. The type is owner-thread-bound.
pub struct TerminalSession {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
    /// Whether `Shift+Enter` is deliverable (Windows native or kitty
    /// enhancement); the hint line adapts.
    pub shift_enter: bool,
    token: u64,
    owner: ThreadId,
    restored: bool,
    _not_send: PhantomData<*const ()>,
}

struct SetupGuard {
    token: u64,
}

impl Drop for SetupGuard {
    fn drop(&mut self) {
        let _ = restore_token(self.token);
    }
}

impl TerminalSession {
    /// Takes terminal ownership and installs the panic-restore hook.
    pub fn enter(screen: TuiScreen) -> std::io::Result<Self> {
        let owner = std::thread::current().id();
        let token = REGISTRY.allocate(owner);
        enable_raw_mode()?;
        REGISTRY.activate(SessionFlags {
            token,
            owner,
            enhanced_keys: false,
            alternate: false,
            mouse: false,
        });
        install_panic_hook();
        let setup_guard = SetupGuard { token };

        let mut out = stdout();
        let mut enhanced_keys = false;
        let mut alternate = false;

        if matches!(screen, TuiScreen::Alternate) {
            crossterm::execute!(out, EnterAlternateScreen)?;
            alternate = true;
        }
        crossterm::execute!(out, EnableMouseCapture)?;
        let mouse = true;
        crossterm::execute!(out, EnableBracketedPaste)?;

        let mut shift_enter = cfg!(windows);
        if !cfg!(windows) && matches!(supports_keyboard_enhancement(), Ok(true)) {
            crossterm::execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
            enhanced_keys = true;
            shift_enter = true;
        }

        REGISTRY.activate(SessionFlags {
            token,
            owner,
            enhanced_keys,
            alternate,
            mouse,
        });

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
            token,
            owner,
            restored: false,
            _not_send: PhantomData,
        })
    }

    /// Owner thread of this session.
    pub fn owner(&self) -> ThreadId {
        self.owner
    }

    /// Restores the terminal. A stale token cannot tear down a newer session.
    pub fn restore(&mut self) -> RestoreReport {
        debug_assert_eq!(
            std::thread::current().id(),
            self.owner(),
            "TerminalSession::restore must run on the owner thread"
        );
        if self.restored {
            return RestoreReport {
                restored: false,
                skipped_stale: false,
                errors: Vec::new(),
            };
        }
        self.restored = true;
        restore_token(self.token)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if !self.restored {
            let _ = restore_token(self.token);
            self.restored = true;
        }
    }
}

fn restore_token(token: u64) -> RestoreReport {
    let Some(flags) = REGISTRY.take_if(token) else {
        return RestoreReport {
            restored: false,
            skipped_stale: REGISTRY.snapshot().is_some(),
            errors: Vec::new(),
        };
    };
    apply_restore(flags)
}

fn apply_restore(flags: SessionFlags) -> RestoreReport {
    let mut errors = Vec::new();
    let mut out = stdout();
    if flags.enhanced_keys
        && let Err(error) = crossterm::execute!(out, PopKeyboardEnhancementFlags)
    {
        errors.push(format!("pop keyboard enhancement: {error}"));
    }
    if flags.mouse
        && let Err(error) = crossterm::execute!(out, DisableMouseCapture)
    {
        errors.push(format!("disable mouse: {error}"));
    }
    if flags.alternate
        && let Err(error) = crossterm::execute!(out, LeaveAlternateScreen)
    {
        errors.push(format!("leave alternate screen: {error}"));
    }
    if let Err(error) = crossterm::execute!(out, DisableBracketedPaste) {
        errors.push(format!("disable bracketed paste: {error}"));
    }
    if let Err(error) = disable_raw_mode() {
        errors.push(format!("disable raw mode: {error}"));
    }
    let _ = std::io::Write::write_all(&mut out, b"\n");
    RestoreReport {
        restored: true,
        skipped_stale: false,
        errors,
    }
}

fn install_panic_hook() {
    static HOOKED: AtomicBool = AtomicBool::new(false);
    if HOOKED.swap(true, Ordering::SeqCst) {
        return;
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(flags) = REGISTRY.snapshot()
            && std::thread::current().id() == flags.owner
        {
            let _ = restore_token(flags.token);
        }
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_token_does_not_clear_a_newer_session() {
        let owner = std::thread::current().id();
        let old = REGISTRY.allocate(owner);
        let new = REGISTRY.allocate(owner);
        REGISTRY.activate(SessionFlags {
            token: new,
            owner,
            enhanced_keys: false,
            alternate: false,
            mouse: false,
        });
        let report = restore_token(old);
        assert!(!report.restored);
        assert!(report.skipped_stale);
        assert_eq!(REGISTRY.snapshot().map(|flags| flags.token), Some(new));
        let _ = REGISTRY.take_if(new);
    }

    #[test]
    fn matching_token_clears_the_registry_without_terminal_flags() {
        let owner = std::thread::current().id();
        let token = REGISTRY.allocate(owner);
        REGISTRY.activate(SessionFlags {
            token,
            owner,
            enhanced_keys: false,
            alternate: false,
            mouse: false,
        });
        let report = restore_token(token);
        assert!(report.restored);
        assert!(!report.skipped_stale);
        assert!(REGISTRY.snapshot().is_none());
    }
}
