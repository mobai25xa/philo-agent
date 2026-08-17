//! Shared dependency assembly for single-shot and interactive commands.
//!
//! Both modes consume the same prepared graph; only their presentation and
//! lifetime differ. Keeping this path singular prevents configuration drift.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use tokio::sync::oneshot;

use philo_agent_runtime::{
    AgentRuntime, GenerationDisplay, GenerationId, IdSource, ModelPort, ReasoningEffort,
    RuntimeConfig, RuntimeDeps, RuntimeGeneration, ToolPort,
};
use philo_agent_service::{
    AssembleError, AssembleRequest, AssembledGeneration, FrontendClient, FrontendCommand,
    FrontendReasoningEffort, GenerationAssembler, ServiceDeps,
};
use philo_coding_profile::CodingProfile;
use philo_model::{AdapterBuildError, FileModelReplayStore, ModelReplayStore, PhiloModelAdapter};
use philo_session::SessionStore;
use philo_session_jsonl::JsonlSessionStore;
use philo_tools_std::BlockingToolExecutor;

use crate::args::Cli;
use crate::config::{ResolveFlags, Settings};
use crate::error::UsageError;
use crate::ids::ProcessIdSource;

/// Running AgentService plus the stores the composition root still owns.
pub(crate) struct Bootstrap {
    pub service: philo_agent_service::AgentService,
    pub client: FrontendClient,
    pub sessions: Arc<JsonlSessionStore>,
    pub assembler: Arc<CliGenerationAssembler>,
    pub settings: Settings,
}

struct AssemblerState {
    flags: ResolveFlags,
    settings: Mutex<Settings>,
    replay_store: Arc<dyn ModelReplayStore>,
    data_dir: PathBuf,
    reload_fault: Mutex<Option<String>>,
    #[cfg(test)]
    last_assemble_thread: Mutex<Option<std::thread::ThreadId>>,
}

impl AssemblerState {
    fn current_settings(&self) -> Settings {
        self.settings
            .lock()
            .expect("generation settings lock")
            .clone()
    }

    fn take_reload_fault(&self) -> Option<String> {
        self.reload_fault.lock().expect("reload fault lock").take()
    }

    fn assemble_sync(
        &self,
        request: AssembleRequest,
    ) -> Result<AssembledGeneration, AssembleError> {
        #[cfg(test)]
        {
            *self
                .last_assemble_thread
                .lock()
                .expect("assemble thread lock") = Some(std::thread::current().id());
        }
        if let Some(message) = self.take_reload_fault() {
            return Err(AssembleError::new(message));
        }
        let settings = self.current_settings();
        if settings.data_dir != self.data_dir {
            return Err(AssembleError::new(
                "data_dir cannot be changed without restarting",
            ));
        }
        let runtime_config = runtime_config_for(&self.flags.to_cli(), &settings, &request.name)
            .map_err(|error| AssembleError::new(error.0))?;
        let model: Arc<dyn ModelPort> = Arc::new(
            build_model(
                &settings.deployment,
                &request.name,
                self.replay_store.clone(),
            )
            .map_err(|error| AssembleError::new(format!("{error}")))?,
        );
        let tools = tool_port_for(&settings, runtime_config.max_parallel_tool_calls)
            .map_err(|error| AssembleError::new(error.0))?;
        Ok(AssembledGeneration {
            model,
            tools,
            runtime_config,
            model_name: request.name,
        })
    }
}

struct BuildJob<T> {
    work: Box<dyn FnOnce() -> Result<T, AssembleError> + Send>,
    reply: oneshot::Sender<Result<T, AssembleError>>,
}

struct BuildMailbox<T> {
    pending: Mutex<Option<BuildJob<T>>>,
    closed: AtomicBool,
    unavailable: AtomicBool,
}

