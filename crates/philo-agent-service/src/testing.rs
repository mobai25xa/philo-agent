//! Test doubles for the service crate. Not a production composition root.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use philo_agent_runtime::{
    AdmissionError, AgentAvailability, CancelResult, CompactionSpec, EpochEndReason,
    GenerationDisplay, GenerationId, MaintenanceAccepted, MaintenanceError, MaintenanceId,
    MaintenanceResult, ModelCallSnapshot, ModelError, ModelEventStream, ModelPort,
    OperationAccepted, OperationId, OperationSpec, OperationStatus, RuntimeConfig, RuntimeEpoch,
    RuntimeEvent, RuntimeFuture, RuntimeGeneration, RuntimeSnapshot, SessionId,
    SettlementDurability, SettlementRevision, ShutdownMode, ShutdownReport, ShutdownState,
    TryRecvError, TurnId,
};
use philo_session::{MemorySessionStore, SessionStore};
use philo_tools::{ToolPort, ToolRegistry};
use tokio::sync::{Notify, mpsc, watch};

use crate::FrontendClient;
use crate::bounds::RUNTIME_EVENT_CAP;
use crate::generation::{AssembleError, AssembleRequest, AssembledGeneration, GenerationAssembler};
use crate::runtime_api::{RuntimeEvents, RuntimePort};
use crate::service::{AgentService, ServiceDeps, StartOptions, start_inner};

/// Model port that never starts a stream.
#[derive(Debug, Default)]
pub struct UnavailableModel;

impl ModelPort for UnavailableModel {
    fn start<'a>(
        &'a self,
        _request: ModelCallSnapshot,
    ) -> RuntimeFuture<'a, Result<Box<dyn ModelEventStream>, ModelError>> {
        Box::pin(async {
            Err(ModelError::assembly(
                "model.assembly.request_build",
                "unavailable test model",
                "unavailable test model",
            ))
        })
    }
}

/// Empty tool registry as a `ToolPort`.
pub fn empty_tools() -> Arc<dyn ToolPort> {
    Arc::new(ToolRegistry::empty())
}

/// Builds a generation that cannot execute model calls.
pub fn test_generation(model_name: &str) -> Arc<RuntimeGeneration> {
    Arc::new(RuntimeGeneration {
        generation_id: GenerationId::new("generation-0"),
        model: Arc::new(UnavailableModel),
        tools: empty_tools(),
        runtime_config: RuntimeConfig::default(),
        display: GenerationDisplay {
            provider: None,
            model_name: model_name.to_owned(),
            model_id: model_name.to_owned(),
            image_input: true,
        },
    })
}

/// Releases runtime/assembler child calls previously parked by [`ChildHold`].
pub struct ChildHold {
    tx: watch::Sender<bool>,
    rx: watch::Receiver<bool>,
}

impl ChildHold {
    fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self { tx, rx }
    }

    /// Additional receiver for a second fake that must wait on the same gate.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.rx.clone()
    }

    /// Unblocks every waiter. Dropping without release also unblocks (channel closes).
    pub fn release(self) {
        let _ = self.tx.send(true);
    }
}

