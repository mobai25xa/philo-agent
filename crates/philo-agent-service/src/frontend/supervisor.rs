//! Private supervisor-lane transport. Public command shapes stay in [`super::lease`].

use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use super::lease::{AttachError, DetachError, DetachReport, FrontendLease, SupervisorCommand};

/// Minimum time to wait for a supervisor ack after send succeeds.
/// Distinct from the CLI attach/detach handshake grace: this only covers the
/// in-flight reply after the envelope is already in the mailbox.
pub(crate) const SUPERVISOR_REPLY_GRACE: Duration = Duration::from_millis(200);

pub(crate) enum SupervisorReply {
    Attach(Result<FrontendLease, AttachError>),
    Detach(Result<DetachReport, DetachError>),
    Shutdown,
}

pub(crate) struct SupervisorEnvelope {
    pub command: SupervisorCommand,
    pub reply: oneshot::Sender<SupervisorReply>,
    pub deadline: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SupervisorLaneError {
    Backpressured,
    Disconnected,
    /// Send did not complete, or send completed but no ack arrived.
    /// After a successful send this is **uncertain**: the actor may already
    /// have applied attach/detach. Callers must not treat it as "not executed".
    DeadlineExceeded,
    ServiceGone,
}

pub(crate) async fn exchange_supervisor(
    tx: &mpsc::Sender<SupervisorEnvelope>,
    command: SupervisorCommand,
    deadline: Instant,
) -> Result<SupervisorReply, SupervisorLaneError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    let envelope = SupervisorEnvelope {
        command,
        reply: reply_tx,
        deadline,
    };

    if deadline <= Instant::now() {
        match tx.try_send(envelope) {
            Ok(()) => {
                return recv_supervisor_reply(reply_rx, SUPERVISOR_REPLY_GRACE).await;
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err(SupervisorLaneError::Backpressured);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(SupervisorLaneError::Disconnected);
            }
        }
    }

    let send_timeout = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(send_timeout, tx.send(envelope)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err(SupervisorLaneError::Disconnected),
        Err(_) => return Err(SupervisorLaneError::DeadlineExceeded),
    }

    recv_supervisor_reply(reply_rx, reply_timeout_after_send(deadline)).await
}

fn reply_timeout_after_send(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .max(SUPERVISOR_REPLY_GRACE)
}

async fn recv_supervisor_reply(
    reply_rx: oneshot::Receiver<SupervisorReply>,
    timeout: Duration,
) -> Result<SupervisorReply, SupervisorLaneError> {
    match tokio::time::timeout(timeout, reply_rx).await {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(_)) => Err(SupervisorLaneError::ServiceGone),
        Err(_) => Err(SupervisorLaneError::DeadlineExceeded),
    }
}

impl SupervisorLaneError {
    pub(crate) fn into_attach(self) -> AttachError {
        match self {
            Self::Backpressured => AttachError::Backpressured,
            Self::Disconnected => AttachError::Disconnected,
            Self::DeadlineExceeded => AttachError::DeadlineExceeded,
            Self::ServiceGone => AttachError::ServiceGone,
        }
    }

