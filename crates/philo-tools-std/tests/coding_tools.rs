//! TOOL-004: list / grep / write / edit / shell behavior contracts —
//! success paths, business-error taxonomy, truncation marks, containment,
//! and shell credential scrubbing. CI-offline (shell uses local commands).

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolCancel, ToolDisplay, ToolHandler, ToolInvokeCx,
    ToolInvokeEnd, ToolProgressSink, ToolResult,
};
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

fn fact<'a>(display: &'a ToolDisplay, name: &str) -> &'a str {
    display
        .facts()
        .iter()
        .find(|fact| fact.name() == name)
        .map(|fact| fact.value())
        .unwrap_or_else(|| panic!("missing fact {name}"))
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
    assert!(
        content.contains("[list truncated: showing first 2 of 5 entries"),
        "marker states the truncation explicitly: {content}"
    );
    assert!(
        content.contains("raise \"limit\""),
        "marker tells the model how to see more: {content}"
    );
    let display = result.display().expect("display present");
    assert_eq!(fact(display, "title"), "List Directory");
    assert_eq!(fact(display, "subject"), ".");
    assert_eq!(fact(display, "count"), "1 directory");
    assert!(
        display
            .facts()
            .iter()
            .any(|f| f.name() == "entries_total" && f.value() == "5")
    );
}