async fn wait_hold(rx: Option<watch::Receiver<bool>>) {
    let Some(mut rx) = rx else {
        return;
    };
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Assembler used by service tests. Can fail or delay selected names.
#[derive(Clone)]
pub struct FakeAssembler {
    fail_names: Vec<String>,
    delay_names: Vec<String>,
    delay: Duration,
    model: Arc<dyn ModelPort>,
    tools: Arc<dyn ToolPort>,
    config: RuntimeConfig,
    hold: Option<watch::Receiver<bool>>,
    started: Arc<AtomicU64>,
    started_notify: Arc<Notify>,
}

impl FakeAssembler {
    /// Succeeds immediately for every name.
    pub fn new() -> Self {
        Self {
            fail_names: Vec::new(),
            delay_names: Vec::new(),
            delay: Duration::from_millis(0),
            model: Arc::new(UnavailableModel),
            tools: empty_tools(),
            config: RuntimeConfig::default(),
            hold: None,
            started: Arc::new(AtomicU64::new(0)),
            started_notify: Arc::new(Notify::new()),
        }
    }

    /// Fails assembly for the given model names.
    pub fn failing(names: &[&str]) -> Self {
        let mut assembler = Self::new();
        assembler.fail_names = names.iter().map(|name| (*name).to_owned()).collect();
        assembler
    }

    /// Delays assembly for the given model names.
    pub fn with_delay(mut self, names: &[&str], delay: Duration) -> Self {
        self.delay_names = names.iter().map(|name| (*name).to_owned()).collect();
        self.delay = delay;
        self
    }

    /// Parks `assemble` until the matching [`ChildHold`] is released.
    pub fn with_hold(mut self, hold: watch::Receiver<bool>) -> Self {
        self.hold = Some(hold);
        self
    }

    /// How many assemble calls have entered the child task.
    pub fn started(&self) -> u64 {
        self.started.load(Ordering::SeqCst)
    }

    /// Waits until at least `count` assemble calls have started.
    pub async fn wait_started(&self, count: u64) {
        loop {
            let notified = self.started_notify.notified();
            if self.started() >= count {
                return;
            }
            notified.await;
        }
    }
}

impl Default for FakeAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl GenerationAssembler for FakeAssembler {
    fn assemble(
        &self,
        request: AssembleRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AssembledGeneration, AssembleError>> + Send + '_>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.started_notify.notify_waiters();
            wait_hold(self.hold.clone()).await;
            if self.delay_names.iter().any(|name| name == &request.name) {
                tokio::time::sleep(self.delay).await;
            }
            if self.fail_names.iter().any(|name| name == &request.name) {
                return Err(AssembleError::new(format!(
                    "failed to assemble model {}",
                    request.name
                )));
            }
            Ok(AssembledGeneration {
                model: self.model.clone(),
                tools: self.tools.clone(),
                runtime_config: self.config.clone(),
                model_name: request.name.clone(),
                provider: None,
                model_id: request.name,
                image_input: true,
            })
        })
    }
}

struct FakeInner {
    submitted: Vec<String>,
    submitted_sessions: Vec<String>,
    cancel_calls: Vec<OperationId>,
    cancel_maintenance_calls: Vec<MaintenanceId>,
    shutdown_calls: Vec<ShutdownMode>,
    panic_next_child: bool,
    submit_error: Option<AdmissionError>,
    next_op: u64,
    snapshot: RuntimeSnapshot,
    child_started: u64,
    child_hold: Option<watch::Receiver<bool>>,
}

/// Cloneable fake [`RuntimePort`].
#[derive(Clone)]
pub struct FakeRuntimeHandle {
    inner: Arc<Mutex<FakeInner>>,
    event_tx: mpsc::Sender<RuntimeEvent>,
    consumed: Arc<AtomicU64>,
    child_started_notify: Arc<Notify>,
}

/// Fake [`RuntimeEvents`] subscription.
pub struct FakeRuntimeSubscription {
    rx: mpsc::Receiver<RuntimeEvent>,
    /// Keeps the event channel open even if the test drops every handle.
    _keep_open: mpsc::Sender<RuntimeEvent>,
    consumed: Arc<AtomicU64>,
}

impl FakeRuntimeHandle {
    /// Creates a connected handle/subscription pair.
    pub fn pair() -> (Self, FakeRuntimeSubscription) {
        let (event_tx, rx) = mpsc::channel(RUNTIME_EVENT_CAP);
        let consumed = Arc::new(AtomicU64::new(0));
        let handle = Self {
            inner: Arc::new(Mutex::new(FakeInner {
                submitted: Vec::new(),
                submitted_sessions: Vec::new(),
                cancel_calls: Vec::new(),
                cancel_maintenance_calls: Vec::new(),
                shutdown_calls: Vec::new(),
                panic_next_child: false,
                submit_error: None,
                next_op: 1,
                snapshot: RuntimeSnapshot {
                    epoch: RuntimeEpoch::new("epoch-1"),
                    availability: AgentAvailability::Idle,
                    queued: Vec::new(),
                    active: None,
                    maintenance: None,
                    shutdown: ShutdownState::Running,
                    last_settled: Vec::new(),
                    runtime_revision: 0,
                },
                child_started: 0,
                child_hold: None,
            })),
            event_tx: event_tx.clone(),
            consumed: consumed.clone(),
            child_started_notify: Arc::new(Notify::new()),
        };
        (
            handle,
            FakeRuntimeSubscription {
                rx,
                _keep_open: event_tx,
                consumed,
            },
        )
    }

