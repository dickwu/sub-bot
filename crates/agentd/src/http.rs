use crate::{
    config::{Client, Config},
    hub::{Hub, SessionHandle, SessionRequest, TaskStatus, TooManySessions, WebhookQueue},
    webhook::WebhookTarget,
};
use agent_ai::Message;
use agent_core::AgentEvent;
use axum::{
    extract::{Extension, Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::{convert::Infallible, net::SocketAddr, sync::Arc};

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
}

pub fn random_token() -> String {
    use rand::Rng;
    let bytes: [u8; 24] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Bearer token -> Client（每个 bot 一个 token），写入请求扩展供 handler 使用。
async fn auth(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let span = tracing::Span::current();
    let client = req.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).and_then(|t| st.hub.config.client_for_token(t)).cloned();
    match client {
        Some(c) => {
            span.record("bot", c.name.as_str());
            req.extensions_mut().insert(Arc::new(c));
            next.run(req).await
        }
        None => {
            // 不记录 token 本身
            tracing::warn!("unauthorized request (missing or unknown bearer token)");
            (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response()
        }
    }
}

type Caller = Extension<Arc<Client>>;

/// 每个 HTTP 请求一个 span：request_id（沿用 X-Request-Id 或新生成）、method、path、bot；结束时记录状态码和耗时。
async fn trace_request(req: Request, next: Next) -> Response {
    use tracing::Instrument;
    let request_id = req.headers().get("x-request-id").and_then(|v| v.to_str().ok()).filter(|v| v.len() <= 64).map(str::to_string).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let span = tracing::info_span!("http", request_id = %request_id, method = %req.method(), path = %req.uri().path(), bot = tracing::field::Empty);
    let start = std::time::Instant::now();
    async move {
        let mut resp = next.run(req).await;
        let status = resp.status().as_u16();
        let ms = start.elapsed().as_millis() as u64;
        if status >= 500 {
            tracing::error!(status, latency_ms = ms, "request finished");
        } else if status >= 400 {
            tracing::warn!(status, latency_ms = ms, "request finished");
        } else {
            tracing::info!(status, latency_ms = ms, "request finished");
        }
        if let Ok(v) = request_id.parse() {
            resp.headers_mut().insert("x-request-id", v);
        }
        resp
    }
    .instrument(span)
    .await
}

fn router(st: AppState) -> Router {
    let api = Router::new()
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/v1/sessions/{id}/prompt", post(prompt))
        .route("/v1/sessions/{id}/events", get(events))
        .route("/v1/sessions/{id}/abort", post(abort))
        .route("/v1/sessions/{id}/messages", get(messages))
        .route("/v1/tasks/{id}", get(get_task))
        .route("/v1/chat", post(chat))
        .route("/v1/profiles", get(profiles))
        .route("/v1/models", get(models))
        .layer(middleware::from_fn_with_state(st.clone(), auth));
    Router::new().route("/v1/health", get(|| async { Json(json!({"ok": true})) })).merge(api).layer(middleware::from_fn(trace_request)).with_state(st)
}

pub async fn serve(hub: Arc<Hub>, bind: SocketAddr) -> anyhow::Result<()> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let mut tasks = tokio::task::JoinSet::new();
    if let Some(period) = hub.sweep_interval() {
        let hub = hub.clone();
        tasks.spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                hub.evict_idle(std::time::Instant::now());
            }
        });
    }
    // JoinSet 在退出（包括错误/取消）时终止后台清理任务。
    axum::serve(listener, router(AppState { hub: hub.clone() }))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
            tracing::info!("shutting down; aborting sessions");
            hub.abort_all().await;
        })
        .await?;
    Ok(())
}

type ApiResult = Result<Response, (StatusCode, Json<serde_json::Value>)>;
fn not_found() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::NOT_FOUND, Json(json!({"error": "session not found"})))
}

fn bad_request(e: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()})))
}

