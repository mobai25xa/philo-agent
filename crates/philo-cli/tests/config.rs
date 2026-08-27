//! Black-box configuration checks over the real binary: the five-layer
//! chain, forward-compatible warnings, invalid values, the secret red line,
//! and the bare-command split into interactive mode. Nothing here touches
//! the network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-cli-config-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::create_dir_all(&path).expect("create dir");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The binary with the developer's environment and config layers removed;
/// `config_home` supplies the global layer.
fn philo(config_home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_philo"));
    command
        .env_remove("PHILO_MODEL")
        .env_remove("PHILO_ENDPOINT")
        .env_remove("PHILO_PROTOCOL")
        .env_remove("PHILO_COMPAT")
        .env_remove("PHILO_PROVIDER")
        .env_remove("PHILO_DATA_DIR")
        .env("PHILO_CONFIG_HOME", config_home);
    command
}

fn write_config(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).expect("create config dir");
    std::fs::write(dir.join("config.toml"), body).expect("write config");
}

/// TOML literal string: Windows paths keep their backslashes.
fn literal(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// A minimal valid provider catalog: one provider, one model.
fn catalog() -> String {
    concat!(
        "[providers.gw]\n",
        "endpoint = \"https://example.invalid/v1/chat/completions\"\n",
        "\n",
        "[providers.gw.models]\n",
        "model-a = {}\n",
    )
    .to_owned()
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn the_data_dir_follows_the_five_layer_chain() {
    let root = TempRoot::new();
    let config_home = root.dir("global-home");
    let project = root.dir("project");
    let empty_home = root.dir("empty-home");
    let global_store = root.dir("store-global");
    let project_store = root.dir("store-project");
    let env_store = root.dir("store-env");
    let flag_store = root.dir("store-flag");
    seed_session(&global_store, "from-global");
    seed_session(&project_store, "from-project");
    seed_session(&env_store, "from-env");
    seed_session(&flag_store, "from-flag");

    write_config(
        &config_home,
        &format!("data_dir = {}\n", literal(&global_store)),
    );
    write_config(
        &project.join(".philo"),
        &format!("data_dir = {}\n", literal(&project_store)),
    );

    let listed = |command: &mut Command| -> String {
        let output = command.output().expect("run");
        assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
        stdout_text(&output)
    };

    assert_eq!(
        listed(
            philo(&config_home)
                .current_dir(&project)
                .env("PHILO_DATA_DIR", &env_store)
                .args(["sessions", "--data-dir"])
                .arg(&flag_store)
        ),
        "from-flag  hi\n",
        "the flag outranks every other layer"
    );
    assert_eq!(
        listed(
            philo(&config_home)
                .current_dir(&project)
                .env("PHILO_DATA_DIR", &env_store)
                .arg("sessions")
        ),
        "from-env  hi\n",
        "the environment outranks both files"
    );
    assert_eq!(
        listed(philo(&config_home).current_dir(&project).arg("sessions")),
        "from-project  hi\n",
        "the project file outranks the global one"
    );
    assert_eq!(
        listed(philo(&config_home).current_dir(&root.path).arg("sessions")),
        "from-global  hi\n",
        "without a project file the global one applies"
    );

    let bare = listed(philo(&empty_home).current_dir(&root.path).arg("sessions"));
    for seeded in ["from-global", "from-project", "from-env", "from-flag"] {
        assert!(
            !bare.contains(seeded),
            "with no layer configured the default root applies: {bare}"
        );
    }
}

#[test]
fn unknown_keys_warn_and_keep_running() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    let store = root.dir("store");
    seed_session(&store, "alpha");
    write_config(
        &config_home,
        &format!(
            "data_dir = {}\nfrom_the_future = 1\n\
             [compaction]\ncontext_budget = 96000\nauto_threshold = 0.8\n\
             keep_recent_turns = 4\nestimate_bytes_per_token = 3\nfuture = true\n\
             [telemetry]\nenabled = true\n",
            literal(&store)
        ),
    );

    let output = philo(&config_home)
        .current_dir(&root.path)
        .arg("sessions")
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    assert_eq!(stdout_text(&output), "alpha  hi\n");
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("'from_the_future' is not a section"),
        "{stderr}"
    );
    assert!(
        stderr.contains("unknown key [compaction].future"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("unknown section [compaction]"),
        "the compaction key domain is recognized: {stderr}"
    );
    assert!(stderr.contains("unknown section [telemetry]"), "{stderr}");
}

#[test]
fn an_invalid_parallel_tool_cap_exits_two() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    write_config(
        &config_home,
        "[defaults]\nmax_parallel_tool_calls = 0\n",
    );

    let output = philo(&config_home)
        .current_dir(&root.path)
        .arg("hello")
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr_text(&output).contains("must be a positive integer"),
        "{}",
        stderr_text(&output)
    );
}

