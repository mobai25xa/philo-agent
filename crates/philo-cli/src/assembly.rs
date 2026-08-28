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
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};

use philo_agent_runtime::{
    AgentRuntime, GenerationDisplay, GenerationId, IdSource, ModelPort, ReasoningEffort,
    RuntimeConfig, RuntimeDeps, RuntimeGeneration, ToolPort,
};
use philo_agent_service::{
    AssembleError, AssembleRequest, AssembledGeneration, CommandDispatch, FrontendClient,
    FrontendCommand, FrontendReasoningEffort, GenerationAssembler, ModelListingEntry, ServiceDeps,
};
use philo_coding_profile::CodingProfile;
use philo_model::{
    AdapterBuildError, FileModelReplayStore, ModelReplayStore, ModelRequestHeaders,
    PhiloModelAdapter, RetryPolicy, TimeoutPolicy,
};
use philo_session::SessionStore;
use philo_session_jsonl::JsonlSessionStore;
use philo_tools_std::BlockingToolExecutor;

use crate::args::Cli;
use crate::config::{Deployment, ResolveFlags, Settings, WatchTask};
use crate::error::UsageError;
use crate::ids::ProcessIdSource;

/// Default process-level grace used by interactive and oneshot drain.
pub(crate) const PROCESS_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// Attach/detach handshake deadline owned by the interactive supervisor.
pub(crate) const FRONTEND_REGISTRATION_GRACE: Duration = Duration::from_secs(5);

/// Components that missed the process shutdown deadline.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProcessShutdownReport {
    pub pending: Vec<String>,
    pub diagnostics: Vec<String>,
}

