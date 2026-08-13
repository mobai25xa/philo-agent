//! Platform-shell command execution: fixed working directory (workspace
//! root), credential-variable scrubbing, no stdin, a configurable timeout,
//! and an explicit exit code on the model channel. A non-zero exit code is a
//! normal execution result — the model needs to see failing output itself;
//! only execution obstacles (timeout, spawn failure) are business errors.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolDefinition, ToolDisplay, ToolHandler,
    ToolHandlerFuture, ToolResult,
};

use crate::args::{optional_u64, required_string};
use crate::error_code;
use crate::helpers::field_error;

/// Stable registry name of the shell tool.
pub const SHELL_TOOL_NAME: &str = "shell";

/// Default command timeout, in seconds.
pub const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 60;

/// Default upper bound a call may request via `timeout_secs`.
pub const DEFAULT_SHELL_MAX_TIMEOUT_SECS: u64 = 300;

/// Default upper bound of merged output bytes on the model channel.
pub const DEFAULT_SHELL_MAX_OUTPUT_BYTES: usize = 16 * 1024;

/// Default upper bound of merged output lines on the model channel.
pub const DEFAULT_SHELL_MAX_OUTPUT_LINES: usize = 400;

/// Credential-bearing environment variables are scrubbed from the child
/// process so secrets cannot reach either output channel. The name-pattern
/// list is an implementation detail pinned by tests.
const CREDENTIAL_MARKERS: [&str; 6] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"];

/// Platform-shell command runner with a fixed workspace-root working
/// directory (assembly-injected, never a call parameter).
pub struct ShellTool {
    root: PathBuf,
    default_timeout_secs: u64,
    max_timeout_secs: u64,
    max_output_bytes: usize,
    max_output_lines: usize,
}

