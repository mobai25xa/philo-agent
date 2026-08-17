//! Bounded blocking pool: saturation, join panic, cooperative cancel, and
//! `shell` bypass. Existing `read` / `coding_tools` tests cover unwrapped
//! handlers; this file covers the Wave 1 executor.

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use philo_tools::{
    EffectClass, RichToolResult, ToolArguments, ToolCancel, ToolDefinition, ToolHandler,
    ToolHandlerFuture, ToolInvocation, ToolInvokeCx, ToolInvokeEnd, ToolPort, ToolProgressSink,
    ToolRegistry, ToolResult,
};
use philo_tools_std::{
    BlockingPool, BlockingToolExecutor, GREP_TOOL_NAME, GrepTool, LIST_TOOL_NAME, ListTool,
    READ_TOOL_NAME, ReadTool, SHELL_TOOL_NAME, blocking_fs_handler, error_code,
};

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
            "philo-tools-blocking-{}-{}",
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

struct HoldHandler {
    started: mpsc::Sender<()>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
}

impl ToolHandler for HoldHandler {
    fn call<'a>(&'a self, _arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin(async move {
            let _ = self.started.send(());
            let rx = self
                .release
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
                .expect("hold handler invoked once");
            let _ = rx.recv();
            RichToolResult::success("held")
        })
    }
}

struct ReleaseOnDrop(Option<mpsc::Sender<()>>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

struct Boom;

impl ToolHandler for Boom {
    fn call<'a>(&'a self, _arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin(async move { panic!("boom from test handler") })
    }
}

struct AsyncShell;

impl ToolHandler for AsyncShell {
    fn call<'a>(&'a self, _arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(15)).await;
            RichToolResult::success("shell-ok")
        })
    }
}

fn hold_pair() -> (HoldHandler, mpsc::Receiver<()>, ReleaseOnDrop) {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let handler = HoldHandler {
        started: started_tx,
        release: Mutex::new(Some(release_rx)),
    };
    (handler, started_rx, ReleaseOnDrop(Some(release_tx)))
}

fn empty_args() -> ToolArguments {
    ToolArguments::parse("{}").expect("empty object")
}

fn invoke_named(name: &str) -> ToolInvocation {
    ToolInvocation::new("call-1", name, "{}")
}

async fn wait_started(started_rx: mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || {
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("handler should start")
    })
    .await
    .expect("wait-started join");
}

// ------------------------------- cancel ----------------------------------

