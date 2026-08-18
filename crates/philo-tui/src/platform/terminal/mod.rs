//! Crossterm terminal ownership: raw mode, optional alternate screen, mouse
//! capture, bracketed paste, and owner-thread restore.
//!
//! Alternate mode owns the isolated alternate buffer. Inline mode draws an
//! inline viewport on the main buffer and never enters the alternate screen.
//! Native main-buffer scrollback dump on exit is not implemented here.

mod backend;

use std::fmt;
use std::io::{self, Stdout, Write, stdout};
use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::ThreadId;

use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::api::types::{RestoreFailure, RestoreReport, TerminalCapability, TuiScreen};

pub use backend::{CrosstermTerminalBackend, TerminalBackend};

#[cfg(test)]
pub use backend::{BackendOp, RecordingBackend};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TerminalCapabilities {
    raw_mode: bool,
    alternate_screen: bool,
    mouse_capture: bool,
    bracketed_paste: bool,
    keyboard_enhancement: bool,
}

impl TerminalCapabilities {
    fn held_restore_order(self) -> Vec<TerminalCapability> {
        let mut caps = Vec::new();
        if self.keyboard_enhancement {
            caps.push(TerminalCapability::KeyboardEnhancement);
        }
        if self.bracketed_paste {
            caps.push(TerminalCapability::BracketedPaste);
        }
        if self.mouse_capture {
            caps.push(TerminalCapability::MouseCapture);
        }
        if self.alternate_screen {
            caps.push(TerminalCapability::AlternateScreen);
        }
        if self.raw_mode {
            caps.push(TerminalCapability::RawMode);
        }
        caps
    }

    fn set(&mut self, cap: TerminalCapability, held: bool) {
        match cap {
            TerminalCapability::RawMode => self.raw_mode = held,
            TerminalCapability::AlternateScreen => self.alternate_screen = held,
            TerminalCapability::MouseCapture => self.mouse_capture = held,
            TerminalCapability::BracketedPaste => self.bracketed_paste = held,
            TerminalCapability::KeyboardEnhancement => self.keyboard_enhancement = held,
        }
    }

    fn get(self, cap: TerminalCapability) -> bool {
        match cap {
            TerminalCapability::RawMode => self.raw_mode,
            TerminalCapability::AlternateScreen => self.alternate_screen,
            TerminalCapability::MouseCapture => self.mouse_capture,
            TerminalCapability::BracketedPaste => self.bracketed_paste,
            TerminalCapability::KeyboardEnhancement => self.keyboard_enhancement,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SessionFlags {
    token: u64,
    owner: ThreadId,
    capabilities: TerminalCapabilities,
}

struct Registry {
    next: AtomicU64,
    active: Mutex<Option<SessionFlags>>,
    /// Sticky: `Mutex` poison is consumed by `into_inner()`, but emergency
    /// restore still needs to diagnose uncertain ownership afterwards.
    poisoned: AtomicBool,
}

impl Registry {
    const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            active: Mutex::new(None),
            poisoned: AtomicBool::new(false),
        }
    }

    fn allocate(&self, _owner: ThreadId) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    fn lock_active(&self) -> (std::sync::MutexGuard<'_, Option<SessionFlags>>, bool) {
        match self.active.lock() {
            Ok(guard) => (guard, false),
            Err(poisoned) => {
                // `into_inner()` clears Mutex poison; keep a sticky bit so
                // `try_take_if` can still diagnose after `snapshot()`.
                self.poisoned.store(true, Ordering::SeqCst);
                (poisoned.into_inner(), true)
            }
        }
    }

    fn activate(&self, flags: SessionFlags) {
        let (mut active, _) = self.lock_active();
        *active = Some(flags);
    }

    fn snapshot(&self) -> Option<SessionFlags> {
        let (active, _) = self.lock_active();
        *active
    }

    #[allow(dead_code)]
    fn take_if(&self, token: u64) -> Option<SessionFlags> {
        self.take_if_recovered(token).0
    }

    fn take_if_recovered(&self, token: u64) -> (Option<SessionFlags>, bool) {
        let (mut active, poisoned) = self.lock_active();
        let taken = match *active {
            Some(flags) if flags.token == token => active.take(),
            _ => None,
        };
        (taken, poisoned)
    }

    fn try_take_if(&self, token: u64) -> Result<Option<SessionFlags>, Option<SessionFlags>> {
        let (taken, this_lock_poisoned) = self.take_if_recovered(token);
        let sticky = self.poisoned.swap(false, Ordering::SeqCst);
        if this_lock_poisoned || sticky {
            Err(taken)
        } else {
            Ok(taken)
        }
    }
}

static REGISTRY: Registry = Registry::new();

#[cfg(test)]
static TEST_DIAGNOSTICS: Mutex<Option<Vec<String>>> = Mutex::new(None);
#[cfg(test)]
static TERMINAL_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Setup failure that already rolled back acquired modes.
#[derive(Debug)]
pub struct TerminalEnterError {
    pub fault: io::Error,
    pub restore: RestoreReport,
}

impl fmt::Display for TerminalEnterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.fault)
    }
}

