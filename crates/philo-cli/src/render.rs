//! Event rendering: a pure state machine from `AgentEvent` to channel
//! writes, separated from terminal I/O so every path is unit-testable.
//!
//! Channel discipline (no exceptions): stdout carries only the model's
//! final answer text; everything else goes to stderr. `Unconfirmed`
//! settlement must never read like an ordinary completion.

use philo_agent_runtime::{
    AgentEvent, CancelReason, OperationStatus, SettlementDurability, TokenUsage,
};

use crate::config::Verbosity;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Stdout,
    Stderr,
}

/// One rendered write: the text goes to the channel exactly as given
/// (streaming deltas carry no trailing newline; line output does).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub channel: Channel,
    pub text: String,
}

fn out(text: impl Into<String>) -> Output {
    Output {
        channel: Channel::Stdout,
        text: text.into(),
    }
}

fn err(text: impl Into<String>) -> Output {
    Output {
        channel: Channel::Stderr,
        text: text.into(),
    }
}

/// Stateful renderer for one operation's event replay.
pub struct Renderer {
    verbosity: Verbosity,
    last_usage: Option<TokenUsage>,
    /// stdout has content whose final character is not a newline.
    stdout_open: bool,
    /// stderr is inside a streaming reasoning block.
    reasoning_open: bool,
    tool_batch_size: usize,
    /// The cancellation reason observed on this operation (M11), suffixed
    /// onto the terminal line.
    cancel_reason: Option<CancelReason>,
    /// `[ui].show_reasoning`: when off, reasoning never reaches stderr.
    show_reasoning: bool,
}

impl Renderer {
    pub fn new(verbosity: Verbosity) -> Self {
        Self {
            verbosity,
            last_usage: None,
            stdout_open: false,
            reasoning_open: false,
            tool_batch_size: 0,
            cancel_reason: None,
            show_reasoning: true,
        }
    }

    /// Applies `[ui].show_reasoning` from the configuration chain.
    pub fn with_reasoning(mut self, show: bool) -> Self {
        self.show_reasoning = show;
        self
    }

    fn verbose(&self) -> bool {
        self.verbosity == Verbosity::Verbose
    }

    fn quiet(&self) -> bool {
        self.verbosity == Verbosity::Quiet
    }

    /// Closes a streaming reasoning block before line-oriented output.
    fn close_reasoning(&mut self, outputs: &mut Vec<Output>) {
        if self.reasoning_open {
            outputs.push(err("\n"));
            self.reasoning_open = false;
        }
    }

