//! TOOL-001: Registry stable order and error normalization.

use philo_tools::*;
use std::sync::Arc;

#[test]
fn registry_is_stable_and_normalizes_errors() {
    let registry = ToolRegistry::builder()
        .register(
            ToolDefinition::simple("echo", "echoes", EffectClass::ReadOnly),
            |_args: ToolArguments| async { RichToolResult::success("ok") },
        )
        .unwrap()
        .build();
    assert_eq!(registry.definitions()[0].name(), "echo");
    let success = futures_block_on(registry.invoke(
        ToolInvocation::new("c", "echo", "{}"),
        ToolInvokeCx::ignore(),
    ))
    .unwrap()
    .into_done()
    .expect("closure handler completes");
    assert_eq!(success.result().content(), Some("ok"));
    let invalid = futures_block_on(registry.invoke(
        ToolInvocation::new("c", "echo", "[]"),
        ToolInvokeCx::ignore(),
    ))
    .unwrap()
    .into_done()
    .expect("invalid arguments still complete");
    assert_eq!(
        invalid.result().as_error().unwrap().code(),
        "invalid_arguments"
    );
    let unknown = futures_block_on(registry.invoke(
        ToolInvocation::new("c", "missing", "{}"),
        ToolInvokeCx::ignore(),
    ))
    .unwrap()
    .into_done()
    .expect("unknown tool still completes");
    assert_eq!(unknown.result().as_error().unwrap().code(), "unknown_tool");
    let object: Arc<dyn ToolPort> = Arc::new(registry);
    let _ = object.definitions();
}

fn futures_block_on<F: std::future::Future>(future: F) -> F::Output {
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
