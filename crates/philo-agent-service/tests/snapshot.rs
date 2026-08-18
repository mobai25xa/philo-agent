//! Session-scoped snapshot fence: settlement floors, load tokens, supersession, resync.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use philo_agent_runtime::{
    ActiveOperationSnapshot, AgentAvailability, AgentEvent, ModelCallId, OperationId,
    OperationPhase, OperationStatus, RuntimeEpoch, RuntimeSnapshot, SettlementDurability,
    ShutdownState,
};
use philo_agent_service::testing::{FakeAssembler, start_test_service, start_test_service_with};
use philo_agent_service::{
    CommandDispatch, CommandReject, FrontendAssistantBlock, FrontendCommand,
    FrontendContextMessage, FrontendRevision, FrontendUpdate, FrontendUpdateKind, RecvOutcome,
    RuntimeEvent,
};
use philo_session::{
    MemorySessionStore, OperationOutcome, SessionAssistantBlock, SessionCommit, SessionContextView,
    SessionEntryKind, SessionError, SessionFuture, SessionId, SessionRevision, SessionStore,
    SessionTransaction, SessionUserPart, TurnId, TurnOutcome,
};
use tokio::sync::{Notify, watch};

fn accept_snapshot(
    client: &philo_agent_service::FrontendClient,
) -> philo_agent_service::FrontendRequestId {
    match client.request_snapshot(FrontendRevision::ZERO) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("expected accepted snapshot, got {other:?}"),
    }
}

async fn recv_matching(
    client: &philo_agent_service::FrontendClient,
    mut pred: impl FnMut(&FrontendUpdate) -> bool,
) -> FrontendUpdate {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut seen = Vec::new();
    loop {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(50))
            .await
        {
            RecvOutcome::Update(update) if pred(&update) => return update,
            RecvOutcome::Update(update) => seen.push(format!("{:?}", update.kind)),
            RecvOutcome::Timeout if Instant::now() < deadline => continue,
            RecvOutcome::Timeout => panic!("timed out waiting for frontend update; seen={seen:?}"),
            RecvOutcome::Disconnected => {
                panic!("frontend disconnected while waiting; seen={seen:?}")
            }
        }
    }
}

fn is_operation_settled(kind: &FrontendUpdateKind) -> bool {
    matches!(
        kind,
        FrontendUpdateKind::OperationEvent(
            philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
        )
    )
}

fn assistant_texts(view: &philo_agent_service::DurableSessionView) -> Vec<&str> {
    view.messages
        .iter()
        .filter_map(|message| match message {
            FrontendContextMessage::Assistant { blocks } => blocks.iter().find_map(|block| {
                if let FrontendAssistantBlock::Text { text } = block {
                    Some(text.as_str())
                } else {
                    None
                }
            }),
            _ => None,
        })
        .collect()
}

async fn seed_session(store: &MemorySessionStore, session_id: &str) {
    commit_turn(
        store,
        session_id,
        SessionRevision::ZERO,
        "op-seed",
        "turn-seed",
        "hello from store",
        "durable answer",
    )
    .await;
}

async fn commit_turn(
    store: &impl SessionStore,
    session_id: &str,
    expected: SessionRevision,
    operation_id: &str,
    turn_id: &str,
    user: &str,
    assistant: &str,
) -> SessionCommit {
    store
        .commit(SessionTransaction::linear(
            SessionId::new(session_id),
            expected,
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: philo_session::OperationId::new(operation_id),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: philo_session::OperationId::new(operation_id),
                    turn_id: TurnId::new(turn_id),
                },
                SessionEntryKind::UserMessage {
                    turn_id: TurnId::new(turn_id),
                    parts: SessionUserPart::text_parts(user),
                },
                SessionEntryKind::AssistantMessage {
                    turn_id: TurnId::new(turn_id),
                    blocks: vec![SessionAssistantBlock::Text {
                        text: assistant.into(),
                    }],
                },
                SessionEntryKind::TurnTerminated {
                    turn_id: TurnId::new(turn_id),
                    outcome: TurnOutcome::Succeeded,
                },
                SessionEntryKind::OperationSettled {
                    operation_id: philo_session::OperationId::new(operation_id),
                    outcome: OperationOutcome::Succeeded,
                },
            ],
        ))
        .await
        .unwrap()
}

async fn commit_turn_start(
    store: &impl SessionStore,
    session_id: &str,
    expected: SessionRevision,
    operation_id: &str,
    turn_id: &str,
    user: &str,
) -> SessionCommit {
    store
        .commit(SessionTransaction::linear(
            SessionId::new(session_id),
            expected,
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: philo_session::OperationId::new(operation_id),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: philo_session::OperationId::new(operation_id),
                    turn_id: TurnId::new(turn_id),
                },
                SessionEntryKind::UserMessage {
                    turn_id: TurnId::new(turn_id),
                    parts: SessionUserPart::text_parts(user),
                },
            ],
        ))
        .await
        .unwrap()
}

async fn commit_turn_finish(
    store: &impl SessionStore,
    session_id: &str,
    expected: SessionRevision,
    operation_id: &str,
    turn_id: &str,
    assistant: &str,
) -> SessionCommit {
    store
        .commit(SessionTransaction::linear(
            SessionId::new(session_id),
            expected,
            vec![
                SessionEntryKind::AssistantMessage {
                    turn_id: TurnId::new(turn_id),
                    blocks: vec![SessionAssistantBlock::Text {
                        text: assistant.into(),
                    }],
                },
                SessionEntryKind::TurnTerminated {
                    turn_id: TurnId::new(turn_id),
                    outcome: TurnOutcome::Succeeded,
                },
                SessionEntryKind::OperationSettled {
                    operation_id: philo_session::OperationId::new(operation_id),
                    outcome: OperationOutcome::Succeeded,
                },
            ],
        ))
        .await
        .unwrap()
}

async fn load_session(client: &philo_agent_service::FrontendClient, session_id: &str) {
    assert!(matches!(
        client.try_command(FrontendCommand::LoadSession {
            session_id: session_id.into(),
        }),
        CommandDispatch::Enqueued(_)
    ));
    recv_matching(client, |update| {
        matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
    })
    .await;
}

