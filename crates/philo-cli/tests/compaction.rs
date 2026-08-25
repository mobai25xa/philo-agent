//! CLI-005: the real single-shot binary applies the shared compaction
//! configuration and renders automatic compaction on stderr.

use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use philo_session::{
    OperationId, OperationOutcome, SessionAssistantBlock, SessionEntryKind, SessionId,
    SessionRevision, SessionStore, SessionTransaction, SessionUserPart, TurnId, TurnOutcome,
};

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "philo-cli-m13-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self(path)
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(&path).expect("create temp directory");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn single_shot_uses_compaction_config_and_renders_automatic_events() {
    let root = TempRoot::new();
    let config_home = root.dir("config-home");
    let sessions_dir = root.dir("sessions");
    seed_completed_turns(&sessions_dir, "continued", 2);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub model server");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("stub address")
    );
    let server = serve_model(listener, ["durable summary", "final answer"]);

    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "data_dir = '{}'\n\
             [providers.stub]\nendpoint = \"{endpoint}\"\nprotocol = \"openai-chat\"\n\
             [providers.stub.headers]\n\"User-Agent\" = \"philo-cli-test/1\"\n\
             \"X-Title\" = \"Philo CLI Test\"\n\
             [providers.stub.models]\n\"stub-model\" = {{}}\n\
             [compaction]\ncontext_budget = 1\nauto_threshold = 0.8\n\
             keep_recent_turns = 1\nestimate_bytes_per_token = 1\n",
            sessions_dir.display()
        ),
    )
    .expect("write config");

    let output = isolated_philo(&config_home)
        .current_dir(&root.0)
        .args(["--session", "continued", "continue"])
        .output()
        .expect("run single-shot binary");
    let requests = server.join().expect("stub model server");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert_eq!(stdout, "final answer\n");
    assert!(stderr.contains("compacting context..."), "{stderr}");
    assert!(stderr.contains("context compacted"), "{stderr}");
    assert_eq!(requests.len(), 2, "summary call followed by turn call");
    for request in &requests {
        assert!(
            request.contains("user-agent: philo-cli-test/1"),
            "{request}"
        );
        assert!(request.contains("x-title: Philo CLI Test"), "{request}");
    }
    assert!(
        requests[1].contains("Summary of earlier conversation:\\ndurable summary"),
        "the post-compaction turn consumes the durable summary: {}",
        requests[1]
    );
}

fn isolated_philo(config_home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_philo"));
    command
        .env_remove("PHILO_MODEL")
        .env_remove("PHILO_ENDPOINT")
        .env_remove("PHILO_PROTOCOL")
        .env_remove("PHILO_COMPAT")
        .env_remove("PHILO_PROVIDER")
        .env_remove("PHILO_DATA_DIR")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env("PHILO_API_KEY", "test-key")
        .env("PHILO_CONFIG_HOME", config_home);
    command
}

fn seed_completed_turns(root: &Path, session: &str, count: usize) {
    let store = philo_session_jsonl::JsonlSessionStore::open(root).expect("open session store");
    let session_id = SessionId::new(session);
    let mut revision = SessionRevision::ZERO;
    for index in 1..=count {
        let operation_id = OperationId::new(format!("seed-operation-{index}"));
        let turn_id = TurnId::new(format!("seed-turn-{index}"));
        let commit = block_on(store.commit(SessionTransaction::linear(
            session_id.clone(),
            revision,
            vec![
                SessionEntryKind::OperationStarted {
                    operation_id: operation_id.clone(),
                },
                SessionEntryKind::TurnStarted {
                    operation_id: operation_id.clone(),
                    turn_id: turn_id.clone(),
                },
                SessionEntryKind::UserMessage {
                    turn_id: turn_id.clone(),
                    parts: SessionUserPart::text_parts(format!("seed question {index}")),
                },
                SessionEntryKind::AssistantMessage {
                    turn_id: turn_id.clone(),
                    blocks: vec![SessionAssistantBlock::Text {
                        text: format!("seed answer {index}"),
                    }],
                },
                SessionEntryKind::TurnTerminated {
                    turn_id,
                    outcome: TurnOutcome::Succeeded,
                },
                SessionEntryKind::OperationSettled {
                    operation_id,
                    outcome: OperationOutcome::Succeeded,
                },
            ],
        )))
        .expect("seed completed turn");
        revision = commit.revision();
    }
}

fn serve_model(
    listener: TcpListener,
    responses: [&'static str; 2],
) -> std::thread::JoinHandle<Vec<String>> {
    listener
        .set_nonblocking(true)
        .expect("configure stub listener");
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut requests = Vec::new();
        while requests.len() < responses.len() && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .expect("configure accepted stub stream");
                    requests.push(read_http_request(&mut stream));
                    write_sse_response(&mut stream, responses[requests.len() - 1]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept stub request: {error}"),
            }
        }
        requests
    })
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set request timeout");
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).expect("read stub request");
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        let Some(header_end) = find_bytes(&bytes, b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8(bytes).expect("request is UTF-8")
}

fn write_sse_response(stream: &mut TcpStream, text: &str) {
    let json_text = format!("\"{}\"", text.replace('"', "\\\""));
    let body = format!(
        "data: {{\"id\":\"response-1\",\"object\":\"chat.completion.chunk\",\
         \"model\":\"stub-model\",\"choices\":[{{\"index\":0,\"delta\":{{\
         \"role\":\"assistant\"}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"id\":\"response-1\",\"object\":\"chat.completion.chunk\",\
         \"model\":\"stub-model\",\"choices\":[{{\"index\":0,\"delta\":{{\
         \"content\":{json_text}}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"id\":\"response-1\",\"object\":\"chat.completion.chunk\",\
         \"model\":\"stub-model\",\"choices\":[{{\"index\":0,\"delta\":{{}},\
         \"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .expect("write stub response");
    stream.flush().expect("flush stub response");
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}
