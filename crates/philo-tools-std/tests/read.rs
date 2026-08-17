//! Root-constrained `read` tool: containment, error taxonomy, and the
//! numbered/dual-limit output shape.

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};

use philo_tools::{
    RichToolResult, ToolCancel, ToolDisplay, ToolInvocation, ToolInvokeCx, ToolInvokeEnd, ToolPort,
    ToolProgressSink, ToolRegistry, ToolResult,
};
use philo_tools_std::{READ_TOOL_NAME, ReadTool, error_code};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}

/// A unique temp directory removed on drop.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-tools-std-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn file(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&path, bytes).expect("write fixture file");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn registry(tool: ReadTool) -> ToolRegistry {
    ToolRegistry::builder()
        .register(ReadTool::definition(), tool)
        .expect("register read tool")
        .build()
}

fn invoke(registry: &ToolRegistry, arguments: &str) -> RichToolResult {
    match block_on(registry.invoke(
        ToolInvocation::new("call-1", READ_TOOL_NAME, arguments),
        ToolInvokeCx::ignore(),
    ))
    .expect("read tool never raises infrastructure errors")
    {
        ToolInvokeEnd::Done(result) => result,
        ToolInvokeEnd::Stopped => panic!("expected Done without a requested cancel"),
    }
}

fn error_code_of(rich: &RichToolResult) -> &str {
    match rich.result() {
        ToolResult::Error { code, .. } => code,
        ToolResult::Success { .. } => panic!("expected error, got success"),
    }
}

fn content_of(rich: &RichToolResult) -> &str {
    rich.result().content().expect("expected a success result")
}

fn fact<'a>(display: &'a ToolDisplay, name: &str) -> &'a str {
    display
        .facts()
        .iter()
        .find(|fact| fact.name() == name)
        .map(|fact| fact.value())
        .unwrap_or_else(|| panic!("missing fact {name}"))
}

#[test]
fn definition_registers_with_schema_and_effect_class() {
    let root = TempRoot::new();
    let registry = registry(ReadTool::new(&root.path));
    let definitions = registry.definitions();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name(), READ_TOOL_NAME);
    assert_eq!(
        definitions[0].effect_class(),
        philo_tools::EffectClass::ReadOnly
    );
    assert!(definitions[0].parameters().as_str().contains("\"path\""));
}

