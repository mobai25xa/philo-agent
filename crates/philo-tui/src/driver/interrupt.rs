//! Supervisor-owned Ctrl+C pulses consumed by the TUI event loop.
//!
//! The TUI never exits the process. It only cancels, requests a user exit,
//! or returns [`crate::TuiOutcome::ForcedExitRequested`].

/// Exit code for SIGINT / forced quit (`128 + 2`).
pub(crate) const FORCED_EXIT_CODE: u8 = 130;

/// Bound of the current cancel/force epoch. Idle, settled, and restart reset it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum CtrlCPhase {
    #[default]
    Idle,
    Busy {
        operation_id: Option<String>,
    },
    Cancelling {
        operation_id: Option<String>,
    },
}

/// Decision produced by one Ctrl+C pulse or keyboard interrupt-cancel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CtrlCDecision {
    UserExit,
    Cancel { operation_id: String },
    WaitForId,
    ForcedExit { code: u8 },
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
pub(crate) fn take_pulses(rx: &mut tokio::sync::watch::Receiver<u64>, seen: &mut u64) -> u64 {
    let now = *rx.borrow_and_update();
    let delta = now.saturating_sub(*seen);
    *seen = now;
    delta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_first_cancels_same_op_second_forces() {
        let mut phase = CtrlCPhase::Idle;
        phase.observe_busy("op-1");
        assert!(matches!(phase.on_ctrl_c(), CtrlCDecision::Cancel { .. }));
        assert_eq!(
            phase.on_ctrl_c(),
            CtrlCDecision::ForcedExit {
                code: FORCED_EXIT_CODE,
            }
        );
    }

    #[test]
    fn idle_after_settle_is_a_fresh_first_press() {
        let mut phase = CtrlCPhase::Idle;
        phase.observe_busy("op-1");
        let _ = phase.on_ctrl_c();
        phase.observe_idle();
        assert_eq!(phase.on_ctrl_c(), CtrlCDecision::UserExit);
    }
}
