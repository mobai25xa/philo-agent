//! Host double used by state-machine and cross-layer tests.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use philo_agent_runtime::{
    AgentError, CompactionError, CompactionReport, OperationHandle, ReasoningEffort, RuntimeFuture,
    SessionId, ToolDefinition, UserMessage,
};
use philo_session::SessionContextView;
use tokio::sync::Notify;

use crate::api::confirmation::ConfirmationChannel;
use crate::api::host::{ConfigEntry, HostError, TuiHost};

type BlockingGate = Arc<(Mutex<bool>, Condvar)>;

pub(crate) struct FakeHost {
    prompts: Mutex<Vec<(SessionId, UserMessage)>>,
    confirmations: ConfirmationChannel,
    sessions: Mutex<Result<Vec<philo_session::SessionId>, String>>,
    views: Mutex<HashMap<String, SessionContextView>>,
    view_calls: Mutex<Vec<String>>,
    view_gates: Mutex<HashMap<String, Arc<Notify>>>,
    prompt_gate: Mutex<Option<Arc<Notify>>>,
    pending_prompts: Arc<AtomicUsize>,
    prompt_cancellations: Arc<AtomicUsize>,
    prompt_cancellation_notify: Arc<Notify>,
    model_error: Mutex<Option<String>>,
    model_calls: Mutex<Vec<String>>,
    model_gate: Mutex<Option<BlockingGate>>,
    reasoning: Mutex<Vec<ReasoningEffort>>,
    reasoning_error: Mutex<Option<String>>,
    config: Mutex<Vec<ConfigEntry>>,
    tools: Mutex<Vec<ToolDefinition>>,
    next_session_id: Mutex<String>,
    compaction_scripts: Mutex<VecDeque<FakeCompactionScript>>,
    compaction_calls: Mutex<Vec<SessionId>>,
    compaction_cancellations: Arc<AtomicUsize>,
}

enum FakeCompactionScript {
    Ready(Result<CompactionReport, CompactionError>),
    Pending,
}

struct PendingCompactionGuard(Arc<AtomicUsize>);

struct PendingPromptGuard {
    pending: Arc<AtomicUsize>,
    cancellations: Arc<AtomicUsize>,
    cancellation_notify: Arc<Notify>,
    completed: bool,
}

impl Drop for PendingCompactionGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for PendingPromptGuard {
    fn drop(&mut self) {
        self.pending.fetch_sub(1, Ordering::SeqCst);
        if !self.completed {
            self.cancellations.fetch_add(1, Ordering::SeqCst);
            self.cancellation_notify.notify_one();
        }
    }
}

