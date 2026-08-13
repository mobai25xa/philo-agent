#![allow(dead_code)]

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream;
use futures::{Stream, StreamExt};
use http::header::CONTENT_TYPE;
use http::{HeaderMap, HeaderValue, StatusCode, Version};
use philo::api::extension::{HttpRequest, HttpResponse, HttpResponseHead, Transport};
use philo::api::stable::{DeliveryState, TransportStage};
use philo_agent_runtime::{
    GenerationConfig, ModelCallId, ModelCallSnapshot, ModelError, ModelEvent, ModelEventStream,
    ModelMessage, OperationId, ToolDefinition, TurnId,
};
use philo_model::{ModelProtocol, PhiloModelAdapter};

/// Observes whether the HTTP response body stream has been dropped, proving
/// that dropping the normalized event stream aborts the underlying call.
#[derive(Clone, Debug, Default)]
pub struct BodyDropFlag(Arc<AtomicBool>);

impl BodyDropFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_dropped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

struct TrackedBody {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, philo::api::extension::TransportError>> + Send>>,
    flag: BodyDropFlag,
}

impl Stream for TrackedBody {
    type Item = Result<Bytes, philo::api::extension::TransportError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl Drop for TrackedBody {
    fn drop(&mut self) {
        self.flag.0.store(true, Ordering::SeqCst);
    }
}

/// One scripted transport exchange.
pub enum StubResponse {
    /// 200 with a `text/event-stream` body.
    Sse(Vec<u8>),
    /// 200 SSE that emits `head` bytes, then stays pending forever. The flag
    /// records when the body stream is dropped.
    SseSuspended { head: Vec<u8>, flag: BodyDropFlag },
    /// An arbitrary status with an `application/json` body.
    Status(StatusCode, Vec<u8>),
    /// Fails at connect before anything is sent.
    ConnectError,
    /// Never resolves; used to exercise timeout policies.
    Hang,
}

/// Scripted SDK transport. Test-support only; never part of production APIs.
#[derive(Clone)]
pub struct StubTransport {
    scripts: Arc<Mutex<VecDeque<StubResponse>>>,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl StubTransport {
    pub fn new(scripts: impl IntoIterator<Item = StubResponse>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().expect("stub requests lock").clone()
    }

    pub fn request_bodies(&self) -> Vec<serde_json::Value> {
        self.requests()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).expect("request body is JSON"))
            .collect()
    }
}

impl Transport for StubTransport {
    fn send(
        &self,
        request: HttpRequest,
    ) -> BoxFuture<'static, Result<HttpResponse, philo::api::extension::TransportError>> {
        self.requests
            .lock()
            .expect("stub requests lock")
            .push(request);
        let script = self
            .scripts
            .lock()
            .expect("stub scripts lock")
            .pop_front()
            .expect("stub transport called more times than scripted");
        match script {
            StubResponse::Sse(body) => Box::pin(async move {
                Ok(response(
                    "text/event-stream; charset=utf-8",
                    StatusCode::OK,
                    body,
                ))
            }),
            StubResponse::SseSuspended { head, flag } => Box::pin(async move {
                let body = TrackedBody {
                    inner: Box::pin(stream::iter([Ok(Bytes::from(head))]).chain(stream::pending())),
                    flag,
                };
                let mut headers = HeaderMap::new();
                headers.insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream; charset=utf-8"),
                );
                Ok(HttpResponse {
                    head: HttpResponseHead {
                        status: StatusCode::OK,
                        version: Version::HTTP_11,
                        headers,
                    },
                    body: Box::pin(body),
                })
            }),
            StubResponse::Status(status, body) => {
                Box::pin(async move { Ok(response("application/json", status, body)) })
            }
            StubResponse::ConnectError => Box::pin(async {
                Err(philo::api::extension::TransportError::new(
                    TransportStage::Connect,
                    DeliveryState::NotSent,
                ))
            }),
            StubResponse::Hang => Box::pin(futures::future::pending()),
        }
    }
}

fn response(content_type: &'static str, status: StatusCode, body: Vec<u8>) -> HttpResponse {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    HttpResponse {
        head: HttpResponseHead {
            status,
            version: Version::HTTP_11,
            headers,
        },
        body: Box::pin(stream::iter([Ok(Bytes::from(body))])),
    }
}

