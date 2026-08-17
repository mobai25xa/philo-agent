//! Pending confirmation map. Replaces `ConfirmationChannel`.

use std::collections::HashMap;
use std::future::Future;

use tokio::sync::{mpsc, oneshot};

use crate::bounds::CONFIRMATION_MAP_CAP;
use crate::frontend::command::ConfirmationDecision;
use crate::frontend::snapshot::PendingConfirmationView;

/// One approval question shown to the frontend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmationRequest {
    /// Short title.
    pub title: String,
    /// Body.
    pub body: String,
}

/// Handle used by approval decorators to ask the service a question.
#[derive(Clone, Debug)]
pub struct ConfirmationGate {
    tx: mpsc::Sender<ConfirmationSubmit>,
}

impl ConfirmationGate {
    pub(crate) fn new(tx: mpsc::Sender<ConfirmationSubmit>) -> Self {
        Self { tx }
    }

    /// Submits a question and resolves with the user's decision.
    ///
    /// A full or closed map auto-denies. Dropping the frontend, settling the
    /// owning operation, or shutting down the service also denies.
    pub fn request(
        &self,
        request: ConfirmationRequest,
    ) -> impl Future<Output = ConfirmationDecision> + Send + 'static {
        self.request_for_operation(request, None)
    }

    /// Same as [`Self::request`], scoped to an operation for auto-deny on settle.
    pub fn request_for_operation(
        &self,
        request: ConfirmationRequest,
        operation_id: Option<String>,
    ) -> impl Future<Output = ConfirmationDecision> + Send + 'static {
        let tx = self.tx.clone();
        async move {
            let (reply, rx) = oneshot::channel();
            match tx.try_send(ConfirmationSubmit {
                request,
                operation_id,
                reply,
            }) {
                Ok(()) => rx.await.unwrap_or(ConfirmationDecision::Deny),
                Err(_) => ConfirmationDecision::Deny,
            }
        }
    }
}

pub(crate) struct ConfirmationSubmit {
    pub request: ConfirmationRequest,
    pub operation_id: Option<String>,
    pub reply: oneshot::Sender<ConfirmationDecision>,
}

struct PendingConfirmation {
    request: ConfirmationRequest,
    operation_id: Option<String>,
    reply: oneshot::Sender<ConfirmationDecision>,
}

/// Service-owned pending set.
pub(crate) struct ConfirmationMap {
    next_id: u64,
    pending: HashMap<u64, PendingConfirmation>,
}

impl ConfirmationMap {
    pub(crate) fn new() -> Self {
        Self {
            next_id: 0,
            pending: HashMap::new(),
        }
    }

    pub(crate) fn insert(
        &mut self,
        submit: ConfirmationSubmit,
    ) -> Result<(u64, ConfirmationRequest), ConfirmationDecision> {
        if self.pending.len() >= CONFIRMATION_MAP_CAP {
            let _ = submit.reply.send(ConfirmationDecision::Deny);
            return Err(ConfirmationDecision::Deny);
        }
        self.next_id += 1;
        let id = self.next_id;
        let request = submit.request.clone();
        self.pending.insert(
            id,
            PendingConfirmation {
                request: submit.request,
                operation_id: submit.operation_id,
                reply: submit.reply,
            },
        );
        Ok((id, request))
    }

    pub(crate) fn respond(
        &mut self,
        id: u64,
        decision: ConfirmationDecision,
    ) -> Option<ConfirmationDecision> {
        let pending = self.pending.remove(&id)?;
        let _ = pending.reply.send(decision);
        Some(decision)
    }

    pub(crate) fn deny_for_operation(&mut self, operation_id: &str) -> Vec<u64> {
        let ids: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.operation_id.as_deref() == Some(operation_id))
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            if let Some(pending) = self.pending.remove(id) {
                let _ = pending.reply.send(ConfirmationDecision::Deny);
            }
        }
        ids
    }

    pub(crate) fn deny_all(&mut self) -> Vec<u64> {
        let ids: Vec<u64> = self.pending.keys().copied().collect();
        for id in &ids {
            if let Some(pending) = self.pending.remove(id) {
                let _ = pending.reply.send(ConfirmationDecision::Deny);
            }
        }
        ids
    }

    pub(crate) fn views(&self) -> Vec<PendingConfirmationView> {
        let mut views: Vec<PendingConfirmationView> = self
            .pending
            .iter()
            .map(|(id, pending)| PendingConfirmationView {
                confirmation_id: *id,
                title: pending.request.title.clone(),
                body: pending.request.body.clone(),
                operation_id: pending.operation_id.clone(),
            })
            .collect();
        views.sort_by_key(|view| view.confirmation_id);
        views
    }
}

/// Shared gate constructor used by [`crate::AgentService`].
pub(crate) fn gate_pair() -> (ConfirmationGate, mpsc::Receiver<ConfirmationSubmit>) {
    let (tx, rx) = mpsc::channel(CONFIRMATION_MAP_CAP);
    (ConfirmationGate::new(tx), rx)
}

/// Test helper: a gate that is already disconnected (every request denies).
#[allow(dead_code)]
pub fn disconnected_gate() -> ConfirmationGate {
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    ConfirmationGate::new(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_auto_denies_without_growing() {
        let mut map = ConfirmationMap::new();
        for index in 0..CONFIRMATION_MAP_CAP {
            let (tx, _rx) = oneshot::channel();
            map.insert(ConfirmationSubmit {
                request: ConfirmationRequest {
                    title: format!("q{index}"),
                    body: String::new(),
                },
                operation_id: None,
                reply: tx,
            })
            .expect("fits");
        }
        let (tx, rx) = oneshot::channel();
        let overflow = map.insert(ConfirmationSubmit {
            request: ConfirmationRequest {
                title: "overflow".into(),
                body: String::new(),
            },
            operation_id: None,
            reply: tx,
        });
        assert_eq!(overflow, Err(ConfirmationDecision::Deny));
        assert_eq!(map.views().len(), CONFIRMATION_MAP_CAP);
        assert_eq!(rx.blocking_recv(), Ok(ConfirmationDecision::Deny));
    }

    #[test]
    fn settle_denies_only_matching_operation() {
        let mut map = ConfirmationMap::new();
        let (tx_a, rx_a) = oneshot::channel();
        let (id_a, _) = map
            .insert(ConfirmationSubmit {
                request: ConfirmationRequest {
                    title: "a".into(),
                    body: String::new(),
                },
                operation_id: Some("op-1".into()),
                reply: tx_a,
            })
            .unwrap();
        let (tx_b, mut rx_b) = oneshot::channel();
        map.insert(ConfirmationSubmit {
            request: ConfirmationRequest {
                title: "b".into(),
                body: String::new(),
            },
            operation_id: Some("op-2".into()),
            reply: tx_b,
        })
        .unwrap();
        let denied = map.deny_for_operation("op-1");
        assert_eq!(denied, vec![id_a]);
        assert_eq!(rx_a.blocking_recv(), Ok(ConfirmationDecision::Deny));
        assert!(rx_b.try_recv().is_err());
        assert_eq!(map.views().len(), 1);
    }
}
