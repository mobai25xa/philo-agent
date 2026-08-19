//! Track 07 frontend lease registration: supervisor lane, generation, Occupied/StaleLease.

use std::time::{Duration, Instant};

use philo_agent_service::testing::{
    abort_service_actor_and_wait, start_test_service, start_test_service_with_command_hold,
};
use philo_agent_service::{
    AttachError, CommandDispatch, CommandReject, ConfirmationDecision, ConfirmationRequest,
    DetachError, FRONTEND_COMMAND_CAP, FRONTEND_CONTROL_CAP, FrontendCommand, FrontendInstanceId,
    FrontendUpdate, FrontendUpdateKind, RecvOutcome, ServiceHealth, ShutdownMode,
};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

fn id(name: &str) -> FrontendInstanceId {
    FrontendInstanceId::new(name)
}

async fn recv_matching(
    client: &philo_agent_service::FrontendClient,
    mut pred: impl FnMut(&FrontendUpdate) -> bool,
) -> FrontendUpdate {
    let until = Instant::now() + Duration::from_secs(2);
    let mut seen = Vec::new();
    loop {
        match client
            .recv_until_async(Instant::now() + Duration::from_millis(50))
            .await
        {
            RecvOutcome::Update(update) if pred(&update) => return update,
            RecvOutcome::Update(update) => seen.push(format!("{:?}", update.kind)),
            RecvOutcome::Timeout if Instant::now() < until => continue,
            RecvOutcome::Timeout => panic!("timed out waiting for frontend update; seen={seen:?}"),
            RecvOutcome::Disconnected => {
                panic!("frontend disconnected while waiting; seen={seen:?}")
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_acks_when_caller_deadline_already_elapsed() {
    let (service, _client, _runtime) = start_test_service();
    let lease = service
        .attach_frontend(id("front-a"), Instant::now() - Duration::from_secs(1))
        .await
        .expect("reply grace must cover an in-flight attach ack");
    assert_eq!(lease.frontend_id(), &id("front-a"));
    service
        .detach_frontend(lease, Instant::now() - Duration::from_secs(1))
        .await
        .expect("reply grace must cover an in-flight detach ack");
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_acks_when_command_lane_is_full() {
    let (service, client, _runtime, hold) = start_test_service_with_command_hold();
    let mut enqueued = 0;
    let mut backpressured = 0;
    for _ in 0..FRONTEND_COMMAND_CAP + 4 {
        match client.try_command(FrontendCommand::ListSessions) {
            CommandDispatch::Enqueued(_) => enqueued += 1,
            CommandDispatch::Backpressured => backpressured += 1,
            CommandDispatch::Disconnected { lane } => panic!("disconnected: {lane}"),
        }
    }
    assert_eq!(enqueued, FRONTEND_COMMAND_CAP);
    assert!(backpressured > 0);

    let lease = service
        .attach_frontend(id("front-a"), deadline())
        .await
        .expect("attach must not compete with the command lane");
    assert_eq!(lease.frontend_id(), &id("front-a"));
    hold.release();
    service
        .detach_frontend(lease, deadline())
        .await
        .expect("detach");
}

#[tokio::test(flavor = "multi_thread")]
async fn second_frontend_gets_occupied() {
    let (service, client, _runtime) = start_test_service();
    let lease_a = service
        .attach_frontend(id("front-a"), deadline())
        .await
        .expect("attach A");
    let occupied = service
        .attach_frontend(id("front-b"), deadline())
        .await
        .expect_err("B must not replace A");
    assert_eq!(
        occupied,
        AttachError::FrontendOccupied {
            current: id("front-a")
        }
    );

    let gate = service.confirmation_gate();
    let decision = tokio::spawn(async move {
        gate.request(ConfirmationRequest {
            title: "q".into(),
            body: "b".into(),
        })
        .await
    });
    let requested = recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::ConfirmationRequested { .. }
        )
    })
    .await;
    let FrontendUpdateKind::ConfirmationRequested {
        confirmation_id, ..
    } = requested.kind
    else {
        unreachable!();
    };
    assert!(matches!(
        client.try_command(FrontendCommand::RespondConfirmation {
            confirmation_id,
            decision: ConfirmationDecision::Allow,
        }),
        CommandDispatch::Enqueued(_)
    ));
    assert_eq!(decision.await.unwrap(), ConfirmationDecision::Allow);
    service
        .detach_frontend(lease_a, deadline())
        .await
        .expect("detach A");
}

#[tokio::test(flavor = "multi_thread")]
async fn same_id_replay_already_attached() {
    let (service, _client, _runtime) = start_test_service();
    let lease = service
        .attach_frontend(id("front-a"), deadline())
        .await
        .expect("attach");
    assert_eq!(
        service
            .attach_frontend(id("front-a"), deadline())
            .await
            .expect_err("replay"),
        AttachError::AlreadyAttached
    );
    service
        .detach_frontend(lease, deadline())
        .await
        .expect("detach");
}

#[tokio::test(flavor = "multi_thread")]
async fn detach_denies_pending_confirmations() {
    let (service, client, _runtime) = start_test_service();
    let lease = service
        .attach_frontend(id("front-a"), deadline())
        .await
        .expect("attach");
    let gate = service.confirmation_gate();
    let decision = tokio::spawn(async move {
        gate.request(ConfirmationRequest {
            title: "q".into(),
            body: "b".into(),
        })
        .await
    });
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::ConfirmationRequested { .. }
        )
    })
    .await;
    let report = service
        .detach_frontend(lease, deadline())
        .await
        .expect("detach");
    assert_eq!(report.denied_confirmations, 1);
    assert_eq!(decision.await.unwrap(), ConfirmationDecision::Deny);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_lease_detach_does_not_clear_new_frontend() {
    let (service, client, _runtime) = start_test_service();
    let lease_a = service
        .attach_frontend(id("front-a"), deadline())
        .await
        .expect("attach A");
    let stale = lease_a.clone();
    service
        .detach_frontend(lease_a, deadline())
        .await
        .expect("detach A");
    let lease_b = service
        .attach_frontend(id("front-b"), deadline())
        .await
        .expect("attach B");

    let gate = service.confirmation_gate();
    let decision = tokio::spawn(async move {
        gate.request(ConfirmationRequest {
            title: "new".into(),
            body: "b".into(),
        })
        .await
    });
    let requested = recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::ConfirmationRequested { .. }
        )
    })
    .await;
    let FrontendUpdateKind::ConfirmationRequested {
        confirmation_id, ..
    } = requested.kind
    else {
        unreachable!();
    };

    assert_eq!(
        service
            .detach_frontend(stale, deadline())
            .await
            .expect_err("stale"),
        DetachError::StaleLease
    );
    assert!(matches!(
        client.try_command(FrontendCommand::RespondConfirmation {
            confirmation_id,
            decision: ConfirmationDecision::Allow,
        }),
        CommandDispatch::Enqueued(_)
    ));
    assert_eq!(decision.await.unwrap(), ConfirmationDecision::Allow);
    service
        .detach_frontend(lease_b, deadline())
        .await
        .expect("detach B");
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_confirmation_reply_is_unknown() {
    let (service, client, _runtime) = start_test_service();
    let lease_a = service
        .attach_frontend(id("front-a"), deadline())
        .await
        .expect("attach A");
    let gate = service.confirmation_gate();
    let decision = tokio::spawn(async move {
        gate.request(ConfirmationRequest {
            title: "old".into(),
            body: "b".into(),
        })
        .await
    });
    let requested = recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::ConfirmationRequested { .. }
        )
    })
    .await;
    let FrontendUpdateKind::ConfirmationRequested {
        confirmation_id, ..
    } = requested.kind
    else {
        unreachable!();
    };
    service
        .detach_frontend(lease_a, deadline())
        .await
        .expect("detach A");
    assert_eq!(decision.await.unwrap(), ConfirmationDecision::Deny);

    let lease_b = service
        .attach_frontend(id("front-b"), deadline())
        .await
        .expect("attach B");
    let request_id = match client.try_command(FrontendCommand::RespondConfirmation {
        confirmation_id,
        decision: ConfirmationDecision::Allow,
    }) {
        CommandDispatch::Enqueued(id) => id,
        other => panic!("{other:?}"),
    };
    recv_matching(&client, |update| {
        update.request_id == Some(request_id)
            && matches!(
                update.kind,
                FrontendUpdateKind::CommandRejected {
                    reason: CommandReject::UnknownConfirmation,
                }
            )
    })
    .await;
    service
        .detach_frontend(lease_b, deadline())
        .await
        .expect("detach B");
}