impl ShellTool {
    /// Creates a shell tool executing in `root` with default limits.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            default_timeout_secs: DEFAULT_SHELL_TIMEOUT_SECS,
            max_timeout_secs: DEFAULT_SHELL_MAX_TIMEOUT_SECS,
            max_output_bytes: DEFAULT_SHELL_MAX_OUTPUT_BYTES,
            max_output_lines: DEFAULT_SHELL_MAX_OUTPUT_LINES,
        }
    }

    /// Overrides the default timeout (clamped to the maximum, minimum 1s).
    pub fn with_default_timeout_secs(mut self, seconds: u64) -> Self {
        self.default_timeout_secs = seconds.clamp(1, self.max_timeout_secs);
        self
    }

    /// Overrides the maximum timeout a call may request (minimum 1s).
    pub fn with_max_timeout_secs(mut self, seconds: u64) -> Self {
        self.max_timeout_secs = seconds.max(1);
        self.default_timeout_secs = self.default_timeout_secs.min(self.max_timeout_secs);
        self
    }

    /// Overrides the merged-output byte limit (minimum 1).
    pub fn with_max_output_bytes(mut self, max_bytes: usize) -> Self {
        self.max_output_bytes = max_bytes.max(1);
        self
    }

    /// Overrides the merged-output line limit (minimum 1).
    pub fn with_max_output_lines(mut self, max_lines: usize) -> Self {
        self.max_output_lines = max_lines.max(1);
        self
    }

    /// Returns the model-facing definition registered for this tool.
    pub fn definition() -> ToolDefinition {
        let shell_name = if cfg!(windows) { "PowerShell" } else { "sh -c" };
        ToolDefinition::new(
            SHELL_TOOL_NAME,
            format!(
                "Run a command with the platform shell ({shell_name}) in the \
                 workspace root directory (the working directory is fixed). \
                 stdout and stderr are captured; the exit code is reported \
                 explicitly and a non-zero exit code is a normal result. There \
                 is no stdin, so interactive commands are not supported. Long \
                 output is truncated. An optional timeout_secs overrides the \
                 default timeout up to a configured maximum."
            ),
            r#"{"type":"object","properties":{"command":{"type":"string","description":"The command line to execute"},"timeout_secs":{"type":"integer","description":"Optional timeout in seconds, capped at the assembly maximum"}},"required":["command"]}"#,
            EffectClass::System,
        )
        .expect("shell tool definition is valid")
    }

    async fn run(&self, arguments: &ToolArguments) -> RichToolResult {
        let command_line = match required_string(arguments.as_str(), "command") {
            Ok(command) => command,
            Err(error) => return field_error("command", &error),
        };
        let timeout_secs = match optional_u64(arguments.as_str(), "timeout_secs") {
            Ok(Some(seconds)) => {
                if seconds == 0 || seconds > self.max_timeout_secs {
                    return RichToolResult::error(
                        error_code::INVALID_ARGUMENTS,
                        format!(
                            "timeout_secs must be between 1 and {}",
                            self.max_timeout_secs
                        ),
                    );
                }
                seconds
            }
            Ok(None) => self.default_timeout_secs,
            Err(error) => return field_error("timeout_secs", &error),
        };

        let mut command = platform_command(&command_line);
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Scrub credential-bearing variables: secrets must not be reachable
        // through the shell environment on either channel.
        for (name, _) in std::env::vars_os() {
            let upper = name.to_string_lossy().to_ascii_uppercase();
            if CREDENTIAL_MARKERS
                .iter()
                .any(|marker| upper.contains(marker))
            {
                command.env_remove(&name);
            }
        }

        let started = Instant::now();
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return RichToolResult::error(
                    error_code::SPAWN_FAILED,
                    format!("failed to start the platform shell: {}", error.kind()),
                );
            }
        };

        let waited =
            tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await;
        let elapsed_ms = started.elapsed().as_millis();
        let output = match waited {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                return RichToolResult::error(
                    error_code::IO_ERROR,
                    format!("command execution failed: {}", error.kind()),
                );
            }
            Err(_elapsed) => {
                // wait_with_output consumed the child; the drop path has
                // already detached — nothing to kill through this handle, so
                // report the obstacle. Timeout is a business error: the
                // model may retry with a longer timeout or a cheaper command.
                return RichToolResult::error(
                    error_code::TIMEOUT,
                    format!(
                        "command did not finish within {timeout_secs}s and was \
                         terminated: {command_line}"
                    ),
                )
                .with_display(
                    ToolDisplay::new(format!("timed out after {elapsed_ms}ms"))
                        .with_fact("duration_ms", elapsed_ms.to_string())
                        .with_fact("timeout_secs", timeout_secs.to_string()),
                );
            }
        };

        let exit_code = output
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut merged = String::new();
        merged.push_str(stdout.trim_end_matches('\n'));
        if !stderr.trim().is_empty() {
            if !merged.is_empty() {
                merged.push('\n');
            }
            merged.push_str("[stderr]\n");
            merged.push_str(stderr.trim_end_matches('\n'));
        }

        let (truncated_text, truncated) =
            truncate_output(&merged, self.max_output_bytes, self.max_output_lines);
        let mut model_text = format!("exit_code: {exit_code}\n{truncated_text}");
        if truncated {
            model_text.push_str(&format!(
                "\n[shell output truncated: {} bytes total]",
                merged.len()
            ));
        }

        let display = ToolDisplay::new(if merged.is_empty() {
            format!("(no output) exit_code={exit_code}")
        } else {
            merged.clone()
        })
        .with_fact("exit_code", exit_code)
        .with_fact("duration_ms", elapsed_ms.to_string())
        .with_fact("truncated", truncated.to_string());
        RichToolResult::new(ToolResult::success(model_text)).with_display(display)
    }
}

fn platform_command(command_line: &str) -> tokio::process::Command {
    if cfg!(windows) {
        let mut command = tokio::process::Command::new("powershell");
        command.args(["-NoProfile", "-NonInteractive", "-Command", command_line]);
        command
    } else {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", command_line]);
        command
    }
}

/// Dual byte/line truncation at a character boundary.
fn truncate_output(text: &str, max_bytes: usize, max_lines: usize) -> (String, bool) {
    let mut kept = String::new();
    let mut truncated = false;
    for (index, line) in text.lines().enumerate() {
        let row_len = line.len() + 1;
        if index >= max_lines || kept.len() + row_len > max_bytes {
            truncated = true;
            break;
        }
        if index > 0 {
            kept.push('\n');
        }
        kept.push_str(line);
    }
    if !truncated && kept.len() < text.len() && text.len() > max_bytes {
        truncated = true;
    }
    (kept, truncated)
}

impl ToolHandler for ShellTool {
    fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin(async move { self.run(&arguments).await })
    }
}
