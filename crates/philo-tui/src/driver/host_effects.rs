//! Pure application of completed host requests.
//!
//! The driver task registry performs the potentially blocking work. This
//! module only reduces owned results into App state and follow-up effects.

use philo_agent_runtime::EffectClass;
use philo_agent_runtime::{ReasoningEffort, ToolDefinition};
use philo_session::{SessionContextView, SessionId};

use crate::api::host::{ConfigEntry, HostError};
use crate::app::command;
use crate::app::effect::{Effect, HostRequest};
use crate::app::history;
use crate::app::overlay::Preview;
use crate::app::state::App;
use crate::app::transcript::{LineKind, TranscriptLine};

#[cfg(test)]
use crate::api::host::TuiHost;

/// Preview rows loaded per session in the picker (the overlay body height).
const PREVIEW_ROWS: usize = 5;

fn line(kind: LineKind, text: impl Into<String>) -> TranscriptLine {
    TranscriptLine {
        kind,
        text: text.into(),
    }
}

/// Owned output of one host task. No variant borrows the App or host.
pub(crate) enum HostResult {
    NewSession(String),
    OpenSessions(Result<Vec<SessionId>, HostError>),
    Preview {
        id: SessionId,
        result: Result<SessionContextView, HostError>,
    },
    SwitchSession {
        id: SessionId,
        result: Result<SessionContextView, HostError>,
    },
    RebuildModel {
        name: String,
        result: Result<(), HostError>,
    },
    SetReasoning {
        effort: ReasoningEffort,
        result: Result<(), HostError>,
    },
    ShowConfig(Vec<ConfigEntry>),
    ShowStatus(Vec<ToolDefinition>),
}

impl HostResult {
    /// A successful session transition starts a fresh live Markdown parser.
    pub(crate) fn resets_session(&self) -> bool {
        matches!(
            self,
            Self::NewSession(_) | Self::SwitchSession { result: Ok(_), .. }
        )
    }
}

/// Applies one completed host result through the pure App state machine.
pub(crate) fn apply(app: &mut App, result: HostResult) -> Vec<Effect> {
    match result {
        HostResult::NewSession(id) => {
            app.begin_session(&id);
            append(vec![line(LineKind::Meta, format!("new session: {id}"))])
        }
        HostResult::OpenSessions(Err(error)) => append(vec![line(
            LineKind::Error,
            format!("error: sessions unavailable: {}", error.message()),
        )]),
        HostResult::OpenSessions(Ok(sessions)) if sessions.is_empty() => append(vec![line(
            LineKind::Notice,
            "no sessions recorded yet; this one starts with the first message",
        )]),
        HostResult::OpenSessions(Ok(sessions)) => {
            app.open_picker(sessions);
            app.claim_preview()
                .map(|id| vec![Effect::Host(HostRequest::LoadPreview(id))])
                .unwrap_or_default()
        }
        HostResult::Preview { id, result } => {
            let preview = match result {
                Ok(view) => Preview::Ready(history::preview_lines(&view, PREVIEW_ROWS)),
                Err(error) => Preview::Failed(error.message().to_owned()),
            };
            app.set_preview(&id, preview);
            Vec::new()
        }
        HostResult::SwitchSession {
            id,
            result: Ok(view),
        } => {
            app.begin_session(id.as_str());
            let mut lines = vec![line(LineKind::Meta, format!("session {}", id.as_str()))];
            let history = history::history_lines(&view);
            if history.is_empty() {
                lines.push(line(LineKind::Meta, "(no history yet)"));
            } else {
                lines.extend(history);
            }
            append(lines)
        }
        HostResult::SwitchSession {
            id,
            result: Err(error),
        } => append(vec![line(
            LineKind::Error,
            format!(
                "error: session {} not opened: {}",
                id.as_str(),
                error.message()
            ),
        )]),
        HostResult::RebuildModel {
            name,
            result: Ok(()),
        } => {
            app.status.model.clone_from(&name);
            append(vec![line(LineKind::Meta, format!("model: {name}"))])
        }
        HostResult::RebuildModel {
            result: Err(error), ..
        } => append(vec![line(
            LineKind::Error,
            format!(
                "error: model not switched: {}; still on {}",
                error.message(),
                app.status.model
            ),
        )]),
        HostResult::SetReasoning {
            effort,
            result: Ok(()),
        } => append(vec![line(
            LineKind::Meta,
            format!(
                "reasoning: {} (from the next turn on)",
                command::reasoning_name(effort)
            ),
        )]),
        HostResult::SetReasoning {
            result: Err(error), ..
        } => append(vec![line(
            LineKind::Error,
            format!("error: reasoning not changed: {}", error.message()),
        )]),
        HostResult::ShowConfig(entries) => append(config_lines(entries)),
        HostResult::ShowStatus(tools) => append(status_lines(app, tools)),
    }
}

fn append(lines: Vec<TranscriptLine>) -> Vec<Effect> {
    vec![Effect::Append(lines)]
}

