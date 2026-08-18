//! Wave 2F SnapshotFence: settlement/view races, supersession, revision retry, resync.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use philo_agent_runtime::{
    AgentAvailability, AgentEvent, ModelCallId, OperationId, OperationStatus, RuntimeEpoch,
    RuntimeSnapshot, SettlementDurability, ShutdownState,
};
use philo_agent_service::testing::{FakeAssembler, start_test_service, start_test_service_with};
use philo_agent_service::{
    CommandDispatch, FrontendAssistantBlock, FrontendCommand, FrontendContextMessage,
    FrontendRevision, FrontendUpdate, FrontendUpdateKind, RecvOutcome, RuntimeEvent,
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
}

impl ScriptedStore {
    fn new(inner: Arc<MemorySessionStore>) -> Self {
        Self {
            inner,
            prepend: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn prepend(&self, view: SessionContextView) {
        self.prepend.lock().expect("scripted store").push_back(view);
    }
}

impl SessionStore for ScriptedStore {
    fn context_view<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<SessionContextView, SessionError>> {
        Box::pin(async move {
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

    for index in 0..200 {
        runtime.emit_agent(AgentEvent::TextDelta {
            delta: format!("y{index}"),
        });
    }
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
