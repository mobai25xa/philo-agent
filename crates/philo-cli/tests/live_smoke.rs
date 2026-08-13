//! M9-010 live smoke: opt-in real-API checks over the actual binary, never
//! a CI gate. Configure the deployment and run with `--ignored`:
//!
//! ```text
//! set PHILO_MODEL=some-model
//! set PHILO_ENDPOINT=https://api.example.com/v1/chat/completions
//! set PHILO_PROTOCOL=openai-chat-compatible   (optional)
//! set PHILO_API_KEY=<secret>
//! cargo test -p philo-cli --test live_smoke -- --ignored
//! ```

use std::path::PathBuf;
use std::process::Command;

fn live_configured() -> bool {
    std::env::var("PHILO_MODEL").is_ok() && std::env::var("PHILO_ENDPOINT").is_ok()
}

fn temp_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("philo-cli-live-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create data dir");
    dir
}

#[test]
#[ignore = "live smoke is opt-in: configure PHILO_MODEL / PHILO_ENDPOINT / PHILO_API_KEY"]
fn live_new_session_direct_answer() {
    assert!(
        live_configured(),
        "PHILO_MODEL and PHILO_ENDPOINT must be set"
    );
    let data_dir = temp_data_dir("new");
    let output = Command::new(env!("CARGO_BIN_EXE_philo"))
        .args(["--data-dir"])
        .arg(&data_dir)
        .arg("Reply with exactly the single word OK.")
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(!output.stdout.is_empty(), "the answer streams to stdout");
    assert!(
        stderr.contains("session: "),
        "the fresh session id echoes to stderr"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
#[ignore = "live smoke is opt-in: configure PHILO_MODEL / PHILO_ENDPOINT / PHILO_API_KEY"]
fn live_session_continuation_references_earlier_content() {
    assert!(
        live_configured(),
        "PHILO_MODEL and PHILO_ENDPOINT must be set"
    );
    let data_dir = temp_data_dir("continue");
    let session = "live-continuation";
    let first = Command::new(env!("CARGO_BIN_EXE_philo"))
        .args(["--data-dir"])
        .arg(&data_dir)
        .args(["--session", session])
        .arg("Remember this token: PHILO-LIVE-M9. Reply OK.")
        .output()
        .expect("run first turn");
    assert_eq!(first.status.code(), Some(0));

    let second = Command::new(env!("CARGO_BIN_EXE_philo"))
        .args(["--data-dir"])
        .arg(&data_dir)
        .args(["--session", session])
        .arg("What token did I ask you to remember? Reply with the token only.")
        .output()
        .expect("run second turn");
    assert_eq!(second.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("PHILO-LIVE-M9"),
        "the continuation sees the first turn"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
#[ignore = "live smoke is opt-in: configure PHILO_MODEL / PHILO_ENDPOINT / PHILO_API_KEY"]
fn live_image_question() {
    assert!(
        live_configured(),
        "PHILO_MODEL and PHILO_ENDPOINT must be set"
    );
    let data_dir = temp_data_dir("image");
    let image_path = std::env::var("PHILO_LIVE_IMAGE")
        .expect("set PHILO_LIVE_IMAGE to a small local image file for this test");
    let output = Command::new(env!("CARGO_BIN_EXE_philo"))
        .args(["--data-dir"])
        .arg(&data_dir)
        .args(["--image", &image_path])
        .arg("Describe this image in one short sentence.")
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(!output.stdout.is_empty());
    let _ = std::fs::remove_dir_all(&data_dir);
}