/// Single-worker generation builder. At most one unstarted candidate is kept;
/// a newer request supersedes it. `assemble_sync` runs on this OS thread.
struct GenerationBuildPool<T: Send + 'static> {
    mailbox: Arc<BuildMailbox<T>>,
    wake: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl<T: Send + 'static> GenerationBuildPool<T> {
    fn new() -> Self {
        let mailbox = Arc::new(BuildMailbox {
            pending: Mutex::new(None),
            closed: AtomicBool::new(false),
            unavailable: AtomicBool::new(false),
        });
        let (wake_tx, wake_rx) = mpsc::channel();
        let actor_mailbox = Arc::clone(&mailbox);
        let thread = thread::Builder::new()
            .name("philo-generation-build".to_owned())
            .spawn(move || run_build_actor(actor_mailbox, wake_rx))
            .expect("generation build thread");
        Self {
            mailbox,
            wake: Mutex::new(Some(wake_tx)),
            thread: Mutex::new(Some(thread)),
        }
    }

    fn submit(
        &self,
        work: impl FnOnce() -> Result<T, AssembleError> + Send + 'static,
    ) -> oneshot::Receiver<Result<T, AssembleError>> {
        let (reply, rx) = oneshot::channel();
        if self.mailbox.closed.load(Ordering::Acquire)
            || self.mailbox.unavailable.load(Ordering::Acquire)
        {
            let _ = reply.send(Err(AssembleError::new(
                "generation build pool is unavailable",
            )));
            return rx;
        }
        let job = BuildJob {
            work: Box::new(work),
            reply,
        };
        if let Some(previous) = self
            .mailbox
            .pending
            .lock()
            .expect("generation build mailbox")
            .replace(job)
        {
            let _ = previous
                .reply
                .send(Err(AssembleError::new("generation assembly superseded")));
        }
        if let Some(wake) = self.wake.lock().expect("generation wake").as_ref() {
            let _ = wake.send(());
        }
        rx
    }

    fn shutdown(&self) {
        self.mailbox.closed.store(true, Ordering::Release);
        if let Some(previous) = self
            .mailbox
            .pending
            .lock()
            .expect("generation build mailbox")
            .take()
        {
            let _ = previous.reply.send(Err(AssembleError::new(
                "generation build pool is shutting down",
            )));
        }
        if let Some(wake) = self.wake.lock().expect("generation wake").take() {
            let _ = wake.send(());
        }
        if let Some(thread) = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = thread.join();
        }
    }
}

fn run_build_actor<T: Send + 'static>(
    mailbox: Arc<BuildMailbox<T>>,
    wake_rx: std::sync::mpsc::Receiver<()>,
) {
    loop {
        let job = mailbox
            .pending
            .lock()
            .expect("generation build mailbox")
            .take();
        if let Some(job) = job {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job.work));
            match result {
                Ok(result) => {
                    let _ = job.reply.send(result);
                }
                Err(_) => {
                    mailbox.unavailable.store(true, Ordering::Release);
                    let _ = job
                        .reply
                        .send(Err(AssembleError::new("generation build worker panicked")));
                    return;
                }
            }
            continue;
        }
        if mailbox.closed.load(Ordering::Acquire) {
            return;
        }
        match wake_rx.recv() {
            Ok(()) => {}
            Err(_) => return,
        }
    }
}

/// CLI-owned assembler: rebuilds model, tools, and RuntimeConfig from the
/// latest settings. SessionStore, IdSource, and the replay store are reused.
pub(crate) struct CliGenerationAssembler {
    state: Arc<AssemblerState>,
    pool: GenerationBuildPool<AssembledGeneration>,
}

impl CliGenerationAssembler {
    pub(crate) fn new(
        flags: ResolveFlags,
        settings: Settings,
        replay_store: Arc<dyn ModelReplayStore>,
    ) -> Self {
        Self {
            state: Arc::new(AssemblerState {
                flags,
                data_dir: settings.data_dir.clone(),
                settings: Mutex::new(settings),
                replay_store,
                reload_fault: Mutex::new(None),
                #[cfg(test)]
                last_assemble_thread: Mutex::new(None),
            }),
            pool: GenerationBuildPool::new(),
        }
    }

    pub(crate) fn data_dir(&self) -> &PathBuf {
        &self.state.data_dir
    }

    pub(crate) fn current_settings(&self) -> Settings {
        self.state.current_settings()
    }

    pub(crate) fn store_settings(&self, settings: Settings) {
        *self
            .state
            .settings
            .lock()
            .expect("generation settings lock") = settings;
    }

    pub(crate) fn set_reload_fault(&self, message: impl Into<String>) {
        *self.state.reload_fault.lock().expect("reload fault lock") = Some(message.into());
    }

    fn assemble_sync(
        &self,
        request: AssembleRequest,
    ) -> Result<AssembledGeneration, AssembleError> {
        self.state.assemble_sync(request)
    }

