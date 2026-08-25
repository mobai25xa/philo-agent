//! Snapshot tests for host-backed commands applied through frontend updates.

#[cfg(test)]
mod tests {
    use philo_agent_service::{
        ConfirmationDecision, FrontendAvailability, FrontendConfigEntry, FrontendGeneration,
        FrontendReasoningEffort, FrontendSessionSummary, FrontendStatus, FrontendTokenUsage,
        FrontendToolListing, FrontendUpdateKind,
    };

    use crate::app::action::Action;
    use crate::app::effect::{Effect, HostRequest};
    use crate::app::state::App;
    use crate::app::status::StatusData;
    use crate::app::transcript::InfoLevel;
    use crate::tests::support::{empty_session_view, frontend_update, session_view};

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

    fn run_command(app: &mut App, text: &str) -> Vec<String> {
        type_text(app, text);
        let mut rendered = Vec::new();
        for effect in app.on_action(Action::Submit) {
            match effect {
                Effect::Append(lines) => {
                    rendered.extend(lines.into_iter().map(|line| line.text));
                }
                Effect::Host(request) => rendered.extend(apply_host(app, request)),
                Effect::Quit => rendered.push("[exit]".to_owned()),
                Effect::RequestShutdown => rendered.push("[shutdown]".to_owned()),
                other => panic!("command produced an unexpected effect: {other:?}"),
            }
        }
        rendered
    }

    fn apply_host(app: &mut App, request: HostRequest) -> Vec<String> {
        let mut rendered = Vec::new();
        let mut pending = vec![request];
        while let Some(request) = pending.pop() {
            let Some(kind) = kind_for(app, request) else {
                continue;
            };
            for effect in app.apply_update(&frontend_update(1, kind)) {
                match effect {
                    Effect::Append(lines) => {
                        rendered.extend(lines.into_iter().map(|line| line.text));
                    }
                    Effect::Host(next) => pending.push(next),
                    other => panic!("unexpected follow-up: {other:?}"),
                }
            }
        }
        rendered
    }

    fn kind_for(app: &App, request: HostRequest) -> Option<FrontendUpdateKind> {
        Some(match request {
            HostRequest::NewSession => FrontendUpdateKind::SessionLoaded {
                session_id: "fresh".to_owned(),
                view: empty_session_view("fresh"),
            },
            HostRequest::OpenSessions => FrontendUpdateKind::SessionListLoaded {
                sessions: vec![
                    FrontendSessionSummary {
                        session_id: "s-1".to_owned(),
                        title: Some("first session".to_owned()),
                        updated_at: None,
                    },
                    FrontendSessionSummary {
                        session_id: "s-2".to_owned(),
                        title: None,
                        updated_at: None,
                    },
                ],
            },
            HostRequest::OpenModels => FrontendUpdateKind::ModelListLoaded {
                models: vec![
                    philo_agent_service::FrontendModelListing {
                        id: "test/model-a".to_owned(),
                        provider: "test".to_owned(),
                        model: "model-a".to_owned(),
                        current: true,
                    },
                    philo_agent_service::FrontendModelListing {
                        id: "test/model-b".to_owned(),
                        provider: "test".to_owned(),
                        model: "model-b".to_owned(),
                        current: false,
                    },
                ],
            },
            HostRequest::LoadPreview(id) => FrontendUpdateKind::SessionPreviewed {
                session_id: id.clone(),
                view: session_view(&id),
            },
            HostRequest::SwitchSession(id) => FrontendUpdateKind::SessionLoaded {
                session_id: id.clone(),
                view: session_view(&id),
            },
            HostRequest::RenameSession { .. } => FrontendUpdateKind::CommandAccepted,
            HostRequest::RebuildModel(name) => FrontendUpdateKind::GenerationInstalled {
                display: FrontendGeneration {
                    generation_id: "g-1".to_owned(),
                    model_name: name,
                    reasoning_effort: None,
                    image_input: true,
                    tool_names: Vec::new(),
                },
            },
            HostRequest::SetReasoning(effort) => FrontendUpdateKind::GenerationInstalled {
                display: FrontendGeneration {
                    generation_id: "g-1".to_owned(),
                    model_name: app.status.model.clone(),
                    reasoning_effort: Some(format!("{effort:?}")),
                    image_input: true,
                    tool_names: Vec::new(),
                },
            },
            HostRequest::ShowConfig => FrontendUpdateKind::ConfigChanged {
                entries: vec![
                    FrontendConfigEntry {
                        key: "model".to_owned(),
                        value: "model-b".to_owned(),
                        source: "project".to_owned(),
                    },
                    FrontendConfigEntry {
                        key: "api_key_env".to_owned(),
                        value: "PHILO_API_KEY".to_owned(),
                        source: "global".to_owned(),
                    },
                ],
            },
            HostRequest::ShowStatus => FrontendUpdateKind::StatusReady(FrontendStatus {
                availability: FrontendAvailability::Idle,
                queued: 0,
                generation: FrontendGeneration {
                    generation_id: "g-1".to_owned(),
                    model_name: app.status.model.clone(),
                    reasoning_effort: None,
                    image_input: true,
                    tool_names: vec![
                        "read_file".to_owned(),
                        "write_file".to_owned(),
                        "shell".to_owned(),
                    ],
                },
                tools: vec![
                    FrontendToolListing {
                        name: "read_file".to_owned(),
                        effect_class: "read_only".to_owned(),
                    },
                    FrontendToolListing {
                        name: "write_file".to_owned(),
                        effect_class: "workspace".to_owned(),
                    },
                    FrontendToolListing {
                        name: "shell".to_owned(),
                        effect_class: "system".to_owned(),
                    },
                ],
            }),
            HostRequest::Respond(..) => return None,
        })
    }

