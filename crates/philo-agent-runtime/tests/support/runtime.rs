//! Shared helpers for driving the self-driven runtime in tests.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use philo_agent_runtime::{
    AgentAvailability, AgentEvent, AgentFailure, CancelResult, ChannelBounds,
    CompactionError, CompactionReport, CompactionSpec, GenerationDisplay, GenerationId, IdSource,
    MaintenanceAccepted, MaintenanceError, MaintenanceId, MaintenanceResult, OperationAccepted,
    OperationId, OperationOutcome, OperationPhase, OperationSpec, OperationStatus, RuntimeConfig,
    RuntimeDeps, RuntimeEvent, RuntimeEventReceiver, RuntimeGeneration, RuntimeHandle,
    RuntimeSnapshot, SequentialIdSource, SessionId, SettlementDurability, ShutdownMode,
    ShutdownState, ToolCallId, ToolPort, ToolRegistry, UserMessage,
};
use philo_session::SessionStore;
use tokio::sync::{Mutex, Notify};

/// Polling budget for snapshot/phase waits. Long enough for a loaded
/// Windows `cargo test --workspace` without turning tests into sleep-luck.
const WAIT_DEADLINE: Duration = Duration::from_secs(20);

pub fn empty_tools() -> Arc<dyn ToolPort> {
    Arc::new(ToolRegistry::empty())
}

pub fn generation(
    model: Arc<dyn philo_agent_runtime::ModelPort>,
    tools: Arc<dyn ToolPort>,
    config: RuntimeConfig,
) -> Arc<RuntimeGeneration> {
    let model_name = config.model_target.clone();
    Arc::new(RuntimeGeneration {
        generation_id: GenerationId::new("test-generation"),
        model,
        tools,
        runtime_config: config,
        display: GenerationDisplay { model_name },
    })
}

pub async fn start(
    _model: Arc<dyn philo_agent_runtime::ModelPort>,
    sessions: Arc<dyn SessionStore>,
    _tools: Arc<dyn ToolPort>,
    _config: RuntimeConfig,
) -> (RuntimeHandle, RuntimeEventReceiver) {
    start_with_ids(sessions, Arc::new(SequentialIdSource::new())).await
}

pub async fn start_with_ids(
    sessions: Arc<dyn SessionStore>,
    ids: Arc<dyn IdSource>,
) -> (RuntimeHandle, RuntimeEventReceiver) {
    start_with_bounds(sessions, ids, ChannelBounds::default()).await
}

pub async fn start_with_bounds(
    sessions: Arc<dyn SessionStore>,
    ids: Arc<dyn IdSource>,
    bounds: ChannelBounds,
) -> (RuntimeHandle, RuntimeEventReceiver) {
    let parts = philo_agent_runtime::AgentRuntime::start(RuntimeDeps {
        sessions,
        ids,
        bounds,
    })
    .expect("start runtime");
    (parts.handle, parts.events)
}

pub fn event_cap_bounds(event_cap: usize) -> ChannelBounds {
    ChannelBounds {
        command_cap: 32,
        control_cap: 16,
        event_cap,
        queue_max: 32,
        driver_event_budget: 32,
        reliable_staging_cap: 64,
    }
}

pub fn tiny_pipeline_bounds() -> ChannelBounds {
    ChannelBounds {
        command_cap: 4,
        control_cap: 8,
        event_cap: 1,
        queue_max: 2,
        driver_event_budget: 8,
        reliable_staging_cap: 4,
    }
}

/// Continuous drain of the one-shot event receiver. Pause stops `recv` so
/// backpressure can be observed without dropping the outlet.
pub struct EventProbe {
    events: Arc<Mutex<VecDeque<RuntimeEvent>>>,
    paused: Arc<AtomicBool>,
    wake: Arc<Notify>,
    _join: tokio::task::JoinHandle<()>,
}

