use crate::{
    config::{Client, Config, ResolvedTarget, TargetSpec, MAX_TARGETS},
    hub::{AccountNotAllowed, DepthLocked, Hub, SessionHandle, SessionRequest, TaskStatus, TooManySessions, WebhookQueue},
    issues::Severity,
    routing::Depth,
    sessions::{CompactError, ResumeFailed},
    webhook::WebhookTarget,
};
use agent_ai::{Message, ThinkingLevel};
use agent_core::AgentEvent;
use axum::{
    extract::{Extension, FromRequest, Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::{FutureExt, Stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::{convert::Infallible, net::SocketAddr, sync::Arc};

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
}

impl axum::extract::FromRef<AppState> for Arc<Hub> {
    fn from_ref(st: &AppState) -> Self {
        st.hub.clone()
    }
}

pub fn random_token() -> String {
    use rand::Rng;
    let bytes: [u8; 24] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Bearer token -> Client（每个 bot 一个 token），写入请求扩展供 handler 使用。
async fn auth(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let span = tracing::Span::current();
    let client = if req.headers().contains_key("authorization") {
        req.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).and_then(|t| st.hub.config.client_for_token(t))
    } else {
        req.extensions().get::<axum::extract::ConnectInfo<crate::local::Peer>>().and_then(|peer| peer.0.client(&st.hub.config))
    }
    .cloned();
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
#[derive(Clone)]
struct RequestId(String);

/// 每个 HTTP 请求一个 span：request_id（沿用 X-Request-Id 或新生成）、method、path、bot；结束时记录状态码和耗时。
async fn trace_request(mut req: Request, next: Next) -> Response {
    use tracing::Instrument;
    let request_id = req.headers().get("x-request-id").and_then(|v| v.to_str().ok()).filter(|v| v.len() <= 64).map(str::to_string).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    req.extensions_mut().insert(RequestId(request_id.clone()));
    let span = tracing::info_span!("http", request_id = %request_id, method = %req.method(), path = %req.uri().path(), bot = tracing::field::Empty);
    let start = std::time::Instant::now();
    async move {
        let mut resp = next.run(req).await;
        let status = resp.status().as_u16();
        let ms = start.elapsed().as_millis() as u64;
        // 访问日志不是具体问题：4xx 是调用方错误（响应里已有原因；401 等需要关注的情况由具体日志上报），永不成为 issue；
        // 5xx 只在本请求里还没有更具体的 issue（如 "session rehydration failed"、typed 的超时）时作为兜底上报。
        // 否则同一请求的两条 WARN/ERROR 共享去重键，会被合并成同一 issue 的 count 2（见 logging.rs `LogIssue`）。
        if status >= 500 {
            tracing::error!(issue = "fallback", status, latency_ms = ms, "request finished");
        } else if status >= 400 {
            tracing::warn!(issue = "none", status, latency_ms = ms, "request finished");
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

#[cfg(test)]
pub(crate) fn test_router(hub: Arc<Hub>) -> Router {
    router(AppState { hub })
}

fn router(st: AppState) -> Router {
    let api = Router::new()
        .route("/v1/sessions", post(create_session).get(list_sessions))
        .route("/v1/sessions/{id}", get(get_session).patch(patch_session).delete(delete_session))
        .route("/v1/sessions/{id}/prompt", post(prompt))
        .route("/v1/sessions/{id}/events", get(events))
        .route("/v1/sessions/{id}/abort", post(abort))
        .route("/v1/sessions/{id}/messages", get(messages))
        .route("/v1/sessions/{id}/approvals", get(crate::approvals::list))
        .route("/v1/sessions/{id}/approvals/{call_id}", post(crate::approvals::decide))
        .route("/v1/sessions/{id}/compact", post(compact_session))
        .route("/v1/issues", get(issues))
        .route("/v1/tasks/{id}", get(get_task))
        .route("/v1/chat", post(chat))
        .route("/v1/profiles", get(profiles))
        .route("/v1/mcp", get(crate::mcp::http_status))
        .route("/v1/models", get(models))
        .layer(middleware::from_fn_with_state(st.clone(), auth));
    Router::new().route("/v1/health", get(|| async { Json(json!({"ok": true})) })).merge(api).layer(middleware::from_fn(trace_request)).with_state(st)
}

pub fn app(hub: Arc<Hub>) -> Router {
    router(AppState { hub })
}

pub async fn serve(hub: Arc<Hub>, bind: Option<SocketAddr>) -> anyhow::Result<()> {
    crate::server::serve(hub, bind).await
}

type ApiResult = Result<Response, (StatusCode, Json<serde_json::Value>)>;
fn not_found() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::NOT_FOUND, Json(json!({"error": "session not found"})))
}

fn bad_request(e: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()})))
}

/// JSON 请求体；任何解析错误（含非法 reasoningEffort / depth 名称）都返回 400 而不是 axum 默认的 422。
pub(crate) struct ApiJson<T>(pub T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = (StatusCode, Json<serde_json::Value>);
    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(e) => Err(bad_request(e.body_text())),
        }
    }
}

fn depth_error(e: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    if e.is::<DepthLocked>() {
        (StatusCode::CONFLICT, Json(json!({"error": e.to_string()})))
    } else {
        bad_request(e)
    }
}

/// 粘性修改：depth（仅第一次 prompt 之前）与推理强度（从下一次模型调用生效）。
fn apply_overrides(h: &SessionHandle, effort: Option<ThinkingLevel>, depth: Option<Depth>) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if let Some(depth) = depth {
        h.set_depth(depth).map_err(depth_error)?;
    }
    if let Some(level) = effort {
        h.set_reasoning_effort(Some(level));
    }
    Ok(())
}