#[test]
fn list_reports_empty_directories_explicitly() {
    let root = TempRoot::new();
    std::fs::create_dir_all(root.path.join("void")).expect("dir");
    let tool = ListTool::new(&root.path);
    let result = call(&tool, r#"{"path":"void"}"#);
    assert_eq!(content_of(&result), "(empty directory)");
}

#[test]
fn list_limit_argument_caps_the_call_and_defaults_path_to_root() {
    let root = TempRoot::new();
    for index in 0..4 {
        root.file(&format!("f{index}.txt"), b"x");
    }
    let tool = ListTool::new(&root.path);

    // No path: the root itself is listed.
    let default_root = call(&tool, "{}");
    assert_eq!(content_of(&default_root).lines().count(), 4);

    // Call-level limit caps the listing; the marker reports the true total.
    let limited = call(&tool, r#"{"limit":2}"#);
    let content = content_of(&limited);
    assert!(
        content.contains("[list truncated: showing first 2 of 4 entries"),
        "{content}"
    );

    // A call limit above the configured cap is clamped to it.
    let clamped = call(
        &ListTool::new(&root.path).with_max_entries(3),
        r#"{"limit":99}"#,
    );
    let content = content_of(&clamped);
    assert!(
        content.contains("[list truncated: showing first 3 of 4 entries"),
        "{content}"
    );
}

#[test]
fn list_sorts_names_case_insensitively() {
    let root = TempRoot::new();
    root.file("B.txt", b"b");
    root.file("a.txt", b"a");
    root.file("Zebra", b"z");
    root.file("apple2", b"a");
    let tool = ListTool::new(&root.path);
    let content = call(&tool, r#"{"path":"."}"#);
    let names: Vec<&str> = content_of(&content)
        .lines()
        .map(|l| l.split('\t').nth(1).expect("name"))
        .collect();
    assert_eq!(names, vec!["a.txt", "apple2", "B.txt", "Zebra"]);
}

#[test]
fn list_classifies_directory_symlinks_and_skips_unstatable_entries() {
    let root = TempRoot::new();
    std::fs::create_dir_all(root.path.join("real-dir")).expect("dir");
    root.file("plain.txt", b"x");

    let link = root.path.join("linked-dir");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(root.path.join("real-dir"), &link).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(root.path.join("real-dir"), &link).is_ok();
    if !linked {
        // Symlink creation needs privileges on some Windows setups; the
        // classification path is still covered on unix and privileged wins.
        return;
    }

    let tool = ListTool::new(&root.path);
    let result = call(&tool, r#"{"path":"."}"#);
    let content = content_of(&result);
    assert!(
        content.contains("dir\tlinked-dir"),
        "symlinked directories are typed as dir: {content}"
    );
}

#[test]
fn list_byte_cap_truncates_with_the_same_marker() {
    let root = TempRoot::new();
    for index in 0..6 {
        root.file(&format!("f{index}.txt"), b"x");
    }
    // Each row is "file\tfN.txt\n" (12-13 bytes); a 40-byte budget fits 3.
    let tool = ListTool::new(&root.path).with_max_bytes(40);
    let result = call(&tool, r#"{"path":"."}"#);
    let content = content_of(&result);
    assert!(
        content.contains("[list truncated: showing first 3 of 6 entries"),
        "byte cap truncates at a row boundary with the shared marker: {content}"
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

    // Scanning stops at the match limit: only the first two matches exist
    // on the channels, and the marker says how to proceed.
    let truncated = call(&tool, r#"{"pattern":"needle"}"#);
    let content = content_of(&truncated);
    assert!(content.contains("needle"));
    assert!(
        content.contains("[grep truncated: match limit of 2 reached"),
        "{content}"
    );
    let display = truncated.display().expect("display present");
    assert_eq!(display.detail(), "text.txt:1\ntext.txt:2");
    assert!(
        !display.detail().contains("needle"),
        "locs omit the matched line text"
    );
    assert_eq!(fact(display, "matches_total"), "2");
    assert_eq!(fact(display, "limit_reached"), "true");
    assert_eq!(fact(display, "title"), "Grep");
    assert_eq!(fact(display, "count"), "1 search");
    assert_eq!(fact(display, "subject"), "\"needle\"");
    assert_eq!(fact(display, "verb"), "Searched");
    assert_eq!(fact(display, "body"), "locs");
}

#[test]
fn grep_truncates_overlong_match_lines() {
    let root = TempRoot::new();
    let long_line = "x".repeat(2000);
    root.file(
        "wide.txt",
        format!("{long_line}\nshort needle\n").as_bytes(),
    );
    let tool = GrepTool::new(&root.path);
    let result = call(&tool, r#"{"pattern":"x|needle"}"#);
    let content = content_of(&result);
    let wide_row = content
        .lines()
        .find(|line| line.contains("wide.txt:1:"))
        .expect("match row for the oversized line");
    assert!(
        wide_row.chars().count() < 600 && wide_row.ends_with("... [truncated]"),
        "oversized lines are capped with a suffix: {wide_row}"
    );
    assert!(
        content.contains("short needle"),
        "normal lines pass through"
    );
}

#[test]
fn grep_byte_cap_truncates_output() {
    let root = TempRoot::new();
    root.file(
        "big.txt",
        b"matchme matchme matchme matchme\n".repeat(64).as_slice(),
    );
    let tool = GrepTool::new(&root.path)
        .with_max_matches(64)
        .with_max_bytes(256);
    let result = call(&tool, r#"{"pattern":"matchme"}"#);
    let content = content_of(&result);
    assert!(
        content.contains("output exceeded 256 bytes"),
        "byte cap is reported in the truncation marker: {content}"
    );
}

#[test]
fn grep_ignore_case_matches() {
    let root = TempRoot::new();
    root.file("case.txt", b"Needle In Haystack\n");
    let tool = GrepTool::new(&root.path);

    let sensitive = call(&tool, r#"{"pattern":"needle"}"#);
    assert!(content_of(&sensitive).contains("no matches"));

    let insensitive = call(&tool, r#"{"pattern":"needle","ignore_case":true}"#);
    assert!(content_of(&insensitive).contains("case.txt:1:Needle In Haystack"));
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
        display.detail().contains("+second!"),
        "display carries plus-prefixed written lines"
    );
    assert!(!display.detail().contains("wrote "));
    assert_eq!(fact(display, "title"), "Write");
    assert_eq!(fact(display, "subject"), "new/dir/file.txt");
    assert_eq!(
        fact(display, "result"),
        "Succeeded. File overwritten.  (+1 added)"
    );
    assert_eq!(fact(display, "verb"), "Wrote");
    assert_eq!(fact(display, "body"), "diff");
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

#[test]
fn write_already_requested_cancel_does_not_create_the_file() {
    let root = TempRoot::new();
    let tool = WriteTool::new(&root.path);
    let cancel = ToolCancel::new();
    cancel.request();
    let arguments =
        ToolArguments::parse(r#"{"path":"stopped.txt","content":"nope"}"#).expect("valid JSON");
    let end = block_on(tool.call_with_cx(
        arguments,
        ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
    ));
    assert_eq!(end, ToolInvokeEnd::Stopped);
    assert!(
        !root.path.join("stopped.txt").exists(),
        "cancelled write must not create the target"
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
    assert!(display.detail().contains("-fn old_name()"));
    assert!(display.detail().contains("+fn new_name()"));
    assert!(
        display.detail().contains("call(old_value);"),
        "hunk includes file context: {}",
        display.detail()
    );
    assert_eq!(fact(display, "title"), "Edit");
    assert_eq!(fact(display, "subject"), "code.rs");
    assert_eq!(
        fact(display, "result"),
        "Succeeded. File edited.  (+1 added, -1 removed)"
    );
    assert_eq!(fact(display, "verb"), "Edited");
    assert_eq!(fact(display, "body"), "diff");
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

#[test]
fn edit_matches_lf_old_string_in_a_crlf_file_and_preserves_crlf() {
    let root = TempRoot::new();
    root.file("win.txt", b"first line\r\nold_name();\r\nlast\r\n");
    let tool = EditTool::new(&root.path);
    let result = call(
        &tool,
        r#"{"path":"win.txt","old_string":"old_name();","new_string":"new_name();"}"#,
    );
    assert!(
        content_of(&result).starts_with("edited win.txt"),
        "an LF old_string must hit a CRLF file: {:?}",
        result.result()
    );
    assert_eq!(
        std::fs::read_to_string(root.path.join("win.txt")).unwrap(),
        "first line\r\nnew_name();\r\nlast\r\n",
        "the file's CRLF endings are preserved"
    );
}

#[test]
fn edit_preserves_a_utf8_bom() {
    let root = TempRoot::new();
    root.file("bom.txt", b"\xEF\xBB\xBFheader value\n");
    let tool = EditTool::new(&root.path);
    let result = call(
        &tool,
        r#"{"path":"bom.txt","old_string":"header value","new_string":"title"}"#,
    );
    assert!(content_of(&result).starts_with("edited bom.txt"));
    assert_eq!(
        std::fs::read(root.path.join("bom.txt")).unwrap(),
        b"\xEF\xBB\xBFtitle\n",
        "the BOM survives the replacement"
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
    assert_eq!(fact(display, "title"), "Run");
    assert_eq!(fact(display, "subject"), "echo shell-probe");
    assert_eq!(fact(display, "count"), "1 command");
    let result_phrase = fact(display, "result");
    assert!(
        result_phrase.starts_with("exit 0 · "),
        "shell result names the exit code: {result_phrase}"
    );
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
    let display = result.display().expect("timeout carries display");
    assert_eq!(fact(display, "title"), "Run");
    assert_eq!(fact(display, "count"), "1 command");
    assert!(
        !display.facts().iter().any(|f| f.name() == "result"),
        "timeout emits no result phrase; the error channel reports it"
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

fn shell_call_cx(tool: &ShellTool, arguments: &str, cx: ToolInvokeCx) -> ToolInvokeEnd {
    let arguments = ToolArguments::parse(arguments).expect("valid JSON");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(tool.call_with_cx(arguments, cx))
}

#[test]
fn shell_already_requested_cancel_returns_stopped_not_timeout() {
    let root = TempRoot::new();
    let tool = ShellTool::new(&root.path);
    let cancel = ToolCancel::new();
    cancel.request();
    let command = if cfg!(windows) {
        r#"{"command":"Start-Sleep -Seconds 30","timeout_secs":5}"#
    } else {
        r#"{"command":"sleep 30","timeout_secs":5}"#
    };
    let end = shell_call_cx(
        &tool,
        command,
        ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
    );
    assert_eq!(end, ToolInvokeEnd::Stopped);
}

#[test]
fn shell_cancel_after_spawn_returns_stopped_not_timeout() {
    let root = TempRoot::new();
    let tool = ShellTool::new(&root.path).with_max_timeout_secs(600);
    let cancel = ToolCancel::new();
    let cancel_later = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        cancel_later.request();
    });
    let command = if cfg!(windows) {
        r#"{"command":"Start-Sleep -Seconds 30","timeout_secs":20}"#
    } else {
        r#"{"command":"sleep 30","timeout_secs":20}"#
    };
    let end = shell_call_cx(
        &tool,
        command,
        ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
    );
    assert_eq!(end, ToolInvokeEnd::Stopped);
}

#[test]
fn shell_truncates_long_output_keeping_the_tail() {
    let root = TempRoot::new();
    let tool = ShellTool::new(&root.path).with_max_output_lines(2);
    let command = if cfg!(windows) {
        r#"{"command":"1..5 | ForEach-Object { Write-Output \"line-$_\" }"}"#
    } else {
        r#"{"command":"for i in 1 2 3 4 5; do echo line-$i; done"}"#
    };
    let result = shell_call(&tool, command);
    let content = content_of(&result);
    // Errors and summaries live at the end of command output, so the tail
    // is what survives truncation.
    assert!(content.contains("line-5"));
    assert!(!content.contains("line-1"), "{content}");
    assert!(
        content.contains("[shell output truncated: showing last 2 of 5 lines"),
        "{content}"
    );
    let display = result.display().expect("display present");
    assert!(
        display.detail().contains("line-1"),
        "small outputs still fit in the display cap"
    );
}

#[test]
fn shell_streams_progress_and_caps_display() {
    use std::sync::{Arc, Mutex};
    let root = TempRoot::new();
    let tool = ShellTool::new(&root.path).with_max_display_bytes(16);
    let pushed = Arc::new(Mutex::new(String::new()));
    let pushed_for_sink = Arc::clone(&pushed);
    let sink = ToolProgressSink::from_fn(move |text| {
        pushed_for_sink.lock().expect("pushed").push_str(text);
    });
    let command = if cfg!(windows) {
        r#"{"command":"Write-Output \"abcdefghijklmnopqrstuvwxyz\""}"#
    } else {
        r#"{"command":"printf abcdefghijklmnopqrstuvwxyz"}"#
    };
    let arguments = ToolArguments::parse(command).expect("valid JSON");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let end = runtime.block_on(tool.call_with_cx(arguments, ToolInvokeCx::progress_only(sink)));
    let result = end
        .into_done()
        .expect("progress-only context never requests cancel");
    let live = pushed.lock().expect("pushed").clone();
    assert!(
        live.contains("abcd"),
        "incremental output reached the sink: {live:?}"
    );
    let display = result.display().expect("display").detail().to_owned();
    assert!(display.len() <= 16, "display is capped: {display:?}");
    assert_eq!(
        result
            .display()
            .unwrap()
            .facts()
            .iter()
            .find(|fact| fact.name() == "truncated")
            .map(|fact| fact.value()),
        Some("true")
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