impl EventProbe {
    pub fn start(sub: RuntimeEventReceiver) -> Self {
        Self::start_with(sub, 256, false)
    }

    pub fn start_paused(sub: RuntimeEventReceiver) -> Self {
        Self::start_with(sub, 256, true)
    }

    fn start_with(mut sub: RuntimeEventReceiver, cap: usize, paused: bool) -> Self {
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let paused_flag = Arc::new(AtomicBool::new(paused));
        let wake = Arc::new(Notify::new());
        let join = {
            let events = events.clone();
            let paused_flag = paused_flag.clone();
            let wake = wake.clone();
            tokio::spawn(async move {
                loop {
                    while paused_flag.load(Ordering::SeqCst) {
                        wake.notified().await;
                    }
                    match sub.recv().await {
                        Some(event) => {
                            let mut held = events.lock().await;
                            if held.len() >= cap {
                                held.pop_front();
                            }
                            held.push_back(event);
                        }
                        None => break,
                    }
                }
            })
        };
        Self {
            events,
            paused: paused_flag,
            wake,
            _join: join,
        }
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    pub async fn snapshot(&self) -> Vec<RuntimeEvent> {
        self.events.lock().await.iter().cloned().collect()
    }

    pub async fn wait_for(
        &self,
        predicate: impl Fn(&[RuntimeEvent]) -> bool,
        timeout: Duration,
    ) -> Vec<RuntimeEvent> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let events = self.snapshot().await;
            if predicate(&events) {
                return events;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("event probe timed out; held {} events", events.len());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

pub async fn wait_until_shutdown_leaves_running(handle: &RuntimeHandle) {
    let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
    loop {
        let snapshot = handle.snapshot().await;
        if snapshot.shutdown != ShutdownState::Running {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for runtime to leave Running");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub async fn submit_prompt(
    handle: &RuntimeHandle,
    generation: Arc<RuntimeGeneration>,
    session: impl Into<String>,
    text: impl Into<String>,
) -> OperationAccepted {
    handle
        .submit(OperationSpec {
            session_id: SessionId::new(session.into()),
            user_message: UserMessage::new(text.into()),
            generation,
            service_request_id: None,
        })
        .await
        .expect("submit")
}

pub async fn submit_compaction(
    handle: &RuntimeHandle,
    generation: Arc<RuntimeGeneration>,
    session: impl Into<String>,
) -> MaintenanceAccepted {
    handle
        .start_compaction(CompactionSpec {
            session_id: SessionId::new(session.into()),
            generation,
        })
        .await
        .expect("start compaction")
}

/// Cursor that can drain one operation without dropping sibling events.
pub struct EventCursor {
    sub: RuntimeEventReceiver,
    held: VecDeque<RuntimeEvent>,
}

impl EventCursor {
    pub fn new(sub: RuntimeEventReceiver) -> Self {
        Self {
            sub,
            held: VecDeque::new(),
        }
    }

    pub async fn recv(&mut self) -> Option<RuntimeEvent> {
        if let Some(event) = self.held.pop_front() {
            return Some(event);
        }
        self.sub.recv().await
    }

    async fn next_incoming(
        &mut self,
        pending: &mut VecDeque<RuntimeEvent>,
    ) -> Option<RuntimeEvent> {
        if let Some(event) = pending.pop_front() {
            return Some(event);
        }
        self.sub.recv().await
    }

    /// Pull the next AgentEvent for `operation_id` without waiting for settlement.
    pub async fn next_for_operation(
        &mut self,
        operation_id: &OperationId,
        started: &mut bool,
    ) -> Option<AgentEvent> {
        let mut pending = std::mem::take(&mut self.held);
        loop {
            let Some(event) = self.next_incoming(&mut pending).await else {
                self.held.extend(pending);
                return None;
            };
            match event {
                event @ (RuntimeEvent::Agent(_) | RuntimeEvent::OperationSettled { .. }) => {
                    let agent = into_agent_event(event).expect("agent-shaped event");
                    if !belongs(&agent, operation_id, *started) {
                        self.held.push_back(RuntimeEvent::Agent(agent));
                        continue;
                    }
                    if matches!(&agent, AgentEvent::OperationStarted { .. }) {
                        *started = true;
                    }
                    self.held.extend(pending);
                    return Some(agent);
                }
                other => self.held.push_back(other),
            }
        }
    }

    pub async fn drain_until_settled(
        &mut self,
        operation_id: &OperationId,
    ) -> (Vec<AgentEvent>, OperationOutcome) {
        let mut events = Vec::new();
        let mut started = false;
        let mut assistant = None;
        let mut failure = None;
        let mut pending = std::mem::take(&mut self.held);
        loop {
            let Some(event) = self.next_incoming(&mut pending).await else {
                panic!("subscription closed before {operation_id} settled");
            };
            let agent = match event {
                event @ (RuntimeEvent::Agent(_) | RuntimeEvent::OperationSettled { .. }) => {
                    into_agent_event(event).expect("agent-shaped event")
                }
                other => {
                    self.held.push_back(other);
                    continue;
                }
            };
            if !belongs(&agent, operation_id, started) {
                self.held.push_back(RuntimeEvent::Agent(agent));
                continue;
            }
            if matches!(&agent, AgentEvent::OperationStarted { .. }) {
                started = true;
            }
            match &agent {
                AgentEvent::AssistantMessageCompleted { message, .. } => {
                    assistant = Some(message.clone());
                }
                AgentEvent::TurnFailed { failure: next, .. } => {
                    failure = Some(next.clone());
                }
                _ => {}
            }
            let settled = match &agent {
                AgentEvent::OperationSettled {
                    operation_id: id,
                    status,
                    durability,
                    ..
                } if id == operation_id => Some((*status, *durability)),
                _ => None,
            };
            events.push(agent);
            if let Some((status, durability)) = settled {
                self.held.extend(pending);
                return (events, outcome_from(status, durability, assistant, failure));
            }
        }
    }
}

/// Collects AgentEvents for `operation_id` until it settles. Non-agent
/// runtime events are skipped. Sibling operation events are also skipped,
/// so multi-operation tests should use [`EventCursor`].
pub async fn drain_until_settled(
    sub: &mut RuntimeEventReceiver,
    operation_id: &OperationId,
) -> (Vec<AgentEvent>, OperationOutcome) {
    let mut events = Vec::new();
    let mut started = false;
    let mut assistant = None;
    let mut failure = None;
    loop {
        let Some(event) = sub.recv().await else {
            panic!("subscription closed before {operation_id} settled");
        };
        let Some(agent) = into_agent_event(event) else {
            continue;
        };
        if !belongs(&agent, operation_id, started) {
            continue;
        }
        if matches!(&agent, AgentEvent::OperationStarted { .. }) {
            started = true;
        }
        match &agent {
            AgentEvent::AssistantMessageCompleted { message, .. } => {
                assistant = Some(message.clone());
            }
            AgentEvent::TurnFailed { failure: next, .. } => {
                failure = Some(next.clone());
            }
            _ => {}
        }
        let settled = match &agent {
            AgentEvent::OperationSettled {
                operation_id: id,
                status,
                durability,
                ..
            } if id == operation_id => Some((*status, *durability)),
            _ => None,
        };
        events.push(agent);
        if let Some((status, durability)) = settled {
            return (events, outcome_from(status, durability, assistant, failure));
        }
    }
}

fn into_agent_event(event: RuntimeEvent) -> Option<AgentEvent> {
    match event {
        RuntimeEvent::Agent(agent) => Some(agent),
        RuntimeEvent::OperationSettled {
            operation_id,
            status,
            durability,
            session_revision,
            ..
        } => Some(AgentEvent::OperationSettled {
            operation_id,
            status,
            durability,
            session_revision,
        }),
        _ => None,
    }
}

fn belongs(event: &AgentEvent, operation_id: &OperationId, started: bool) -> bool {
    match event {
        AgentEvent::OperationQueued { operation_id: id }
        | AgentEvent::OperationStarted { operation_id: id }
        | AgentEvent::CancellationRequested {
            operation_id: id, ..
        }
        | AgentEvent::OperationSettled {
            operation_id: id, ..
        } => id == operation_id,
        _ => started,
    }
}

fn outcome_from(
    status: OperationStatus,
    durability: SettlementDurability,
    assistant: Option<philo_agent_runtime::AssistantMessage>,
    failure: Option<AgentFailure>,
) -> OperationOutcome {
    use philo_agent_runtime::{FailureDomain, FailureStage, RetryDisposition};
    match status {
        OperationStatus::Succeeded => OperationOutcome::Succeeded {
            assistant: assistant.expect("succeeded operation published assistant message"),
        },
        OperationStatus::Cancelled => OperationOutcome::Cancelled,
        OperationStatus::Failed => OperationOutcome::Failed {
            failure: failure.unwrap_or_else(|| {
                AgentFailure::new(
                    "engine.invariant_violation",
                    FailureDomain::Internal,
                    FailureStage::TurnEngine,
                    RetryDisposition::Never,
                    "an internal driver invariant was violated",
                    match durability {
                        SettlementDurability::Unconfirmed => "unconfirmed failure",
                        SettlementDurability::Confirmed => "failed without TurnFailed",
                    },
                )
            }),
            durability,
        },
    }
}

async fn enrich_outcome(
    handle: &RuntimeHandle,
    operation_id: &OperationId,
    outcome: OperationOutcome,
) -> OperationOutcome {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let snapshot = handle.snapshot().await;
        if let Some(settled) = snapshot
            .last_settled
            .iter()
            .find(|settled| settled.operation_id == *operation_id)
        {
            return apply_snapshot_failure(outcome, settled.failure.clone());
        }
        if tokio::time::Instant::now() >= deadline {
            return outcome;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn apply_snapshot_failure(
    outcome: OperationOutcome,
    snapshot_failure: Option<AgentFailure>,
) -> OperationOutcome {
    match (outcome, snapshot_failure) {
        (
            OperationOutcome::Failed {
                durability,
                failure,
            },
            Some(real),
        ) if failure.code() == "engine.invariant_violation" => OperationOutcome::Failed {
            failure: real,
            durability,
        },
        (outcome, _) => outcome,
    }
}

#[allow(dead_code)]
pub async fn wait_until_compacting(handle: &RuntimeHandle, session: &SessionId) {
    let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
    loop {
        let snapshot = handle.snapshot().await;
        if matches!(
            snapshot.availability,
            AgentAvailability::Compacting { session_id: ref id } if id == session
        ) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for compaction of {session}");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[allow(dead_code)]
pub fn snapshot_of(
    handle: &RuntimeHandle,
) -> impl std::future::Future<Output = RuntimeSnapshot> + '_ {
    handle.snapshot()
}

pub async fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
    while !ready() {
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for test condition");
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

pub async fn wait_until_busy(handle: &RuntimeHandle, operation_id: &OperationId) {
    let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
    loop {
        let snapshot = handle.snapshot().await;
        if matches!(
            snapshot.availability,
            AgentAvailability::Busy { operation_id: ref id } if id == operation_id
        ) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for busy {operation_id}");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

pub async fn wait_until_queued(handle: &RuntimeHandle, operation_id: &OperationId) {
    let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
    loop {
        let snapshot = handle.snapshot().await;
        if snapshot
            .queued
            .iter()
            .any(|queued| queued.operation_id == *operation_id)
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for queued {operation_id}");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

pub async fn wait_until_idle(handle: &RuntimeHandle) {
    let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
    loop {
        let snapshot = handle.snapshot().await;
        if snapshot.availability == AgentAvailability::Idle
            && snapshot.queued.is_empty()
            && snapshot.maintenance.is_none()
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for idle runtime");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

pub async fn wait_until_phase(
    handle: &RuntimeHandle,
    operation_id: &OperationId,
    mut pred: impl FnMut(&philo_agent_runtime::OperationPhase) -> bool,
) {
    let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
    loop {
        let snapshot = handle.snapshot().await;
        if let Some(active) = &snapshot.active
            && active.operation_id == *operation_id
            && pred(&active.phase)
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for operation phase");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

pub struct Harness {
    pub handle: RuntimeHandle,
    pub sub: EventCursor,
    pub generation: Arc<RuntimeGeneration>,
}

impl Harness {
    pub async fn launch(
        model: Arc<dyn philo_agent_runtime::ModelPort>,
        sessions: Arc<dyn SessionStore>,
        tools: Arc<dyn ToolPort>,
        config: RuntimeConfig,
        ids: Arc<dyn IdSource>,
    ) -> Self {
        let generation = generation(model, tools, config);
        let (handle, sub) = start_with_ids(sessions, ids).await;
        Self {
            handle,
            sub: EventCursor::new(sub),
            generation,
        }
    }

    pub async fn launch_default(
        model: Arc<dyn philo_agent_runtime::ModelPort>,
        sessions: Arc<dyn SessionStore>,
        tools: Arc<dyn ToolPort>,
        config: RuntimeConfig,
    ) -> Self {
        Self::launch(
            model,
            sessions,
            tools,
            config,
            Arc::new(SequentialIdSource::new()),
        )
        .await
    }

    pub async fn submit(
        &self,
        session: impl Into<String>,
        text: impl Into<String>,
    ) -> OperationAccepted {
        submit_prompt(&self.handle, self.generation.clone(), session, text).await
    }

    pub async fn drain(
        &mut self,
        operation_id: &OperationId,
    ) -> (Vec<AgentEvent>, OperationOutcome) {
        let (events, outcome) = self.sub.drain_until_settled(operation_id).await;
        (
            events,
            enrich_outcome(&self.handle, operation_id, outcome).await,
        )
    }

    pub async fn submit_message(
        &self,
        session: impl Into<String>,
        message: UserMessage,
    ) -> OperationAccepted {
        self.handle
            .submit(OperationSpec {
                session_id: SessionId::new(session.into()),
                user_message: message,
                generation: self.generation.clone(),
                service_request_id: None,
            })
            .await
            .expect("submit")
    }

    pub async fn run(
        &mut self,
        session: impl Into<String>,
        text: impl Into<String>,
    ) -> (Vec<AgentEvent>, OperationOutcome) {
        let accepted = self.submit(session, text).await;
        self.drain(&accepted.operation_id).await
    }

    pub async fn compact(
        &mut self,
        session: impl Into<String>,
    ) -> Result<CompactionReport, CompactionError> {
        let accepted = match self
            .handle
            .start_compaction(CompactionSpec {
                session_id: SessionId::new(session.into()),
                generation: self.generation.clone(),
            })
            .await
        {
            Ok(accepted) => accepted,
            Err(MaintenanceError::Unavailable { availability }) => {
                return Err(CompactionError::Unavailable { availability });
            }
            Err(error) => {
                return Err(CompactionError::Session {
                    message: error.message().to_owned(),
                });
            }
        };
        match drain_maintenance_from_cursor(&mut self.sub, &accepted.id).await {
            MaintenanceResult::Compacted(report) => Ok(report),
            MaintenanceResult::Failed(error) => Err(error),
            MaintenanceResult::Cancelled => Err(CompactionError::Session {
                message: "compaction cancelled".to_owned(),
            }),
            MaintenanceResult::Panicked { .. } => Err(CompactionError::Session {
                message: "compaction panicked".to_owned(),
            }),
        }
    }

    pub async fn start_compaction(&self, session: impl Into<String>) -> MaintenanceAccepted {
        submit_compaction(&self.handle, self.generation.clone(), session).await
    }

    pub async fn drain_maintenance(&mut self, id: &MaintenanceId) -> MaintenanceResult {
        drain_maintenance_from_cursor(&mut self.sub, id).await
    }

    pub async fn run_message(
        &mut self,
        session: impl Into<String>,
        message: UserMessage,
    ) -> (Vec<AgentEvent>, OperationOutcome) {
        let accepted = self.submit_message(session, message).await;
        self.drain(&accepted.operation_id).await
    }
}

pub async fn drain_maintenance(
    sub: &mut RuntimeEventReceiver,
    id: &MaintenanceId,
) -> MaintenanceResult {
    loop {
        let Some(event) = sub.recv().await else {
            panic!("subscription closed before maintenance {id:?} settled");
        };
        match event {
            RuntimeEvent::MaintenanceSettled {
                id: settled,
                result,
                ..
            } if settled == *id => {
                return result;
            }
            _ => {}
        }
    }
}

async fn drain_maintenance_from_cursor(
    cursor: &mut EventCursor,
    id: &MaintenanceId,
) -> MaintenanceResult {
    let mut pending = std::mem::take(&mut cursor.held);
    loop {
        let Some(event) = cursor.next_incoming(&mut pending).await else {
            panic!("subscription closed before maintenance {id} settled");
        };
        match event {
            RuntimeEvent::MaintenanceSettled {
                id: settled,
                result,
                ..
            } if settled == *id => {
                cursor.held.extend(pending);
                return result;
            }
            other => cursor.held.push_back(other),
        }
    }
}

/// Test-facing runtime used while migrating integration tests off
/// `OperationHandle`. Commands go through [`RuntimeHandle`]; events through
/// a shared [`EventCursor`].
pub struct TestRuntime {
    pub handle: RuntimeHandle,
    cursor: Arc<Mutex<EventCursor>>,
    pub generation: Arc<RuntimeGeneration>,
}

impl TestRuntime {
    pub async fn with_tools(
        model: Arc<dyn philo_agent_runtime::ModelPort>,
        sessions: Arc<dyn SessionStore>,
        ids: Arc<dyn IdSource>,
        config: RuntimeConfig,
        tools: Arc<dyn ToolPort>,
    ) -> Self {
        let generation = generation(model, tools, config);
        let (handle, sub) = start_with_ids(sessions, ids).await;
        Self {
            handle,
            cursor: Arc::new(Mutex::new(EventCursor::new(sub))),
            generation,
        }
    }

    pub async fn new(
        model: Arc<dyn philo_agent_runtime::ModelPort>,
        sessions: Arc<dyn SessionStore>,
        ids: Arc<dyn IdSource>,
        config: RuntimeConfig,
    ) -> Self {
        Self::with_tools(model, sessions, ids, config, empty_tools()).await
    }

    pub async fn prompt(&self, session_id: SessionId, user_message: UserMessage) -> TestOp {
        let accepted = self
            .handle
            .submit(OperationSpec {
                session_id,
                user_message,
                generation: self.generation.clone(),
                service_request_id: None,
            })
            .await
            .expect("submit");
        TestOp {
            operation_id: accepted.operation_id,
            handle: self.handle.clone(),
            cursor: self.cursor.clone(),
            state: Mutex::new(TestOpState::default()),
        }
    }

    pub async fn compact(
        &self,
        session_id: SessionId,
    ) -> Result<CompactionReport, CompactionError> {
        let accepted = match self
            .handle
            .start_compaction(CompactionSpec {
                session_id,
                generation: self.generation.clone(),
            })
            .await
        {
            Ok(accepted) => accepted,
            Err(MaintenanceError::Unavailable { availability }) => {
                return Err(CompactionError::Unavailable { availability });
            }
            Err(error) => {
                return Err(CompactionError::Session {
                    message: error.message().to_owned(),
                });
            }
        };
        let mut cursor = self.cursor.lock().await;
        match drain_maintenance_from_cursor(&mut cursor, &accepted.id).await {
            MaintenanceResult::Compacted(report) => Ok(report),
            MaintenanceResult::Failed(error) => Err(error),
            MaintenanceResult::Cancelled => Err(CompactionError::Session {
                message: "compaction cancelled".to_owned(),
            }),
            MaintenanceResult::Panicked { .. } => Err(CompactionError::Session {
                message: "compaction panicked".to_owned(),
            }),
        }
    }

    pub async fn availability(&self) -> AgentAvailability {
        self.handle.snapshot().await.availability
    }

    pub async fn start_compaction(
        &self,
        session_id: SessionId,
    ) -> Result<MaintenanceAccepted, MaintenanceError> {
        self.handle
            .start_compaction(CompactionSpec {
                session_id,
                generation: self.generation.clone(),
            })
            .await
    }

    pub async fn cancel_maintenance(&self, id: MaintenanceId) -> CancelResult {
        self.handle.cancel_maintenance(id).await
    }

    /// Stop the coordinator and drop this handle so JSONL session locks can
    /// be released before another store instance reopens the same root.
    pub async fn stop(self) {
        let _ = self
            .handle
            .shutdown(
                ShutdownMode::Forced,
                std::time::Instant::now() + Duration::from_secs(30),
            )
            .await;
        drop(self);
        tokio::task::yield_now().await;
    }
}

#[derive(Default)]
struct TestOpState {
    events: Vec<AgentEvent>,
    consumed: usize,
    started: bool,
    assistant: Option<philo_agent_runtime::AssistantMessage>,
    failure: Option<AgentFailure>,
    outcome: Option<OperationOutcome>,
}

/// One admitted operation.
///
/// `next_event` pulls this operation's `AgentEvent`s incrementally from the
/// shared cursor (live, while the op is still running). `wait` drains until
/// `OperationSettled`. After settle, `next_event` yields any remaining
/// buffered events and then `None`.
pub struct TestOp {
    pub operation_id: OperationId,
    handle: RuntimeHandle,
    cursor: Arc<Mutex<EventCursor>>,
    state: Mutex<TestOpState>,
}

impl TestOp {
    fn apply_event(state: &mut TestOpState, event: &AgentEvent) {
        match event {
            AgentEvent::AssistantMessageCompleted { message, .. } => {
                state.assistant = Some(message.clone());
            }
            AgentEvent::TurnFailed { failure: next, .. } => {
                state.failure = Some(next.clone());
            }
            AgentEvent::OperationSettled {
                status, durability, ..
            } => {
                state.outcome = Some(outcome_from(
                    *status,
                    *durability,
                    state.assistant.clone(),
                    state.failure.clone(),
                ));
            }
            _ => {}
        }
        state.events.push(event.clone());
    }

    async fn pull_next(&self) -> Option<AgentEvent> {
        let mut cursor = self.cursor.lock().await;
        let mut started = {
            let state = self.state.lock().await;
            if state.outcome.is_some() {
                return None;
            }
            state.started
        };
        let event = cursor
            .next_for_operation(&self.operation_id, &mut started)
            .await;
        let mut state = self.state.lock().await;
        state.started = started;
        if let Some(event) = event {
            Self::apply_event(&mut state, &event);
            Some(event)
        } else {
            None
        }
    }

    async fn wait_until_snapshot_settled(&self) {
        let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
        loop {
            let snapshot = self.handle.snapshot().await;
            if snapshot
                .last_settled
                .iter()
                .any(|settled| settled.operation_id == self.operation_id)
            {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for {} to settle", self.operation_id);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    pub async fn wait(&self) -> OperationOutcome {
        {
            let state = self.state.lock().await;
            if let Some(outcome) = state.outcome.clone() {
                return enrich_outcome(&self.handle, &self.operation_id, outcome).await;
            }
        }
        // Do not drain the subscription while the op is running: mergeable
        // events (progress, deltas) coalesce only while they stay unread.
        self.wait_until_snapshot_settled().await;
        loop {
            {
                let state = self.state.lock().await;
                if state.outcome.is_some() {
                    break;
                }
            }
            if self.pull_next().await.is_none() {
                panic!("subscription closed before {} settled", self.operation_id);
            }
        }
        let mut state = self.state.lock().await;
        let consumed = state.consumed;
        coalesce_unconsumed_progress(&mut state.events, consumed);
        let outcome = state
            .outcome
            .clone()
            .expect("settled operation must have an outcome");
        drop(state);
        enrich_outcome(&self.handle, &self.operation_id, outcome).await
    }

    pub async fn next_event(&self) -> Option<AgentEvent> {
        {
            let mut state = self.state.lock().await;
            if state.consumed < state.events.len() {
                let event = state.events[state.consumed].clone();
                state.consumed += 1;
                return Some(event);
            }
            if state.outcome.is_some() {
                return None;
            }
        }
        self.pull_next().await?;
        let mut state = self.state.lock().await;
        if state.consumed < state.events.len() {
            let event = state.events[state.consumed].clone();
            state.consumed += 1;
            return Some(event);
        }
        None
    }

    pub async fn recorded_events(&self) -> Vec<AgentEvent> {
        self.state.lock().await.events.clone()
    }

    pub async fn cancel(&self) -> CancelResult {
        self.handle.cancel(self.operation_id.clone()).await
    }

    /// Stop the shared coordinator so a JSONL root can be reopened.
    pub async fn stop_runtime(self) {
        let _ = self
            .handle
            .shutdown(
                ShutdownMode::Forced,
                std::time::Instant::now() + Duration::from_secs(30),
            )
            .await;
        drop(self);
        tokio::task::yield_now().await;
    }

    pub async fn wait_until_busy(&self) {
        wait_until_busy(&self.handle, &self.operation_id).await;
    }

    pub async fn wait_until_phase(&self, mut pred: impl FnMut(&OperationPhase) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT_DEADLINE;
        loop {
            let phase = self.phase().await;
            if pred(&phase) {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "timed out waiting for operation phase (last={phase:?}, id={})",
                    self.operation_id
                );
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    pub async fn phase(&self) -> OperationPhase {
        let snapshot = self.handle.publish_snapshot().await;
        if snapshot
            .queued
            .iter()
            .any(|queued| queued.operation_id == self.operation_id)
        {
            return OperationPhase::Queued;
        }
        if let Some(active) = &snapshot.active
            && active.operation_id == self.operation_id
        {
            return active.phase.clone();
        }
        if let Some(settled) = snapshot
            .last_settled
            .iter()
            .find(|settled| settled.operation_id == self.operation_id)
        {
            return OperationPhase::Settled(settled.status);
        }
        OperationPhase::PreparingTurn
    }
}

fn coalesce_unconsumed_progress(events: &mut Vec<AgentEvent>, from: usize) {
    let mut last_index = std::collections::HashMap::<ToolCallId, usize>::new();
    let mut keep = vec![true; events.len()];
    for (index, event) in events.iter().enumerate().skip(from) {
        if let AgentEvent::ToolExecutionProgress { tool_call_id, .. } = event
            && let Some(previous) = last_index.insert(tool_call_id.clone(), index)
        {
            keep[previous] = false;
        }
    }
    let mut kept = Vec::with_capacity(events.len());
    for (index, event) in std::mem::take(events).into_iter().enumerate() {
        if index < from || keep[index] {
            kept.push(event);
        }
    }
    *events = kept;
}

pub async fn collect_events(handle: &TestOp) -> Vec<AgentEvent> {
    handle.wait().await;
    handle.recorded_events().await
}
