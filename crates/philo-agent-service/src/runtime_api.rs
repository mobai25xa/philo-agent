//! Runtime actor ports. Value types are owned by `philo-agent-runtime`.
//!
//! [`RuntimePort`] / [`RuntimeEvents`] let tests inject
//! [`crate::testing::FakeRuntimeHandle`] while production code uses
//! [`philo_agent_runtime::RuntimeHandle`] / [`philo_agent_runtime::RuntimeEventReceiver`].

use std::future::Future;
use std::time::Instant;

use philo_agent_runtime::{
    AdmissionError, CancelResult, CompactionSpec, MaintenanceAccepted, MaintenanceError,
    MaintenanceId, OperationAccepted, OperationId, OperationSpec, RuntimeEvent,
    RuntimeEventReceiver, RuntimeHandle, RuntimeSnapshot, ShutdownError, ShutdownMode,
    ShutdownReport, TryRecvError,
};

/// Cloneable runtime control plane. Does not hold an engine.
pub trait RuntimePort: Clone + Send + Sync + 'static {
    /// Admits one user operation, freezing `spec.generation`.
    fn submit(
        &self,
        spec: OperationSpec,
    ) -> impl Future<Output = Result<OperationAccepted, AdmissionError>> + Send;

    /// Requests cancel for one operation.
    fn cancel(&self, operation_id: OperationId) -> impl Future<Output = CancelResult> + Send;

    /// Admits manual compaction when idle.
    fn start_compaction(
        &self,
        spec: CompactionSpec,
    ) -> impl Future<Output = Result<MaintenanceAccepted, MaintenanceError>> + Send;

    /// Requests cancel for one maintenance task.
    fn cancel_maintenance(&self, id: MaintenanceId) -> impl Future<Output = CancelResult> + Send;

    /// Reads the coordinator snapshot.
    fn snapshot(&self) -> impl Future<Output = RuntimeSnapshot> + Send;

    /// Stops the coordinator. Success only comes from the real supervisor.
    fn shutdown(
        &self,
        mode: ShutdownMode,
        deadline: Instant,
    ) -> impl Future<Output = Result<ShutdownReport, ShutdownError>> + Send;
}

/// Bounded runtime event subscription. Must be consumed continuously.
pub trait RuntimeEvents: Send {
    /// Waits for the next event. `None` ends the subscription.
    fn recv(&mut self) -> impl Future<Output = Option<RuntimeEvent>> + Send;

    /// Non-blocking poll used to drain a per-turn budget.
    fn try_recv(&mut self) -> Result<RuntimeEvent, TryRecvError>;
}

impl RuntimePort for RuntimeHandle {
    fn submit(
        &self,
        spec: OperationSpec,
    ) -> impl Future<Output = Result<OperationAccepted, AdmissionError>> + Send {
        RuntimeHandle::submit(self, spec)
    }

    fn cancel(&self, operation_id: OperationId) -> impl Future<Output = CancelResult> + Send {
        RuntimeHandle::cancel(self, operation_id)
    }

    fn start_compaction(
        &self,
        spec: CompactionSpec,
    ) -> impl Future<Output = Result<MaintenanceAccepted, MaintenanceError>> + Send {
        RuntimeHandle::start_compaction(self, spec)
    }

    fn cancel_maintenance(&self, id: MaintenanceId) -> impl Future<Output = CancelResult> + Send {
        RuntimeHandle::cancel_maintenance(self, id)
    }

    fn snapshot(&self) -> impl Future<Output = RuntimeSnapshot> + Send {
        RuntimeHandle::snapshot(self)
    }

    fn shutdown(
        &self,
        mode: ShutdownMode,
        deadline: Instant,
    ) -> impl Future<Output = Result<ShutdownReport, ShutdownError>> + Send {
        RuntimeHandle::shutdown(self, mode, deadline)
    }
}

impl RuntimeEvents for RuntimeEventReceiver {
    fn recv(&mut self) -> impl Future<Output = Option<RuntimeEvent>> + Send {
        RuntimeEventReceiver::recv(self)
    }

    fn try_recv(&mut self) -> Result<RuntimeEvent, TryRecvError> {
        RuntimeEventReceiver::try_recv(self)
    }
}
