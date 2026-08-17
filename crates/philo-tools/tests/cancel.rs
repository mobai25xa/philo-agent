//! TOOL-004: cancel token, invoke context, and Stopped vs Done.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use philo_tools::*;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::pin::pin;
    let mut cx = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

#[test]
fn cancel_token_wakes_waiters() {
    let cancel = ToolCancel::new();
    assert!(!cancel.is_requested());
    let waiter = cancel.cancelled();
    cancel.request();
    assert!(cancel.is_requested());
    block_on(waiter);
    cancel.request();
}

#[test]
fn closure_handler_ignores_cancel_and_completes() {
    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::simple("echo", "echoes", EffectClass::ReadOnly),
            |_args: ToolArguments| async { RichToolResult::success("ok") },
        )
        .unwrap()
        .build();
    let cancel = ToolCancel::new();
    cancel.request();
    let end = block_on(registry.invoke(
        ToolInvocation::new("c", "echo", "{}"),
        ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
    ))
    .unwrap();
    let done = end.into_done().expect("default handler completes");
    assert_eq!(done.result().content(), Some("ok"));
}

#[test]
fn handler_can_return_stopped() {
    struct Stops;
    impl ToolHandler for Stops {
        fn call<'a>(&'a self, _arguments: ToolArguments) -> ToolHandlerFuture<'a> {
            Box::pin(async { RichToolResult::success("should not run") })
        }
        fn call_with_cx<'a>(
            &'a self,
            _arguments: ToolArguments,
            cx: ToolInvokeCx,
        ) -> ToolHandlerEndFuture<'a> {
            Box::pin(async move {
                if cx.cancel().is_requested() {
                    ToolInvokeEnd::Stopped
                } else {
                    ToolInvokeEnd::Done(RichToolResult::success("ran"))
                }
            })
        }
    }

    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::simple("stop", "stops", EffectClass::ReadOnly),
            Stops,
        )
        .unwrap()
        .build();
    let cancel = ToolCancel::new();
    cancel.request();
    let end = block_on(registry.invoke(
        ToolInvocation::new("c", "stop", "{}"),
        ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
    ))
    .unwrap();
    assert_eq!(end, ToolInvokeEnd::Stopped);
}

#[test]
fn synthesized_errors_stay_done_and_ignore_token() {
    let registry = ToolRegistry::empty();
    let cancel = ToolCancel::new();
    cancel.request();
    let end = block_on(registry.invoke(
        ToolInvocation::new("c", "missing", "{}"),
        ToolInvokeCx::new(ToolProgressSink::noop(), cancel),
    ))
    .unwrap();
    let done = end.into_done().expect("unknown tool is Done");
    assert_eq!(done.result().as_error().unwrap().code(), "unknown_tool");
    assert!(done.display().is_none());
}

#[test]
fn push_text_after_cancel_does_not_panic() {
    let seen = Arc::new(Mutex::new(String::new()));
    let seen_sink = Arc::clone(&seen);
    let sink = ToolProgressSink::from_fn(move |text| {
        seen_sink.lock().expect("seen").push_str(text);
    });
    let cancel = ToolCancel::new();
    cancel.request();
    let cx = ToolInvokeCx::new(sink, cancel);
    cx.progress().push_text("after-cancel");
    assert_eq!(seen.lock().expect("seen").as_str(), "after-cancel");
}