/// Running AgentService plus the stores the composition root still owns.
pub(crate) struct Bootstrap {
    pub service: philo_agent_service::AgentService,
    pub client: FrontendClient,
    pub sessions: Arc<JsonlSessionStore>,
    pub assembler: Arc<CliGenerationAssembler>,
    pub settings: Settings,
    /// Process working directory resolved once at assembly and injected into
    /// `TuiLaunchConfig.workspace_root`; the TUI never probes the cwd itself.
    pub workspace_root: String,
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
        // Resolve the requested id through aliases and the provider catalog;
        // the catalog is the only deployment source, so unmatched names fail.
        let (deployment, wire_model) = crate::config::deployment_for(&settings, &request.name)
            .map_err(|error| AssembleError::new(error.0))?;
        // display_name falls back to the wire name (the composite id) when the
        // config omits it; the model catalog entry is the single source of truth.
        let display_name = settings
            .models
            .iter()
            .find(|choice| choice.id == deployment.model)
            .map(|choice| choice.display_name.clone())
            .unwrap_or_else(|| request.name.clone());
        let runtime_config =
            runtime_config_for(&self.flags.to_cli(), &settings, &deployment, &request.name)
                .map_err(|error| AssembleError::new(error.0))?;
        // An explicit install-time effort (model picker tier) freezes into
        // the new generation atomically and outranks flag/model defaults.
        let mut runtime_config = runtime_config;
        if let Some(effort) = request.effort {
            runtime_config.generation.reasoning_effort = Some(effort);
        }
        let model: Arc<dyn ModelPort> = Arc::new(
            build_model(&deployment, &wire_model, self.replay_store.clone())
                .map_err(|error| AssembleError::new(format!("{error}")))?,
        );
        let tools = tool_port_for(&settings, runtime_config.max_parallel_tool_calls)
            .map_err(|error| AssembleError::new(error.0))?;
        Ok(AssembledGeneration {
            model,
            tools,
            runtime_config,
            model_name: display_name,
            provider: Some(deployment.provider.clone()),
            model_id: deployment.model.clone(),
            image_input: deployment.image_input,
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

    fn note_reload_dispatch_failure(&self, reason: &str) {
        let mut fault = self.state.reload_fault.lock().expect("reload fault lock");
        if fault.is_none() {
            *fault = Some(format!("config not applied: {reason}"));
        }
    }

    #[cfg(test)]
    pub(crate) fn take_reload_fault(&self) -> Option<String> {
        self.state.take_reload_fault()
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

    fn list_models(&self) -> Vec<ModelListingEntry> {
        self.state
            .current_settings()
            .models
            .iter()
            .map(|choice| ModelListingEntry {
                id: choice.id.clone(),
                provider: choice.provider_id.clone(),
                model: choice.display_name.clone(),
                reasoning_tiers: choice
                    .reasoning_tiers
                    .iter()
                    .map(|tier| crate::config::effort_label(*tier).to_owned())
                    .collect(),
            })
            .collect()
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
            effort: None,
        })
        .map_err(|error| UsageError::new(format!("model assembly failed: {}", error.message)))?;
    let initial_generation = Arc::new(RuntimeGeneration {
        generation_id: GenerationId::new("generation-0"),
        model: assembled.model,
        tools: assembled.tools,
        runtime_config: assembled.runtime_config,
        display: GenerationDisplay {
            provider: assembled.provider,
            model_name: assembled.model_name,
            model_id: assembled.model_id,
            image_input: assembled.image_input,
        },
    });
    let session_store: Arc<dyn SessionStore> = sessions.clone();
    let generation_assembler: Arc<dyn GenerationAssembler> = assembler.clone();
    let parts = AgentRuntime::start(RuntimeDeps {
        sessions: session_store.clone(),
        ids,
        bounds: Default::default(),
    })
    .map_err(|error| UsageError::new(format!("cannot start the runtime: {}", error.message())))?;
    let (service, client) = philo_agent_service::start(ServiceDeps {
        runtime: parts.handle,
        subscription: parts.events,
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
        workspace_root: workspace_root()?,
    })
}

/// Resolves the composition root's working directory for `TuiLaunchConfig`.
/// Tool roots go through [`coding_profile`]; this only feeds the TUI.
fn workspace_root() -> Result<String, UsageError> {
    std::env::current_dir()
        .map_err(|error| UsageError::new(format!("cannot resolve the working directory: {error}")))
        .map(|path| path.to_string_lossy().into_owned())
}

/// Graceful service + store shutdown with a process-wide deadline.
///
/// Shutdown is sent on the supervisor lane, never the ordinary control mailbox.
/// `interrupt` stays live so a later Ctrl+C upgrades Drain to Forced and
/// shortens `deadline` to now.
pub(crate) async fn shutdown_with_deadline(
    bootstrap: Bootstrap,
    interrupt: &mut watch::Receiver<u64>,
    mut deadline: Instant,
) -> ProcessShutdownReport {
    let Bootstrap {
        service,
        sessions,
        assembler,
        ..
    } = bootstrap;
    let mut seen = skip_past_pulses(interrupt);
    let mut report = ProcessShutdownReport::default();
    deadline = shorten_deadline(interrupt, &mut seen, deadline);

    drain_service(&service, interrupt, &mut seen, &mut deadline, &mut report).await;

    deadline = shorten_deadline(interrupt, &mut seen, deadline);

    match timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || assembler.shutdown()),
    )
    .await
    {
        Ok(_) => {}
        Err(_) => report.pending.push("generation-build-pool".to_owned()),
    }
    deadline = shorten_deadline(interrupt, &mut seen, deadline);

    match timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || {
            let _ = sessions.shutdown();
        }),
    )
    .await
    {
        Ok(_) => {}
        Err(_) => report.pending.push("session-store".to_owned()),
    }
    report
}

/// Maps a requested process exit onto the shutdown report. UserExit with
/// leftover pending components is not success.
pub(crate) fn shutdown_exit_code(requested: u8, pending: &[String]) -> u8 {
    if !pending.is_empty() && requested == 0 {
        1
    } else {
        requested
    }
}

/// Drops a config watch under the same process deadline.
pub(crate) async fn join_watch_with_deadline(
    watch: WatchTask,
    deadline: Instant,
    report: &mut ProcessShutdownReport,
) {
    match timeout_at(deadline, tokio::task::spawn_blocking(move || drop(watch))).await {
        Ok(_) => {}
        Err(_) => report.pending.push("config-watch".to_owned()),
    }
}

pub(crate) fn shorten_deadline(
    interrupt: &mut watch::Receiver<u64>,
    seen: &mut u64,
    deadline: Instant,
) -> Instant {
    if take_pulses(interrupt, seen) > 0 {
        Instant::now()
    } else {
        deadline
    }
}

fn take_pulses(rx: &mut watch::Receiver<u64>, seen: &mut u64) -> u64 {
    let now = *rx.borrow_and_update();
    let delta = now.saturating_sub(*seen);
    *seen = now;
    delta
}

fn skip_past_pulses(rx: &mut watch::Receiver<u64>) -> u64 {
    *rx.borrow_and_update()
}