#[test]
fn list_already_requested_cancel_returns_stopped() {
    let root = TempRoot::new();
    root.file("a.txt", b"x");
    let tool = ListTool::new(&root.path);
    let cancel = ToolCancel::new();
    cancel.request();
    let arguments = ToolArguments::parse(r#"{"path":"."}"#).expect("valid JSON");
    let end = block_on(tool.call_with_cx(
        arguments,
        ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
    ));
    assert_eq!(end, ToolInvokeEnd::Stopped);
}

#[test]
fn list_observes_cancel_during_a_large_directory() {
    let root = TempRoot::new();
    for index in 0..3000 {
        root.file(&format!("n{index:04}.txt"), b"x");
    }
    let tool = ListTool::new(&root.path);
    let cancel = ToolCancel::new();
    let later = cancel.clone();
    let arguments = ToolArguments::parse(r#"{"path":"."}"#).expect("valid JSON");
    let cx = ToolInvokeCx::new(ToolProgressSink::noop(), cancel);
    let handle = std::thread::spawn(move || block_on(tool.call_with_cx(arguments, cx)));
    later.request();
    let end = handle.join().expect("list thread");
    assert_eq!(end, ToolInvokeEnd::Stopped);
}

#[test]
fn grep_already_requested_cancel_returns_stopped() {
    let root = TempRoot::new();
    root.file("a.txt", b"needle");
    let tool = GrepTool::new(&root.path);
    let cancel = ToolCancel::new();
    cancel.request();
    let arguments = ToolArguments::parse(r#"{"pattern":"needle"}"#).expect("valid JSON");
    let end = block_on(tool.call_with_cx(
        arguments,
        ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
    ));
    assert_eq!(end, ToolInvokeEnd::Stopped);
}

#[test]
fn grep_observes_cancel_during_a_large_tree() {
    let root = TempRoot::new();
    let body = "aaaaaaaa\n".repeat(20_000);
    for dir in 0..8 {
        for file in 0..8 {
            root.file(&format!("d{dir}/f{file}.txt"), body.as_bytes());
        }
    }
    let tool = GrepTool::new(&root.path);
    let cancel = ToolCancel::new();
    let later = cancel.clone();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_micros(200) {
            std::hint::spin_loop();
        }
        later.request();
    });
    let arguments = ToolArguments::parse(r#"{"pattern":"zzzz-absent"}"#).expect("valid JSON");
    let end = block_on(tool.call_with_cx(
        arguments,
        ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
    ));
    assert_eq!(end, ToolInvokeEnd::Stopped);
}

// ------------------------------- pool ------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn wrap_handler_runs_read_on_the_pool() {
    let root = TempRoot::new();
    root.file("hello.txt", b"hello pool");
    let pool = BlockingPool::new(2, 0);
    let handler = blocking_fs_handler(&pool, ReadTool::new(&root.path));
    let arguments = ToolArguments::parse(r#"{"path":"hello.txt"}"#).expect("valid JSON");
    let end = handler
        .invoke(arguments, ToolInvokeCx::ignore())
        .await
        .expect("pool must accept the call");
    let result = end.into_done().expect("read completed");
    assert!(
        result
            .result()
            .content()
            .expect("success")
            .contains("hello pool")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wrap_handler_saturation_returns_busy_without_hanging() {
    let pool = BlockingPool::new(1, 0);
    let (hold, started_rx, _release) = hold_pair();
    let handler = pool.wrap_handler(hold);
    let first = {
        let handler = handler.clone();
        tokio::spawn(async move { handler.invoke(empty_args(), ToolInvokeCx::ignore()).await })
    };
    wait_started(started_rx).await;
    let second = tokio::time::timeout(
        Duration::from_secs(1),
        handler.invoke(empty_args(), ToolInvokeCx::ignore()),
    )
    .await
    .expect("busy must not wait on the runtime worker");
    let error = second.expect_err("second invoke must be infrastructure busy");
    assert!(
        error.message().contains("busy"),
        "busy message: {}",
        error.message()
    );
    drop(first);
}

#[tokio::test(flavor = "multi_thread")]
async fn executor_saturation_returns_tool_port_error_busy() {
    let root = TempRoot::new();
    let (hold, started_rx, _release) = hold_pair();
    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::simple("hold", "test hold", EffectClass::ReadOnly),
            hold,
        )
        .expect("register hold")
        .register(ReadTool::definition(), ReadTool::new(&root.path))
        .expect("register read")
        .build();
    let executor = BlockingToolExecutor::new(registry, BlockingPool::new(1, 0));
    let first = {
        let executor = executor.clone();
        tokio::spawn(async move {
            executor
                .invoke(invoke_named("hold"), ToolInvokeCx::ignore())
                .await
        })
    };
    wait_started(started_rx).await;
    let second = tokio::time::timeout(
        Duration::from_secs(1),
        executor.invoke(
            ToolInvocation::new("call-2", READ_TOOL_NAME, r#"{"path":"missing.txt"}"#),
            ToolInvokeCx::ignore(),
        ),
    )
    .await
    .expect("busy must not hang");
    let error = second.expect_err("saturated pool is ToolPortError, not a business error");
    assert!(error.message().contains("busy"), "{}", error.message());
    drop(first);
}

#[tokio::test(flavor = "multi_thread")]
async fn executor_queue_slot_then_third_call_is_busy() {
    let root = TempRoot::new();
    root.file("ok.txt", b"queued");
    let (hold, started_rx, _release) = hold_pair();
    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::simple("hold", "test hold", EffectClass::ReadOnly),
            hold,
        )
        .expect("register hold")
        .register(ReadTool::definition(), ReadTool::new(&root.path))
        .expect("register read")
        .build();
    let executor = BlockingToolExecutor::new(registry, BlockingPool::new(1, 1));
    let first = {
        let executor = executor.clone();
        tokio::spawn(async move {
            executor
                .invoke(invoke_named("hold"), ToolInvokeCx::ignore())
                .await
        })
    };
    wait_started(started_rx).await;
    let queued = {
        let executor = executor.clone();
        tokio::spawn(async move {
            executor
                .invoke(
                    ToolInvocation::new("call-2", READ_TOOL_NAME, r#"{"path":"ok.txt"}"#),
                    ToolInvokeCx::ignore(),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    let third = tokio::time::timeout(
        Duration::from_secs(1),
        executor.invoke(
            ToolInvocation::new("call-3", READ_TOOL_NAME, r#"{"path":"ok.txt"}"#),
            ToolInvokeCx::ignore(),
        ),
    )
    .await
    .expect("busy must not hang");
    assert!(
        third.expect_err("queue is full").message().contains("busy"),
        "third call should be busy"
    );
    drop(first);
    drop(queued);
}

#[tokio::test(flavor = "multi_thread")]
async fn business_errors_stay_tool_result_error_not_port_error() {
    let root = TempRoot::new();
    let registry = ToolRegistry::builder()
        .register(ReadTool::definition(), ReadTool::new(&root.path))
        .expect("register read")
        .register(ListTool::definition(), ListTool::new(&root.path))
        .expect("register list")
        .register(GrepTool::definition(), GrepTool::new(&root.path))
        .expect("register grep")
        .build();
    let executor = BlockingToolExecutor::with_parallelism(registry, 4);

    let missing = executor
        .invoke(
            ToolInvocation::new("c1", READ_TOOL_NAME, r#"{"path":"missing.txt"}"#),
            ToolInvokeCx::ignore(),
        )
        .await
        .expect("missing file is not infrastructure failure");
    let result = missing.into_done().expect("Done");
    match result.result() {
        ToolResult::Error { code, .. } => assert_eq!(code, error_code::NOT_FOUND),
        ToolResult::Success { .. } => panic!("expected business error"),
    }

    let not_dir = executor
        .invoke(
            ToolInvocation::new("c2", LIST_TOOL_NAME, r#"{"path":"missing.txt"}"#),
            ToolInvokeCx::ignore(),
        )
        .await
        .expect("list of a missing path is not infrastructure failure");
    let result = not_dir.into_done().expect("Done");
    match result.result() {
        ToolResult::Error { code, .. } => {
            assert!(
                code == error_code::NOT_FOUND || code == error_code::NOT_A_DIRECTORY,
                "unexpected list error {code}"
            );
        }
        ToolResult::Success { .. } => panic!("expected business error"),
    }

    let bad_regex = executor
        .invoke(
            ToolInvocation::new("c3", GREP_TOOL_NAME, r#"{"pattern":"[unclosed"}"#),
            ToolInvokeCx::ignore(),
        )
        .await
        .expect("invalid regex is not infrastructure failure");
    let result = bad_regex.into_done().expect("Done");
    match result.result() {
        ToolResult::Error { code, .. } => assert_eq!(code, error_code::INVALID_REGEX),
        ToolResult::Success { .. } => panic!("expected business error"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn join_panic_becomes_tool_port_error() {
    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::simple("boom", "panics", EffectClass::ReadOnly),
            Boom,
        )
        .expect("register boom")
        .build();
    let executor = BlockingToolExecutor::new(registry, BlockingPool::new(1, 0));
    let error = executor
        .invoke(invoke_named("boom"), ToolInvokeCx::ignore())
        .await
        .expect_err("join panic is ToolPortError");
    assert!(
        error.message().contains("panic"),
        "panic message: {}",
        error.message()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_bypasses_the_blocking_pool() {
    let (hold, started_rx, _release) = hold_pair();
    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::simple("hold", "test hold", EffectClass::ReadOnly),
            hold,
        )
        .expect("register hold")
        .register(
            ToolDefinition::simple(SHELL_TOOL_NAME, "async shell", EffectClass::System),
            AsyncShell,
        )
        .expect("register shell")
        .build();
    let executor = BlockingToolExecutor::new(registry, BlockingPool::new(1, 0));
    let first = {
        let executor = executor.clone();
        tokio::spawn(async move {
            executor
                .invoke(invoke_named("hold"), ToolInvokeCx::ignore())
                .await
        })
    };
    wait_started(started_rx).await;
    let shell = tokio::time::timeout(
        Duration::from_secs(1),
        executor.invoke(invoke_named(SHELL_TOOL_NAME), ToolInvokeCx::ignore()),
    )
    .await
    .expect("shell must not wait for the filesystem pool")
    .expect("shell is not a port error");
    let result = shell.into_done().expect("shell Done");
    assert_eq!(result.result().content(), Some("shell-ok"));
    drop(first);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_discards_a_late_spawn_blocking_result() {
    let (hold, started_rx, _release) = hold_pair();
    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::simple("hold", "test hold", EffectClass::ReadOnly),
            hold,
        )
        .expect("register hold")
        .build();
    let executor = BlockingToolExecutor::new(registry, BlockingPool::new(1, 0));
    let cancel = ToolCancel::new();
    let first = {
        let executor = executor.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            executor
                .invoke(
                    invoke_named("hold"),
                    ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
                )
                .await
        })
    };
    wait_started(started_rx).await;
    cancel.request();
    let end = tokio::time::timeout(Duration::from_secs(1), first)
        .await
        .expect("cancel must not wait for the blocking worker")
        .expect("join spawned invoke");
    assert_eq!(end.expect("not a port error"), ToolInvokeEnd::Stopped);
}

#[tokio::test(flavor = "multi_thread")]
async fn wrapped_grep_cancel_on_a_large_tree_returns_stopped() {
    let root = TempRoot::new();
    let body = "bbbbbbbb\n".repeat(8_000);
    for dir in 0..6 {
        for file in 0..6 {
            root.file(&format!("t{dir}/g{file}.txt"), body.as_bytes());
        }
    }
    let pool = BlockingPool::new(2, 0);
    let handler = pool.wrap_handler(GrepTool::new(&root.path));
    let cancel = ToolCancel::new();
    let later = cancel.clone();
    let join = {
        let handler = handler.clone();
        tokio::spawn(async move {
            handler
                .invoke(
                    ToolArguments::parse(r#"{"pattern":"no-such-token"}"#).expect("valid JSON"),
                    ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(4)).await;
    later.request();
    let end = tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .expect("grep cancel must settle")
        .expect("join")
        .expect("not a port error");
    assert_eq!(end, ToolInvokeEnd::Stopped);
}