impl Default for FakeHost {
    fn default() -> Self {
        Self {
            prompts: Mutex::default(),
            confirmations: ConfirmationChannel::default(),
            sessions: Mutex::new(Ok(vec![philo_session::SessionId::new("fake-session")])),
            views: Mutex::default(),
            view_calls: Mutex::default(),
            view_gates: Mutex::default(),
            prompt_gate: Mutex::default(),
            pending_prompts: Arc::new(AtomicUsize::new(0)),
            prompt_cancellations: Arc::new(AtomicUsize::new(0)),
            prompt_cancellation_notify: Arc::new(Notify::new()),
            model_error: Mutex::default(),
            model_calls: Mutex::default(),
            model_gate: Mutex::default(),
            reasoning: Mutex::default(),
            reasoning_error: Mutex::default(),
            config: Mutex::new(vec![ConfigEntry {
                key: "model".to_owned(),
                value: "fake".to_owned(),
                source: "default".to_owned(),
            }]),
            tools: Mutex::default(),
            next_session_id: Mutex::new("fake-new-session".to_owned()),
            compaction_scripts: Mutex::default(),
            compaction_calls: Mutex::default(),
            compaction_cancellations: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl FakeHost {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn prompt_count(&self) -> usize {
        self.prompts.lock().expect("fake host mutex").len()
    }

    pub(crate) fn confirmations(&self) -> ConfirmationChannel {
        self.confirmations.clone()
    }

    pub(crate) fn set_sessions(&self, ids: &[&str]) {
        *self.sessions.lock().expect("fake host mutex") = Ok(ids
            .iter()
            .map(|id| philo_session::SessionId::new(*id))
            .collect());
    }

    pub(crate) fn set_view(&self, id: &str, view: SessionContextView) {
        self.views
            .lock()
            .expect("fake host mutex")
            .insert(id.to_owned(), view);
    }

    pub(crate) fn view_calls(&self) -> Vec<String> {
        self.view_calls.lock().expect("fake host mutex").clone()
    }

    pub(crate) fn delay_view(&self, id: &str) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.view_gates
            .lock()
            .expect("fake host mutex")
            .insert(id.to_owned(), Arc::clone(&gate));
        gate
    }

    pub(crate) fn delay_prompts(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.prompt_gate.lock().expect("fake host mutex") = Some(Arc::clone(&gate));
        gate
    }

    pub(crate) fn resume_prompts(&self) {
        if let Some(gate) = self.prompt_gate.lock().expect("fake host mutex").take() {
            gate.notify_one();
        }
    }

    pub(crate) fn prompt_sessions(&self) -> Vec<String> {
        self.prompts
            .lock()
            .expect("fake host mutex")
            .iter()
            .map(|(session, _)| session.as_str().to_owned())
            .collect()
    }

    pub(crate) fn prompt_cancellations(&self) -> usize {
        self.prompt_cancellations.load(Ordering::SeqCst)
    }

    pub(crate) fn pending_prompts(&self) -> usize {
        self.pending_prompts.load(Ordering::SeqCst)
    }

    pub(crate) async fn wait_for_prompt_cancellation(&self) {
        if self.prompt_cancellations() == 0 {
            self.prompt_cancellation_notify.notified().await;
        }
    }

    pub(crate) fn fail_model(&self, message: &str) {
        *self.model_error.lock().expect("fake host mutex") = Some(message.to_owned());
    }

    pub(crate) fn delay_models(&self) {
        *self.model_gate.lock().expect("fake host mutex") =
            Some(Arc::new((Mutex::new(false), Condvar::new())));
    }

    pub(crate) fn resume_models(&self) {
        let Some(gate) = self.model_gate.lock().expect("fake host mutex").take() else {
            return;
        };
        let (ready, condition) = &*gate;
        *ready.lock().expect("fake model gate") = true;
        condition.notify_all();
    }

    pub(crate) fn model_calls(&self) -> Vec<String> {
        self.model_calls.lock().expect("fake host mutex").clone()
    }

    pub(crate) fn reasoning_calls(&self) -> Vec<ReasoningEffort> {
        self.reasoning.lock().expect("fake host mutex").clone()
    }

    pub(crate) fn set_config(&self, entries: Vec<ConfigEntry>) {
        *self.config.lock().expect("fake host mutex") = entries;
    }

    pub(crate) fn set_tools(&self, tools: Vec<ToolDefinition>) {
        *self.tools.lock().expect("fake host mutex") = tools;
    }

    pub(crate) fn set_next_session_id(&self, id: &str) {
        *self.next_session_id.lock().expect("fake host mutex") = id.to_owned();
    }

    pub(crate) fn enqueue_compaction(&self, result: Result<CompactionReport, CompactionError>) {
        self.compaction_scripts
            .lock()
            .expect("fake host mutex")
            .push_back(FakeCompactionScript::Ready(result));
    }

    pub(crate) fn enqueue_pending_compaction(&self) {
        self.compaction_scripts
            .lock()
            .expect("fake host mutex")
            .push_back(FakeCompactionScript::Pending);
    }

    pub(crate) fn compaction_calls(&self) -> Vec<SessionId> {
        self.compaction_calls
            .lock()
            .expect("fake host mutex")
            .clone()
    }

    pub(crate) fn compaction_cancellations(&self) -> usize {
        self.compaction_cancellations.load(Ordering::SeqCst)
    }
}

impl TuiHost for FakeHost {
    fn prompt<'a>(
        &'a self,
        session_id: SessionId,
        message: UserMessage,
    ) -> RuntimeFuture<'a, Result<OperationHandle, AgentError>> {
        self.prompts
            .lock()
            .expect("fake host mutex")
            .push((session_id, message));
        let gate = self.prompt_gate.lock().expect("fake host mutex").clone();
        let pending = Arc::clone(&self.pending_prompts);
        let cancellations = Arc::clone(&self.prompt_cancellations);
        let cancellation_notify = Arc::clone(&self.prompt_cancellation_notify);
        Box::pin(async move {
            if let Some(gate) = gate {
                pending.fetch_add(1, Ordering::SeqCst);
                let mut guard = PendingPromptGuard {
                    pending,
                    cancellations,
                    cancellation_notify,
                    completed: false,
                };
                gate.notified().await;
                guard.completed = true;
            }
            Err(AgentError::new("fake host has no runtime"))
        })
    }

