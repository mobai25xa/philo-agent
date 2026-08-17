//! TOOL-003: dual-channel result model, effect classification, and the
//! registry's no-display synthesized errors.

use philo_tools::*;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};
    let mut cx = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

#[test]
fn rich_result_carries_both_channels() {
    let rich = RichToolResult::success("model text").with_display(
        ToolDisplay::new("full untruncated detail")
            .with_fact("bytes_total", "12345")
            .with_fact("truncated", "true"),
    );
    assert_eq!(rich.result().content(), Some("model text"));
    let display = rich.display().expect("display present");
    assert_eq!(display.detail(), "full untruncated detail");
    assert_eq!(display.facts().len(), 2);
    assert_eq!(display.facts()[0].name(), "bytes_total");
    assert_eq!(display.facts()[0].value(), "12345");

    let (result, display) = rich.into_parts();
    assert_eq!(result.content(), Some("model text"));
    assert!(display.is_some());
}

#[test]
fn error_results_may_carry_display_detail() {
    let rich = RichToolResult::error("timeout", "command exceeded 30s").with_display(
        ToolDisplay::new("captured output before timeout: ...").with_fact("duration_ms", "30000"),
    );
    let error = rich.result().as_error().expect("error variant");
    assert_eq!(error.code(), "timeout");
    assert!(rich.display().is_some(), "errors may carry display");
}

#[test]
fn effect_class_is_mandatory_and_queryable() {
    let read = ToolDefinition::simple("read", "reads", EffectClass::ReadOnly);
    let write = ToolDefinition::new(
        "write",
        "writes",
        r#"{"type":"object","required":["path"]}"#,
        EffectClass::Workspace,
    )
    .unwrap();
    let shell = ToolDefinition::simple("shell", "runs commands", EffectClass::System);

    assert_eq!(read.effect_class(), EffectClass::ReadOnly);
    assert_eq!(write.effect_class(), EffectClass::Workspace);
    assert_eq!(shell.effect_class(), EffectClass::System);

    // Queryable through the registry's definitions for decorators and UIs.
    let registry = ToolRegistry::builder()
        .register(read, |_args: ToolArguments| async {
            RichToolResult::success("x")
        })
        .unwrap()
        .build();
    assert_eq!(
        registry.definitions()[0].effect_class(),
        EffectClass::ReadOnly
    );
}

#[test]
fn registry_synthesized_errors_have_no_display() {
    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::new(
                "strict",
                "strict tool",
                r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#,
                EffectClass::ReadOnly,
            )
            .unwrap(),
            |_args: ToolArguments| async { RichToolResult::success("never reached") },
        )
        .unwrap()
        .build();

    let unknown = block_on(registry.invoke(
        ToolInvocation::new("c", "nope", "{}"),
        ToolInvokeCx::ignore(),
    ))
    .unwrap()
    .into_done()
    .expect("unknown tool completes");
    assert_eq!(unknown.result().as_error().unwrap().code(), "unknown_tool");
    assert!(unknown.display().is_none());

    let bad_json = block_on(registry.invoke(
        ToolInvocation::new("c", "strict", "not json"),
        ToolInvokeCx::ignore(),
    ))
    .unwrap()
    .into_done()
    .expect("bad json completes");
    assert_eq!(
        bad_json.result().as_error().unwrap().code(),
        "invalid_arguments"
    );
    assert!(bad_json.display().is_none());

    let missing = block_on(registry.invoke(
        ToolInvocation::new("c", "strict", "{}"),
        ToolInvokeCx::ignore(),
    ))
    .unwrap()
    .into_done()
    .expect("missing field completes");
    assert_eq!(
        missing.result().as_error().unwrap().code(),
        "invalid_arguments"
    );
    assert!(missing.display().is_none());
}

#[test]
fn handler_display_passes_through_untouched() {
    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::simple("probe", "probes", EffectClass::ReadOnly),
            |_args: ToolArguments| async {
                RichToolResult::success("truncated model view")
                    .with_display(ToolDisplay::new("the full detail").with_fact("k", "v"))
            },
        )
        .unwrap()
        .build();
    let rich = block_on(registry.invoke(
        ToolInvocation::new("c", "probe", "{}"),
        ToolInvokeCx::ignore(),
    ))
    .unwrap()
    .into_done()
    .expect("probe completes");
    assert_eq!(rich.result().content(), Some("truncated model view"));
    assert_eq!(rich.display().unwrap().detail(), "the full detail");
}

#[test]
fn progress_sink_does_not_change_the_final_result() {
    use std::sync::{Arc, Mutex};
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_for_handler = Arc::clone(&seen);
    struct StreamingEcho {
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl ToolHandler for StreamingEcho {
        fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a> {
            let future = self.call_with_cx(arguments, ToolInvokeCx::ignore());
            Box::pin(async move { future.await.into_done().expect("echo completes") })
        }
        fn call_with_cx<'a>(
            &'a self,
            _arguments: ToolArguments,
            cx: ToolInvokeCx,
        ) -> ToolHandlerEndFuture<'a> {
            let seen = Arc::clone(&self.seen);
            Box::pin(async move {
                cx.progress().push_text("chunk-a");
                cx.progress().push_text("chunk-b");
                seen.lock().expect("seen").push("ran".to_owned());
                ToolInvokeEnd::Done(RichToolResult::success("final"))
            })
        }
    }

    let pushed = Arc::new(Mutex::new(String::new()));
    let pushed_for_sink = Arc::clone(&pushed);
    let sink = ToolProgressSink::from_fn(move |text| {
        pushed_for_sink.lock().expect("pushed").push_str(text);
    });
    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::simple("echo", "echoes", EffectClass::ReadOnly),
            StreamingEcho {
                seen: seen_for_handler,
            },
        )
        .unwrap()
        .build();
    let rich = block_on(registry.invoke(
        ToolInvocation::new("c", "echo", "{}"),
        ToolInvokeCx::progress_only(sink),
    ))
    .unwrap()
    .into_done()
    .expect("echo completes");
    assert_eq!(rich.result().content(), Some("final"));
    assert_eq!(pushed.lock().expect("pushed").as_str(), "chunk-achunk-b");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["ran"]);
}

#[test]
fn definitions_keep_registration_order() {
    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::simple("b", "second", EffectClass::ReadOnly),
            |_args: ToolArguments| async { RichToolResult::success("b") },
        )
        .unwrap()
        .register(
            ToolDefinition::simple("a", "first", EffectClass::System),
            |_args: ToolArguments| async { RichToolResult::success("a") },
        )
        .unwrap()
        .build();
    let definitions = registry.definitions();
    let names: Vec<&str> = definitions.iter().map(|d| d.name()).collect();
    assert_eq!(names, ["b", "a"], "stable registration order");
}