    pub(crate) fn into_detach(self) -> DetachError {
        match self {
            Self::Backpressured => DetachError::Backpressured,
            Self::Disconnected => DetachError::Disconnected,
            Self::DeadlineExceeded => DetachError::DeadlineExceeded,
            Self::ServiceGone => DetachError::ServiceGone,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::ids::FrontendInstanceId;

    use super::*;

    fn attach_cmd(id: &str) -> SupervisorCommand {
        SupervisorCommand::AttachFrontend {
            id: FrontendInstanceId::new(id),
        }
    }

    #[tokio::test]
    async fn exchange_times_out_when_lane_is_full() {
        let (tx, _rx) = mpsc::channel(1);
        let (reply, _reply_rx) = oneshot::channel();
        tx.try_send(SupervisorEnvelope {
            command: attach_cmd("held"),
            reply,
            deadline: Instant::now() + Duration::from_secs(1),
        })
        .expect("fill lane");
        let error = exchange_supervisor(
            &tx,
            attach_cmd("waiting"),
            Instant::now() + Duration::from_millis(40),
        )
        .await;
        assert_eq!(error.err(), Some(SupervisorLaneError::DeadlineExceeded));
    }

    #[tokio::test]
    async fn exchange_is_backpressured_when_deadline_elapsed_and_lane_is_full() {
        let (tx, _rx) = mpsc::channel(1);
        let (reply, _reply_rx) = oneshot::channel();
        tx.try_send(SupervisorEnvelope {
            command: attach_cmd("held"),
            reply,
            deadline: Instant::now() + Duration::from_secs(1),
        })
        .expect("fill lane");
        let error = exchange_supervisor(
            &tx,
            attach_cmd("late"),
            Instant::now() - Duration::from_secs(1),
        )
        .await;
        assert_eq!(error.err(), Some(SupervisorLaneError::Backpressured));
    }

    #[tokio::test]
    async fn exchange_reports_disconnected_when_lane_is_closed() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let error = exchange_supervisor(
            &tx,
            attach_cmd("gone"),
            Instant::now() + Duration::from_millis(40),
        )
        .await;
        assert_eq!(error.err(), Some(SupervisorLaneError::Disconnected));
    }

    #[tokio::test]
    async fn exchange_waits_for_reply_after_send_even_if_deadline_elapsed() {
        let (tx, mut rx) = mpsc::channel(1);
        let join = tokio::spawn(async move {
            exchange_supervisor(
                &tx,
                attach_cmd("late"),
                Instant::now() - Duration::from_secs(1),
            )
            .await
        });
        let envelope = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("recv timed out")
            .expect("send succeeded after the caller deadline");
        assert!(
            envelope.reply.send(SupervisorReply::Shutdown).is_ok(),
            "reply channel must still be open"
        );
        let reply = tokio::time::timeout(SUPERVISOR_REPLY_GRACE + Duration::from_millis(200), join)
            .await
            .expect("grace must wait for the in-flight ack")
            .expect("join");
        assert!(matches!(reply, Ok(SupervisorReply::Shutdown)));
    }

    #[tokio::test]
    async fn exchange_reports_uncertain_deadline_when_send_succeeds_but_reply_is_held() {
        let (tx, mut rx) = mpsc::channel(1);
        let join = tokio::spawn(async move {
            exchange_supervisor(
                &tx,
                attach_cmd("held-reply"),
                Instant::now() - Duration::from_secs(1),
            )
            .await
        });
        let envelope = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("recv timed out")
            .expect("send succeeded");
        let error = tokio::time::timeout(SUPERVISOR_REPLY_GRACE + Duration::from_millis(400), join)
            .await
            .expect("exchange must finish after reply grace")
            .expect("join");
        assert_eq!(error.err(), Some(SupervisorLaneError::DeadlineExceeded));
        assert!(matches!(
            envelope.command,
            SupervisorCommand::AttachFrontend { .. }
        ));
        // The envelope is still with the "actor"; applying it after the caller
        // timed out is exactly the uncertain-lease case.
        let _ = envelope.reply.send(SupervisorReply::Shutdown);
    }

    #[tokio::test]
    async fn exchange_reports_service_gone_when_held_envelope_is_dropped() {
        let (tx, mut rx) = mpsc::channel(1);
        let join = tokio::spawn(async move {
            exchange_supervisor(
                &tx,
                attach_cmd("dropped"),
                Instant::now() + Duration::from_secs(2),
            )
            .await
        });
        let envelope = rx.recv().await.expect("send succeeded");
        drop(envelope);
        let error = join.await.expect("join");
        assert_eq!(error.err(), Some(SupervisorLaneError::ServiceGone));
    }
}
