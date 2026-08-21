//! Root-constrained recursive regex search over text files, with
//! `file:line` locations, glob filtering, a match-count limit with early
//! stop, and per-line / total-output size caps.

use std::path::{Path, PathBuf};

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolCancel, ToolDefinition, ToolHandler,
    ToolHandlerEndFuture, ToolHandlerFuture, ToolInvokeCx, ToolInvokeEnd, ToolResult,
};

use crate::args::{optional_bool, optional_string, required_string};
use crate::display::card;
use crate::error_code;
use crate::helpers::{field_error, path_error, stopped_if_requested};
use crate::path::resolve_in_root;

/// Stable registry name of the grep tool.
pub const GREP_TOOL_NAME: &str = "grep";

/// Default upper bound of returned matches; scanning stops once reached.
pub const DEFAULT_MAX_GREP_MATCHES: usize = 200;

/// Default upper bound of returned output bytes.
pub const DEFAULT_MAX_GREP_BYTES: usize = 50 * 1024;

/// Match lines longer than this many characters are truncated with an
/// explicit suffix (the full line stays readable via the read tool).
const GREP_MAX_LINE_CHARS: usize = 500;

/// Cooperative cancel is observed at the start of each file/directory and
/// every this many lines within a file.
const GREP_CANCEL_LINE_INTERVAL: usize = 64;

/// Recursive regex search constrained to a root directory. Hidden
/// directories (names starting with `.`) and non-UTF-8 files are skipped;
/// directory symlinks are not followed (loop safety).
pub struct GrepTool {
    root: PathBuf,
    max_matches: usize,
    max_bytes: usize,
}