    /// Pushes a runtime event onto the subscription. Drops if the cap is full.
    ///
    /// Command replies never call this. Tests must emit lifecycle facts
    /// explicitly so reply and event stay independently controllable.
    pub fn emit(&self, event: RuntimeEvent) {
        let _ = self.event_tx.try_send(event);
    }

    /// Convenience for [`RuntimeEvent::Agent`].
    pub fn emit_agent(&self, event: philo_agent_runtime::AgentEvent) {
        self.emit(RuntimeEvent::Agent(event));
    }

    /// Publishes one admission fact. Does not complete a command reply.
    pub fn emit_operation_accepted(
        &self,
        operation_id: OperationId,
        session_id: SessionId,
        turn_id: TurnId,
    ) {
        self.emit(RuntimeEvent::OperationAccepted {
            operation_id,
            session_id,
            turn_id,
        });
    }

    /// Publishes one terminal fact. Does not complete a command reply.
    pub fn emit_operation_settled(
        &self,
        operation_id: OperationId,
        session_id: SessionId,
        status: OperationStatus,
        durability: SettlementDurability,
        session_revision: SettlementRevision,
    ) {
        self.emit(RuntimeEvent::OperationSettled {
            operation_id,
            session_id,
            status,
            durability,
            session_revision,
        });
    }

    /// Publishes one maintenance admission fact.
    pub fn emit_maintenance_accepted(&self, id: MaintenanceId, session_id: SessionId) {
        self.emit(RuntimeEvent::MaintenanceAccepted { id, session_id });
    }

    /// Publishes one maintenance terminal fact.
    pub fn emit_maintenance_settled(
        &self,
        id: MaintenanceId,
        session_id: SessionId,
        result: MaintenanceResult,
    ) {
        self.emit(RuntimeEvent::MaintenanceSettled {
            id,
            session_id,
            result,
        });
    }

    /// Publishes an epoch diagnostic. Never carries settlements.
    pub fn emit_epoch_ended(
        &self,
        epoch: RuntimeEpoch,
        reason: EpochEndReason,
        forced_count: usize,
    ) {
        self.emit(RuntimeEvent::EpochEnded {
            epoch,
            reason,
            forced_count,
        });
    }

    /// How many events the service has consumed from the subscription.
    pub fn consumed(&self) -> u64 {
        self.consumed.load(Ordering::SeqCst)
    }

    /// Recorded submit specs, in order.
    pub fn submitted(&self) -> usize {
        self.inner.lock().expect("fake runtime").submitted.len()
    }

    /// Last submitted generation id, if any.
    pub fn last_submitted_generation(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("fake runtime")
            .submitted
            .last()
            .cloned()
    }

    /// Last submitted session id, if any.
    pub fn last_submitted_session(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("fake runtime")
            .submitted_sessions
            .last()
            .cloned()
    }

    /// Recorded cancel calls.
    pub fn cancel_calls(&self) -> usize {
        self.inner.lock().expect("fake runtime").cancel_calls.len()
    }

    /// Forces the next submit to fail.
    pub fn fail_next_submit(&self, error: AdmissionError) {
        self.inner.lock().expect("fake runtime").submit_error = Some(error);
    }

    /// Parks subsequent runtime child calls until the returned hold is released.
    pub fn hold_children(&self) -> ChildHold {
        let hold = ChildHold::new();
        self.inner.lock().expect("fake runtime").child_hold = Some(hold.subscribe());
        hold
    }