impl std::error::Error for TerminalEnterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.fault)
    }
}

/// Session-local mode ownership backed by a [`TerminalBackend`].
struct ModeOwner<B: TerminalBackend> {
    backend: B,
    capabilities: TerminalCapabilities,
    token: u64,
    owner: ThreadId,
    finished: bool,
}

impl<B: TerminalBackend + fmt::Debug> fmt::Debug for ModeOwner<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModeOwner")
            .field("backend", &self.backend)
            .field("capabilities", &self.capabilities)
            .field("token", &self.token)
            .field("owner", &self.owner)
            .field("finished", &self.finished)
            .finish()
    }
}

impl<B: TerminalBackend> ModeOwner<B> {
    fn new(backend: B) -> Self {
        let owner = std::thread::current().id();
        let token = REGISTRY.allocate(owner);
        Self {
            backend,
            capabilities: TerminalCapabilities::default(),
            token,
            owner,
            finished: false,
        }
    }

    fn sync_registry(&self) {
        REGISTRY.activate(SessionFlags {
            token: self.token,
            owner: self.owner,
            capabilities: self.capabilities,
        });
    }

    fn acquire(&mut self, cap: TerminalCapability) -> io::Result<()> {
        match cap {
            TerminalCapability::RawMode => self.backend.enable_raw_mode()?,
            TerminalCapability::AlternateScreen => self.backend.enter_alternate_screen()?,
            TerminalCapability::MouseCapture => self.backend.enable_mouse_capture()?,
            TerminalCapability::BracketedPaste => self.backend.enable_bracketed_paste()?,
            TerminalCapability::KeyboardEnhancement => self.backend.push_keyboard_enhancement()?,
        }
        self.capabilities.set(cap, true);
        self.sync_registry();
        Ok(())
    }

    /// Setup modes in doc order. Keyboard enhancement is optional.
    fn setup_modes(&mut self, screen: TuiScreen) -> io::Result<bool> {
        self.acquire(TerminalCapability::RawMode)?;
        if matches!(screen, TuiScreen::Alternate) {
            self.acquire(TerminalCapability::AlternateScreen)?;
        }
        self.acquire(TerminalCapability::MouseCapture)?;
        self.acquire(TerminalCapability::BracketedPaste)?;
        let mut shift_enter = cfg!(windows);
        if self.backend.supports_keyboard_enhancement() {
            self.acquire(TerminalCapability::KeyboardEnhancement)?;
            shift_enter = true;
        }
        Ok(shift_enter)
    }

    fn finish(&mut self) -> RestoreReport {
        if self.finished {
            return RestoreReport::default();
        }
        self.finished = true;
        let (taken, poisoned) = REGISTRY.take_if_recovered(self.token);
        let mut report = match taken {
            None => RestoreReport {
                restored: false,
                skipped_stale: REGISTRY.snapshot().is_some(),
                ..RestoreReport::default()
            },
            Some(flags) => {
                self.capabilities = flags.capabilities;
                restore_held_capabilities(&mut self.capabilities, &mut self.backend)
            }
        };
        if poisoned {
            report.failures.push(RestoreFailure {
                capability: TerminalCapability::RawMode,
                message: "uncertain ownership: registry lock poisoned".to_owned(),
            });
        }
        report
    }
}