    #[test]
    fn host_backed_command_output_snapshot() {
        let mut app = app();
        app.status.context_window = Some(128_000);
        app.status.usage = Some(FrontendTokenUsage {
            input_tokens: Some(120),
            output_tokens: Some(30),
            ..FrontendTokenUsage::default()
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
            output.extend(run_command(&mut app, command));
        }

        crate::tests::assert_tui_snapshot!("host_backed_commands", output.join("\n"));
    }

    #[test]
    fn model_rebuild_failure_keeps_the_old_model() {
        let mut app = app();
        type_text(&mut app, "/model broken");
        let mut output = Vec::new();
        for effect in app.on_action(Action::Submit) {
            match effect {
                Effect::Append(lines) => {
                    output.extend(lines.into_iter().map(|line| line.text));
                }
                Effect::Host(HostRequest::RebuildModel(_)) => {
                    output.extend(
                        app.apply_update(&frontend_update(
                            1,
                            FrontendUpdateKind::GenerationInstallFailed {
                                name: "broken".to_owned(),
                                message: "adapter rejected the name".to_owned(),
                            },
                        ))
                        .into_iter()
                        .filter_map(|effect| match effect {
                            Effect::Append(lines) => Some(lines.into_iter().map(|line| line.text)),
                            _ => None,
                        })
                        .flatten(),
                    );
                }
                other => panic!("unexpected effect: {other:?}"),
            }
        }

        assert_eq!(app.status.model, "model-a");
        assert_eq!(
            output,
            [
                "/model broken",
                "error: model not switched: adapter rejected the name; still on model-a",
            ]
        );
    }

    #[test]
    fn session_picker_lazily_previews_and_switches_with_history() {
        let mut app = app();
        let follow = apply_host(&mut app, HostRequest::OpenSessions);
        assert!(follow.is_empty());
        let first = app.overlay_frame(5).expect("picker opened").to_text();

        let effects = app.on_action(Action::MoveDown);
        assert_eq!(effects.len(), 1);
        let Effect::Host(request) = effects.into_iter().next().expect("preview request") else {
            panic!("moving the picker must load a preview")
        };
        apply_host(&mut app, request);
        let second = app.overlay_frame(5).expect("picker stays open").to_text();

        let effects = app.on_action(Action::Submit);
        let Effect::Host(request) = effects.into_iter().next().expect("switch request") else {
            panic!("Enter must switch the selected session")
        };
        let history = apply_host(&mut app, request);
        assert_eq!(app.status.session, "s-2");
        assert!(app.picker().is_none());

        crate::tests::assert_tui_snapshot!(
            "session_picker_flow",
            format!(
                "FIRST\n{first}\n\nSECOND\n{second}\n\nSWITCHED\n{}",
                history.join("\n")
            )
        );
    }

    #[test]
    fn confirmation_answer_is_a_respond_effect() {
        let mut app = app();
        app.apply_update(&frontend_update(
            1,
            FrontendUpdateKind::ConfirmationRequested {
                confirmation_id: 1,
                title: "write workspace file".to_owned(),
                body: "path: src/main.rs\noperation: replace".to_owned(),
            },
        ));
        let frame = app
            .overlay_frame(5)
            .expect("request opens overlay")
            .to_text();

        let effects = app.on_action(Action::InsertChar('y'));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Host(HostRequest::Respond(1, ConfirmationDecision::Allow))
        )));
        assert!(app.confirm_prompt().is_none());
        crate::tests::assert_tui_snapshot!("confirmation_flow", frame);
    }

    #[test]
    fn confirmation_resolved_closes_the_overlay() {
        let mut app = app();
        app.apply_update(&frontend_update(
            1,
            FrontendUpdateKind::ConfirmationRequested {
                confirmation_id: 1,
                title: "first".to_owned(),
                body: "one".to_owned(),
            },
        ));
        app.apply_update(&frontend_update(
            2,
            FrontendUpdateKind::ConfirmationResolved {
                confirmation_id: 1,
                decision: ConfirmationDecision::Deny,
            },
        ));
        assert!(app.overlay_frame(5).is_none());
    }

    #[test]
    fn reasoning_effort_debug_label_is_humanized() {
        let mut app = app();
        let lines = apply_host(
            &mut app,
            HostRequest::SetReasoning(FrontendReasoningEffort::High),
        );
        assert_eq!(lines, ["reasoning: high (from the next turn on)"]);
    }
}
