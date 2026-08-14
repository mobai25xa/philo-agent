//! Slash-command vocabulary: table, parsing, and prefix completion.
//!
//! The table is contract-level — it mirrors the TUI contract's command
//! table exactly, and extending it is a contract change, not an
//! implementation detail. Parsing is pure: a `/` prefix never reaches the
//! model, and an unrecognised word yields one error line.

use philo_agent_runtime::ReasoningEffort;

/// One command's user-facing description, shown by `/help` and the
/// completion menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
}

/// The command table (contract-level).
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        usage: "/help",
        summary: "commands and key bindings",
    },
    CommandSpec {
        name: "new",
        usage: "/new",
        summary: "start a new session",
    },
    CommandSpec {
        name: "sessions",
        usage: "/sessions",
        summary: "pick a session to continue",
    },
    CommandSpec {
        name: "model",
        usage: "/model <name>",
        summary: "switch the model (idle only)",
    },
    CommandSpec {
        name: "reasoning",
        usage: "/reasoning <level>",
        summary: "reasoning effort, from the next turn on",
    },
    CommandSpec {
        name: "compact",
        usage: "/compact",
        summary: "summarize older context (idle only)",
    },
    CommandSpec {
        name: "image",
        usage: "/image <path>",
        summary: "attach an image to the next message",
    },
    CommandSpec {
        name: "verbose",
        usage: "/verbose",
        summary: "toggle the information tier (same as Ctrl+O)",
    },
    CommandSpec {
        name: "status",
        usage: "/status",
        summary: "session, model, usage and the tool lineup",
    },
    CommandSpec {
        name: "config",
        usage: "/config",
        summary: "effective configuration and its source layers",
    },
    CommandSpec {
        name: "quit",
        usage: "/quit",
        summary: "leave the session",
    },
];

/// The reasoning levels `/reasoning` accepts, for error messages.
pub const REASONING_LEVELS: &str = "minimal | low | medium | high | very-high | maximum";

/// One parsed command. Argument-taking commands keep the raw argument so
/// the state machine can report a usage error itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    New,
    Sessions,
    Model { name: Option<String> },
    Reasoning { level: Option<String> },
    Compact,
    Image { path: Option<String> },
    Verbose,
    Status,
    Config,
    Quit,
}

/// A `/word` that is not in the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownCommand {
    word: String,
}

impl UnknownCommand {
    /// The word as typed, without the slash.
    pub fn word(&self) -> &str {
        &self.word
    }
}

/// Parses one command line. `input` carries the leading `/`; everything
/// after the first whitespace is the argument, trimmed and verbatim
/// otherwise (paths may contain spaces).
pub fn parse(input: &str) -> Result<Command, UnknownCommand> {
    let body = input.strip_prefix('/').unwrap_or(input);
    let mut words = body.splitn(2, char::is_whitespace);
    let name = words.next().unwrap_or_default();
    let argument = words
        .next()
        .map(str::trim)
        .filter(|argument| !argument.is_empty())
        .map(str::to_owned);
    match name {
        "help" => Ok(Command::Help),
        "new" => Ok(Command::New),
        "sessions" => Ok(Command::Sessions),
        "model" => Ok(Command::Model { name: argument }),
        "reasoning" => Ok(Command::Reasoning { level: argument }),
        "compact" => Ok(Command::Compact),
        "image" => Ok(Command::Image { path: argument }),
        "verbose" => Ok(Command::Verbose),
        "status" => Ok(Command::Status),
        "config" => Ok(Command::Config),
        "quit" => Ok(Command::Quit),
        _ => Err(UnknownCommand {
            word: name.to_owned(),
        }),
    }
}

/// Completion candidates for the command word being typed. Completion
/// applies to the word only: once the line has an argument there is
/// nothing to complete (`@path` completion is out of scope).
pub fn candidates(input: &str) -> Vec<&'static CommandSpec> {
    let Some(word) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if word.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(word))
        .collect()
}

/// The longest prefix shared by every name, for the first Tab press.
pub fn common_prefix(names: &[&str]) -> String {
    let Some((first, rest)) = names.split_first() else {
        return String::new();
    };
    let mut prefix = String::new();
    for (index, ch) in first.char_indices() {
        let candidate_len = index + ch.len_utf8();
        if rest
            .iter()
            .all(|name| name.starts_with(&first[..candidate_len]))
        {
            prefix.push(ch);
        } else {
            break;
        }
    }
    prefix
}