impl<B: TerminalBackend> Drop for ModeOwner<B> {
    fn drop(&mut self) {
        if !self.finished {
            let report = self.finish();
            emit_emergency_diagnostics(&report);
        }
    }
}

fn restore_held_capabilities(
    capabilities: &mut TerminalCapabilities,
    backend: &mut impl TerminalBackend,
) -> RestoreReport {
    let mut attempted = Vec::new();
    let mut restored_caps = Vec::new();
    let mut failures = Vec::new();

    for cap in capabilities.held_restore_order() {
        if !capabilities.get(cap) {
            continue;
        }
        attempted.push(cap);
        let result = match cap {
            TerminalCapability::KeyboardEnhancement => backend.pop_keyboard_enhancement(),
            TerminalCapability::BracketedPaste => backend.disable_bracketed_paste(),
            TerminalCapability::MouseCapture => backend.disable_mouse_capture(),
            TerminalCapability::AlternateScreen => backend.leave_alternate_screen(),
            TerminalCapability::RawMode => backend.disable_raw_mode(),
        };
        match result {
            Ok(()) => {
                capabilities.set(cap, false);
                restored_caps.push(cap);
            }
            Err(error) => failures.push(RestoreFailure {
                capability: cap,
                message: format!("{}: {error}", restore_label(cap)),
            }),
        }
    }
    let _ = backend.write_restore_newline();
    RestoreReport {
        restored: true,
        skipped_stale: false,
        attempted,
        restored_caps,
        failures,
    }
}

fn restore_label(cap: TerminalCapability) -> &'static str {
    match cap {
        TerminalCapability::KeyboardEnhancement => "pop keyboard enhancement",
        TerminalCapability::BracketedPaste => "disable bracketed paste",
        TerminalCapability::MouseCapture => "disable mouse",
        TerminalCapability::AlternateScreen => "leave alternate screen",
        TerminalCapability::RawMode => "disable raw mode",
    }
}

fn emit_emergency_diagnostics(report: &RestoreReport) {
    if report.failures.is_empty() && !report.skipped_stale {
        return;
    }
    let mut lines = Vec::new();
    for failure in &report.failures {
        lines.push(format!(
            "terminal restore: {}: {}",
            failure.capability.as_str(),
            failure.message
        ));
    }
    if report.skipped_stale {
        lines.push("terminal restore: skipped stale session".to_owned());
    }

    #[cfg(test)]
    {
        if let Ok(mut guard) = TEST_DIAGNOSTICS.lock()
            && let Some(buf) = guard.as_mut()
        {
            buf.extend(lines);
            return;
        }
    }

    let mut err = io::stderr();
    for line in lines {
        let _ = writeln!(err, "{line}");
    }
}

/// Restore the active registry session on the owner thread (panic / uncertain paths).
fn emergency_restore_active_session(backend: &mut impl TerminalBackend) -> RestoreReport {
    let Some(flags) = REGISTRY.snapshot() else {
        return RestoreReport::default();
    };
    if std::thread::current().id() != flags.owner {
        return RestoreReport::default();
    }
    match REGISTRY.try_take_if(flags.token) {
        Ok(Some(taken)) => {
            let mut caps = taken.capabilities;
            restore_held_capabilities(&mut caps, backend)
        }
        Ok(None) => RestoreReport {
            restored: false,
            skipped_stale: REGISTRY.snapshot().is_some(),
            ..RestoreReport::default()
        },
        Err(taken) => {
            let mut caps = taken.unwrap_or(flags).capabilities;
            let mut report = restore_held_capabilities(&mut caps, backend);
            report.failures.push(RestoreFailure {
                capability: TerminalCapability::RawMode,
                message: "uncertain ownership: registry lock unavailable".to_owned(),
            });
            report
        }
    }
}

fn build_ratatui_terminal(screen: TuiScreen) -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    let backend = CrosstermBackend::new(stdout());
    match screen {
        TuiScreen::Alternate => Terminal::new(backend),
        TuiScreen::Inline => {
            let (_, height) = crossterm::terminal::size()?;
            Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Inline(height),
                },
            )
        }
    }
}

/// One terminal session: raw mode plus either an isolated alternate screen
/// or an inline viewport on the main buffer. The type is owner-thread-bound.
pub struct TerminalSession {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
    /// Whether `Shift+Enter` is deliverable (Windows native or kitty
    /// enhancement); the hint line adapts.
    pub shift_enter: bool,
    modes: ModeOwner<CrosstermTerminalBackend>,
    _not_send: PhantomData<*const ()>,
}

