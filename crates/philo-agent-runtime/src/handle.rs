//! Cloneable control plane: send commands, never hold the engine.

use std::time::Instant;

use crate::coordinator::{ControlMessage, RuntimeCommand};
use crate::{
    AdmissionError, CancelResult, CompactionSpec, MaintenanceAccepted, MaintenanceError,
    OperationAccepted, OperationId, OperationSpec, RuntimeSnapshot, ShutdownError, ShutdownMode,
    ShutdownReport,
};
use tokio::sync::{mpsc, oneshot, watch};

/// Cloneable handle to one runtime epoch. Sends commands only.
#[derive(Clone)]
pub struct RuntimeHandle {
    pub(crate) command_tx: mpsc::Sender<RuntimeCommand>,
    pub(crate) control_tx: mpsc::Sender<ControlMessage>,
    pub(crate) snapshot_rx: watch::Receiver<RuntimeSnapshot>,
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
        let (reply, rx) = oneshot::channel();
        if self
            .control_tx
            .try_send(ControlMessage::Shutdown { mode, reply })
            .is_err()
        {
            return Err(ShutdownError::RuntimeGone);
        }
        rx.await.map_err(|_| ShutdownError::SupervisorPanicked)
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
}