#[test]
fn an_invalid_value_is_a_usage_error() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    write_config(&config_home, "data_dir = 42\n");

    let output = philo(&config_home)
        .current_dir(&root.path)
        .arg("sessions")
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr_text(&output).contains("must be a string, found integer"),
        "{}",
        stderr_text(&output)
    );
}

#[test]
fn an_invalid_enum_names_the_layer_it_came_from() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    write_config(
        &config_home,
        concat!(
            "[providers.gw]\nendpoint = \"https://e.test\"\n",
            "protocol = \"carrier-pigeon\"\n",
            "[providers.gw.models]\nmodel-a = {}\n",
        ),
    );

    let output = philo(&config_home)
        .current_dir(&root.path)
        .arg("hello")
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("carrier-pigeon"), "{stderr}");
    assert!(
        stderr.contains("[providers.gw].protocol in the global config"),
        "{stderr}"
    );
}

#[test]
fn a_retired_protocol_name_is_a_usage_error() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    write_config(
        &config_home,
        concat!(
            "[providers.gw]\nendpoint = \"https://e.test\"\n",
            "protocol = \"openai-chat-compatible\"\n",
            "[providers.gw.models]\nmodel-a = {}\n",
        ),
    );

    let output = philo(&config_home)
        .current_dir(&root.path)
        .arg("hello")
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("openai-chat-compatible"), "{stderr}");
    assert!(stderr.contains("protocol=openai-chat"), "{stderr}");
    assert!(stderr.contains("compat=compatible"), "{stderr}");
    assert!(
        !stderr.contains("unknown key"),
        "retired names are hard errors: {stderr}"
    );
}

#[test]
fn the_removed_deployment_section_is_a_usage_error_not_a_warning() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    let store = root.dir("store");
    seed_session(&store, "alpha");
    write_config(
        &config_home,
        &format!(
            "[deployment]\ndata_dir = {}\nmodel = \"m\"\n",
            literal(&store)
        ),
    );

    let output = philo(&config_home)
        .current_dir(&root.path)
        .arg("sessions")
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("[deployment] has been removed"), "{stderr}");
    assert!(stderr.contains("[providers.<id>.models]"), "{stderr}");
    assert!(
        !stderr.contains("unknown section"),
        "the retired section must not be ignored: {stderr}"
    );
    assert_eq!(stdout_text(&output), "");
}

#[test]
fn an_invalid_compaction_threshold_exits_two_before_starting_an_operation() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    write_config(
        &config_home,
        &format!(
            "{}[compaction]\nauto_threshold = 1.1\n",
            catalog()
        ),
    );

    let output = philo(&config_home)
        .current_dir(&root.path)
        .arg("hello")
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("[compaction].auto_threshold"), "{stderr}");
    assert!(stderr.contains("greater than 0 and at most 1"), "{stderr}");
    assert!(
        !stderr.contains("session:"),
        "operation was not started: {stderr}"
    );
}

#[test]
fn a_secret_in_the_config_is_refused_without_echoing_it() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    write_config(
        &config_home,
        "[providers.gw]\ntoken = \"sk-must-never-be-used\"\n",
    );

    let output = philo(&config_home)
        .current_dir(&root.path)
        .arg("sessions")
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("would store a secret"), "{stderr}");
    assert!(stderr.contains("api_key_env"), "{stderr}");
    assert!(
        !stderr.contains("sk-must-never-be-used"),
        "the value never reaches any output: {stderr}"
    );

    write_config(
        &config_home,
        "[providers.gw]\napi_key_env = \"sk-also-must-not-be-used\"\n",
    );
    let output = philo(&config_home)
        .current_dir(&root.path)
        .arg("sessions")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("must be an environment variable name"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("sk-also-must-not-be-used"),
        "a misplaced key never reaches output: {stderr}"
    );
}