async fn drain_service(
    service: &philo_agent_service::AgentService,
    interrupt: &mut watch::Receiver<u64>,
    seen: &mut u64,
    deadline: &mut Instant,
    report: &mut ProcessShutdownReport,
) {
    *deadline = shorten_deadline(interrupt, seen, *deadline);
    let reason = if Instant::now() >= *deadline {
        "process forced exit"
    } else {
        "process shutdown"
    };
    if let Err(error) = service.shutdown_from_supervisor(reason, *deadline).await {
        report
            .diagnostics
            .push(format!("supervisor shutdown: {error}"));
    }

    let wait = service.wait_stopped();
    tokio::pin!(wait);
    loop {
        *deadline = shorten_deadline(interrupt, seen, *deadline);
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            biased;
            _ = &mut wait => break,
            _ = interrupt.changed() => {
                *deadline = Instant::now();
                if let Err(error) = service
                    .shutdown_from_supervisor("process interrupt", *deadline)
                    .await
                {
                    report
                        .diagnostics
                        .push(format!("supervisor forced shutdown: {error}"));
                }
            }
            _ = tokio::time::sleep(remaining) => {
                if let Err(error) = service
                    .shutdown_from_supervisor("process deadline", Instant::now())
                    .await
                {
                    report
                        .diagnostics
                        .push(format!("supervisor deadline shutdown: {error}"));
                }
                service.abort_actor();
                report.pending.push("service".to_owned());
                break;
            }
        }
    }
}

async fn timeout_at<T>(deadline: Instant, future: impl Future<Output = T>) -> Result<T, ()> {
    tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), future)
        .await
        .map_err(|_| ())
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
            enqueue_generation_command(
                client,
                assembler,
                FrontendCommand::InstallModel { name, effort: None },
            );
        }
        Ok((settings, _warnings)) => {
            if settings.data_dir != *assembler.data_dir() {
                assembler.set_reload_fault(
                    "config not reloaded: data_dir cannot be changed without restarting",
                );
                let name = assembler.current_settings().deployment.model;
                enqueue_generation_command(
                    client,
                    assembler,
                    FrontendCommand::InstallModel { name, effort: None },
                );
                return;
            }
            let previous = assembler.current_settings();
            assembler.store_settings(settings.clone());
            if !generation_fields_changed(&previous, &settings) {
                return;
            }
            if only_reasoning_changed(&previous, &settings)
                && let Ok((deployment, _)) =
                    crate::config::deployment_for(&settings, &settings.deployment.model)
            {
                let effort = effective_reasoning(
                    assembler.state.flags.reasoning_effort.is_some(),
                    &settings,
                    &deployment,
                );
                // An unset effective effort falls through to a full install,
                // which resets reasoning the same way startup would.
                if let Some(effort) = effort {
                    enqueue_generation_command(
                        client,
                        assembler,
                        FrontendCommand::SetReasoning {
                            effort: frontend_effort(effort),
                        },
                    );
                    return;
                }
            }
            enqueue_generation_command(
                client,
                assembler,
                FrontendCommand::InstallModel {
                    name: settings.deployment.model,
                    effort: None,
                },
            );
        }
    }
}

fn enqueue_generation_command(
    client: &FrontendClient,
    assembler: &CliGenerationAssembler,
    command: FrontendCommand,
) {
    match client.try_command(command) {
        CommandDispatch::Enqueued(_) => {}
        CommandDispatch::Backpressured => {
            assembler.note_reload_dispatch_failure("command lane full");
        }
        CommandDispatch::Disconnected { lane } => {
            assembler.note_reload_dispatch_failure(&format!("{lane} disconnected"));
        }
    }
}

fn frontend_effort(effort: ReasoningEffort) -> FrontendReasoningEffort {
    match effort {
        ReasoningEffort::Minimal => FrontendReasoningEffort::Minimal,
        ReasoningEffort::Low => FrontendReasoningEffort::Low,
        ReasoningEffort::Medium => FrontendReasoningEffort::Medium,
        ReasoningEffort::High => FrontendReasoningEffort::High,
        ReasoningEffort::Xhigh => FrontendReasoningEffort::Xhigh,
        ReasoningEffort::Max => FrontendReasoningEffort::Max,
    }
}

