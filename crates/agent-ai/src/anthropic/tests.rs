use super::*;
use axum::{
    body::Body,
    extract::{Request as HttpRequest, State},
    http::Response,
    routing::{get, post},
    Router,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct Mock {
    replies: Arc<Mutex<VecDeque<(u16, String)>>>,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

async fn handler(State(mock): State<Mock>, request: HttpRequest) -> Response<Body> {
    assert_eq!(request.headers()["x-api-key"], "test-key");
    assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
    assert_eq!(request.headers()["content-type"], "application/json");
    let uri = request.uri().to_string();
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    mock.requests.lock().unwrap().push((uri, value));
    let (status, body) = mock.replies.lock().unwrap().pop_front().unwrap_or((500, "unexpected request".into()));
    // Split even UTF-8 and JSON across transport chunks.
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> = body.as_bytes().chunks(7).map(|chunk| Ok(chunk.to_vec())).collect();
    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .header("request-id", "req-test")
        .header("retry-after", "0")
        .body(Body::from_stream(futures::stream::iter(chunks)))
        .unwrap()
}

struct Server {
    url: String,
    mock: Mock,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server(replies: Vec<(u16, String)>) -> Server {
    let mock = Mock { replies: Arc::new(Mutex::new(replies.into())), requests: Default::default() };
    let app = Router::new().route("/v1/messages", post(handler)).route("/v1/models", get(handler)).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server { url, mock, task }
}

fn request() -> Request {
    Request {
        model: "claude-sonnet-4-5".into(),
        system_prompt: Some("be helpful".into()),
        messages: vec![Message::user_text("hello")],
        tools: vec![],
        reasoning: None,
        api: Api::Auto,
        max_tokens: None,
        llm_max_retries: Some(1),
    }
}

fn sse(events: Vec<Value>) -> String {
    events.into_iter().map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap())).collect()
}
fn text_events() -> Vec<Value> {
    vec![
        json!({"type":"message_start", "message":{"id":"msg-test", "usage":{"input_tokens":10, "cache_read_input_tokens":20, "cache_creation_input_tokens":30}}}),
        json!({"type":"ping"}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":"你好"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":" world"}}),
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ]
}
async fn collect(provider: &AnthropicProvider, req: Request) -> Vec<AssistantEvent> {
    tokio::time::timeout(Duration::from_secs(10), provider.stream(req, CancellationToken::new()).collect()).await.unwrap()
}
fn done(events: &[AssistantEvent]) -> AssistantMessage {
    match events.last().unwrap() {
        AssistantEvent::Done { message } => message.clone(),
        other => panic!("expected success: {other:?}"),
    }
}

#[tokio::test]
async fn text_stream_and_cache_usage() {
    let server = server(vec![(200, sse(text_events()))]).await;
    let provider = AnthropicProvider::new(crate::Secret::new("test-key"), Some(format!("{}/v1/", server.url)));
    assert_eq!(provider.id(), "anthropic");
    let events = collect(&provider, request()).await;
    assert!(matches!(events.first(), Some(AssistantEvent::Start)));
    assert_eq!(events.iter().filter(|e| matches!(e, AssistantEvent::TextDelta { .. })).count(), 2);
    let message = done(&events);
    assert_eq!(message.text(), "你好 world");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!((message.usage.input, message.usage.output, message.usage.cache_read, message.usage.cache_write, message.usage.total_tokens), (10, 5, 20, 30, 65));
    assert!(message.api.is_none());
}

#[tokio::test]
async fn parallel_tool_calls_and_split_json() {
    let body = sse(vec![
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"a", "name":"read", "input":{}}}),
        json!({"type":"content_block_start", "index":1, "content_block":{"type":"tool_use", "id":"b", "name":"ls", "input":{}}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"{\"path\":"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"\"a.rs\"}"}}),
        json!({"type":"content_block_stop", "index":1}),
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":12}}),
        json!({"type":"message_stop"}),
    ]);
    let server = server(vec![(200, body)]).await;
    let provider = AnthropicProvider::new(crate::Secret::new("test-key"), Some(server.url.clone()));
    let events = collect(&provider, request()).await;
    let message = done(&events);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let calls = message.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!((calls[0].id.as_str(), &calls[0].arguments), ("a", &json!({"path":"a.rs"})));
    assert_eq!((calls[1].id.as_str(), &calls[1].arguments), ("b", &json!({})));
    let ends: Vec<_> = events.iter().filter_map(|e| if let AssistantEvent::ToolcallEnd { content_index, .. } = e { Some(*content_index) } else { None }).collect();
    assert_eq!(ends, [1, 0]);
}