#[tokio::test(flavor = "multi_thread")]
async fn detach_after_service_abort_is_gone_or_disconnected() {
    let (service, _client, _runtime) = start_test_service();
    let lease = service
        .attach_frontend(id("front-a"), deadline())
        .await
        .expect("attach");
    abort_service_actor_and_wait(&service).await;
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        service.detach_frontend(lease, Instant::now() + Duration::from_millis(200)),
    )
    .await
    .expect("detach must not hang");
    assert!(
        matches!(
            result,
            Err(DetachError::ServiceGone)
                | Err(DetachError::Disconnected)
                | Err(DetachError::DeadlineExceeded)
        ),
        "{result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_after_service_abort_is_gone_or_disconnected() {
    let (service, _client, _runtime) = start_test_service();
    abort_service_actor_and_wait(&service).await;
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        service.attach_frontend(id("front-a"), Instant::now() + Duration::from_millis(200)),
    )
    .await
    .expect("attach must not hang");
    assert!(
        matches!(
            result,
            Err(AttachError::ServiceGone)
                | Err(AttachError::Disconnected)
                | Err(AttachError::DeadlineExceeded)
        ),
        "{result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_clears_lease_then_denies() {
    let (service, client, _runtime) = start_test_service();
    let _lease = service
        .attach_frontend(id("front-a"), deadline())
        .await
        .expect("attach");
    let gate = service.confirmation_gate();
    let decision = tokio::spawn(async move {
        gate.request(ConfirmationRequest {
            title: "q".into(),
            body: "b".into(),
        })
        .await
    });
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::ConfirmationRequested { .. }
        )
    })
    .await;
    assert!(matches!(
        service.request_shutdown(),
        CommandDispatch::Enqueued(_)
    ));
    recv_matching(&client, |update| {
        matches!(
            update.kind,
            FrontendUpdateKind::ServiceHealthChanged {
                health: ServiceHealth::ShuttingDown,
            }
        )
    })
    .await;
    assert_eq!(decision.await.unwrap(), ConfirmationDecision::Deny);
    let again = service.attach_frontend(id("front-b"), deadline()).await;
    assert!(
        matches!(
            again,
            Err(AttachError::Disconnected) | Err(AttachError::ServiceGone)
        ),
        "{again:?}"
    );
    service.join().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn supervisor_shutdown_clears_lease_and_denies_confirmations() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let (service, client, _runtime) = start_test_service();
        let _lease = service
            .attach_frontend(id("front-a"), deadline())
            .await
            .expect("attach");
        let gate = service.confirmation_gate();
        let decision = tokio::spawn(async move {
            gate.request(ConfirmationRequest {
                title: "q".into(),
                body: "b".into(),
            })
            .await
        });
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::ConfirmationRequested { .. }
            )
        })
        .await;
        service
            .shutdown_from_supervisor("test", deadline())
            .await
            .expect("supervisor shutdown");
        recv_matching(&client, |update| {
            matches!(
                update.kind,
                FrontendUpdateKind::ServiceHealthChanged {
                    health: ServiceHealth::ShuttingDown,
                }
            )
        })
        .await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), decision)
                .await
                .expect("confirmation deny must not hang")
                .unwrap(),
            ConfirmationDecision::Deny
        );
        let again = service.attach_frontend(id("front-b"), deadline()).await;
        assert!(
            matches!(
                again,
                Err(AttachError::Disconnected) | Err(AttachError::ServiceGone)
            ),
            "{again:?}"
        );
        service.join().await;
    })
    .await
    .expect("supervisor shutdown test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn confirmation_without_lease_is_denied() {
    let (service, _client, _runtime) = start_test_service();
    let gate = service.confirmation_gate();
    let decision = tokio::time::timeout(
        Duration::from_secs(1),
        gate.request(ConfirmationRequest {
            title: "q".into(),
            body: "b".into(),
        }),
    )
    .await
    .expect("auto-deny must not hang");
    assert_eq!(decision, ConfirmationDecision::Deny);
}

