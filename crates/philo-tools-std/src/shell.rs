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
    ToolHandlerEndFuture, ToolHandlerFuture, ToolInvokeCx, ToolInvokeEnd, ToolProgressSink,
    ToolResult,
};
use tokio::io::AsyncReadExt;

use crate::args::{optional_u64, required_string};
use crate::error_code;
use crate::helpers::{field_error, stopped_if_cancelled};

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

/// Default upper bound of the final display-channel payload.
pub const DEFAULT_SHELL_MAX_DISPLAY_BYTES: usize = 128 * 1024;

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
    max_display_bytes: usize,
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
            max_display_bytes: DEFAULT_SHELL_MAX_DISPLAY_BYTES,
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

    /// Overrides the final display-channel byte limit (minimum 1).
    pub fn with_max_display_bytes(mut self, max_bytes: usize) -> Self {
        self.max_display_bytes = max_bytes.max(1);
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

    async fn run(&self, arguments: &ToolArguments, cx: ToolInvokeCx) -> ToolInvokeEnd {
        if let Some(stopped) = stopped_if_cancelled(&cx) {
            return stopped;
        }
        let command_line = match required_string(arguments.as_str(), "command") {
            Ok(command) => command,
            Err(error) => return ToolInvokeEnd::Done(field_error("command", &error)),
        };
        let timeout_secs = match optional_u64(arguments.as_str(), "timeout_secs") {
            Ok(Some(seconds)) => {
                if seconds == 0 || seconds > self.max_timeout_secs {
                    return ToolInvokeEnd::Done(RichToolResult::error(
                        error_code::INVALID_ARGUMENTS,
                        format!(
                            "timeout_secs must be between 1 and {}",
                            self.max_timeout_secs
                        ),
                    ));
                }
                seconds
            }
            Ok(None) => self.default_timeout_secs,
            Err(error) => return ToolInvokeEnd::Done(field_error("timeout_secs", &error)),
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

        if let Some(stopped) = stopped_if_cancelled(&cx) {
            return stopped;
        }

        let started = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ToolInvokeEnd::Done(RichToolResult::error(
                    error_code::SPAWN_FAILED,
                    format!("failed to start the platform shell: {}", error.kind()),
                ));
            }
        };

        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let captured = capture_output(
            &mut child,
            &mut stdout,
            &mut stderr,
            &cx,
            started + Duration::from_secs(timeout_secs),
        )
        .await;
        if captured.cancelled {
            return ToolInvokeEnd::Stopped;
        }
        let elapsed_ms = started.elapsed().as_millis();
        if captured.timed_out {
            return ToolInvokeEnd::Done(timeout_result(
                &command_line,
                timeout_secs,
                elapsed_ms,
                &captured,
                self.max_display_bytes,
            ));
        }
        let status = match captured.status {
            Ok(status) => status,
            Err(error) => {
                return ToolInvokeEnd::Done(RichToolResult::error(
                    error_code::IO_ERROR,
                    format!("command execution failed: {}", error.kind()),
                ));
            }
        };
        let stdout = captured.stdout;
        let stderr = captured.stderr;
        let exit_code = status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        let merged = merge_output(&stdout, &stderr);

        let (truncated_text, truncated) =
            truncate_output(&merged, self.max_output_bytes, self.max_output_lines);
        let mut model_text = format!("exit_code: {exit_code}\n{truncated_text}");
        if truncated {
            model_text.push_str(&format!(
                "\n[shell output truncated: {} bytes total]",
                merged.len()
            ));
        }

        let (display_text, display_truncated) = cap_display(&merged, self.max_display_bytes);
        let display = ToolDisplay::new(if display_text.is_empty() {
            format!("(no output) exit_code={exit_code}")
        } else {
            display_text
        })
        .with_fact("exit_code", exit_code)
        .with_fact("duration_ms", elapsed_ms.to_string())
        .with_fact("truncated", (truncated || display_truncated).to_string());
        ToolInvokeEnd::Done(
            RichToolResult::new(ToolResult::success(model_text)).with_display(display),
        )
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

struct Captured {
    status: std::io::Result<std::process::ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
    cancelled: bool,
}

async fn capture_output(
    child: &mut tokio::process::Child,
    stdout: &mut Option<tokio::process::ChildStdout>,
    stderr: &mut Option<tokio::process::ChildStderr>,
    cx: &ToolInvokeCx,
    deadline: Instant,
) -> Captured {
    let progress = cx.progress();
    let mut stdout_all = Vec::new();
    let mut stderr_all = Vec::new();
    let mut stdout_pending = Vec::new();
    let mut stderr_pending = Vec::new();
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];
    let mut stdout_done = stdout.is_none();
    let mut stderr_done = stderr.is_none();
    let mut status = None;
    let mut timed_out = false;
    let mut cancelled = false;

    while status.is_none() || !stdout_done || !stderr_done {
        let until_deadline = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            result = child.wait(), if status.is_none() => {
                status = Some(result);
            }
            result = read_opt(stdout.as_mut(), &mut out_buf), if !stdout_done => {
                match result {
                    Ok(0) | Err(_) => {
                        flush_pending(&mut stdout_pending, progress);
                        stdout_done = true;
                    }
                    Ok(n) => {
                        stdout_all.extend_from_slice(&out_buf[..n]);
                        let text = take_utf8(&mut stdout_pending, &out_buf[..n]);
                        if !text.is_empty() {
                            progress.push_text(&text);
                        }
                    }
                }
            }
            result = read_opt(stderr.as_mut(), &mut err_buf), if !stderr_done => {
                match result {
                    Ok(0) | Err(_) => {
                        flush_pending(&mut stderr_pending, progress);
                        stderr_done = true;
                    }
                    Ok(n) => {
                        stderr_all.extend_from_slice(&err_buf[..n]);
                        let text = take_utf8(&mut stderr_pending, &err_buf[..n]);
                        if !text.is_empty() {
                            progress.push_text(&text);
                        }
                    }
                }
            }
            _ = tokio::time::sleep(until_deadline), if !timed_out && !cancelled && status.is_none() => {
                let _ = child.start_kill();
                timed_out = true;
            }
            _ = cx.cancel().cancelled(), if !cancelled && !timed_out && status.is_none() => {
                let _ = child.start_kill();
                cancelled = true;
            }
        }
    }

    Captured {
        status: status.unwrap_or_else(|| Err(std::io::Error::other("child wait missing"))),
        stdout: stdout_all,
        stderr: stderr_all,
        timed_out,
        cancelled,
    }
}

