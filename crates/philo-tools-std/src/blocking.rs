//! Bounded blocking pool for the five filesystem tools.
//!
//! `read` / `list` / `grep` / `write` / `edit` perform synchronous `std::fs`
//! work. [`BlockingPool::wrap_handler`] and [`BlockingToolExecutor`] move that
//! work onto `tokio::task::spawn_blocking` under a semaphore so Runtime workers
//! are never parked in an OS syscall.
//!
//! Admission uses `try_acquire_owned` only: a full pool returns
//! [`ToolPortError`] immediately (infrastructure busy). A panicked blocking
//! worker is also a [`ToolPortError`]. Tool business failures stay
//! [`philo_tools::ToolResult::Error`].
//!
//! `shell` is native async (process I/O + cancel/kill) and must not be wrapped.
//! [`BlockingToolExecutor`] routes `shell` around the pool; [`BlockingPool::wrap_handler`]
//! is for the five filesystem handlers only.
//!
//! Cooperative cancel cannot abort an in-flight OS syscall. The pool `select`s
//! the cancel token against the join: after cancel it returns `Stopped` and
//! discards a late result. `list` / `grep` additionally poll the token inside
//! their walks so a large tree stops without finishing the scan.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use philo_tools::{
    ToolArguments, ToolCancel, ToolDefinition, ToolHandler, ToolHandlerEndFuture,
    ToolHandlerFuture, ToolInvocation, ToolInvokeCx, ToolInvokeEnd, ToolPort, ToolPortError,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::SHELL_TOOL_NAME;

/// Default running-permit count, matching `RuntimeConfig.max_parallel_tool_calls`.
pub const DEFAULT_BLOCKING_TOOL_CONCURRENCY: usize = 8;

/// Default extra admission slots waiting off the Runtime worker (`BLOCKING_TOOL_QUEUE`).
pub const DEFAULT_BLOCKING_TOOL_QUEUE: usize = 32;

/// Shared bounded pool: `concurrency` running filesystem workers plus up to
/// `queue_bound` waiters that wait on a blocking thread, never on a Runtime worker.
#[derive(Clone)]
pub struct BlockingPool {
    running: Arc<Semaphore>,
    queue: Arc<Semaphore>,
    concurrency: usize,
    queue_bound: usize,
}

impl BlockingPool {
    /// Creates a pool with `concurrency` running permits (minimum 1) and
    /// `queue_bound` extra admission slots.
    ///
    /// A call that cannot take a running permit tries a queue slot via
    /// `try_acquire_owned`. Queue waiters then wait for a running permit
    /// *inside* `spawn_blocking`. If both are exhausted the call returns
    /// [`ToolPortError`] without awaiting a permit on the Runtime worker.
    pub fn new(concurrency: usize, queue_bound: usize) -> Self {
        let concurrency = concurrency.max(1);
        Self {
            running: Arc::new(Semaphore::new(concurrency)),
            queue: Arc::new(Semaphore::new(queue_bound)),
            concurrency,
            queue_bound,
        }
    }

    /// Running-permit count passed to [`BlockingPool::new`].
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// Extra admission slots passed to [`BlockingPool::new`].
    pub fn queue_bound(&self) -> usize {
        self.queue_bound
    }

    /// Wraps a filesystem [`ToolHandler`] so each invoke runs on this pool.
    ///
    /// Do not wrap [`crate::ShellTool`]. Prefer this for the five FS tools when
    /// assembling a registry; [`BlockingToolExecutor`] is the [`ToolPort`]
    /// decorator that also bypasses `shell` by name and surfaces pool busy /
    /// join panic as [`ToolPortError`].
    pub fn wrap_handler<H: ToolHandler + 'static>(&self, handler: H) -> BlockingFsHandler {
        BlockingFsHandler {
            pool: self.clone(),
            inner: Arc::new(handler),
        }
    }

    fn try_admit(&self) -> Result<Admission, ToolPortError> {
        match self.running.clone().try_acquire_owned() {
            Ok(permit) => Ok(Admission::Running(permit)),
            Err(_) => match self.queue.clone().try_acquire_owned() {
                Ok(permit) => Ok(Admission::Queued(permit)),
                Err(_) => Err(pool_busy()),
            },
        }
    }

    async fn run_blocking<F, T>(
        &self,
        cancel: ToolCancel,
        work: F,
    ) -> Result<RunOutcome<T>, ToolPortError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        if cancel.is_requested() {
            return Ok(RunOutcome::Cancelled);
        }
        let admission = self.try_admit()?;
        let runtime = match &admission {
            Admission::Queued(_) => Some(tokio::runtime::Handle::current()),
            Admission::Running(_) => None,
        };
        let running = Arc::clone(&self.running);
        let join = tokio::task::spawn_blocking(move || {
            let _running_permit = match admission {
                Admission::Running(permit) => permit,
                Admission::Queued(queue_permit) => {
                    let permit = runtime
                        .expect("queued admission always captures a runtime handle")
                        .block_on(running.acquire_owned())
                        .expect("blocking tool pool semaphore closed");
                    drop(queue_permit);
                    permit
                }
            };
            work()
        });
        tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(RunOutcome::Cancelled),
            join = join => match join {
                Ok(value) => Ok(RunOutcome::Completed(value)),
                Err(error) if error.is_panic() => Err(ToolPortError::new(
                    "filesystem tool worker panicked",
                )),
                Err(error) => Err(ToolPortError::new(format!(
                    "filesystem tool worker failed to join: {error}"
                ))),
            },
        }
    }
}

