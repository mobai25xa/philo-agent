use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use philo_session::{
    MemorySessionStore, SessionCommit, SessionContextView, SessionError, SessionFuture, SessionId,
    SessionStore, SessionTransaction,
};

use super::gate::Gate;

/// SessionStore wrapper that suspends one numbered read or commit on a gate,
/// creating deterministic windows for cancellation tests.
pub struct GatedSessionStore {
    inner: Arc<dyn SessionStore>,
    context_gate: Option<(usize, Gate)>,
    commit_gate: Option<(usize, Gate)>,
    context_reads: AtomicUsize,
    commits: AtomicUsize,
}

impl GatedSessionStore {
    pub fn memory() -> Self {
        Self {
            inner: Arc::new(MemorySessionStore::new()),
            context_gate: None,
            commit_gate: None,
            context_reads: AtomicUsize::new(0),
            commits: AtomicUsize::new(0),
        }
    }

    /// Suspends the `number`-th context read (1-based) until the gate opens.
    pub fn gate_context_read_at(mut self, number: usize, gate: &Gate) -> Self {
        self.context_gate = Some((number, gate.clone()));
        self
    }

    /// Suspends the `number`-th commit (1-based) until the gate opens.
    pub fn gate_commit_at(mut self, number: usize, gate: &Gate) -> Self {
        self.commit_gate = Some((number, gate.clone()));
        self
    }
}

impl SessionStore for GatedSessionStore {
    fn context_view<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<SessionContextView, SessionError>> {
        let read_number = self.context_reads.fetch_add(1, Ordering::Relaxed) + 1;
        let gate = self
            .context_gate
            .as_ref()
            .filter(|(number, _)| *number == read_number)
            .map(|(_, gate)| gate.clone());
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.wait().await;
            }
            self.inner.context_view(session_id).await
        })
    }

    fn commit<'a>(
        &'a self,
        transaction: SessionTransaction,
    ) -> SessionFuture<'a, Result<SessionCommit, SessionError>> {
        let commit_number = self.commits.fetch_add(1, Ordering::Relaxed) + 1;
        let gate = self
            .commit_gate
            .as_ref()
            .filter(|(number, _)| *number == commit_number)
            .map(|(_, gate)| gate.clone());
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.wait().await;
            }
            self.inner.commit(transaction).await
        })
    }
}