#[tokio::test(flavor = "multi_thread")]
async fn supervisor_shutdown_reaches_actor_when_control_lane_is_full() {
    let (service, client, runtime) = start_test_service();
    let mut backpressured = 0;
    for _ in 0..FRONTEND_CONTROL_CAP + 4 {
        match client.try_command(FrontendCommand::CancelOperation {
            operation_id: "held".into(),
        }) {
            CommandDispatch::Enqueued(_) => {}
            CommandDispatch::Backpressured => backpressured += 1,
            CommandDispatch::Disconnected { lane } => panic!("disconnected: {lane}"),
        }
    }
    assert!(backpressured > 0);
    service
        .shutdown_from_supervisor("test", deadline())
        .await
        .expect("supervisor shutdown must not use the control mailbox");
    tokio::time::timeout(Duration::from_secs(2), service.join())
        .await
        .expect("service join after supervisor shutdown");
    assert!(runtime.shutdown_calls() >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn supervisor_shutdown_with_elapsed_deadline_is_forced() {
    let (service, _client, runtime) = start_test_service();
    service
        .shutdown_from_supervisor("forced", Instant::now())
        .await
        .expect("elapsed deadline still sends shutdown");
    tokio::time::timeout(Duration::from_secs(2), service.join())
        .await
        .expect("join");
    assert_eq!(runtime.last_shutdown_mode(), Some(ShutdownMode::Forced));
}
