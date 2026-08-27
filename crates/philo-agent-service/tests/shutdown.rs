//! JoinSet child supervision: saturation, terminal results, and shutdown join.

use std::time::{Duration, Instant};

use philo_agent_service::testing::{FakeAssembler, start_test_service, start_test_service_with};
use philo_agent_service::{
    CommandDispatch, FrontendCommand, FrontendUpdate, FrontendUpdateKind, RecvOutcome,
    STORE_COMMAND_CAP,
};
use philo_session::MemorySessionStore;

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

async fn drain_updates(client: &philo_agent_service::FrontendClient) {
    while let RecvOutcome::Update(_) = client
        .recv_until_async(Instant::now() + Duration::from_millis(20))
        .await
    {}
}

async fn load_session(client: &philo_agent_service::FrontendClient) {
    assert!(matches!(
        client.try_command(FrontendCommand::LoadSession {
            session_id: "sess-cap".into(),
        }),
        CommandDispatch::Enqueued(_)
    ));
    recv_matching(client, |update| {
        matches!(update.kind, FrontendUpdateKind::SessionLoaded { .. })
    })
    .await;
}

async fn submit_until(client: &philo_agent_service::FrontendClient, count: usize) {
    let mut accepted = 0usize;
    while accepted < count {
        match client.try_command(FrontendCommand::Submit {
            draft: format!("hello-{accepted}"),
            attachments: Vec::new(),
        }) {
            CommandDispatch::Enqueued(_) => accepted += 1,
            CommandDispatch::Backpressured => tokio::task::yield_now().await,
            CommandDispatch::Disconnected { .. } => panic!("command lane closed while submitting"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn join_completes_after_64_children_and_shutdown() {
    let (service, client, runtime) = start_test_service();
    load_session(&client).await;
    let hold = runtime.hold_children();

    submit_until(&client, STORE_COMMAND_CAP).await;
    runtime.wait_child_started(STORE_COMMAND_CAP as u64).await;
    drain_updates(&client).await;

    let overflow = match client.try_command(FrontendCommand::Submit {
        draft: "too many".into(),
        attachments: Vec::new(),
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("overflow enqueue {other:?}"),
    };
    let rejected = recv_matching(&client, |update| {
        update.request_id == Some(overflow)
            && matches!(update.kind, FrontendUpdateKind::CommandRejected { .. })
    })
    .await;
    match &rejected.kind {
        FrontendUpdateKind::CommandRejected { reason } => {
            assert!(
                reason.to_string().contains("capacity"),
                "expected capacity rejection, got {reason}"
            );
        }
        other => panic!("{other:?}"),
    }

    assert!(matches!(
        service.request_shutdown(),
        CommandDispatch::Enqueued(_)
    ));
    runtime
        .wait_child_started((STORE_COMMAND_CAP as u64) + 1)
        .await;
    assert!(matches!(
        service.request_shutdown(),
        CommandDispatch::Enqueued(_)
    ));
    tokio::task::yield_now().await;

    hold.release();
    tokio::time::timeout(Duration::from_secs(2), service.join())
        .await
        .expect("service join must finish after children and shutdown");
    assert_eq!(runtime.shutdown_calls(), 1);
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_and_install_terminal_updates_are_not_dropped() {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let assembler = FakeAssembler::new().with_hold(rx);
    let assemble_started = assembler.clone();
    let (service, client, runtime) = start_test_service_with(assembler, MemorySessionStore::new());
    load_session(&client).await;
    let runtime_hold = runtime.hold_children();

    let install_id = match client.try_command(FrontendCommand::InstallModel {
        name: "fast".into(),
        effort: None,
        }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("install {other:?}"),
    };
    let cancel_id = match client.try_command(FrontendCommand::CancelOperation {
        operation_id: "op-1".into(),
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("cancel {other:?}"),
    };

    assemble_started.wait_started(1).await;
    runtime.wait_child_started(1).await;
    submit_until(&client, 8).await;
    runtime.wait_child_started(9).await;

    let _ = tx.send(true);
    runtime_hold.release();

    let install = recv_matching(&client, |update| {
        update.request_id == Some(install_id)
            && matches!(update.kind, FrontendUpdateKind::GenerationInstalled { .. })
    })
    .await;
    assert!(matches!(
        install.kind,
        FrontendUpdateKind::GenerationInstalled { .. }
    ));

    let cancel = recv_matching(&client, |update| {
        update.request_id == Some(cancel_id)
            && matches!(update.kind, FrontendUpdateKind::CommandAccepted)
    })
    .await;
    assert_eq!(cancel.request_id, Some(cancel_id));

    assert!(matches!(
        service.request_shutdown(),
        CommandDispatch::Enqueued(_)
    ));
    tokio::time::timeout(Duration::from_secs(2), service.join())
        .await
        .expect("join after cancel/install");
    drop(client);
}
