//! Interactive-mode TOML watch: poll mtimes, then atomically reload.
//!
//! Single-shot mode never starts this task. Discovery matches startup:
//! project `.philo/config.toml` and global `~/.philo/config.toml` (or
//! `PHILO_CONFIG_HOME`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;

use super::LoadedConfig;
use super::resolve::Settings;
use crate::args::Cli;
use crate::error::UsageError;

const CONFIG_HOME_ENV: &str = "PHILO_CONFIG_HOME";
const CONFIG_FILE: &str = "config.toml";
const DEFAULT_POLL: Duration = Duration::from_millis(500);
const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(200);

static ACTIVE_WATCHES: AtomicUsize = AtomicUsize::new(0);

/// Command-line flags frozen at process start so a reload keeps flag > file.
#[derive(Clone, Debug)]
pub struct ResolveFlags {
    pub model: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub system: Option<String>,
    pub max_tool_rounds: Option<u32>,
    pub reasoning_effort: Option<String>,
    pub verbose: bool,
    pub quiet: bool,
}

impl ResolveFlags {
    pub fn from_cli(cli: &Cli) -> Self {
        Self {
            model: cli.model.clone(),
            data_dir: cli.data_dir.clone(),
            system: cli.system.clone(),
            max_tool_rounds: cli.max_tool_rounds,
            reasoning_effort: cli.reasoning_effort.clone(),
            verbose: cli.verbose,
            quiet: cli.quiet,
        }
    }

    pub fn to_cli(&self) -> Cli {
        Cli {
            command: None,
            message: None,
            session: None,
            model: self.model.clone(),
            data_dir: self.data_dir.clone(),
            system: self.system.clone(),
            max_tool_rounds: self.max_tool_rounds,
            reasoning_effort: self.reasoning_effort.clone(),
            image: Vec::new(),
            verbose: self.verbose,
            quiet: self.quiet,
        }
    }
}

/// Poll and debounce intervals. Production uses ≤ 500ms / 200ms.
#[derive(Clone, Copy, Debug)]
pub struct WatchIntervals {
    pub poll: Duration,
    pub debounce: Duration,
}

impl Default for WatchIntervals {
    fn default() -> Self {
        Self {
            poll: DEFAULT_POLL,
            debounce: DEFAULT_DEBOUNCE,
        }
    }
}

/// The two TOML paths startup already knows about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchedPaths {
    pub global: Option<PathBuf>,
    pub project: PathBuf,
}

impl WatchedPaths {
    pub fn from_locations(config_home: Option<&Path>, workspace_root: &Path) -> Self {
        let global = match config_home {
            Some(dir) => Some(dir.join(CONFIG_FILE)),
            None => dirs::home_dir().map(|home| home.join(".philo").join(CONFIG_FILE)),
        };
        Self {
            global,
            project: workspace_root.join(".philo").join(CONFIG_FILE),
        }
    }

    pub fn discover() -> Result<Self, UsageError> {
        let workspace_root = std::env::current_dir().map_err(|error| {
            UsageError::new(format!("cannot resolve the working directory: {error}"))
        })?;
        let config_home = std::env::var_os(CONFIG_HOME_ENV).map(PathBuf::from);
        Ok(Self::from_locations(
            config_home.as_deref(),
            &workspace_root,
        ))
    }
}

/// mtime snapshot of the two watched files. Missing files are `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamps {
    pub global: Option<SystemTime>,
    pub project: Option<SystemTime>,
}

impl FileStamps {
    pub fn capture(paths: &WatchedPaths) -> Self {
        Self {
            global: paths.global.as_deref().and_then(mtime),
            project: mtime(&paths.project),
        }
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// Atomically reload through the same entry points as startup.
pub fn reload_settings(flags: &ResolveFlags) -> Result<(Settings, Vec<String>), UsageError> {
    let loaded = LoadedConfig::load()?;
    let warnings = loaded.warnings().to_vec();
    let settings = loaded.resolve_run(&flags.to_cli())?;
    Ok((settings, warnings))
}

/// Abort-on-drop handle. Creating one is the only way a watch task exists.
pub struct WatchTask {
    handle: JoinHandle<()>,
}

impl Drop for WatchTask {
    fn drop(&mut self) {
        self.handle.abort();
        ACTIVE_WATCHES.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Number of live [`WatchTask`] values. `LoadedConfig::load` never increments this.
#[cfg_attr(not(test), allow(dead_code))]
pub fn active_watch_count() -> usize {
    ACTIVE_WATCHES.load(Ordering::SeqCst)
}

/// Starts the interactive TOML watch. Single-shot must not call this.
pub fn spawn(
    flags: ResolveFlags,
    intervals: WatchIntervals,
    on_reload: impl FnMut(Result<(Settings, Vec<String>), UsageError>) + Send + 'static,
    on_poll: impl FnMut() + Send + 'static,
) -> Result<WatchTask, UsageError> {
    let paths = WatchedPaths::discover()?;
    Ok(spawn_with(
        paths,
        intervals,
        move || reload_settings(&flags),
        on_reload,
        on_poll,
    ))
}

pub(super) fn spawn_with<R>(
    paths: WatchedPaths,
    intervals: WatchIntervals,
    reload: R,
    on_reload: impl FnMut(Result<(Settings, Vec<String>), UsageError>) + Send + 'static,
    on_poll: impl FnMut() + Send + 'static,
) -> WatchTask
where
    R: FnMut() -> Result<(Settings, Vec<String>), UsageError> + Send + 'static,
{
    ACTIVE_WATCHES.fetch_add(1, Ordering::SeqCst);
    WatchTask {
        handle: tokio::spawn(run_loop(paths, intervals, reload, on_reload, on_poll)),
    }
}

pub(super) async fn run_loop<R>(
    paths: WatchedPaths,
    intervals: WatchIntervals,
    mut reload: R,
    mut on_reload: impl FnMut(Result<(Settings, Vec<String>), UsageError>),
    mut on_poll: impl FnMut(),
) where
    R: FnMut() -> Result<(Settings, Vec<String>), UsageError>,
{
    let mut last = FileStamps::capture(&paths);
    loop {
        tokio::time::sleep(intervals.poll).await;
        let now = FileStamps::capture(&paths);
        if now != last {
            last = stabilize(&paths, intervals.debounce).await;
            on_reload(reload());
        }
        on_poll();
    }
}

pub(super) async fn stabilize(paths: &WatchedPaths, debounce: Duration) -> FileStamps {
    loop {
        let stamps = FileStamps::capture(paths);
        tokio::time::sleep(debounce).await;
        if FileStamps::capture(paths) == stamps {
            return stamps;
        }
    }
}

#[cfg(test)]
pub(super) fn reload_from_layers(
    flags: &ResolveFlags,
    global: Option<&Path>,
    project: Option<&Path>,
) -> Result<(Settings, Vec<String>), UsageError> {
    let file = super::file::load_layers(global, project)?;
    let warnings = file.warnings.clone();
    let settings = super::resolve::resolve(&flags.to_cli(), &file)?;
    Ok((settings, warnings))
}
