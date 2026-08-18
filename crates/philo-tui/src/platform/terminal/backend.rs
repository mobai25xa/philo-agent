//! Narrow terminal-mode backend: setup and restore only (not ratatui draw).

use std::io::{self, Write, stdout};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};

#[cfg(test)]
use crate::api::types::TerminalCapability;

/// Crossterm (or test double) operations that acquire/release terminal modes.
pub trait TerminalBackend {
    fn enable_raw_mode(&mut self) -> io::Result<()>;
    fn enter_alternate_screen(&mut self) -> io::Result<()>;
    fn enable_mouse_capture(&mut self) -> io::Result<()>;
    fn enable_bracketed_paste(&mut self) -> io::Result<()>;
    fn push_keyboard_enhancement(&mut self) -> io::Result<()>;
    fn supports_keyboard_enhancement(&mut self) -> bool;

    fn pop_keyboard_enhancement(&mut self) -> io::Result<()>;
    fn disable_bracketed_paste(&mut self) -> io::Result<()>;
    fn disable_mouse_capture(&mut self) -> io::Result<()>;
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
    fn disable_raw_mode(&mut self) -> io::Result<()>;
    fn write_restore_newline(&mut self) -> io::Result<()>;
}

/// Production backend: thin wrappers around crossterm.
#[derive(Debug, Default)]
pub struct CrosstermTerminalBackend;

impl TerminalBackend for CrosstermTerminalBackend {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        crossterm::execute!(stdout(), EnterAlternateScreen)
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        crossterm::execute!(stdout(), EnableMouseCapture)
    }

    fn enable_bracketed_paste(&mut self) -> io::Result<()> {
        crossterm::execute!(stdout(), EnableBracketedPaste)
    }

    fn push_keyboard_enhancement(&mut self) -> io::Result<()> {
        crossterm::execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
    }

    fn supports_keyboard_enhancement(&mut self) -> bool {
        !cfg!(windows) && matches!(supports_keyboard_enhancement(), Ok(true))
    }

    fn pop_keyboard_enhancement(&mut self) -> io::Result<()> {
        crossterm::execute!(stdout(), PopKeyboardEnhancementFlags)
    }

    fn disable_bracketed_paste(&mut self) -> io::Result<()> {
        crossterm::execute!(stdout(), DisableBracketedPaste)
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        crossterm::execute!(stdout(), DisableMouseCapture)
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        crossterm::execute!(stdout(), LeaveAlternateScreen)
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }

    fn write_restore_newline(&mut self) -> io::Result<()> {
        let mut out = stdout();
        out.write_all(b"\n")?;
        out.flush()
    }
}

/// Named setup/restore ops for [`RecordingBackend`] assertions.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendOp {
    EnableRawMode,
    EnterAlternateScreen,
    EnableMouseCapture,
    EnableBracketedPaste,
    PushKeyboardEnhancement,
    PopKeyboardEnhancement,
    DisableBracketedPaste,
    DisableMouseCapture,
    LeaveAlternateScreen,
    DisableRawMode,
    WriteRestoreNewline,
}

#[cfg(test)]
impl BackendOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EnableRawMode => "enable_raw_mode",
            Self::EnterAlternateScreen => "enter_alternate_screen",
            Self::EnableMouseCapture => "enable_mouse_capture",
            Self::EnableBracketedPaste => "enable_bracketed_paste",
            Self::PushKeyboardEnhancement => "push_keyboard_enhancement",
            Self::PopKeyboardEnhancement => "pop_keyboard_enhancement",
            Self::DisableBracketedPaste => "disable_bracketed_paste",
            Self::DisableMouseCapture => "disable_mouse_capture",
            Self::LeaveAlternateScreen => "leave_alternate_screen",
            Self::DisableRawMode => "disable_raw_mode",
            Self::WriteRestoreNewline => "write_restore_newline",
        }
    }
}

/// Test double: records call order and can fail at a chosen setup index or restore capability.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct RecordingBackend {
    pub calls: Vec<BackendOp>,
    /// 0-based index among setup ops only (`enable_*` / `enter_*` / `push_*`).
    pub setup_fail_at: Option<usize>,
    /// Fail when restoring these capabilities (continue to later steps).
    pub restore_fail: Vec<TerminalCapability>,
    pub keyboard_supported: bool,
    setup_count: usize,
}

#[cfg(test)]
impl RecordingBackend {
    pub fn new() -> Self {
        Self {
            keyboard_supported: true,
            ..Self::default()
        }
    }

    pub fn with_setup_fail_at(mut self, index: usize) -> Self {
        self.setup_fail_at = Some(index);
        self
    }

    pub fn with_restore_fail(mut self, caps: Vec<TerminalCapability>) -> Self {
        self.restore_fail = caps;
        self
    }

    pub fn call_names(&self) -> Vec<&'static str> {
        self.calls.iter().map(|op| op.as_str()).collect()
    }

    fn record_setup(&mut self, op: BackendOp) -> io::Result<()> {
        let index = self.setup_count;
        self.setup_count += 1;
        self.calls.push(op);
        if self.setup_fail_at == Some(index) {
            return Err(io::Error::other(format!("{} failed", op.as_str())));
        }
        Ok(())
    }

    fn record_restore(&mut self, cap: TerminalCapability, op: BackendOp) -> io::Result<()> {
        self.calls.push(op);
        if self.restore_fail.contains(&cap) {
            return Err(io::Error::other(format!("{} failed", op.as_str())));
        }
        Ok(())
    }
}

#[cfg(test)]
impl TerminalBackend for RecordingBackend {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        self.record_setup(BackendOp::EnableRawMode)
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        self.record_setup(BackendOp::EnterAlternateScreen)
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        self.record_setup(BackendOp::EnableMouseCapture)
    }

    fn enable_bracketed_paste(&mut self) -> io::Result<()> {
        self.record_setup(BackendOp::EnableBracketedPaste)
    }

    fn push_keyboard_enhancement(&mut self) -> io::Result<()> {
        self.record_setup(BackendOp::PushKeyboardEnhancement)
    }

    fn supports_keyboard_enhancement(&mut self) -> bool {
        self.keyboard_supported
    }

    fn pop_keyboard_enhancement(&mut self) -> io::Result<()> {
        self.record_restore(
            TerminalCapability::KeyboardEnhancement,
            BackendOp::PopKeyboardEnhancement,
        )
    }

    fn disable_bracketed_paste(&mut self) -> io::Result<()> {
        self.record_restore(
            TerminalCapability::BracketedPaste,
            BackendOp::DisableBracketedPaste,
        )
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        self.record_restore(
            TerminalCapability::MouseCapture,
            BackendOp::DisableMouseCapture,
        )
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        self.record_restore(
            TerminalCapability::AlternateScreen,
            BackendOp::LeaveAlternateScreen,
        )
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        self.record_restore(TerminalCapability::RawMode, BackendOp::DisableRawMode)
    }

    fn write_restore_newline(&mut self) -> io::Result<()> {
        self.calls.push(BackendOp::WriteRestoreNewline);
        Ok(())
    }
}
