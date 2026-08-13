//! Root-constrained recursive regex search over text files, with
//! `file:line` locations, glob filtering, and a match-count limit.

use std::path::{Path, PathBuf};

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolDefinition, ToolDisplay, ToolHandler,
    ToolHandlerFuture, ToolResult,
};

use crate::args::{optional_string, required_string};
use crate::error_code;
use crate::helpers::{field_error, path_error};
use crate::path::resolve_in_root;

/// Stable registry name of the grep tool.
pub const GREP_TOOL_NAME: &str = "grep";

/// Default upper bound of returned matches.
pub const DEFAULT_MAX_GREP_MATCHES: usize = 200;

/// Recursive regex search constrained to a root directory. Hidden
/// directories (names starting with `.`) and non-UTF-8 files are skipped.
pub struct GrepTool {
    root: PathBuf,
    max_matches: usize,
}

impl GrepTool {
    /// Creates a grep tool rooted at `root` with the default match limit.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_matches: DEFAULT_MAX_GREP_MATCHES,
        }
    }

    /// Overrides the match limit (minimum 1).
    pub fn with_max_matches(mut self, max_matches: usize) -> Self {
        self.max_matches = max_matches.max(1);
        self
    }

    /// Returns the model-facing definition registered for this tool.
    pub fn definition() -> ToolDefinition {
        ToolDefinition::new(
            GREP_TOOL_NAME,
            "Search text files inside the workspace root with a regular \
             expression. Searches recursively (hidden directories and binary \
             files are skipped); each match line is 'path:line:content'. An \
             optional glob restricts which file names are searched.",
            r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Regular expression to search for"},"path":{"type":"string","description":"File or directory to search, resolved against the root; defaults to the whole root"},"glob":{"type":"string","description":"Optional glob filtering file names, e.g. *.rs"}},"required":["pattern"]}"#,
            EffectClass::ReadOnly,
        )
        .expect("grep tool definition is valid")
    }

    fn grep(&self, arguments: &ToolArguments) -> RichToolResult {
        let pattern = match required_string(arguments.as_str(), "pattern") {
            Ok(pattern) => pattern,
            Err(error) => return field_error("pattern", &error),
        };
        let regex = match regex::Regex::new(&pattern) {
            Ok(regex) => regex,
            Err(error) => {
                return RichToolResult::error(
                    error_code::INVALID_REGEX,
                    format!("invalid regular expression '{pattern}': {error}"),
                );
            }
        };
        let path = match optional_string(arguments.as_str(), "path") {
            Ok(path) => path.unwrap_or_else(|| ".".to_owned()),
            Err(error) => return field_error("path", &error),
        };
        let glob = match optional_string(arguments.as_str(), "glob") {
            Ok(glob) => glob,
            Err(error) => return field_error("glob", &error),
        };
        let matcher = match glob.as_deref() {
            None => None,
            Some(pattern) => match globset::Glob::new(pattern) {
                Ok(glob) => Some(glob.compile_matcher()),
                Err(error) => {
                    return RichToolResult::error(
                        error_code::INVALID_GLOB,
                        format!("invalid glob '{pattern}': {error}"),
                    );
                }
            },
        };

        let target = match resolve_in_root(&self.root, &path, true) {
            Ok(target) => target,
            Err(error) => return path_error(&path, &error),
        };

        let mut matches: Vec<String> = Vec::new();
        let mut files_scanned = 0usize;
        let mut total_matches = 0usize;
        let mut stack = vec![target.clone()];
        while let Some(current) = stack.pop() {
            if current.is_dir() {
                let Ok(reader) = std::fs::read_dir(&current) else {
                    continue;
                };
                let mut children: Vec<PathBuf> = reader
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|child| {
                        let name = child
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        !(child.is_dir() && name.starts_with('.'))
                    })
                    .collect();
                // Deterministic traversal order.
                children.sort();
                for child in children.into_iter().rev() {
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
                if regex.is_match(line) {
                    total_matches += 1;
                    matches.push(format!("{relative}:{}:{line}", index + 1));
                }
            }
        }

        let truncated = total_matches > self.max_matches;
        let shown = total_matches.min(self.max_matches);
        let full_matches = matches.join("\n");
        let mut model_text = matches
            .iter()
            .take(self.max_matches)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        if total_matches == 0 {
            model_text = format!("no matches for '{pattern}' in {path}");
        } else if truncated {
            model_text.push_str(&format!(
                "\n[grep truncated: showing first {shown} of {total_matches} matches]"
            ));
        }

        let display = ToolDisplay::new(if total_matches == 0 {
            format!("grep '{pattern}' in {path}: no matches")
        } else {
            full_matches
        })
        .with_fact("files_scanned", files_scanned.to_string())
        .with_fact("matches_total", total_matches.to_string())
        .with_fact("truncated", truncated.to_string());
        RichToolResult::new(ToolResult::success(model_text)).with_display(display)
    }
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
        Box::pin(async move { self.grep(&arguments) })
    }
}