    /// Renders one event into channel writes, in order.
    pub fn render(&mut self, event: &AgentEvent) -> Vec<Output> {
        let mut outputs = Vec::new();
        match event {
            AgentEvent::TextDelta { delta } => {
                self.close_reasoning(&mut outputs);
                if !delta.is_empty() {
                    self.stdout_open = !delta.ends_with('\n');
                    outputs.push(out(delta.clone()));
                }
            }
            AgentEvent::ReasoningDelta { text, .. } => {
                if self.show_reasoning && !self.quiet() && !text.is_empty() {
                    if !self.reasoning_open {
                        outputs.push(err("[reasoning] "));
                        self.reasoning_open = true;
                    }
                    outputs.push(err(text.clone()));
                }
            }
            AgentEvent::ModelUsageUpdated { usage, .. } => {
                self.last_usage = Some(*usage);
                if self.verbose() {
                    outputs.push(err(format!("usage update: {}\n", usage_line(usage))));
                }
            }
            AgentEvent::OperationQueued { operation_id } => {
                if self.verbose() {
                    outputs.push(err(format!("operation {operation_id} queued\n")));
                }
            }
            AgentEvent::OperationStarted { operation_id } => {
                if self.verbose() {
                    outputs.push(err(format!("operation {operation_id} started\n")));
                }
            }
            AgentEvent::TurnStarted { turn_id } => {
                if self.verbose() {
                    outputs.push(err(format!("turn {turn_id} started\n")));
                }
            }
            AgentEvent::ModelCallStarted { model_call_id } => {
                self.close_reasoning(&mut outputs);
                if self.verbose() {
                    outputs.push(err(format!("model call {model_call_id}\n")));
                }
            }
            AgentEvent::ModelResponseStarted {
                response_model,
                response_id,
                ..
            } => {
                if self.verbose() {
                    outputs.push(err(format!(
                        "model response: model={} id={}\n",
                        response_model.as_deref().unwrap_or("-"),
                        response_id.as_deref().unwrap_or("-"),
                    )));
                }
            }
            AgentEvent::ToolBatchRequested {
                tool_batch_id,
                call_count,
            } => {
                self.close_reasoning(&mut outputs);
                self.tool_batch_size = *call_count;
                if self.verbose() {
                    outputs.push(err(format!(
                        "tool batch {tool_batch_id}: {call_count} call(s)\n"
                    )));
                }
            }
            // M10: the event carries the tool identity and raw arguments.
            AgentEvent::ToolExecutionStarted {
                tool_name,
                arguments,
                index,
                ..
            } => {
                self.close_reasoning(&mut outputs);
                if self.verbose() {
                    outputs.push(err(format!(
                        "tool {}/{} {tool_name} arguments: {arguments}\n",
                        index + 1,
                        self.tool_batch_size,
                    )));
                } else if !self.quiet() {
                    outputs.push(err(format!(
                        "tool {tool_name}({})\n",
                        preview(arguments, 60)
                    )));
                }
            }
            // M10: the event carries the durable result and the display
            // channel; both render to stderr only.
            AgentEvent::ToolExecutionCompleted {
                tool_name,
                result,
                display,
                ..
            } => {
                self.close_reasoning(&mut outputs);
                let summary = match result {
                    philo_agent_runtime::ToolResult::Success { content } => {
                        format!("ok: {}", preview(content, 80))
                    }
                    philo_agent_runtime::ToolResult::Error { code, message } => {
                        format!("error {code}: {}", preview(message, 80))
                    }
                };
                if self.verbose() {
                    let full = match result {
                        philo_agent_runtime::ToolResult::Success { content } => content.clone(),
                        philo_agent_runtime::ToolResult::Error { code, message } => {
                            format!("[{code}] {message}")
                        }
                    };
                    outputs.push(err(format!("tool {tool_name} result:\n{full}\n")));
                    if let Some(display) = display {
                        if !display.detail().is_empty() {
                            outputs.push(err(format!("detail: {}\n", display.detail())));
                        }
                        if !display.facts().is_empty() {
                            let facts = display
                                .facts()
                                .iter()
                                .map(|fact| format!("{}={}", fact.name(), fact.value()))
                                .collect::<Vec<_>>()
                                .join(" ");
                            outputs.push(err(format!("facts: {facts}\n")));
                        }
                    }
                } else if !self.quiet() {
                    outputs.push(err(format!("  {tool_name} -> {summary}\n")));
                }
            }
            AgentEvent::AssistantMessageCompleted { .. } => {
                self.close_reasoning(&mut outputs);
                if self.stdout_open {
                    outputs.push(out("\n"));
                    self.stdout_open = false;
                }
            }
            // M11: a stale unfinished turn was sealed before this turn.
            AgentEvent::PriorTurnSealed { turn_id } => {
                self.close_reasoning(&mut outputs);
                if self.verbose() {
                    outputs.push(err(format!(
                        "notice: previous turn {turn_id} did not end cleanly and was sealed; \
                         its tool calls may have executed without recorded results\n"
                    )));
                } else if !self.quiet() {
                    outputs.push(err(
                        "notice: a previous turn did not end cleanly and was sealed; \
                         its tool calls may have executed without recorded results\n",
                    ));
                }
            }
            AgentEvent::ContextCompactionStarted => {
                self.close_reasoning(&mut outputs);
                if !self.quiet() {
                    outputs.push(err("compacting context...\n"));
                }
            }
            AgentEvent::ContextCompactionCompleted { covers_up_to } => {
                self.close_reasoning(&mut outputs);
                if self.verbose() {
                    outputs.push(err(format!("context compacted through {covers_up_to}\n")));
                } else if !self.quiet() {
                    outputs.push(err("context compacted\n"));
                }
            }
            AgentEvent::ContextCompactionFailed { message } => {
                self.close_reasoning(&mut outputs);
                outputs.push(err(format!(
                    "warning: context compaction failed: {message}; continuing without \
                     compaction\n"
                )));
            }
            AgentEvent::CancellationRequested { reason, .. } => {
                self.close_reasoning(&mut outputs);
                self.cancel_reason = Some(*reason);
                outputs.push(err(format!("cancelling ({})...\n", reason_text(*reason))));
            }
            AgentEvent::TurnCancelled { reason, .. } => {
                self.close_reasoning(&mut outputs);
                self.cancel_reason = Some(*reason);
                outputs.push(err(format!("turn cancelled ({})\n", reason_text(*reason))));
            }
            AgentEvent::TurnFailed { failure, .. } => {
                self.close_reasoning(&mut outputs);
                outputs.push(err(format!(
                    "error: turn failed ({:?}): {}\n",
                    failure.kind(),
                    failure.message(),
                )));
            }
            AgentEvent::OperationSettled {
                status, durability, ..
            } => {
                self.close_reasoning(&mut outputs);
                if self.stdout_open {
                    outputs.push(out("\n"));
                    self.stdout_open = false;
                }
                let unconfirmed = *durability == SettlementDurability::Unconfirmed;
                let show_line = !self.quiet() || *status != OperationStatus::Succeeded;
                if show_line {
                    let status_text = match (status, self.cancel_reason) {
                        (OperationStatus::Succeeded, _) => "succeeded".to_owned(),
                        (OperationStatus::Failed, _) => "failed".to_owned(),
                        (OperationStatus::Cancelled, Some(reason)) => {
                            format!("cancelled: {}", reason_text(reason))
                        }
                        (OperationStatus::Cancelled, None) => "cancelled".to_owned(),
                    };
                    outputs.push(err(format!("done ({status_text})\n")));
                    if let Some(usage) = &self.last_usage
                        && !self.quiet()
                    {
                        outputs.push(err(format!("{}\n", usage_line(usage))));
                    }
                }
                if unconfirmed {
                    outputs.push(err(
                        "WARNING: settlement durability UNCONFIRMED - the session may not \
                         have durably recorded this outcome\n",
                    ));
                }
            }
            // Future events (the enum is #[non_exhaustive]): ignore quietly.
            _ => {}
        }
        outputs
    }
}