fn ui_entry(key: &str) -> bool {
    matches!(key, "verbosity" | "show_reasoning")
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
    // Compare the deployments the current models actually resolve to, so an
    // edited provider section counts even when the active model is unchanged.
    let left_resolves = crate::config::deployment_for(left, &left.deployment.model);
    let right_resolves = crate::config::deployment_for(right, &right.deployment.model);
    let (Ok((left_deployment, _)), Ok((right_deployment, _))) =
        (&left_resolves, &right_resolves)
    else {
        // A catalog that no longer resolves the active model always counts
        // as a generation change; the rebuild surfaces the error.
        return true;
    };
    left.deployment.model != right.deployment.model
        || deployment_fields_changed(left_deployment, right_deployment)
        || left.compaction != right.compaction
        || left.reasoning_effort != right.reasoning_effort
        || left.max_tool_rounds != right.max_tool_rounds
        || left.max_parallel_tool_calls != right.max_parallel_tool_calls
        || left.operation_timeout != right.operation_timeout
        || left.shell_timeout_secs != right.shell_timeout_secs
        || left.recovery != right.recovery
        || non_ui_entries(left) != non_ui_entries(right)
}

fn deployment_fields_changed(left: &Deployment, right: &Deployment) -> bool {
    left.provider != right.provider
        || left.endpoint != right.endpoint
        || left.protocol != right.protocol
        || left.credential != right.credential
        || left.compat != right.compat
        || left.chat_reasoning_format != right.chat_reasoning_format
        || left.continuation_policy != right.continuation_policy
        || left.max_output_tokens != right.max_output_tokens
        || left.default_reasoning != right.default_reasoning
        || left.image_input != right.image_input
        || left.cache_policy != right.cache_policy
        || left.response_head_timeout != right.response_head_timeout
        || left.stream_idle_timeout != right.stream_idle_timeout
        || header_names_of(&left.request_headers) != header_names_of(&right.request_headers)
}

fn header_names_of(headers: &ModelRequestHeaders) -> Vec<String> {
    headers.names().map(str::to_owned).collect()
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
///
/// Transport hardening is wired here, inside the SDK's frozen pre-2xx scope:
/// `RetryPolicy::transient` bounds connect/DNS/write failures and throttling
/// before a response starts, and the timeout policy turns hung response
/// heads and stalled streams into fast recoverable failures instead of
/// waiting on the operation deadline. Mid-stream recovery stays with the
/// turn engine (`RuntimeConfig.recovery`).
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
    .request_headers(deployment.request_headers.clone())
    .replay_store(replay_store)
    .compat(deployment.compat)
    .continuation_policy(deployment.continuation_policy)
    .cache_policy(deployment.cache_policy)
    .retry_policy(TRANSPORT_RETRY_POLICY)
    .timeout_policy(TimeoutPolicy {
        total: None,
        response_head: deployment.response_head_timeout,
        stream_idle: deployment.stream_idle_timeout,
    });
    builder = match &deployment.credential {
        crate::config::Credential::EnvName(name) => builder.api_key_env(name.clone()),
        crate::config::Credential::Literal(secret) => builder.api_key(secret.clone()),
    };
    if let Some(format) = deployment.chat_reasoning_format {
        builder = builder.chat_reasoning_format(format);
    }
    builder.build()
}

/// Bounded SDK attempt-level retries for transient faults before a 2xx
/// response (connect, TLS, request write, throttling). Full jitter and caps
/// come from the policy defaults; mid-stream faults are never retried here.
const TRANSPORT_RETRY_POLICY: RetryPolicy =
    RetryPolicy::transient(std::num::NonZeroU32::new(3).expect("three is non-zero"));

/// Maps resolved settings onto a RuntimeConfig the same way bootstrap does.
///
/// Model-level declarations win over `[defaults]`: the effective deployment
/// carries `max_output_tokens` / `default_reasoning` from the active model's
/// catalog entry, and an explicit `--reasoning-effort` flag beats both.
pub(crate) fn runtime_config_for(
    cli: &Cli,
    settings: &Settings,
    deployment: &Deployment,
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
    if let Some(effort) = effective_reasoning(cli.reasoning_effort.is_some(), settings, deployment)
    {
        runtime_config.generation.reasoning_effort = Some(effort);
    }
    if let Some(tokens) = effective_max_output(deployment) {
        runtime_config.generation.max_output_tokens = tokens;
    }
    runtime_config.operation_timeout = settings.operation_timeout;
    runtime_config.compaction = settings.compaction.clone();
    runtime_config.recovery = settings.recovery;
    Ok(runtime_config)
}

