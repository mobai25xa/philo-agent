use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use philo_session::{
    MemorySessionStore, SessionCommit, SessionContextView, SessionError, SessionFuture, SessionId,
    SessionStore, SessionTransaction,
};

/// Failure policy for the test SessionStore wrapper.
#[derive(Clone, Debug, Default)]
pub struct FailurePlan {
    context_read_at: Option<usize>,
    commit_at: Option<usize>,
    persistent: bool,
}

impl FailurePlan {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn context_read_at(number: usize) -> Self {
        Self {
            context_read_at: Some(number),
            ..Self::default()
        }
    }

    pub fn commit_at(number: usize) -> Self {
        Self {
            commit_at: Some(number),
            ..Self::default()
        }
    }

    pub fn persistent_commit_at(number: usize) -> Self {
        Self {
            commit_at: Some(number),
            persistent: true,
            ..Self::default()
        }
    }
}

/// SessionStore wrapper that injects deterministic read/commit failures.
pub struct FailingSessionStore {
    inner: Arc<dyn SessionStore>,
    plan: FailurePlan,
    context_reads: AtomicUsize,
    commits: AtomicUsize,
}

impl FailingSessionStore {
    pub fn around(inner: Arc<dyn SessionStore>, plan: FailurePlan) -> Self {
        Self {
            inner,
            plan,
            context_reads: AtomicUsize::new(0),
            commits: AtomicUsize::new(0),
        }
    }

    pub fn memory(plan: FailurePlan) -> Self {
        Self::around(Arc::new(MemorySessionStore::new()), plan)
    }

    pub fn context_read_count(&self) -> usize {
        self.context_reads.load(Ordering::Relaxed)
    }

    pub fn commit_count(&self) -> usize {
        self.commits.load(Ordering::Relaxed)
    }
}

impl SessionStore for FailingSessionStore {
    fn context_view<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<SessionContextView, SessionError>> {
        let read_number = self.context_reads.fetch_add(1, Ordering::Relaxed) + 1;
        if self.plan.context_read_at == Some(read_number) {
            return Box::pin(async {
                Err(SessionError::store_unavailable("scripted read failure"))
            });
        }
        self.inner.context_view(session_id)
    }

    fn commit<'a>(
        &'a self,
        transaction: SessionTransaction,
    ) -> SessionFuture<'a, Result<SessionCommit, SessionError>> {
        let commit_number = self.commits.fetch_add(1, Ordering::Relaxed) + 1;
        let should_fail = self.plan.commit_at.is_some_and(|number| {
            commit_number >= number && (self.plan.persistent || commit_number == number)
        });
        if should_fail {
            return Box::pin(async {
                Err(SessionError::store_unavailable("scripted commit failure"))
            });
        }
        self.inner.commit(transaction)
    }

    fn list_sessions(&self) -> SessionFuture<'_, Result<Vec<SessionId>, SessionError>> {
        self.inner.list_sessions()
    }
}