#[tokio::test]
async fn thinking_signature_and_redacted_round_trip() {
    let body = sse(vec![
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"thinking", "thinking":""}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"thinking_delta", "thinking":"consider"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"signature_delta", "signature":"signed-"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"signature_delta", "signature":"value"}}),
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"content_block_start", "index":1, "content_block":{"type":"redacted_thinking", "data":"opaque"}}),
        json!({"type":"content_block_stop", "index":1}),
        json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ]);
    let server = server(vec![(200, body), (200, sse(text_events()))]).await;
    let provider = AnthropicProvider::new(crate::Secret::new("test-key"), Some(server.url.clone()));
    let mut req = request();
    req.reasoning = Some(ThinkingLevel::High);
    let events = collect(&provider, req.clone()).await;
    assert!(events.iter().any(|e| matches!(e, AssistantEvent::ThinkingDelta { delta, .. } if delta == "consider")));
    let message = done(&events);
    let stored = serde_json::to_string(&message).unwrap();
    let restored: AssistantMessage = serde_json::from_str(&stored).unwrap();
    assert_eq!(message, restored);
    req.messages.push(Message::Assistant(restored));
    req.messages.push(Message::user_text("continue"));
    done(&collect(&provider, req).await);
    let requests = server.mock.requests.lock().unwrap();
    let content = &requests[1].1["messages"][1]["content"];
    assert_eq!(content[0], json!({"type":"thinking", "thinking":"consider", "signature":"signed-value"}));
    assert_eq!(content[1], json!({"type":"redacted_thinking", "data":"opaque"}));
}

#[test]
fn body_shape_merging_caching_images_and_budgets() {
    let mut req = request();
    req.reasoning = Some(ThinkingLevel::High);
    req.max_tokens = Some(10);
    req.messages.push(Message::user_text("second"));
    let mut assistant = AssistantMessage::empty(&req.model);
    assistant.content.push(Content::Text { text: "checking".into() });
    assistant.content.push(Content::ToolCall(ToolCall { id: "a".into(), name: "read".into(), arguments: json!({}) }));
    req.messages.push(Message::Assistant(assistant.clone()));
    req.messages.push(Message::Assistant(assistant));
    for id in ["a", "b"] {
        req.messages.push(Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![Content::Text { text: "result".into() }, Content::Image { data: "aGVsbG8=".into(), mime_type: "image/png".into() }],
            is_error: id == "b",
            timestamp: 0,
        }));
    }
    req.tools = vec![ToolSpec { name: "read".into(), description: "read file".into(), parameters: json!({"type":"object"}) }; 2];
    let body = build_body(&req);
    assert_eq!(body["stream"], true);
    assert_eq!(body["system"][0], json!({"type":"text", "text":"be helpful", "cache_control":{"type":"ephemeral"}}));
    assert_eq!(body["tools"][1]["input_schema"], json!({"type":"object"}));
    assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
    assert!(body["tools"][0].get("cache_control").is_none());
    assert_eq!(body["messages"].as_array().unwrap().len(), 3);
    assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 2);
    assert_eq!(body["messages"][1]["content"].as_array().unwrap().len(), 4);
    let results = &body["messages"][2];
    assert_eq!(results["role"], "user");
    assert_eq!(results["content"].as_array().unwrap().len(), 2);
    assert_eq!(results["content"][0]["tool_use_id"], "a");
    assert_eq!(results["content"][1]["is_error"], true);
    assert_eq!(results["content"][0]["content"][1]["source"], json!({"type":"base64", "media_type":"image/png", "data":"aGVsbG8="}));
    assert_eq!(results["content"][1]["cache_control"]["type"], "ephemeral");
    assert_eq!(body.to_string().matches("cache_control").count(), 3);
    assert!(body["max_tokens"].as_u64().unwrap() > body["thinking"]["budget_tokens"].as_u64().unwrap());
    assert!(body.get("temperature").is_none() && body.get("top_k").is_none());
    for level in [ThinkingLevel::Off, ThinkingLevel::Minimal] {
        req.reasoning = Some(level);
        assert!(build_body(&req).get("thinking").is_none());
    }
    req.max_tokens = None;
    assert_eq!(build_body(&req)["max_tokens"], 16384);
    req.model = "claude-3-7-sonnet-20250219".into();
    // Older Claude models use the conservative default too.
    req.model = "claude-3-5-sonnet-20241022".into();
    assert_eq!(build_body(&req)["max_tokens"], 8192);
}