fn create_error(e: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    if e.is::<TooManySessions>() {
        (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": e.to_string()})))
    } else {
        bad_request(e)
    }
}

async fn get_session(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>) -> ApiResult {
    let h = st.hub.get(&c, &id).ok_or_else(not_found)?;
    Ok(Json(json!({"sessionId": h.id, "profile": h.profile, "cwd": h.cwd, "model": h.agent.config.model, "isStreaming": h.agent.is_running()})).into_response())
}

async fn delete_session(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>) -> ApiResult {
    if !st.hub.remove(&c, &id) {
        return Err(not_found());
    }
    Ok(Json(json!({"deleted": true})).into_response())
}

/// 列出当前 bot 可用的 profile 及其工具（不含任何密钥）。
async fn profiles(State(st): State<AppState>, Extension(c): Caller) -> ApiResult {
    let list: Vec<_> = c
        .profiles
        .iter()
        .filter_map(|n| st.hub.config.profiles.get(n).map(|p| json!({"name": n, "model": p.model.clone().unwrap_or_else(|| st.hub.config.default_model.clone()), "allowedModels": p.allowed_models, "reasoningEffort": p.reasoning_effort.or(st.hub.config.default_reasoning_effort), "api": p.api.unwrap_or(st.hub.config.default_api), "maxOutputTokens": p.max_output_tokens, "tokenBudget": p.token_budget, "tools": p.tools, "workspaceRoots": p.workspace_roots, "allowCallerTools": p.allow_caller_tools, "allowSystemPromptOverride": p.allow_system_prompt_override})))
        .collect();
    Ok(Json(json!({"client": c.name, "profiles": list, "registeredTools": st.hub.tool_names()})).into_response())
}

#[derive(Deserialize, Default)]
struct ModelsQuery {
    /// 只返回该 profile 允许的模型
    profile: Option<String>,
    /// 不过滤（包含 embedding、语音、快照等）
    #[serde(default)]
    all: bool,
}

/// 实时模型列表（来自 provider 的 /v1/models，缓存 10 分钟），附带推断的推理强度。
async fn models(State(st): State<AppState>, Extension(c): Caller, Query(q): Query<ModelsQuery>) -> ApiResult {
    let mut list = st.hub.list_models(q.all).await.map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({"error": e.to_string()}))))?;
    if let Some(name) = &q.profile {
        if !c.profiles.contains(name) {
            return Err(bad_request(format!("profile '{name}' is not allowed for this client")));
        }
        let p = st.hub.config.profiles.get(name).ok_or_else(|| bad_request(format!("unknown profile '{name}'")))?;
        list.retain(|m| Config::model_allowed(p, &m.id));
    }
    Ok(Json(json!({"provider": st.hub.provider_id(), "models": list})).into_response())
}

async fn create_session(State(st): State<AppState>, Extension(c): Caller, body: Option<Json<SessionRequest>>) -> ApiResult {
    let b = body.map(|b| b.0).unwrap_or_default();
    let h = st.hub.create_session(&c, b).map_err(create_error)?;
    Ok((StatusCode::CREATED, Json(json!({"sessionId": h.id, "profile": h.profile, "cwd": h.cwd, "model": h.agent.config.model, "reasoningEffort": h.agent.config.reasoning, "requestedReasoningEffort": h.requested_effort, "api": st.hub.effective_api(&h.agent.config), "tools": h.agent.config.tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>()}))).into_response())
}

#[derive(Deserialize)]
struct PromptBody {
    message: String,
    /// "steer" | "follow_up"：运行中时如何排队
    mode: Option<String>,
}

async fn prompt(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>, Json(b): Json<PromptBody>) -> ApiResult {
    let d = st.hub.submit(&c, &id, b.message, b.mode.as_deref()).ok_or_else(not_found)?;
    Ok((StatusCode::ACCEPTED, Json(json!({"disposition": d}))).into_response())
}