fn submit_hello(
    client: &philo_agent_service::FrontendClient,
) -> philo_agent_service::FrontendRequestId {
    match client.try_command(FrontendCommand::Submit {
        draft: "hello".into(),
        attachments: Vec::new(),
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("submit enqueue {other:?}"),
    }
}

async fn current_session_id(client: &philo_agent_service::FrontendClient) -> Option<String> {
    let request_id = accept_snapshot(client);
    let update = recv_matching(client, |update| {
        update.request_id == Some(request_id)
            && matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
    })
    .await;
    match update.kind {
        FrontendUpdateKind::SnapshotReady(snapshot) => snapshot.current_session_id,
        other => panic!("expected snapshot, got {other:?}"),
    }
}

fn emit_live(
    runtime: &philo_agent_service::testing::FakeRuntimeHandle,
    session_id: &str,
    operation_id: &str,
    turn_id: &str,
    text: &str,
) {
    runtime.emit(RuntimeEvent::OperationAccepted {
        operation_id: OperationId::new(operation_id),
        session_id: philo_agent_runtime::SessionId::new(session_id),
        turn_id: philo_agent_runtime::TurnId::new(turn_id),
    });
    runtime.emit_agent(AgentEvent::OperationStarted {
        operation_id: OperationId::new(operation_id),
    });
    runtime.emit_agent(AgentEvent::TurnStarted {
        turn_id: philo_agent_runtime::TurnId::new(turn_id),
    });
    runtime.emit_agent(AgentEvent::ModelCallStarted {
        model_call_id: ModelCallId::new("mc-live"),
    });
    runtime.emit_agent(AgentEvent::TextDelta { delta: text.into() });
}

fn emit_settled(
    runtime: &philo_agent_service::testing::FakeRuntimeHandle,
    operation_id: &str,
    session_id: &str,
    session_revision: u64,
) {
    runtime.emit(RuntimeEvent::OperationSettled {
        operation_id: OperationId::new(operation_id),
        session_id: philo_agent_runtime::SessionId::new(session_id),
        status: OperationStatus::Succeeded,
        durability: SettlementDurability::Confirmed,
        session_revision: philo_agent_runtime::SettlementRevision::Committed(
            philo_session::SessionRevision::new(session_revision),
        ),
    });
}

#[derive(Clone)]
struct ScriptedStore {
    inner: Arc<MemorySessionStore>,
    prepend: Arc<Mutex<VecDeque<SessionContextView>>>,
    errors: Arc<Mutex<VecDeque<SessionError>>>,
}

impl ScriptedStore {
    fn new(inner: Arc<MemorySessionStore>) -> Self {
        Self {
            inner,
            prepend: Arc::new(Mutex::new(VecDeque::new())),
            errors: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn prepend(&self, view: SessionContextView) {
        self.prepend.lock().expect("scripted store").push_back(view);
    }

    fn fail_next(&self, error: SessionError) {
        self.errors.lock().expect("scripted store").push_back(error);
    }
}

impl SessionStore for ScriptedStore {
    fn context_view<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<SessionContextView, SessionError>> {
        Box::pin(async move {
            if let Some(error) = self.errors.lock().expect("scripted store").pop_front() {
                return Err(error);
            }
            if let Some(view) = self.prepend.lock().expect("scripted store").pop_front() {
                return Ok(view);
            }
            self.inner.context_view(session_id).await
        })
    }

    fn commit<'a>(
        &'a self,
        transaction: SessionTransaction,
    ) -> SessionFuture<'a, Result<SessionCommit, SessionError>> {
        self.inner.commit(transaction)
    }

    fn list_sessions(&self) -> SessionFuture<'_, Result<Vec<SessionId>, SessionError>> {
        self.inner.list_sessions()
    }
}

struct AfterReadStore {
    inner: Arc<MemorySessionStore>,
    target: usize,
    reads: AtomicUsize,
    parked: Arc<AtomicUsize>,
    parked_notify: Arc<Notify>,
    go: watch::Receiver<bool>,
}

struct AfterReadHold {
    tx: watch::Sender<bool>,
    parked: Arc<AtomicUsize>,
    parked_notify: Arc<Notify>,
    target: usize,
}

impl AfterReadHold {
    async fn wait_held(&self) {
        loop {
            let notified = self.parked_notify.notified();
            if self.parked.load(Ordering::SeqCst) == self.target {
                return;
            }
            notified.await;
        }
    }

    fn release(self) {
        let _ = self.tx.send(true);
    }
}

impl AfterReadStore {
    fn gate_after(inner: Arc<MemorySessionStore>, target: usize) -> (Self, AfterReadHold) {
        let (tx, rx) = watch::channel(false);
        let parked = Arc::new(AtomicUsize::new(0));
        let parked_notify = Arc::new(Notify::new());
        (
            Self {
                inner,
                target,
                reads: AtomicUsize::new(0),
                parked: parked.clone(),
                parked_notify: parked_notify.clone(),
                go: rx,
            },
            AfterReadHold {
                tx,
                parked,
                parked_notify,
                target,
            },
        )
    }
}

impl SessionStore for AfterReadStore {
    fn context_view<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<SessionContextView, SessionError>> {
        let read = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        let gate = (read == self.target).then(|| {
            (
                self.parked.clone(),
                self.parked_notify.clone(),
                self.go.clone(),
                self.target,
            )
        });
        Box::pin(async move {
            let view = self.inner.context_view(session_id).await?;
            if let Some((parked, parked_notify, mut go, target)) = gate {
                parked.store(target, Ordering::SeqCst);
                parked_notify.notify_waiters();
                loop {
                    if *go.borrow() {
                        break;
                    }
                    if go.changed().await.is_err() {
                        break;
                    }
                }
            }
            Ok(view)
        })
    }

    fn commit<'a>(
        &'a self,
        transaction: SessionTransaction,
    ) -> SessionFuture<'a, Result<SessionCommit, SessionError>> {
        self.inner.commit(transaction)
    }

    fn list_sessions(&self) -> SessionFuture<'_, Result<Vec<SessionId>, SessionError>> {
        self.inner.list_sessions()
    }
}

#[derive(Clone)]
struct CountingStaleStore {
    inner: Arc<MemorySessionStore>,
    stale: SessionContextView,
    reads: Arc<AtomicUsize>,
}

