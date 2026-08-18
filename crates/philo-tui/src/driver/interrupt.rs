//! Supervisor-owned Ctrl+C pulses consumed by the TUI event loop.
//!
//! The TUI never exits the process. It only cancels, requests a user exit,
//! or returns [`crate::TuiOutcome::ForcedExitRequested`].
//!
//! Cancelling is entered only after cancel command enqueue is accepted
//! (`CommandDispatch::Enqueued`). Backpressure keeps the phase Busy.

use philo_agent_service::FrontendRequestId;

use crate::app::submit::CancelDispatchResult;

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
    /// Cancel command is being dispatched; not yet accepted by the lane.
    CancelDispatching {
        operation_id: Option<String>,
        /// Local correlation id for the in-flight cancel dispatch.
        cancel_request: u64,
    },
    /// Cancel was enqueued; grace / second Ctrl+C may escalate.
    Cancelling {
        operation_id: Option<String>,
        request_id: Option<FrontendRequestId>,
    },
}

/// Decision produced by one Ctrl+C pulse or keyboard interrupt-cancel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CtrlCDecision {
    UserExit,
    /// Enter CancelDispatching and dispatch cancel for this operation.
    Cancel {
        operation_id: String,
        cancel_request: u64,
    },
    /// Busy without an id yet; wait for OperationAccepted.
    WaitForId {
        cancel_request: u64,
    },
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
                ..
            }
            | Self::CancelDispatching {
                operation_id: current,
                ..
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

    /// First Ctrl+C while busy begins cancel dispatch; only `Cancelling`
    /// (post-enqueue) escalates to forced exit.
    pub(crate) fn on_ctrl_c(&mut self, next_cancel_request: u64) -> CtrlCDecision {
        match self {
            Self::Idle => CtrlCDecision::UserExit,
            Self::Busy {
                operation_id: Some(operation_id),
            } => {
                let operation_id = operation_id.clone();
                *self = Self::CancelDispatching {
                    operation_id: Some(operation_id.clone()),
                    cancel_request: next_cancel_request,
                };
                CtrlCDecision::Cancel {
                    operation_id,
                    cancel_request: next_cancel_request,
                }
            }
            Self::Busy { operation_id: None } => {
                *self = Self::CancelDispatching {
                    operation_id: None,
                    cancel_request: next_cancel_request,
                };
                CtrlCDecision::WaitForId {
                    cancel_request: next_cancel_request,
                }
            }
            Self::CancelDispatching { cancel_request, .. } => CtrlCDecision::WaitForId {
                cancel_request: *cancel_request,
            },
            Self::Cancelling { .. } => CtrlCDecision::ForcedExit {
                code: FORCED_EXIT_CODE,
            },
        }
    }

    pub(crate) fn pending_cancel_id(&self) -> Option<&str> {
        match self {
            Self::Cancelling {
                operation_id: Some(id),
                ..
            }
            | Self::CancelDispatching {
                operation_id: Some(id),
                ..
            } => Some(id.as_str()),
            _ => None,
        }
    }

    pub(crate) fn cancel_request(&self) -> Option<u64> {
        match self {
            Self::CancelDispatching { cancel_request, .. } => Some(*cancel_request),
            _ => None,
        }
    }

    /// Apply structured cancel dispatch result. Returns a user-visible notice
    /// when the cancel was not accepted.
    pub(crate) fn on_cancel_dispatch_finished(
        &mut self,
        cancel_request: u64,
        result: CancelDispatchResult,
    ) -> Option<&'static str> {
        let matches = matches!(
            self,
            Self::CancelDispatching {
                cancel_request: current,
                ..
            } if *current == cancel_request
        );
        if !matches {
            return None;
        }
        let operation_id = match self {
            Self::CancelDispatching { operation_id, .. } => operation_id.clone(),
            _ => None,
        };
        match result {
            CancelDispatchResult::Enqueued(request_id) => {
                *self = Self::Cancelling {
                    operation_id,
                    request_id: Some(request_id),
                };
                None
            }
            CancelDispatchResult::Backpressured => {
                *self = Self::Busy { operation_id };
                Some("取消请求未发送")
            }
            CancelDispatchResult::Disconnected { .. } => {
                *self = Self::Busy { operation_id };
                Some("取消请求未发送（连接已断开）")
            }
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
    fn busy_first_enters_dispatching_not_cancelling() {
        let mut phase = CtrlCPhase::Idle;
        phase.observe_busy("op-1");
        assert!(matches!(
            phase.on_ctrl_c(1),
            CtrlCDecision::Cancel {
                operation_id,
                cancel_request: 1,
            } if operation_id == "op-1"
        ));
        assert!(matches!(
            phase,
            CtrlCPhase::CancelDispatching {
                operation_id: Some(ref id),
                cancel_request: 1,
            } if id == "op-1"
        ));
    }

    #[test]
    fn backpressure_returns_to_busy_without_cancelling() {
        let mut phase = CtrlCPhase::Idle;
        phase.observe_busy("op-1");
        let _ = phase.on_ctrl_c(7);
        let notice = phase.on_cancel_dispatch_finished(7, CancelDispatchResult::Backpressured);
        assert_eq!(notice, Some("取消请求未发送"));
        assert_eq!(
            phase,
            CtrlCPhase::Busy {
                operation_id: Some("op-1".to_owned()),
            }
        );
    }

    #[test]
    fn enqueue_enters_cancelling_then_second_forces() {
        let mut phase = CtrlCPhase::Idle;
        phase.observe_busy("op-1");
        let _ = phase.on_ctrl_c(3);
        assert!(
            phase
                .on_cancel_dispatch_finished(
                    3,
                    CancelDispatchResult::Enqueued(FrontendRequestId::new(9)),
                )
                .is_none()
        );
        assert!(matches!(phase, CtrlCPhase::Cancelling { .. }));
        assert_eq!(
            phase.on_ctrl_c(4),
            CtrlCDecision::ForcedExit {
                code: FORCED_EXIT_CODE,
            }
        );
    }

    #[test]
    fn idle_after_settle_is_a_fresh_first_press() {
        let mut phase = CtrlCPhase::Idle;
        phase.observe_busy("op-1");
        let _ = phase.on_ctrl_c(1);
        assert!(
            phase
                .on_cancel_dispatch_finished(
                    1,
                    CancelDispatchResult::Enqueued(FrontendRequestId::new(1)),
                )
                .is_none()
        );
        phase.observe_idle();
        assert_eq!(phase.on_ctrl_c(2), CtrlCDecision::UserExit);
    }

    #[test]
    fn dispatching_second_ctrl_c_does_not_force() {
        let mut phase = CtrlCPhase::Idle;
        phase.observe_busy("op-1");
        let first = phase.on_ctrl_c(1);
        assert!(matches!(
            first,
            CtrlCDecision::Cancel {
                cancel_request: 1,
                ..
            }
        ));
        assert_eq!(
            phase.on_ctrl_c(2),
            CtrlCDecision::WaitForId { cancel_request: 1 }
        );
        assert!(matches!(
            phase,
            CtrlCPhase::CancelDispatching {
                operation_id: Some(ref id),
                cancel_request: 1,
            } if id == "op-1"
        ));
    }

    #[test]
    fn wait_for_id_second_ctrl_c_keeps_waiting() {
        let mut phase = CtrlCPhase::Busy { operation_id: None };
        assert_eq!(
            phase.on_ctrl_c(5),
            CtrlCDecision::WaitForId { cancel_request: 5 }
        );
        assert_eq!(
            phase.on_ctrl_c(6),
            CtrlCDecision::WaitForId { cancel_request: 5 }
        );
        assert!(matches!(
            phase,
            CtrlCPhase::CancelDispatching {
                operation_id: None,
                cancel_request: 5,
            }
        ));
    }
}