    pub(crate) fn shutdown(&self) {
        self.pool.shutdown();
    }
}

impl Drop for CliGenerationAssembler {
    fn drop(&mut self) {
        self.pool.shutdown();
    }
}

impl GenerationAssembler for CliGenerationAssembler {
    fn assemble(
        &self,
        request: AssembleRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AssembledGeneration, AssembleError>> + Send + '_>> {
        let state = Arc::clone(&self.state);
        let rx = self.pool.submit(move || state.assemble_sync(request));
        Box::pin(async move {
            rx.await
                .unwrap_or_else(|_| Err(AssembleError::new("generation build worker stopped")))
        })
    }
}

/// Builds stores, the initial generation, Runtime, and AgentService.
///
/// Store open and the first `assemble_sync` run on the composition-root
/// thread before any AgentService actor turn. Later installs go through
/// [`GenerationBuildPool`].
pub(crate) fn bootstrap(cli: &Cli, settings: Settings) -> Result<Bootstrap, UsageError> {
    let display_settings = settings.clone();
    let flags = ResolveFlags::from_cli(cli);
    let sessions = Arc::new(
        JsonlSessionStore::open(&settings.data_dir)
            .map_err(|error| UsageError::new(format!("cannot open the session store: {error}")))?,
    );
    let replay_store: Arc<dyn ModelReplayStore> = Arc::new(
        FileModelReplayStore::open(&settings.data_dir).map_err(|error| {
            UsageError::new(format!("cannot open the model replay sidecar: {error}"))
        })?,
    );
    let ids: Arc<dyn IdSource> = Arc::new(ProcessIdSource::new());
    let assembler = Arc::new(CliGenerationAssembler::new(flags, settings, replay_store));
    let assembled = assembler
        .assemble_sync(AssembleRequest {
            name: display_settings.deployment.model.clone(),
        })
        .map_err(|error| UsageError::new(format!("model assembly failed: {}", error.message)))?;
    let initial_generation = Arc::new(RuntimeGeneration {
        generation_id: GenerationId::new("generation-0"),
        model: assembled.model,
        tools: assembled.tools,
        runtime_config: assembled.runtime_config,
        display: GenerationDisplay {
            model_name: assembled.model_name,
        },
    });
    let session_store: Arc<dyn SessionStore> = sessions.clone();
    let generation_assembler: Arc<dyn GenerationAssembler> = assembler.clone();
    let (handle, subscription) = AgentRuntime::start(RuntimeDeps {
        sessions: session_store.clone(),
        ids,
        bounds: Default::default(),
    })
    .map_err(|error| UsageError::new(format!("cannot start the runtime: {}", error.message())))?;
    let (service, client) = philo_agent_service::start(ServiceDeps {
        runtime: handle,
        subscription,
        sessions: session_store,
        assembler: generation_assembler,
        initial_generation,
    });
    Ok(Bootstrap {
        service,
        client,
        sessions,
        assembler,
        settings: display_settings,
    })
}

/// Graceful service + store shutdown. Does not cancel in-flight work beyond Drain.
pub(crate) async fn shutdown(bootstrap: Bootstrap) {
    let Bootstrap {
        service,
        client,
        sessions,
        assembler,
        ..
    } = bootstrap;
    match client.try_command(FrontendCommand::ShutdownRequested) {
        philo_agent_service::CommandSubmitResult::Accepted(_) => {}
        _ => {
            let _ = service.request_shutdown();
        }
    }
    service.join().await;
    assembler.shutdown();
    let _ = sessions.shutdown();
}