impl GrepTool {
    /// Creates a grep tool rooted at `root` with the default limits.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_matches: DEFAULT_MAX_GREP_MATCHES,
            max_bytes: DEFAULT_MAX_GREP_BYTES,
        }
    }

    /// Overrides the match limit (minimum 1). Scanning stops early at it.
    pub fn with_max_matches(mut self, max_matches: usize) -> Self {
        self.max_matches = max_matches.max(1);
        self
    }

    /// Overrides the output byte limit (minimum 1).
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes.max(1);
        self
    }

    /// Returns the model-facing definition registered for this tool.
    pub fn definition() -> ToolDefinition {
        ToolDefinition::new(
            GREP_TOOL_NAME,
            "Search text files inside the workspace root with a regular \
             expression. Searches recursively (hidden directories and binary \
             files are skipped); each match line is 'path:line:content', and \
             long lines are truncated. Scanning stops at the match limit \
             (default 200) — refine the pattern to narrow results. An \
             optional glob restricts which file names are searched; \
             ignore_case makes the search case-insensitive.",
            r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Regular expression to search for"},"path":{"type":"string","description":"File or directory to search, resolved against the root; defaults to the whole root"},"glob":{"type":"string","description":"Optional glob filtering file names, e.g. *.rs"},"ignore_case":{"type":"boolean","description":"Case-insensitive search (default false)"}},"required":["pattern"]}"#,
            EffectClass::ReadOnly,
        )
        .expect("grep tool definition is valid")
    }

    fn grep(&self, arguments: &ToolArguments, cancel: &ToolCancel) -> ToolInvokeEnd {
        if let Some(stopped) = stopped_if_requested(cancel) {
            return stopped;
        }
        let pattern = match required_string(arguments.as_str(), "pattern") {
            Ok(pattern) => pattern,
            Err(error) => return ToolInvokeEnd::Done(field_error("pattern", &error)),
        };
        let ignore_case = match optional_bool(arguments.as_str(), "ignore_case") {
            Ok(ignore_case) => ignore_case.unwrap_or(false),
            Err(error) => return ToolInvokeEnd::Done(field_error("ignore_case", &error)),
        };
        let pattern_source = if ignore_case {
            format!("(?i){pattern}")
        } else {
            pattern.clone()
        };
        let regex = match regex::Regex::new(&pattern_source) {
            Ok(regex) => regex,
            Err(error) => {
                return ToolInvokeEnd::Done(RichToolResult::error(
                    error_code::INVALID_REGEX,
                    format!("invalid regular expression '{pattern}': {error}"),
                ));
            }
        };
        let path = match optional_string(arguments.as_str(), "path") {
            Ok(path) => path.unwrap_or_else(|| ".".to_owned()),
            Err(error) => return ToolInvokeEnd::Done(field_error("path", &error)),
        };
        let glob = match optional_string(arguments.as_str(), "glob") {
            Ok(glob) => glob,
            Err(error) => return ToolInvokeEnd::Done(field_error("glob", &error)),
        };
        let matcher = match glob.as_deref() {
            None => None,
            Some(pattern) => match globset::Glob::new(pattern) {
                Ok(glob) => Some(glob.compile_matcher()),
                Err(error) => {
                    return ToolInvokeEnd::Done(RichToolResult::error(
                        error_code::INVALID_GLOB,
                        format!("invalid glob '{pattern}': {error}"),
                    ));
                }
            },
        };

        let target = match resolve_in_root(&self.root, &path, true) {
            Ok(target) => target,
            Err(error) => return ToolInvokeEnd::Done(path_error(&path, &error)),
        };

        let mut matches: Vec<String> = Vec::new();
        let mut locs: Vec<String> = Vec::new();
        let mut files_scanned = 0usize;
        let mut limit_reached = false;
        let mut stack = vec![target.clone()];
        'scan: while let Some(current) = stack.pop() {
            if let Some(stopped) = stopped_if_requested(cancel) {
                return stopped;
            }
            // symlink_metadata does not follow links: directory symlinks are
            // treated as files (fs::read rejects them), so symlink cycles
            // cannot recurse forever.
            let Ok(metadata) = std::fs::symlink_metadata(&current) else {
                continue;
            };
            if metadata.is_dir() {
                let name = current
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if current != target && name.starts_with('.') {
                    continue;
                }
                let Ok(reader) = std::fs::read_dir(&current) else {
                    continue;
                };
                let mut children: Vec<PathBuf> =
                    reader.flatten().map(|entry| entry.path()).collect();
                // Deterministic traversal order.
                children.sort();
                for child in children.into_iter().rev() {
                    if let Some(stopped) = stopped_if_requested(cancel) {
                        return stopped;
                    }
                    stack.push(child);
                }
                continue;
            }
            if let Some(matcher) = &matcher {
                let name = current
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !matcher.is_match(&name) {
                    continue;
                }
            }
            let Ok(bytes) = std::fs::read(&current) else {
                continue;
            };
            if bytes.contains(&0) {
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            files_scanned += 1;
            let relative = display_path(&target, &current, &path);
            for (index, line) in text.lines().enumerate() {
                if index > 0
                    && index % GREP_CANCEL_LINE_INTERVAL == 0
                    && let Some(stopped) = stopped_if_requested(cancel)
                {
                    return stopped;
                }
                if regex.is_match(line) {
                    let line_no = index + 1;
                    locs.push(format!("{relative}:{line_no}"));
                    matches.push(format!("{relative}:{line_no}:{}", truncate_line(line)));
                    if matches.len() >= self.max_matches {
                        limit_reached = true;
                        break 'scan;
                    }
                }
            }
            if let Some(stopped) = stopped_if_requested(cancel) {
                return stopped;
            }
        }

        let total_matches = matches.len();
        let mut model_text = if total_matches == 0 {
            format!("no matches for '{pattern}' in {path}")
        } else {
            matches.join("\n")
        };
        let mut byte_capped = false;
        if model_text.len() > self.max_bytes {
            let mut end = self.max_bytes;
            while end > 0 && !model_text.is_char_boundary(end) {
                end -= 1;
            }
            model_text.truncate(end);
            byte_capped = true;
        }
        let mut notices: Vec<String> = Vec::new();
        if limit_reached {
            notices.push(format!(
                "match limit of {} reached; refine the pattern or raise the limit",
                self.max_matches
            ));
        }
        if byte_capped {
            notices.push(format!("output exceeded {} bytes", self.max_bytes));
        }
        if !notices.is_empty() {
            if !model_text.is_empty() {
                model_text.push('\n');
            }
            model_text.push_str(&format!("[grep truncated: {}]", notices.join("; ")));
        }

        let detail = if total_matches == 0 {
            String::new()
        } else {
            locs.join("\n")
        };
        let display = card("Searched", format!("'{pattern}' in {path}"), "locs", detail)
            .with_fact("files_scanned", files_scanned.to_string())
            .with_fact("matches_total", total_matches.to_string())
            .with_fact("limit_reached", limit_reached.to_string())
            .with_fact("truncated", (limit_reached || byte_capped).to_string());
        ToolInvokeEnd::Done(
            RichToolResult::new(ToolResult::success(model_text)).with_display(display),
        )
    }
}

/// Caps a matched line at [`GREP_MAX_LINE_CHARS`] characters so oversized
/// source lines cannot dominate the model channel.
fn truncate_line(line: &str) -> String {
    if line.chars().count() <= GREP_MAX_LINE_CHARS {
        return line.to_owned();
    }
    let head: String = line.chars().take(GREP_MAX_LINE_CHARS).collect();
    format!("{head}... [truncated]")
}

/// Match locations render relative to the searched path for stability.
fn display_path(base: &Path, file: &Path, requested: &str) -> String {
    let relative = file.strip_prefix(base).unwrap_or(file);
    let mut text = relative.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        text = requested.to_owned();
    }
    text
}

impl ToolHandler for GrepTool {
    fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin(async move {
            self.grep(&arguments, &ToolCancel::none())
                .into_done()
                .expect("grep cannot stop without a requested cancel")
        })
    }

    fn call_with_cx<'a>(
        &'a self,
        arguments: ToolArguments,
        cx: ToolInvokeCx,
    ) -> ToolHandlerEndFuture<'a> {
        Box::pin(async move { self.grep(&arguments, cx.cancel()) })
    }
}
