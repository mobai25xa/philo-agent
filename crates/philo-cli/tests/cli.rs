//! Offline black-box checks over the real binary: usage errors exit 2
//! before any operation starts, and `sessions` lists through the store's
//! public API. No test here touches the network.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

fn philo() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_philo"));
    // Isolate from developer environment, including any real config file:
    // the global layer is pointed at a directory that holds none.
    command
        .env_remove("PHILO_MODEL")
        .env_remove("PHILO_ENDPOINT")
        .env_remove("PHILO_PROTOCOL")
        .env_remove("PHILO_COMPAT")
        .env_remove("PHILO_PROVIDER")
        .env_remove("PHILO_DATA_DIR")
        .env(
            "PHILO_CONFIG_HOME",
            std::env::temp_dir().join(format!("philo-cli-no-config-{}", std::process::id())),
        )
        .current_dir(std::env::temp_dir());
    command
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-cli-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A minimal valid provider catalog written as the project layer config of a
/// fresh workspace; the binary runs with that workspace as its cwd.
const CATALOG_CONFIG: &str = concat!(
    "[providers.gw]\n",
    "endpoint = \"https://example.invalid/v1/chat/completions\"\n",
    "\n",
    "[providers.gw.models]\n",
    "\"model-a\" = { reasoning = [\"low\", \"high\"] }\n",
);

fn workspace_with_config(config: &str) -> TempRoot {
    let root = TempRoot::new();
    let philo_dir = root.path.join(".philo");
    std::fs::create_dir_all(&philo_dir).expect("create .philo dir");
    std::fs::write(philo_dir.join("config.toml"), config).expect("write project config");
    root
}

// --- Usage errors exit 2 before the operation starts ---------------------------

#[test]
fn no_arguments_is_a_usage_error() {
    let output = philo().output().expect("run");
    assert_eq!(output.status.code(), Some(2), "{}", stderr_text(&output));
}

#[test]
fn verbose_and_quiet_are_mutually_exclusive() {
    let output = philo()
        .args(["--verbose", "--quiet", "hello"])
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn missing_model_configuration_is_a_usage_error() {
    let root = TempRoot::new();
    let output = philo()
        .args(["--data-dir"])
        .arg(&root.path)
        .arg("hello")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr_text(&output).contains("no models configured"),
        "diagnostic names the missing configuration: {}",
        stderr_text(&output)
    );
}

#[test]
fn missing_endpoint_is_a_usage_error() {
    let root = workspace_with_config("[providers.gw]\n[providers.gw.models]\nmodel-a = {}\n");
    let output = philo()
        .current_dir(&root.path)
        .args(["--data-dir"])
        .arg(&root.path)
        .arg("hello")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr_text(&output).contains("[providers.gw] needs an endpoint"),
        "{}",
        stderr_text(&output)
    );
}

#[test]
fn invalid_reasoning_effort_is_a_usage_error() {
    let root = workspace_with_config(CATALOG_CONFIG);
    let output = philo()
        .current_dir(&root.path)
        .args(["--data-dir"])
        .arg(root.path.join("sessions"))
        .args(["--reasoning-effort", "extreme", "hello"])
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr_text(&output).contains("invalid reasoning effort 'extreme'"),
        "{}",
        stderr_text(&output)
    );
}

#[test]
fn unreadable_image_is_a_usage_error() {
    let root = workspace_with_config(CATALOG_CONFIG);
    let output = philo()
        .current_dir(&root.path)
        .args(["--data-dir"])
        .arg(root.path.join("sessions"))
        .args(["--image", "definitely-missing.png", "hello"])
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr_text(&output).contains("cannot read image"),
        "{}",
        stderr_text(&output)
    );
}

#[test]
fn unknown_image_extension_is_a_usage_error() {
    let root = workspace_with_config(CATALOG_CONFIG);
    let document = root.path.join("notes.txt");
    std::fs::write(&document, "not an image").expect("write");
    let output = philo()
        .current_dir(&root.path)
        .args(["--data-dir"])
        .arg(root.path.join("sessions"))
        .arg("--image")
        .arg(&document)
        .arg("hello")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr_text(&output).contains("unsupported image extension"),
        "{}",
        stderr_text(&output)
    );
}

#[test]
fn invalid_protocol_is_a_usage_error() {
    let root = workspace_with_config(concat!(
        "[providers.gw]\n",
        "endpoint = \"https://example.invalid/v1/chat/completions\"\n",
        "protocol = \"carrier-pigeon\"\n",
        "[providers.gw.models]\nmodel-a = {}\n",
    ));
    let output = philo()
        .current_dir(&root.path)
        .args(["--data-dir"])
        .arg(root.path.join("sessions"))
        .arg("hello")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("carrier-pigeon"), "{stderr}");
    assert!(stderr.contains("[providers.gw].protocol"), "{stderr}");
}

#[test]
fn retired_protocol_env_is_a_usage_error() {
    let root = workspace_with_config(concat!(
        "[providers.gw]\n",
        "endpoint = \"https://example.invalid/v1/chat/completions\"\n",
        "protocol = \"openai-chat-compatible\"\n",
        "[providers.gw.models]\nmodel-a = {}\n",
    ));
    let output = philo()
        .current_dir(&root.path)
        .args(["--data-dir"])
        .arg(root.path.join("sessions"))
        .arg("hello")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("[providers.gw].protocol"), "{stderr}");
    assert!(stderr.contains("protocol=openai-chat"), "{stderr}");
    assert!(stderr.contains("compat=compatible"), "{stderr}");
}

// --- sessions subcommand ---------------------------------------------------------

#[test]
fn sessions_on_a_missing_or_empty_root_lists_nothing() {
    let root = TempRoot::new();
    let missing = root.path.join("never-created");
    let output = philo()
        .args(["sessions", "--data-dir"])
        .arg(&missing)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout_text(&output), "");

    let output = philo()
        .args(["sessions", "--data-dir"])
        .arg(&root.path)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout_text(&output), "");
}

#[test]
fn sessions_lists_ids_written_by_the_real_store() {
    let root = TempRoot::new();
    seed_session(&root.path, "alpha");
    seed_session(&root.path, "beta-2");

    let output = philo()
        .args(["sessions", "--data-dir"])
        .arg(&root.path)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    assert_eq!(
        stdout_text(&output),
        "alpha  hi\nbeta-2  hi\n",
        "sorted ids with derived titles on stdout"
    );
}

/// Seeds one committed session through the public store API.
fn seed_session(root: &std::path::Path, id: &str) {
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
