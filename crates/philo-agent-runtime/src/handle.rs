//! Cloneable control plane: send commands, never hold the engine.

use std::time::Instant;

use crate::coordinator::{ControlMessage, RuntimeCommand};
use crate::shutdown::{ShutdownOutcome, ShutdownRequest, merge_shutdown};
use crate::{
    AdmissionError, CancelResult, CompactionSpec, MaintenanceAccepted, MaintenanceError,
    OperationAccepted, OperationId, OperationSpec, OutboundStats, RuntimeSnapshot, ShutdownError,
    ShutdownMode, ShutdownReport,
};
use tokio::sync::{mpsc, oneshot, watch};

/// Cloneable handle to one runtime epoch. Sends commands only.
#[derive(Clone)]
pub struct RuntimeHandle {
    pub(crate) command_tx: mpsc::Sender<RuntimeCommand>,
    pub(crate) control_tx: mpsc::Sender<ControlMessage>,
    pub(crate) snapshot_rx: watch::Receiver<RuntimeSnapshot>,
    pub(crate) shutdown_tx: watch::Sender<Option<ShutdownRequest>>,
    pub(crate) completion_rx: watch::Receiver<Option<ShutdownOutcome>>,
}

impl std::fmt::Debug for RuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeHandle").finish_non_exhaustive()
    }
}

impl RuntimeHandle {
    pub async fn submit(&self, spec: OperationSpec) -> Result<OperationAccepted, AdmissionError> {
        let (reply, rx) = oneshot::channel();
        self.command_tx
            .try_send(RuntimeCommand::Submit { spec, reply })
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => AdmissionError::Backpressured,
                mpsc::error::TrySendError::Closed(_) => AdmissionError::RuntimeStopped,
            })?;
        rx.await.map_err(|_| AdmissionError::RuntimeStopped)?
    }

    pub async fn cancel(&self, operation_id: OperationId) -> CancelResult {
        let (reply, rx) = oneshot::channel();
        if self
            .control_tx
            .try_send(ControlMessage::Cancel {
                operation_id,
                reply,
            })
            .is_err()
        {
            return match self.control_tx.is_closed() {
                true => CancelResult::RuntimeStopped,
                false => CancelResult::Backpressured,
            };
        }
        rx.await.unwrap_or(CancelResult::RuntimeStopped)
    }

    pub async fn start_compaction(
        &self,
        spec: CompactionSpec,
    ) -> Result<MaintenanceAccepted, MaintenanceError> {
        let (reply, rx) = oneshot::channel();
        self.command_tx
            .try_send(RuntimeCommand::StartCompaction { spec, reply })
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => MaintenanceError::Backpressured,
                mpsc::error::TrySendError::Closed(_) => MaintenanceError::RuntimeStopped,
            })?;
        rx.await.map_err(|_| MaintenanceError::RuntimeStopped)?
    }

    pub async fn cancel_maintenance(&self, id: crate::MaintenanceId) -> CancelResult {
        let (reply, rx) = oneshot::channel();
        if self
            .control_tx
            .try_send(ControlMessage::CancelMaintenance { id, reply })
            .is_err()
        {
            return match self.control_tx.is_closed() {
                true => CancelResult::RuntimeStopped,
                false => CancelResult::Backpressured,
            };
        }
        rx.await.unwrap_or(CancelResult::RuntimeStopped)
    }

    pub async fn snapshot(&self) -> RuntimeSnapshot {
        self.snapshot_rx.borrow().clone()
    }

    pub async fn shutdown(
        &self,
        mode: ShutdownMode,
        deadline: Instant,
    ) -> Result<ShutdownReport, ShutdownError> {
        if Instant::now() >= deadline {
            return Err(ShutdownError::DeadlineExceeded {
                pending: vec!["runtime".into()],
            });
        }
        if let Some(outcome) = self.completion_rx.borrow().clone() {
            return outcome;
        }
        if self.shutdown_tx.receiver_count() == 0 {
            return Err(ShutdownError::RuntimeGone);
        }
        self.shutdown_tx.send_modify(|slot| {
            *slot = Some(merge_shutdown(
                *slot,
                ShutdownRequest { mode, deadline },
            ));
        });
        let mut completion_rx = self.completion_rx.clone();
        loop {
            if let Some(outcome) = completion_rx.borrow().clone() {
                return outcome;
            }
            tokio::select! {
                changed = completion_rx.changed() => {
                    if changed.is_err() {
                        return match completion_rx.borrow().clone() {
                            Some(outcome) => outcome,
                            None => Err(ShutdownError::SupervisorPanicked),
                        };
                    }
                }
                _ = tokio::time::sleep(deadline.saturating_duration_since(Instant::now())) => {
                    return Err(ShutdownError::DeadlineExceeded {
                        pending: vec!["runtime".into()],
                    });
                }
            }
        }
    }

    /// Test hook: occupy one control-mailbox slot without requesting shutdown.
    #[doc(hidden)]
    pub fn try_send_control_probe(&self) -> bool {
        let (reply, _rx) = oneshot::channel();
        self.control_tx
            .try_send(ControlMessage::PublishOutboundStats { reply })
            .is_ok()
    }

    /// Test hook: panic the coordinator actor on the next control turn.
    #[doc(hidden)]
    pub async fn inject_coordinator_panic(&self) {
        let _ = self
            .control_tx
            .send(ControlMessage::InjectCoordinatorPanic)
            .await;
    }

    /// Test hook: force a coordinator turn that republishes the live snapshot.
    #[doc(hidden)]
    pub async fn publish_snapshot(&self) -> RuntimeSnapshot {
        let (reply, rx) = oneshot::channel();
        if self
            .control_tx
            .try_send(ControlMessage::PublishSnapshot { reply })
            .is_err()
        {
            return self.snapshot().await;
        }
        rx.await
            .unwrap_or_else(|_| self.snapshot_rx.borrow().clone())
    }

    /// Test hook: occupancy of reliable staging and the transient coalescer.
    #[doc(hidden)]
    pub async fn outbound_stats(&self) -> OutboundStats {
        let (reply, rx) = oneshot::channel();
        if self
            .control_tx
            .try_send(ControlMessage::PublishOutboundStats { reply })
            .is_err()
        {
            return OutboundStats {
                reliable_staging_len: 0,
                reliable_staging_cap: 0,
                transient_len: 0,
                transient_cap: 0,
            };
        }
        rx.await.unwrap_or(OutboundStats {
            reliable_staging_len: 0,
            reliable_staging_cap: 0,
            transient_len: 0,
            transient_cap: 0,
        })
    }
}