    /// Replaces the snapshot returned by [`RuntimePort::snapshot`].
    pub fn set_snapshot(&self, snapshot: RuntimeSnapshot) {
        self.inner.lock().expect("fake runtime").snapshot = snapshot;
    }

    /// How many runtime child calls have entered before completing.
    pub fn child_started(&self) -> u64 {
        self.inner.lock().expect("fake runtime").child_started
    }

    /// Waits until at least `count` runtime child calls have started.
    pub async fn wait_child_started(&self, count: u64) {
        loop {
            let notified = self.child_started_notify.notified();
            if self.child_started() >= count {
                return;
            }
            notified.await;
        }
    }

    /// Recorded shutdown calls.
    pub fn shutdown_calls(&self) -> usize {
        self.inner
            .lock()
            .expect("fake runtime")
            .shutdown_calls
            .len()
    }

    /// Last recorded shutdown mode, if any.
    pub fn last_shutdown_mode(&self) -> Option<ShutdownMode> {
        self.inner
            .lock()
            .expect("fake runtime")
            .shutdown_calls
            .last()
            .copied()
    }

    /// Panics the next runtime child after it has entered the hold/park point.
    pub fn panic_next_child(&self) {
        self.inner.lock().expect("fake runtime").panic_next_child = true;
    }
}

async fn park_runtime_child(inner: &Arc<Mutex<FakeInner>>, notify: &Notify) {
    let hold = {
        let mut guard = inner.lock().expect("fake runtime");
        guard.child_started += 1;
        guard.child_hold.clone()
    };
    notify.notify_waiters();
    wait_hold(hold).await;
}

impl RuntimePort for FakeRuntimeHandle {
    /// Completes the command reply only. Lifecycle facts must be emitted separately.
    fn submit(
        &self,
        spec: OperationSpec,
    ) -> impl Future<Output = Result<OperationAccepted, AdmissionError>> + Send {
        let inner = self.inner.clone();
        let notify = self.child_started_notify.clone();
        async move {
            park_runtime_child(&inner, &notify).await;
            let mut inner = inner.lock().expect("fake runtime");
            if inner.panic_next_child {
                inner.panic_next_child = false;
                drop(inner);
                panic!("test child panic");
            }
            if let Some(error) = inner.submit_error.take() {
                return Err(error);
            }
            let index = inner.next_op;
            inner.next_op += 1;
            inner
                .submitted
                .push(spec.generation.generation_id.to_string());
            inner.submitted_sessions.push(spec.session_id.to_string());
            Ok(OperationAccepted {
                operation_id: OperationId::new(format!("op-{index}")),
                turn_id: TurnId::new(format!("turn-{index}")),
            })
        }
    }

    fn cancel(&self, operation_id: OperationId) -> impl Future<Output = CancelResult> + Send {
        let inner = self.inner.clone();
        let notify = self.child_started_notify.clone();
        async move {
            park_runtime_child(&inner, &notify).await;
            inner
                .lock()
                .expect("fake runtime")
                .cancel_calls
                .push(operation_id);
            CancelResult::Requested
        }
    }

    fn start_compaction(
        &self,
        _spec: CompactionSpec,
    ) -> impl Future<Output = Result<MaintenanceAccepted, MaintenanceError>> + Send {
        let inner = self.inner.clone();
        let notify = self.child_started_notify.clone();
        async move {
            park_runtime_child(&inner, &notify).await;
            Ok(MaintenanceAccepted {
                id: MaintenanceId::new("maint-1"),
            })
        }
    }

    fn cancel_maintenance(&self, id: MaintenanceId) -> impl Future<Output = CancelResult> + Send {
        let inner = self.inner.clone();
        let notify = self.child_started_notify.clone();
        async move {
            park_runtime_child(&inner, &notify).await;
            inner
                .lock()
                .expect("fake runtime")
                .cancel_maintenance_calls
                .push(id);
            CancelResult::Requested
        }
    }

    fn snapshot(&self) -> impl Future<Output = RuntimeSnapshot> + Send {
        let inner = self.inner.clone();
        async move { inner.lock().expect("fake runtime").snapshot.clone() }
    }