#[test]
fn a_literal_api_key_is_sanctioned_and_stays_out_of_output() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    let store = root.dir("store");
    seed_session(&store, "alpha");
    write_config(
        &config_home,
        &format!(
            "data_dir = {}\n\
             [providers.gw]\nendpoint = \"https://example.invalid/v1/chat/completions\"\n\
             api_key = \"sk-file-literal-secret\"\n\
             [providers.gw.models]\nmodel-a = {{}}\n",
            literal(&store)
        ),
    );

    // The read-only sessions command resolves the config successfully.
    let output = philo(&config_home)
        .current_dir(&root.path)
        .arg("sessions")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    assert_eq!(stdout_text(&output), "alpha  hi\n");
    let stdout = stdout_text(&output);
    let stderr = stderr_text(&output);
    for text in [stdout.as_str(), stderr.as_str()] {
        assert!(
            !text.contains("sk-file-literal-secret"),
            "the literal key never reaches output: {text}"
        );
    }
}

#[test]
fn a_credential_header_is_refused_without_echoing_its_value() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    write_config(
        &config_home,
        concat!(
            "[providers.gw]\nendpoint = \"https://e.test\"\n",
            "[providers.gw.headers]\nAuthorization = \"Bearer header-secret\"\n",
            "[providers.gw.models]\nmodel-a = {}\n",
        ),
    );

    let output = philo(&config_home)
        .current_dir(&root.path)
        .arg("hello")
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("authorization"), "{stderr}");
    assert!(stderr.contains("must not be configured"), "{stderr}");
    assert!(!stderr.contains("header-secret"), "{stderr}");
}

#[test]
fn a_bare_command_without_a_terminal_reports_the_interactive_requirement() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    write_config(&config_home, &catalog());

    // Test harnesses give the child pipes, not a terminal: the interactive
    // session refuses before touching the terminal, and says how to run
    // single-shot instead.
    let output = philo(&config_home)
        .current_dir(&root.path)
        .output()
        .expect("run");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr_text(&output).contains("needs a terminal"),
        "{}",
        stderr_text(&output)
    );
}

#[test]
fn both_modes_validate_the_same_effective_configuration() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    write_config(&config_home, &format!("{}\n[ui]\nverbosity = \"loud\"\n", catalog()));

    for args in [Vec::<&str>::new(), vec!["hello"]] {
        let output = philo(&config_home)
            .current_dir(&root.path)
            .args(args)
            .output()
            .expect("run");
        assert_eq!(output.status.code(), Some(2));
        let stderr = stderr_text(&output);
        assert!(
            stderr.contains("invalid [ui].verbosity 'loud'"),
            "both entry paths resolve the same setting: {stderr}"
        );
        assert!(
            stderr.contains("[ui].verbosity in the global config"),
            "both entry paths retain the same source layer: {stderr}"
        );
        assert!(
            !stderr.contains("needs a terminal"),
            "configuration is resolved before TUI startup: {stderr}"
        );
    }
}

#[test]
fn removed_ui_screen_is_a_hard_error() {
    let root = TempRoot::new();
    let config_home = root.dir("home");
    write_config(&config_home, &format!("{}\n[ui]\nscreen = \"inline\"\n", catalog()));

    for args in [Vec::<&str>::new(), vec!["hello"]] {
        let output = philo(&config_home)
            .current_dir(&root.path)
            .args(args)
            .output()
            .expect("run");
        assert_eq!(output.status.code(), Some(2));
        let stderr = stderr_text(&output);
        assert!(
            stderr.contains("unknown key [ui].screen") && stderr.contains("removed in v4.0"),
            "screen must be a hard removal error: {stderr}"
        );
        assert!(
            !stderr.contains("needs a terminal"),
            "configuration is resolved before TUI startup: {stderr}"
        );
    }
}

/// Seeds one committed session through the public store API.
fn seed_session(root: &Path, id: &str) {
    use philo_session::{
        OperationId, SessionEntryKind, SessionId, SessionRevision, SessionStore,
        SessionTransaction, SessionUserPart, TurnId,
    };
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
    }

    let store = philo_session_jsonl::JsonlSessionStore::open(root).expect("open store");
    block_on(store.commit(SessionTransaction::linear(
        SessionId::new(id),
        SessionRevision::ZERO,
        vec![
            SessionEntryKind::OperationStarted {
                operation_id: OperationId::new("op-1"),
            },
            SessionEntryKind::TurnStarted {
                operation_id: OperationId::new("op-1"),
                turn_id: TurnId::new("turn-1"),
            },
            SessionEntryKind::UserMessage {
                turn_id: TurnId::new("turn-1"),
                parts: SessionUserPart::text_parts("hi"),
            },
        ],
    )))
    .expect("seed commit");
}