fn sse_stream(rx: tokio::sync::broadcast::Receiver<AgentEvent>, stop_on_settled: bool) -> impl Stream<Item = Result<Event, Infallible>> {
    // 用 unfold 在发出 agent_settled 后立即结束流（take_while 要等到下一个事件才会结束，导致 /v1/chat 流挂起）
    futures::stream::unfold((rx, false), move |(mut rx, done)| async move {
        if done {
            return None;
        }
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let settled = stop_on_settled && matches!(ev, AgentEvent::AgentSettled { .. });
                    return Some((ev, (rx, settled)));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "SSE subscriber lagged; events dropped")
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .map(|ev| {
        // `event:` 名与 pi AgentEvent.type 一致，便于客户端按事件类型订阅
        let v = serde_json::to_value(&ev).unwrap_or_default();
        let name = v["type"].as_str().unwrap_or("event").to_string();
        Ok(Event::default().event(name).json_data(&v).unwrap_or_default())
    })
}

async fn events(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>) -> ApiResult {
    let h = st.hub.get(&c, &id).ok_or_else(not_found)?;
    Ok(Sse::new(sse_stream(h.subscribe(), false)).keep_alive(Default::default()).into_response())
}

async fn abort(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>) -> ApiResult {
    st.hub.get(&c, &id).ok_or_else(not_found)?.agent.abort();
    Ok(Json(json!({"ok": true})).into_response())
}

async fn messages(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>) -> ApiResult {
    let h = st.hub.get(&c, &id).ok_or_else(not_found)?;
    Ok(Json(json!({"messages": h.agent.messages().await})).into_response())
}

async fn get_task(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>) -> ApiResult {
    let task = st.hub.get_task(&c, &id).ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error": "task not found"}))))?;
    Ok(Json(task).into_response())
}

#[derive(Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WebhookEvents {
    #[default]
    Final,
    All,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatBody {
    message: String,
    session_id: Option<String>,
    /// 新建会话时使用（profile / systemPrompt / tools / callerTools）
    #[serde(default)]
    session: SessionRequest,
    #[serde(default)]
    stream: bool,
    #[serde(default, rename = "async")]
    asynchronous: bool,
    callback_url: Option<String>,
    #[serde(default)]
    webhook_events: WebhookEvents,
    /// 非流式等待上限；不填使用全局 chatTimeoutSecs。
    timeout_secs: Option<u64>,
}

/// A host entry allows any port; host:port restricts it. The configured URL implicitly
/// allows its host. No URL input is included in errors or logs.
fn webhook_target(c: &Client, callback: Option<&str>) -> Result<Option<WebhookTarget>, (StatusCode, Json<serde_json::Value>)> {
    let Some(value) = callback.or(c.webhook_url.as_deref()) else { return Ok(None) };
    let url = crate::config::webhook_url(value).ok_or_else(|| bad_request("callbackUrl must be an http/https URL"))?;
    let host = url.host_str().unwrap_or_default();
    let authority = format!("{host}:{}", url.port_or_known_default().unwrap_or_default());
    let implicit = c.webhook_url.as_deref().and_then(crate::config::webhook_url).is_some_and(|default| default.host_str() == Some(host));
    if callback.is_some() && !implicit && !c.webhook_allowed_hosts.iter().any(|allowed| allowed.eq_ignore_ascii_case(host) || allowed.eq_ignore_ascii_case(&authority)) {
        return Err(bad_request("callback host not allowed"));
    }
    Ok(Some(WebhookTarget { url: url.into(), secret: c.webhook_secret.clone() }))
}

/// 一步到位接口：给只想“发一句话拿回答”的 bot 用。
async fn chat(State(st): State<AppState>, Extension(c): Caller, Json(b): Json<ChatBody>) -> ApiResult {
    if st.hub.is_shutting_down() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "server is shutting down"}))));
    }
    if b.asynchronous && b.stream {
        return Err(bad_request("async and stream cannot be combined"));
    }
    let target = webhook_target(&c, b.callback_url.as_deref())?;
    let timeout_secs = b.timeout_secs.unwrap_or(st.hub.config.chat_timeout_secs);
    let h = match b.session_id {
        Some(id) => st.hub.get(&c, &id).ok_or_else(not_found)?,
        None => st.hub.create_chat_session(&c, b.session).map_err(create_error)?,
    };
    let rx = h.subscribe(); // 先订阅再提交，避免漏事件
    let submit_error = || {
        if st.hub.is_shutting_down() {
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "server is shutting down"})))
        } else {
            not_found()
        }
    };
    if b.asynchronous {
        let task = st.hub.submit_task(&c, &h.id, b.message).ok_or_else(submit_error)?;
        let status_url = format!("/v1/tasks/{}", task.task_id);
        let queue = target.map(|target| st.hub.webhook_queue(task.task_id.clone(), target));
        let response = (
            StatusCode::ACCEPTED,
            [("location", status_url.clone())],
            Json(json!({
                "taskId": task.task_id, "sessionId": h.id, "status": "running", "statusUrl": status_url,
            })),
        )
            .into_response();
        // Detached from the request future: disconnecting cannot cancel the run or its result.
        tokio::spawn(async move {
            let progress = if b.webhook_events == WebhookEvents::All { queue.as_ref().map(|q| (task.task_id.as_str(), q)) } else { None };
            let outcome = collect_chat(&h, rx, timeout_secs, progress).await;
            if let Some(task) = st.hub.finish_task(&task.task_id, outcome.status, outcome.body) {
                if let Some(queue) = queue {
                    let _ = queue.send(("task.completed", json!(task)));
                }
            }
        });
        return Ok(response);
    }
    st.hub.submit(&c, &h.id, b.message, None).ok_or_else(submit_error)?;
    if b.stream {
        let mut response = Sse::new(sse_stream(rx, true)).into_response();
        response.headers_mut().insert("x-session-id", h.id.parse().expect("UUID header"));
        return Ok(response);
    }
    let outcome = collect_chat(&h, rx, timeout_secs, None).await;
    let status = if outcome.status == TaskStatus::Timeout { StatusCode::GATEWAY_TIMEOUT } else { StatusCode::OK };
    Ok((status, Json(outcome.body)).into_response())
}