    fn shutdown(
        &self,
        mode: ShutdownMode,
        _deadline: Instant,
    ) -> impl Future<Output = Result<ShutdownReport, philo_agent_runtime::ShutdownError>> + Send
    {
        let inner = self.inner.clone();
        let notify = self.child_started_notify.clone();
        async move {
            park_runtime_child(&inner, &notify).await;
            let mut inner = inner.lock().expect("fake runtime");
            inner.shutdown_calls.push(mode);
            Ok(ShutdownReport {
                epoch: inner.snapshot.epoch.clone(),
                final_state: ShutdownState::Stopped,
                settlements: Vec::new(),
                diagnostics: Vec::new(),
            })
        }
    }
}

impl RuntimeEvents for FakeRuntimeSubscription {
    async fn recv(&mut self) -> Option<RuntimeEvent> {
        let event = self.rx.recv().await;
        if event.is_some() {
            self.consumed.fetch_add(1, Ordering::SeqCst);
        }
        event
    }

    fn try_recv(&mut self) -> Result<RuntimeEvent, TryRecvError> {
        match self.rx.try_recv() {
            Ok(event) => {
                self.consumed.fetch_add(1, Ordering::SeqCst);
                Ok(event)
            }
            Err(mpsc::error::TryRecvError::Empty) => Err(TryRecvError::Empty),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(TryRecvError::Closed),
        }
    }
}

/// Releases a paused command-lane poll once the test has filled that mailbox.
pub struct CommandLaneHold {
    tx: watch::Sender<bool>,
}

impl CommandLaneHold {
    /// Allows the actor to start polling the ordinary command lane.
    pub fn release(self) {
        let _ = self.tx.send(true);
    }
}

/// Starts a service against [`FakeRuntimeHandle`] and [`MemorySessionStore`].
pub fn start_test_service() -> (AgentService, FrontendClient, FakeRuntimeHandle) {
    start_test_service_with(FakeAssembler::new(), MemorySessionStore::new())
}

/// Starts a service with an explicit assembler and store.
pub fn start_test_service_with(
    assembler: FakeAssembler,
    sessions: impl SessionStore + 'static,
) -> (AgentService, FrontendClient, FakeRuntimeHandle) {
    start_test_service_with_generation(assembler, sessions, test_generation("base"))
}

/// Starts a service with an explicit initial generation (e.g. one whose
/// display declares no image input).
pub fn start_test_service_with_generation(
    assembler: FakeAssembler,
    sessions: impl SessionStore + 'static,
    initial_generation: Arc<RuntimeGeneration>,
) -> (AgentService, FrontendClient, FakeRuntimeHandle) {
    let (runtime, subscription) = FakeRuntimeHandle::pair();
    let handle = runtime.clone();
    let (service, client) = crate::start(ServiceDeps {
        runtime,
        subscription,
        sessions: Arc::new(sessions),
        assembler: Arc::new(assembler),
        initial_generation,
    });
    (service, client, handle)
}

/// Starts a service that does not poll the ordinary command lane until released.
pub fn start_test_service_with_command_hold() -> (
    AgentService,
    FrontendClient,
    FakeRuntimeHandle,
    CommandLaneHold,
) {
    let (hold_tx, hold_rx) = watch::channel(false);
    let (runtime, subscription) = FakeRuntimeHandle::pair();
    let handle = runtime.clone();
    let (service, client) = start_inner(
        ServiceDeps {
            runtime,
            subscription,
            sessions: Arc::new(MemorySessionStore::new()),
            assembler: Arc::new(FakeAssembler::new()),
            initial_generation: test_generation("base"),
        },
        StartOptions {
            command_hold: Some(hold_rx),
        },
    );
    (service, client, handle, CommandLaneHold { tx: hold_tx })
}

/// Aborts the service actor and waits until its task has actually stopped.
pub async fn abort_service_actor_and_wait(service: &AgentService) {
    service.abort_actor();
    service.wait_stopped().await;
}