/// Human-readable cancellation reason (M11). Seals never reach the
/// cancellation events, but the vocabulary stays total.
fn reason_text(reason: CancelReason) -> &'static str {
    match reason {
        CancelReason::User => "user",
        CancelReason::Timeout => "timeout",
        CancelReason::Abandoned => "abandoned",
    }
}

/// Single-line preview: newlines collapse to spaces, long text truncates
/// with an ellipsis at a character boundary.
fn preview(text: &str, max_chars: usize) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let flat = flat.trim();
    if flat.chars().count() <= max_chars {
        return flat.to_owned();
    }
    let kept: String = flat.chars().take(max_chars).collect();
    format!("{kept}...")
}

fn usage_line(usage: &TokenUsage) -> String {
    let part = |value: Option<u64>| value.map_or_else(|| "-".to_owned(), |v| v.to_string());
    let mut line = format!(
        "tokens: input {}, output {}",
        part(usage.input_tokens),
        part(usage.output_tokens)
    );
    if usage.reasoning_tokens.is_some() {
        line.push_str(&format!(", reasoning {}", part(usage.reasoning_tokens)));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use philo_agent_runtime::{
        AgentFailure, AgentFailureKind, ModelCallId, OperationId, ToolBatchId, ToolCallId, TurnId,
    };

    fn text(delta: &str) -> AgentEvent {
        AgentEvent::TextDelta {
            delta: delta.to_owned(),
        }
    }

    fn reasoning(text: &str) -> AgentEvent {
        AgentEvent::ReasoningDelta {
            model_call_id: ModelCallId::new("call-1"),
            text: text.to_owned(),
        }
    }

    fn settled(status: OperationStatus, durability: SettlementDurability) -> AgentEvent {
        AgentEvent::OperationSettled {
            operation_id: OperationId::new("op-1"),
            status,
            durability,
        }
    }

    fn usage() -> AgentEvent {
        AgentEvent::ModelUsageUpdated {
            model_call_id: ModelCallId::new("call-1"),
            usage: TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(20),
                ..TokenUsage::default()
            },
        }
    }

    fn render_all(renderer: &mut Renderer, events: &[AgentEvent]) -> Vec<Output> {
        events
            .iter()
            .flat_map(|event| renderer.render(event))
            .collect()
    }

    fn stdout_text(outputs: &[Output]) -> String {
        outputs
            .iter()
            .filter(|output| output.channel == Channel::Stdout)
            .map(|output| output.text.as_str())
            .collect()
    }

    fn stderr_text(outputs: &[Output]) -> String {
        outputs
            .iter()
            .filter(|output| output.channel == Channel::Stderr)
            .map(|output| output.text.as_str())
            .collect()
    }

    #[test]
    fn stdout_carries_only_answer_text_in_every_mode() {
        // `AssistantMessageCompleted` cannot be constructed outside the
        // runtime (no public AssistantMessage constructor); the settled
        // event covers the same newline-closing duty in this sequence.
        for verbosity in [Verbosity::Default, Verbosity::Verbose, Verbosity::Quiet] {
            let mut renderer = Renderer::new(verbosity);
            let outputs = render_all(
                &mut renderer,
                &[
                    AgentEvent::OperationStarted {
                        operation_id: OperationId::new("op-1"),
                    },
                    AgentEvent::TurnStarted {
                        turn_id: TurnId::new("turn-1"),
                    },
                    AgentEvent::ModelCallStarted {
                        model_call_id: ModelCallId::new("call-1"),
                    },
                    reasoning("thinking"),
                    text("hel"),
                    text("lo"),
                    usage(),
                    settled(OperationStatus::Succeeded, SettlementDurability::Confirmed),
                ],
            );
            assert_eq!(
                stdout_text(&outputs),
                "hello\n",
                "stdout is exactly the answer plus the final newline ({verbosity:?})"
            );
        }
    }

    #[test]
    fn show_reasoning_false_drops_the_reasoning_stream() {
        let mut renderer = Renderer::new(Verbosity::Default).with_reasoning(false);
        let outputs = render_all(&mut renderer, &[reasoning("hidden"), text("answer")]);
        assert_eq!(stderr_text(&outputs), "");
        assert_eq!(stdout_text(&outputs), "answer");
    }

    #[test]
    fn reasoning_streams_to_stderr_with_a_header_and_closes_before_text() {
        let mut renderer = Renderer::new(Verbosity::Default);
        let outputs = render_all(
            &mut renderer,
            &[reasoning("first "), reasoning("second"), text("answer")],
        );
        assert_eq!(stderr_text(&outputs), "[reasoning] first second\n");
        assert_eq!(stdout_text(&outputs), "answer");
    }

    fn tool_started(name: &str, arguments: &str) -> AgentEvent {
        AgentEvent::ToolExecutionStarted {
            tool_batch_id: ToolBatchId::new("batch-1"),
            tool_call_id: ToolCallId::new("call-1"),
            index: 0,
            tool_name: name.to_owned(),
            arguments: arguments.to_owned(),
        }
    }

    fn tool_completed(
        name: &str,
        result: philo_agent_runtime::ToolResult,
        display: Option<philo_agent_runtime::ToolDisplay>,
    ) -> AgentEvent {
        AgentEvent::ToolExecutionCompleted {
            tool_batch_id: ToolBatchId::new("batch-1"),
            tool_call_id: ToolCallId::new("call-1"),
            index: 0,
            tool_name: name.to_owned(),
            result,
            display,
        }
    }

    #[test]
    fn quiet_silences_reasoning_tools_and_success_but_keeps_errors() {
        let mut renderer = Renderer::new(Verbosity::Quiet);
        let outputs = render_all(
            &mut renderer,
            &[
                reasoning("hidden"),
                AgentEvent::ToolBatchRequested {
                    tool_batch_id: ToolBatchId::new("batch-1"),
                    call_count: 2,
                },
                tool_started("read", r#"{"path":"a.txt"}"#),
                tool_completed(
                    "read",
                    philo_agent_runtime::ToolResult::success("content"),
                    None,
                ),
                text("answer\n"),
                usage(),
                settled(OperationStatus::Succeeded, SettlementDurability::Confirmed),
            ],
        );
        assert_eq!(stdout_text(&outputs), "answer\n");
        assert_eq!(stderr_text(&outputs), "", "quiet success renders nothing");

        let mut renderer = Renderer::new(Verbosity::Quiet);
        let outputs = render_all(
            &mut renderer,
            &[
                AgentEvent::TurnFailed {
                    turn_id: TurnId::new("turn-1"),
                    failure: AgentFailure::new(AgentFailureKind::ModelCall, "offline"),
                },
                settled(OperationStatus::Failed, SettlementDurability::Confirmed),
            ],
        );
        let stderr = stderr_text(&outputs);
        assert!(
            stderr.contains("turn failed"),
            "errors stay visible: {stderr}"
        );
        assert!(stderr.contains("done (failed)"));
    }

    #[test]
    fn unconfirmed_settlement_is_prominent_even_in_quiet() {
        for verbosity in [Verbosity::Default, Verbosity::Verbose, Verbosity::Quiet] {
            let mut renderer = Renderer::new(verbosity);
            let outputs = render_all(
                &mut renderer,
                &[settled(
                    OperationStatus::Failed,
                    SettlementDurability::Unconfirmed,
                )],
            );
            let stderr = stderr_text(&outputs);
            assert!(
                stderr.contains("UNCONFIRMED"),
                "unconfirmed must be prominent ({verbosity:?}): {stderr}"
            );
        }
    }

    #[test]
    fn default_mode_shows_tool_name_with_argument_and_result_previews() {
        let mut renderer = Renderer::new(Verbosity::Default);
        let outputs = render_all(
            &mut renderer,
            &[
                AgentEvent::ToolBatchRequested {
                    tool_batch_id: ToolBatchId::new("batch-1"),
                    call_count: 1,
                },
                tool_started("grep", r#"{"pattern":"needle","path":"src"}"#),
                tool_completed(
                    "grep",
                    philo_agent_runtime::ToolResult::success(
                        "src/a.rs:1:needle\nsrc/b.rs:2:needle",
                    ),
                    None,
                ),
            ],
        );
        let stderr = stderr_text(&outputs);
        assert!(
            stderr.contains(r#"tool grep({"pattern":"needle","path":"src"})"#),
            "the summary line names the tool with an argument preview: {stderr}"
        );
        assert!(
            stderr.contains("grep -> ok: src/a.rs:1:needle src/b.rs:2:needle"),
            "the completion line previews the result on one line: {stderr}"
        );
    }

    #[test]
    fn default_mode_previews_are_truncated() {
        let mut renderer = Renderer::new(Verbosity::Default);
        let long_arguments = format!(r#"{{"content":"{}"}}"#, "x".repeat(200));
        let outputs = render_all(&mut renderer, &[tool_started("write", &long_arguments)]);
        let stderr = stderr_text(&outputs);
        assert!(stderr.contains("..."), "long previews truncate: {stderr}");
        assert!(stderr.len() < 120);
    }

    #[test]
    fn error_results_render_their_code_in_default_mode() {
        let mut renderer = Renderer::new(Verbosity::Default);
        let outputs = render_all(
            &mut renderer,
            &[tool_completed(
                "shell",
                philo_agent_runtime::ToolResult::error("timeout", "command exceeded 5s"),
                None,
            )],
        );
        let stderr = stderr_text(&outputs);
        assert!(
            stderr.contains("shell -> error timeout: command exceeded 5s"),
            "{stderr}"
        );
    }

    #[test]
    fn verbose_mode_details_arguments_results_and_display_facts() {
        let mut renderer = Renderer::new(Verbosity::Verbose);
        let display = philo_agent_runtime::ToolDisplay::new("full output here")
            .with_fact("exit_code", "1")
            .with_fact("duration_ms", "842");
        let outputs = render_all(
            &mut renderer,
            &[
                AgentEvent::ToolBatchRequested {
                    tool_batch_id: ToolBatchId::new("batch-1"),
                    call_count: 1,
                },
                tool_started("shell", r#"{"command":"cargo test"}"#),
                tool_completed(
                    "shell",
                    philo_agent_runtime::ToolResult::success("exit_code: 1\nfailures..."),
                    Some(display),
                ),
                usage(),
            ],
        );
        let stderr = stderr_text(&outputs);
        assert!(stderr.contains("tool batch batch-1: 1 call(s)"));
        assert!(stderr.contains(r#"tool 1/1 shell arguments: {"command":"cargo test"}"#));
        assert!(stderr.contains("tool shell result:\nexit_code: 1\nfailures..."));
        assert!(stderr.contains("detail: full output here"));
        assert!(stderr.contains("facts: exit_code=1 duration_ms=842"));
        assert!(stderr.contains("usage update: tokens: input 10, output 20"));
    }

    #[test]
    fn settled_success_reports_status_and_final_usage() {
        let mut renderer = Renderer::new(Verbosity::Default);
        let outputs = render_all(
            &mut renderer,
            &[
                text("hi\n"),
                usage(),
                settled(OperationStatus::Succeeded, SettlementDurability::Confirmed),
            ],
        );
        let stderr = stderr_text(&outputs);
        assert!(stderr.contains("done (succeeded)"));
        assert!(stderr.contains("tokens: input 10, output 20"));
    }

    #[test]
    fn cancellation_events_render_their_terminal_lines_with_the_reason() {
        let mut renderer = Renderer::new(Verbosity::Default);
        let outputs = render_all(
            &mut renderer,
            &[
                AgentEvent::CancellationRequested {
                    operation_id: OperationId::new("op-1"),
                    reason: CancelReason::User,
                },
                AgentEvent::TurnCancelled {
                    turn_id: TurnId::new("turn-1"),
                    reason: CancelReason::User,
                },
                settled(OperationStatus::Cancelled, SettlementDurability::Confirmed),
            ],
        );
        let stderr = stderr_text(&outputs);
        assert!(stderr.contains("cancelling (user)..."));
        assert!(stderr.contains("turn cancelled (user)"));
        assert!(stderr.contains("done (cancelled: user)"));
    }

    #[test]
    fn timeout_cancellation_renders_its_reason() {
        let mut renderer = Renderer::new(Verbosity::Default);
        let outputs = render_all(
            &mut renderer,
            &[
                AgentEvent::CancellationRequested {
                    operation_id: OperationId::new("op-1"),
                    reason: CancelReason::Timeout,
                },
                AgentEvent::TurnCancelled {
                    turn_id: TurnId::new("turn-1"),
                    reason: CancelReason::Timeout,
                },
                settled(OperationStatus::Cancelled, SettlementDurability::Confirmed),
            ],
        );
        let stderr = stderr_text(&outputs);
        assert!(stderr.contains("cancelling (timeout)..."));
        assert!(stderr.contains("turn cancelled (timeout)"));
        assert!(stderr.contains("done (cancelled: timeout)"));
    }

    fn sealed(turn: &str) -> AgentEvent {
        AgentEvent::PriorTurnSealed {
            turn_id: TurnId::new(turn),
        }
    }

    #[test]
    fn seal_notice_renders_one_line_by_default_and_stays_off_stdout() {
        let mut renderer = Renderer::new(Verbosity::Default);
        let outputs = render_all(
            &mut renderer,
            &[
                sealed("stale-turn-1"),
                text("answer\n"),
                settled(OperationStatus::Succeeded, SettlementDurability::Confirmed),
            ],
        );
        let stderr = stderr_text(&outputs);
        assert!(
            stderr.contains("notice: a previous turn did not end cleanly and was sealed"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("stale-turn-1"),
            "the default line does not name the turn id: {stderr}"
        );
        assert_eq!(stdout_text(&outputs), "answer\n", "stdout stays pure");
    }

    #[test]
    fn seal_notice_names_the_turn_in_verbose_and_is_silent_in_quiet() {
        let mut renderer = Renderer::new(Verbosity::Verbose);
        let outputs = render_all(&mut renderer, &[sealed("stale-turn-1")]);
        assert!(stderr_text(&outputs).contains("previous turn stale-turn-1"));

        let mut renderer = Renderer::new(Verbosity::Quiet);
        let outputs = render_all(
            &mut renderer,
            &[
                sealed("stale-turn-1"),
                text("answer\n"),
                settled(OperationStatus::Succeeded, SettlementDurability::Confirmed),
            ],
        );
        assert_eq!(stderr_text(&outputs), "", "quiet silences the seal notice");
        assert_eq!(stdout_text(&outputs), "answer\n");
    }

    #[test]
    fn compaction_events_are_status_lines_and_never_reach_stdout() {
        let events = [
            AgentEvent::ContextCompactionStarted,
            AgentEvent::ContextCompactionCompleted {
                covers_up_to: "entry-42".to_owned(),
            },
        ];

        let mut renderer = Renderer::new(Verbosity::Default);
        let outputs = render_all(&mut renderer, &events);
        assert_eq!(stdout_text(&outputs), "");
        assert_eq!(
            stderr_text(&outputs),
            "compacting context...\ncontext compacted\n"
        );

        let mut renderer = Renderer::new(Verbosity::Verbose);
        let outputs = render_all(&mut renderer, &events);
        assert!(stderr_text(&outputs).contains("context compacted through entry-42"));
    }

    #[test]
    fn quiet_hides_compaction_progress_but_keeps_failure_warnings() {
        let mut renderer = Renderer::new(Verbosity::Quiet);
        let outputs = render_all(
            &mut renderer,
            &[
                AgentEvent::ContextCompactionStarted,
                AgentEvent::ContextCompactionFailed {
                    message: "summary model unavailable".to_owned(),
                },
            ],
        );

        assert_eq!(stdout_text(&outputs), "");
        let stderr = stderr_text(&outputs);
        assert!(!stderr.contains("compacting context..."));
        assert!(stderr.contains("warning: context compaction failed"));
        assert!(stderr.contains("summary model unavailable"));
        assert!(stderr.contains("continuing without compaction"));
    }
}
