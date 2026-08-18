//! Wave 2G: durable session catalog via `SessionStore::list_sessions`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use philo_agent_service::testing::{FakeAssembler, start_test_service_with};
use philo_agent_service::{
    CommandDispatch, FrontendCommand, FrontendUpdate, FrontendUpdateKind, RecvOutcome,
};
use philo_session::{
    MemorySessionStore, OperationOutcome, SessionAssistantBlock, SessionEntryKind, SessionError,
    SessionFuture, SessionId, SessionRevision, SessionStore, SessionTransaction, SessionUserPart,
    TurnId, TurnOutcome,
};
use philo_session_jsonl::JsonlSessionStore;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CatalogFault {
    None,
    Busy,
    Unavailable,
}

#[derive(Clone)]
struct ControllableSessionStore {
    inner: Arc<MemorySessionStore>,
    fault: Arc<Mutex<CatalogFault>>,
}

impl ControllableSessionStore {
    fn new(inner: MemorySessionStore) -> Self {
        Self {
            inner: Arc::new(inner),
            fault: Arc::new(Mutex::new(CatalogFault::None)),
        }
    }

    fn set_fault(&self, fault: CatalogFault) {
        *self.fault.lock().expect("catalog fault") = fault;
    }
}

impl SessionStore for ControllableSessionStore {
    fn context_view<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<philo_session::SessionContextView, SessionError>> {
        self.inner.context_view(session_id)
    }

    fn commit<'a>(
        &'a self,
        transaction: SessionTransaction,
    ) -> SessionFuture<'a, Result<philo_session::SessionCommit, SessionError>> {
        self.inner.commit(transaction)
    }

    fn list_sessions(&self) -> SessionFuture<'_, Result<Vec<SessionId>, SessionError>> {
        match *self.fault.lock().expect("catalog fault") {
            CatalogFault::None => self.inner.list_sessions(),
            CatalogFault::Busy => {
                Box::pin(async { Err(SessionError::store_busy("catalog paused")) })
            }
            CatalogFault::Unavailable => {
                Box::pin(async { Err(SessionError::store_unavailable("catalog paused")) })
            }
        }
    }
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-service-catalog-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
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

async fn drain_briefly(client: &philo_agent_service::FrontendClient) -> Vec<FrontendUpdateKind> {
    let mut kinds = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(80);
    while Instant::now() < deadline {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(20))
            .await
        {
            RecvOutcome::Update(update) => kinds.push(update.kind),
            RecvOutcome::Timeout | RecvOutcome::Disconnected => break,
        }
    }
    kinds
}

async fn seed_session(store: &dyn SessionStore, session_id: &str) {
    let session_id = SessionId::new(session_id);
    store
        .commit(SessionTransaction::linear(
            session_id,
            SessionRevision::ZERO,
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: philo_session::OperationId::new("op-seed"),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: philo_session::OperationId::new("op-seed"),
                    turn_id: TurnId::new("turn-seed"),
                },
                SessionEntryKind::UserMessage {
                    turn_id: TurnId::new("turn-seed"),
                    parts: SessionUserPart::text_parts("hello from store"),
                },
                SessionEntryKind::AssistantMessage {
                    turn_id: TurnId::new("turn-seed"),
                    blocks: vec![SessionAssistantBlock::Text {
                        text: "durable answer".into(),
                    }],
                },
                SessionEntryKind::TurnTerminated {
                    turn_id: TurnId::new("turn-seed"),
                    outcome: TurnOutcome::Succeeded,
                },
                SessionEntryKind::OperationSettled {
                    operation_id: philo_session::OperationId::new("op-seed"),
                    outcome: OperationOutcome::Succeeded,
                },
            ],
        ))
        .await
        .unwrap();
}

fn list_sessions(
    client: &philo_agent_service::FrontendClient,
) -> philo_agent_service::FrontendRequestId {
    match client.try_command(FrontendCommand::ListSessions) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("list sessions {other:?}"),
    }
}