/// Applies a watched TOML reload: update assembler state, then InstallModel
/// or SetReasoning. Parse failures and `data_dir` changes keep the old
/// generation and surface through `GenerationInstallFailed`.
pub(crate) fn apply_config_reload(
    client: &FrontendClient,
    assembler: &CliGenerationAssembler,
    result: Result<(Settings, Vec<String>), UsageError>,
) {
    match result {
        Err(error) => {
            assembler.set_reload_fault(format!("config not reloaded: {}", error.0));
            let name = assembler.current_settings().deployment.model;
            let _ = client.try_command(FrontendCommand::InstallModel { name });
        }
        Ok((settings, _warnings)) => {
            if settings.data_dir != *assembler.data_dir() {
                assembler.set_reload_fault(
                    "config not reloaded: data_dir cannot be changed without restarting",
                );
                let name = assembler.current_settings().deployment.model;
                let _ = client.try_command(FrontendCommand::InstallModel { name });
                return;
            }
            let previous = assembler.current_settings();
            assembler.store_settings(settings.clone());
            if !generation_fields_changed(&previous, &settings) {
                return;
            }
            if only_reasoning_changed(&previous, &settings)
                && let Some(effort) = settings.reasoning_effort
            {
                let _ = client.try_command(FrontendCommand::SetReasoning {
                    effort: frontend_effort(effort),
                });
                return;
            }
            let _ = client.try_command(FrontendCommand::InstallModel {
                name: settings.deployment.model,
            });
        }
    }
}

fn frontend_effort(effort: ReasoningEffort) -> FrontendReasoningEffort {
    match effort {
        ReasoningEffort::Minimal => FrontendReasoningEffort::Minimal,
        ReasoningEffort::Low => FrontendReasoningEffort::Low,
        ReasoningEffort::Medium => FrontendReasoningEffort::Medium,
        ReasoningEffort::High => FrontendReasoningEffort::High,
        ReasoningEffort::VeryHigh => FrontendReasoningEffort::VeryHigh,
        ReasoningEffort::Maximum => FrontendReasoningEffort::Maximum,
    }
}

fn ui_entry(key: &str) -> bool {
    matches!(key, "verbosity" | "show_reasoning" | "screen")
}

fn header_names(settings: &Settings) -> Vec<String> {
    settings
        .deployment
        .request_headers
        .names()
        .map(str::to_owned)
        .collect()
}

fn non_ui_entries(settings: &Settings) -> Vec<(String, String, String)> {
    settings
        .entries
        .iter()
        .filter(|entry| !ui_entry(&entry.key))
        .map(|entry| (entry.key.clone(), entry.value.clone(), entry.source.clone()))
        .collect()
}

fn generation_fields_changed(left: &Settings, right: &Settings) -> bool {
    left.deployment.model != right.deployment.model
        || left.deployment.endpoint != right.deployment.endpoint
        || left.deployment.protocol != right.deployment.protocol
        || left.deployment.provider != right.deployment.provider
        || left.deployment.api_key_env != right.deployment.api_key_env
        || left.deployment.compat != right.deployment.compat
        || left.deployment.chat_reasoning_format != right.deployment.chat_reasoning_format
        || left.deployment.continuation_policy != right.deployment.continuation_policy
        || header_names(left) != header_names(right)
        || left.compaction != right.compaction
        || left.reasoning_effort != right.reasoning_effort
        || left.max_tool_rounds != right.max_tool_rounds
        || left.max_parallel_tool_calls != right.max_parallel_tool_calls
        || left.operation_timeout != right.operation_timeout
        || left.shell_timeout_secs != right.shell_timeout_secs
        || non_ui_entries(left) != non_ui_entries(right)
}

fn only_reasoning_changed(left: &Settings, right: &Settings) -> bool {
    if left.reasoning_effort == right.reasoning_effort {
        return false;
    }
    let mut right_without = right.clone();
    right_without.reasoning_effort = left.reasoning_effort;
    !generation_fields_changed(left, &right_without)
}

/// One model construction path shared by startup and interactive generation
/// install, so deployment headers and credentials cannot drift.
pub(crate) fn build_model(
    deployment: &crate::config::Deployment,
    model: &str,
    replay_store: Arc<dyn ModelReplayStore>,
) -> Result<PhiloModelAdapter, AdapterBuildError> {
    let mut builder = PhiloModelAdapter::builder(
        deployment.provider.clone(),
        deployment.protocol,
        model,
        deployment.endpoint.clone(),
    )
    .api_key_env(&deployment.api_key_env)
    .request_headers(deployment.request_headers.clone())
    .replay_store(replay_store)
    .compat(deployment.compat)
    .continuation_policy(deployment.continuation_policy);
    if let Some(format) = deployment.chat_reasoning_format {
        builder = builder.chat_reasoning_format(format);
    }
    builder.build()
}