fn create_error(e: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    if e.is::<TooManySessions>() {
        (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": e.to_string()})))
    } else if e.is::<AccountNotAllowed>() {
        (StatusCode::FORBIDDEN, Json(json!({"error": e.to_string()})))
    } else {
        bad_request(e)
    }
}

/// 调用方自己的会话：内存里没有时从磁盘恢复（同一 client 的会话目录）。未知 / 非 owner / 非法 id ⇒ 404。
async fn session(st: &AppState, c: &Client, id: &str) -> Result<Arc<SessionHandle>, (StatusCode, Json<serde_json::Value>)> {
    match st.hub.get_or_load(c, id).await {
        Ok(Some(h)) => Ok(h),
        Ok(None) => Err(not_found()),
        Err(e) if e.is::<TooManySessions>() => Err((StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": e.to_string()})))),
        Err(e) if e.is::<ResumeFailed>() => {
            tracing::warn!(session_id = %id, error = %e, "session rehydration failed");
            Err((StatusCode::CONFLICT, Json(json!({"error": e.to_string()}))))
        }
        Err(e) => {
            tracing::error!(session_id = %id, error = %e, "session rehydration failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "session could not be loaded"}))))
        }
    }
}

#[derive(Deserialize, Default)]
struct ListQuery {
    limit: Option<usize>,
}

/// 调用方的会话（内存 + 已持久化，按 updatedAt 倒序）。
async fn list_sessions(State(st): State<AppState>, Extension(c): Caller, Query(q): Query<ListQuery>) -> ApiResult {
    let limit = q.limit.unwrap_or(crate::sessions::DEFAULT_LIST_LIMIT);
    if limit == 0 || limit > 1000 {
        return Err(bad_request("limit must be between 1 and 1000"));
    }
    Ok(Json(json!({"sessions": st.hub.list_sessions(&c, limit).await})).into_response())
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompactBody {
    /// 给摘要模型的附加说明（例如“重点保留测试失败信息”）
    instructions: Option<String>,
}

/// 立即压缩上下文；运行中 409，摘要调用失败 502。
async fn compact_session(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>, body: axum::body::Bytes) -> ApiResult {
    let b: CompactBody = if body.iter().all(u8::is_ascii_whitespace) { CompactBody::default() } else { serde_json::from_slice(&body).map_err(bad_request)? };
    let h = session(&st, &c, &id).await?;
    match h.compact(b.instructions).await {
        Ok(report) => Ok(Json(report).into_response()),
        Err(e @ CompactError::Busy) => Err((StatusCode::CONFLICT, Json(json!({"error": e.to_string()})))),
        Err(e @ CompactError::Failed(_)) => Err((StatusCode::BAD_GATEWAY, Json(json!({"error": e.to_string()})))),
    }
}

async fn get_session(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>) -> ApiResult {
    let h = session(&st, &c, &id).await?;
    Ok(Json(st.hub.session_view(&h)).into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchBody {
    reasoning_effort: Option<ThinkingLevel>,
    /// 只能在第一次 prompt 之前修改（否则 409）
    depth: Option<Depth>,
}

async fn patch_session(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>, ApiJson(b): ApiJson<PatchBody>) -> ApiResult {
    let h = session(&st, &c, &id).await?;
    apply_overrides(&h, b.reasoning_effort, b.depth)?;
    Ok(Json(st.hub.session_view(&h)).into_response())
}

#[derive(Deserialize, Default)]
struct DeleteQuery {
    /// 同时删除磁盘上的 JSONL（之后不能再恢复）
    #[serde(default)]
    purge: bool,
}

/// 默认只从内存删除（JSONL 保留，之后仍可按 id 恢复）；`?purge=true` 同时删除 JSONL。
async fn delete_session(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>, Query(q): Query<DeleteQuery>) -> ApiResult {
    match st.hub.delete_session(&c, &id, q.purge) {
        Ok(Some(outcome)) => Ok(Json(json!({"deleted": true, "inMemory": outcome.in_memory, "purged": outcome.purged})).into_response()),
        Ok(None) => Err(not_found()),
        Err(e) => {
            tracing::error!(session_id = %id, error = %e, "session purge failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "session file could not be deleted"}))))
        }
    }
}

/// 列出当前 bot 可用的 profile 及其工具（不含任何密钥）。
async fn profiles(State(st): State<AppState>, Extension(c): Caller) -> ApiResult {
    let list: Vec<_> = c
        .profiles
        .iter()
        .filter_map(|n| {
            st.hub.config.profiles.get(n).map(|p| {
                let provider = st.hub.profile_provider(p);
                let model = st.hub.profile_model(p);
                let account = st.hub.profile_account(p, provider);
                json!({"name": n, "provider": provider, "account": account, "allowedAccounts": st.hub.allowed_accounts(p),
                    "fallbackAccounts": st.hub.fallback_accounts(p, provider, account), "model": model, "allowedModels": p.allowed_models,
                    "reasoningEffort": p.reasoning_effort.or(st.hub.config.default_reasoning_effort),
                    "allowedEfforts": agent_ai::allowed_efforts(&agent_ai::effort_caps(provider, &model)),
                    "defaultEffort": agent_ai::effort_caps(provider, &model).default,
                    "routing": {"deepModel": st.hub.deep_model_spec(p, provider), "autoRoute": p.auto_route, "routerModel": p.router_model.clone().unwrap_or_else(|| model.clone()),
                        "defaultDepth": if p.auto_route { Depth::Auto } else { Depth::Standard }},
                    "api": p.api.unwrap_or(st.hub.config.default_api), "maxOutputTokens": p.max_output_tokens, "tokenBudget": p.token_budget, "tools": p.tools.iter().cloned().chain(st.hub.mcp.profile_tool_names(n)).collect::<Vec<_>>(), "mcpServers": st.hub.mcp.profile_servers(n), "toolApproval": st.hub.config.tool_approval_of(p), "workspaceRoots": p.workspace_roots, "allowCallerTools": p.allow_caller_tools, "allowSystemPromptOverride": p.allow_system_prompt_override})
            })
        })
        .collect();
    Ok(Json(json!({"client": c.name, "profiles": list, "registeredTools": st.hub.tool_names()})).into_response())
}

#[derive(Deserialize, Default)]
struct ModelsQuery {
    provider: Option<String>,
    /// 只查该账号的目录（需要 provider；必须是该 client 某个 profile 允许的账号）
    account: Option<String>,
    /// 只返回该 profile 允许的模型
    profile: Option<String>,
    /// 不过滤（包含 embedding、语音、快照等）
    #[serde(default)]
    all: bool,
}

/// 实时模型列表（来自 provider 的 /v1/models，缓存 10 分钟），附带推断的推理强度。
async fn models(State(st): State<AppState>, Extension(c): Caller, Query(q): Query<ModelsQuery>) -> ApiResult {
    match (&q.provider, &q.account) {
        (None, Some(_)) => return Err(bad_request("account filter requires provider")),
        (Some(id), Some(account)) => {
            crate::config::validate_provider(id).map_err(bad_request)?;
            // 只允许查询本 client 某个 profile 可选的账号；不区分“不存在”与“不允许”
            let allowed = c.profiles.iter().filter_map(|n| st.hub.config.profiles.get(n)).any(|p| st.hub.account_allowed(p, id, account));
            if !allowed {
                return Err((StatusCode::FORBIDDEN, Json(json!({"error": format!("account '{account}' is not allowed for this client")}))));
            }
            st.hub.provider(id, account).map_err(bad_request)?;
        }
        (Some(id), None) => {
            st.hub.provider(id, st.hub.default_account(id)).map_err(bad_request)?;
        }
        (None, None) => {}
    }
    let mut list = st.hub.list_models_filtered(q.all, q.provider.as_deref(), q.account.as_deref()).await.map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({"error": e.to_string()}))))?;
    if let Some(name) = &q.profile {
        if !c.profiles.contains(name) {
            return Err(bad_request(format!("profile '{name}' is not allowed for this client")));
        }
        let p = st.hub.config.profiles.get(name).ok_or_else(|| bad_request(format!("unknown profile '{name}'")))?;
        list.retain(|m| Config::model_allowed(p, &m.model.id));
    }
    Ok(Json(json!({"models": list})).into_response())
}

async fn create_session(State(st): State<AppState>, Extension(c): Caller, body: axum::body::Bytes) -> ApiResult {
    // 允许空请求体（全部默认值）
    let b: SessionRequest = if body.iter().all(u8::is_ascii_whitespace) { SessionRequest::default() } else { serde_json::from_slice(&body).map_err(bad_request)? };
    let h = st.hub.create_session(&c, b).map_err(create_error)?;
    Ok((StatusCode::CREATED, Json(st.hub.session_view(&h))).into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptBody {
    message: String,
    /// "steer" | "follow_up"：运行中时如何排队
    mode: Option<String>,
    /// 粘性：从这条消息起修改会话推理强度
    reasoning_effort: Option<ThinkingLevel>,
    /// 只在第一次 prompt 时有效（之后 409）
    depth: Option<Depth>,
}

async fn prompt(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>, request_id: Option<Extension<RequestId>>, ApiJson(b): ApiJson<PromptBody>) -> ApiResult {
    let h = session(&st, &c, &id).await?;
    if let Some(Extension(id_value)) = request_id {
        h.set_request_id(id_value.0);
    }
    apply_overrides(&h, b.reasoning_effort, b.depth)?;
    let d = st.hub.submit(&c, &id, b.message, b.mode.as_deref()).ok_or_else(not_found)?;
    Ok((StatusCode::ACCEPTED, Json(json!({"disposition": d}))).into_response())
}

fn sse_stream(rx: tokio::sync::broadcast::Receiver<AgentEvent>, stop_on_settled: bool, h: Arc<SessionHandle>) -> impl Stream<Item = Result<Event, Infallible>> {
    // 用 unfold 在发出 agent_settled 后立即结束流（take_while 要等到下一个事件才会结束，导致 /v1/chat 流挂起）
    futures::stream::unfold((rx, false, h), move |(mut rx, done, h)| async move {
        if done {
            return None;
        }
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let settled = stop_on_settled && matches!(ev, AgentEvent::AgentSettled { .. });
                    return Some((ev, (rx, settled, h)));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    h.report_lag(n);
                    tracing::warn!(issue = "typed", skipped = n, "SSE subscriber lagged; events dropped")
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
    let h = session(&st, &c, &id).await?;
    Ok(Sse::new(sse_stream(h.subscribe(), false, h)).keep_alive(Default::default()).into_response())
}

async fn abort(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>) -> ApiResult {
    session(&st, &c, &id).await?.agent.abort();
    Ok(Json(json!({"ok": true})).into_response())
}

async fn messages(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>) -> ApiResult {
    let h = session(&st, &c, &id).await?;
    Ok(Json(json!({"messages": h.agent.messages().await})).into_response())
}

async fn get_task(State(st): State<AppState>, Extension(c): Caller, Path(id): Path<String>) -> ApiResult {
    let task = st.hub.get_task(&c, &id).ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error": "task not found"}))))?;
    Ok(Json(task).into_response())
}

#[derive(Deserialize, Default)]
struct IssuesQuery {
    #[serde(default)]
    since: u64,
    limit: Option<usize>,
}
async fn issues(State(st): State<AppState>, Extension(c): Caller, Query(q): Query<IssuesQuery>) -> ApiResult {
    let limit = q.limit.unwrap_or(100);
    if limit > 500 {
        return Err(bad_request("limit must be <= 500"));
    }
    Ok(Json(json!({"issues": st.hub.issues.recent(Some(&c.name), q.since, limit)})).into_response())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WebhookEvents {
    One(String),
    Many(Vec<String>),
}
impl Default for WebhookEvents {
    fn default() -> Self {
        Self::One("final".into())
    }
}
impl WebhookEvents {
    fn resolve(&self) -> Result<Vec<String>, (StatusCode, Json<serde_json::Value>)> {
        let values = match self {
            Self::One(v) => std::slice::from_ref(v),
            Self::Many(v) => v.as_slice(),
        };
        let mut events = vec![];
        for v in values {
            match v.as_str() {
                "final" => events.push("task.completed".into()),
                "progress" => events.push("task.progress".into()),
                "approval_required" => events.push(crate::approvals::WEBHOOK_EVENT.into()),
                "all" => events.extend(["task.completed".into(), "task.progress".into(), crate::approvals::WEBHOOK_EVENT.into()]),
                "issues" => events.push("issues".into()),
                _ => return Err(bad_request("invalid webhookEvents")),
            }
        }
        Ok(events)
    }
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
    callback_url: Option<serde_json::Value>,
    #[serde(default)]
    webhook_events: WebhookEvents,
    /// 非流式等待上限；不填使用全局 chatTimeoutSecs。
    timeout_secs: Option<u64>,
    /// 粘性：新会话或 sessionId 指定的已有会话从这条消息起使用该推理强度
    #[serde(default)]
    reasoning_effort: Option<ThinkingLevel>,
    /// standard | deep | auto：新会话或尚未收到 prompt 的已有会话
    #[serde(default)]
    depth: Option<Depth>,
    /// 新会话的账号（同 session.account，顶层优先）；已有 sessionId 时忽略（账号是会话属性）
    #[serde(default)]
    account: Option<String>,
    /// `session.profile` 的别名（两处都给且不同 ⇒ 400）
    #[serde(default)]
    profile: Option<String>,
    /// `session.model` 的别名（两处都给且不同 ⇒ 400）
    #[serde(default)]
    model: Option<String>,
    /// 其余未知顶层字段：收集起来在 handler 里返回 400（而不是静默忽略）。
    /// 用 flatten 收集而不是 deny_unknown_fields，是为了给出可操作的提示（session.* 字段该放进 "session"）。
    #[serde(flatten)]
    unknown: serde_json::Map<String, serde_json::Value>,
}

/// `/v1/chat` 文档化的顶层字段（未知字段错误里列出）。
const CHAT_FIELDS: &[&str] = &["message", "sessionId", "session", "stream", "async", "callbackUrl", "webhookEvents", "timeoutSecs", "reasoningEffort", "depth", "account", "profile", "model"];
/// 只能放在 `session` 里的会话选项（顶层出现时提示放进 "session"；profile/model/reasoningEffort/depth/account 顶层可用）。
const SESSION_ONLY_FIELDS: &[&str] = &["provider", "cwd", "systemPrompt", "appendSystemPrompt", "api", "tools", "callerTools", "toolApproval"];

/// 错误信息里展示的字段名：去掉控制字符并截断（只回显键名，不回显任何值）。
fn field_label(key: &str) -> String {
    key.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// 未知顶层字段 ⇒ 400 文案；会话选项优先提示放进 "session"。
fn unknown_chat_field(unknown: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    if let Some(key) = unknown.keys().find(|k| SESSION_ONLY_FIELDS.contains(&k.as_str())) {
        return Some(format!(r#"unknown field '{}'; session options go under "session": {{...}}"#, field_label(key)));
    }
    unknown.keys().next().map(|key| format!("unknown field '{}'; expected one of: {}", field_label(key), CHAT_FIELDS.join(", ")))
}

/// 顶层别名并入 `session`：只给一处 ⇒ 使用它；两处相同 ⇒ 接受；两处不同 ⇒ 400。
fn merge_session_alias(name: &str, top: Option<String>, session: &mut Option<String>) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    match (top, session.as_ref()) {
        (Some(top), Some(inner)) if &top != inner => Err(bad_request(format!("{name} given both at top level and in session; put it in session.{name}"))),
        (Some(top), _) => {
            *session = Some(top);
            Ok(())
        }
        (None, _) => Ok(()),
    }
}

impl ChatBody {
    /// 拒绝未知字段并合并 profile/model 别名。
    fn normalize(&mut self) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
        if let Some(message) = unknown_chat_field(&self.unknown) {
            return Err(bad_request(message));
        }
        merge_session_alias("profile", self.profile.take(), &mut self.session.profile)?;
        merge_session_alias("model", self.model.take(), &mut self.session.model)
    }
}

/// A host entry allows any port; host:port restricts it. The configured URL implicitly
/// allows its host. No URL input is included in errors or logs.
fn webhook_targets(c: &Client, callback: Option<&serde_json::Value>, events: &WebhookEvents) -> Result<Vec<ResolvedTarget>, (StatusCode, Json<serde_json::Value>)> {
    let defaults = events.resolve()?;
    let mut targets = if let Some(value) = callback {
        let entries = match value {
            serde_json::Value::Array(v) => v.as_slice(),
            _ => std::slice::from_ref(value),
        };
        if entries.len() > MAX_TARGETS {
            return Err(bad_request("too many callback targets"));
        }
        let mut targets = vec![];
        for entry in entries {
            if entry.as_object().is_some_and(|v| v.contains_key("secretFile")) {
                return Err(bad_request("secretFile is not allowed in requests"));
            }
            // Objects are allowed only inside the array.
            if !value.is_array() && !entry.is_string() {
                return Err(bad_request("callbackUrl must be a string or array"));
            }
            let spec: TargetSpec = serde_json::from_value(entry.clone()).map_err(|_| bad_request("invalid callback target"))?;
            let (value, events, min_severity) = match spec {
                TargetSpec::Url(url) => (url, None, None),
                TargetSpec::Full { url, events, min_severity, .. } => (url, events, min_severity),
            };
            if events.as_ref().is_some_and(|es| es.iter().any(|e| !matches!(e.as_str(), "task.completed" | "task.progress" | "issues" | crate::approvals::WEBHOOK_EVENT))) {
                return Err(bad_request("invalid target events"));
            }
            let url = crate::config::webhook_url(&value).ok_or_else(|| bad_request("callbackUrl must be an http/https URL"))?;
            let host = url.host_str().unwrap_or_default();
            let authority = format!("{host}:{}", url.port_or_known_default().unwrap_or_default());
            let implicit = c.webhook_url.specs().iter().any(|spec| {
                let value = match spec {
                    TargetSpec::Url(url) | TargetSpec::Full { url, .. } => url,
                };
                crate::config::webhook_url(value).is_some_and(|default| default.host_str() == Some(host))
            });
            if !implicit && !c.webhook_allowed_hosts.iter().any(|allowed| allowed.eq_ignore_ascii_case(host) || allowed.eq_ignore_ascii_case(&authority)) {
                return Err(bad_request("callback host not allowed"));
            }
            targets.push(ResolvedTarget { target: WebhookTarget { url: url.into(), secret: c.webhook_secret.clone() }, events, min_severity });
        }
        targets
    } else {
        c.resolved_webhooks.clone()
    };
    for target in &mut targets {
        if target.events.is_none() {
            target.events = Some(defaults.clone());
        }
    }
    Ok(targets)
}

/// 一步到位接口：给只想“发一句话拿回答”的 bot 用。
async fn chat(State(st): State<AppState>, Extension(c): Caller, request_id: Option<Extension<RequestId>>, ApiJson(mut b): ApiJson<ChatBody>) -> ApiResult {
    if st.hub.is_shutting_down() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "server is shutting down"}))));
    }
    b.normalize()?;
    if b.asynchronous && b.stream {
        return Err(bad_request("async and stream cannot be combined"));
    }
    let targets = webhook_targets(&c, b.callback_url.as_ref(), &b.webhook_events)?;
    let timeout_secs = b.timeout_secs.unwrap_or(st.hub.config.chat_timeout_secs);
    let h = match b.session_id {
        Some(id) => {
            let h = session(&st, &c, &id).await?;
            apply_overrides(&h, b.reasoning_effort, b.depth)?;
            h
        }
        None => {
            b.session.reasoning_effort = b.reasoning_effort.or(b.session.reasoning_effort);
            b.session.depth = b.depth.or(b.session.depth);
            b.session.account = b.account.or(b.session.account);
            st.hub.create_chat_session(&c, b.session).map_err(create_error)?
        }
    };
    if let Some(Extension(id)) = request_id {
        h.set_request_id(id.0);
    }
    let rx = h.subscribe(); // 先订阅再提交，避免漏事件
    let approval_rx = b.asynchronous.then(|| h.subscribe());
    let submit_error = || {
        if st.hub.is_shutting_down() {
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "server is shutting down"})))
        } else {
            not_found()
        }
    };
    if b.asynchronous {
        let task = st.hub.submit_task(&c, &h.id, b.message, &targets).ok_or_else(submit_error)?;
        let status_url = format!("/v1/tasks/{}", task.task_id);
        let queues: Vec<_> = targets
            .into_iter()
            .map(|target| {
                let queue = st.hub.webhook_queue(task.task_id.clone(), target.target.clone());
                (target, queue)
            })
            .collect();
        let approval_queues: Vec<_> = queues.iter().filter(|(t, _)| t.wants(crate::approvals::WEBHOOK_EVENT)).map(|(_, q)| q.clone()).collect();
        if let (Some(approval_rx), false) = (approval_rx, approval_queues.is_empty()) {
            crate::approvals::forward_to_webhooks(task.task_id.clone(), h.clone(), approval_rx, approval_queues);
        }
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
            let progress: Vec<_> = queues.iter().filter(|(t, _)| t.wants("task.progress")).map(|(_, q)| q.clone()).collect();
            let outcome = std::panic::AssertUnwindSafe(collect_chat(&h, rx, timeout_secs, Some((&task.task_id, &progress)))).catch_unwind().await;
            let outcome = outcome.unwrap_or_else(|_| {
                h.agent.abort();
                h.report_issue(Severity::Fatal, "task_panic", "runtime", "Internal panic", json!({}));
                ChatOutcome { status: TaskStatus::Failed, body: json!({"sessionId": h.id, "error": "internal panic"}) }
            });
            if let Some(task) = st.hub.finish_task(&task.task_id, outcome.status, outcome.body) {
                for (target, queue) in queues {
                    if target.wants("task.completed") {
                        let _ = queue.send(("task.completed", json!(task)));
                    }
                }
            }
            // Allow the independent source observer to flush finishing events before removing routes.
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            st.hub.unsubscribe_task(&task.task_id, &h);
        });
        return Ok(response);
    }
    st.hub.submit(&c, &h.id, b.message, None).ok_or_else(submit_error)?;
    if b.stream {
        let mut response = Sse::new(sse_stream(rx, true, h.clone())).into_response();
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
async fn collect_chat(h: &SessionHandle, mut rx: tokio::sync::broadcast::Receiver<AgentEvent>, timeout_secs: u64, progress: Option<(&str, &[WebhookQueue])>) -> ChatOutcome {
    let mut new_messages: Vec<Message> = vec![];
    let mut segment_start = 0;
    let mut partial_text = String::new();
    let mut aborted = false;
    let mut timed_out = false;
    let mut panicked = false;
    let mut deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let event = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                if timed_out {
                    break;
                }
                timed_out = true;
                h.report_issue(Severity::Warn, "run_timeout", "http", "Chat timed out", json!({}));
                tracing::warn!(issue = "typed", session_id = %h.id, "chat timed out");
                h.agent.abort();
                deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                continue;
            }
            event = rx.recv() => event,
        };
        match event {
            Ok(AgentEvent::AgentStart) => segment_start = new_messages.len(),
            Ok(AgentEvent::MessageRetry { .. }) => partial_text.clear(),
            Ok(AgentEvent::MessageUpdate { assistant_message_event: agent_ai::AssistantEvent::TextDelta { delta, .. } }) => partial_text.push_str(&delta),
            Ok(AgentEvent::MessageEnd { message }) => {
                if let (Some((id, queue)), Message::Assistant(a)) = (progress, &message) {
                    if let Some(payload) = assistant_progress(id, a) {
                        for queue in queue {
                            let _ = queue.send(("task.progress", payload.clone()));
                        }
                    }
                }
                new_messages.push(message);
            }
            Ok(AgentEvent::ToolExecutionEnd { tool_name, tool_call_id, is_error, .. }) => {
                if let Some((id, queue)) = progress {
                    for queue in queue {
                        let _ = queue.send(("task.progress", json!({"taskId": id, "toolName": tool_name, "toolCallId": tool_call_id, "isError": is_error})));
                    }
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
            Ok(AgentEvent::RunOutcome { kind }) if kind == "task_panic" => panicked = true,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => h.report_lag(n),
            Err(_) => break,
        }
    }
    if panicked {
        return ChatOutcome { status: TaskStatus::Failed, body: json!({"sessionId": h.id, "error": "internal panic"}) };
    }
    if timed_out {
        if partial_text.is_empty() {
            partial_text = new_messages.iter().filter_map(|m| if let Message::Assistant(a) = m { Some(a.text()) } else { None }).collect::<Vec<_>>().join("");
        }
        let account = new_messages.iter().rev().find_map(|m| if let Message::Assistant(a) = m { a.account.clone() } else { None }).unwrap_or_else(|| h.account.clone());
        let mut body = json!({"sessionId": h.id, "error": "timeout", "text": partial_text, "model": h.model(), "account": account, "messages": new_messages});
        body.as_object_mut().unwrap().extend(h.effort_fields());
        return ChatOutcome { status: TaskStatus::Timeout, body };
    }
    let last = new_messages.iter().rev().find_map(|m| if let Message::Assistant(a) = m { Some(a.clone()) } else { None });
    let status = if aborted || last.as_ref().is_some_and(|a| a.stop_reason == agent_ai::StopReason::Aborted) {
        TaskStatus::Aborted
    } else if last.as_ref().is_none_or(|a| a.stop_reason == agent_ai::StopReason::Error || a.error_message.is_some()) {
        TaskStatus::Failed
    } else {
        TaskStatus::Succeeded
    };
    // 实际使用的账号（failover 后可能不同于会话账号）
    let account = last.as_ref().and_then(|a| a.account.clone()).unwrap_or_else(|| h.account.clone());
    let mut body = json!({
        "sessionId": h.id,
        "model": h.model(),
        "account": account,
        "text": last.as_ref().map(|a| a.text()).unwrap_or_default(),
        "stopReason": last.as_ref().map(|a| a.stop_reason),
        "error": last.as_ref().and_then(|a| a.error_message.clone()),
        "aborted": aborted,
        "usage": last.as_ref().map(|a| &a.usage),
        "messages": new_messages,
    });
    body.as_object_mut().unwrap().extend(h.effort_fields());
    ChatOutcome { status, body }
}