impl CountingStaleStore {
    fn new(inner: Arc<MemorySessionStore>, stale: SessionContextView) -> Self {
        Self {
            inner,
            stale,
            reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl SessionStore for CountingStaleStore {
    fn context_view<'a>(
        &'a self,
        _session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<SessionContextView, SessionError>> {
        let view = self.stale.clone();
        self.reads.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(view) })
    }

    fn commit<'a>(
        &'a self,
        transaction: SessionTransaction,
    ) -> SessionFuture<'a, Result<SessionCommit, SessionError>> {
        self.inner.commit(transaction)
    }

    fn list_sessions(&self) -> SessionFuture<'_, Result<Vec<SessionId>, SessionError>> {
        self.inner.list_sessions()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_view_after_settlement_retries_until_final_once() {
    let inner = Arc::new(MemorySessionStore::new());
    seed_session(&inner, "sess-fence").await;
    commit_turn_start(
        inner.as_ref(),
        "sess-fence",
        SessionRevision::new(1),
        "op-live",
        "turn-live",
        "ask",
    )
    .await;
    let stale = inner
        .context_view(&SessionId::new("sess-fence"))
        .await
        .unwrap();
    assert_eq!(stale.revision().get(), 2);
    let final_commit = commit_turn_finish(
        inner.as_ref(),
        "sess-fence",
        SessionRevision::new(2),
        "op-live",
        "turn-live",
        "final answer",
    )
    .await;
    let store = ScriptedStore::new(inner);
    let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store.clone());
    load_session(&client, "sess-fence").await;
    emit_live(&runtime, "sess-fence", "op-live", "turn-live", "partial");
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::OperationEvent(
                philo_agent_service::FrontendOperationEvent::TextDelta { .. }
            )
        )
    })
    .await;
    emit_settled(
        &runtime,
        "op-live",
        "sess-fence",
        final_commit.revision().get(),
    );
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::OperationEvent(
                philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
            )
        )
    })
    .await;
    store.prepend(stale);

    let _request_id = accept_snapshot(&client);
    let update = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
    })
    .await;
    let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
        unreachable!();
    };
    let durable = snapshot.durable_session_view.expect("session loaded");
    let texts = assistant_texts(&durable);
    assert_eq!(
        texts.iter().filter(|text| **text == "final answer").count(),
        1
    );
    assert!(!snapshot.live.text.contains("partial"));
    assert!(snapshot.live.operation_id.is_none());
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_final_strips_live_before_settlement() {
    let store = MemorySessionStore::new();
    seed_session(&store, "sess-dup").await;
    commit_turn(
        &store,
        "sess-dup",
        SessionRevision::new(1),
        "op-live",
        "turn-live",
        "ask",
        "final answer",
    )
    .await;
    let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);
    load_session(&client, "sess-dup").await;
    emit_live(&runtime, "sess-dup", "op-live", "turn-live", "partial");
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::OperationEvent(
                philo_agent_service::FrontendOperationEvent::TextDelta { .. }
            )
        )
    })
    .await;

    let _request_id = accept_snapshot(&client);
    let update = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
    })
    .await;
    let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
        unreachable!();
    };
    let durable = snapshot.durable_session_view.expect("session loaded");
    assert_eq!(
        assistant_texts(&durable)
            .iter()
            .filter(|text| **text == "final answer")
            .count(),
        1
    );
    assert!(
        snapshot.live.text.is_empty(),
        "live answer must not duplicate durable final"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn superseded_snapshot_does_not_publish() {
    let inner = Arc::new(MemorySessionStore::new());
    seed_session(&inner, "sess-a").await;
    seed_session(&inner, "sess-b").await;
    let (store, hold) = AfterReadStore::gate_after(inner, 2);
    let (service, client, _runtime) = start_test_service_with(FakeAssembler::new(), store);
    load_session(&client, "sess-a").await;

    let first = accept_snapshot(&client);
    hold.wait_held().await;
    assert!(matches!(
        client.try_command(FrontendCommand::LoadSession {
            session_id: "sess-b".into(),
        }),
        CommandDispatch::Enqueued(_)
    ));
    recv_matching(&client, |update| match &update.kind {
        FrontendUpdateKind::SessionLoaded { session_id, .. } => session_id == "sess-b",
        _ => false,
    })
    .await;
    let second = accept_snapshot(&client);
    assert!(second > first);

    let update = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
    })
    .await;
    assert_eq!(update.request_id, Some(second));
    let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
        unreachable!();
    };
    assert_eq!(snapshot.current_session_id.as_deref(), Some("sess-b"));

    hold.release();
    let late_deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < late_deadline {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(50))
            .await
        {
            RecvOutcome::Update(update) => {
                if let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind {
                    assert_ne!(
                        update.request_id,
                        Some(first),
                        "superseded snapshot must not publish"
                    );
                    assert_eq!(snapshot.current_session_id.as_deref(), Some("sess-b"));
                }
            }
            RecvOutcome::Timeout => break,
            RecvOutcome::Disconnected => panic!("disconnected"),
        }
    }
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn confirmed_settlement_retries_until_required_revision() {
    let inner = Arc::new(MemorySessionStore::new());
    seed_session(&inner, "sess-rev").await;
    commit_turn_start(
        inner.as_ref(),
        "sess-rev",
        SessionRevision::new(1),
        "op-live",
        "turn-live",
        "ask",
    )
    .await;
    let stale = inner
        .context_view(&SessionId::new("sess-rev"))
        .await
        .unwrap();
    let final_commit = commit_turn_finish(
        inner.as_ref(),
        "sess-rev",
        SessionRevision::new(2),
        "op-live",
        "turn-live",
        "final answer",
    )
    .await;
    let store = ScriptedStore::new(inner);
    let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store.clone());
    load_session(&client, "sess-rev").await;
    runtime.emit_operation_accepted(
        OperationId::new("op-live"),
        philo_agent_runtime::SessionId::new("sess-rev"),
        philo_agent_runtime::TurnId::new("turn-live"),
    );
    recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::OperationAccepted { .. })
    })
    .await;
    emit_settled(
        &runtime,
        "op-live",
        "sess-rev",
        final_commit.revision().get(),
    );
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::OperationEvent(
                philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
            )
        )
    })
    .await;
    store.prepend(stale);

    let _request_id = accept_snapshot(&client);
    let update = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
    })
    .await;
    let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
        unreachable!();
    };
    let durable = snapshot.durable_session_view.expect("session loaded");
    assert!(durable.revision >= final_commit.revision().get());
    assert!(assistant_texts(&durable).contains(&"final answer"));
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn feed_overflow_resync_snapshot_is_atomic_and_later_revisions_increase() {
    let store = MemorySessionStore::new();
    seed_session(&store, "sess-resync").await;
    let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);
    load_session(&client, "sess-resync").await;

    for index in 0..200 {
        runtime.emit_agent(AgentEvent::TextDelta {
            delta: format!("x{index}"),
        });
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while runtime.consumed() < 200 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::ResyncRequired { .. })
    })
    .await;

    let _request_id = accept_snapshot(&client);
    let update = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
    })
    .await;
    let snapshot_revision = update.revision;
    let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
        unreachable!();
    };
    assert!(snapshot.revision >= snapshot_revision);
    assert_eq!(
        snapshot.durable_session_view.as_ref().unwrap().session_id,
        "sess-resync"
    );

    runtime.emit_agent(AgentEvent::TextDelta {
        delta: "after-snapshot".into(),
    });
    let later = recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::OperationEvent(
                philo_agent_service::FrontendOperationEvent::TextDelta { .. }
            )
        )
    })
    .await;
    assert!(
        later.revision > snapshot_revision,
        "updates after SnapshotReady must only carry a higher revision"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn settlement_during_resync_queues_fenced_snapshot() {
    let store = MemorySessionStore::new();
    seed_session(&store, "sess-auto").await;
    commit_turn(
        &store,
        "sess-auto",
        SessionRevision::new(1),
        "op-live",
        "turn-live",
        "ask",
        "final answer",
    )
    .await;
    let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);
    load_session(&client, "sess-auto").await;
    emit_live(&runtime, "sess-auto", "op-live", "turn-live", "partial");
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::OperationEvent(
                philo_agent_service::FrontendOperationEvent::TextDelta { .. }
            )
        )
    })
    .await;

    let consumed_before = runtime.consumed();
    for index in 0..200 {
        runtime.emit_agent(AgentEvent::TextDelta {
            delta: format!("y{index}"),
        });
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while runtime.consumed() < consumed_before + 200 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        runtime.consumed() >= consumed_before + 200,
        "runtime flood must be consumed before draining the frontend feed"
    );
    recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::ResyncRequired { .. })
    })
    .await;

    emit_settled(&runtime, "op-live", "sess-auto", 2);
    let update = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
    })
    .await;
    let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
        unreachable!();
    };
    let durable = snapshot.durable_session_view.expect("session loaded");
    assert!(assistant_texts(&durable).contains(&"final answer"));
    assert!(snapshot.live.text.is_empty());
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn subscription_lag_requests_runtime_snapshot() {
    let (service, client, runtime) = start_test_service();
    runtime.set_snapshot(RuntimeSnapshot {
        epoch: RuntimeEpoch::new("epoch-1"),
        availability: AgentAvailability::Busy {
            operation_id: OperationId::new("op-lag"),
        },
        queued: vec![philo_agent_runtime::QueuedOperationSnapshot {
            operation_id: OperationId::new("op-queued"),
            session_id: philo_agent_runtime::SessionId::new("sess-lag"),
        }],
        active: None,
        maintenance: None,
        shutdown: ShutdownState::Running,
        last_settled: Vec::new(),
        runtime_revision: 9,
    });
    runtime.emit(RuntimeEvent::SubscriptionLagged { dropped: 3 });
    let update = recv_matching(&client, |update| {
        matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
    })
    .await;
    let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
        unreachable!();
    };
    assert!(matches!(
        snapshot.availability,
        philo_agent_service::FrontendAvailability::Busy { .. }
    ));
    assert_eq!(snapshot.queued.len(), 1);
    assert_eq!(snapshot.queued[0].operation_id, "op-queued");
    assert_eq!(snapshot.queued[0].session_id, "sess-lag");
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn settlement_updates_floor_of_event_session_not_current() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let inner = Arc::new(MemorySessionStore::new());
        seed_session(&inner, "sess-a").await;
        seed_session(&inner, "sess-b").await;
        let mid = commit_turn(
            inner.as_ref(),
            "sess-a",
            SessionRevision::new(1),
            "op-a",
            "turn-a",
            "ask-a",
            "answer-a",
        )
        .await;
        let stale_a = inner.context_view(&SessionId::new("sess-a")).await.unwrap();
        let final_a = commit_turn(
            inner.as_ref(),
            "sess-a",
            mid.revision(),
            "op-a-final",
            "turn-a-final",
            "ask-a-2",
            "answer-a-final",
        )
        .await;
        let store = ScriptedStore::new(inner);
        let (service, client, runtime) =
            start_test_service_with(FakeAssembler::new(), store.clone());
        load_session(&client, "sess-b").await;
        runtime.emit_operation_accepted(
            OperationId::new("op-a"),
            philo_agent_runtime::SessionId::new("sess-a"),
            philo_agent_runtime::TurnId::new("turn-a"),
        );
        emit_settled(&runtime, "op-a", "sess-a", final_a.revision().get());
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
                )
            )
        })
        .await;

        let _ = accept_snapshot(&client);
        let update = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
        })
        .await;
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        assert_eq!(snapshot.current_session_id.as_deref(), Some("sess-b"));
        let durable = snapshot.durable_session_view.expect("b snapshot");
        assert_eq!(durable.session_id, "sess-b");
        assert_eq!(durable.revision, 1);

        load_session(&client, "sess-a").await;
        store.prepend(stale_a);
        let _ = accept_snapshot(&client);
        let update = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
        })
        .await;
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        let durable = snapshot.durable_session_view.expect("a snapshot");
        assert_eq!(durable.session_id, "sess-a");
        assert!(durable.revision >= final_a.revision().get());
        assert!(assistant_texts(&durable).contains(&"answer-a-final"));
        drop(service);
    })
    .await
    .expect("event-session floor test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_summary_uses_accepted_session() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let store = MemorySessionStore::new();
        seed_session(&store, "sess-a").await;
        seed_session(&store, "sess-b").await;
        let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);
        load_session(&client, "sess-b").await;
        runtime.emit_operation_accepted(
            OperationId::new("op-a"),
            philo_agent_runtime::SessionId::new("sess-a"),
            philo_agent_runtime::TurnId::new("turn-a"),
        );
        recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::OperationAccepted { .. })
        })
        .await;
        runtime.emit_agent(AgentEvent::OperationQueued {
            operation_id: OperationId::new("op-a"),
        });
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::OperationQueued { .. }
                )
            )
        })
        .await;

        let _ = accept_snapshot(&client);
        let update = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
        })
        .await;
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        assert_eq!(snapshot.queued.len(), 1);
        assert_eq!(snapshot.queued[0].operation_id, "op-a");
        assert_eq!(
            snapshot.queued[0].session_id, "sess-a",
            "queued summary must use accepted session, not current UI session"
        );
        drop(service);
    })
    .await
    .expect("queued accepted-session test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn switch_back_rejects_stale_load_and_reloads() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let inner = Arc::new(MemorySessionStore::new());
        seed_session(&inner, "sess-a").await;
        seed_session(&inner, "sess-b").await;
        let mid = commit_turn(
            inner.as_ref(),
            "sess-a",
            SessionRevision::new(1),
            "op-a",
            "turn-a",
            "ask-a",
            "answer-a",
        )
        .await;
        let stale_a = inner.context_view(&SessionId::new("sess-a")).await.unwrap();
        let final_a = commit_turn(
            inner.as_ref(),
            "sess-a",
            mid.revision(),
            "op-a-final",
            "turn-a-final",
            "ask-a-2",
            "answer-a-final",
        )
        .await;
        let store = ScriptedStore::new(inner);
        let (service, client, runtime) =
            start_test_service_with(FakeAssembler::new(), store.clone());
        load_session(&client, "sess-b").await;
        runtime.emit_operation_accepted(
            OperationId::new("op-a"),
            philo_agent_runtime::SessionId::new("sess-a"),
            philo_agent_runtime::TurnId::new("turn-a"),
        );
        emit_settled(&runtime, "op-a", "sess-a", final_a.revision().get());
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
                )
            )
        })
        .await;

        store.prepend(stale_a);
        assert!(matches!(
            client.try_command(FrontendCommand::LoadSession {
                session_id: "sess-a".into(),
            }),
            CommandDispatch::Enqueued(_)
        ));
        let loaded = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
        })
        .await;
        let FrontendUpdateKind::SessionLoaded { session_id, view } = loaded.kind else {
            unreachable!();
        };
        assert_eq!(session_id, "sess-a");
        assert!(view.revision >= final_a.revision().get());
        assert!(assistant_texts(&view).contains(&"answer-a-final"));
        drop(service);
    })
    .await
    .expect("switch-back stale load test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn late_load_of_a_does_not_publish_onto_b() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let inner = Arc::new(MemorySessionStore::new());
        seed_session(&inner, "sess-a").await;
        seed_session(&inner, "sess-b").await;
        let (store, hold) = AfterReadStore::gate_after(inner, 1);
        let (service, client, _runtime) = start_test_service_with(FakeAssembler::new(), store);
        assert!(matches!(
            client.try_command(FrontendCommand::LoadSession {
                session_id: "sess-a".into(),
            }),
            CommandDispatch::Enqueued(_)
        ));
        hold.wait_held().await;
        assert!(matches!(
            client.try_command(FrontendCommand::LoadSession {
                session_id: "sess-b".into(),
            }),
            CommandDispatch::Enqueued(_)
        ));
        recv_matching(&client, |update| match &update.kind {
            FrontendUpdateKind::SessionLoaded { session_id, .. } => session_id == "sess-b",
            _ => false,
        })
        .await;

        hold.release();
        let late_deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < late_deadline {
            match client
                .recv_until_async(Instant::now() + Duration::from_millis(50))
                .await
            {
                RecvOutcome::Update(update) => {
                    if let FrontendUpdateKind::SessionLoaded { session_id, .. } = update.kind {
                        assert_ne!(
                            session_id, "sess-a",
                            "late load of A must not publish onto B"
                        );
                    }
                }
                RecvOutcome::Timeout => break,
                RecvOutcome::Disconnected => panic!("disconnected"),
            }
        }
        assert_eq!(current_session_id(&client).await.as_deref(), Some("sess-b"));
        drop(service);
    })
    .await
    .expect("late load A onto B test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_during_first_pending_load_is_no_current_session() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let inner = Arc::new(MemorySessionStore::new());
        seed_session(&inner, "sess-a").await;
        seed_session(&inner, "sess-b").await;
        let (store, hold) = AfterReadStore::gate_after(inner, 1);
        let (service, client, _runtime) = start_test_service_with(FakeAssembler::new(), store);

        assert!(matches!(
            client.try_command(FrontendCommand::LoadSession {
                session_id: "sess-a".into(),
            }),
            CommandDispatch::Enqueued(_)
        ));
        hold.wait_held().await;
        assert!(current_session_id(&client).await.is_none());

        let request_id = submit_hello(&client);
        let rejected = recv_matching(&client, |update| {
            update.request_id == Some(request_id)
                && matches!(update.kind, FrontendUpdateKind::CommandRejected { .. })
        })
        .await;
        match &rejected.kind {
            FrontendUpdateKind::CommandRejected {
                reason: CommandReject::NoCurrentSession,
            } => {}
            other => panic!("expected NoCurrentSession, got {other:?}"),
        }

        hold.release();
        recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
        })
        .await;
        drop(service);
    })
    .await
    .expect("submit during first pending load timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_during_replacement_load_targets_current_session() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let inner = Arc::new(MemorySessionStore::new());
        seed_session(&inner, "sess-a").await;
        seed_session(&inner, "sess-b").await;
        let (store, hold) = AfterReadStore::gate_after(inner, 2);
        let (service, client, _runtime) = start_test_service_with(FakeAssembler::new(), store);
        load_session(&client, "sess-a").await;

        assert!(matches!(
            client.try_command(FrontendCommand::LoadSession {
                session_id: "sess-b".into(),
            }),
            CommandDispatch::Enqueued(_)
        ));
        hold.wait_held().await;

        let request_id = submit_hello(&client);
        let accepted = recv_matching(&client, |update| {
            update.request_id == Some(request_id)
                && matches!(
                    update.kind,
                    FrontendUpdateKind::SubmitAccepted { .. }
                        | FrontendUpdateKind::CommandRejected { .. }
                )
        })
        .await;
        assert!(
            matches!(accepted.kind, FrontendUpdateKind::SubmitAccepted { .. }),
            "submit must target current A while B is loading, got {:?}",
            accepted.kind
        );

        hold.release();
        recv_matching(&client, |update| match &update.kind {
            FrontendUpdateKind::SessionLoaded { session_id, .. } => session_id == "sess-b",
            _ => false,
        })
        .await;
        drop(service);
    })
    .await
    .expect("submit during replacement load timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn store_failure_does_not_commit_current() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let inner = Arc::new(MemorySessionStore::new());
        seed_session(&inner, "sess-a").await;
        seed_session(&inner, "sess-b").await;
        let store = ScriptedStore::new(inner);
        store.fail_next(SessionError::store_busy("queue full"));
        let (service, client, _runtime) =
            start_test_service_with(FakeAssembler::new(), store.clone());

        assert!(matches!(
            client.try_command(FrontendCommand::LoadSession {
                session_id: "sess-a".into(),
            }),
            CommandDispatch::Enqueued(_)
        ));
        recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::CommandRejected { .. })
        })
        .await;
        let late_deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < late_deadline {
            match client
                .recv_until_async(Instant::now() + Duration::from_millis(50))
                .await
            {
                RecvOutcome::Update(update) => {
                    assert!(
                        !matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. }),
                        "store failure must not publish SessionLoaded"
                    );
                }
                RecvOutcome::Timeout => break,
                RecvOutcome::Disconnected => panic!("disconnected"),
            }
        }
        assert!(current_session_id(&client).await.is_none());

        load_session(&client, "sess-a").await;
        store.fail_next(SessionError::store_unavailable("actor stopped"));
        assert!(matches!(
            client.try_command(FrontendCommand::LoadSession {
                session_id: "sess-b".into(),
            }),
            CommandDispatch::Enqueued(_)
        ));
        recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::CommandRejected { .. })
        })
        .await;
        assert_eq!(current_session_id(&client).await.as_deref(), Some("sess-a"));
        drop(service);
    })
    .await
    .expect("store failure current test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn load_reload_before_commit_is_not_dropped() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let inner = Arc::new(MemorySessionStore::new());
        seed_session(&inner, "sess-a").await;
        let (store, hold) = AfterReadStore::gate_after(inner.clone(), 1);
        let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);

        assert!(matches!(
            client.try_command(FrontendCommand::LoadSession {
                session_id: "sess-a".into(),
            }),
            CommandDispatch::Enqueued(_)
        ));
        hold.wait_held().await;
        assert!(current_session_id(&client).await.is_none());

        runtime.emit_operation_accepted(
            OperationId::new("op-raise"),
            philo_agent_runtime::SessionId::new("sess-a"),
            philo_agent_runtime::TurnId::new("turn-raise"),
        );
        recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::OperationAccepted { .. })
        })
        .await;
        let raised = commit_turn(
            inner.as_ref(),
            "sess-a",
            SessionRevision::new(1),
            "op-raise",
            "turn-raise",
            "ask-raise",
            "answer-raise",
        )
        .await;
        emit_settled(&runtime, "op-raise", "sess-a", raised.revision().get());
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
                )
            )
        })
        .await;

        hold.release();
        let loaded = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
        })
        .await;
        let FrontendUpdateKind::SessionLoaded { session_id, view } = loaded.kind else {
            unreachable!();
        };
        assert_eq!(session_id, "sess-a");
        assert!(
            view.revision >= raised.revision().get(),
            "reloaded load must meet the raised floor, got {}",
            view.revision
        );
        assert_eq!(current_session_id(&client).await.as_deref(), Some("sess-a"));
        drop(service);
    })
    .await
    .expect("load reload before commit timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn floor_is_monotonic_across_settlements() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let inner = Arc::new(MemorySessionStore::new());
        seed_session(&inner, "sess-mono").await;
        let first = commit_turn(
            inner.as_ref(),
            "sess-mono",
            SessionRevision::new(1),
            "op-1",
            "turn-1",
            "ask-1",
            "answer-1",
        )
        .await;
        let mid_view = inner
            .context_view(&SessionId::new("sess-mono"))
            .await
            .unwrap();
        let second = commit_turn(
            inner.as_ref(),
            "sess-mono",
            first.revision(),
            "op-2",
            "turn-2",
            "ask-2",
            "answer-2",
        )
        .await;
        let store = ScriptedStore::new(inner);
        let (service, client, runtime) =
            start_test_service_with(FakeAssembler::new(), store.clone());
        load_session(&client, "sess-mono").await;
        runtime.emit_operation_accepted(
            OperationId::new("op-1"),
            philo_agent_runtime::SessionId::new("sess-mono"),
            philo_agent_runtime::TurnId::new("turn-1"),
        );
        emit_settled(&runtime, "op-1", "sess-mono", first.revision().get());
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
                )
            )
        })
        .await;
        runtime.emit_operation_accepted(
            OperationId::new("op-2"),
            philo_agent_runtime::SessionId::new("sess-mono"),
            philo_agent_runtime::TurnId::new("turn-2"),
        );
        emit_settled(&runtime, "op-2", "sess-mono", second.revision().get());
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
                )
            )
        })
        .await;

        store.prepend(mid_view);
        let _ = accept_snapshot(&client);
        let update = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
        })
        .await;
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        let durable = snapshot.durable_session_view.expect("mono snapshot");
        assert!(durable.revision >= second.revision().get());
        assert!(assistant_texts(&durable).contains(&"answer-2"));
        drop(service);
    })
    .await
    .expect("monotonic floor test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn settled_session_mismatch_is_protocol_error() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let store = MemorySessionStore::new();
        seed_session(&store, "sess-a").await;
        seed_session(&store, "sess-b").await;
        let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);
        load_session(&client, "sess-a").await;
        runtime.emit_operation_accepted(
            OperationId::new("op-a"),
            philo_agent_runtime::SessionId::new("sess-a"),
            philo_agent_runtime::TurnId::new("turn-a"),
        );
        recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::OperationAccepted { .. })
        })
        .await;
        runtime.emit_operation_settled(
            OperationId::new("op-a"),
            philo_agent_runtime::SessionId::new("sess-b"),
            OperationStatus::Succeeded,
            SettlementDurability::Confirmed,
            philo_agent_runtime::SettlementRevision::Committed(SessionRevision::new(7)),
        );
        recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::ServiceHealthChanged { .. })
        })
        .await;

        let request_id = accept_snapshot(&client);
        let mut saw_settled = false;
        let update = loop {
            let update = recv_matching(&client, |_| true).await;
            if is_operation_settled(&update.kind) {
                saw_settled = true;
            }
            if update.request_id == Some(request_id)
                && matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
            {
                break update;
            }
        };
        assert!(
            !saw_settled,
            "ownership mismatch must not forward OperationSettled"
        );
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        assert!(
            snapshot
                .config_notices
                .iter()
                .any(|notice| notice.contains("protocol error")),
            "mismatch must be visible: {:?}",
            snapshot.config_notices
        );
        assert_eq!(
            snapshot.durable_session_view.as_ref().unwrap().revision,
            1,
            "A floor must not rise from a B settlement"
        );

        load_session(&client, "sess-b").await;
        let _ = accept_snapshot(&client);
        let update = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
        })
        .await;
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        assert_eq!(
            snapshot.durable_session_view.as_ref().unwrap().revision,
            1,
            "B floor must not rise from a mismatched settlement"
        );
        drop(service);
    })
    .await
    .expect("mismatch protocol-error test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn settled_without_accepted_is_protocol_error() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let store = MemorySessionStore::new();
        seed_session(&store, "sess-a").await;
        let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);
        load_session(&client, "sess-a").await;
        emit_settled(&runtime, "op-ghost", "sess-a", 9);
        recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::ServiceHealthChanged { .. })
        })
        .await;

        let request_id = accept_snapshot(&client);
        let mut saw_settled = false;
        let update = loop {
            let update = recv_matching(&client, |_| true).await;
            if is_operation_settled(&update.kind) {
                saw_settled = true;
            }
            if update.request_id == Some(request_id)
                && matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
            {
                break update;
            }
        };
        assert!(
            !saw_settled,
            "missing ownership must not forward OperationSettled"
        );
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        assert!(
            snapshot
                .config_notices
                .iter()
                .any(|notice| notice.contains("protocol error")),
            "missing ownership must be visible: {:?}",
            snapshot.config_notices
        );
        assert_eq!(
            snapshot.durable_session_view.as_ref().unwrap().revision,
            1,
            "floor must stay 0 so the seeded revision can publish"
        );
        drop(service);
    })
    .await
    .expect("missing-ownership protocol-error test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn unchanged_settlement_does_not_raise_floor() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let inner = Arc::new(MemorySessionStore::new());
        seed_session(&inner, "sess-u").await;
        let stale = inner.context_view(&SessionId::new("sess-u")).await.unwrap();
        let store = ScriptedStore::new(inner);
        let (service, client, runtime) =
            start_test_service_with(FakeAssembler::new(), store.clone());
        load_session(&client, "sess-u").await;
        runtime.emit_operation_accepted(
            OperationId::new("op-u"),
            philo_agent_runtime::SessionId::new("sess-u"),
            philo_agent_runtime::TurnId::new("turn-u"),
        );
        runtime.emit_operation_settled(
            OperationId::new("op-u"),
            philo_agent_runtime::SessionId::new("sess-u"),
            OperationStatus::Cancelled,
            SettlementDurability::Confirmed,
            philo_agent_runtime::SettlementRevision::Unchanged,
        );
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
                )
            )
        })
        .await;

        store.prepend(stale);
        let _ = accept_snapshot(&client);
        let update = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
        })
        .await;
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        assert_eq!(
            snapshot.durable_session_view.as_ref().unwrap().revision,
            1,
            "Unchanged must not force a higher store revision"
        );
        drop(service);
    })
    .await
    .expect("unchanged settlement test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_revision_is_not_session_floor() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let store = MemorySessionStore::new();
        seed_session(&store, "sess-rt").await;
        let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);
        load_session(&client, "sess-rt").await;
        runtime.set_snapshot(RuntimeSnapshot {
            epoch: RuntimeEpoch::new("epoch-1"),
            availability: AgentAvailability::Idle,
            queued: Vec::new(),
            active: None,
            maintenance: None,
            shutdown: ShutdownState::Running,
            last_settled: Vec::new(),
            runtime_revision: 99,
        });
        runtime.emit(RuntimeEvent::SubscriptionLagged { dropped: 1 });
        let update = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
        })
        .await;
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        let durable = snapshot.durable_session_view.expect("session snapshot");
        assert_eq!(durable.session_id, "sess-rt");
        assert_eq!(
            durable.revision, 1,
            "runtime_revision must not become the session floor"
        );
        drop(service);
    })
    .await
    .expect("runtime revision floor test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn settling_queued_operation_does_not_clear_other_live() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let store = MemorySessionStore::new();
        seed_session(&store, "sess-live").await;
        let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);
        load_session(&client, "sess-live").await;

        runtime.emit_operation_accepted(
            OperationId::new("op-queued"),
            philo_agent_runtime::SessionId::new("sess-live"),
            philo_agent_runtime::TurnId::new("turn-queued"),
        );
        runtime.emit_agent(AgentEvent::OperationQueued {
            operation_id: OperationId::new("op-queued"),
        });
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::OperationQueued { .. }
                )
            )
        })
        .await;

        emit_live(&runtime, "sess-live", "op-live", "turn-live", "partial");
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::TextDelta { .. }
                )
            )
        })
        .await;

        emit_settled(&runtime, "op-queued", "sess-live", 1);
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
                )
            )
        })
        .await;

        let request_id = accept_snapshot(&client);
        let update = recv_matching(&client, |update| {
            update.request_id == Some(request_id)
                && matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
        })
        .await;
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        assert_eq!(snapshot.live.operation_id.as_deref(), Some("op-live"));
        assert!(
            snapshot.live.text.contains("partial"),
            "queued settlement must not clear the displayed live: {:?}",
            snapshot.live.text
        );
        drop(service);
    })
    .await
    .expect("queued settlement live test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_stops_after_bounded_attempts() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let inner = Arc::new(MemorySessionStore::new());
        seed_session(&inner, "sess-bound").await;
        let stale = inner
            .context_view(&SessionId::new("sess-bound"))
            .await
            .unwrap();
        assert_eq!(stale.revision().get(), 1);
        let store = CountingStaleStore::new(inner, stale);
        let (service, client, runtime) =
            start_test_service_with(FakeAssembler::new(), store.clone());
        load_session(&client, "sess-bound").await;

        runtime.emit_operation_accepted(
            OperationId::new("op-raise"),
            philo_agent_runtime::SessionId::new("sess-bound"),
            philo_agent_runtime::TurnId::new("turn-raise"),
        );
        recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::OperationAccepted { .. })
        })
        .await;
        emit_settled(&runtime, "op-raise", "sess-bound", 9);
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::OperationSettled { .. }
                )
            )
        })
        .await;

        let reads_before = store.reads();
        let request_id = accept_snapshot(&client);
        let update = recv_matching(&client, |update| {
            update.request_id == Some(request_id)
                && matches!(
                    update.kind,
                    FrontendUpdateKind::CommandRejected { .. }
                        | FrontendUpdateKind::SnapshotReady(_)
                )
        })
        .await;
        match update.kind {
            FrontendUpdateKind::CommandRejected {
                reason: CommandReject::InvalidInput { reason },
            } => {
                assert!(
                    reason.contains("reload limit"),
                    "expected reload-limit reject, got {reason}"
                );
            }
            FrontendUpdateKind::SnapshotReady(snapshot) => {
                panic!(
                    "reload limit must not emit SnapshotReady; current={:?} durable={:?} notices={:?}",
                    snapshot.current_session_id,
                    snapshot
                        .durable_session_view
                        .as_ref()
                        .map(|view| view.session_id.as_str()),
                    snapshot.config_notices
                );
            }
            other => panic!("expected reject or snapshot, got {other:?}"),
        }
        let extra = store.reads().saturating_sub(reads_before);
        // Keep in sync with SNAPSHOT_RELOAD_ATTEMPT_MAX (8): first view + 8 reloads.
        assert!(
            extra <= 9,
            "reload must stop after the bound, extra reads={extra}"
        );
        assert!(
            extra >= 2,
            "floor-insufficient snapshot must reload at least once, extra reads={extra}"
        );

        let late = Instant::now() + Duration::from_millis(200);
        while Instant::now() < late {
            match client
                .recv_until_async(Instant::now() + Duration::from_millis(50))
                .await
            {
                RecvOutcome::Update(update) => {
                    if let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind {
                        assert!(
                            snapshot.durable_session_view.is_some(),
                            "must not publish current_session_id with empty durable: {:?}",
                            snapshot.current_session_id
                        );
                        panic!("reload limit must not emit SnapshotReady");
                    }
                }
                RecvOutcome::Timeout => break,
                RecvOutcome::Disconnected => panic!("disconnected after reload limit"),
            }
        }
        drop(service);
    })
    .await
    .expect("reload bound test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn switch_during_live_stream_keeps_snapshot_session_isolated() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let store = MemorySessionStore::new();
        seed_session(&store, "sess-a").await;
        seed_session(&store, "sess-b").await;
        let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);
        load_session(&client, "sess-a").await;
        emit_live(&runtime, "sess-a", "op-a", "turn-a", "stream-from-a");
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::OperationEvent(
                    philo_agent_service::FrontendOperationEvent::TextDelta { .. }
                )
            )
        })
        .await;

        load_session(&client, "sess-b").await;
        let request_id = accept_snapshot(&client);
        let update = recv_matching(&client, |update| {
            update.request_id == Some(request_id)
                && matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
        })
        .await;
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        assert_eq!(snapshot.current_session_id.as_deref(), Some("sess-b"));
        let durable = snapshot.durable_session_view.expect("B must have durable");
        assert_eq!(durable.session_id, "sess-b");
        assert!(
            !assistant_texts(&durable)
                .iter()
                .any(|text| text.contains("stream-from-a")),
            "B durable must not include A's live stream: {:?}",
            assistant_texts(&durable)
        );
        assert_ne!(
            snapshot.live.operation_id.as_deref(),
            Some("op-a"),
            "B snapshot must not carry A's live operation"
        );
        assert!(
            !snapshot.live.text.contains("stream-from-a"),
            "B snapshot live must not include A's stream: {:?}",
            snapshot.live.text
        );
        assert!(
            snapshot.live.operation_id.is_none()
                || snapshot.live.operation_id.as_deref() == Some("op-b"),
            "B live must be empty or belong to B: {:?}",
            snapshot.live.operation_id
        );
        drop(service);
    })
    .await
    .expect("switch-during-live isolation test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_snapshot_active_from_other_session_stays_out_of_current_live() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let store = MemorySessionStore::new();
        seed_session(&store, "sess-b").await;
        let (service, client, runtime) = start_test_service_with(FakeAssembler::new(), store);
        load_session(&client, "sess-b").await;
        runtime.set_snapshot(RuntimeSnapshot {
            epoch: RuntimeEpoch::new("epoch-1"),
            availability: AgentAvailability::Busy {
                operation_id: OperationId::new("op-a"),
            },
            queued: Vec::new(),
            active: Some(ActiveOperationSnapshot {
                operation_id: OperationId::new("op-a"),
                turn_id: philo_agent_runtime::TurnId::new("turn-a"),
                session_id: philo_agent_runtime::SessionId::new("sess-a"),
                phase: OperationPhase::PreparingTurn,
                started: true,
            }),
            maintenance: None,
            shutdown: ShutdownState::Running,
            last_settled: Vec::new(),
            runtime_revision: 4,
        });
        runtime.emit(RuntimeEvent::SubscriptionLagged { dropped: 1 });
        let update = recv_matching(&client, |update| {
            matches!(update.kind, FrontendUpdateKind::SnapshotReady(_))
        })
        .await;
        let FrontendUpdateKind::SnapshotReady(snapshot) = update.kind else {
            unreachable!();
        };
        assert_eq!(snapshot.current_session_id.as_deref(), Some("sess-b"));
        let durable = snapshot.durable_session_view.expect("B durable");
        assert_eq!(durable.session_id, "sess-b");
        assert_ne!(snapshot.live.operation_id.as_deref(), Some("op-a"));
        assert!(
            snapshot.live.is_empty(),
            "current-session snapshot must not install another session's active: {:?}",
            snapshot.live
        );
        drop(service);
    })
    .await
    .expect("runtime snapshot live isolation test timed out");
}

#[test]
fn frontend_submit_has_no_session_id() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/frontend/command.rs"
    ));
    let start = source
        .find("    Submit {")
        .expect("FrontendCommand::Submit");
    let rest = &source[start..];
    let end = rest.find("    CancelOperation").expect("next variant");
    let submit = &rest[..end];
    assert!(
        !submit.contains("session_id"),
        "frontend Submit must not carry session_id:\n{submit}"
    );
    assert!(
        !source.contains("service_state_revision"),
        "service_state_revision must stay deleted"
    );
}