impl Default for BlockingPool {
    fn default() -> Self {
        Self::new(
            DEFAULT_BLOCKING_TOOL_CONCURRENCY,
            DEFAULT_BLOCKING_TOOL_QUEUE,
        )
    }
}

impl std::fmt::Debug for BlockingPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockingPool")
            .field("concurrency", &self.concurrency)
            .field("queue_bound", &self.queue_bound)
            .finish()
    }
}

enum Admission {
    Running(OwnedSemaphorePermit),
    Queued(OwnedSemaphorePermit),
}

enum RunOutcome<T> {
    Completed(T),
    Cancelled,
}

fn pool_busy() -> ToolPortError {
    ToolPortError::new("blocking tool pool busy")
}

fn poll_once_ready<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!(
            "filesystem tool parked inside spawn_blocking; blocking handlers must complete without awaiting"
        ),
    }
}

/// Filesystem [`ToolHandler`] bound to a [`BlockingPool`].
///
/// [`BlockingFsHandler::invoke`] is the faithful API: pool busy and join panic
/// are [`ToolPortError`]. The [`ToolHandler`] impl panics on those paths so a
/// `ToolRegistry` cannot rewrite them into `ToolResult::Error`. Assemble a
/// six-tool registry with [`BlockingToolExecutor`] when the port must return
/// [`ToolPortError`].
#[derive(Clone)]
pub struct BlockingFsHandler {
    pool: BlockingPool,
    inner: Arc<dyn ToolHandler>,
}

impl BlockingFsHandler {
    /// Runs the inner handler on the pool.
    pub async fn invoke(
        &self,
        arguments: ToolArguments,
        cx: ToolInvokeCx,
    ) -> Result<ToolInvokeEnd, ToolPortError> {
        let inner = Arc::clone(&self.inner);
        match self
            .pool
            .run_blocking(cx.cancel().clone(), move || {
                poll_once_ready(inner.call_with_cx(arguments, cx))
            })
            .await?
        {
            RunOutcome::Cancelled => Ok(ToolInvokeEnd::Stopped),
            RunOutcome::Completed(end) => Ok(end),
        }
    }
}

impl ToolHandler for BlockingFsHandler {
    fn call<'a>(&'a self, arguments: ToolArguments) -> ToolHandlerFuture<'a> {
        let this = self.clone();
        Box::pin(async move {
            match this.invoke(arguments, ToolInvokeCx::ignore()).await {
                Ok(end) => end
                    .into_done()
                    .expect("blocking handler call() cannot stop without cancel"),
                Err(error) => panic!("{}", error.message()),
            }
        })
    }

    fn call_with_cx<'a>(
        &'a self,
        arguments: ToolArguments,
        cx: ToolInvokeCx,
    ) -> ToolHandlerEndFuture<'a> {
        let this = self.clone();
        Box::pin(async move {
            this.invoke(arguments, cx)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message()))
        })
    }
}

/// Wraps a filesystem handler so it runs on `pool` (`spawn_blocking` + cap).
///
/// Equivalent to [`BlockingPool::wrap_handler`]. Do not wrap `shell`.
pub fn blocking_fs_handler<H: ToolHandler + 'static>(
    pool: &BlockingPool,
    handler: H,
) -> BlockingFsHandler {
    pool.wrap_handler(handler)
}

/// [`ToolPort`] decorator: filesystem tools run on a [`BlockingPool`]; `shell`
/// is forwarded to the inner port on the native async path.
#[derive(Clone)]
pub struct BlockingToolExecutor {
    inner: Arc<dyn ToolPort>,
    pool: BlockingPool,
}

impl BlockingToolExecutor {
    /// Wraps `inner` with an explicit pool.
    pub fn new(inner: impl ToolPort + 'static, pool: BlockingPool) -> Self {
        Self {
            inner: Arc::new(inner),
            pool,
        }
    }

    /// Wraps `inner` with [`BlockingPool::default`] (concurrency 8, queue 32).
    pub fn wrap(inner: impl ToolPort + 'static) -> Self {
        Self::new(inner, BlockingPool::default())
    }

    /// Wraps `inner` with `max_parallel_tool_calls` running permits and the
    /// default queue bound. Intended as the later CodingProfile/CLI one-liner.
    pub fn with_parallelism(
        inner: impl ToolPort + 'static,
        max_parallel_tool_calls: usize,
    ) -> Self {
        Self::new(
            inner,
            BlockingPool::new(max_parallel_tool_calls, DEFAULT_BLOCKING_TOOL_QUEUE),
        )
    }

    /// The pool this executor admits through.
    pub fn pool(&self) -> &BlockingPool {
        &self.pool
    }
}

impl ToolPort for BlockingToolExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }

    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        cx: ToolInvokeCx,
    ) -> philo_tools::ToolFuture<'a> {
        if invocation.name() == SHELL_TOOL_NAME {
            return self.inner.invoke(invocation, cx);
        }
        let inner = Arc::clone(&self.inner);
        let pool = self.pool.clone();
        Box::pin(async move {
            match pool
                .run_blocking(cx.cancel().clone(), move || {
                    poll_once_ready(inner.invoke(invocation, cx))
                })
                .await?
            {
                RunOutcome::Cancelled => Ok(ToolInvokeEnd::Stopped),
                RunOutcome::Completed(result) => result,
            }
        })
    }
}

impl std::fmt::Debug for BlockingToolExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockingToolExecutor")
            .field("pool", &self.pool)
            .field("definitions", &self.inner.definitions())
            .finish()
    }
}
