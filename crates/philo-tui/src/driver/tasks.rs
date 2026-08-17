//! Owned, cancellable work that must never block the terminal event loop.
//!
//! The driver deliberately keeps only constant-time control operations inline:
//! cloning/responding to the in-memory `ConfirmationChannel`, signalling
//! `OperationHandle::cancel`, and obtaining the already-owned compaction future.
//! Every Host read/mutation, prompt admission, file/media decode, and clipboard
//! access is registered here instead.

use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use philo_agent_runtime::{OperationHandle, SessionId as RuntimeSessionId, UserMessage, UserPart};
use tokio::task::JoinHandle;

use crate::api::host::TuiHost;
use crate::app::attachment::PendingAttachment;
use crate::app::effect::{HostRequest, HostRequest::*};
use crate::platform::clipboard::{self, ClipboardContent};

use super::host_effects::HostResult;
use super::media;

#[derive(Clone, Debug, PartialEq, Eq)]
enum TaskScope {
    SessionList,
    Preview(String),
    SessionChange,
    Model,
    Reasoning,
    Config,
    Status,
    Clipboard,
    Submission(u64),
}

struct PendingTask {
    scope: TaskScope,
    handle: JoinHandle<TaskCompletion>,
}

impl PendingTask {
    fn poll(&mut self, context: &mut Context<'_>) -> Poll<TaskCompletion> {
        match Pin::new(&mut self.handle).poll(context) {
            Poll::Ready(Ok(completion)) => Poll::Ready(completion),
            Poll::Ready(Err(error)) => Poll::Ready(TaskCompletion::Failed(format!(
                "background task failed: {error}"
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for PendingTask {
    fn drop(&mut self) {
        // Async host work is cancelled immediately. `spawn_blocking` work that
        // has already started may finish on its worker thread, but its result
        // is detached and can no longer mutate the App.
        self.handle.abort();
    }
}

struct SubmissionRequest {
    id: u64,
    session_id: Option<RuntimeSessionId>,
    text: String,
    attachments: Vec<PendingAttachment>,
    draft_generation: u64,
}

/// One task completion selected by the fair driver event pump.
pub(crate) enum TaskCompletion {
    Host(HostResult),
    Clipboard(Result<ClipboardContent, String>),
    Submission(SubmissionResult),
    Superseded,
    Failed(String),
}

/// Terminal result of one FIFO prompt-admission pipeline.
pub(crate) enum SubmissionResult {
    Accepted(OperationHandle),
    Rejected(String),
    MediaRefused {
        text: String,
        kept: Vec<PendingAttachment>,
        errors: Vec<String>,
        draft_generation: u64,
    },
}

/// Registry for independently polled host/media tasks. Replacement removes
/// the previous scope before a new task is registered, so late completions
/// have no path back into the App.
pub(crate) struct PendingTasks {
    host: Arc<dyn TuiHost>,
    pending: Vec<PendingTask>,
    submissions: VecDeque<SubmissionRequest>,
    active_submission: bool,
    next_submission_id: u64,
    poll_cursor: usize,
    session_change_active: bool,
    model_active: bool,
    queued_model: Option<String>,
    reasoning_active: bool,
    queued_reasoning: Option<philo_agent_runtime::ReasoningEffort>,
}

impl PendingTasks {
    pub(crate) fn new(host: Arc<dyn TuiHost>) -> Self {
        Self {
            host,
            pending: Vec::new(),
            submissions: VecDeque::new(),
            active_submission: false,
            next_submission_id: 1,
            poll_cursor: 0,
            session_change_active: false,
            model_active: false,
            queued_model: None,
            reasoning_active: false,
            queued_reasoning: None,
        }
    }

    /// Registers an owned host request. Confirmation responses are deliberately
    /// excluded: the in-memory channel response is constant-time and immediate.
    pub(crate) fn start_host(&mut self, request: HostRequest) {
        match request {
            NewSession => {
                self.cancel_session_navigation();
                self.session_change_active = true;
                let host = Arc::clone(&self.host);
                self.replace(
                    TaskScope::SessionChange,
                    tokio::task::spawn_blocking(move || {
                        TaskCompletion::Host(HostResult::NewSession(host.new_session_id()))
                    }),
                );
            }
            OpenSessions => {
                self.cancel_session_navigation();
                let host = Arc::clone(&self.host);
                self.replace(
                    TaskScope::SessionList,
                    tokio::task::spawn_blocking(move || {
                        TaskCompletion::Host(HostResult::OpenSessions(host.list_sessions()))
                    }),
                );
            }
            LoadPreview(id) => {
                let scope = TaskScope::Preview(id.as_str().to_owned());
                let host = Arc::clone(&self.host);
                self.replace(
                    scope,
                    tokio::spawn(async move {
                        let result = host.context_view(&id).await;
                        TaskCompletion::Host(HostResult::Preview { id, result })
                    }),
                );
            }
            SwitchSession(id) => {
                self.cancel_session_navigation();
                self.session_change_active = true;
                let host = Arc::clone(&self.host);
                self.replace(
                    TaskScope::SessionChange,
                    tokio::spawn(async move {
                        let result = host.context_view(&id).await;
                        TaskCompletion::Host(HostResult::SwitchSession { id, result })
                    }),
                );
            }
            RebuildModel(name) => {
                if self.model_active {
                    self.queued_model = Some(name);
                } else {
                    self.start_model(name);
                }
            }
            SetReasoning(effort) => {
                if self.reasoning_active {
                    self.queued_reasoning = Some(effort);
                } else {
                    self.start_reasoning(effort);
                }
            }
            ShowConfig => {
                let host = Arc::clone(&self.host);
                self.replace(
                    TaskScope::Config,
                    tokio::task::spawn_blocking(move || {
                        TaskCompletion::Host(HostResult::ShowConfig(host.config_view()))
                    }),
                );
            }
            ShowStatus => {
                let host = Arc::clone(&self.host);
                self.replace(
                    TaskScope::Status,
                    tokio::task::spawn_blocking(move || {
                        TaskCompletion::Host(HostResult::ShowStatus(host.tool_definitions()))
                    }),
                );
            }
            Respond(..) => unreachable!("confirmation responses are immediate"),
        }
    }

    /// Adds one user submission to the FIFO admission pipeline. Only its front
    /// performs media work and calls `prompt`, preserving user-message order.
    pub(crate) fn enqueue_submission(
        &mut self,
        session_id: RuntimeSessionId,
        text: String,
        attachments: Vec<PendingAttachment>,
        draft_generation: u64,
    ) {
        let id = self.next_submission_id;
        self.next_submission_id = self.next_submission_id.wrapping_add(1).max(1);
        self.submissions.push_back(SubmissionRequest {
            id,
            session_id: (!self.session_change_active).then_some(session_id),
            text,
            attachments,
            draft_generation,
        });
        self.start_next_submission();
    }

    pub(crate) fn start_clipboard(&mut self) {
        self.replace(
            TaskScope::Clipboard,
            tokio::task::spawn_blocking(|| TaskCompletion::Clipboard(clipboard::read())),
        );
    }

    pub(crate) fn submission_count(&self) -> usize {
        usize::from(self.active_submission) + self.submissions.len()
    }

    /// Assigns deferred submissions to the stable live session, then starts
    /// admission once session/model/reasoning mutations have settled.
    pub(crate) fn resume_submissions(&mut self, session_id: RuntimeSessionId) {
        if self.session_change_active {
            return;
        }
        for request in &mut self.submissions {
            if request.session_id.is_none() {
                request.session_id = Some(session_id.clone());
            }
        }
        self.start_next_submission();
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Cancels prompt admission when there is no Runtime handle to cancel.
    pub(crate) fn cancel_submissions(&mut self) {
        self.pending
            .retain(|task| !matches!(task.scope, TaskScope::Submission(_)));
        self.submissions.clear();
        self.active_submission = false;
    }

    /// Esc closes or supersedes UI-owned reads/rebuilds. Prompt admission has
    /// separate queue semantics and is cancelled by `Effect::CancelActive`.
    pub(crate) fn cancel_transient(&mut self) {
        // Started blocking mutations cannot be interrupted safely. Keep them
        // registered so their completion remains consistent with the host,
        // but discard not-yet-started replacements.
        self.queued_model = None;
        self.queued_reasoning = None;
        self.session_change_active = false;
        self.pending.retain(|task| {
            matches!(
                task.scope,
                TaskScope::Submission(_) | TaskScope::Model | TaskScope::Reasoning
            )
        });
    }

    pub(crate) async fn next_completion(&mut self) -> TaskCompletion {
        poll_fn(|context| self.poll_completion(context)).await
    }

    fn poll_completion(&mut self, context: &mut Context<'_>) -> Poll<TaskCompletion> {
        if self.pending.is_empty() {
            return Poll::Pending;
        }
        let len = self.pending.len();
        for offset in 0..len {
            let index = (self.poll_cursor + offset) % len;
            if let Poll::Ready(completion) = self.pending[index].poll(context) {
                let scope = self.pending[index].scope.clone();
                self.pending.remove(index);
                self.poll_cursor = if self.pending.is_empty() {
                    0
                } else {
                    index % self.pending.len()
                };
                match scope {
                    TaskScope::Submission(_) => {
                        self.active_submission = false;
                        self.start_next_submission();
                    }
                    TaskScope::Model => {
                        self.model_active = false;
                        if let Some(name) = self.queued_model.take() {
                            self.start_model(name);
                            return Poll::Ready(TaskCompletion::Superseded);
                        }
                    }
                    TaskScope::Reasoning => {
                        self.reasoning_active = false;
                        if let Some(effort) = self.queued_reasoning.take() {
                            self.start_reasoning(effort);
                            return Poll::Ready(TaskCompletion::Superseded);
                        }
                    }
                    TaskScope::SessionChange => self.session_change_active = false,
                    _ => {}
                }
                return Poll::Ready(completion);
            }
        }
        self.poll_cursor = (self.poll_cursor + 1) % len;
        Poll::Pending
    }

    fn start_next_submission(&mut self) {
        if self.active_submission
            || self.session_change_active
            || self.model_active
            || self.reasoning_active
        {
            return;
        }
        if self
            .submissions
            .front()
            .is_some_and(|request| request.session_id.is_none())
        {
            return;
        }
        let Some(request) = self.submissions.pop_front() else {
            return;
        };
        self.active_submission = true;
        let scope = TaskScope::Submission(request.id);
        let host = Arc::clone(&self.host);
        let handle = tokio::spawn(run_submission(host, request));
        self.pending.push(PendingTask { scope, handle });
    }

    fn replace(&mut self, scope: TaskScope, handle: JoinHandle<TaskCompletion>) {
        self.pending.retain(|task| task.scope != scope);
        self.pending.push(PendingTask { scope, handle });
    }

    fn start_model(&mut self, name: String) {
        self.model_active = true;
        let host = Arc::clone(&self.host);
        let handle = tokio::task::spawn_blocking(move || {
            let result = host.rebuild_model(&name);
            TaskCompletion::Host(HostResult::RebuildModel { name, result })
        });
        self.pending.push(PendingTask {
            scope: TaskScope::Model,
            handle,
        });
    }

    fn start_reasoning(&mut self, effort: philo_agent_runtime::ReasoningEffort) {
        self.reasoning_active = true;
        let host = Arc::clone(&self.host);
        let handle = tokio::task::spawn_blocking(move || {
            TaskCompletion::Host(HostResult::SetReasoning {
                effort,
                result: host.set_reasoning(effort),
            })
        });
        self.pending.push(PendingTask {
            scope: TaskScope::Reasoning,
            handle,
        });
    }

    fn cancel_session_navigation(&mut self) {
        self.pending.retain(|task| {
            !matches!(
                task.scope,
                TaskScope::SessionList | TaskScope::Preview(_) | TaskScope::SessionChange
            )
        });
        self.session_change_active = false;
    }
}

async fn run_submission(host: Arc<dyn TuiHost>, request: SubmissionRequest) -> TaskCompletion {
    let SubmissionRequest {
        session_id,
        text,
        attachments,
        draft_generation,
        ..
    } = request;
    let session_id = session_id.expect("submission starts only with a stable session");
    let resolved = match tokio::task::spawn_blocking(move || media::resolve(attachments)).await {
        Ok(resolved) => resolved,
        Err(error) => {
            return TaskCompletion::Submission(SubmissionResult::MediaRefused {
                text,
                kept: Vec::new(),
                errors: vec![format!("attachment task failed: {error}")],
                draft_generation,
            });
        }
    };
    if !resolved.errors.is_empty() {
        return TaskCompletion::Submission(SubmissionResult::MediaRefused {
            text,
            kept: resolved.kept,
            errors: resolved.errors,
            draft_generation,
        });
    }

    let mut parts = vec![UserPart::Text(text)];
    parts.extend(resolved.parts);
    let message = match UserMessage::from_parts(parts) {
        Ok(message) => message,
        Err(error) => {
            return TaskCompletion::Submission(SubmissionResult::Rejected(format!(
                "message rejected: {error:?}"
            )));
        }
    };
    match host.prompt(session_id, message).await {
        Ok(handle) => TaskCompletion::Submission(SubmissionResult::Accepted(handle)),
        Err(error) => TaskCompletion::Submission(SubmissionResult::Rejected(format!(
            "prompt rejected: {}",
            error.message()
        ))),
    }
}

pub(crate) fn task_error(
    message: impl Into<String>,
) -> Vec<crate::app::transcript::TranscriptLine> {
    vec![crate::app::transcript::TranscriptLine {
        kind: crate::app::transcript::LineKind::Error,
        text: format!("error: {}", message.into()),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::action::Action;
    use crate::app::effect::Effect;
    use crate::app::state::App;
    use crate::app::status::StatusData;
    use crate::app::transcript::InfoLevel;
    use crate::driver::host_effects;
    use crate::tests::support::{FakeHost, session_view};

    fn app() -> App {
        App::new(
            StatusData::new("model-a", "current", InfoLevel::Default),
            true,
        )
    }

    async fn yield_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..256 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert!(condition(), "condition did not become ready");
    }

    #[tokio::test]
    async fn slow_host_read_leaves_input_cancel_and_quit_runnable() {
        let host = FakeHost::new();
        host.set_view("slow", session_view("slow"));
        let gate = host.delay_view("slow");
        let dynamic: Arc<dyn TuiHost> = host.clone();
        let mut tasks = PendingTasks::new(dynamic);
        tasks.start_host(HostRequest::LoadPreview(philo_session::SessionId::new(
            "slow",
        )));
        yield_until(|| host.view_calls() == ["slow"]).await;

        let mut app = app();
        app.on_action(Action::InsertChar('x'));
        assert_eq!(app.input.text(), "x");
        tasks.cancel_transient();
        assert!(app.on_action(Action::Escape).is_empty());
        assert!(app.on_action(Action::CtrlC).is_empty());
        assert_eq!(app.on_action(Action::CtrlD), [Effect::Quit]);
        assert_eq!(tasks.pending_len(), 0);

        gate.notify_one();
    }

    #[tokio::test]
    async fn replacement_cancels_old_session_result_before_it_can_apply() {
        let host = FakeHost::new();
        host.set_view("old", session_view("old"));
        host.set_view("new", session_view("new"));
        let old_gate = host.delay_view("old");
        let dynamic: Arc<dyn TuiHost> = host.clone();
        let mut tasks = PendingTasks::new(dynamic);
        tasks.start_host(HostRequest::SwitchSession(philo_session::SessionId::new(
            "old",
        )));
        yield_until(|| host.view_calls() == ["old"]).await;

        tasks.start_host(HostRequest::SwitchSession(philo_session::SessionId::new(
            "new",
        )));
        let TaskCompletion::Host(result) = tasks.next_completion().await else {
            panic!("new session task must complete");
        };
        let mut app = app();
        host_effects::apply(&mut app, result);
        assert_eq!(app.status.session, "new");
        assert_eq!(tasks.pending_len(), 0);

        old_gate.notify_one();
        tokio::task::yield_now().await;
        assert_eq!(app.status.session, "new");
    }

    #[tokio::test]
    async fn submission_waits_for_session_switch_and_uses_final_target() {
        let host = FakeHost::new();
        host.set_view("new", session_view("new"));
        let gate = host.delay_view("new");
        let dynamic: Arc<dyn TuiHost> = host.clone();
        let mut tasks = PendingTasks::new(dynamic);
        tasks.start_host(HostRequest::SwitchSession(philo_session::SessionId::new(
            "new",
        )));
        yield_until(|| host.view_calls() == ["new"]).await;
        tasks.enqueue_submission(
            RuntimeSessionId::new("old"),
            "after switch".to_owned(),
            Vec::new(),
            1,
        );
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(host.prompt_count(), 0, "admission must wait for the switch");

        gate.notify_one();
        let TaskCompletion::Host(result) = tasks.next_completion().await else {
            panic!("session switch must complete");
        };
        let mut app = app();
        host_effects::apply(&mut app, result);
        tasks.resume_submissions(RuntimeSessionId::new(&app.status.session));
        assert!(matches!(
            tasks.next_completion().await,
            TaskCompletion::Submission(SubmissionResult::Rejected(_))
        ));
        assert_eq!(host.prompt_sessions(), ["new"]);
    }

    #[tokio::test]
    async fn prompt_admission_is_fifo_even_when_more_submissions_arrive() {
        let host = FakeHost::new();
        host.delay_prompts();
        let dynamic: Arc<dyn TuiHost> = host.clone();
        let mut tasks = PendingTasks::new(dynamic);
        tasks.enqueue_submission(
            RuntimeSessionId::new("s-1"),
            "first".to_owned(),
            Vec::new(),
            1,
        );
        tasks.enqueue_submission(
            RuntimeSessionId::new("s-2"),
            "second".to_owned(),
            Vec::new(),
            2,
        );

        yield_until(|| host.prompt_count() == 1).await;
        assert_eq!(host.prompt_sessions(), ["s-1"]);
        host.resume_prompts();
        assert!(matches!(
            tasks.next_completion().await,
            TaskCompletion::Submission(SubmissionResult::Rejected(_))
        ));
        assert!(matches!(
            tasks.next_completion().await,
            TaskCompletion::Submission(SubmissionResult::Rejected(_))
        ));
        assert_eq!(host.prompt_sessions(), ["s-1", "s-2"]);
        assert_eq!(tasks.submission_count(), 0);
    }

    #[tokio::test]
    async fn blocking_model_replacements_run_serially_and_latest_wins() {
        let host = FakeHost::new();
        host.delay_models();
        let dynamic: Arc<dyn TuiHost> = host.clone();
        let mut tasks = PendingTasks::new(dynamic);
        tasks.start_host(HostRequest::RebuildModel("old".to_owned()));
        yield_until(|| host.model_calls() == ["old"]).await;

        tasks.start_host(HostRequest::RebuildModel("new".to_owned()));
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(host.model_calls(), ["old"], "mutations must not overlap");

        host.resume_models();
        assert!(matches!(
            tasks.next_completion().await,
            TaskCompletion::Superseded
        ));
        let TaskCompletion::Host(result) = tasks.next_completion().await else {
            panic!("latest model result must complete");
        };
        let mut app = app();
        host_effects::apply(&mut app, result);
        assert_eq!(host.model_calls(), ["old", "new"]);
        assert_eq!(app.status.model, "new");
    }

    #[tokio::test]
    async fn cancelling_pending_prompt_drops_its_future_without_terminal_fact() {
        let host = FakeHost::new();
        host.delay_prompts();
        let dynamic: Arc<dyn TuiHost> = host.clone();
        let mut tasks = PendingTasks::new(dynamic);
        tasks.enqueue_submission(
            RuntimeSessionId::new("s"),
            "hello".to_owned(),
            Vec::new(),
            1,
        );
        yield_until(|| host.pending_prompts() == 1).await;

        tasks.cancel_submissions();
        host.wait_for_prompt_cancellation().await;
        assert_eq!(host.prompt_cancellations(), 1);
        assert_eq!(tasks.submission_count(), 0);
        assert_eq!(tasks.pending_len(), 0);
    }

    #[test]
    fn stale_media_failure_does_not_replace_newer_draft() {
        let mut app = app();
        app.on_action(Action::InsertChar('a'));
        let effects = app.on_action(Action::Submit);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Append(_), Effect::Submit { .. }]
        ));
        let submitted_generation = app.draft_generation();
        app.on_action(Action::InsertChar('n'));

        assert!(!app.restore_draft_if_current(submitted_generation, "a", Vec::new()));
        assert_eq!(app.input.text(), "n");
    }
}