/// Reasoning precedence: explicit flag > the active model's default tier.
/// Without a flag, `settings.reasoning_effort` is `None`, so this collapses
/// to the model default.
fn effective_reasoning(
    flag_set: bool,
    settings: &Settings,
    deployment: &Deployment,
) -> Option<ReasoningEffort> {
    if flag_set {
        settings.reasoning_effort
    } else {
        settings.reasoning_effort.or(deployment.default_reasoning)
    }
}

/// Output cap: the active model's declared value; `None` keeps the coding
/// profile default.
fn effective_max_output(deployment: &Deployment) -> Option<u32> {
    deployment.max_output_tokens
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
                credential: crate::config::Credential::EnvName("PHILO_API_KEY".to_owned()),
                request_headers: ModelRequestHeaders::default(),
                compat: ModelCompat::Compatible,
                chat_reasoning_format: None,
                continuation_policy: ModelContinuationPolicy::StatelessLocalReplay,
                max_output_tokens: None,
                default_reasoning: None,
                image_input: true,
                cache_policy: philo_model::ModelCachePolicy::default(),
                response_head_timeout: None,
                stream_idle_timeout: None,
            },
            models: vec![crate::config::resolve::ModelChoice {
                id: model.to_owned(),
                provider_id: "test".to_owned(),
                model: model.to_owned(),
                endpoint: endpoint.to_owned(),
                protocol: ModelProtocol::OpenAiChat,
                credential: crate::config::Credential::EnvName("PHILO_API_KEY".to_owned()),
                compat: ModelCompat::Compatible,
                chat_reasoning_format: None,
                continuation_policy: ModelContinuationPolicy::StatelessLocalReplay,
                context_window: None,
                request_headers: Vec::new(),
                max_output_tokens: None,
                reasoning_tiers: Vec::new(),
                default_reasoning: None,
                image_input: true,
                cache_policy: philo_model::ModelCachePolicy::default(),
                display_name: model.to_owned(),
            }],
            aliases: Vec::new(),
            data_dir: dir.to_path_buf(),
            context_window: None,
            compaction: CompactionConfig::default(),
            reasoning_effort: None,
            max_tool_rounds: Some(1),
            max_parallel_tool_calls: Some(2),
            operation_timeout: Some(Duration::from_secs(30)),
            shell_timeout_secs: None,
            recovery: Default::default(),
            verbosity: Verbosity::Default,
            show_reasoning: true,
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
            effort: None,
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
            effort: None,
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
    fn runtime_config_applies_model_level_generation_params() {
        use clap::Parser as _;

        let dir = temp_dir("model-params");
        let base = settings(
            &dir,
            "gw/model-a",
            "https://example.test/v1/chat/completions",
        );

        // The active model's declarations win over profile defaults.
        let mut deployment = base.deployment.clone();
        deployment.max_output_tokens = Some(64_000);
        deployment.default_reasoning = Some(ReasoningEffort::High);
        let cli = crate::args::Cli::try_parse_from(["philo", "--model", "gw/model-a", "hi"])
            .expect("valid CLI");
        let config = runtime_config_for(&cli, &base, &deployment, "model-a").expect("config");
        assert_eq!(config.generation.max_output_tokens, 64_000);
        assert_eq!(
            config.generation.reasoning_effort,
            Some(ReasoningEffort::High)
        );

        // Without model declarations the profile defaults stand.
        let plain = base.deployment.clone();
        let config = runtime_config_for(&cli, &base, &plain, "model-a").expect("config");
        assert_ne!(config.generation.max_output_tokens, 0);
        assert_eq!(config.generation.reasoning_effort, None);

        // An explicit flag beats the model declaration. After resolve() the
        // flag value is baked into `settings.reasoning_effort`, and the flag
        // branch of `effective_reasoning` must keep it over the model
        // declaration.
        let mut flagged_settings = base.clone();
        flagged_settings.reasoning_effort = Some(ReasoningEffort::Low);
        let flagged =
            crate::args::Cli::try_parse_from(["philo", "--reasoning-effort", "low", "hi"])
                .expect("valid CLI");
        let config = runtime_config_for(&flagged, &flagged_settings, &deployment, "model-a")
            .expect("config");
        assert_eq!(
            config.generation.reasoning_effort,
            Some(ReasoningEffort::Low)
        );
        assert_eq!(config.generation.max_output_tokens, 64_000);

        // Deployment edits count as generation changes.
        assert!(deployment_fields_changed(&plain, &deployment));
        let mut next = deployment.clone();
        next.default_reasoning = Some(ReasoningEffort::Xhigh);
        assert!(deployment_fields_changed(&deployment, &next));
        next.cache_policy.retention = philo_model::CacheRetention::None;
        assert!(deployment_fields_changed(&deployment, &next));
        next.image_input = false;
        assert!(deployment_fields_changed(&deployment, &next));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reload_fault_is_consumed_once() {
        let dir = temp_dir("fault");
        let assembler = assembler(&dir, "https://example.test/v1/chat/completions");
        assembler.set_reload_fault("config not reloaded: invalid TOML");
        let Err(error) = assembler.assemble_sync(AssembleRequest {
            name: "model-a".to_owned(),
            effort: None,
        }) else {
            panic!("fault");
        };
        assert!(error.message.contains("invalid TOML"));
        // A subsequent assemble is not stuck on the stale fault; it may still
        // fail on model construction in tests without a real endpoint.
        let _ = assembler.assemble_sync(AssembleRequest {
            name: "model-a".to_owned(),
            effort: None,
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reload_keeps_parse_fault_when_install_cannot_enqueue() {
        let dir = temp_dir("reload-disconnected");
        let assembler = assembler(&dir, "https://example.test/v1/chat/completions");
        let (service, client, _runtime) = philo_agent_service::testing::start_test_service();
        philo_agent_service::testing::abort_service_actor_and_wait(&service).await;
        apply_config_reload(&client, &assembler, Err(UsageError::new("invalid TOML")));
        assert_eq!(
            assembler.take_reload_fault().as_deref(),
            Some("config not reloaded: invalid TOML")
        );
        drop(service);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reload_records_dispatch_failure_when_generation_command_is_dropped() {
        let dir = temp_dir("reload-backpressure");
        let assembler = assembler(&dir, "https://example.test/v1/chat/completions");
        let (service, client, _runtime, _hold) =
            philo_agent_service::testing::start_test_service_with_command_hold();
        for _ in 0..philo_agent_service::FRONTEND_COMMAND_CAP + 4 {
            match client.try_command(FrontendCommand::ReadStatus) {
                CommandDispatch::Enqueued(_) | CommandDispatch::Backpressured => {}
                CommandDispatch::Disconnected { lane } => panic!("disconnected: {lane}"),
            }
        }
        let mut next = assembler.current_settings();
        next.deployment.model = "model-b".to_owned();
        apply_config_reload(&client, &assembler, Ok((next, Vec::new())));
        let fault = assembler.take_reload_fault().expect("dispatch failure");
        assert!(
            fault.contains("command lane full"),
            "expected a visible dispatch failure, got {fault}"
        );
        drop(service);
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
                effort: None,
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

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_at_names_a_component_that_does_not_exit() {
        let result = timeout_at(Instant::now(), std::future::pending::<()>()).await;
        assert_eq!(result, Err(()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ctrl_c_pulse_shortens_deadline_to_now() {
        let (tx, mut rx) = watch::channel(0u64);
        let mut seen = skip_past_pulses(&mut rx);
        let later = Instant::now() + Duration::from_secs(30);
        tx.send_modify(|n| *n = n.saturating_add(1));
        let shortened = shorten_deadline(&mut rx, &mut seen, later);
        assert!(shortened <= Instant::now() + Duration::from_millis(20));
    }

    #[test]
    fn pending_components_turn_success_into_failure() {
        assert_eq!(shutdown_exit_code(0, &[]), 0);
        assert_eq!(shutdown_exit_code(0, &["service".into()]), 1);
        assert_eq!(shutdown_exit_code(130, &["service".into()]), 130);
        assert_eq!(shutdown_exit_code(1, &["generation-build-pool".into()]), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn elapsed_deadline_records_named_service_pending() {
        let (service, _client, runtime) = philo_agent_service::testing::start_test_service();
        let hold = runtime.hold_children();
        let (_tx, mut rx) = watch::channel(0u64);
        let mut seen = skip_past_pulses(&mut rx);
        let mut deadline = Instant::now();
        let mut report = ProcessShutdownReport::default();
        drain_service(&service, &mut rx, &mut seen, &mut deadline, &mut report).await;
        assert!(
            report.pending.iter().any(|name| name == "service"),
            "deadline must name the component: {report:?}"
        );
        assert_eq!(shutdown_exit_code(0, &report.pending), 1);
        drop(hold);
    }
}
