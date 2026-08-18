//! Frontend-local workers: clipboard and image decode.
//!
//! Session, model, compaction, and prompt admission all go through
//! `FrontendCommand`. This registry must never own Runtime work.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll};

use philo_agent_service::FrontendAttachment;
use tokio::task::JoinHandle;

use crate::app::attachment::PendingAttachment;
use crate::platform::clipboard::{self, ClipboardContent};

use super::media;

#[derive(Clone, Debug, PartialEq, Eq)]
enum TaskScope {
    Clipboard,
    ClipboardWrite,
    Media(u64),
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
        self.handle.abort();
    }
}

struct MediaRequest {
    id: u64,
    intent_id: u64,
    text: String,
    attachments: Vec<PendingAttachment>,
}

/// One task completion selected by the fair driver event pump.
#[derive(Debug)]
pub(crate) enum TaskCompletion {
    Clipboard(Result<ClipboardContent, String>),
    ClipboardWrite(Result<(), String>),
    Media(MediaResult),
    Failed(String),
}

/// Terminal result of one FIFO media-decode pipeline.
#[derive(Debug)]
pub(crate) enum MediaResult {
    Ready {
        intent_id: u64,
        draft: String,
        attachments: Vec<FrontendAttachment>,
    },
    Refused {
        intent_id: u64,
        kept: Vec<PendingAttachment>,
        errors: Vec<String>,
    },
}

/// Registry for independently polled frontend-local tasks.
pub(crate) struct PendingTasks {
    pending: Vec<PendingTask>,
    media: VecDeque<MediaRequest>,
    media_active: bool,
    next_media_id: u64,
    poll_cursor: usize,
}

impl PendingTasks {
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
            media: VecDeque::new(),
            media_active: false,
            next_media_id: 1,
            poll_cursor: 0,
        }
    }

    pub(crate) fn enqueue_media(
        &mut self,
        intent_id: u64,
        text: String,
        attachments: Vec<PendingAttachment>,
    ) {
        let id = self.next_media_id;
        self.next_media_id = self.next_media_id.wrapping_add(1).max(1);
        self.media.push_back(MediaRequest {
            id,
            intent_id,
            text,
            attachments,
        });
        self.start_next_media();
    }

    pub(crate) fn start_clipboard(&mut self) {
        self.replace(
            TaskScope::Clipboard,
            tokio::task::spawn_blocking(|| TaskCompletion::Clipboard(clipboard::read())),
        );
    }

    pub(crate) fn start_clipboard_write(&mut self, text: String) {
        self.replace(
            TaskScope::ClipboardWrite,
            tokio::task::spawn_blocking(move || {
                TaskCompletion::ClipboardWrite(clipboard::write_text(text))
            }),
        );
    }

    pub(crate) fn cancel_media(&mut self) {
        self.pending
            .retain(|task| !matches!(task.scope, TaskScope::Media(_)));
        self.media.clear();
        self.media_active = false;
    }

    pub(crate) fn cancel_transient(&mut self) {
        self.pending
            .retain(|task| !matches!(task.scope, TaskScope::Clipboard | TaskScope::ClipboardWrite));
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
                if matches!(scope, TaskScope::Media(_)) {
                    self.media_active = false;
                    self.start_next_media();
                }
                return Poll::Ready(completion);
            }
        }
        self.poll_cursor = (self.poll_cursor + 1) % len;
        Poll::Pending
    }

    fn start_next_media(&mut self) {
        if self.media_active {
            return;
        }
        let Some(request) = self.media.pop_front() else {
            return;
        };
        self.media_active = true;
        let scope = TaskScope::Media(request.id);
        let handle = tokio::task::spawn_blocking(move || run_media(request));
        self.pending.push(PendingTask { scope, handle });
    }

    fn replace(&mut self, scope: TaskScope, handle: JoinHandle<TaskCompletion>) {
        self.pending.retain(|task| task.scope != scope);
        self.pending.push(PendingTask { scope, handle });
    }
}

fn run_media(request: MediaRequest) -> TaskCompletion {
    let MediaRequest {
        intent_id,
        text,
        attachments,
        ..
    } = request;
    let resolved = media::resolve(attachments);
    if !resolved.errors.is_empty() {
        return TaskCompletion::Media(MediaResult::Refused {
            intent_id,
            kept: resolved.kept,
            errors: resolved.errors,
        });
    }
    TaskCompletion::Media(MediaResult::Ready {
        intent_id,
        draft: text,
        attachments: resolved.attachments,
    })
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

    fn app() -> App {
        App::new(
            StatusData::new("model-a", "current", InfoLevel::Default),
            true,
        )
    }

    #[test]
    fn stale_media_failure_does_not_replace_newer_draft() {
        let mut app = app();
        app.on_action(Action::InsertChar('a'));
        let effects = app.on_action(Action::Submit);
        assert!(matches!(
            effects.as_slice(),
            [Effect::PrepareSubmit { .. }]
                | [Effect::Append(_), Effect::PrepareSubmit { .. }]
        ));
        let intent_id = app.submit_state().intent_id().expect("pending intent");
        app.on_action(Action::InsertChar('n'));

        let effects = app.on_action(Action::SubmitMediaRefused {
            intent_id,
            kept: Vec::new(),
            errors: vec!["missing".to_owned()],
        });
        assert!(effects.iter().any(|effect| matches!(effect, Effect::Append(_))));
        assert_eq!(app.input.text(), "n");
    }

    #[tokio::test]
    async fn media_queue_decodes_in_order() {
        let mut tasks = PendingTasks::new();
        tasks.enqueue_media(1, "first".to_owned(), Vec::new());
        tasks.enqueue_media(2, "second".to_owned(), Vec::new());
        let first = tasks.next_completion().await;
        let second = tasks.next_completion().await;
        match first {
            TaskCompletion::Media(MediaResult::Ready {
                draft, intent_id, ..
            }) => {
                assert_eq!(draft, "first");
                assert_eq!(intent_id, 1);
            }
            other => panic!("unexpected {other:?}"),
        }
        match second {
            TaskCompletion::Media(MediaResult::Ready {
                draft, intent_id, ..
            }) => {
                assert_eq!(draft, "second");
                assert_eq!(intent_id, 2);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