#[tokio::test]
async fn transient_failures_retry_and_discard_partial_text() {
    let cut = sse(text_events()[..4].to_vec());
    for first in [
        (529, "overloaded".into()),
        (200, cut),
        (200, sse(vec![json!({"type":"error", "error":{"type":"overloaded_error", "message":"busy"}})])),
        (200, sse(vec![json!({"type":"error", "error":{"type":"api_error", "message":"busy"}})])),
    ] {
        let server = server(vec![first, (200, sse(text_events()))]).await;
        let provider = AnthropicProvider::new(crate::Secret::new("test-key"), Some(server.url.clone()));
        let events = collect(&provider, request()).await;
        assert_eq!(done(&events).text(), "你好 world");
        assert_eq!(events.iter().filter(|e| matches!(e, AssistantEvent::Retry { attempt: 1, .. })).count(), 1);
        assert_eq!(server.mock.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn errors_after_tool_start_never_retry() {
    let body = sse(vec![
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"a", "name":"read", "input":{}}}),
        json!({"type":"error", "error":{"type":"overloaded_error", "message":"busy"}}),
    ]);
    let server = server(vec![(200, body)]).await;
    let provider = AnthropicProvider::new(crate::Secret::new("test-key"), Some(server.url.clone()));
    let events = collect(&provider, request()).await;
    assert!(events.iter().any(|e| matches!(e, AssistantEvent::ToolcallStart { .. })));
    assert!(!events.iter().any(|e| matches!(e, AssistantEvent::Retry { .. })));
    assert!(matches!(events.last(), Some(AssistantEvent::Error { .. })));
    assert_eq!(server.mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn auth_errors_redact_key_and_do_not_retry() {
    for code in [401, 403] {
        let server = server(vec![(code, "invalid API key: test-key sk-ant-partial***".into())]).await;
        let provider = AnthropicProvider::new(crate::Secret::new("test-key"), Some(server.url.clone()));
        let events = collect(&provider, request()).await;
        let Some(AssistantEvent::Error { message }) = events.last() else { panic!("expected error") };
        let error = message.error_message.as_ref().unwrap();
        assert!(error.contains(&format!("Anthropic HTTP {code}")));
        assert!(!error.contains("test-key") && !error.contains("sk-ant"));
        assert_eq!(server.mock.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn thinking_adjustment_is_not_transient_retry() {
    let server = server(vec![(400, "thinking is not supported on this model".into()), (200, sse(text_events()))]).await;
    let provider = AnthropicProvider::new(crate::Secret::new("test-key"), Some(server.url.clone()));
    let mut req = request();
    req.reasoning = Some(ThinkingLevel::High);
    req.llm_max_retries = Some(0);
    let events = collect(&provider, req).await;
    assert_eq!(done(&events).reasoning_effort, None);
    assert!(!events.iter().any(|e| matches!(e, AssistantEvent::Retry { .. })));
    let requests = server.mock.requests.lock().unwrap();
    assert!(requests[0].1.get("thinking").is_some());
    assert!(requests[1].1.get("thinking").is_none());
}

#[tokio::test]
async fn models_paginate() {
    let server = server(vec![
        (200, json!({"data":[{"id":"claude-sonnet-4", "created_at":"2025-05-14T00:00:00Z"}], "has_more":true, "last_id":"claude-sonnet-4"}).to_string()),
        (200, json!({"data":[{"id":"claude-opus-4"}], "has_more":false, "last_id":"claude-opus-4"}).to_string()),
    ])
    .await;
    let provider = AnthropicProvider::new(crate::Secret::new("test-key"), Some(server.url.clone()));
    let models = provider.list_models().await.unwrap();
    assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["claude-opus-4", "claude-sonnet-4"]);
    assert!(models.iter().all(|m| crate::models::is_chat_model(&m.id)));
    assert!(models[1].created.is_some());
    let requests = server.mock.requests.lock().unwrap();
    assert_eq!(requests[0].0, "/v1/models?limit=1000");
    assert_eq!(requests[1].0, "/v1/models?limit=1000&after_id=claude-sonnet-4");
}

#[tokio::test]
async fn idle_timeout_and_cancellation() {
    async fn stalled() -> Response<Body> {
        let partial = sse(text_events()[..4].to_vec());
        let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(partial) }).chain(futures::stream::pending());
        Response::new(Body::from_stream(stream))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/v1/messages", post(stalled))).await.unwrap();
    });
    let provider = AnthropicProvider::with_stream_idle_timeout(crate::Secret::new("test-key"), Some(url), Duration::from_millis(50));
    let mut req = request();
    req.llm_max_retries = Some(0);
    let events = collect(&provider, req.clone()).await;
    assert!(matches!(events.last(), Some(AssistantEvent::Error { message }) if message.error_message.as_ref().unwrap().contains("stream idle timeout")));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let events = provider.stream(req, cancel).collect::<Vec<_>>().await;
    assert!(matches!(events.last(), Some(AssistantEvent::Error { message }) if message.stop_reason == StopReason::Aborted));
    task.abort();
}

#[test]
fn stop_reasons_and_legacy_serialization() {
    for (reason, expected) in [
        ("end_turn", StopReason::Stop),
        ("stop_sequence", StopReason::Stop),
        ("max_tokens", StopReason::Length),
        ("tool_use", StopReason::ToolUse),
        ("refusal", StopReason::Stop),
        ("pause_turn", StopReason::Stop),
    ] {
        let mut state = StreamState::new("test");
        assert!(state.on_event(&json!({"type":"message_delta", "delta":{"stop_reason":reason}}), &mut vec![]).is_ok());
        assert_eq!(state.message.stop_reason, expected);
    }
    let thinking: Content = serde_json::from_value(json!({"type":"thinking", "thinking":"legacy"})).unwrap();
    assert_eq!(thinking, Content::Thinking { thinking: "legacy".into(), signature: None });
    let usage: Usage = serde_json::from_value(json!({"input":1, "output":2, "cacheRead":0, "totalTokens":3})).unwrap();
    assert_eq!(usage.cache_write, 0);
    assert!(serde_json::to_value(usage).unwrap().get("cacheWrite").is_none());
}