fn config_lines(entries: Vec<ConfigEntry>) -> Vec<TranscriptLine> {
    if entries.is_empty() {
        return vec![line(LineKind::Meta, "config: no effective entries")];
    }
    let width = entries
        .iter()
        .map(|entry| entry.key.len())
        .max()
        .unwrap_or(0);
    let mut lines = vec![line(LineKind::Meta, "config (effective):")];
    lines.extend(entries.iter().map(|entry| {
        line(
            LineKind::Meta,
            format!(
                "  {:width$} = {}  [{}]",
                entry.key, entry.value, entry.source
            ),
        )
    }));
    lines
}

fn status_lines(app: &App, tools: Vec<ToolDefinition>) -> Vec<TranscriptLine> {
    let mut lines = vec![line(LineKind::Meta, app.status.line())];
    if let Some(summary) = app.attachments().summary() {
        lines.push(line(LineKind::Meta, summary));
    }
    if tools.is_empty() {
        lines.push(line(LineKind::Meta, "tools: none"));
        return lines;
    }
    lines.push(line(LineKind::Meta, format!("tools ({}):", tools.len())));
    lines.extend(tools.iter().map(|tool| {
        line(
            LineKind::Meta,
            format!(
                "  {} [{}]",
                tool.name(),
                effect_class_name(tool.effect_class())
            ),
        )
    }));
    lines
}

fn effect_class_name(class: EffectClass) -> &'static str {
    match class {
        EffectClass::ReadOnly => "read-only",
        EffectClass::Workspace => "workspace",
        EffectClass::System => "system",
    }
}

/// Compatibility adapter for state/snapshot tests. Production requests are
/// always executed by `driver::tasks` and never await in the effect loop.
#[cfg(test)]
pub(crate) async fn execute(
    app: &mut App,
    host: &dyn TuiHost,
    request: HostRequest,
) -> Vec<TranscriptLine> {
    use std::collections::VecDeque;

    let mut requests = VecDeque::from([request]);
    let mut lines = Vec::new();
    while let Some(request) = requests.pop_front() {
        if let HostRequest::Respond(id, response) = request {
            host.confirmations().respond(id, response);
            continue;
        }
        let result = execute_for_test(host, request).await;
        for effect in apply(app, result) {
            match effect {
                Effect::Append(mut appended) => lines.append(&mut appended),
                Effect::Host(request) => requests.push_back(request),
                other => panic!("host result produced an unexpected effect: {other:?}"),
            }
        }
    }
    lines
}