pub const STUB_ENDPOINT: &str = "https://stub.invalid/v1/chat/completions";

/// Assembles the production adapter (OpenAI Chat, official shape) over a stub
/// transport through the same builder real callers use.
pub fn adapter_over(transport: StubTransport) -> PhiloModelAdapter {
    PhiloModelAdapter::builder(
        "stub-provider",
        ModelProtocol::OpenAiChat,
        "stub-model",
        STUB_ENDPOINT,
    )
    .build_with_transport(transport)
    .expect("stub adapter assembly succeeds")
}

/// Joins SSE data records into one response body.
pub fn sse(records: &[&str]) -> Vec<u8> {
    let mut body = String::new();
    for record in records {
        body.push_str("data: ");
        body.push_str(record);
        body.push_str("\n\n");
    }
    body.into_bytes()
}

/// A minimal streamed text response: role chunk, one content chunk per delta,
/// stop, DONE.
pub fn text_sse(response_id: &str, response_model: &str, deltas: &[&str]) -> Vec<u8> {
    let mut records = vec![format!(
        r#"{{"id":"{response_id}","object":"chat.completion.chunk","model":"{response_model}","choices":[{{"index":0,"delta":{{"role":"assistant"}},"finish_reason":null}}]}}"#
    )];
    for delta in deltas {
        records.push(format!(
            r#"{{"id":"{response_id}","object":"chat.completion.chunk","model":"{response_model}","choices":[{{"index":0,"delta":{{"content":{}}},"finish_reason":null}}]}}"#,
            serde_json::Value::String((*delta).to_owned())
        ));
    }
    records.push(format!(
        r#"{{"id":"{response_id}","object":"chat.completion.chunk","model":"{response_model}","choices":[{{"index":0,"delta":{{}},"finish_reason":"stop"}}]}}"#
    ));
    records.push("[DONE]".to_owned());
    let records: Vec<&str> = records.iter().map(String::as_str).collect();
    sse(&records)
}

pub fn snapshot(messages: Vec<ModelMessage>, tools: Vec<ToolDefinition>) -> ModelCallSnapshot {
    ModelCallSnapshot {
        operation_id: OperationId::new("operation-1"),
        turn_id: TurnId::new("turn-1"),
        model_call_id: ModelCallId::new("model-call-1"),
        model_call_index: 1,
        session_revision: philo_session::SessionRevision::ZERO,
        messages,
        tools,
        model_target: "stub-model".to_owned(),
        generation: GenerationConfig {
            max_output_tokens: 256,
            temperature: 0.25,
            reasoning_effort: None,
            tool_choice: philo_agent_runtime::ToolChoice::Auto,
        },
    }
}

/// Snapshot with an explicit reasoning effort and free call identity fields,
/// for multi-call replay tests.
pub fn reasoning_snapshot(
    turn_id: &str,
    model_call_index: u32,
    effort: Option<philo_agent_runtime::ReasoningEffort>,
    messages: Vec<ModelMessage>,
    tools: Vec<ToolDefinition>,
) -> ModelCallSnapshot {
    ModelCallSnapshot {
        operation_id: OperationId::new("operation-1"),
        turn_id: TurnId::new(turn_id),
        model_call_id: ModelCallId::new(format!("{turn_id}:model-call:{model_call_index}")),
        model_call_index,
        session_revision: philo_session::SessionRevision::ZERO,
        messages,
        tools,
        model_target: "stub-model".to_owned(),
        generation: GenerationConfig {
            max_output_tokens: 256,
            temperature: 0.25,
            reasoning_effort: effort,
            tool_choice: philo_agent_runtime::ToolChoice::Auto,
        },
    }
}

pub fn read_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "read",
        "Read a file",
        r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#,
        philo_agent_runtime::EffectClass::ReadOnly,
    )
    .expect("read tool definition")
}

/// Drains a normalized stream to completion.
pub async fn collect(mut stream: Box<dyn ModelEventStream>) -> Vec<Result<ModelEvent, ModelError>> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

/// Unwraps a stream where every event is expected to be Ok.
pub async fn collect_ok(stream: Box<dyn ModelEventStream>) -> Vec<ModelEvent> {
    collect(stream)
        .await
        .into_iter()
        .map(|event| event.expect("stream event is ok"))
        .collect()
}
