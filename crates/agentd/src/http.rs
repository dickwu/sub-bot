use crate::{
    config::{Client, Config},
    hub::{Hub, SessionRequest},
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

pub async fn serve(hub: Arc<Hub>, bind: SocketAddr) -> anyhow::Result<()> {
    let st = AppState { hub };
    let api = Router::new()
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/{id}/prompt", post(prompt))
        .route("/v1/sessions/{id}/events", get(events))
        .route("/v1/sessions/{id}/abort", post(abort))
        .route("/v1/sessions/{id}/messages", get(messages))
        .route("/v1/chat", post(chat))
        .route("/v1/profiles", get(profiles))
        .route("/v1/models", get(models))
        .layer(middleware::from_fn_with_state(st.clone(), auth));
    let app = Router::new().route("/v1/health", get(|| async { Json(json!({"ok": true})) })).merge(api).layer(middleware::from_fn(trace_request)).with_state(st);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
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
    let h = st.hub.create_session(&c, b).map_err(bad_request)?;
    Ok((StatusCode::CREATED, Json(json!({"sessionId": h.id, "profile": h.profile, "cwd": h.cwd, "model": h.agent.config.model, "reasoningEffort": h.agent.config.reasoning, "requestedReasoningEffort": h.requested_effort, "api": st.hub.effective_api(&h.agent.config), "tools": h.agent.config.tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>()}))).into_response())
}

#[derive(Deserialize)]
struct PromptBody {
    message: String,
    /// "steer" | "follow_up"：运行中时如何排队
    mode: Option<String>,
}

async fn prompt(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>, Json(b): Json<PromptBody>) -> ApiResult {
    let h = st.hub.get(&c, &id).ok_or_else(not_found)?;
    let d = h.submit(b.message, b.mode.as_deref());
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
}

/// 一步到位接口：给只想“发一句话拿回答”的 bot 用。
async fn chat(State(st): State<AppState>, Extension(c): Caller, Json(b): Json<ChatBody>) -> ApiResult {
    let h = match b.session_id {
        Some(id) => st.hub.get(&c, &id).ok_or_else(not_found)?,
        None => st.hub.create_session(&c, b.session).map_err(bad_request)?,
    };
    let rx = h.subscribe(); // 先订阅再提交，避免漏事件
    h.submit(b.message, None);
    if b.stream {
        return Ok(Sse::new(sse_stream(rx, true)).into_response());
    }
    let mut rx = rx;
    let mut new_messages: Vec<Message> = vec![];
    let mut aborted = false;
    loop {
        match rx.recv().await {
            Ok(AgentEvent::AgentEnd { messages }) => new_messages.extend(messages),
            Ok(AgentEvent::AgentSettled { aborted: a }) => {
                aborted = a;
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => break,
        }
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
