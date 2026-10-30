use crate::{
    config::{Client, Config},
    hub::{Hub, SessionRequest, TooManySessions},
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
    /// 非流式等待上限；不填使用全局 chatTimeoutSecs。
    timeout_secs: Option<u64>,
}

/// 一步到位接口：给只想“发一句话拿回答”的 bot 用。
async fn chat(State(st): State<AppState>, Extension(c): Caller, Json(b): Json<ChatBody>) -> ApiResult {
    let h = match b.session_id {
        Some(id) => st.hub.get(&c, &id).ok_or_else(not_found)?,
        None => st.hub.create_chat_session(&c, b.session).map_err(create_error)?,
    };
    let rx = h.subscribe(); // 先订阅再提交，避免漏事件
    st.hub.submit(&c, &h.id, b.message, None).ok_or_else(not_found)?;
    if b.stream {
        let mut response = Sse::new(sse_stream(rx, true)).into_response();
        response.headers_mut().insert("x-session-id", h.id.parse().expect("UUID header"));
        return Ok(response);
    }
    let mut rx = rx;
    let mut new_messages: Vec<Message> = vec![];
    let mut segment_start = 0;
    let mut partial_text = String::new();
    let mut aborted = false;
    let mut timed_out = false;
    let mut deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(b.timeout_secs.unwrap_or(st.hub.config.chat_timeout_secs));
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
            Ok(AgentEvent::MessageEnd { message }) => new_messages.push(message),
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
        return Ok((StatusCode::GATEWAY_TIMEOUT, Json(json!({"sessionId": h.id, "error": "timeout", "text": partial_text, "messages": new_messages}))).into_response());
    }
    let last = new_messages.iter().rev().find_map(|m| if let Message::Assistant(a) = m { Some(a.clone()) } else { None });
    Ok(Json(json!({
        "sessionId": h.id,
        "text": last.as_ref().map(|a| a.text()).unwrap_or_default(),
        "stopReason": last.as_ref().map(|a| a.stop_reason),
        "error": last.as_ref().and_then(|a| a.error_message.clone()),
        "aborted": aborted,
        "messages": new_messages,
    }))
    .into_response())
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
        cfg.clients.push(Client { name: "other".into(), token_sha256: crate::config::sha256_hex("other-token"), profiles: vec!["default".into()] });
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
                    Json(ChatBody { message: r#"/tool bash {"command":"sleep 10"}"#.into(), session_id: None, session: SessionRequest::default(), stream: false, timeout_secs }),
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
