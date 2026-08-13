use std::collections::VecDeque;
use std::sync::Mutex;

use philo_agent_runtime::{
    ModelCallSnapshot, ModelError, ModelEvent, ModelEventStream, ModelPort, RuntimeFuture,
    TokenUsage,
};

use super::gate::Gate;

/// One scripted model call. Events are replayed exactly in the supplied order.
#[derive(Clone, Debug)]
pub enum ModelScript {
    Events(Vec<Result<ModelEvent, ModelError>>),
    StartError(String),
    /// Emits `head`, then suspends the stream until the gate opens, then
    /// emits `tail`. Creates a deterministic mid-stream cancellation window.
    SuspendedEvents {
        head: Vec<Result<ModelEvent, ModelError>>,
        gate: Gate,
        tail: Vec<Result<ModelEvent, ModelError>>,
    },
}

impl ModelScript {
    pub fn text(deltas: &[&str]) -> Self {
        let mut events = deltas
            .iter()
            .map(|delta| Ok(ModelEvent::TextDelta((*delta).to_owned())))
            .collect::<Vec<_>>();
        events.push(Ok(ModelEvent::Completed));
        Self::Events(events)
    }

    pub fn tool_call(
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        argument_fragments: &[&str],
    ) -> Self {
        let mut events = argument_fragments
            .iter()
            .enumerate()
            .map(|(fragment, arguments)| {
                Ok(ModelEvent::ToolCallDelta {
                    index,
                    id: if fragment == 0 {
                        id.map(str::to_owned)
                    } else {
                        None
                    },
                    name: if fragment == 0 {
                        name.map(str::to_owned)
                    } else {
                        None
                    },
                    arguments: (*arguments).to_owned(),
                })
            })
            .collect::<Vec<_>>();
        events.push(Ok(ModelEvent::Completed));
        Self::Events(events)
    }

    pub fn tool_calls(calls: &[(usize, &str, &str, &str)]) -> Self {
        let events = calls
            .iter()
            .map(|(index, id, name, arguments)| {
                Ok(ModelEvent::ToolCallDelta {
                    index: *index,
                    id: Some((*id).to_owned()),
                    name: Some((*name).to_owned()),
                    arguments: (*arguments).to_owned(),
                })
            })
            .chain([Ok(ModelEvent::Completed)])
            .collect();
        Self::Events(events)
    }

    pub fn error(message: &str) -> Self {
        Self::Events(vec![Err(ModelError::new(message))])
    }

    /// Text stream that suspends after `head` deltas until the gate opens,
    /// then finishes with `tail` deltas and `Completed`.
    pub fn text_suspending(head: &[&str], gate: &Gate, tail: &[&str]) -> Self {
        let deltas = |values: &[&str]| {
            values
                .iter()
                .map(|delta| Ok(ModelEvent::TextDelta((*delta).to_owned())))
                .collect::<Vec<_>>()
        };
        let mut tail_events = deltas(tail);
        tail_events.push(Ok(ModelEvent::Completed));
        Self::SuspendedEvents {
            head: deltas(head),
            gate: gate.clone(),
            tail: tail_events,
        }
    }

    /// Prepends visible reasoning deltas to this script.
    pub fn with_reasoning(self, deltas: &[&str]) -> Self {
        let reasoning = deltas
            .iter()
            .map(|text| {
                Ok(ModelEvent::ReasoningDelta {
                    text: (*text).to_owned(),
                })
            })
            .collect::<Vec<_>>();
        self.prepend(reasoning)
    }

    /// Inserts a usage observation immediately before the final `Completed`.
    pub fn with_usage(self, usage: TokenUsage) -> Self {
        match self {
            Self::Events(mut events) => {
                let insert_at = events
                    .iter()
                    .rposition(|event| matches!(event, Ok(ModelEvent::Completed)))
                    .map_or(events.len(), |position| position);
                events.insert(insert_at, Ok(ModelEvent::UsageUpdated { usage }));
                Self::Events(events)
            }
            other => other,
        }
    }

    fn prepend(self, mut head: Vec<Result<ModelEvent, ModelError>>) -> Self {
        match self {
            Self::Events(events) => {
                head.extend(events);
                Self::Events(head)
            }
            Self::StartError(message) => Self::StartError(message),
            Self::SuspendedEvents {
                head: suspended_head,
                gate,
                tail,
            } => {
                head.extend(suspended_head);
                Self::SuspendedEvents { head, gate, tail }
            }
        }
    }