/// Maps a typed level onto the runtime's reasoning vocabulary.
pub fn parse_reasoning(level: &str) -> Option<ReasoningEffort> {
    match level.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "very-high" => Some(ReasoningEffort::VeryHigh),
        "maximum" => Some(ReasoningEffort::Maximum),
        _ => None,
    }
}

/// The display name of a reasoning level, for echoes.
pub fn reasoning_name(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::VeryHigh => "very-high",
        ReasoningEffort::Maximum => "maximum",
    }
}

/// The `/help` body: the command table plus the key bindings.
pub fn help_lines() -> Vec<String> {
    let width = COMMANDS
        .iter()
        .map(|spec| spec.usage.len())
        .max()
        .unwrap_or(0);
    let mut lines = vec!["commands:".to_owned()];
    lines.extend(
        COMMANDS
            .iter()
            .map(|spec| format!("  {:width$}  {}", spec.usage, spec.summary)),
    );
    lines.push("keys:".to_owned());
    lines.extend(
        [
            "Enter submit (queued while busy) | Ctrl+J newline (Shift+Enter where supported)",
            "Esc cancel the running turn | Ctrl+C clear input, cancel, or press twice to exit",
            "Ctrl+D exit on an empty prompt | Ctrl+O detail tier | Ctrl+L redraw",
            "Tab complete a command | Up/Down input history | scrolling stays native",
        ]
        .into_iter()
        .map(|hint| format!("  {hint}")),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_the_contract_vocabulary() {
        let names: Vec<&str> = COMMANDS.iter().map(|spec| spec.name).collect();
        assert_eq!(
            names,
            [
                "help",
                "new",
                "sessions",
                "model",
                "reasoning",
                "compact",
                "image",
                "verbose",
                "status",
                "config",
                "quit",
            ],
            "extending the command table is a contract change"
        );
    }

    #[test]
    fn every_command_parses() {
        assert_eq!(parse("/help"), Ok(Command::Help));
        assert_eq!(parse("/new"), Ok(Command::New));
        assert_eq!(parse("/sessions"), Ok(Command::Sessions));
        assert_eq!(
            parse("/model gpt-test"),
            Ok(Command::Model {
                name: Some("gpt-test".to_owned())
            })
        );
        assert_eq!(parse("/model"), Ok(Command::Model { name: None }));
        assert_eq!(
            parse("/reasoning high"),
            Ok(Command::Reasoning {
                level: Some("high".to_owned())
            })
        );
        assert_eq!(parse("/compact"), Ok(Command::Compact));
        assert_eq!(
            parse("/image  a b/c.png "),
            Ok(Command::Image {
                path: Some("a b/c.png".to_owned())
            }),
            "paths keep their inner spaces"
        );
        assert_eq!(parse("/verbose"), Ok(Command::Verbose));
        assert_eq!(parse("/status"), Ok(Command::Status));
        assert_eq!(parse("/config"), Ok(Command::Config));
        assert_eq!(parse("/quit"), Ok(Command::Quit));
    }

    #[test]
    fn unknown_words_report_themselves() {
        let error = parse("/nope please").expect_err("not in the table");
        assert_eq!(error.word(), "nope");
    }

    #[test]
    fn completion_matches_the_word_only() {
        let names = |input: &str| {
            candidates(input)
                .iter()
                .map(|spec| spec.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("/").len(), COMMANDS.len());
        assert_eq!(names("/s"), ["sessions", "status"]);
        assert_eq!(names("/se"), ["sessions"]);
        assert!(names("/zzz").is_empty());
        assert!(
            names("/model gpt").is_empty(),
            "arguments are not completed"
        );
        assert!(names("plain text").is_empty());
    }

    #[test]
    fn common_prefix_extends_as_far_as_it_can() {
        assert_eq!(common_prefix(&["sessions", "status"]), "s");
        assert_eq!(common_prefix(&["sessions"]), "sessions");
        assert_eq!(common_prefix(&["help", "new"]), "");
        assert_eq!(common_prefix(&[]), "");
    }

    #[test]
    fn reasoning_levels_map_onto_the_runtime_vocabulary() {
        assert_eq!(parse_reasoning("HIGH"), Some(ReasoningEffort::High));
        assert_eq!(
            parse_reasoning("very_high"),
            Some(ReasoningEffort::VeryHigh)
        );
        assert_eq!(parse_reasoning(" maximum "), Some(ReasoningEffort::Maximum));
        assert_eq!(parse_reasoning("turbo"), None);
        assert_eq!(reasoning_name(ReasoningEffort::VeryHigh), "very-high");
    }
}