/// Maps resolved settings onto a RuntimeConfig the same way bootstrap does.
pub(crate) fn runtime_config_for(
    cli: &Cli,
    settings: &Settings,
    model_target: &str,
) -> Result<RuntimeConfig, UsageError> {
    let profile = coding_profile(settings)?;
    let mut runtime_config = profile.runtime_config(model_target);
    if let Some(system) = &cli.system {
        runtime_config.system_prompt = system.clone();
    }
    if let Some(rounds) = settings.max_tool_rounds {
        runtime_config.max_tool_rounds = rounds;
    }
    if let Some(parallel) = settings.max_parallel_tool_calls {
        runtime_config.max_parallel_tool_calls = parallel;
    }
    if settings.reasoning_effort.is_some() {
        runtime_config.generation.reasoning_effort = settings.reasoning_effort;
    }
    runtime_config.operation_timeout = settings.operation_timeout;
    runtime_config.compaction = settings.compaction.clone();
    Ok(runtime_config)
}

fn coding_profile(settings: &Settings) -> Result<CodingProfile, UsageError> {
    let workspace_root = std::env::current_dir().map_err(|error| {
        UsageError::new(format!("cannot resolve the working directory: {error}"))
    })?;
    let mut profile = CodingProfile::new(workspace_root);
    if let Some(seconds) = settings.shell_timeout_secs {
        profile = profile.with_shell_timeout_secs(seconds);
    }
    Ok(profile)
}

fn blocking_tool_executor(
    settings: &Settings,
    max_parallel_tool_calls: u32,
) -> Result<BlockingToolExecutor, UsageError> {
    let profile = coding_profile(settings)?;
    Ok(BlockingToolExecutor::with_parallelism(
        profile.tool_registry(),
        max_parallel_tool_calls.max(1) as usize,
    ))
}