    /// Prepends an optional ResponseStarted metadata event to this script.
    pub fn with_response_started(
        self,
        response_model: Option<&str>,
        response_id: Option<&str>,
    ) -> Self {
        let started = Ok(ModelEvent::ResponseStarted {
            response_model: response_model.map(str::to_owned),
            response_id: response_id.map(str::to_owned),
        });
        match self {
            Self::Events(events) => {
                let mut with_started = vec![started];
                with_started.extend(events);
                Self::Events(with_started)
            }
            Self::StartError(message) => Self::StartError(message),
            Self::SuspendedEvents { head, gate, tail } => {
                let mut with_started = vec![started];
                with_started.extend(head);
                Self::SuspendedEvents {
                    head: with_started,
                    gate,
                    tail,
                }
            }
        }
    }

    pub fn mixed_output() -> Self {
        Self::Events(vec![
            Ok(ModelEvent::TextDelta("text".to_owned())),
            Ok(ModelEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".to_owned()),
                name: Some("tool".to_owned()),
                arguments: "{}".to_owned(),
            }),
            Ok(ModelEvent::Completed),
        ])
    }

    pub fn missing_tool_identity() -> Self {
        Self::Events(vec![
            Ok(ModelEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: Some("tool".to_owned()),
                arguments: "{}".to_owned(),
            }),
            Ok(ModelEvent::Completed),
        ])
    }

    pub fn duplicate_tool_identity() -> Self {
        Self::tool_calls(&[(0, "call-1", "tool", "{}"), (1, "call-1", "tool", "{}")])
    }

    pub fn completed_twice() -> Self {
        Self::Events(vec![
            Ok(ModelEvent::TextDelta("text".to_owned())),
            Ok(ModelEvent::Completed),
            Ok(ModelEvent::Completed),
        ])
    }

    pub fn without_completed() -> Self {
        Self::Events(vec![Ok(ModelEvent::TextDelta("unfinished".to_owned()))])
    }

    pub fn second_tool_call() -> Self {
        Self::tool_call(0, Some("call-2"), Some("tool"), &["{}"])
    }
}

/// Deterministic, script-driven ModelPort used by Runtime integration tests.
pub struct FakeModel {
    scripts: Mutex<VecDeque<ModelScript>>,
    calls: Mutex<Vec<ModelCallSnapshot>>,
}

impl FakeModel {
    pub fn new(scripts: impl IntoIterator<Item = ModelScript>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn succeeds(deltas: &[&str]) -> Self {
        Self::new([ModelScript::text(deltas)])
    }

    pub fn succeeds_sequence(outputs: Vec<Vec<&str>>) -> Self {
        Self::new(outputs.into_iter().map(|deltas| ModelScript::text(&deltas)))
    }

    pub fn start_fails(message: &str) -> Self {
        Self::new([ModelScript::StartError(message.to_owned())])
    }

    pub fn stream_fails_after(deltas: &[&str], message: &str) -> Self {
        let mut events = deltas
            .iter()
            .map(|delta| Ok(ModelEvent::TextDelta((*delta).to_owned())))
            .collect::<Vec<_>>();
        events.push(Err(ModelError::new(message)));
        Self::new([ModelScript::Events(events)])
    }

    pub fn calls(&self) -> Vec<ModelCallSnapshot> {
        self.calls.lock().expect("fake model calls mutex").clone()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("fake model calls mutex").len()
    }
}

impl ModelPort for FakeModel {
    fn start<'a>(
        &'a self,
        request: ModelCallSnapshot,
    ) -> RuntimeFuture<'a, Result<Box<dyn ModelEventStream>, ModelError>> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("fake model calls mutex")
                .push(request);
            let script = self
                .scripts
                .lock()
                .expect("fake model scripts mutex")
                .pop_front()
                .expect("fake model called more times than scripted");
            match script {
                ModelScript::StartError(message) => Err(ModelError::new(message)),
                ModelScript::Events(events) => Ok(Box::new(FakeStream {
                    events: events.into(),
                    suspension: None,
                }) as Box<dyn ModelEventStream>),
                ModelScript::SuspendedEvents { head, gate, tail } => Ok(Box::new(FakeStream {
                    events: head.into(),
                    suspension: Some((gate, tail.into())),
                })
                    as Box<dyn ModelEventStream>),
            }
        })
    }
}

struct FakeStream {
    events: VecDeque<Result<ModelEvent, ModelError>>,
    suspension: Option<(Gate, VecDeque<Result<ModelEvent, ModelError>>)>,
}

impl ModelEventStream for FakeStream {
    fn next<'a>(&'a mut self) -> RuntimeFuture<'a, Option<Result<ModelEvent, ModelError>>> {
        Box::pin(async move {
            loop {
                if let Some(event) = self.events.pop_front() {
                    return Some(event);
                }
                match &self.suspension {
                    Some((gate, _)) => {
                        gate.wait().await;
                        let (_, tail) = self.suspension.take().expect("suspension present");
                        self.events = tail;
                    }
                    None => return None,
                }
            }
        })
    }
}