#[test]
fn reads_with_line_number_prefixes() {
    let root = TempRoot::new();
    root.file("hello.txt", b"hello tools\nsecond line");
    let registry = registry(ReadTool::new(&root.path));
    let result = invoke(&registry, r#"{"path":"hello.txt"}"#);
    assert_eq!(
        content_of(&result),
        "    1|hello tools\n    2|second line\n"
    );
    let display = result.display().expect("read carries display");
    assert!(display.detail().is_empty());
    assert_eq!(fact(display, "verb"), "Read");
    assert_eq!(fact(display, "body"), "none");
    assert_eq!(fact(display, "start_line"), "1");
    assert_eq!(fact(display, "end_line"), "2");
    assert_eq!(fact(display, "lines_shown"), "2");
    assert_eq!(fact(display, "lines_total"), "2");
}

#[test]
fn reads_nested_and_absolute_paths_inside_the_root() {
    let root = TempRoot::new();
    let absolute = root.file("nested/dir/file.txt", b"nested");
    let registry = registry(ReadTool::new(&root.path));

    let nested = invoke(&registry, r#"{"path":"nested/dir/file.txt"}"#);
    assert_eq!(content_of(&nested), "    1|nested\n");

    let absolute_json = format!(
        r#"{{"path":"{}"}}"#,
        absolute.display().to_string().replace('\\', "\\\\")
    );
    let by_absolute = invoke(&registry, &absolute_json);
    assert_eq!(content_of(&by_absolute), "    1|nested\n");
}

#[test]
fn empty_file_reads_as_empty_success() {
    let root = TempRoot::new();
    root.file("empty.txt", &[]);
    let registry = registry(ReadTool::new(&root.path));
    assert_eq!(
        content_of(&invoke(&registry, r#"{"path":"empty.txt"}"#)),
        ""
    );
}

#[test]
fn missing_file_is_a_not_found_business_error() {
    let root = TempRoot::new();
    let registry = registry(ReadTool::new(&root.path));
    let result = invoke(&registry, r#"{"path":"missing.txt"}"#);
    assert_eq!(error_code_of(&result), error_code::NOT_FOUND);
}

#[test]
fn parent_escape_is_rejected_without_touching_the_filesystem() {
    let root = TempRoot::new();
    let registry = registry(ReadTool::new(&root.path));
    let result = invoke(&registry, r#"{"path":"../outside-root-probe.txt"}"#);
    assert_eq!(error_code_of(&result), error_code::OUTSIDE_ROOT);
}

#[test]
fn absolute_path_outside_the_root_is_rejected() {
    let root = TempRoot::new();
    let outside = TempRoot::new();
    let escape = outside.file("escape.txt", b"secret");
    let registry = registry(ReadTool::new(&root.path));
    let arguments = format!(
        r#"{{"path":"{}"}}"#,
        escape.display().to_string().replace('\\', "\\\\")
    );
    let result = invoke(&registry, &arguments);
    assert_eq!(error_code_of(&result), error_code::OUTSIDE_ROOT);
}

#[test]
fn dot_dot_that_stays_inside_the_root_is_allowed() {
    let root = TempRoot::new();
    root.file("a/target.txt", b"inside");
    std::fs::create_dir_all(root.path.join("b")).expect("create sibling dir");
    let registry = registry(ReadTool::new(&root.path));
    let result = invoke(&registry, r#"{"path":"b/../a/target.txt"}"#);
    assert_eq!(content_of(&result), "    1|inside\n");
}

#[test]
fn non_utf8_content_is_a_binary_or_utf8_error() {
    let root = TempRoot::new();
    root.file("weird.bin", &[0xFF, 0xFE, 0x00, 0x41]);
    let registry = registry(ReadTool::new(&root.path));
    let result = invoke(&registry, r#"{"path":"weird.bin"}"#);
    // Contains NUL: classified as a binary file.
    assert_eq!(error_code_of(&result), error_code::BINARY_FILE);

    root.file("invalid.txt", &[0xFF, 0xFE, 0x41]);
    let result = invoke(&registry, r#"{"path":"invalid.txt"}"#);
    assert_eq!(error_code_of(&result), error_code::NOT_UTF8);
}

#[test]
fn image_files_are_rejected_with_an_image_hint() {
    let root = TempRoot::new();
    root.file("shot.png", &[0x89, 0x50, 0x4E, 0x47]);
    let registry = registry(ReadTool::new(&root.path));
    let result = invoke(&registry, r#"{"path":"shot.png"}"#);
    assert_eq!(error_code_of(&result), error_code::BINARY_FILE);
    let ToolResult::Error { message, .. } = result.result() else {
        panic!("expected error");
    };
    assert!(
        message.contains("--image"),
        "the error suggests attaching the image instead: {message}"
    );
}

#[test]
fn directory_path_is_a_business_error() {
    let root = TempRoot::new();
    std::fs::create_dir_all(root.path.join("subdir")).expect("create subdir");
    let registry = registry(ReadTool::new(&root.path));
    let result = invoke(&registry, r#"{"path":"subdir"}"#);
    assert_eq!(error_code_of(&result), error_code::NOT_A_FILE);
}

#[test]
fn byte_limit_truncates_at_a_row_boundary_with_a_marker() {
    let root = TempRoot::new();
    root.file("large.txt", "0123456789\n".repeat(10).as_bytes());
    // Each numbered row is "    N|0123456789\n" = 17 bytes; a 40-byte limit
    // fits two rows.
    let registry = registry(ReadTool::new(&root.path).with_max_bytes(40));
    let result = invoke(&registry, r#"{"path":"large.txt"}"#);
    let content = content_of(&result);
    assert!(content.starts_with("    1|0123456789\n    2|0123456789\n"));
    assert!(
        content.contains("[read truncated: showing first 2 of 10 lines"),
        "marker states the truncation explicitly: {content}"
    );
    let display = result.display().expect("display present");
    assert!(
        display
            .facts()
            .iter()
            .any(|fact| { fact.name() == "truncated" && fact.value() == "true" })
    );
}

#[test]
fn line_limit_truncates_with_a_marker() {
    let root = TempRoot::new();
    root.file("lines.txt", b"a\nb\nc\nd\n");
    let registry = registry(ReadTool::new(&root.path).with_max_lines(2));
    let result = invoke(&registry, r#"{"path":"lines.txt"}"#);
    let content = content_of(&result);
    assert!(content.starts_with("    1|a\n    2|b\n"));
    assert!(content.contains("showing first 2 of 4 lines"));
}

#[test]
fn missing_path_argument_fails_schema_validation() {
    let root = TempRoot::new();
    let registry = registry(ReadTool::new(&root.path));
    let result = invoke(&registry, r#"{"file":"hello.txt"}"#);
    assert_eq!(error_code_of(&result), "invalid_arguments");
    assert!(
        result.display().is_none(),
        "synthesized errors carry no display"
    );
}

#[test]
fn non_object_arguments_are_rejected_by_the_registry() {
    let root = TempRoot::new();
    let registry = registry(ReadTool::new(&root.path));
    let result = invoke(&registry, r#""hello.txt""#);
    assert_eq!(error_code_of(&result), "invalid_arguments");
}

#[test]
fn non_string_path_is_an_invalid_arguments_error() {
    let root = TempRoot::new();
    let registry = registry(ReadTool::new(&root.path));
    let result = invoke(&registry, r#"{"path":42}"#);
    assert_eq!(error_code_of(&result), error_code::INVALID_ARGUMENTS);
}

#[test]
fn already_requested_cancel_returns_stopped() {
    let root = TempRoot::new();
    root.file("hello.txt", b"hello tools");
    let registry = registry(ReadTool::new(&root.path));
    let cancel = ToolCancel::new();
    cancel.request();
    let end = block_on(registry.invoke(
        ToolInvocation::new("call-1", READ_TOOL_NAME, r#"{"path":"hello.txt"}"#),
        ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
    ))
    .expect("read tool never raises infrastructure errors");
    assert_eq!(end, ToolInvokeEnd::Stopped);
}