#[cfg(test)]
async fn execute_for_test(host: &dyn TuiHost, request: HostRequest) -> HostResult {
    match request {
        HostRequest::NewSession => HostResult::NewSession(host.new_session_id()),
        HostRequest::OpenSessions => HostResult::OpenSessions(host.list_sessions()),
        HostRequest::LoadPreview(id) => {
            let result = host.context_view(&id).await;
            HostResult::Preview { id, result }
        }
        HostRequest::SwitchSession(id) => {
            let result = host.context_view(&id).await;
            HostResult::SwitchSession { id, result }
        }
        HostRequest::RebuildModel(name) => {
            let result = host.rebuild_model(&name);
            HostResult::RebuildModel { name, result }
        }
        HostRequest::SetReasoning(effort) => HostResult::SetReasoning {
            effort,
            result: host.set_reasoning(effort),
        },
        HostRequest::ShowConfig => HostResult::ShowConfig(host.config_view()),
        HostRequest::ShowStatus => HostResult::ShowStatus(host.tool_definitions()),
        HostRequest::Respond(..) => unreachable!("responses are handled before task execution"),
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use philo_agent_runtime::{EffectClass, ReasoningEffort, TokenUsage};

    use super::*;
    use crate::api::confirmation::{ConfirmationRequest, ConfirmationResponse};
    use crate::api::host::ConfigEntry;
    use crate::app::action::Action;
    use crate::app::effect::Effect;
    use crate::app::status::StatusData;
    use crate::app::transcript::InfoLevel;
    use crate::tests::support::{FakeHost, session_view, tool};

    fn app() -> App {
        App::new(
            StatusData::new("model-a", "current", InfoLevel::Default),
            true,
        )
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.on_action(Action::InsertChar(ch));
        }
    }

    async fn run_command(app: &mut App, host: &FakeHost, text: &str) -> Vec<String> {
        type_text(app, text);
        let mut rendered = Vec::new();
        for effect in app.on_action(Action::Submit) {
            match effect {
                Effect::Append(lines) => {
                    rendered.extend(lines.into_iter().map(|line| line.text));
                }
                Effect::Host(request) => {
                    rendered.extend(
                        execute(app, host, request)
                            .await
                            .into_iter()
                            .map(|line| line.text),
                    );
                }
                Effect::Quit => rendered.push("[exit]".to_owned()),
                other => panic!("command produced an unexpected effect: {other:?}"),
            }
        }
        rendered
    }

    #[tokio::test]
    async fn host_backed_command_output_snapshot() {
        let host = FakeHost::new();
        host.set_next_session_id("fresh");
        host.set_config(vec![
            ConfigEntry {
                key: "model".to_owned(),
                value: "model-b".to_owned(),
                source: "project".to_owned(),
            },
            ConfigEntry {
                key: "api_key_env".to_owned(),
                value: "PHILO_API_KEY".to_owned(),
                source: "global".to_owned(),
            },
        ]);
        host.set_tools(vec![
            tool("read_file", EffectClass::ReadOnly),
            tool("write_file", EffectClass::Workspace),
            tool("shell", EffectClass::System),
        ]);
        let mut app = app();
        app.status.context_window = Some(128_000);
        app.status.usage = Some(TokenUsage {
            input_tokens: Some(120),
            output_tokens: Some(30),
            ..TokenUsage::default()
        });

        let mut output = Vec::new();
        for command in [
            "/new",
            "/model model-b",
            "/reasoning high",
            "/image shots/diagram.png",
            "/verbose",
            "/status",
            "/config",
            "/quit",
        ] {
            output.extend(run_command(&mut app, host.as_ref(), command).await);
        }

        assert_eq!(host.reasoning_calls(), [ReasoningEffort::High]);
        crate::tests::assert_tui_snapshot!("host_backed_commands", output.join("\n"));
    }

    #[tokio::test]
    async fn model_rebuild_failure_keeps_the_old_model() {
        let host = FakeHost::new();
        host.fail_model("adapter rejected the name");
        let mut app = app();

        let output = run_command(&mut app, host.as_ref(), "/model broken").await;

        assert_eq!(app.status.model, "model-a");
        assert_eq!(
            output,
            [
                "/model broken",
                "error: model not switched: adapter rejected the name; still on model-a",
            ]
        );
    }

    #[tokio::test]
    async fn session_picker_lazily_previews_and_switches_with_history() {
        let host = FakeHost::new();
        host.set_sessions(&["s-1", "s-2"]);
        host.set_view("s-1", session_view("s-1"));
        host.set_view("s-2", session_view("s-2"));
        let mut app = app();

        let lines = execute(&mut app, host.as_ref(), HostRequest::OpenSessions).await;
        assert!(lines.is_empty());
        assert_eq!(host.view_calls(), ["s-1"]);
        let first = app.overlay_frame(5).expect("picker opened").to_text();

        let effects = app.on_action(Action::MoveDown);
        assert_eq!(effects.len(), 1);
        let Effect::Host(request) = effects.into_iter().next().expect("preview request") else {
            panic!("moving the picker must load a preview")
        };
        execute(&mut app, host.as_ref(), request).await;
        assert_eq!(host.view_calls(), ["s-1", "s-2"]);
        let second = app.overlay_frame(5).expect("picker stays open").to_text();

        let effects = app.on_action(Action::Submit);
        let Effect::Host(request) = effects.into_iter().next().expect("switch request") else {
            panic!("Enter must switch the selected session")
        };
        let history = execute(&mut app, host.as_ref(), request).await;
        assert_eq!(app.status.session, "s-2");
        assert!(app.picker().is_none());

        crate::tests::assert_tui_snapshot!(
            "session_picker_flow",
            format!(
                "FIRST\n{first}\n\nSECOND\n{second}\n\nSWITCHED\n{}",
                history
                    .iter()
                    .map(|line| line.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        );
    }

    #[tokio::test]
    async fn fake_requester_receives_the_overlay_answer() {
        let host = FakeHost::new();
        let channel = host.confirmations();
        let response = channel.request(ConfirmationRequest {
            title: "write workspace file".to_owned(),
            body: "path: src/main.rs\noperation: replace".to_owned(),
        });
        let mut response = pin!(response);
        let mut app = app();
        app.sync_confirmation(channel.front());
        let frame = app
            .overlay_frame(5)
            .expect("request opens overlay")
            .to_text();

        let effects = app.on_action(Action::InsertChar('y'));
        for effect in effects {
            if let Effect::Host(request) = effect {
                execute(&mut app, host.as_ref(), request).await;
            }
        }

        assert_eq!(response.as_mut().await, ConfirmationResponse::Allow);
        assert!(channel.is_idle());
        assert!(app.confirm_prompt().is_none());
        crate::tests::assert_tui_snapshot!("confirmation_flow", frame);
    }

    #[tokio::test]
    async fn operation_end_auto_denies_every_fake_requester() {
        let host = FakeHost::new();
        let channel = host.confirmations();
        let first = channel.request(ConfirmationRequest {
            title: "first".to_owned(),
            body: "one".to_owned(),
        });
        let second = channel.request(ConfirmationRequest {
            title: "second".to_owned(),
            body: "two".to_owned(),
        });
        let mut first = pin!(first);
        let mut second = pin!(second);
        let mut app = app();
        app.sync_confirmation(channel.front());

        channel.deny_all();
        app.sync_confirmation(channel.front());

        assert_eq!(first.as_mut().await, ConfirmationResponse::Deny);
        assert_eq!(second.as_mut().await, ConfirmationResponse::Deny);
        assert!(app.overlay_frame(5).is_none());
    }
}