fn assistant_progress(id: &str, message: &agent_ai::AssistantMessage) -> Option<serde_json::Value> {
    let text = message.text();
    (!text.trim().is_empty()).then(|| json!({"taskId": id, "text": text}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::ScopeKey;
    use agent_ai::ThinkingLevel;
    use axum::body::{to_bytes, Body};
    use serde_json::Value;
    use tower::ServiceExt;

    #[test]
    fn empty_assistant_progress_is_skipped() {
        let mut message = agent_ai::AssistantMessage::empty("test");
        message.content.push(agent_ai::Content::ToolCall(agent_ai::ToolCall { id: "c".into(), name: "read".into(), arguments: serde_json::json!({}) }));
        assert!(super::assistant_progress("task", &message).is_none());
        message.content.push(agent_ai::Content::Text { text: " \n\t".into() });
        assert!(super::assistant_progress("task", &message).is_none());
        message.content.push(agent_ai::Content::Text { text: "answer".into() });
        assert!(super::assistant_progress("task", &message).is_some());
    }

    fn config() -> Config {
        let mut cfg = Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
        cfg.data_dir = Some(std::env::temp_dir().join(format!("sub-bot-http-{}", uuid::Uuid::new_v4())));
        cfg
    }

    fn app(cap: usize) -> (Router, Arc<Hub>) {
        let mut cfg = config();
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

    /// 生产 IssuesLayer 规则下驱动真实 router：一个 401 请求只产生一条可读的 issue（v0.10.1 会出现同一 id 的 count 1 / count 2 两行）。
    #[tokio::test]
    async fn unauthorized_request_reports_exactly_one_readable_issue() {
        use tracing_subscriber::layer::SubscriberExt;
        for token in [None, Some("unknown-token"), Some("abcDEF1234567890ghijKLMN")] {
            // Hub 在订阅之前创建，避免其启动日志混入。
            let (app, _hub) = app(1);
            let dir = std::env::temp_dir().join(format!("sub-bot-http-issues-{}", uuid::Uuid::new_v4()));
            let reporter = crate::issues::Reporter::new(&crate::config::AlertsConfig::default(), Some(dir.clone()), vec![]);
            let guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(crate::logging::IssuesTo(reporter.clone())));
            let (status, body) = request(&app, "GET", "/v1/sessions", token, json!({})).await;
            assert_eq!((status, body), (StatusCode::UNAUTHORIZED, json!({"error": "unauthorized"})));
            // 客户端错误（4xx）的访问日志本身不是 issue：有效 token 的 404 不产生任何 issue。
            assert_eq!(request(&app, "GET", "/v1/sessions/missing", Some("test-token"), json!({})).await.0, StatusCode::NOT_FOUND);
            drop(guard);
            // 先让后台 drain 按生产路径处理（flush 会关闭去重窗口，掩盖 count 2 的症状），再 flush 确认没有摘要。
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            reporter.flush(std::time::Duration::from_secs(2)).await;
            let recent = reporter.recent(None, 0, 100);
            assert_eq!(recent.len(), 1, "{recent:?}");
            assert_eq!(recent[0].count, 1);
            assert_eq!(recent[0].kind, "log_warn");
            assert!(recent[0].message.contains("unauthorized request (missing or unknown bearer token)"), "{}", recent[0].message);
            let log = std::fs::read_to_string(dir.join("issues.log")).unwrap();
            let lines: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
            assert_eq!(lines.len(), 1, "{log}");
            assert_eq!(lines[0]["id"], recent[0].id.as_str());
            assert_eq!(lines[0]["count"], 1);
            assert!(lines[0]["message"].as_str().unwrap().contains("missing or unknown bearer token"), "{log}");
            assert!(!log.contains("unknown-token") && !log.contains("abcDEF"));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn unloaded_provider_is_400_and_catalog_and_profiles_include_provider() {
        let mut hub = Hub::new(config(), "faux", None, None).unwrap();
        Arc::get_mut(&mut hub).unwrap().providers.remove(&crate::accounts::AccountKey::from("anthropic"));
        let app = test_router(hub);
        let expected = "provider anthropic is not configured (set --anthropic-key-file / ANTHROPIC_API_KEY_FILE / providers.anthropic.apiKeyFile / apiKeyKeychain / ANTHROPIC_API_KEY, or run `sub-bot keys set --provider anthropic`)";
        for (path, body) in [("/v1/sessions", json!({"provider": "anthropic"})), ("/v1/chat", json!({"message": "hello", "session": {"model": "claude-sonnet-4-5"}}))] {
            let (status, response) = request(&app, "POST", path, Some("test-token"), body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(response["error"], expected);
        }
        let (status, response) = request(&app, "GET", "/v1/models?provider=anthropic", Some("test-token"), json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["error"], expected);
        let (status, response) = request(&app, "GET", "/v1/models?provider=faux", Some("test-token"), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["models"][0]["provider"], "faux");
        let (status, response) = request(&app, "GET", "/v1/profiles", Some("test-token"), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["profiles"][0]["provider"], "faux");
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
    async fn callback_arrays_validate_before_admission() {
        let (app, _) = app(1);
        for callback in [json!([{"url":"http://localhost", "secretFile":null}]), json!(vec!["http://localhost"; 11]), json!(["http://evil.example", {"url":"http://localhost"}])] {
            let (status, _) = request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message":"hello", "async":true, "callbackUrl":callback})).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        let c = Client { webhook_allowed_hosts: vec!["localhost".into()], ..Default::default() };
        let targets = webhook_targets(
            &c,
            Some(&json!(["http://localhost/a", {"url":"http://localhost/b", "events":["issues"], "minSeverity":"error"}])),
            &WebhookEvents::Many(vec!["all".into(), "issues".into()]),
        )
        .unwrap();
        assert_eq!(targets.len(), 2);
        assert!(targets[0].wants("task.progress"));
        assert!(targets[0].wants("task.completed"));
        assert!(!targets[1].wants("task.completed"));
        assert!(!targets[1].wants_issue(Severity::Warn));
        assert!(targets[1].wants_issue(Severity::Error));
    }

    #[tokio::test]
    async fn multi_target_failure_is_independent_and_issues_are_client_scoped() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let receiver = Router::new().route(
            "/{name}",
            post(move |Path(name): Path<String>, Json(body): Json<Value>| {
                let tx = tx.clone();
                async move {
                    tx.send((name.clone(), body)).unwrap();
                    if name == "bad" {
                        StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        );
        let mut servers = tokio::task::JoinSet::new();
        servers.spawn(async move { axum::serve(listener, receiver).await.unwrap() });
        let mut cfg = config();
        cfg.webhook_max_attempts = 2;
        cfg.profiles.get_mut("default").unwrap().load_context_files = false;
        cfg.clients[0].webhook_allowed_hosts = vec![address.to_string()];
        cfg.clients.push(Client { name: "other".into(), token_sha256: crate::config::sha256_hex("other-token"), profiles: vec!["default".into()], ..Default::default() });
        let mut hub = Hub::new(cfg, "faux", None, None).unwrap();
        Arc::get_mut(&mut hub).unwrap().issues = crate::issues::Reporter::new(&hub.config.alerts, None, vec![]);
        let app = router(AppState { hub: hub.clone() });
        let other = hub.create_session(&hub.config.clients[1], SessionRequest::default()).unwrap();
        hub.issues.subscribe(
            ScopeKey::Session(other.id.clone()),
            "other".into(),
            vec![ResolvedTarget { target: WebhookTarget { url: format!("http://{address}/other"), secret: None }, events: Some(vec!["issues".into()]), min_severity: None }],
        );
        let started = start_async(
            &app,
            json!({"message":"/tool bash {\"command\":\"printf PRIVATE_OUTPUT; exit 7\"}", "async":true,
            "webhookEvents":["final", "issues"], "callbackUrl":[
                {"url":format!("http://{address}/bad"), "events":["task.completed"]},
                format!("http://{address}/good")]}),
        )
        .await;
        let mut received = vec![];
        let task = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                while let Ok(event) = rx.try_recv() {
                    received.push(event);
                }
                let (_, task) = request(&app, "GET", started["statusUrl"].as_str().unwrap(), Some("test-token"), json!({})).await;
                if task["deliveries"][0]["state"] == "failed" && task["deliveries"][1]["state"] == "delivered" {
                    break task;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        hub.issues.flush(std::time::Duration::from_secs(2)).await;
        while let Ok(event) = rx.try_recv() {
            received.push(event);
        }
        assert_eq!(task["deliveries"][0]["attempts"], 2);
        assert_eq!(task["delivery"]["state"], "failed");
        assert_eq!(task["deliveries"][1]["host"], "127.0.0.1");
        assert_eq!(received.iter().filter(|(name, e)| name == "good" && e["event"] == "task.completed").count(), 1);
        let first_good = received.iter().position(|(name, e)| name == "good" && e["event"] == "task.completed").unwrap();
        let last_bad = received.iter().rposition(|(name, _)| name == "bad").unwrap();
        assert!(first_good < last_bad, "healthy delivery must not wait for the failing receiver's retries");
        assert_eq!(received.iter().filter(|(name, _)| name == "bad").count(), 2);
        assert!(!received.iter().any(|(name, _)| name == "other"));
        assert_eq!(received.iter().filter(|(name, e)| name == "good" && e["event"] == "issue").count(), 2, "one tool issue and one delivery failure, without duplicate scope delivery");
        let issue = received.iter().find(|(name, e)| name == "good" && e["event"] == "issue" && e["data"]["kind"] == "tool_error").unwrap();
        assert_eq!(issue.1["data"]["kind"], "tool_error");
        assert_eq!(issue.1["data"]["taskId"], started["taskId"]);
        assert!(!issue.1.to_string().contains("PRIVATE_OUTPUT"));
        let failures: Vec<_> = received.iter().filter(|(name, e)| name == "good" && e["event"] == "issue" && e["data"]["kind"] == "webhook_delivery_failed").collect();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].1["data"]["taskId"], started["taskId"]);
        assert_eq!(failures[0].1["data"]["details"]["host"], "127.0.0.1");
        let (_, body) = request(&app, "GET", "/v1/issues?since=0&limit=500", Some("test-token"), json!({})).await;
        assert_eq!(body["issues"].as_array().unwrap().iter().filter(|e| e["kind"] == "webhook_delivery_failed").count(), 1, "report once after retries, not once per attempt or update");
        assert!(body["issues"].as_array().unwrap().iter().all(|e| e["client"] == "default"));
        let (_, body) = request(&app, "GET", "/v1/issues", Some("other-token"), json!({})).await;
        assert_eq!(body["issues"], json!([]));
        assert_eq!(request(&app, "GET", "/v1/issues?limit=501", Some("test-token"), json!({})).await.0, StatusCode::BAD_REQUEST);
        hub.abort_all().await;
    }

    struct PanicProvider;
    impl agent_ai::Provider for PanicProvider {
        fn id(&self) -> &str {
            "panic-test"
        }
        fn stream(&self, _: agent_ai::Request, _: tokio_util::sync::CancellationToken) -> agent_ai::EventStream {
            panic!("test provider panic")
        }
    }
    #[tokio::test]
    async fn provider_panic_fails_task_and_shutdown_completes() {
        let mut cfg = config();
        cfg.profiles.get_mut("default").unwrap().load_context_files = false;
        let mut hub = Hub::new(cfg, "faux", None, None).unwrap();
        Arc::get_mut(&mut hub).unwrap().providers.insert("faux".into(), Arc::new(PanicProvider));
        Arc::get_mut(&mut hub).unwrap().issues = crate::issues::Reporter::new(&hub.config.alerts, None, vec![]);
        let app = router(AppState { hub: hub.clone() });
        let started = start_async(&app, json!({"message":"hello", "async":true})).await;
        let task = wait_task(&app, &started, false).await;
        assert_eq!(task["status"], "failed");
        assert_eq!(task["error"], "internal panic");
        tokio::time::timeout(std::time::Duration::from_secs(2), hub.abort_all()).await.unwrap();
        hub.issues.flush(std::time::Duration::from_secs(2)).await;
        assert!(hub.issues.recent(Some("default"), 0, 100).iter().any(|e| e.kind == "task_panic" && e.severity == Severity::Fatal));
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
        let check = |c: &Client, url: &str| webhook_targets(c, Some(&json!(url)), &WebhookEvents::default());
        assert!(check(&c, "https://example.com/path").is_ok());
        assert!(check(&c, "http://example.com/path").is_err());
        assert!(check(&c, "http://example.com.evil:443/path").is_err());
        assert!(check(&c, "http://localhost:1234/path").is_ok());
        c.webhook_url = crate::config::Targets::One(TargetSpec::Url("https://default.example/hook".into()));
        assert!(check(&c, "https://default.example/override").is_ok());
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
            let mut cfg = config();
            cfg.profiles.get_mut("default").unwrap().load_context_files = false;
            cfg.clients[0].webhook_allowed_hosts = vec![address.to_string()];
            cfg.clients[0].webhook_secret = Some("integration-test-key".into());
            cfg.chat_timeout_secs = 1;
            if default_target {
                cfg.clients[0].webhook_url = crate::config::Targets::One(TargetSpec::Url(url.clone()));
                cfg.clients[0].resolved_webhooks = vec![ResolvedTarget { target: WebhookTarget { url: url.clone(), secret: cfg.clients[0].webhook_secret.clone() }, events: None, min_severity: None }];
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
                assert_eq!(received.len(), 3); // tool-only assistant is skipped; tool end, answer, final
                assert!(received[..2].iter().all(|e| e["event"] == "task.progress"));
                assert_eq!(received[0]["data"]["toolName"], "bash");
                assert!(received[0]["data"]["toolCallId"].is_string());
                assert_eq!(received[0]["data"]["isError"], false);
                assert!(received[1]["data"]["text"].as_str().unwrap().contains("webhook-result"));
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

        let mut cfg = config();
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
    async fn dynamic_effort_patch_mid_run_and_reporting() {
        let mut cfg = config();
        cfg.profiles.get_mut("default").unwrap().load_context_files = false;
        let mut hub = Hub::new(cfg, "faux", None, None).unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<agent_ai::Request>::new()));
        Arc::get_mut(&mut hub).unwrap().providers.insert("faux".into(), Arc::new(crate::hub::tests::RecordingProvider { id: "faux", seen: seen.clone(), models: vec![] }));
        let app = test_router(hub.clone());
        let (status, view) = request(&app, "POST", "/v1/sessions", Some("test-token"), json!({"reasoningEffort": "low"})).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!((view["reasoningEffort"].clone(), view["requestedReasoningEffort"].clone(), view["effortAdjusted"].clone()), (json!("low"), json!("low"), Value::Null));
        assert_eq!(view["routing"]["depth"], "standard");
        assert_eq!(view["allowedEfforts"], json!([]));
        let id = view["sessionId"].as_str().unwrap().to_string();
        let path = format!("/v1/sessions/{id}");
        for body in [json!({"reasoningEffort": "NOT VALID"}), json!({"reasoningEffort": "high", "model": "x"}), json!({"depth": "deeper"})] {
            assert_eq!(request(&app, "PATCH", &path, Some("test-token"), body).await.0, StatusCode::BAD_REQUEST);
        }
        assert_eq!(request(&app, "POST", "/v1/sessions", Some("test-token"), json!({"reasoningEffort": "Not Valid"})).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(request(&app, "PATCH", "/v1/sessions/unknown", Some("test-token"), json!({"reasoningEffort": "high"})).await.0, StatusCode::NOT_FOUND);
        let h = hub.get(&hub.config.clients[0], &id).unwrap();
        let mut rx = h.subscribe();
        let (status, _) = request(&app, "POST", &format!("{path}/prompt"), Some("test-token"), json!({"message": "/tool bash {\"command\":\"sleep 0.5\"}"})).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while seen.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        // 运行中修改：从下一次模型调用（工具结果之后）生效
        let (status, view) = request(&app, "PATCH", &path, Some("test-token"), json!({"reasoningEffort": "high"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["reasoningEffort"], "high");
        assert_eq!(view["isStreaming"], true);
        assert_eq!(request(&app, "PATCH", &path, Some("test-token"), json!({"depth": "deep"})).await.0, StatusCode::CONFLICT);
        let mut changed = vec![];
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match rx.recv().await.unwrap() {
                    AgentEvent::EffortChanged { effective, .. } => changed.push(effective),
                    AgentEvent::AgentSettled { .. } => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(changed, [Some(ThinkingLevel::High)]);
        let efforts: Vec<_> = seen.lock().unwrap().iter().map(|r| r.reasoning).collect();
        assert_eq!(efforts, [Some(ThinkingLevel::Low), Some(ThinkingLevel::High)]);
        // prompt 自带的 reasoningEffort 是粘性的；depth 在第一次 prompt 之后 409
        let (status, _) = request(&app, "POST", &format!("{path}/prompt"), Some("test-token"), json!({"message": "x", "depth": "deep"})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = request(&app, "POST", &format!("{path}/prompt"), Some("test-token"), json!({"message": "x", "reasoningEffort": "medium"})).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let (_, view) = request(&app, "GET", &path, Some("test-token"), json!({})).await;
        assert_eq!(view["requestedReasoningEffort"], "medium");
        // /v1/chat：顶层 reasoningEffort / depth，作用于新会话或已有会话
        let (status, body) = request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message": "hi", "reasoningEffort": "xhigh", "depth": "auto", "session": {"reasoningEffort": "low"}})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["reasoningEffort"], "xhigh");
        assert_eq!(body["routing"]["depth"], "auto");
        assert_eq!(body["routing"]["reason"], "no deepModel configured; using base model");
        assert_eq!(body["routing"]["decided"], true);
        assert!(body["allowedEfforts"].is_array() && body.get("effortAdjusted").is_some());
        let (status, body) = request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message": "again", "sessionId": body["sessionId"], "reasoningEffort": "minimal"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["reasoningEffort"], "minimal");
        assert_eq!(seen.lock().unwrap().last().unwrap().reasoning, Some(ThinkingLevel::Minimal));
        let (_, body) = request(&app, "GET", "/v1/profiles", Some("test-token"), json!({})).await;
        assert_eq!(body["profiles"][0]["routing"], json!({"deepModel": null, "autoRoute": false, "routerModel": "faux", "defaultDepth": "standard"}));
        hub.abort_all().await;
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
        assert_eq!(body, json!({"deleted": true, "inMemory": true, "purged": false}));
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
            let mut config = config();
            config.chat_timeout_secs = if timeout_secs.is_none() { 1 } else { 900 };
            let client = Arc::new(config.clients[0].clone());
            let hub = Hub::new(config, "faux", None, None).unwrap();
            let started = std::time::Instant::now();
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                chat(
                    State(AppState { hub: hub.clone() }),
                    Extension(client.clone()),
                    None,
                    ApiJson(ChatBody {
                        message: r#"/tool bash {"command":"sleep 10"}"#.into(),
                        session_id: None,
                        session: SessionRequest::default(),
                        stream: false,
                        timeout_secs,
                        asynchronous: false,
                        callback_url: None,
                        webhook_events: WebhookEvents::default(),
                        reasoning_effort: None,
                        depth: None,
                        account: None,
                        profile: None,
                        model: None,
                        unknown: Default::default(),
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

    fn reviewer_app() -> Router {
        let mut cfg = config();
        cfg.profiles.get_mut("default").unwrap().load_context_files = false;
        let reviewer = cfg.profiles["default"].clone();
        cfg.profiles.insert("reviewer".into(), reviewer);
        cfg.clients[0].profiles.push("reviewer".into());
        router(AppState { hub: Hub::new(cfg, "faux", None, None).unwrap() })
    }

    async fn chat_session_view(app: &Router, body: Value) -> Value {
        let (status, response) = request(app, "POST", "/v1/chat", Some("test-token"), body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body} -> {response}");
        let (status, view) = request(app, "GET", &format!("/v1/sessions/{}", response["sessionId"].as_str().unwrap()), Some("test-token"), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        view
    }

    #[tokio::test]
    async fn chat_top_level_profile_and_model_alias_session_fields() {
        let app = reviewer_app();
        // 以前顶层 profile 被静默忽略（落到 default）；现在等价于 session.profile
        assert_eq!(chat_session_view(&app, json!({"message": "hi", "profile": "reviewer"})).await["profile"], "reviewer");
        assert_eq!(chat_session_view(&app, json!({"message": "hi", "profile": "reviewer", "session": {"profile": "reviewer"}})).await["profile"], "reviewer");
        let view = chat_session_view(&app, json!({"message": "hi", "model": "faux-alt"})).await;
        assert_eq!((view["profile"].as_str(), view["model"].as_str()), (Some("default"), Some("faux-alt")));
        // 现有写法不变
        assert_eq!(chat_session_view(&app, json!({"message": "hi", "session": {"profile": "reviewer", "model": "faux-alt"}})).await["model"], "faux-alt");
        assert_eq!(chat_session_view(&app, json!({"message": "hi"})).await["profile"], "default");
        for (body, error) in [
            (json!({"message": "hi", "profile": "reviewer", "session": {"profile": "default"}}), "profile given both at top level and in session; put it in session.profile"),
            (json!({"message": "hi", "model": "faux-a", "session": {"model": "faux-b"}}), "model given both at top level and in session; put it in session.model"),
        ] {
            assert_eq!(request(&app, "POST", "/v1/chat", Some("test-token"), body).await, (StatusCode::BAD_REQUEST, json!({"error": error})));
        }
    }

    #[tokio::test]
    async fn chat_and_session_reject_unknown_fields() {
        let app = reviewer_app();
        for field in ["cwd", "systemPrompt", "provider", "tools", "callerTools", "api", "appendSystemPrompt"] {
            let (status, body) = request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message": "hi", field: "x"})).await;
            assert_eq!((status, body), (StatusCode::BAD_REQUEST, json!({"error": format!(r#"unknown field '{field}'; session options go under "session": {{...}}"#)})));
        }
        let (status, body) = request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message": "hi", "apiKey": "sk-secret-value"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error = body["error"].as_str().unwrap();
        assert!(error.starts_with("unknown field 'apiKey'; expected one of: message, sessionId, session,"), "{error}");
        assert!(!error.contains("sk-secret-value"), "{error}");
        // session 内部的未知字段同样 400（不再静默忽略拼写错误）
        let (status, body) = request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message": "hi", "session": {"profiel": "reviewer"}})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("unknown field `profiel`"), "{body}");
        let (status, body) = request(&app, "POST", "/v1/sessions", Some("test-token"), json!({"profile": "reviewer", "message": "sk-secret-value"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("unknown field `message`") && !body.to_string().contains("sk-secret-value"), "{body}");
        // 文档化的 session 字段与顶层字段仍然可用
        let (status, body) = request(
            &app,
            "POST",
            "/v1/sessions",
            Some("test-token"),
            json!({"provider": "faux", "profile": "reviewer", "cwd": ".", "model": "faux", "reasoningEffort": "high", "depth": "standard", "api": "auto", "account": "default", "tools": ["read"], "callerTools": [], "systemPrompt": "s", "appendSystemPrompt": "a"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = request(
            &app,
            "POST",
            "/v1/chat",
            Some("test-token"),
            json!({"message": "hi", "session": {"profile": "reviewer"}, "stream": false, "async": false, "timeoutSecs": 30, "reasoningEffort": "low", "depth": "standard", "account": "default", "webhookEvents": "final"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// default profile：openai，只允许 default；multi profile：allowedAccounts [default, work]。
    fn multi_account_app() -> (Router, Arc<Hub>) {
        let (mut hub, _) = crate::hub::tests::multi_hub(|p| p.load_context_files = false);
        let inner = Arc::get_mut(&mut hub).unwrap();
        let mut multi = inner.config.profiles["default"].clone();
        multi.allowed_accounts = Some(vec!["default".into(), "work".into()]);
        multi.fallback_accounts = Some(vec!["work".into()]);
        inner.config.profiles.insert("multi".into(), multi);
        inner.config.clients[0].profiles.push("multi".into());
        (test_router(hub.clone()), hub)
    }

    #[tokio::test]
    async fn account_override_is_403_when_not_allowed_and_reported_when_allowed() {
        let (app, hub) = multi_account_app();
        let denied = |name: &str, profile: &str| json!({"error": format!("account '{name}' is not allowed in profile '{profile}' (allowedAccounts)")});
        for (path, body, name, profile) in [
            ("/v1/sessions", json!({"account": "work"}), "work", "default"),
            ("/v1/sessions", json!({"account": "ghost"}), "ghost", "default"),
            ("/v1/sessions", json!({"profile": "multi", "account": "proxy"}), "proxy", "multi"),
            ("/v1/chat", json!({"message": "hi", "account": "work"}), "work", "default"),
            ("/v1/chat", json!({"message": "hi", "session": {"account": "work"}}), "work", "default"),
            ("/v1/chat", json!({"message": "hi", "async": true, "session": {"profile": "multi", "account": "ghost"}}), "ghost", "multi"),
        ] {
            let (status, response) = request(&app, "POST", path, Some("test-token"), body.clone()).await;
            assert_eq!((status, response), (StatusCode::FORBIDDEN, denied(name, profile)), "{path} {body}");
        }
        let (status, view) = request(&app, "POST", "/v1/sessions", Some("test-token"), json!({"profile": "multi", "account": "work"})).await;
        assert_eq!((status, view["account"].clone()), (StatusCode::CREATED, json!("work")));
        let (status, view) = request(&app, "POST", "/v1/sessions", Some("test-token"), json!({})).await;
        assert_eq!((status, view["account"].clone(), view["provider"].clone()), (StatusCode::CREATED, json!("default"), json!("openai")));
        // /v1/chat：顶层 account 优先于 session.account；响应带实际账号；消息也带账号
        let (status, body) = request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message": "hi", "account": "work", "session": {"profile": "multi", "account": "default"}})).await;
        assert_eq!((status, body["account"].clone()), (StatusCode::OK, json!("work")));
        assert!(body["messages"].as_array().unwrap().iter().any(|m| m["role"] == "assistant" && m["account"] == "work"));
        // 已有 sessionId：account 被忽略（账号是会话属性）
        let (status, again) = request(&app, "POST", "/v1/chat", Some("test-token"), json!({"message": "again", "sessionId": body["sessionId"], "account": "ghost"})).await;
        assert_eq!((status, again["account"].clone()), (StatusCode::OK, json!("work")));
        // 异步任务结果带账号
        let started = start_async(&app, json!({"message": "hi", "async": true, "account": "work", "session": {"profile": "multi"}})).await;
        let task = wait_task(&app, &started, false).await;
        assert_eq!((task["status"].clone(), task["result"]["account"].clone()), (json!("succeeded"), json!("work")));
        // 允许但加载失败的账号：400，不泄露启动错误细节
        drop(hub);
        let (mut hub, _) = crate::hub::tests::multi_hub(|p| p.allowed_accounts = Some(vec!["default".into(), "backup".into()]));
        Arc::get_mut(&mut hub).unwrap().config.profiles.get_mut("default").unwrap().load_context_files = false;
        let app = test_router(hub);
        let (status, response) = request(&app, "POST", "/v1/sessions", Some("test-token"), json!({"account": "backup"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["error"], "account 'backup' of provider openai is unavailable (it failed to load at startup; see server logs)");
        assert!(!response.to_string().contains("private"));
    }

    #[tokio::test]
    async fn profiles_and_models_report_accounts() {
        let (app, _hub) = multi_account_app();
        let (status, body) = request(&app, "GET", "/v1/profiles", Some("test-token"), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let find = |name: &str| body["profiles"].as_array().unwrap().iter().find(|p| p["name"] == name).unwrap().clone();
        let default = find("default");
        assert_eq!((default["account"].clone(), default["allowedAccounts"].clone(), default["fallbackAccounts"].clone()), (json!("default"), json!(["default"]), json!([])));
        let multi = find("multi");
        assert_eq!((multi["account"].clone(), multi["allowedAccounts"].clone(), multi["fallbackAccounts"].clone()), (json!("default"), json!(["default", "work"]), json!(["work"])));
        let (status, body) = request(&app, "GET", "/v1/models?account=work", Some("test-token"), json!({})).await;
        assert_eq!((status, body), (StatusCode::BAD_REQUEST, json!({"error": "account filter requires provider"})));
        let (status, body) = request(&app, "GET", "/v1/models?provider=openai&account=work&all=true", Some("test-token"), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!((body["models"][0]["id"].clone(), body["models"][0]["account"].clone(), body["models"][0]["provider"].clone()), (json!("gpt-deep-2"), json!("work"), json!("openai")));
        let (_, body) = request(&app, "GET", "/v1/models?provider=openai&all=true", Some("test-token"), json!({})).await;
        assert_eq!((body["models"][0]["id"].clone(), body["models"][0]["account"].clone()), (json!("gpt-deep-1"), json!("default")));
        // 已加载但没有任何 profile 允许 / 不存在：同样 403
        for name in ["proxy", "ghost"] {
            let (status, body) = request(&app, "GET", &format!("/v1/models?provider=openai&account={name}"), Some("test-token"), json!({})).await;
            assert_eq!((status, body), (StatusCode::FORBIDDEN, json!({"error": format!("account '{name}' is not allowed for this client")})));
        }
    }
}
