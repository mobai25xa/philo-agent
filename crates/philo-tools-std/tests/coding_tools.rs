//! TOOL-004: list / grep / write / edit / shell behavior contracts —
//! success paths, business-error taxonomy, truncation marks, containment,
//! and shell credential scrubbing. CI-offline (shell uses local commands).

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};

use philo_tools::{EffectClass, RichToolResult, ToolArguments, ToolHandler, ToolResult};
use philo_tools_std::{EditTool, GrepTool, ListTool, ShellTool, WriteTool, error_code};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-tools-m10-{}-{}",
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

fn call(handler: &dyn ToolHandler, arguments: &str) -> RichToolResult {
    let arguments = ToolArguments::parse(arguments).expect("test arguments are valid JSON");
    block_on(handler.call(arguments))
}

fn error_code_of(rich: &RichToolResult) -> &str {
    match rich.result() {
        ToolResult::Error { code, .. } => code,
        ToolResult::Success { .. } => panic!("expected error, got success"),
    }
}

fn content_of(rich: &RichToolResult) -> &str {
    rich.result().content().expect("expected success")
}

// ------------------------------- list ------------------------------------

#[test]
fn list_types_sorts_and_filters_entries() {
    let root = TempRoot::new();
    root.file("b.rs", b"b");
    root.file("a.txt", b"a");
    std::fs::create_dir_all(root.path.join("zdir")).expect("dir");
    let tool = ListTool::new(&root.path);

    let all = call(&tool, r#"{"path":"."}"#);
    assert_eq!(content_of(&all), "dir\tzdir\nfile\ta.txt\nfile\tb.rs");

    let filtered = call(&tool, r#"{"path":".","glob":"*.rs"}"#);
    assert_eq!(content_of(&filtered), "file\tb.rs");
}

#[test]
fn list_truncates_with_a_marker_and_reports_totals() {
    let root = TempRoot::new();
    for index in 0..5 {
        root.file(&format!("f{index}.txt"), b"x");
    }
    let tool = ListTool::new(&root.path).with_max_entries(2);
    let result = call(&tool, r#"{"path":"."}"#);
    let content = content_of(&result);
    assert!(content.contains("[list truncated: showing first 2 of 5 entries]"));
    let display = result.display().expect("display present");
    assert!(
        display
            .facts()
            .iter()
            .any(|f| f.name() == "entries_total" && f.value() == "5")
    );
}

#[test]
fn list_rejects_files_missing_paths_escapes_and_bad_globs() {
    let root = TempRoot::new();
    root.file("plain.txt", b"x");
    let tool = ListTool::new(&root.path);
    assert_eq!(
        error_code_of(&call(&tool, r#"{"path":"plain.txt"}"#)),
        error_code::NOT_A_DIRECTORY
    );
    assert_eq!(
        error_code_of(&call(&tool, r#"{"path":"ghost"}"#)),
        error_code::NOT_FOUND
    );
    assert_eq!(
        error_code_of(&call(&tool, r#"{"path":"../"}"#)),
        error_code::OUTSIDE_ROOT
    );
    assert_eq!(
        error_code_of(&call(&tool, r#"{"path":".","glob":"[unclosed"}"#)),
        error_code::INVALID_GLOB
    );
}

// ------------------------------- grep ------------------------------------

#[test]
fn grep_finds_matches_recursively_with_locations() {
    let root = TempRoot::new();
    root.file("src/a.rs", b"fn alpha() {}\nstruct Beta;\n");
    root.file("src/sub/b.rs", b"fn beta() {}\n");
    root.file("notes.md", b"alpha note\n");
    let tool = GrepTool::new(&root.path);

    let result = call(&tool, r#"{"pattern":"alpha"}"#);
    let content = content_of(&result);
    assert!(content.contains("notes.md:1:alpha note"));
    assert!(content.contains("src/a.rs:1:fn alpha() {}"));

    let scoped = call(&tool, r#"{"pattern":"fn ","path":"src","glob":"*.rs"}"#);
    let content = content_of(&scoped);
    assert!(content.contains("a.rs:1:fn alpha() {}"));
    assert!(content.contains("sub/b.rs:1:fn beta() {}"));
}

#[test]
fn grep_skips_binaries_reports_no_matches_and_truncates() {
    let root = TempRoot::new();
    root.file("bin.dat", &[0x00, 0x01, 0x02]);
    root.file("text.txt", b"needle\nneedle\nneedle\n");
    let tool = GrepTool::new(&root.path).with_max_matches(2);

    let none = call(&tool, r#"{"pattern":"absent-pattern"}"#);
    assert!(content_of(&none).contains("no matches"));

    let truncated = call(&tool, r#"{"pattern":"needle"}"#);
    let content = content_of(&truncated);
    assert!(content.contains("[grep truncated: showing first 2 of 3 matches]"));
    let display = truncated.display().expect("display present");
    assert!(
        display.detail().matches("needle").count() >= 3,
        "display carries the full untruncated matches"
    );
}

#[test]
fn grep_rejects_invalid_regex_as_a_business_error() {
    let root = TempRoot::new();
    let tool = GrepTool::new(&root.path);
    assert_eq!(
        error_code_of(&call(&tool, r#"{"pattern":"[unclosed"}"#)),
        error_code::INVALID_REGEX
    );
}

// ------------------------------- write -----------------------------------

#[test]
fn write_creates_with_parents_and_reports_overwrites() {
    let root = TempRoot::new();
    let tool = WriteTool::new(&root.path);

    let created = call(&tool, r#"{"path":"new/dir/file.txt","content":"first"}"#);
    assert_eq!(content_of(&created), "created new/dir/file.txt (5 bytes)");
    assert_eq!(
        std::fs::read_to_string(root.path.join("new/dir/file.txt")).unwrap(),
        "first"
    );

    let overwrote = call(&tool, r#"{"path":"new/dir/file.txt","content":"second!"}"#);
    assert_eq!(
        content_of(&overwrote),
        "overwrote new/dir/file.txt (7 bytes, was 5 bytes)"
    );
    let display = overwrote.display().expect("display present");
    assert!(
        display.detail().contains("second!"),
        "display carries the written text"
    );
}

#[test]
fn write_rejects_escapes_and_directories() {
    let root = TempRoot::new();
    std::fs::create_dir_all(root.path.join("subdir")).expect("dir");
    let tool = WriteTool::new(&root.path);
    assert_eq!(
        error_code_of(&call(&tool, r#"{"path":"../escape.txt","content":"x"}"#)),
        error_code::OUTSIDE_ROOT
    );
    assert_eq!(
        error_code_of(&call(&tool, r#"{"path":"subdir","content":"x"}"#)),
        error_code::NOT_A_FILE
    );
    assert_eq!(
        error_code_of(&call(&tool, r#"{"path":"file.txt"}"#)),
        error_code::INVALID_ARGUMENTS
    );
}

// ------------------------------- edit ------------------------------------

#[test]
fn edit_replaces_exactly_one_occurrence() {
    let root = TempRoot::new();
    root.file("code.rs", b"fn old_name() {}\ncall(old_value);\n");
    let tool = EditTool::new(&root.path);

    let result = call(
        &tool,
        r#"{"path":"code.rs","old_string":"fn old_name()","new_string":"fn new_name()"}"#,
    );
    assert!(content_of(&result).starts_with("edited code.rs: replaced 1 occurrence"));
    assert_eq!(
        std::fs::read_to_string(root.path.join("code.rs")).unwrap(),
        "fn new_name() {}\ncall(old_value);\n"
    );
    let display = result.display().expect("display present");
    assert!(display.detail().contains("--- old"));
}

#[test]
fn edit_distinguishes_no_match_from_not_unique() {
    let root = TempRoot::new();
    root.file("dup.txt", b"same\nsame\n");
    let tool = EditTool::new(&root.path);

    assert_eq!(
        error_code_of(&call(
            &tool,
            r#"{"path":"dup.txt","old_string":"absent","new_string":"x"}"#
        )),
        error_code::NO_MATCH
    );
    assert_eq!(
        error_code_of(&call(
            &tool,
            r#"{"path":"dup.txt","old_string":"same","new_string":"x"}"#
        )),
        error_code::NOT_UNIQUE
    );
    assert_eq!(
        error_code_of(&call(
            &tool,
            r#"{"path":"ghost.txt","old_string":"a","new_string":"b"}"#
        )),
        error_code::NOT_FOUND
    );
    assert_eq!(
        error_code_of(&call(
            &tool,
            r#"{"path":"dup.txt","old_string":"","new_string":"x"}"#
        )),
        error_code::INVALID_ARGUMENTS
    );
}

#[test]
fn edit_supports_deletion_with_an_empty_new_string() {
    let root = TempRoot::new();
    root.file("del.txt", b"keep REMOVE keep");
    let tool = EditTool::new(&root.path);
    let result = call(
        &tool,
        r#"{"path":"del.txt","old_string":" REMOVE","new_string":""}"#,
    );
    assert!(content_of(&result).starts_with("edited del.txt"));
    assert_eq!(
        std::fs::read_to_string(root.path.join("del.txt")).unwrap(),
        "keep keep"
    );
}

// ------------------------------- shell -----------------------------------

fn shell_call(tool: &ShellTool, arguments: &str) -> RichToolResult {
    let arguments = ToolArguments::parse(arguments).expect("valid JSON");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(tool.call(arguments))
}

#[test]
fn shell_reports_exit_code_zero_with_output() {
    let root = TempRoot::new();
    let tool = ShellTool::new(&root.path);
    let result = shell_call(&tool, r#"{"command":"echo shell-probe"}"#);
    let content = content_of(&result);
    assert!(content.starts_with("exit_code: 0\n"), "{content}");
    assert!(content.contains("shell-probe"));
    let display = result.display().expect("display present");
    assert!(
        display
            .facts()
            .iter()
            .any(|f| f.name() == "exit_code" && f.value() == "0")
    );
    assert!(display.facts().iter().any(|f| f.name() == "duration_ms"));
}

#[test]
fn shell_nonzero_exit_code_is_a_normal_result() {
    let root = TempRoot::new();
    let tool = ShellTool::new(&root.path);
    let result = shell_call(&tool, r#"{"command":"exit 3"}"#);
    let content = content_of(&result);
    assert!(
        content.starts_with("exit_code: 3"),
        "non-zero exit is Success carrying the code: {content}"
    );
}

#[test]
fn shell_runs_in_the_workspace_root() {
    let root = TempRoot::new();
    root.file("marker.txt", b"cwd-probe");
    let tool = ShellTool::new(&root.path);
    let command = if cfg!(windows) {
        r#"{"command":"Get-Content marker.txt"}"#
    } else {
        r#"{"command":"cat marker.txt"}"#
    };
    let result = shell_call(&tool, command);
    assert!(content_of(&result).contains("cwd-probe"));
}

#[test]
fn shell_timeout_is_a_business_error_with_display() {
    let root = TempRoot::new();
    let tool = ShellTool::new(&root.path).with_max_timeout_secs(600);
    let command = if cfg!(windows) {
        r#"{"command":"Start-Sleep -Seconds 30","timeout_secs":1}"#
    } else {
        r#"{"command":"sleep 30","timeout_secs":1}"#
    };
    let result = shell_call(&tool, command);
    assert_eq!(error_code_of(&result), error_code::TIMEOUT);
    let ToolResult::Error { message, .. } = result.result() else {
        panic!("expected error");
    };
    assert!(message.contains("1s"), "message names the limit: {message}");
    assert!(
        result.display().is_some(),
        "timeout may carry display facts"
    );
}

#[test]
fn shell_rejects_out_of_range_timeouts() {
    let root = TempRoot::new();
    let tool = ShellTool::new(&root.path).with_max_timeout_secs(10);
    assert_eq!(
        error_code_of(&shell_call(
            &tool,
            r#"{"command":"echo x","timeout_secs":0}"#
        )),
        error_code::INVALID_ARGUMENTS
    );
    assert_eq!(
        error_code_of(&shell_call(
            &tool,
            r#"{"command":"echo x","timeout_secs":99}"#
        )),
        error_code::INVALID_ARGUMENTS
    );
}

#[test]
fn shell_scrubs_credential_environment_variables() {
    let root = TempRoot::new();
    let tool = ShellTool::new(&root.path);
    // SAFETY: test-local env mutation; keys are unique to this test.
    unsafe {
        std::env::set_var("PHILO_TEST_API_KEY", "super-secret-value");
        std::env::set_var("PHILO_TEST_PLAIN", "plain-value");
    }
    let command = if cfg!(windows) {
        r#"{"command":"Write-Output \"k=[$env:PHILO_TEST_API_KEY] p=[$env:PHILO_TEST_PLAIN]\""}"#
    } else {
        r#"{"command":"echo \"k=[$PHILO_TEST_API_KEY] p=[$PHILO_TEST_PLAIN]\""}"#
    };
    let result = shell_call(&tool, command);
    let content = content_of(&result);
    let display_detail = result.display().expect("display").detail().to_owned();
    unsafe {
        std::env::remove_var("PHILO_TEST_API_KEY");
        std::env::remove_var("PHILO_TEST_PLAIN");
    }
    assert!(
        !content.contains("super-secret-value"),
        "credentials must not reach the model channel: {content}"
    );
    assert!(
        !display_detail.contains("super-secret-value"),
        "credentials must not reach the display channel: {display_detail}"
    );
    assert!(
        content.contains("plain-value"),
        "non-credential variables pass through: {content}"
    );
}

#[test]
fn shell_truncates_long_output_with_a_marker() {
    let root = TempRoot::new();
    let tool = ShellTool::new(&root.path).with_max_output_lines(2);
    let command = if cfg!(windows) {
        r#"{"command":"1..5 | ForEach-Object { Write-Output \"line-$_\" }"}"#
    } else {
        r#"{"command":"for i in 1 2 3 4 5; do echo line-$i; done"}"#
    };
    let result = shell_call(&tool, command);
    let content = content_of(&result);
    assert!(content.contains("line-1"));
    assert!(!content.contains("line-5"));
    assert!(content.contains("[shell output truncated"), "{content}");
    let display = result.display().expect("display present");
    assert!(
        display.detail().contains("line-5"),
        "display keeps the full output"
    );
}

// --------------------------- effect classes ------------------------------

#[test]
fn effect_classes_are_fixed_per_tool() {
    assert_eq!(ReadToolClass::get(), EffectClass::ReadOnly);
    assert_eq!(ListTool::definition().effect_class(), EffectClass::ReadOnly);
    assert_eq!(GrepTool::definition().effect_class(), EffectClass::ReadOnly);
    assert_eq!(
        WriteTool::definition().effect_class(),
        EffectClass::Workspace
    );
    assert_eq!(
        EditTool::definition().effect_class(),
        EffectClass::Workspace
    );
    assert_eq!(ShellTool::definition().effect_class(), EffectClass::System);
}

struct ReadToolClass;
impl ReadToolClass {
    fn get() -> EffectClass {
        philo_tools_std::ReadTool::definition().effect_class()
    }
}