struct ChatOutcome {
    status: TaskStatus,
    body: serde_json::Value,
}

/// Shared sync/async event collection, including partial text and the abort grace period.
async fn collect_chat(h: &SessionHandle, mut rx: tokio::sync::broadcast::Receiver<AgentEvent>, timeout_secs: u64, progress: Option<(&str, &WebhookQueue)>) -> ChatOutcome {
    let mut new_messages: Vec<Message> = vec![];
    let mut segment_start = 0;
    let mut partial_text = String::new();
    let mut aborted = false;
    let mut timed_out = false;
    let mut deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let event = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                if timed_out {
                    break;
                }
                timed_out = true;
                tracing::warn!(session_id = %h.id, "chat timed out");
                h.agent.abort();
                deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                continue;
            }
            event = rx.recv() => event,
        };
        match event {
            Ok(AgentEvent::AgentStart) => segment_start = new_messages.len(),
            Ok(AgentEvent::MessageUpdate { assistant_message_event: agent_ai::AssistantEvent::TextDelta { delta, .. } }) => partial_text.push_str(&delta),
            Ok(AgentEvent::MessageEnd { message }) => {
                if let (Some((id, queue)), Message::Assistant(a)) = (progress, &message) {
                    let _ = queue.send(("task.progress", json!({"taskId": id, "text": a.text()})));
                }
                new_messages.push(message);
            }
            Ok(AgentEvent::ToolExecutionEnd { tool_name, tool_call_id, is_error, .. }) => {
                if let Some((id, queue)) = progress {
                    let _ = queue.send(("task.progress", json!({"taskId": id, "toolName": tool_name, "toolCallId": tool_call_id, "isError": is_error})));
                }
            }
            Ok(AgentEvent::AgentEnd { messages }) => {
                // AgentEnd 是权威快照，也补齐广播丢失的消息；不重复 MessageEnd。
                new_messages.truncate(segment_start);
                new_messages.extend(messages);
            }
            Ok(AgentEvent::AgentSettled { aborted: a }) => {
                aborted = a;
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
    if timed_out {
        if partial_text.is_empty() {
            partial_text = new_messages.iter().filter_map(|m| if let Message::Assistant(a) = m { Some(a.text()) } else { None }).collect::<Vec<_>>().join("");
        }
        return ChatOutcome { status: TaskStatus::Timeout, body: json!({"sessionId": h.id, "error": "timeout", "text": partial_text, "messages": new_messages}) };
    }
    let last = new_messages.iter().rev().find_map(|m| if let Message::Assistant(a) = m { Some(a.clone()) } else { None });
    let status = if aborted || last.as_ref().is_some_and(|a| a.stop_reason == agent_ai::StopReason::Aborted) {
        TaskStatus::Aborted
    } else if last.as_ref().is_none_or(|a| a.stop_reason == agent_ai::StopReason::Error || a.error_message.is_some()) {
        TaskStatus::Failed
    } else {
        TaskStatus::Succeeded
    };
    ChatOutcome {
        status,
        body: json!({
            "sessionId": h.id,
            "text": last.as_ref().map(|a| a.text()).unwrap_or_default(),
            "stopReason": last.as_ref().map(|a| a.stop_reason),
            "error": last.as_ref().and_then(|a| a.error_message.clone()),
            "aborted": aborted,
            "usage": last.as_ref().map(|a| &a.usage),
            "messages": new_messages,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use serde_json::Value;
    use tower::ServiceExt;

    fn app(cap: usize) -> (Router, Arc<Hub>) {
        let mut cfg = Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
        cfg.max_sessions_per_client = cap;
        cfg.profiles.get_mut("default").unwrap().load_context_files = false;
        cfg.clients.push(Client { name: "other".into(), token_sha256: crate::config::sha256_hex("other-token"), profiles: vec!["default".into()], ..Default::default() });
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        (router(AppState { hub: hub.clone() }), hub)
    }

    async fn request(app: &Router, method: &str, path: &str, token: Option<&str>, body: Value) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json");
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let response = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn start_async(app: &Router, body: Value) -> Value {
        let req = Request::builder().method("POST").uri("/v1/chat").header("content-type", "application/json").header("authorization", "Bearer test-token").body(Body::from(body.to_string())).unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let location = response.headers()["location"].to_str().unwrap().to_string();
        let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap();
        assert_eq!(body["status"], "running");
        assert_eq!(body["statusUrl"], location);
        assert_eq!(location, format!("/v1/tasks/{}", body["taskId"].as_str().unwrap()));
        body
    }

    async fn wait_task(app: &Router, started: &Value, delivered: bool) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                let (status, task) = request(app, "GET", started["statusUrl"].as_str().unwrap(), Some("test-token"), json!({})).await;
                assert_eq!(status, StatusCode::OK);
                if task["status"] != "running" && (!delivered || task["delivery"]["event"] == "task.completed") {
                    return task;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("task did not finish")
    }

    #[tokio::test]
    async fn async_poll_only_owner_ttl_and_shutdown() {
        let (app, hub) = app(64);
        let started = start_async(&app, json!({"message": "/tool bash {\"command\":\"sleep 0.2; printf done\"}", "async": true})).await;
        let path = started["statusUrl"].as_str().unwrap();
        let (status, task) = request(&app, "GET", path, Some("test-token"), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(task["status"], "running");
        assert!(task["finishedAt"].is_null());
        assert_eq!(request(&app, "GET", path, Some("other-token"), json!({})).await.0, StatusCode::NOT_FOUND);
        hub.evict_idle(std::time::Instant::now() + std::time::Duration::from_secs(4000));
        assert_eq!(request(&app, "GET", path, Some("test-token"), json!({})).await.0, StatusCode::OK);
        let task = wait_task(&app, &started, false).await;
        assert_eq!(task["status"], "succeeded");
        assert_eq!(task["sessionId"], started["sessionId"]);
        assert!(task["result"]["text"].as_str().unwrap().contains("done"));
        assert!(task["result"]["messages"].is_array());
        assert!(task["finishedAt"].as_u64().unwrap() >= task["createdAt"].as_u64().unwrap());
        assert!(task["delivery"].is_null());
        hub.evict_idle(std::time::Instant::now() + std::time::Duration::from_secs(3601));
        assert_eq!(request(&app, "GET", path, Some("test-token"), json!({})).await.0, StatusCode::NOT_FOUND);
        let started = start_async(&app, json!({"message": "/tool bash {\"command\":\"sleep 10\"}", "async": true})).await;
        tokio::time::timeout(std::time::Duration::from_secs(3), hub.abort_all()).await.unwrap();
        assert_eq!(wait_task(&app, &started, false).await["status"], "aborted");
        assert_eq!(request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message": "late", "async": true})).await.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn async_rejects_disallowed_callback_before_creating_session() {
        let (app, hub) = app(1);
        for url in ["http://evil.example/hook", "http://localhost:8080/hook"] {
            let (status, body) = request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message":"hello", "async":true, "callbackUrl":url})).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["error"], "callback host not allowed");
        }
        for body in [json!({"message":"hello", "async":true, "callbackUrl":"file:///tmp/hook"}), json!({"message":"hello", "async":true, "stream":true})] {
            assert_eq!(request(&app, "POST", "/v1/chat", Some("test-token"), body).await.0, StatusCode::BAD_REQUEST);
        }
        assert!(hub.create_session(&hub.config.clients[0], SessionRequest::default()).is_ok());
    }

    #[test]
    fn callback_host_matching() {
        let mut c = Client { webhook_allowed_hosts: vec!["example.com:443".into(), "localhost".into()], ..Default::default() };
        assert!(webhook_target(&c, Some("https://example.com/path")).is_ok());
        assert!(webhook_target(&c, Some("http://example.com/path")).is_err());
        assert!(webhook_target(&c, Some("http://example.com.evil:443/path")).is_err());
        assert!(webhook_target(&c, Some("http://localhost:1234/path")).is_ok());
        c.webhook_url = Some("https://default.example/hook".into());
        assert!(webhook_target(&c, Some("https://default.example/override")).is_ok());
        assert!(webhook_target(&c, None).unwrap().is_some());
    }

    #[tokio::test]
    async fn async_webhook_final_progress_and_timeout() {
        use axum::http::HeaderMap;
        for (all, timeout, default_target) in [(false, false, false), (true, false, false), (false, true, true)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let url = format!("http://{address}/hook");
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let receiver = Router::new().route(
                "/hook",
                post(move |headers: HeaderMap, body: axum::body::Bytes| {
                    let tx = tx.clone();
                    async move {
                        tx.send((headers, body)).unwrap();
                        StatusCode::NO_CONTENT
                    }
                }),
            );
            let mut servers = tokio::task::JoinSet::new();
            servers.spawn(async move { axum::serve(listener, receiver).await.unwrap() });
            let mut cfg = Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
            cfg.profiles.get_mut("default").unwrap().load_context_files = false;
            cfg.clients[0].webhook_allowed_hosts = vec![address.to_string()];
            cfg.clients[0].webhook_secret = Some("integration-test-key".into());
            cfg.chat_timeout_secs = 1;
            if default_target {
                cfg.clients[0].webhook_url = Some(url.clone());
            }
            let hub = Hub::new(cfg, "faux", None, None).unwrap();
            let app = router(AppState { hub });
            let message = if timeout { "/tool bash {\"command\":\"sleep 10\"}" } else { "/tool bash {\"command\":\"printf webhook-result\"}" };
            let mut body = json!({"message":message, "async":true, "webhookEvents": if all { "all" } else { "final" }});
            if !default_target {
                body["callbackUrl"] = json!(url);
            }
            let started = start_async(&app, body).await;
            let task = wait_task(&app, &started, true).await;
            assert_eq!(task["status"], if timeout { "timeout" } else { "succeeded" });
            assert_eq!(task["delivery"]["state"], "delivered");
            assert!(task["result"]["text"].is_string());
            if timeout {
                assert_eq!(task["result"]["error"], "timeout");
            }
            let mut received = vec![];
            while let Ok((headers, bytes)) = rx.try_recv() {
                let event: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(headers["x-subbot-event"], event["event"].as_str().unwrap());
                assert_eq!(headers["x-subbot-delivery"], event["deliveryId"].as_str().unwrap());
                assert_eq!(headers["content-type"], "application/json");
                assert_eq!(headers["user-agent"], concat!("sub-bot/", env!("CARGO_PKG_VERSION")));
                let timestamp = event["timestamp"].as_i64().unwrap();
                assert_eq!(headers["x-subbot-timestamp"], timestamp.to_string());
                assert!(crate::webhook::verify(b"integration-test-key", headers["x-subbot-signature"].to_str().unwrap(), &bytes, timestamp, 300));
                assert_eq!(event["data"]["taskId"], started["taskId"]);
                received.push(event);
            }
            let last = received.last().unwrap();
            assert_eq!(last["event"], "task.completed");
            assert_eq!(last["data"]["result"], task["result"]);
            assert_eq!(last["data"]["status"], task["status"]);
            if all {
                assert_eq!(received.len(), 4); // assistant tool call, tool end, assistant answer, final
                assert!(received[..3].iter().all(|e| e["event"] == "task.progress"));
                assert!(received[0]["data"]["text"].is_string());
                assert_eq!(received[1]["data"]["toolName"], "bash");
                assert!(received[1]["data"]["toolCallId"].is_string());
                assert_eq!(received[1]["data"]["isError"], false);
                assert!(received[2]["data"]["text"].as_str().unwrap().contains("webhook-result"));
            } else {
                assert_eq!(received.len(), 1);
            }
        }
    }

    /// Real loopback HTTP receiver; no tracing middleware or request/header logging.
    async fn callback_chat(status: StatusCode, reply: String, slow: bool, token: Option<&str>) -> Value {
        use axum::http::{header, HeaderMap, Method};
        use std::time::Duration;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/callback", listener.local_addr().unwrap());
        let (tx, mut requests) = tokio::sync::mpsc::channel(8);
        let receiver = Router::new().route(
            "/callback",
            post(move |method: Method, headers: HeaderMap, Json(body): Json<Value>| {
                let tx = tx.clone();
                let reply = reply.clone();
                async move {
                    tx.send((method, headers, body)).await.unwrap();
                    if slow {
                        std::future::pending::<()>().await;
                    }
                    (status, [(header::CONTENT_TYPE, "application/json")], reply)
                }
            }),
        );
        // Drop aborts the listener even if an assertion fails.
        let mut servers = tokio::task::JoinSet::new();
        servers.spawn(async move { axum::serve(listener, receiver).await.unwrap() });

        let mut cfg = Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
        cfg.profiles.get_mut("default").unwrap().load_context_files = false;
        let mut hub = Hub::new(cfg, "faux", None, None).unwrap();
        if slow {
            Arc::get_mut(&mut hub).unwrap().callback_timeout = Some(Duration::from_secs(1));
        }
        let app = router(AppState { hub });
        let args = json!({"query": "hello 回调", "limit": 3, "options": {"enabled": true}, "ids": [1, 2]});
        let mut spec = json!({
            "name": "caller_lookup", "description": "Look up caller data",
            "parameters": {"type": "object", "properties": {
                "query": {"type": "string"}, "limit": {"type": "integer"},
                "options": {"type": "object"}, "ids": {"type": "array", "items": {"type": "integer"}}
            }, "required": ["query"]},
            "callbackUrl": url,
        });
        if let Some(token) = token {
            spec["callbackToken"] = json!(token);
        }
        let (chat_status, chat) = tokio::time::timeout(
            Duration::from_secs(5),
            request(
                &app,
                "POST",
                "/v1/chat",
                Some("test-token"),
                json!({
                    "message": format!("/tool caller_lookup {args}"),
                    "session": {"tools": [], "callerTools": [spec]},
                }),
            ),
        )
        .await
        .expect("callback chat hung");
        assert_eq!(chat_status, StatusCode::OK);
        assert_eq!(chat["aborted"], false);
        let (method, headers, body) = requests.try_recv().expect("callback request missing");
        assert_eq!(method, Method::POST);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        // Boolean assertions avoid printing credentials on failure.
        match token {
            Some(token) => assert!(headers.get(header::AUTHORIZATION).is_some_and(|v| v == format!("Bearer {token}").as_str())),
            None => assert!(!headers.contains_key(header::AUTHORIZATION)),
        }
        let session_id = chat["sessionId"].as_str().unwrap();
        assert!(!session_id.is_empty());
        let call_id = body["toolCallId"].as_str().unwrap();
        assert!(!call_id.is_empty());
        assert_eq!(body, json!({"sessionId": session_id, "toolCallId": call_id, "toolName": "caller_lookup", "arguments": args}));
        assert!(requests.try_recv().is_err(), "unexpected duplicate callback");

        let messages = chat["messages"].as_array().unwrap();
        let results: Vec<_> = messages.iter().filter(|m| m["role"] == "toolResult").collect();
        assert_eq!(results.len(), 1);
        let result = results[0];
        assert_eq!(result["toolName"], "caller_lookup");
        assert_eq!(result["toolCallId"], call_id);
        let call = &messages.iter().find(|m| m["role"] == "assistant").unwrap()["content"][0];
        assert_eq!(call["id"], call_id);
        assert_eq!(call["name"], "caller_lookup");
        assert_eq!(call["arguments"], args);
        result.clone()
    }

    #[tokio::test]
    async fn caller_callback_round_trip() {
        let content = "caller reply 回调成功";
        let result = callback_chat(StatusCode::OK, json!({"content": content, "isError": false}).to_string(), false, Some("callback-test-token")).await;
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"], json!([{"type": "text", "text": content}]));
    }

    #[tokio::test]
    async fn caller_callback_without_token() {
        let result = callback_chat(StatusCode::OK, json!({"content": "anonymous reply", "isError": false}).to_string(), false, None).await;
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"], json!([{"type": "text", "text": "anonymous reply"}]));
    }

    #[tokio::test]
    async fn caller_callback_error_reply() {
        let result = callback_chat(StatusCode::OK, json!({"content": "lookup denied", "isError": true}).to_string(), false, Some("callback-test-token")).await;
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"], json!([{"type": "text", "text": "lookup denied"}]));
    }

    #[tokio::test]
    async fn caller_callback_http_error_includes_bounded_body() {
        for body in ["receiver failed".to_string(), format!("receiver failed: {}DO_NOT_INCLUDE_TAIL", "x".repeat(2048))] {
            let result = callback_chat(StatusCode::INTERNAL_SERVER_ERROR, body.clone(), false, Some("callback-test-token")).await;
            assert_eq!(result["isError"], true);
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.contains("500"));
            assert!(text.contains(&body[..body.len().min(1024)]));
            if body.len() > 1024 {
                assert!(text.ends_with(" [truncated]"));
                assert!(!text.contains("DO_NOT_INCLUDE_TAIL"));
                assert_eq!(text.split_once(": ").unwrap().1.len(), 1024 + " [truncated]".len());
            } else {
                assert!(text.ends_with(&body));
            }
        }
    }

    #[tokio::test]
    async fn caller_callback_oversized_response() {
        let reply = json!({"content": "x".repeat(1024 * 1024), "isError": false}).to_string();
        let result = callback_chat(StatusCode::OK, reply, false, Some("callback-test-token")).await;
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"].as_str().unwrap().contains("exceeds"));
    }

    #[tokio::test]
    async fn caller_callback_timeout() {
        let result = callback_chat(StatusCode::OK, String::new(), true, Some("callback-test-token")).await;
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"].as_str().unwrap().contains("callback timed out"));
    }

    #[tokio::test]
    async fn create_delete_get_and_owner_visibility() {
        let (app, _) = app(64);
        let (status, body) = request(&app, "POST", "/v1/sessions", Some("test-token"), json!({})).await;
        assert_eq!(status, StatusCode::CREATED);
        let path = format!("/v1/sessions/{}", body["sessionId"].as_str().unwrap());
        assert_eq!(request(&app, "GET", &path, Some("test-token"), json!({})).await.0, StatusCode::OK);
        for method in ["GET", "DELETE"] {
            assert_eq!(request(&app, method, &path, Some("other-token"), json!({})).await.0, StatusCode::NOT_FOUND);
        }
        let (status, body) = request(&app, "DELETE", &path, Some("test-token"), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"deleted": true}));
        for method in ["GET", "DELETE"] {
            assert_eq!(request(&app, method, &path, Some("test-token"), json!({})).await.0, StatusCode::NOT_FOUND);
        }
        assert_eq!(request(&app, "GET", &format!("{path}/messages"), Some("test-token"), json!({})).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cap_maps_to_429_for_create_and_chat_other_errors_stay_400() {
        let (app, _) = app(1);
        assert_eq!(request(&app, "POST", "/v1/sessions", Some("test-token"), json!({"profile":"missing"})).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(request(&app, "POST", "/v1/sessions", Some("test-token"), json!({})).await.0, StatusCode::CREATED);
        for (path, body) in [("/v1/sessions", json!({})), ("/v1/chat", json!({"message":"hello"}))] {
            let (status, body) = request(&app, "POST", path, Some("test-token"), body).await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
            assert!(body["error"].as_str().unwrap().starts_with("too many sessions"));
        }
        assert_eq!(request(&app, "POST", "/v1/sessions", Some("other-token"), json!({})).await.0, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn missing_auth_is_401() {
        let (app, _) = app(64);
        for (method, path) in [("POST", "/v1/sessions"), ("GET", "/v1/sessions/unknown"), ("DELETE", "/v1/sessions/unknown"), ("POST", "/v1/chat")] {
            assert_eq!(request(&app, method, path, None, json!({})).await.0, StatusCode::UNAUTHORIZED);
        }
        assert_eq!(request(&app, "GET", "/v1/health", None, json!({})).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn chat_can_continue_until_its_short_ttl_expires() {
        let (app, hub) = app(1);
        let (status, body) = request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message":"hello"})).await;
        assert_eq!(status, StatusCode::OK);
        let id = body["sessionId"].as_str().unwrap();
        let (status, body) = request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message":"again", "sessionId":id})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sessionId"], id);
        assert_eq!(hub.evict_idle(std::time::Instant::now() + std::time::Duration::from_secs(601)), 1);
        assert_eq!(request(&app, "GET", &format!("/v1/sessions/{id}"), Some("test-token"), json!({})).await.0, StatusCode::NOT_FOUND);
        assert_eq!(request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message":"new"})).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn streaming_chat_returns_session_header_and_uses_short_ttl() {
        let (app, hub) = app(1);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .body(Body::from(json!({"message":"hello", "stream":true}).to_string()))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let id = response.headers()["x-session-id"].to_str().unwrap().to_string();
        let bytes = tokio::time::timeout(std::time::Duration::from_secs(3), to_bytes(response.into_body(), 1024 * 1024)).await.unwrap().unwrap();
        assert!(std::str::from_utf8(&bytes).unwrap().contains("event: agent_settled"));
        assert!(hub.get(&hub.config.clients[0], &id).is_some());
        assert_eq!(hub.evict_idle(std::time::Instant::now() + std::time::Duration::from_secs(601)), 1);
        assert!(hub.get(&hub.config.clients[0], &id).is_none());
    }

    #[tokio::test]
    async fn chat_timeout_aborts_faux_tool_run() {
        // Faux 可以请求一个阻塞的 bash 工具，无需真实 provider 或外部网络。
        for timeout_secs in [None, Some(1)] {
            let mut config = Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
            config.chat_timeout_secs = if timeout_secs.is_none() { 1 } else { 900 };
            let client = Arc::new(config.clients[0].clone());
            let hub = Hub::new(config, "faux", None, None).unwrap();
            let started = std::time::Instant::now();
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                chat(
                    State(AppState { hub: hub.clone() }),
                    Extension(client.clone()),
                    Json(ChatBody {
                        message: r#"/tool bash {"command":"sleep 10"}"#.into(),
                        session_id: None,
                        session: SessionRequest::default(),
                        stream: false,
                        timeout_secs,
                        asynchronous: false,
                        callback_url: None,
                        webhook_events: WebhookEvents::Final,
                    }),
                ),
            )
            .await
            .expect("chat hung")
            .unwrap();
            assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
            assert!(started.elapsed() < std::time::Duration::from_secs(3));
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"], "timeout");
            assert!(body["text"].is_string());
            assert!(body["messages"].as_array().unwrap().len() >= 3);
            let h = hub.get(&client, body["sessionId"].as_str().unwrap()).unwrap();
            assert!(!h.agent.is_running());
            assert_eq!(hub.evict_idle(std::time::Instant::now() + std::time::Duration::from_secs(601)), 1);
            assert!(hub.get(&client, &h.id).is_none());
        }
    }
}
