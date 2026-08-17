//! Process-level Ctrl+C policy. The watcher only forwards pulses; it never
//! writes the terminal or exits the process.

use tokio::sync::watch;

/// Exit code for SIGINT / forced quit (`128 + 2`).
pub(crate) const FORCED_EXIT_CODE: u8 = 130;

/// Bound of the current cancel/force epoch. Settled, idle, and frontend
/// restart all return to [`CtrlCPhase::Idle`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum CtrlCPhase {
    #[default]
    Idle,
    /// Work is active. `None` means submitted but the id is not known yet.
    Busy { operation_id: Option<String> },
    /// First Ctrl+C already requested cancel for this operation (or unknown id).
    Cancelling { operation_id: Option<String> },
}

/// Decision produced by one Ctrl+C pulse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CtrlCDecision {
    UserExit,
    Cancel {
        operation_id: String,
    },
    /// First pulse arrived before the operation id; cancel when it appears.
    WaitForId,
    ForcedExit {
        code: u8,
    },
}

impl CtrlCPhase {
    pub(crate) fn observe_busy(&mut self, operation_id: impl Into<String>) {
        let operation_id = operation_id.into();
        match self {
            Self::Cancelling {
                operation_id: current,
            } if current.as_ref().is_none_or(|id| *id == operation_id) => {
                *current = Some(operation_id);
            }
            _ => {
                *self = Self::Busy {
                    operation_id: Some(operation_id),
                }
            }
        }
    }

    pub(crate) fn observe_idle(&mut self) {
        *self = Self::Idle;
    }

    #[cfg(test)]
    pub(crate) fn observe_restart(&mut self) {
        *self = Self::Idle;
    }

    pub(crate) fn on_ctrl_c(&mut self) -> CtrlCDecision {
        match self {
            Self::Idle => CtrlCDecision::UserExit,
            Self::Busy {
                operation_id: Some(operation_id),
            } => {
                let operation_id = operation_id.clone();
                *self = Self::Cancelling {
                    operation_id: Some(operation_id.clone()),
                };
                CtrlCDecision::Cancel { operation_id }
            }
            Self::Busy { operation_id: None } => {
                *self = Self::Cancelling { operation_id: None };
                CtrlCDecision::WaitForId
            }
            Self::Cancelling { .. } => CtrlCDecision::ForcedExit {
                code: FORCED_EXIT_CODE,
            },
        }
    }

    /// When an id arrives after [`CtrlCDecision::WaitForId`], send cancel now.
    pub(crate) fn pending_cancel_id(&self) -> Option<&str> {
        match self {
            Self::Cancelling {
                operation_id: Some(id),
            } => Some(id.as_str()),
            _ => None,
        }
    }
}

/// How many new Ctrl+C pulses arrived since `seen`.
pub(crate) fn take_pulses(rx: &mut watch::Receiver<u64>, seen: &mut u64) -> u64 {
    let now = *rx.borrow_and_update();
    let delta = now.saturating_sub(*seen);
    *seen = now;
    delta
}

/// Marks the current pulse count as consumed so a new TUI/oneshot run does
/// not replay historical SIGINTs.
pub(crate) fn skip_past_pulses(rx: &mut watch::Receiver<u64>) -> u64 {
    *rx.borrow_and_update()
}

/// Increments the shared pulse counter. Used by the OS watcher and tests.
pub(crate) fn pulse(tx: &watch::Sender<u64>) {
    tx.send_modify(|n| *n = n.saturating_add(1));
}

/// Forwards OS Ctrl+C into `tx`. Never writes the terminal or exits.
pub(crate) async fn forward_os_ctrl_c(tx: watch::Sender<u64>) {
    loop {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        pulse(&tx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_ctrl_c_is_user_exit() {
        let mut phase = CtrlCPhase::Idle;
        assert_eq!(phase.on_ctrl_c(), CtrlCDecision::UserExit);
    }

    #[test]
    fn busy_first_cancels_second_forces() {
        let mut phase = CtrlCPhase::Idle;
        phase.observe_busy("op-1");
        assert_eq!(
            phase.on_ctrl_c(),
            CtrlCDecision::Cancel {
                operation_id: "op-1".into(),
            }
        );
        assert_eq!(
            phase.on_ctrl_c(),
            CtrlCDecision::ForcedExit {
                code: FORCED_EXIT_CODE,
            }
        );
    }

    #[test]
    fn settled_resets_so_next_ctrl_c_is_first() {
        let mut phase = CtrlCPhase::Idle;
        phase.observe_busy("op-1");
        assert!(matches!(phase.on_ctrl_c(), CtrlCDecision::Cancel { .. }));
        phase.observe_idle();
        phase.observe_busy("op-2");
        assert_eq!(
            phase.on_ctrl_c(),
            CtrlCDecision::Cancel {
                operation_id: "op-2".into(),
            }
        );
    }

    #[test]
    fn restart_clears_a_cancelling_epoch() {
        let mut phase = CtrlCPhase::Idle;
        phase.observe_busy("op-1");
        let _ = phase.on_ctrl_c();
        phase.observe_restart();
        phase.observe_busy("op-1");
        assert!(matches!(phase.on_ctrl_c(), CtrlCDecision::Cancel { .. }));
    }

    #[test]
    fn unknown_id_waits_then_cancels_when_id_arrives() {
        let mut phase = CtrlCPhase::Busy { operation_id: None };
        assert_eq!(phase.on_ctrl_c(), CtrlCDecision::WaitForId);
        phase.observe_busy("op-9");
        assert_eq!(phase.pending_cancel_id(), Some("op-9"));
        assert_eq!(
            phase.on_ctrl_c(),
            CtrlCDecision::ForcedExit {
                code: FORCED_EXIT_CODE,
            }
        );
    }

    #[test]
    fn take_pulses_counts_coalesced_ctrl_c() {
        let (tx, mut rx) = watch::channel(0u64);
        let mut seen = skip_past_pulses(&mut rx);
        pulse(&tx);
        pulse(&tx);
        assert_eq!(take_pulses(&mut rx, &mut seen), 2);
        assert_eq!(take_pulses(&mut rx, &mut seen), 0);
    }
}