impl TerminalSession {
    /// Takes terminal ownership and installs the panic-restore hook.
    pub fn enter(screen: TuiScreen) -> Result<Self, TerminalEnterError> {
        install_panic_hook();
        let mut modes = ModeOwner::new(CrosstermTerminalBackend);
        let shift_enter = match modes.setup_modes(screen) {
            Ok(shift_enter) => shift_enter,
            Err(fault) => {
                let restore = modes.finish();
                return Err(TerminalEnterError { fault, restore });
            }
        };

        let terminal = match build_ratatui_terminal(screen) {
            Ok(terminal) => terminal,
            Err(fault) => {
                let restore = modes.finish();
                return Err(TerminalEnterError { fault, restore });
            }
        };

        Ok(Self {
            terminal,
            shift_enter,
            modes,
            _not_send: PhantomData,
        })
    }

    /// Owner thread of this session.
    pub fn owner(&self) -> ThreadId {
        self.modes.owner
    }

    /// Restores the terminal. A stale token cannot tear down a newer session.
    pub fn finish(&mut self) -> RestoreReport {
        debug_assert_eq!(
            std::thread::current().id(),
            self.owner(),
            "TerminalSession::finish must run on the owner thread"
        );
        self.modes.finish()
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if !self.modes.finished {
            let report = self.modes.finish();
            emit_emergency_diagnostics(&report);
        }
    }
}