async fn read_opt<R: tokio::io::AsyncRead + Unpin>(
    reader: Option<&mut R>,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    match reader {
        Some(reader) => reader.read(buf).await,
        None => std::future::pending().await,
    }
}

fn flush_pending(pending: &mut Vec<u8>, progress: &ToolProgressSink) {
    if pending.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(pending).into_owned();
    pending.clear();
    if !text.is_empty() {
        progress.push_text(&text);
    }
}

fn timeout_result(
    command_line: &str,
    timeout_secs: u64,
    elapsed_ms: u128,
    captured: &Captured,
    max_display_bytes: usize,
) -> RichToolResult {
    let merged = merge_output(&captured.stdout, &captured.stderr);
    let (display_text, display_truncated) = cap_display(&merged, max_display_bytes);
    RichToolResult::error(
        error_code::TIMEOUT,
        format!(
            "command did not finish within {timeout_secs}s and was \
             terminated: {command_line}"
        ),
    )
    .with_display(
        ToolDisplay::new(if display_text.is_empty() {
            format!("timed out after {elapsed_ms}ms")
        } else {
            display_text
        })
        .with_fact("duration_ms", elapsed_ms.to_string())
        .with_fact("timeout_secs", timeout_secs.to_string())
        .with_fact("truncated", display_truncated.to_string()),
    )
}

fn take_utf8(pending: &mut Vec<u8>, incoming: &[u8]) -> String {
    pending.extend_from_slice(incoming);
    match std::str::from_utf8(pending) {
        Ok(text) => {
            let owned = text.to_owned();
            pending.clear();
            owned
        }
        Err(error) => {
            let valid = error.valid_up_to();
            let owned = String::from_utf8(pending.drain(..valid).collect()).unwrap_or_default();
            if let Some(invalid) = error.error_len() {
                let skip = invalid.min(pending.len());
                pending.drain(..skip);
            }
            owned
        }
    }
}

fn merge_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut merged = String::new();
    merged.push_str(stdout.trim_end_matches('\n'));
    if !stderr.trim().is_empty() {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str("[stderr]\n");
        merged.push_str(stderr.trim_end_matches('\n'));
    }
    merged
}

fn cap_display(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    (text[start..].to_owned(), true)
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
        let future = self.call_with_cx(arguments, ToolInvokeCx::ignore());
        Box::pin(async move {
            future
                .await
                .into_done()
                .expect("ToolInvokeCx::ignore() never requests cancel")
        })
    }

    fn call_with_cx<'a>(
        &'a self,
        arguments: ToolArguments,
        cx: ToolInvokeCx,
    ) -> ToolHandlerEndFuture<'a> {
        Box::pin(async move { self.run(&arguments, cx).await })
    }
}
