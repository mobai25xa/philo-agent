use crate::AgentEvent;
use philo_session::CancelReason;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}
string_id!(SessionId);
string_id!(OperationId);
string_id!(TurnId);
string_id!(ModelCallId);
string_id!(ToolBatchId);
string_id!(ToolCallId);

/// One part of a multi-part user message. The runtime never interprets or
/// modifies image bytes; they map byte-for-byte down the explicit chain
/// (runtime -> kernel -> session) and into model calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserPart {
    Text(String),
    Image { media_type: String, bytes: Vec<u8> },
}

/// Why constructing a [`UserMessage`] was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidUserMessage {
    EmptyParts,
    EmptyTextPart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserMessage {
    parts: Vec<UserPart>,
}
impl UserMessage {
    /// Plain-text convenience constructor.
    ///
    /// # Panics
    ///
    /// Panics when `text` is empty; use [`UserMessage::from_parts`] for
    /// fallible construction.
    pub fn new(text: impl Into<String>) -> Self {
        Self::from_parts(vec![UserPart::Text(text.into())])
            .expect("plain-text user message must not be empty")
    }
    /// Multi-part constructor: parts must be non-empty and text parts must
    /// not be empty strings. Image-only messages are valid.
    pub fn from_parts(parts: Vec<UserPart>) -> Result<Self, InvalidUserMessage> {
        if parts.is_empty() {
            return Err(InvalidUserMessage::EmptyParts);
        }
        for part in &parts {
            if matches!(part, UserPart::Text(text) if text.is_empty()) {
                return Err(InvalidUserMessage::EmptyTextPart);
            }
        }
        Ok(Self { parts })
    }
    pub fn parts(&self) -> &[UserPart] {
        &self.parts
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistantMessage {
    pub(crate) content: String,
}
impl AssistantMessage {
    pub fn content(&self) -> &str {
        &self.content
    }
}

pub trait IdSource: Send + Sync {
    fn next_operation_id(&self) -> OperationId;
    fn next_turn_id(&self) -> TurnId;
}
#[derive(Debug, Default)]
pub struct SequentialIdSource {
    next_operation: AtomicU64,
    next_turn: AtomicU64,
}
impl SequentialIdSource {
    pub fn new() -> Self {
        Self::default()
    }
}
impl IdSource for SequentialIdSource {
    fn next_operation_id(&self) -> OperationId {
        OperationId::new(format!(
            "operation-{}",
            self.next_operation.fetch_add(1, Ordering::Relaxed) + 1
        ))
    }
    fn next_turn_id(&self) -> TurnId {
        TurnId::new(format!(
            "turn-{}",
            self.next_turn.fetch_add(1, Ordering::Relaxed) + 1
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelCallPhase {
    Starting,
    WaitingForFirstOutput,
    Streaming,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunningToolBatchPhase {
    Preparing,
    Executing { index: usize },
    CommittingResults,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationPhase {
    /// Waiting in the FIFO follow-up queue; no turn exists yet.
    Queued,
    PreparingTurn,
    RunningModelCall(ModelCallPhase),
    RunningToolBatch(RunningToolBatchPhase),
    Finalizing,
    Settled(OperationStatus),
}

/// Lets a by-value phase compare against `&OperationPhase` expectations.
impl PartialEq<&OperationPhase> for OperationPhase {
    fn eq(&self, other: &&OperationPhase) -> bool {
        self == *other
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationStatus {
    Succeeded,
    Failed,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementDurability {
    Confirmed,
    Unconfirmed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentFailureKind {
    ModelCall,
    InvalidModelOutput,
    ToolExecution,
    Persistence,
    RuntimeDriver,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFailure {
    kind: AgentFailureKind,
    message: String,
}
impl AgentFailure {
    pub fn new(kind: AgentFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn kind(&self) -> AgentFailureKind {
        self.kind
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationOutcome {
    Succeeded {
        assistant: AssistantMessage,
    },
    Failed {
        failure: AgentFailure,
        durability: SettlementDurability,
    },
    /// The operation ended by user request; a normal terminal outcome.
    Cancelled,
}

/// Read-only observation of whether the runtime is driving an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentAvailability {
    Idle,
    Busy { operation_id: OperationId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentError {
    message: String,
}
impl AgentError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// The lazily driven remainder of one admitted operation.
pub(crate) type Engine = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Single-active-operation scheduler with a FIFO follow-up queue.
///
/// The queue is process-local and never persisted: a crash drops it.
pub(crate) struct Scheduler {
    inner: Mutex<SchedulerInner>,
}

struct SchedulerInner {
    active: Option<OperationId>,
    queue: VecDeque<OperationId>,
}

pub(crate) enum Admission {
    /// The caller may drive immediately.
    Direct,
    /// The operation waits in the FIFO queue.
    Queued,
}

impl Scheduler {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(SchedulerInner {
                active: None,
                queue: VecDeque::new(),
            }),
        })
    }

    /// Admits a new operation: claims the active slot when the runtime is
    /// fully idle, otherwise appends to the FIFO queue.
    pub fn admit(&self, operation_id: &OperationId) -> Admission {
        let mut inner = self.inner.lock().expect("scheduler mutex");
        if inner.active.is_none() && inner.queue.is_empty() {
            inner.active = Some(operation_id.clone());
            Admission::Direct
        } else {
            inner.queue.push_back(operation_id.clone());
            Admission::Queued
        }
    }

    /// Atomically claims the active slot for the queue head. Also settles the
    /// race with `OperationHandle::cancel`: a queued operation cancelled in
    /// the same instant is observed as settled, never double-driven.
    pub fn try_claim_queued(&self, shared: &OperationShared) -> QueueClaim {
        let mut scheduler = self.inner.lock().expect("scheduler mutex");
        let mut state = shared.inner.lock().expect("operation mutex");
        if state.outcome.is_some() {
            return QueueClaim::SettledInQueue;
        }
        if scheduler.active.is_none() && scheduler.queue.front() == Some(&shared.operation_id) {
            scheduler.queue.pop_front();
            scheduler.active = Some(shared.operation_id.clone());
            state.phase = OperationPhase::PreparingTurn;
            QueueClaim::Claimed
        } else {
            QueueClaim::NotYet
        }
    }

    pub fn release(&self, operation_id: &OperationId) {
        let mut inner = self.inner.lock().expect("scheduler mutex");
        if inner.active.as_ref() == Some(operation_id) {
            inner.active = None;
        }
    }

    pub fn availability(&self) -> AgentAvailability {
        let inner = self.inner.lock().expect("scheduler mutex");
        match &inner.active {
            Some(operation_id) => AgentAvailability::Busy {
                operation_id: operation_id.clone(),
            },
            None => AgentAvailability::Idle,
        }
    }
}

pub(crate) enum QueueClaim {
    Claimed,
    NotYet,
    SettledInQueue,
}

struct SharedInner {
    phase: OperationPhase,
    /// Live event queue: publication enqueues immediately (M10 real-time
    /// obligation), consumption pops through the handle.
    events: VecDeque<AgentEvent>,
    outcome: Option<OperationOutcome>,
    /// The consumer waiting on events or the outcome.
    waker: Option<std::task::Waker>,
}

/// State shared between an [`OperationHandle`], its engine, and the scheduler.
pub(crate) struct OperationShared {
    operation_id: OperationId,
    turn_id: TurnId,
    scheduler: Arc<Scheduler>,
    cancel_requested: AtomicBool,
    /// First accepted cancellation reason; the winner of a user/timeout
    /// race decides how the terminal facts are recorded.
    cancel_reason: Mutex<Option<CancelReason>>,
    /// Automatic-cancellation deadline, armed when driving actually starts
    /// (dequeue time); `Queued` waiting never counts.
    deadline: Mutex<Option<Instant>>,
    inner: Mutex<SharedInner>,
}

impl OperationShared {
    pub fn new(
        operation_id: OperationId,
        turn_id: TurnId,
        scheduler: Arc<Scheduler>,
        phase: OperationPhase,
    ) -> Self {
        Self {
            operation_id,
            turn_id,
            scheduler,
            cancel_requested: AtomicBool::new(false),
            cancel_reason: Mutex::new(None),
            deadline: Mutex::new(None),
            inner: Mutex::new(SharedInner {
                phase,
                events: VecDeque::new(),
                outcome: None,
                waker: None,
            }),
        }
    }

    pub fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    /// Publishes one event: enqueued and consumable immediately.
    pub fn publish(&self, event: AgentEvent) {
        let waker = {
            let mut inner = self.inner.lock().expect("operation mutex");
            inner.events.push_back(event);
            inner.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub fn set_phase(&self, phase: OperationPhase) {
        self.inner.lock().expect("operation mutex").phase = phase;
    }

    pub fn phase(&self) -> OperationPhase {
        self.inner.lock().expect("operation mutex").phase.clone()
    }

    /// Accepts a cancellation request with its reason; the first acceptance
    /// wins a user/timeout race and later requests change nothing.
    pub fn request_cancel(&self, reason: CancelReason) {
        let mut slot = self.cancel_reason.lock().expect("cancel reason mutex");
        if slot.is_none() {
            *slot = Some(reason);
            self.cancel_requested.store(true, Ordering::SeqCst);
        }
    }

    /// The reason of the accepted cancellation; meaningful only after
    /// [`OperationShared::is_cancel_requested`] returned true.
    pub fn cancel_reason(&self) -> CancelReason {
        self.cancel_reason
            .lock()
            .expect("cancel reason mutex")
            .unwrap_or(CancelReason::User)
    }

    /// Arms the automatic-cancellation deadline at drive start.
    pub fn arm_deadline(&self, timeout: Option<Duration>) {
        *self.deadline.lock().expect("deadline mutex") =
            timeout.map(|timeout| Instant::now() + timeout);
    }

    /// Lazily checked at the M6 injection points (and between stream
    /// events): an expired deadline requests cancellation with reason
    /// `Timeout`, losing gracefully if a user cancel arrived first.
    pub fn is_cancel_requested(&self) -> bool {
        if !self.cancel_requested.load(Ordering::SeqCst)
            && self
                .deadline
                .lock()
                .expect("deadline mutex")
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.request_cancel(CancelReason::Timeout);
        }
        self.cancel_requested.load(Ordering::SeqCst)
    }

    fn settle(&self, outcome: OperationOutcome) {
        let status = match &outcome {
            OperationOutcome::Succeeded { .. } => OperationStatus::Succeeded,
            OperationOutcome::Failed { .. } => OperationStatus::Failed,
            OperationOutcome::Cancelled => OperationStatus::Cancelled,
        };
        let waker = {
            let mut inner = self.inner.lock().expect("operation mutex");
            inner.phase = OperationPhase::Settled(status);
            inner.outcome = Some(outcome);
            inner.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn settled_outcome(&self) -> Option<OperationOutcome> {
        self.inner.lock().expect("operation mutex").outcome.clone()
    }

    fn pop_event(&self) -> Option<AgentEvent> {
        self.inner
            .lock()
            .expect("operation mutex")
            .events
            .pop_front()
    }

    fn register_waker(&self, cx: &std::task::Context<'_>) {
        self.inner.lock().expect("operation mutex").waker = Some(cx.waker().clone());
    }
}

/// Live handle of one admitted operation.
///
/// The handle observes phase and outcome, may request cancellation, and for
/// queued operations owns the engine that drives the work when polled.
pub struct OperationHandle {
    shared: Arc<OperationShared>,
    engine: Mutex<Option<Engine>>,
}

impl std::fmt::Debug for OperationHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationHandle")
            .field("operation_id", &self.shared.operation_id)
            .field("phase", &self.shared.phase())
            .finish_non_exhaustive()
    }
}

impl OperationHandle {
    pub(crate) fn with_engine(shared: Arc<OperationShared>, engine: Engine) -> Self {
        Self {
            shared,
            engine: Mutex::new(Some(engine)),
        }
    }

    pub fn operation_id(&self) -> &OperationId {
        &self.shared.operation_id
    }
    pub fn turn_id(&self) -> &TurnId {
        &self.shared.turn_id
    }

    /// Returns the current phase observation.
    pub fn phase(&self) -> OperationPhase {
        self.shared.phase()
    }

    /// Requests orderly cancellation; idempotent, non-blocking, and callable
    /// at any moment while the operation runs (M10). It takes effect at the
    /// M6 injection points on barrier boundaries.
    ///
    /// Queued operations settle immediately with zero persistent trace.
    /// Once the kernel has made its final decision (`Finalizing`) or the
    /// operation settled, cancellation has no effect and publishes nothing.
    pub fn cancel(&self) {
        let scheduler = self.shared.scheduler.clone();
        let mut scheduler_inner = scheduler.inner.lock().expect("scheduler mutex");
        let mut state = self.shared.inner.lock().expect("operation mutex");
        match state.phase {
            OperationPhase::Settled(_) | OperationPhase::Finalizing => {}
            OperationPhase::Queued => {
                scheduler_inner
                    .queue
                    .retain(|queued| queued != &self.shared.operation_id);
                state.events.push_back(AgentEvent::CancellationRequested {
                    operation_id: self.shared.operation_id.clone(),
                    reason: CancelReason::User,
                });
                state.events.push_back(AgentEvent::OperationSettled {
                    operation_id: self.shared.operation_id.clone(),
                    status: OperationStatus::Cancelled,
                    durability: SettlementDurability::Confirmed,
                });
                state.phase = OperationPhase::Settled(OperationStatus::Cancelled);
                state.outcome = Some(OperationOutcome::Cancelled);
                if let Some(waker) = state.waker.take() {
                    drop(state);
                    drop(scheduler_inner);
                    waker.wake();
                }
            }
            _ => {
                self.shared.request_cancel(CancelReason::User);
                // Wake the consumer so an idle engine re-polls and observes
                // the request promptly (effect points stay the M6 ones).
                if let Some(waker) = state.waker.take() {
                    drop(state);
                    drop(scheduler_inner);
                    waker.wake();
                }
            }
        }
    }

    /// Drives the engine one step within the caller's context.
    fn drive(&self, cx: &mut std::task::Context<'_>) {
        let mut slot = self.engine.lock().expect("engine mutex");
        if let Some(engine) = slot.as_mut()
            && engine.as_mut().poll(cx).is_ready()
        {
            *slot = None;
        }
    }

    /// Returns the next published event as soon as it is available (M10
    /// real-time obligation), driving the operation as needed. `None` after
    /// the terminal event: the operation settled and the queue drained.
    pub async fn next_event(&mut self) -> Option<AgentEvent> {
        std::future::poll_fn(|cx| {
            use std::task::Poll;
            if let Some(event) = self.shared.pop_event() {
                return Poll::Ready(Some(event));
            }
            if self.shared.settled_outcome().is_some() {
                return Poll::Ready(None);
            }
            self.drive(cx);
            if let Some(event) = self.shared.pop_event() {
                return Poll::Ready(Some(event));
            }
            if self.shared.settled_outcome().is_some() {
                return Poll::Ready(None);
            }
            self.shared.register_waker(cx);
            Poll::Pending
        })
        .await
    }

    /// Drives this operation as needed and returns its terminal outcome.
    /// Waiting without consuming events still drives the work to settled;
    /// published events stay queued for `next_event`.
    pub async fn wait(&self) -> OperationOutcome {
        std::future::poll_fn(|cx| {
            use std::task::Poll;
            if let Some(outcome) = self.shared.settled_outcome() {
                return Poll::Ready(outcome);
            }
            self.drive(cx);
            match self.shared.settled_outcome() {
                Some(outcome) => Poll::Ready(outcome),
                None => {
                    self.shared.register_waker(cx);
                    Poll::Pending
                }
            }
        })
        .await
    }
}

/// Publishes events and phase transitions while an operation is driven and
/// settles the shared state exactly once. Publication is immediate (M10):
/// the barrier-ordering guarantee lives at the call sites, which only push
/// after the corresponding barrier committed.
pub(crate) struct OperationBuilder {
    shared: Arc<OperationShared>,
}

impl OperationBuilder {
    /// Starts driving: publishes `OperationStarted` and arms the automatic
    /// cancellation deadline. `TurnStarted` follows separately through
    /// [`OperationBuilder::turn_started`] so seal notifications (M11) can
    /// land between the two.
    pub fn begin(shared: Arc<OperationShared>, operation_timeout: Option<Duration>) -> Self {
        shared.arm_deadline(operation_timeout);
        shared.publish(AgentEvent::OperationStarted {
            operation_id: shared.operation_id().clone(),
        });
        shared.set_phase(OperationPhase::PreparingTurn);
        Self { shared }
    }

    /// Publishes `TurnStarted` once every stale prior turn is sealed.
    pub fn turn_started(&mut self) {
        self.shared.publish(AgentEvent::TurnStarted {
            turn_id: self.shared.turn_id().clone(),
        });
    }

    /// Publishes `PriorTurnSealed` after one seal transaction committed.
    pub fn prior_turn_sealed(&mut self, turn_id: TurnId) {
        self.shared.publish(AgentEvent::PriorTurnSealed { turn_id });
    }

    pub fn operation_id(&self) -> &OperationId {
        self.shared.operation_id()
    }
    pub fn turn_id(&self) -> &TurnId {
        self.shared.turn_id()
    }

    /// The accepted cancellation reason recorded on this operation.
    pub fn cancel_reason(&self) -> CancelReason {
        self.shared.cancel_reason()
    }

    pub fn push(&mut self, event: AgentEvent) {
        self.shared.publish(event);
    }

    pub fn set_phase(&self, phase: OperationPhase) {
        self.shared.set_phase(phase);
    }

    /// Publishes `CancellationRequested` when a cancel signal is observed at
    /// an injection point.
    pub fn cancellation_observed(&mut self) {
        self.shared.publish(AgentEvent::CancellationRequested {
            operation_id: self.shared.operation_id().clone(),
            reason: self.shared.cancel_reason(),
        });
    }

    pub fn succeed(self, assistant: AssistantMessage) {
        self.shared.publish(AgentEvent::AssistantMessageCompleted {
            turn_id: self.shared.turn_id().clone(),
            message: assistant.clone(),
        });
        self.shared.publish(AgentEvent::OperationSettled {
            operation_id: self.shared.operation_id().clone(),
            status: OperationStatus::Succeeded,
            durability: SettlementDurability::Confirmed,
        });
        self.shared
            .settle(OperationOutcome::Succeeded { assistant });
    }

    pub fn fail_confirmed(self, failure: AgentFailure) {
        self.shared.publish(AgentEvent::TurnFailed {
            turn_id: self.shared.turn_id().clone(),
            failure: failure.clone(),
        });
        self.fail(failure, SettlementDurability::Confirmed);
    }

    pub fn fail_unconfirmed(self, failure: AgentFailure) {
        self.fail(failure, SettlementDurability::Unconfirmed);
    }

    fn fail(self, failure: AgentFailure, durability: SettlementDurability) {
        self.shared.publish(AgentEvent::OperationSettled {
            operation_id: self.shared.operation_id().clone(),
            status: OperationStatus::Failed,
            durability,
        });
        self.shared.settle(OperationOutcome::Failed {
            failure,
            durability,
        });
    }

    /// Settles as cancelled before any turn fact was persisted: no
    /// `TurnCancelled` because durably no turn exists.
    pub fn cancel_zero_trace(self) {
        self.shared.publish(AgentEvent::OperationSettled {
            operation_id: self.shared.operation_id().clone(),
            status: OperationStatus::Cancelled,
            durability: SettlementDurability::Confirmed,
        });
        self.shared.settle(OperationOutcome::Cancelled);
    }

    /// Settles as cancelled after the cancellation transaction committed:
    /// publishes `TurnCancelled` before the terminal event.
    pub fn cancel_committed(self) {
        self.shared.publish(AgentEvent::TurnCancelled {
            turn_id: self.shared.turn_id().clone(),
            reason: self.shared.cancel_reason(),
        });
        self.shared.publish(AgentEvent::OperationSettled {
            operation_id: self.shared.operation_id().clone(),
            status: OperationStatus::Cancelled,
            durability: SettlementDurability::Confirmed,
        });
        self.shared.settle(OperationOutcome::Cancelled);
    }
}