async fn recv_session_list(
    client: &philo_agent_service::FrontendClient,
    request_id: philo_agent_service::FrontendRequestId,
) -> Vec<String> {
    let update = recv_matching(client, |update| {
        update.request_id == Some(request_id)
            && matches!(
                update.kind,
                FrontendUpdateKind::SessionListLoaded { .. }
                    | FrontendUpdateKind::CommandRejected { .. }
            )
    })
    .await;
    match update.kind {
        FrontendUpdateKind::SessionListLoaded { session_ids } => session_ids,
        other => panic!("expected SessionListLoaded, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_store_lists_empty_without_creating_a_session() {
    let (service, client, _runtime) =
        start_test_service_with(FakeAssembler::new(), MemorySessionStore::new());
    let request_id = list_sessions(&client);
    let session_ids = recv_session_list(&client, request_id).await;
    assert!(session_ids.is_empty());
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_sessions_reads_durable_store_in_stable_order() {
    let store = MemorySessionStore::new();
    seed_session(&store, "sess-z").await;
    seed_session(&store, "sess-a").await;
    seed_session(&store, "sess-m").await;

    let (service, client, _runtime) = start_test_service_with(FakeAssembler::new(), store);
    let request_id = list_sessions(&client);
    let session_ids = recv_session_list(&client, request_id).await;
    assert_eq!(session_ids, ["sess-a", "sess-m", "sess-z"]);
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn uncommitted_current_session_appears_at_most_once() {
    let store = MemorySessionStore::new();
    seed_session(&store, "sess-z").await;
    seed_session(&store, "sess-a").await;
    let (service, client, _runtime) = start_test_service_with(FakeAssembler::new(), store);

    let first = match client.try_command(FrontendCommand::CreateSession) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("{other:?}"),
    };
    let first_loaded = recv_matching(&client, |update| {
        update.request_id == Some(first)
            && matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
    })
    .await;
    let FrontendUpdateKind::SessionLoaded {
        session_id: first_id,
        ..
    } = first_loaded.kind
    else {
        unreachable!();
    };

    let listed = recv_session_list(&client, list_sessions(&client)).await;
    assert_eq!(listed, ["sess-a", first_id.as_str(), "sess-z"]);

    let second = match client.try_command(FrontendCommand::CreateSession) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("{other:?}"),
    };
    let second_loaded = recv_matching(&client, |update| {
        update.request_id == Some(second)
            && matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
    })
    .await;
    let FrontendUpdateKind::SessionLoaded {
        session_id: second_id,
        ..
    } = second_loaded.kind
    else {
        unreachable!();
    };
    assert_ne!(first_id, second_id);

    let listed = recv_session_list(&client, list_sessions(&client)).await;
    assert_eq!(listed, ["sess-a", second_id.as_str(), "sess-z"]);
    assert_eq!(listed.iter().filter(|id| *id == &first_id).count(), 0);
    assert_eq!(listed.iter().filter(|id| *id == &second_id).count(), 1);
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn store_busy_rejects_instead_of_empty_success() {
    let store = MemorySessionStore::new();
    seed_session(&store, "sess-a").await;
    seed_session(&store, "sess-b").await;
    let store = ControllableSessionStore::new(store);
    store.set_fault(CatalogFault::Busy);
    let (service, client, _runtime) = start_test_service_with(FakeAssembler::new(), store.clone());

    let request_id = list_sessions(&client);
    let rejected = recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(update.kind, FrontendUpdateKind::CommandRejected { .. })
    })
    .await;
    match &rejected.kind {
        FrontendUpdateKind::CommandRejected { reason } => {
            assert!(reason.to_string().contains("busy"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    let extras = drain_briefly(&client).await;
    assert!(
        extras
            .iter()
            .all(|kind| !matches!(kind, FrontendUpdateKind::SessionListLoaded { .. })),
        "busy catalog must not emit an empty success: {extras:?}"
    );

    store.set_fault(CatalogFault::None);
    let session_ids = recv_session_list(&client, list_sessions(&client)).await;
    assert_eq!(session_ids, ["sess-a", "sess-b"]);
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn store_unavailable_rejects_instead_of_empty_success() {
    let store = ControllableSessionStore::new(MemorySessionStore::new());
    store.set_fault(CatalogFault::Unavailable);
    let (service, client, _runtime) = start_test_service_with(FakeAssembler::new(), store.clone());

    let request_id = list_sessions(&client);
    let rejected = recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(update.kind, FrontendUpdateKind::CommandRejected { .. })
    })
    .await;
    match &rejected.kind {
        FrontendUpdateKind::CommandRejected { reason } => {
            assert!(reason.to_string().contains("unavailable"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    let extras = drain_briefly(&client).await;
    assert!(
        extras
            .iter()
            .all(|kind| !matches!(kind, FrontendUpdateKind::SessionListLoaded { .. })),
        "unavailable catalog must not emit an empty success: {extras:?}"
    );
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn jsonl_restart_lists_all_durable_sessions() {
    let root = TempRoot::new();
    {
        let store = JsonlSessionStore::open(&root.path).expect("open seed store");
        seed_session(&store, "sess-z").await;
        seed_session(&store, "sess-a").await;
        seed_session(&store, "sess-m").await;
        store.shutdown().expect("release seed store");
    }

    let store = JsonlSessionStore::open(&root.path).expect("reopen after restart");
    let (service, client, _runtime) = start_test_service_with(FakeAssembler::new(), store);
    let session_ids = recv_session_list(&client, list_sessions(&client)).await;
    assert_eq!(session_ids, ["sess-a", "sess-m", "sess-z"]);
    drop(service);
}