    fn compact(
        &self,
        session_id: SessionId,
    ) -> RuntimeFuture<'static, Result<CompactionReport, CompactionError>> {
        self.compaction_calls
            .lock()
            .expect("fake host mutex")
            .push(session_id);
        let script = self
            .compaction_scripts
            .lock()
            .expect("fake host mutex")
            .pop_front()
            .unwrap_or(FakeCompactionScript::Ready(Ok(
                CompactionReport::NothingToCompact,
            )));
        match script {
            FakeCompactionScript::Ready(result) => Box::pin(async move { result }),
            FakeCompactionScript::Pending => {
                let cancellations = self.compaction_cancellations.clone();
                Box::pin(async move {
                    let _guard = PendingCompactionGuard(cancellations);
                    std::future::pending::<Result<CompactionReport, CompactionError>>().await
                })
            }
        }
    }

    fn list_sessions(&self) -> Result<Vec<philo_session::SessionId>, HostError> {
        self.sessions
            .lock()
            .expect("fake host mutex")
            .clone()
            .map_err(HostError::new)
    }

    fn context_view<'a>(
        &'a self,
        session_id: &'a philo_session::SessionId,
    ) -> RuntimeFuture<'a, Result<SessionContextView, HostError>> {
        self.view_calls
            .lock()
            .expect("fake host mutex")
            .push(session_id.as_str().to_owned());
        let view = self
            .views
            .lock()
            .expect("fake host mutex")
            .get(session_id.as_str())
            .cloned();
        let gate = self
            .view_gates
            .lock()
            .expect("fake host mutex")
            .get(session_id.as_str())
            .cloned();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            view.ok_or_else(|| HostError::new("fake host has no history for this session"))
        })
    }

    fn rebuild_model(&self, name: &str) -> Result<(), HostError> {
        self.model_calls
            .lock()
            .expect("fake host mutex")
            .push(name.to_owned());
        let gate = self.model_gate.lock().expect("fake host mutex").clone();
        if let Some(gate) = gate {
            let (ready, condition) = &*gate;
            let mut ready = ready.lock().expect("fake model gate");
            while !*ready {
                ready = condition.wait(ready).expect("fake model gate");
            }
        }
        match self.model_error.lock().expect("fake host mutex").clone() {
            Some(message) => Err(HostError::new(message)),
            None => Ok(()),
        }
    }

    fn set_reasoning(&self, effort: ReasoningEffort) -> Result<(), HostError> {
        match self
            .reasoning_error
            .lock()
            .expect("fake host mutex")
            .clone()
        {
            Some(message) => Err(HostError::new(message)),
            None => {
                self.reasoning.lock().expect("fake host mutex").push(effort);
                Ok(())
            }
        }
    }

    fn config_view(&self) -> Vec<ConfigEntry> {
        self.config.lock().expect("fake host mutex").clone()
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.lock().expect("fake host mutex").clone()
    }

    fn new_session_id(&self) -> String {
        self.next_session_id
            .lock()
            .expect("fake host mutex")
            .clone()
    }

    fn confirmations(&self) -> ConfirmationChannel {
        self.confirmations.clone()
    }
}