/// Rebuilds the coding ToolPort from resolved settings and wraps the five
/// filesystem tools with [`BlockingToolExecutor`]. `shell` stays async.
pub(crate) fn tool_port_for(
    settings: &Settings,
    max_parallel_tool_calls: u32,
) -> Result<Arc<dyn ToolPort>, UsageError> {
    Ok(Arc::new(blocking_tool_executor(
        settings,
        max_parallel_tool_calls,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    use philo_agent_runtime::CompactionConfig;
    use philo_model::{
        MemoryModelReplayStore, ModelCompat, ModelContinuationPolicy, ModelProtocol,
        ModelRequestHeaders,
    };
    use philo_tui::TuiScreen;

    use crate::config::{Deployment, Verbosity};

    fn temp_dir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("philo-cli-assembly-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create dir");
        path
    }

    fn settings(dir: &Path, model: &str, endpoint: &str) -> Settings {
        Settings {
            deployment: Deployment {
                provider: "test".to_owned(),
                protocol: ModelProtocol::OpenAiChat,
                model: model.to_owned(),
                endpoint: endpoint.to_owned(),
                api_key_env: "PHILO_API_KEY".to_owned(),
                request_headers: ModelRequestHeaders::default(),
                compat: ModelCompat::Compatible,
                chat_reasoning_format: None,
                continuation_policy: ModelContinuationPolicy::StatelessLocalReplay,
            },
            data_dir: dir.to_path_buf(),
            context_window: None,
            compaction: CompactionConfig::default(),
            reasoning_effort: None,
            max_tool_rounds: Some(1),
            max_parallel_tool_calls: Some(2),
            operation_timeout: Some(Duration::from_secs(30)),
            shell_timeout_secs: None,
            verbosity: Verbosity::Default,
            show_reasoning: true,
            screen: TuiScreen::Alternate,
            entries: vec![],
        }
    }

    fn assembler(dir: &Path, endpoint: &str) -> CliGenerationAssembler {
        CliGenerationAssembler::new(
            ResolveFlags {
                model: None,
                data_dir: None,
                system: None,
                max_tool_rounds: None,
                reasoning_effort: None,
                verbose: false,
                quiet: false,
            },
            settings(dir, "model-a", endpoint),
            Arc::new(MemoryModelReplayStore::default()),
        )
    }

    #[test]
    fn data_dir_change_is_rejected_without_touching_the_old_settings() {
        let dir = temp_dir("data-dir");
        let assembler = assembler(&dir, "https://example.test/v1/chat/completions");
        let mut next = assembler.current_settings();
        next.data_dir = dir.join("other");
        assembler.store_settings(next);
        let Err(error) = assembler.assemble_sync(AssembleRequest {
            name: "model-a".to_owned(),
        }) else {
            panic!("data_dir must fail");
        };
        assert!(error.message.contains("data_dir"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn illegal_model_assembly_returns_an_error() {
        let dir = temp_dir("bad-model");
        let assembler = assembler(&dir, "not-a-url");
        let Err(error) = assembler.assemble_sync(AssembleRequest {
            name: "model-a".to_owned(),
        }) else {
            panic!("bad endpoint");
        };
        assert!(!error.message.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ui_only_changes_do_not_count_as_generation_changes() {
        let dir = temp_dir("ui");
        let left = settings(&dir, "model-a", "https://example.test/v1/chat/completions");
        let mut right = left.clone();
        right.show_reasoning = false;
        right.verbosity = Verbosity::Verbose;
        right.screen = TuiScreen::Inline;
        assert!(!generation_fields_changed(&left, &right));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reasoning_only_is_detected_separately_from_other_generation_fields() {
        let dir = temp_dir("reasoning");
        let left = settings(&dir, "model-a", "https://example.test/v1/chat/completions");
        let mut right = left.clone();
        right.reasoning_effort = Some(ReasoningEffort::High);
        assert!(only_reasoning_changed(&left, &right));
        right.max_tool_rounds = Some(9);
        assert!(!only_reasoning_changed(&left, &right));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reload_fault_is_consumed_once() {
        let dir = temp_dir("fault");
        let assembler = assembler(&dir, "https://example.test/v1/chat/completions");
        assembler.set_reload_fault("config not reloaded: invalid TOML");
        let Err(error) = assembler.assemble_sync(AssembleRequest {
            name: "model-a".to_owned(),
        }) else {
            panic!("fault");
        };
        assert!(error.message.contains("invalid TOML"));
        // A subsequent assemble is not stuck on the stale fault; it may still
        // fail on model construction in tests without a real endpoint.
        let _ = assembler.assemble_sync(AssembleRequest {
            name: "model-a".to_owned(),
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn production_tool_port_wraps_filesystem_tools_and_leaves_shell_named() {
        let dir = temp_dir("tools");
        let settings = settings(&dir, "model-a", "https://example.test/v1/chat/completions");
        let executor = blocking_tool_executor(&settings, 2).expect("executor");
        let names: Vec<String> = executor
            .definitions()
            .into_iter()
            .map(|definition| definition.name().to_owned())
            .collect();
        for name in ["read", "list", "grep", "write", "edit", "shell"] {
            assert!(
                names.iter().any(|found| found == name),
                "missing {name} in {names:?}"
            );
        }
        assert_eq!(executor.pool().concurrency(), 2);
        let debug = format!("{executor:?}");
        assert!(debug.contains("BlockingToolExecutor"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unstarted_generation_candidate_is_superseded() {
        let pool = GenerationBuildPool::new();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (hold_tx, hold_rx) = std::sync::mpsc::channel();
        let first = pool.submit(move || {
            started_tx.send(()).expect("started");
            hold_rx.recv().expect("hold");
            Ok(1)
        });
        started_rx.recv().expect("first job is running");
        let second = pool.submit(|| Ok(2));
        let third = pool.submit(|| Ok(3));
        let superseded = second.blocking_recv().expect("second reply");
        assert!(
            superseded
                .expect_err("second was still queued")
                .message
                .contains("superseded")
        );
        hold_tx.send(()).expect("release first");
        assert_eq!(first.blocking_recv().expect("first reply").expect("ok"), 1);
        assert_eq!(third.blocking_recv().expect("third reply").expect("ok"), 3);
        pool.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn trait_assemble_runs_on_the_build_pool_thread() {
        use philo_agent_service::GenerationAssembler;

        let dir = temp_dir("pool-thread");
        let assembler = assembler(&dir, "not-a-url");
        let caller = std::thread::current().id();
        let _ = GenerationAssembler::assemble(
            &assembler,
            AssembleRequest {
                name: "model-a".to_owned(),
            },
        )
        .await;
        let assemble_thread = assembler
            .state
            .last_assemble_thread
            .lock()
            .expect("assemble thread lock")
            .expect("assemble_sync ran");
        assert_ne!(
            assemble_thread, caller,
            "assemble_sync must not run inside the async wrapper"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