fn install_panic_hook() {
    static HOOKED: AtomicBool = AtomicBool::new(false);
    if HOOKED.swap(true, Ordering::SeqCst) {
        return;
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut backend = CrosstermTerminalBackend;
        let report = emergency_restore_active_session(&mut backend);
        emit_emergency_diagnostics(&report);
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DiagnosticGuard {
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for DiagnosticGuard {
        fn drop(&mut self) {
            if let Ok(mut guard) = TEST_DIAGNOSTICS.lock() {
                *guard = None;
            }
        }
    }

    fn capture_diagnostics() -> DiagnosticGuard {
        let serial = lock_terminal_tests();
        let mut guard = TEST_DIAGNOSTICS.lock().expect("diagnostics lock");
        *guard = Some(Vec::new());
        drop(guard);
        DiagnosticGuard { _serial: serial }
    }

    fn lock_terminal_tests() -> std::sync::MutexGuard<'static, ()> {
        let guard = TERMINAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Isolate registry ownership across serialized tests.
        let (mut active, _) = REGISTRY.lock_active();
        *active = None;
        drop(active);
        REGISTRY.active.clear_poison();
        REGISTRY.poisoned.store(false, Ordering::SeqCst);
        guard
    }

    fn diagnostic_lines() -> Vec<String> {
        TEST_DIAGNOSTICS
            .lock()
            .expect("diagnostics lock")
            .clone()
            .unwrap_or_default()
    }

    fn run_setup(
        backend: RecordingBackend,
        screen: TuiScreen,
    ) -> Result<ModeOwner<RecordingBackend>, (ModeOwner<RecordingBackend>, io::Error)> {
        let mut modes = ModeOwner::new(backend);
        match modes.setup_modes(screen) {
            Ok(_) => Ok(modes),
            Err(fault) => Err((modes, fault)),
        }
    }

    #[test]
    fn setup_step1_failure_has_no_restore_calls() {
        let _lock = lock_terminal_tests();
        let backend = RecordingBackend::new().with_setup_fail_at(0);
        let (mut modes, fault) = run_setup(backend, TuiScreen::Alternate).unwrap_err();
        assert!(fault.to_string().contains("enable_raw_mode"));
        let report = modes.finish();
        assert!(!report.restored);
        assert!(report.attempted.is_empty());
        assert_eq!(modes.backend.call_names(), vec!["enable_raw_mode"]);
    }

    #[test]
    fn setup_step2_failure_only_disables_raw() {
        let _lock = lock_terminal_tests();
        let backend = RecordingBackend::new().with_setup_fail_at(1);
        let (mut modes, _) = run_setup(backend, TuiScreen::Alternate).unwrap_err();
        let report = modes.finish();
        assert!(report.restored);
        assert_eq!(report.attempted, vec![TerminalCapability::RawMode]);
        assert_eq!(
            modes.backend.call_names(),
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "disable_raw_mode",
                "write_restore_newline",
            ]
        );
    }

    #[test]
    fn setup_step3_failure_leaves_alternate_then_raw() {
        let _lock = lock_terminal_tests();
        let backend = RecordingBackend::new().with_setup_fail_at(2);
        let (mut modes, _) = run_setup(backend, TuiScreen::Alternate).unwrap_err();
        let _ = modes.finish();
        assert_eq!(
            modes.backend.call_names(),
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "enable_mouse_capture",
                "leave_alternate_screen",
                "disable_raw_mode",
                "write_restore_newline",
            ]
        );
    }

    #[test]
    fn setup_step4_failure_restores_inverse_stack() {
        let _lock = lock_terminal_tests();
        let backend = RecordingBackend::new().with_setup_fail_at(3);
        let (mut modes, _) = run_setup(backend, TuiScreen::Alternate).unwrap_err();
        let _ = modes.finish();
        assert_eq!(
            modes.backend.call_names(),
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "enable_mouse_capture",
                "enable_bracketed_paste",
                "disable_mouse_capture",
                "leave_alternate_screen",
                "disable_raw_mode",
                "write_restore_newline",
            ]
        );
    }

    #[test]
    fn setup_step5_failure_restores_all_prior_inverse() {
        let _lock = lock_terminal_tests();
        let backend = RecordingBackend::new().with_setup_fail_at(4);
        let (mut modes, _) = run_setup(backend, TuiScreen::Alternate).unwrap_err();
        let _ = modes.finish();
        assert_eq!(
            modes.backend.call_names(),
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "enable_mouse_capture",
                "enable_bracketed_paste",
                "push_keyboard_enhancement",
                "disable_bracketed_paste",
                "disable_mouse_capture",
                "leave_alternate_screen",
                "disable_raw_mode",
                "write_restore_newline",
            ]
        );
    }

    #[test]
    fn restore_step_failures_continue_and_collect_all() {
        let _lock = lock_terminal_tests();
        let backend = RecordingBackend::new().with_restore_fail(vec![
            TerminalCapability::KeyboardEnhancement,
            TerminalCapability::MouseCapture,
            TerminalCapability::RawMode,
        ]);
        let mut modes = run_setup(backend, TuiScreen::Alternate).expect("setup");
        let report = modes.finish();
        assert_eq!(report.failures.len(), 3);
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.capability == TerminalCapability::KeyboardEnhancement)
        );
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.capability == TerminalCapability::MouseCapture)
        );
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.capability == TerminalCapability::RawMode)
        );
        assert!(
            modes
                .backend
                .calls
                .contains(&BackendOp::DisableBracketedPaste)
        );
        assert!(
            modes
                .backend
                .calls
                .contains(&BackendOp::LeaveAlternateScreen)
        );
    }

    #[test]
    fn consecutive_finish_is_idempotent() {
        let _lock = lock_terminal_tests();
        let mut modes = run_setup(RecordingBackend::new(), TuiScreen::Inline).expect("setup");
        let first = modes.finish();
        assert!(first.restored);
        let calls = modes.backend.calls.len();
        let second = modes.finish();
        assert!(!second.restored);
        assert!(second.attempted.is_empty());
        assert_eq!(modes.backend.calls.len(), calls);
    }

    #[test]
    fn finish_then_drop_does_not_restore_again() {
        let _lock = lock_terminal_tests();
        let mut modes = run_setup(RecordingBackend::new(), TuiScreen::Alternate).expect("setup");
        let _ = modes.finish();
        let calls = modes.backend.calls.len();
        assert!(modes.finished);
        drop(modes);
        let mut modes = run_setup(RecordingBackend::new(), TuiScreen::Alternate).expect("setup");
        let _ = modes.finish();
        let calls_after = modes.backend.calls.len();
        assert_eq!(calls, calls_after);
        drop(modes);
    }

    #[test]
    fn inline_setup_skips_alternate_screen() {
        let _lock = lock_terminal_tests();
        let mut modes = run_setup(RecordingBackend::new(), TuiScreen::Inline).expect("setup");
        assert!(!modes.capabilities.alternate_screen);
        assert!(modes.capabilities.raw_mode);
        assert!(modes.capabilities.mouse_capture);
        let _ = modes.finish();
        assert!(
            !modes
                .backend
                .calls
                .contains(&BackendOp::EnterAlternateScreen)
        );
        assert!(
            !modes
                .backend
                .calls
                .contains(&BackendOp::LeaveAlternateScreen)
        );
    }

    #[test]
    fn stale_token_leaves_newer_registry_intact() {
        let _lock = lock_terminal_tests();
        let mut older = ModeOwner::new(RecordingBackend::new());
        older.setup_modes(TuiScreen::Inline).expect("old setup");
        let mut newer = ModeOwner::new(RecordingBackend::new());
        newer.setup_modes(TuiScreen::Inline).expect("new setup");
        let new_token = newer.token;
        let report = older.finish();
        assert!(!report.restored);
        assert!(report.skipped_stale);
        assert_eq!(REGISTRY.snapshot().map(|f| f.token), Some(new_token));
        let _ = newer.finish();
    }

    #[test]
    fn matching_token_clears_registry() {
        let _lock = lock_terminal_tests();
        let mut modes = run_setup(RecordingBackend::new(), TuiScreen::Inline).expect("setup");
        let report = modes.finish();
        assert!(report.restored);
        assert!(!report.skipped_stale);
        assert!(REGISTRY.snapshot().is_none());
    }

    #[test]
    fn drop_without_finish_emits_diagnostics_on_failure() {
        let _capture = capture_diagnostics();
        let backend = RecordingBackend::new().with_restore_fail(vec![TerminalCapability::RawMode]);
        let modes = run_setup(backend, TuiScreen::Inline).expect("setup");
        drop(modes);
        let lines = diagnostic_lines();
        assert!(
            lines.iter().any(|line| line.contains("raw mode")),
            "expected diagnostic lines, got {lines:?}"
        );
    }

    #[test]
    fn emergency_restore_runs_and_records_failures() {
        let _capture = capture_diagnostics();
        let mut modes = run_setup(
            RecordingBackend::new().with_restore_fail(vec![TerminalCapability::BracketedPaste]),
            TuiScreen::Inline,
        )
        .expect("setup");
        let token = modes.token;
        modes.finished = true;
        let mut panic_backend =
            RecordingBackend::new().with_restore_fail(vec![TerminalCapability::BracketedPaste]);
        assert_eq!(REGISTRY.snapshot().map(|f| f.token), Some(token));
        let report = emergency_restore_active_session(&mut panic_backend);
        assert!(report.restored);
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.capability == TerminalCapability::BracketedPaste)
        );
        emit_emergency_diagnostics(&report);
        assert!(!diagnostic_lines().is_empty());
    }

    #[test]
    fn panic_hook_wrapper_chains_to_previous_hook() {
        let _lock = lock_terminal_tests();
        use std::sync::Arc;
        let chained = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&chained);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |_| {
            flag.store(true, Ordering::SeqCst);
        }));
        let inner = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut backend = RecordingBackend::new();
            let report = emergency_restore_active_session(&mut backend);
            emit_emergency_diagnostics(&report);
            inner(info);
        }));
        let _ = std::panic::catch_unwind(|| {
            panic!("terminal-raii-test");
        });
        assert!(chained.load(Ordering::SeqCst));
        let _ = std::panic::take_hook();
        std::panic::set_hook(previous);
    }

    #[test]
    fn poisoned_registry_recovers_and_diagnoses_uncertain() {
        let _lock = lock_terminal_tests();
        let mut modes = run_setup(RecordingBackend::new(), TuiScreen::Inline).expect("setup");
        modes.finished = true;
        let _ = std::panic::catch_unwind(|| {
            let _guard = REGISTRY.active.lock().expect("registry lock");
            panic!("poison registry");
        });
        assert!(
            REGISTRY.snapshot().is_some(),
            "poisoned registry must recover the active session"
        );
        let mut backend = RecordingBackend::new();
        let report = emergency_restore_active_session(&mut backend);
        assert!(report.restored);
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.message.contains("uncertain")),
            "{report:?}"
        );
    }
}
