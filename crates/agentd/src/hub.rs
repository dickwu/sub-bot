use crate::routing::{Depth, Routing};
use crate::{
    config::{Client, Config, Profile, ResolvedTarget},
    issues::{IssueEvent, IssueScope, Reporter, Severity},
    webhook::{DeliveryState, DeliveryStatus, RetryPolicy, WebhookTarget},
};
use agent_ai::{anthropic::AnthropicProvider, faux::FauxProvider, openai::OpenAiProvider, EffortResolution, Message, ModelInfo, Provider, ThinkingLevel};
use agent_core::{Agent, AgentEvent, AgentTool, Emit, LoopConfig, Shared};
use agent_session::Session;
use agent_tools::{preset, CallbackTool, CallerToolSpec, ToolContext, ToolRegistry};
use anyhow::{bail, Result};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// 一个会话 = 一个 Agent + 可选 JSONL 持久化，归属于创建它的 client（会话隔离）。
pub struct SessionHandle {
    pub id: String,
    pub owner: String,
    pub profile: String,
    pub cwd: PathBuf,
    pub agent: Arc<Agent>,
    pub issues: Arc<Reporter>,
    issue_targets: Vec<ResolvedTarget>,
    run_generation: std::sync::atomic::AtomicU64,
    caller_tools: Vec<String>,
    observing: AtomicBool,
    /// 运行期可变状态（推理强度、路由、issue scope），与 Agent 的 before_run hook 共享
    dynamic: Arc<SessionDynamic>,
    store: Mutex<Option<Session>>,
    last_activity: Mutex<Instant>,
    idle_ttl: Duration,
    ephemeral: bool,
}

pub use agent_core::Disposition;

type ModelsCache = tokio::sync::Mutex<BTreeMap<String, (Instant, Vec<ModelInfo>)>>;
const MODELS_CACHE_TTL: Duration = Duration::from_secs(600);

/// provider 模型目录（缓存 10 分钟），供 /v1/models 与 `latest:` 别名解析共用。
async fn cached_models(cache: &ModelsCache, id: &str, provider: &dyn Provider) -> Result<Vec<ModelInfo>> {
    let mut cache = cache.lock().await;
    if let Some((_, models)) = cache.get(id).filter(|(t, _)| t.elapsed() < MODELS_CACHE_TTL) {
        return Ok(models.clone());
    }
    let models = provider.list_models().await.map_err(|e| anyhow::anyhow!("provider {id}: {e}"))?;
    cache.insert(id.into(), (Instant::now(), models.clone()));
    Ok(models)
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffortChange {
    pub requested: Option<ThinkingLevel>,
    pub effective: Option<ThinkingLevel>,
    pub adjusted: Option<String>,
    pub allowed: Vec<ThinkingLevel>,
}

impl EffortChange {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({"requested": self.requested, "effective": self.effective, "adjusted": self.adjusted, "allowedEfforts": self.allowed})
    }
}

/// 会话里运行期可变的部分。不引用 Agent / Hub，因此可以放进 Agent 的 before_run hook 而不形成循环引用。
pub(crate) struct SessionDynamic {
    session_id: String,
    provider_id: String,
    provider: Arc<dyn Provider>,
    model: Shared<String>,
    reasoning: Shared<Option<ThinkingLevel>>,
    /// (调用方请求的推理强度, 调整原因)
    effort: Mutex<(Option<ThinkingLevel>, Option<String>)>,
    routing: Mutex<Routing>,
    /// 配置的深度模型（可能是 `latest:<prefix>` 别名）
    deep_spec: Option<String>,
    router_model: String,
    catalog: Arc<ModelsCache>,
    issues: Arc<Reporter>,
    issue_scope: Mutex<IssueScope>,
    /// 最近一次成功调用实际使用的 (API, 推理强度)；推理强度变化后清空
    last_call: Mutex<Option<(Option<agent_ai::Api>, Option<ThinkingLevel>)>>,
    /// 已收到过 prompt：此后 depth 不能再修改，路由不再改变
    prompted: AtomicBool,
}

impl SessionDynamic {
    fn allowed_efforts(&self) -> Vec<ThinkingLevel> {
        agent_ai::allowed_efforts(&agent_ai::effort_caps(&self.provider_id, &self.model.get()))
    }

    /// 按当前模型解析推理强度并写入会话；返回解析结果和实际值是否变化。
    fn apply_effort(&self, requested: Option<ThinkingLevel>) -> (EffortResolution, bool) {
        let resolution = agent_ai::resolve_effort(&self.provider_id, &self.model.get(), requested);
        *self.effort.lock().unwrap() = (requested, resolution.adjusted.clone());
        let changed = self.reasoning.set(resolution.effective) != resolution.effective;
        if changed {
            *self.last_call.lock().unwrap() = None;
        }
        (resolution, changed)
    }

    fn effort_event(&self, r: &EffortResolution) -> AgentEvent {
        tracing::info!(session_id = %self.session_id, requested = r.requested.map(|l| l.as_str()).unwrap_or("-"), effective = r.effective.map(|l| l.as_str()).unwrap_or("-"), "reasoning effort changed");
        AgentEvent::EffortChanged { requested: r.requested, effective: r.effective, adjusted: r.adjusted.clone() }
    }

    fn set_model(&self, model: String) {
        self.issue_scope.lock().unwrap().model = Some(model.clone());
        self.model.set(model);
    }

    /// 第一次 prompt 之前的同步规划：standard/deep（固定 id）直接生效，auto 与 `latest:` 别名在第一次 run 前决定。
    fn plan(&self) {
        let model = {
            let mut r = self.routing.lock().unwrap();
            if r.decided || r.explicit {
                return;
            }
            let (model, reason) = match (r.depth, self.deep_spec.as_deref()) {
                (Depth::Standard, _) => (r.base_model.clone(), "depth standard".to_string()),
                (Depth::Deep, None) => (r.base_model.clone(), "no deepModel configured; using base model".into()),
                (Depth::Deep, Some(spec)) if spec.starts_with("latest:") => (r.base_model.clone(), "pending: deep model alias resolves at first prompt".into()),
                (Depth::Deep, Some(spec)) => (spec.to_string(), "depth deep".into()),
                (Depth::Auto, _) => (r.base_model.clone(), "pending: classified at first prompt".into()),
            };
            r.model = model.clone();
            r.reason = reason;
            model
        };
        self.set_model(model);
    }

    async fn deep_model(&self) -> std::result::Result<String, String> {
        let spec = self.deep_spec.as_deref().ok_or_else(|| "no deepModel configured".to_string())?;
        let Some(prefix) = spec.strip_prefix("latest:") else { return Ok(spec.to_string()) };
        let models = cached_models(&self.catalog, &self.provider_id, &*self.provider).await.map_err(|e| format!("cannot list models: {e}"))?;
        let resolved = crate::routing::resolve_latest(prefix, models.iter().map(|m| m.id.as_str())).ok_or_else(|| format!("no model matches {spec}"))?;
        tracing::info!(alias = spec, resolved = %resolved, "deep model alias resolved");
        Ok(resolved)
    }

    /// 每次 run 开始前调用；只有第一次真正路由。
    async fn route_first_run(self: Arc<Self>, messages: Vec<Message>, emit: Emit) {
        let (depth, explicit, base, current) = {
            let mut r = self.routing.lock().unwrap();
            if r.decided {
                return;
            }
            r.decided = true;
            (r.depth, r.explicit, r.base_model.clone(), r.model.clone())
        };
        let mut usage = None;
        let mut resolved_deep = None;
        let (model, reason, auto) = if explicit {
            (current, "explicit model".to_string(), false)
        } else {
            match depth {
                Depth::Standard => (base, "depth standard".to_string(), false),
                Depth::Deep => match self.deep_model().await {
                    Ok(m) => {
                        resolved_deep = Some(m.clone());
                        (m, "depth deep".into(), false)
                    }
                    Err(e) => (base, format!("deep model unavailable ({e}); using base model"), false),
                },
                Depth::Auto if self.deep_spec.is_none() => (base, "no deepModel configured; using base model".into(), false),
                Depth::Auto => {
                    let task = messages
                        .iter()
                        .filter_map(|m| match m {
                            Message::User { content, .. } => {
                                Some(content.iter().filter_map(|c| if let agent_ai::Content::Text { text } = c { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\n"))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let (result, router_usage) = crate::routing::classify(&*self.provider, &self.router_model, &task).await;
                    usage = router_usage;
                    match result {
                        Ok(c) if c.deep => match self.deep_model().await {
                            Ok(m) => {
                                resolved_deep = Some(m.clone());
                                (m, format!("router: {}", c.reason), true)
                            }
                            Err(e) => (base, format!("router chose deep but deep model unavailable ({e}); using base model"), true),
                        },
                        Ok(c) => (base, format!("router: {}", c.reason), true),
                        Err(e) => {
                            let scope = self.issue_scope.lock().unwrap().clone();
                            self.issues.report(
                                IssueEvent::new(Severity::Warn, "routing_error", "routing", "Model router failed; using the standard model")
                                    .scope(scope)
                                    .details(serde_json::json!({"routerModel": self.router_model, "error": e})),
                            );
                            (base, format!("router error: {e}"), true)
                        }
                    }
                }
            }
        };
        let routing = {
            let mut r = self.routing.lock().unwrap();
            r.model = model.clone();
            r.reason = reason;
            r.auto = auto;
            r.usage = usage;
            if resolved_deep.is_some() {
                r.deep_model = resolved_deep;
            }
            r.clone()
        };
        self.set_model(model);
        let requested = self.effort.lock().unwrap().0;
        let (resolution, changed) = self.apply_effort(requested);
        if changed {
            emit(self.effort_event(&resolution));
        }
        tracing::info!(session_id = %self.session_id, depth = routing.depth.as_str(), model = %routing.model, reason = %routing.reason, "model routed");
        emit(AgentEvent::ModelRouted { routing: serde_json::to_value(&routing).unwrap_or_default() });
    }
}

impl SessionHandle {
    /// 运行中则排队（默认 follow_up），否则后台启动一次 run。
    pub fn submit(self: &Arc<Self>, text: String, mode: Option<&str>) -> Disposition {
        self.dynamic.prompted.store(true, Ordering::Relaxed);
        self.run_generation.fetch_add(1, Ordering::Relaxed);
        if self.scope().task_id.is_none() {
            self.issues.subscribe(crate::issues::ScopeKey::Session(self.id.clone()), self.owner.clone(), self.issue_targets.clone());
        }
        if !self.observing.swap(true, Ordering::Relaxed) {
            let mut rx = self.subscribe();
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                loop {
                    let event = rx.recv().await;
                    let Some(h) = weak.upgrade() else { break };
                    match event {
                        Ok(event) => {
                            h.report_event(&event);
                            if matches!(event, AgentEvent::AgentSettled { .. }) {
                                let generation = h.run_generation.load(Ordering::Relaxed);
                                let weak = Arc::downgrade(&h);
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_millis(250)).await;
                                    if let Some(h) = weak.upgrade() {
                                        if h.run_generation.load(Ordering::Relaxed) == generation && !h.agent.is_running() {
                                            h.issues.unsubscribe(&crate::issues::ScopeKey::Session(h.id.clone()));
                                        }
                                    }
                                });
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => h.report_lag(n),
                        Err(_) => break,
                    }
                }
            });
        }
        let mut activity = self.last_activity.lock().unwrap();
        *activity = Instant::now();
        let msg_len = text.chars().count();
        let msg = Message::user_text(text);
        let this = self.clone();
        // 会话 span 挂在当前请求 span 之下，因此运行期间的日志同时带 request_id / bot / session_id / profile
        let effort = self.agent.config.reasoning.get().map(|l| l.as_str()).unwrap_or("-");
        let span = tracing::info_span!("session_run", session_id = %self.id, bot = %self.owner, profile = %self.profile, provider = %self.agent.config.provider.id(), model = %self.agent.config.model.get(), reasoning_effort = effort);
        let _entered = span.enter();
        let started = std::time::Instant::now();
        let disposition = self.agent.submit_with(msg, mode, move |new| {
            let (mut input, mut output, mut reasoning_tokens, mut tools) = (0u64, 0u64, 0u64, 0usize);
            let mut stop = None;
            for m in &new {
                match m {
                    Message::Assistant(a) => {
                        input += a.usage.input;
                        output += a.usage.output;
                        reasoning_tokens += a.usage.reasoning;
                        if a.api.is_some() || this.agent.config.provider.id() == "anthropic" {
                            *this.dynamic.last_call.lock().unwrap() = Some((a.api, a.reasoning_effort));
                        }
                        stop = Some(a.stop_reason);
                        if let Some(e) = &a.error_message {
                            tracing::warn!(issue = "typed", error = %e, "assistant message ended with error");
                        }
                    }
                    Message::ToolResult(_) => tools += 1,
                    _ => {}
                }
            }
            tracing::info!(latency_ms = started.elapsed().as_millis() as u64, messages = new.len(), tool_calls = tools, input_tokens = input, output_tokens = output, reasoning_tokens, stop_reason = ?stop, "run finished");
            if let Some(store) = this.store.lock().unwrap().as_mut() {
                for m in new {
                    if let Err(e) = store.append_message(m) {
                        tracing::error!(issue = "typed", error = %e, "persist failed");
                        this.report_issue(Severity::Error, "session_persist_failed", "session", "Session persistence failed", serde_json::json!({}));
                    }
                }
            }
            // 持久化完成后更新：长时间运行不会消耗下一次空闲窗口。
            *this.last_activity.lock().unwrap() = Instant::now();
        });
        drop(activity);
        tracing::info!(prompt_chars = msg_len, ?disposition, "message submitted");
        disposition
    }

    /// 实际生效的推理强度：上次修改之后有调用记录时取最近一次真正发送的值（含运行时降级），否则取会话解析后的值。
    pub fn effective_effort(&self) -> Option<ThinkingLevel> {
        match *self.dynamic.last_call.lock().unwrap() {
            Some((_, e)) => e,
            None => self.agent.config.reasoning.get(),
        }
    }

    /// 最近一次调用实际使用的 API（还没调用过时为 None）。
    pub fn effective_api(&self) -> Option<agent_ai::Api> {
        self.dynamic.last_call.lock().unwrap().and_then(|(a, _)| a)
    }

    /// 调用方/配置请求的推理强度（解析前）。
    pub fn requested_effort(&self) -> Option<ThinkingLevel> {
        self.dynamic.effort.lock().unwrap().0
    }

    /// 当前模型
    pub fn model(&self) -> String {
        self.agent.config.model.get()
    }

    pub fn provider_id(&self) -> &str {
        &self.dynamic.provider_id
    }

    /// 修改会话推理强度（粘性，从下一次模型调用生效，运行中也一样）；按当前模型的 top-3 规则解析。
    pub fn set_reasoning_effort(&self, requested: Option<ThinkingLevel>) -> EffortChange {
        let (resolution, _) = self.dynamic.apply_effort(requested);
        self.agent.emit(self.dynamic.effort_event(&resolution));
        EffortChange { requested: resolution.requested, effective: resolution.effective, adjusted: resolution.adjusted, allowed: self.dynamic.allowed_efforts() }
    }

    /// 只能在第一次 prompt 之前修改 depth。
    pub fn set_depth(&self, depth: Depth) -> Result<()> {
        if self.dynamic.prompted.load(Ordering::Relaxed) {
            bail!(DepthLocked);
        }
        self.dynamic.routing.lock().unwrap().depth = depth;
        self.dynamic.plan();
        let requested = self.requested_effort();
        let (resolution, changed) = self.dynamic.apply_effort(requested);
        if changed {
            self.agent.emit(self.dynamic.effort_event(&resolution));
        }
        Ok(())
    }

    pub fn routing(&self) -> Routing {
        self.dynamic.routing.lock().unwrap().clone()
    }

    /// reasoningEffort（实际值）/ requestedReasoningEffort / effortAdjusted / allowedEfforts / routing
    pub fn effort_fields(&self) -> serde_json::Map<String, serde_json::Value> {
        let (requested, adjusted) = self.dynamic.effort.lock().unwrap().clone();
        let value = serde_json::json!({
            "reasoningEffort": self.effective_effort(),
            "requestedReasoningEffort": requested,
            "effortAdjusted": adjusted,
            "allowedEfforts": self.dynamic.allowed_efforts(),
            "routing": self.routing(),
        });
        match value {
            serde_json::Value::Object(map) => map,
            _ => unreachable!(),
        }
    }

    pub fn set_request_id(&self, request_id: String) {
        self.dynamic.issue_scope.lock().unwrap().request_id = Some(request_id);
    }

    pub fn scope(&self) -> IssueScope {
        self.dynamic.issue_scope.lock().unwrap().clone()
    }

    pub fn report_issue(&self, severity: Severity, kind: &str, component: &str, message: &str, details: serde_json::Value) {
        self.issues.report(IssueEvent::new(severity, kind, component, message).scope(self.scope()).details(details));
    }

    pub fn report_lag(&self, skipped: u64) {
        self.report_issue(Severity::Warn, "events_lagged", "session", "Subscriber dropped events", serde_json::json!({"skipped": skipped}));
    }

    fn report_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageRetry { attempt, .. } => {
                self.report_issue(Severity::Warn, "llm_retry", "llm", "Retrying LLM call; partial message discarded", serde_json::json!({"attempt": attempt}));
            }
            AgentEvent::MessageEnd { message: Message::Assistant(a) }
                if a.stop_reason == agent_ai::StopReason::Error || (a.error_message.is_some() && a.stop_reason != agent_ai::StopReason::Aborted) =>
            {
                let error = a.error_message.as_deref().unwrap_or("");
                let (kind, status) = classify_llm_error(error);
                let summary = llm_error_summary(error, &self.issues);
                self.report_issue(Severity::Error, kind, "llm", &summary, serde_json::json!({"status": status, "stopReason": a.stop_reason}));
            }
            AgentEvent::ToolExecutionEnd { tool_name, tool_call_id, result, is_error: true } => {
                let text = result.content.iter().filter_map(|c| if let agent_ai::Content::Text { text } = c { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join(" ").to_lowercase();
                let kind = if text.contains("timed out") {
                    "tool_timeout"
                } else if text.contains("[aborted]") || text.contains("aborted before execution") {
                    "tool_aborted"
                } else if self.caller_tools.contains(tool_name) {
                    "callback_failed"
                } else {
                    "tool_error"
                };
                let exit_code = text.split("exit code ").nth(1).and_then(|s| s.split(|c: char| !c.is_ascii_digit() && c != '-').next()).and_then(|s| s.parse::<i32>().ok());
                self.issues.report(
                    IssueEvent::new(Severity::Warn, kind, "tool", "Tool execution failed")
                        .scope(self.scope())
                        .tool(tool_name)
                        .details(serde_json::json!({"toolCallId": tool_call_id, "exitCode": exit_code})),
                );
            }
            AgentEvent::RunOutcome { kind } => self.report_issue(if kind == "task_panic" { Severity::Fatal } else { Severity::Warn }, kind, "runtime", kind, serde_json::json!({})),
            AgentEvent::AgentSettled { aborted: true } => self.report_issue(Severity::Warn, "run_aborted", "runtime", "Run aborted", serde_json::json!({})),
            _ => {}
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentEvent> {
        self.agent.subscribe()
    }
}

/// 调用方在创建会话时可以提交的覆盖项（都受 profile 权限约束）。
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRequest {
    pub provider: Option<String>,
    pub profile: Option<String>,
    /// 会话工作目录：绝对路径或相对 profile 第一个 workspaceRoot；必须落在某个 workspaceRoot 内
    pub cwd: Option<PathBuf>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    /// 覆盖 profile 的默认模型；受 profile.allowedModels 限制
    pub model: Option<String>,
    /// 推理强度 none|minimal|low|medium|high|xhigh|max 或 provider 的小写名称；按模型 top-3 规则解析（钳制时记 warn）
    pub reasoning_effort: Option<ThinkingLevel>,
    /// standard | deep | auto（显式 model 时忽略）；默认 profile.autoRoute ? auto : standard
    pub depth: Option<Depth>,
    /// OpenAI API：auto | chat | responses（覆盖 profile.api）
    pub api: Option<agent_ai::Api>,
    /// 只能从 profile 已启用的工具里再收窄
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub caller_tools: Vec<CallerToolSpec>,
}

/// 第一次 prompt 之后不能再修改 depth（路由不会重新进行）。
#[derive(Debug)]
pub struct DepthLocked;

impl std::fmt::Display for DepthLocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("depth can only be set before the first prompt (model routing is decided once per session)")
    }
}

impl std::error::Error for DepthLocked {}

#[derive(Debug)]
pub struct TooManySessions;

impl std::fmt::Display for TooManySessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("too many sessions for this client; delete a session or wait for idle expiry")
    }
}

impl std::error::Error for TooManySessions {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Timeout,
    Aborted,
}

pub fn classify_llm_error(message: &str) -> (&'static str, Option<u16>) {
    let text = message.to_lowercase();
    let status = text.split("http ").skip(1).find_map(|suffix| {
        let digits = suffix.as_bytes();
        (digits.len() >= 3 && digits[..3].iter().all(u8::is_ascii_digit) && !digits.get(3).is_some_and(u8::is_ascii_digit)).then(|| suffix[..3].parse::<u16>().ok()).flatten()
    });
    let kind = if matches!(status, Some(401 | 403)) || text.contains("invalid api key") || text.contains("authentication_error") || text.contains("permission_error") {
        "llm_auth"
    } else if status == Some(429) || text.contains("rate_limit_error") {
        "llm_rate_limit"
    } else if text.contains("context length") || text.contains("context_length") || text.contains("prompt is too long") {
        "llm_context_overflow"
    } else if text.contains("idle timeout") || text.contains("idle timed out") {
        "llm_stream_timeout"
    } else {
        "llm_error"
    };
    (kind, status)
}

// Prefer the provider's message over its full JSON body (request IDs, metadata, etc.).
fn llm_error_summary(message: &str, reporter: &Reporter) -> String {
    let body = message.find('{').and_then(|start| serde_json::from_str::<serde_json::Value>(&message[start..]).ok());
    let summary = body
        .as_ref()
        .and_then(|body| body.pointer("/error/message").or_else(|| body.get("message")))
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or(message)
        .trim();
    let summary = if summary.is_empty() { "LLM request failed" } else { summary };
    // Redact before truncation: cutting a configured secret first could expose a prefix.
    reporter.redact(summary).chars().take(300).collect()
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct TargetDelivery {
    pub host: String,
    #[serde(flatten)]
    pub status: DeliveryStatus,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub task_id: String,
    pub owner: String,
    pub session_id: String,
    pub status: TaskStatus,
    pub created_at: u64,
    pub finished_at: Option<u64>,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    pub delivery: Option<DeliveryStatus>,
    pub deliveries: Vec<TargetDelivery>,
    #[serde(skip)]
    finished: Option<Instant>,
}

pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

#[derive(Clone)]
pub struct WebhookQueue {
    store: std::sync::Weak<crate::store::Store>,
    reporter: Arc<Reporter>,
    target: WebhookTarget,
    client: String,
    session_id: String,
    task_id: String,
    index: usize,
    policy: RetryPolicy,
}

impl WebhookQueue {
    pub fn send(&self, (event, payload): (&'static str, serde_json::Value)) -> Result<()> {
        let store = self.store.upgrade().ok_or_else(|| anyhow::anyhow!("hub stopped"))?;
        let delivery = crate::store::Delivery {
            url: self.target.url.clone(),
            payload,
            client: Some(self.client.clone()),
            session_id: Some(self.session_id.clone()),
            task_id: Some(self.task_id.clone()),
            index: Some(self.index),
            status: DeliveryStatus { delivery_id: uuid::Uuid::new_v4().to_string(), event: event.into(), state: DeliveryState::Pending, attempts: 0, last_status: None, last_error: None },
        };
        store.save_delivery(&delivery)?;
        store.start(delivery, Some(self.target.clone()), &self.reporter, self.policy.clone());
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub enum StoreMode {
    Memory,
    Persistent,
}

pub struct Hub {
    pub config: Config,
    pub issues: Arc<Reporter>,
    pub(crate) providers: BTreeMap<String, Arc<dyn Provider>>,
    default_provider: String,
    registry: ToolRegistry,
    session_dir: Option<PathBuf>,
    sessions: Mutex<HashMap<String, Arc<SessionHandle>>>,
    store: Arc<crate::store::Store>,
    shutting_down: AtomicBool,
    http: reqwest::Client,
    #[cfg(test)]
    pub(crate) callback_timeout: Option<Duration>,
    models_cache: Arc<ModelsCache>,
}

fn expand_tools(names: &[String]) -> Result<Vec<String>> {
    let mut out = vec![];
    for n in names {
        match n.strip_prefix("preset:") {
            Some(p) => out.extend(preset(p).ok_or_else(|| anyhow::anyhow!("unknown preset '{p}'"))?),
            None => out.push(n.clone()),
        }
    }
    out.dedup();
    Ok(out)
}

/// 解析并校验会话 cwd：canonicalize 后必须位于 profile 的某个 workspaceRoot 之内（防止越权访问其它目录）。
fn resolve_cwd(p: &Profile, requested: Option<&std::path::Path>) -> Result<(PathBuf, PathBuf)> {
    let roots: Vec<PathBuf> = p.workspace_roots.iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
    let first = roots.first().ok_or_else(|| anyhow::anyhow!("profile has no existing workspaceRoots"))?;
    let cwd = match requested {
        None => first.clone(),
        Some(c) if c.is_absolute() => std::fs::canonicalize(c)?,
        Some(c) => std::fs::canonicalize(first.join(c))?,
    };
    if !cwd.is_dir() {
        bail!("cwd is not a directory");
    }
    let root = roots.iter().find(|r| cwd.starts_with(r)).ok_or_else(|| anyhow::anyhow!("cwd is outside the profile's workspaceRoots"))?;
    Ok((cwd, root.clone()))
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProviderModel {
    pub provider: String,
    #[serde(flatten)]
    pub model: ModelInfo,
}

#[derive(Default)]
pub struct KeyFiles<'a> {
    pub openai: Option<&'a std::path::Path>,
    pub anthropic: Option<&'a std::path::Path>,
}

pub fn select_provider<'a>(requested: Option<&'a str>, profile: Option<&'a str>, model: &str, default: &'a str) -> &'a str {
    requested.or(profile).unwrap_or_else(|| if model.starts_with("claude-") { "anthropic" } else { default })
}

fn not_configured(id: &str) -> String {
    let upper = id.to_ascii_uppercase();
    format!("provider {id} is not configured (set --{id}-key-file / {upper}_API_KEY_FILE / providers.{id}.apiKeyFile / {upper}_API_KEY)")
}

fn load_providers(config: &Config, default: &str, files: KeyFiles<'_>, issues: &Reporter, getenv: impl Fn(&str) -> Option<String>) -> Result<BTreeMap<String, Arc<dyn Provider>>> {
    crate::config::validate_provider(default)?;
    let mut needed = BTreeSet::from([default, select_provider(None, None, &config.default_model, default)]);
    for p in config.profiles.values() {
        needed.insert(select_provider(None, p.provider.as_deref(), p.model.as_deref().unwrap_or(&config.default_model), default));
    }
    let mut providers: BTreeMap<String, Arc<dyn Provider>> = BTreeMap::new();
    providers.insert("faux".into(), Arc::new(FauxProvider));
    for pk in agent_ai::secrets::PROVIDER_KEYS {
        let id = pk.provider;
        let cli_file = if id == "openai" { files.openai } else { files.anthropic };
        let settings = config.providers.get(id);
        let config_file = settings.and_then(|p| p.api_key_file.as_deref()).or_else(|| if id == "openai" { config.api_key_file.as_deref() } else { None });
        let present = cli_file.is_some() || config_file.is_some() || [pk.env, pk.file_env].iter().any(|var| getenv(var).is_some_and(|s| !s.trim().is_empty()));
        if !present && !needed.contains(id) {
            tracing::debug!(provider = id, "provider not configured; skipping");
            continue;
        }
        anyhow::ensure!(present, "{}", not_configured(id));
        let (key, source) = agent_ai::secrets::load_provider_key(pk, cli_file, config_file, &getenv).map_err(anyhow::Error::msg)?;
        issues.add_secret(key.expose().to_owned());
        tracing::info!(provider = id, key_source = %source, "api key loaded");
        let base_url = getenv(&format!("{}_BASE_URL", id.to_ascii_uppercase())).filter(|s| !s.trim().is_empty()).or_else(|| settings.and_then(|p| p.base_url.clone()));
        let timeout = Duration::from_secs(config.stream_idle_timeout_secs);
        let provider: Arc<dyn Provider> = match id {
            "openai" => Arc::new(OpenAiProvider::with_stream_idle_timeout(key, base_url, timeout)),
            "anthropic" => Arc::new(AnthropicProvider::with_stream_idle_timeout(key, base_url, timeout)),
            _ => unreachable!("known provider key"),
        };
        providers.insert(id.into(), provider);
    }
    Ok(providers)
}

impl Hub {
    /// In-memory journals for standalone commands; session history remains independently configurable.
    /// `openai_key_file`：命令行 `--openai-key-file`。key 的加载顺序见 `agent_ai::secrets::load_provider_key`。
    pub fn new(config: Config, provider_name: &str, session_dir: Option<PathBuf>, openai_key_file: Option<&std::path::Path>) -> Result<Arc<Self>> {
        Self::with_reporter(config, provider_name, session_dir, openai_key_file, crate::issues::global().unwrap_or_else(Reporter::noop), StoreMode::Memory)
    }

    pub fn with_reporter(
        config: Config,
        provider_name: &str,
        session_dir: Option<PathBuf>,
        openai_key_file: Option<&std::path::Path>,
        issues: Arc<Reporter>,
        store_mode: StoreMode,
    ) -> Result<Arc<Self>> {
        Self::with_provider_keys(config, provider_name, session_dir, KeyFiles { openai: openai_key_file, anthropic: None }, issues, store_mode)
    }

    pub fn with_provider_keys(config: Config, provider_name: &str, session_dir: Option<PathBuf>, files: KeyFiles<'_>, issues: Arc<Reporter>, store_mode: StoreMode) -> Result<Arc<Self>> {
        config.validate()?;
        let providers = load_providers(&config, provider_name, files, &issues, |k| std::env::var(k).ok())?;
        let registry = ToolRegistry::with_builtins();
        let store = match store_mode {
            StoreMode::Memory => crate::store::Store::memory(),
            StoreMode::Persistent => crate::store::Store::open(&config.data_dir().join("state"), Duration::from_secs(config.task_ttl_secs))?,
        };
        let pending = store.pending();
        issues.attach_store(&store);
        for task in store.tasks.lock().unwrap().values_mut() {
            if matches!(task.status, TaskStatus::Queued | TaskStatus::Running) {
                task.status = TaskStatus::Failed;
                task.error = Some("interrupted by restart".into());
                task.finished_at = Some(unix_ms());
                store.save_task(task)?;
                let scope = IssueScope { client: Some(task.owner.clone()), task_id: Some(task.task_id.clone()), session_id: Some(task.session_id.clone()), ..Default::default() };
                if let Some(client) = config.clients.iter().find(|c| c.name == task.owner) {
                    issues.subscribe(crate::issues::ScopeKey::Task(task.task_id.clone()), task.owner.clone(), client.resolved_webhooks.clone());
                }
                issues.report(IssueEvent::new(Severity::Error, "task_interrupted", "store", "interrupted by restart").scope(scope));
                issues.unsubscribe(&crate::issues::ScopeKey::Task(task.task_id.clone()));
            }
        }
        for delivery in pending {
            let targets = match &delivery.client {
                Some(client) => config.clients.iter().find(|c| &c.name == client).map(|c| c.resolved_webhooks.as_slice()).unwrap_or(&[]),
                None => config.alerts.resolved.as_slice(),
            };
            let route = delivery.task_id.as_ref().map(|id| crate::issues::ScopeKey::Task(id.clone()));
            if let (Some(key), Some(client)) = (&route, &delivery.client) {
                issues.subscribe(key.clone(), client.clone(), targets.to_vec());
            }
            let target = targets.iter().chain(config.alerts.resolved.iter()).find(|t| t.target.url == delivery.url).map(|t| t.target.clone());
            store.start(delivery, target, &issues, RetryPolicy { max_attempts: config.webhook_max_attempts, ..Default::default() });
            if let Some(key) = route {
                issues.unsubscribe(&key);
            }
        }
        Ok(Arc::new(Self {
            issues,
            store,
            config,
            providers,
            default_provider: provider_name.into(),
            registry,
            session_dir,
            sessions: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            http: reqwest::Client::new(),
            #[cfg(test)]
            callback_timeout: None,
            models_cache: Default::default(),
        }))
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    /// Admission and task registration share the session lock with shutdown/removal.
    pub fn submit_task(&self, client: &Client, session_id: &str, text: String, targets: &[ResolvedTarget]) -> Option<Task> {
        let sessions = self.sessions.lock().unwrap();
        if self.is_shutting_down() {
            return None;
        }
        let h = sessions.get(session_id).filter(|h| h.owner == client.name)?;
        let mut task = Task {
            task_id: uuid::Uuid::new_v4().to_string(),
            owner: client.name.clone(),
            session_id: session_id.into(),
            status: TaskStatus::Queued,
            created_at: unix_ms(),
            finished_at: None,
            result: None,
            error: None,
            delivery: None,
            deliveries: vec![],
            finished: None,
        };
        if self.store.save_task(&task).is_err() {
            return None;
        }
        task.status = TaskStatus::Running;
        if self.store.save_task(&task).is_err() {
            return None;
        }
        h.dynamic.issue_scope.lock().unwrap().task_id = Some(task.task_id.clone());
        self.store.tasks.lock().unwrap().insert(task.task_id.clone(), task.clone());
        let issue_targets: Vec<_> = targets.iter().filter(|t| t.wants("issues")).cloned().collect();
        if !issue_targets.is_empty() {
            // I1 routes each matching scope independently; task events already carry task_id.
            self.issues.subscribe(crate::issues::ScopeKey::Task(task.task_id.clone()), client.name.clone(), issue_targets);
        }
        h.submit(text, None);
        Some(task)
    }

    pub fn unsubscribe_task(&self, task_id: &str, session: &SessionHandle) {
        self.issues.unsubscribe(&crate::issues::ScopeKey::Task(task_id.into()));
        let mut scope = session.dynamic.issue_scope.lock().unwrap();
        // A newer task may already have replaced the scope during the grace period.
        if scope.task_id.as_deref() == Some(task_id) {
            scope.task_id = None;
        }
    }

    pub fn get_task(&self, client: &Client, id: &str) -> Option<Task> {
        self.store.tasks.lock().unwrap().get(id).filter(|t| t.owner == client.name).cloned()
    }

    pub fn finish_task(&self, id: &str, status: TaskStatus, result: serde_json::Value) -> Option<Task> {
        let mut tasks = self.store.tasks.lock().unwrap();
        let task = tasks.get_mut(id)?;
        task.status = status;
        task.error = result["error"].as_str().map(str::to_owned);
        task.result = Some(result);
        task.finished_at = Some(unix_ms());
        task.finished = Some(Instant::now());
        if self.store.save_task(task).is_err() {
            self.issues.report(IssueEvent::new(Severity::Error, "store_write_failed", "store", "Task journal write failed"));
        }
        Some(task.clone())
    }

    /// Enqueue durably before spawning network work. Workers never retain the hub.
    pub fn webhook_queue(self: &Arc<Self>, id: String, target: WebhookTarget) -> WebhookQueue {
        let mut tasks = self.store.tasks.lock().unwrap();
        let task = tasks.get_mut(&id).expect("registered task");
        let index = task.deliveries.len();
        task.deliveries.push(TargetDelivery {
            host: crate::issues::host(&target.url),
            status: DeliveryStatus { delivery_id: String::new(), event: String::new(), state: DeliveryState::Pending, attempts: 0, last_status: None, last_error: None },
        });
        if self.store.save_task(task).is_err() {
            self.issues.report(IssueEvent::new(Severity::Error, "store_write_failed", "store", "Task journal write failed"));
        }
        WebhookQueue {
            store: Arc::downgrade(&self.store),
            reporter: self.issues.clone(),
            target,
            client: task.owner.clone(),
            session_id: task.session_id.clone(),
            task_id: id,
            index,
            policy: RetryPolicy { max_attempts: self.config.webhook_max_attempts, ..Default::default() },
        }
    }

    pub fn create_session(&self, client: &Client, req: SessionRequest) -> Result<Arc<SessionHandle>> {
        self.create_session_with_ttl(client, req, false)
    }

    pub fn create_chat_session(&self, client: &Client, req: SessionRequest) -> Result<Arc<SessionHandle>> {
        self.create_session_with_ttl(client, req, true)
    }

    fn create_session_with_ttl(&self, client: &Client, req: SessionRequest, ephemeral: bool) -> Result<Arc<SessionHandle>> {
        // 上限检查与插入共用 map 锁，避免并发创建突破上限。
        let mut sessions = self.sessions.lock().unwrap();
        anyhow::ensure!(!self.shutting_down.load(Ordering::Relaxed), "server is shutting down");
        let cap = self.config.max_sessions_per_client;
        if cap != 0 && sessions.values().filter(|h| h.owner == client.name).count() >= cap {
            Self::evict_idle_from(&mut sessions, Instant::now(), Some(&client.name));
            if sessions.values().filter(|h| h.owner == client.name).count() >= cap {
                self.issues
                    .report(IssueEvent::new(Severity::Warn, "session_limit", "session", "Client session limit reached").scope(IssueScope { client: Some(client.name.clone()), ..Default::default() }));
                return Err(TooManySessions.into());
            }
        }
        let profile_name = req.profile.clone().or_else(|| client.profiles.first().cloned()).ok_or_else(|| anyhow::anyhow!("client has no profiles"))?;
        if !client.profiles.contains(&profile_name) {
            bail!("profile '{profile_name}' is not allowed for client '{}'", client.name);
        }
        let p: &Profile = self.config.profiles.get(&profile_name).ok_or_else(|| anyhow::anyhow!("unknown profile '{profile_name}'"))?;

        let allowed = expand_tools(&p.tools)?;
        let selected = match req.tools {
            Some(t) => {
                if let Some(bad) = t.iter().find(|n| !allowed.contains(n)) {
                    bail!("tool '{bad}' is not enabled in profile '{profile_name}'");
                }
                t
            }
            None => allowed,
        };
        if (req.system_prompt.is_some() || req.append_system_prompt.is_some()) && !p.allow_system_prompt_override {
            bail!("profile '{profile_name}' does not allow system prompt override");
        }
        if !req.caller_tools.is_empty() && !p.allow_caller_tools {
            bail!("profile '{profile_name}' does not allow caller tools");
        }

        let store = match &self.session_dir {
            Some(dir) => Some(Session::create(&dir.join(&client.name), &profile_name)?),
            None => None,
        };
        let id = store.as_ref().map(|s| s.id.clone()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let (cwd, root) = resolve_cwd(p, req.cwd.as_deref())?;
        let ctx = ToolContext { cwd: cwd.clone(), http_allow_hosts: p.http_allow_hosts.clone(), shell_env: p.shell_env.clone() };
        let mut tools: Vec<Arc<dyn AgentTool>> = self.registry.build(&selected, &ctx)?;
        let caller_tools = req.caller_tools.iter().map(|t| t.name.clone()).collect();
        for spec in req.caller_tools {
            if tools.iter().any(|t| t.name() == spec.name) {
                bail!("caller tool '{}' collides with a built-in tool", spec.name);
            }
            let tool = CallbackTool::new(spec, id.clone(), self.http.clone());
            #[cfg(test)]
            let tool = CallbackTool { timeout: self.callback_timeout.unwrap_or(tool.timeout), ..tool };
            tools.push(Arc::new(tool));
        }

        let explicit = req.model.is_some();
        let model = match req.model {
            Some(m) if !Config::model_allowed(p, &m) => {
                bail!("model '{m}' is not allowed in profile '{profile_name}' (allowedModels)")
            }
            Some(m) => m,
            None => p.model.clone().unwrap_or_else(|| self.config.default_model.clone()),
        };
        let provider_id = select_provider(req.provider.as_deref(), p.provider.as_deref(), &model, &self.default_provider).to_string();
        let model = if explicit || p.model.is_some() { model } else { self.default_model_for(&provider_id) };
        if !Config::model_allowed(p, &model) {
            bail!("default model '{model}' is not in profile '{profile_name}' allowedModels");
        }
        let provider_id = provider_id.as_str();
        let provider = self.provider(provider_id)?;
        let requested_effort = req.reasoning_effort.or(p.reasoning_effort).or(self.config.default_reasoning_effort);
        let deep_spec = self.deep_model_spec(p, provider_id);
        let depth = req.depth.unwrap_or(if p.auto_route { Depth::Auto } else { Depth::Standard });
        let dynamic = Arc::new(SessionDynamic {
            session_id: id.clone(),
            provider_id: provider_id.into(),
            provider: provider.clone(),
            model: Shared::new(model.clone()),
            reasoning: Shared::new(None),
            effort: Mutex::new((requested_effort, None)),
            routing: Mutex::new(Routing {
                depth,
                model: model.clone(),
                base_model: model.clone(),
                deep_model: deep_spec.clone(),
                auto: false,
                reason: "explicit model".into(),
                usage: None,
                decided: false,
                explicit,
            }),
            deep_spec,
            router_model: p.router_model.clone().unwrap_or_else(|| model.clone()),
            catalog: self.models_cache.clone(),
            issues: self.issues.clone(),
            issue_scope: Mutex::new(IssueScope {
                client: Some(client.name.clone()),
                session_id: Some(id.clone()),
                profile: Some(profile_name.clone()),
                model: Some(model.clone()),
                provider: Some(provider_id.into()),
                ..Default::default()
            }),
            last_call: Mutex::new(None),
            prompted: AtomicBool::new(false),
        });
        dynamic.plan();
        dynamic.apply_effort(requested_effort);
        let hook = dynamic.clone();
        let config = LoopConfig {
            provider,
            model: dynamic.model.clone(),
            system_prompt: Some(crate::prompt::build(crate::prompt::PromptInput {
                custom: req.system_prompt.as_deref().or(p.system_prompt.as_deref()),
                append: &[p.append_system_prompt.as_deref().unwrap_or(""), req.append_system_prompt.as_deref().unwrap_or("")],
                tools: &tools,
                cwd: &cwd,
                root: &root,
                load_context_files: p.load_context_files,
            })),
            tools,
            reasoning: dynamic.reasoning.clone(),
            api: if provider_id == "openai" { req.api.or(p.api).unwrap_or(self.config.default_api) } else { agent_ai::Api::Auto },
            steering: Default::default(),
            follow_up: Default::default(),
            max_turns: p.max_turns,
            max_tokens: p.max_output_tokens,
            token_budget: p.token_budget,
            llm_max_retries: Some(p.llm_max_retries.unwrap_or(self.config.llm_max_retries)),
            run_timeout: p.run_timeout_secs.map(std::time::Duration::from_secs),
            before_run: Some(Arc::new(move |messages, emit| Box::pin(hook.clone().route_first_run(messages, emit)))),
        };
        tracing::info!(session_id = %id, bot = %client.name, profile = %profile_name, provider = provider_id, cwd = %cwd.display(), model = %config.model.get(), depth = depth.as_str(), reasoning_effort = config.reasoning.get().map(|l| l.as_str()).unwrap_or("-"), api = %self.effective_api(&config), tools = ?config.tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>(), "session created");
        let h = Arc::new(SessionHandle {
            issues: self.issues.clone(),
            issue_targets: client.resolved_webhooks.iter().filter(|t| t.wants("issues")).cloned().collect(),
            run_generation: std::sync::atomic::AtomicU64::new(0),
            caller_tools,
            observing: AtomicBool::new(false),
            id: id.clone(),
            owner: client.name.clone(),
            profile: profile_name,
            cwd,
            agent: Agent::new(config, vec![]),
            dynamic,
            store: Mutex::new(store),
            last_activity: Mutex::new(Instant::now()),
            idle_ttl: Duration::from_secs(if ephemeral { self.config.chat_session_ttl_secs } else { self.config.session_idle_ttl_secs }),
            ephemeral,
        });
        sessions.insert(id, h.clone());
        Ok(h)
    }

    /// 只返回属于该 client 的会话；别的 bot 的会话一律当作不存在。
    pub fn get(&self, client: &Client, id: &str) -> Option<Arc<SessionHandle>> {
        self.sessions.lock().unwrap().get(id).filter(|h| h.owner == client.name).cloned()
    }

    /// HTTP 提交与删除/清理共用 map 锁：已取出的旧 handle 不能在删除后重新启动。
    pub fn submit(&self, client: &Client, id: &str, text: String, mode: Option<&str>) -> Option<Disposition> {
        let sessions = self.sessions.lock().unwrap();
        if self.is_shutting_down() {
            return None;
        }
        let h = sessions.get(id).filter(|h| h.owner == client.name)?;
        Some(h.submit(text, mode))
    }

    pub fn remove(&self, client: &Client, id: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        if !sessions.get(id).is_some_and(|h| h.owner == client.name) {
            return false;
        }
        let h = sessions.remove(id).unwrap();
        h.agent.abort();
        tracing::info!(session_id = %h.id, bot = %h.owner, "session deleted; persisted JSONL files retained");
        true
    }

    /// 关机时禁止新会话/提交，取消全部运行并等待持久化完成；释放 handle 后 SSE 也会结束。
    pub async fn abort_all(&self) {
        let sessions: Vec<_> = {
            let mut sessions = self.sessions.lock().unwrap();
            self.shutting_down.store(true, Ordering::Relaxed);
            sessions.drain().map(|(_, h)| h).collect()
        };
        let n = sessions.iter().filter(|h| h.agent.is_running()).count();
        let m = self.store.tasks.lock().unwrap().values().filter(|t| t.status == TaskStatus::Running).count();
        if n > 0 || m > 0 {
            self.issues.report(IssueEvent::new(Severity::Warn, "shutdown_inflight", "runtime", "Shutdown with in-flight work").details(serde_json::json!({"sessions": n, "tasks": m})));
        }
        for h in &sessions {
            h.agent.abort();
        }
        // Agent 在持久化回调之后才清除 running；不持有 map 锁跨 await。
        let _ = tokio::time::timeout(Duration::from_secs(self.config.shutdown_grace_secs), async {
            while sessions.iter().any(|h| h.agent.is_running()) || self.store.tasks.lock().unwrap().values().any(|t| t.status == TaskStatus::Running) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            self.issues.flush(Duration::from_secs(self.config.shutdown_grace_secs)).await;
            self.store.flush().await;
        })
        .await;
    }

    pub fn evict_idle(&self, now: Instant) -> usize {
        self.store.tasks.lock().unwrap().retain(|_, t| {
            t.finished.map_or_else(
                || t.finished_at.is_none_or(|finished| unix_ms().saturating_sub(finished) <= self.config.task_ttl_secs.saturating_mul(1000)),
                |finished| now.saturating_duration_since(finished) <= Duration::from_secs(self.config.task_ttl_secs),
            )
        });
        Self::evict_idle_from(&mut self.sessions.lock().unwrap(), now, None)
    }

    fn evict_idle_from(sessions: &mut HashMap<String, Arc<SessionHandle>>, now: Instant, owner: Option<&str>) -> usize {
        let before = sessions.len();
        sessions.retain(|_, h| {
            if owner.is_some_and(|o| o != h.owner) || h.idle_ttl.is_zero() {
                return true;
            }
            let activity = h.last_activity.lock().unwrap();
            if h.agent.is_running() || now.saturating_duration_since(*activity) <= h.idle_ttl {
                return true;
            }
            tracing::info!(session_id = %h.id, bot = %h.owner, ephemeral = h.ephemeral, "idle session evicted; persisted JSONL files retained");
            false
        });
        before - sessions.len()
    }

    /// 两类 TTL 独立；普通会话关闭过期时仍然清理 chat 会话。
    pub fn sweep_interval(&self) -> Option<Duration> {
        [self.config.session_idle_ttl_secs, self.config.chat_session_ttl_secs, self.config.task_ttl_secs.max(1)].into_iter().filter(|ttl| *ttl != 0).min().map(|ttl| Duration::from_secs(ttl.min(60)))
    }

    /// 实时模型列表（缓存 10 分钟）。`all=false` 时只保留可用于对话的模型并去掉日期快照。
    pub async fn list_models(&self, all: bool) -> Result<Vec<ProviderModel>> {
        self.list_models_filtered(all, None).await
    }

    pub async fn list_models_filtered(&self, all: bool, filter: Option<&str>) -> Result<Vec<ProviderModel>> {
        if let Some(id) = filter {
            self.provider(id)?;
        }
        let mut result = vec![];
        for (id, provider) in &self.providers {
            if filter.is_some_and(|f| f != id) {
                continue;
            }
            for model in &cached_models(&self.models_cache, id, &**provider).await? {
                if all || id != "openai" || (agent_ai::models::is_chat_model(&model.id) && !agent_ai::models::is_snapshot(&model.id)) {
                    result.push(ProviderModel { provider: id.clone(), model: model.clone() });
                }
            }
        }
        Ok(result)
    }

    /// 会话预计使用的 API（auto 按模型解析；非 OpenAI provider 返回 "n/a"）。运行时若被迫切换，以消息里的 `api` 为准。
    pub fn effective_api(&self, cfg: &LoopConfig) -> &'static str {
        if cfg.provider.id() != "openai" {
            return "n/a";
        }
        match cfg.api {
            agent_ai::Api::Auto => agent_ai::default_api(&cfg.model.get()).as_str(),
            a => a.as_str(),
        }
    }

    pub fn provider(&self, id: &str) -> Result<Arc<dyn Provider>> {
        crate::config::validate_provider(id)?;
        self.providers.get(id).cloned().ok_or_else(|| anyhow::anyhow!(not_configured(id)))
    }

    pub fn profile_provider<'a>(&'a self, p: &'a Profile) -> &'a str {
        select_provider(None, p.provider.as_deref(), p.model.as_deref().unwrap_or(&self.config.default_model), &self.default_provider)
    }

    /// 没有请求/配置模型时 provider 的默认模型：providers.<id>.defaultModel > （anthropic 且全局 defaultModel 不是 Claude 时）claude-opus-5-5 > defaultModel。
    pub fn default_model_for(&self, provider_id: &str) -> String {
        if let Some(model) = self.config.providers.get(provider_id).and_then(|p| p.default_model.clone()) {
            return model;
        }
        if provider_id == "anthropic" && !self.config.default_model.starts_with("claude-") {
            return crate::config::DEFAULT_CLAUDE_MODEL.into();
        }
        self.config.default_model.clone()
    }

    /// profile 的基础模型（与会话创建时的选择一致）。
    pub fn profile_model(&self, p: &Profile) -> String {
        p.model.clone().unwrap_or_else(|| self.default_model_for(self.profile_provider(p)))
    }

    /// profile 配置的深度模型；anthropic profile 默认固定为 claude-fable-5-1。
    pub fn deep_model_spec(&self, p: &Profile, provider_id: &str) -> Option<String> {
        p.deep_model.clone().or_else(|| (provider_id == "anthropic").then(|| crate::config::DEFAULT_CLAUDE_DEEP_MODEL.into()))
    }

    /// 会话视图（POST/GET/PATCH /v1/sessions）。
    pub fn session_view(&self, h: &SessionHandle) -> serde_json::Value {
        let mut view = serde_json::json!({
            "sessionId": h.id, "profile": h.profile, "cwd": h.cwd, "provider": h.provider_id(), "model": h.model(),
            "api": h.effective_api().map(|a| a.as_str()).unwrap_or_else(|| self.effective_api(&h.agent.config)),
            "isStreaming": h.agent.is_running(),
            "tools": h.agent.config.tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>(),
        });
        view.as_object_mut().unwrap().extend(h.effort_fields());
        view
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.registry.names()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn config() -> Config {
        let mut cfg = Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
        cfg.data_dir = Some(std::env::temp_dir().join(format!("sub-bot-state-{}", uuid::Uuid::new_v4())));
        cfg.profiles.get_mut("default").unwrap().load_context_files = false;
        cfg
    }

    #[test]
    fn provider_selection_precedence() {
        for (request, profile, model, default, expected) in [
            (Some("faux"), Some("openai"), "claude-sonnet-4-5", "openai", "faux"),
            (None, Some("faux"), "claude-sonnet-4-5", "openai", "faux"),
            (None, None, "claude-sonnet-4-5", "faux", "anthropic"),
            (None, None, "gpt-4.1", "faux", "faux"),
            (None, None, "unknown", "openai", "openai"),
        ] {
            assert_eq!(select_provider(request, profile, model, default), expected);
        }
    }

    #[test]
    fn needed_keys_fail_and_unneeded_providers_are_skipped() {
        let mut cfg = config();
        let load = |cfg: &Config, default| load_providers(cfg, default, KeyFiles::default(), &Reporter::noop(), |_| None);
        assert!(load(&cfg, "openai").err().unwrap().to_string().contains("provider openai is not configured"));
        assert_eq!(load(&cfg, "faux").unwrap().keys().cloned().collect::<Vec<_>>(), ["faux"]);
        cfg.profiles.get_mut("default").unwrap().provider = Some("anthropic".into());
        assert_eq!(load(&cfg, "faux").err().unwrap().to_string(), not_configured("anthropic"));
        cfg.profiles.get_mut("default").unwrap().provider = None;
        cfg.profiles.get_mut("default").unwrap().model = Some("claude-sonnet-4-5".into());
        assert_eq!(load(&cfg, "faux").err().unwrap().to_string(), not_configured("anthropic"));
        cfg.profiles.get_mut("default").unwrap().model = None;
        cfg.default_model = "claude-sonnet-4-5".into();
        assert_eq!(load(&cfg, "faux").err().unwrap().to_string(), not_configured("anthropic"));
    }

    #[tokio::test]
    async fn optional_keys_load_and_all_loaded_keys_are_redacted() {
        let cfg = config();
        let reporter = Reporter::new(&cfg.alerts, None, vec![]);
        let providers = load_providers(&cfg, "faux", KeyFiles::default(), &reporter, |var| match var {
            "OPENAI_API_KEY" => Some("test-openai-value".into()),
            "ANTHROPIC_API_KEY" => Some("test-anthropic-value".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(providers.len(), 3);
        reporter.report(IssueEvent::new(Severity::Error, "llm_auth", "llm", "test-anthropic-value test-openai-value").details(serde_json::json!({"diagnostic": "test-anthropic-value"})));
        reporter.flush(Duration::from_secs(1)).await;
        let events = reporter.recent(None, 0, 100);
        assert_eq!(events[0].message, "[redacted] [redacted]");
        assert_eq!(events[0].details["diagnostic"], "[redacted]");
    }

    struct CatalogProvider(&'static str);
    impl Provider for CatalogProvider {
        fn id(&self) -> &str {
            self.0
        }
        fn stream(&self, req: agent_ai::Request, cancel: tokio_util::sync::CancellationToken) -> agent_ai::EventStream {
            FauxProvider.stream(req, cancel)
        }
        fn list_models(&self) -> futures::future::BoxFuture<'_, Result<Vec<ModelInfo>, String>> {
            Box::pin(async { Ok(["gpt-4.1", "gpt-4.1-2025-04-14", "text-embedding-3-small"].into_iter().map(|id| agent_ai::models::info_for(id.into(), None, None)).collect()) })
        }
    }

    #[tokio::test]
    async fn merged_catalog_filters_only_openai_and_sessions_use_selected_provider() {
        let mut cfg = config();
        cfg.profiles.get_mut("default").unwrap().reasoning_effort = Some(ThinkingLevel::High);
        cfg.profiles.get_mut("default").unwrap().api = Some(agent_ai::Api::Chat);
        let mut hub = Hub::new(cfg, "faux", None, None).unwrap();
        let inner = Arc::get_mut(&mut hub).unwrap();
        inner.providers.insert("openai".into(), Arc::new(CatalogProvider("openai")));
        inner.providers.insert("anthropic".into(), Arc::new(CatalogProvider("anthropic")));
        let models = hub.list_models(false).await.unwrap();
        assert_eq!(models.len(), 5);
        let json = serde_json::to_value(&models).unwrap();
        assert_eq!(json[0]["provider"], "anthropic");
        assert_eq!(json[0]["id"], "gpt-4.1");
        assert_eq!(hub.list_models_filtered(true, Some("openai")).await.unwrap().len(), 3);
        assert_eq!(hub.list_models_filtered(false, Some("faux")).await.unwrap().len(), 1);
        let h = hub.create_session(&hub.config.clients[0], SessionRequest { provider: Some("anthropic".into()), model: Some("gpt-4.1".into()), ..Default::default() }).unwrap();
        assert_eq!(h.agent.config.provider.id(), "anthropic");
        assert_eq!(h.agent.config.reasoning.get(), Some(ThinkingLevel::High));
        assert_eq!(h.agent.config.api, agent_ai::Api::Auto);
        assert_eq!(hub.effective_api(&h.agent.config), "n/a");
        assert_eq!(h.scope().provider.as_deref(), Some("anthropic"));
        let mut events = h.subscribe();
        h.submit("hello".into(), None);
        let mut text = String::new();
        while let Ok(event) = events.recv().await {
            match event {
                AgentEvent::MessageEnd { message: Message::Assistant(a) } => text = a.text(),
                AgentEvent::AgentSettled { .. } => break,
                _ => {}
            }
        }
        assert_eq!(text, "echo: hello");
        assert_eq!(h.effective_effort(), None); // Non-OpenAI messages still record the actual thinking level.
        assert_eq!(h.effective_api(), None);
    }

    /// 记录每次请求的 provider（回复与 faux 相同）；可指定模型目录。
    pub(crate) struct RecordingProvider {
        pub id: &'static str,
        pub seen: Arc<Mutex<Vec<agent_ai::Request>>>,
        pub models: Vec<&'static str>,
    }
    impl Provider for RecordingProvider {
        fn id(&self) -> &str {
            self.id
        }
        fn stream(&self, req: agent_ai::Request, cancel: tokio_util::sync::CancellationToken) -> agent_ai::EventStream {
            self.seen.lock().unwrap().push(req.clone());
            FauxProvider.stream(req, cancel)
        }
        fn list_models(&self) -> futures::future::BoxFuture<'_, Result<Vec<ModelInfo>, String>> {
            let models = self.models.iter().map(|id| agent_ai::models::info_for(id.to_string(), Some("anthropic".into()), None)).collect();
            Box::pin(async move { Ok(models) })
        }
    }

    type Seen = Arc<Mutex<Vec<agent_ai::Request>>>;

    /// default profile 走 anthropic（记录请求的 faux 实现），issues 可查询。
    fn claude_hub(models: Vec<&'static str>, edit: impl FnOnce(&mut Profile)) -> (Arc<Hub>, Seen) {
        let mut hub = Hub::new(config(), "faux", None, None).unwrap();
        let inner = Arc::get_mut(&mut hub).unwrap();
        let seen = Seen::default();
        inner.providers.insert("anthropic".into(), Arc::new(RecordingProvider { id: "anthropic", seen: seen.clone(), models }));
        inner.issues = Reporter::new(&inner.config.alerts, None, vec![]);
        let profile = inner.config.profiles.get_mut("default").unwrap();
        profile.provider = Some("anthropic".into());
        edit(profile);
        (hub, seen)
    }

    async fn prompt_and_wait(h: &Arc<SessionHandle>, text: &str) {
        let mut rx = h.subscribe();
        h.submit(text.into(), None);
        settled(&mut rx).await;
    }

    fn is_router(req: &agent_ai::Request) -> bool {
        req.system_prompt.as_deref() == Some(crate::routing::ROUTER_SYSTEM_PROMPT)
    }

    #[tokio::test]
    async fn claude_default_model_and_overrides() {
        let (hub, _) = claude_hub(vec![], |_| {});
        let client = &hub.config.clients[0];
        let h = hub.create_session(client, SessionRequest::default()).unwrap();
        assert_eq!(h.model(), crate::config::DEFAULT_CLAUDE_MODEL);
        assert_eq!(h.routing().deep_model.as_deref(), Some(crate::config::DEFAULT_CLAUDE_DEEP_MODEL));
        assert_eq!(h.routing().depth, Depth::Standard);
        let (mut hub, _) = claude_hub(vec![], |_| {});
        let inner = Arc::get_mut(&mut hub).unwrap();
        inner.config.providers.insert("anthropic".into(), crate::config::ProviderConfig { default_model: Some("claude-sonnet-5-5".into()), ..Default::default() });
        let client = hub.config.clients[0].clone();
        assert_eq!(hub.create_session(&client, SessionRequest::default()).unwrap().model(), "claude-sonnet-5-5");
        let (hub, _) = claude_hub(vec![], |p| p.model = Some("claude-haiku-5-5".into()));
        assert_eq!(hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap().model(), "claude-haiku-5-5");
        // 非 anthropic profile 没有默认深度模型
        let hub = Hub::new(config(), "faux", None, None).unwrap();
        assert_eq!(hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap().routing().deep_model, None);
    }

    #[tokio::test]
    async fn explicit_model_wins_and_static_depths() {
        let (hub, seen) = claude_hub(vec![], |p| {
            p.model = Some("claude-sonnet-4-6".into());
            p.reasoning_effort = Some(ThinkingLevel::Medium);
        });
        let client = &hub.config.clients[0];
        let explicit = hub.create_session(client, SessionRequest { model: Some("claude-opus-4-6".into()), depth: Some(Depth::Deep), ..Default::default() }).unwrap();
        prompt_and_wait(&explicit, "hello").await;
        let routing = explicit.routing();
        assert_eq!((routing.model.as_str(), routing.reason.as_str(), routing.depth, routing.decided), ("claude-opus-4-6", "explicit model", Depth::Deep, true));
        assert_eq!(seen.lock().unwrap().last().unwrap().model, "claude-opus-4-6");

        let deep = hub.create_session(client, SessionRequest { depth: Some(Depth::Deep), ..Default::default() }).unwrap();
        // 固定 id 的深度模型在创建时就生效；effort medium 在 fable 上被钳制到 high
        assert_eq!(deep.model(), "claude-fable-5-1");
        assert_eq!(deep.effective_effort(), Some(ThinkingLevel::High));
        assert_eq!(deep.effort_fields()["effortAdjusted"], "clamped to allowed top-three levels");
        prompt_and_wait(&deep, "hello").await;
        let last = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!((last.model.as_str(), last.reasoning), ("claude-fable-5-1", Some(ThinkingLevel::High)));
        assert_eq!(deep.routing().base_model, "claude-sonnet-4-6");
        assert_eq!(deep.routing().reason, "depth deep");

        let standard = hub.create_session(client, SessionRequest::default()).unwrap();
        assert_eq!(standard.effective_effort(), Some(ThinkingLevel::Medium));
        assert!(standard.effort_fields()["effortAdjusted"].is_null());
        // 第一次 prompt 之前可以改 depth
        standard.set_depth(Depth::Deep).unwrap();
        assert_eq!(standard.model(), "claude-fable-5-1");
        standard.set_depth(Depth::Standard).unwrap();
        prompt_and_wait(&standard, "hello").await;
        assert_eq!(seen.lock().unwrap().last().unwrap().model, "claude-sonnet-4-6");
        assert!(standard.set_depth(Depth::Deep).unwrap_err().is::<DepthLocked>());
        assert!(!seen.lock().unwrap().iter().any(is_router));
    }

    #[tokio::test]
    async fn auto_routing_uses_router_once() {
        for (prompt, expected_model, reason) in
            [(r#"{"deep":true,"reason":"open-ended design"}"#, "claude-fable-5-1", "router: open-ended design"), (r#"{"deep":false,"reason":"small edit"}"#, "claude-sonnet-4-6", "router: small edit")]
        {
            let (hub, seen) = claude_hub(vec![], |p| {
                p.model = Some("claude-sonnet-4-6".into());
                p.auto_route = true;
                p.router_model = Some("claude-haiku-4-5".into());
            });
            let h = hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
            assert_eq!(h.routing().depth, Depth::Auto);
            assert!(!h.routing().decided);
            let mut rx = h.subscribe();
            prompt_and_wait(&h, prompt).await;
            let mut routed = None;
            while let Ok(event) = rx.try_recv() {
                if let AgentEvent::ModelRouted { routing } = event {
                    routed = Some(routing);
                }
            }
            assert_eq!(routed.unwrap()["model"], expected_model);
            let routing = h.routing();
            assert_eq!((routing.model.as_str(), routing.reason.as_str(), routing.auto), (expected_model, reason, true));
            assert!(routing.usage.is_some());
            {
                let seen = seen.lock().unwrap();
                assert_eq!(seen.len(), 2);
                assert!(is_router(&seen[0]));
                assert_eq!((seen[0].model.as_str(), seen[0].max_tokens, seen[0].tools.len()), ("claude-haiku-4-5", Some(crate::routing::ROUTER_MAX_TOKENS), 0));
                assert_eq!(seen[0].reasoning, Some(ThinkingLevel::Low));
                assert_eq!(seen[1].model, expected_model);
            }
            // 第二次 prompt 不再路由
            prompt_and_wait(&h, r#"{"deep":true,"reason":"again"}"#).await;
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 3);
            assert!(!is_router(&seen[2]));
            assert_eq!(seen[2].model, expected_model);
            assert_eq!(h.routing().reason, reason);
        }
    }

    #[tokio::test]
    async fn router_garbage_falls_back_to_standard_and_reports_issue() {
        let (hub, seen) = claude_hub(vec![], |p| {
            p.model = Some("claude-sonnet-4-6".into());
            p.auto_route = true;
        });
        let h = hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
        prompt_and_wait(&h, "hello there").await;
        let routing = h.routing();
        assert_eq!(routing.model, "claude-sonnet-4-6");
        assert!(routing.reason.starts_with("router error: "), "{}", routing.reason);
        assert_eq!(seen.lock().unwrap()[0].model, "claude-sonnet-4-6"); // routerModel defaults to the base model
        hub.issues.flush(Duration::from_secs(2)).await;
        let issues = hub.issues.recent(Some("default"), 0, 100);
        let issue = issues.iter().find(|e| e.kind == "routing_error").expect("routing issue");
        assert_eq!(issue.severity, Severity::Warn);
        assert_eq!(issue.scope.session_id.as_deref(), Some(h.id.as_str()));
        assert!(!serde_json::to_string(issue).unwrap().contains("hello there"));
    }

    #[tokio::test]
    async fn latest_alias_resolves_from_catalog_at_first_prompt() {
        let catalog = vec!["claude-fable-5", "claude-fable-5-20260101", "claude-fable-5-1", "claude-opus-5-5"];
        let (hub, seen) = claude_hub(catalog, |p| p.deep_model = Some("latest:claude-fable".into()));
        let h = hub.create_session(&hub.config.clients[0], SessionRequest { depth: Some(Depth::Deep), ..Default::default() }).unwrap();
        assert_eq!(h.model(), "claude-opus-5-5");
        assert_eq!(h.routing().deep_model.as_deref(), Some("latest:claude-fable"));
        prompt_and_wait(&h, "hello").await;
        assert_eq!(h.routing().model, "claude-fable-5-1");
        assert_eq!(h.routing().deep_model.as_deref(), Some("claude-fable-5-1"));
        assert_eq!(seen.lock().unwrap().last().unwrap().model, "claude-fable-5-1");

        let (hub, _) = claude_hub(vec!["claude-opus-5-5"], |p| p.deep_model = Some("latest:claude-fable".into()));
        let h = hub.create_session(&hub.config.clients[0], SessionRequest { depth: Some(Depth::Deep), ..Default::default() }).unwrap();
        prompt_and_wait(&h, "hello").await;
        assert_eq!(h.routing().model, "claude-opus-5-5");
        assert!(h.routing().reason.contains("no model matches latest:claude-fable"), "{}", h.routing().reason);
    }

    #[tokio::test]
    async fn effort_changes_apply_to_next_call_and_clamp() {
        let (hub, seen) = claude_hub(vec![], |p| p.model = Some("claude-sonnet-4-6".into()));
        let h = hub.create_session(&hub.config.clients[0], SessionRequest { reasoning_effort: Some(ThinkingLevel::High), ..Default::default() }).unwrap();
        let mut rx = h.subscribe();
        let change = h.set_reasoning_effort(Some(ThinkingLevel::Low));
        assert_eq!(
            change,
            EffortChange {
                requested: Some(ThinkingLevel::Low),
                effective: Some(ThinkingLevel::Medium),
                adjusted: Some("clamped to allowed top-three levels".into()),
                allowed: vec![ThinkingLevel::Medium, ThinkingLevel::High, ThinkingLevel::Max]
            }
        );
        match rx.try_recv().unwrap() {
            AgentEvent::EffortChanged { requested, effective, adjusted } => {
                assert_eq!((requested, effective), (Some(ThinkingLevel::Low), Some(ThinkingLevel::Medium)));
                assert!(adjusted.is_some());
            }
            other => panic!("unexpected {other:?}"),
        }
        prompt_and_wait(&h, "hello").await;
        assert_eq!(seen.lock().unwrap().last().unwrap().reasoning, Some(ThinkingLevel::Medium));
        h.set_reasoning_effort(Some(ThinkingLevel::Max));
        prompt_and_wait(&h, "hello").await;
        assert_eq!(seen.lock().unwrap().last().unwrap().reasoning, Some(ThinkingLevel::Max));
        assert_eq!(h.requested_effort(), Some(ThinkingLevel::Max));
    }

    #[tokio::test]
    async fn memory_hub_leaves_server_journals_untouched_with_or_without_server() {
        let cfg = config();
        let dir = cfg.data_dir();
        let server = Hub::with_reporter(cfg.clone(), "faux", None, None, Reporter::noop(), StoreMode::Persistent).unwrap();
        let task: Task = serde_json::from_value(serde_json::json!({
            "taskId": "server-task", "owner": "default", "sessionId": "server-session", "status": "running", "createdAt": unix_ms(),
            "finishedAt": null, "result": null, "error": null, "delivery": null, "deliveries": []
        }))
        .unwrap();
        server.store.save_task(&task).unwrap();
        server.store.tasks.lock().unwrap().insert(task.task_id.clone(), task.clone());
        let delivery = crate::store::Delivery {
            url: "http://127.0.0.1:1/hook".into(),
            payload: serde_json::json!({"ok": true}),
            client: Some("default".into()),
            session_id: None,
            task_id: None,
            index: None,
            status: DeliveryStatus { delivery_id: "server-delivery".into(), event: "task.completed".into(), state: DeliveryState::Pending, attempts: 1, last_status: Some(500), last_error: None },
        };
        server.store.save_delivery(&delivery).unwrap();
        let snapshot = || {
            let mut files: Vec<_> = std::fs::read_dir(dir.join("state")).unwrap().map(|entry| entry.unwrap().path()).collect();
            files.sort();
            files.into_iter().map(|path| (path.file_name().unwrap().to_owned(), std::fs::read(path).unwrap())).collect::<Vec<_>>()
        };
        let before = snapshot();
        let mut server = Some(server);
        for running in [true, false] {
            if !running {
                drop(server.take());
            }
            let memory = Hub::new(cfg.clone(), "faux", None, None).unwrap();
            assert!(memory.store.tasks.lock().unwrap().is_empty());
            assert!(memory.store.pending().is_empty());
            let h = memory.create_session(&cfg.clients[0], SessionRequest::default()).unwrap();
            let local = memory.submit_task(&cfg.clients[0], &h.id, "hello".into(), &[]).unwrap();
            memory.finish_task(&local.task_id, TaskStatus::Succeeded, serde_json::json!({"text": "done"})).unwrap();
            memory.store.save_delivery(&delivery).unwrap();
            assert_eq!(memory.store.pending().len(), 1);
            let mut completed = delivery.clone();
            completed.status.state = DeliveryState::Delivered;
            memory.store.save_delivery(&completed).unwrap();
            assert!(memory.store.pending().is_empty());
            memory.abort_all().await;
            drop(memory);
            assert_eq!(snapshot(), before);
        }
        // With no existing state, standalone commands must not even create the data directory.
        let mut fresh = cfg;
        fresh.data_dir = Some(dir.join("unused"));
        let memory = Hub::new(fresh.clone(), "faux", None, None).unwrap();
        assert!(!fresh.data_dir().exists());
        drop(memory);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn restart_resumes_same_delivery_and_interrupts_running_task() {
        use axum::{http::StatusCode, routing::post, Json, Router};
        let healthy = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let app = Router::new().route(
            "/hook",
            post({
                let healthy = healthy.clone();
                let seen = seen.clone();
                move |Json(payload): Json<serde_json::Value>| {
                    let healthy = healthy.clone();
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push(payload);
                        if healthy.load(Ordering::Relaxed) {
                            StatusCode::NO_CONTENT
                        } else {
                            StatusCode::INTERNAL_SERVER_ERROR
                        }
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = WebhookTarget { url: format!("http://{}/hook", listener.local_addr().unwrap()), secret: Some("PRIVATE-WEBHOOK-KEY".into()) };
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = std::env::temp_dir().join(format!("sub-bot-resume-{}", uuid::Uuid::new_v4()));
        let mut cfg = config();
        cfg.data_dir = Some(dir.clone());
        cfg.clients[0].resolved_webhooks = vec![ResolvedTarget { target: target.clone(), events: None, min_severity: None }];
        let reporter = Reporter::new(&cfg.alerts, None, vec![]);
        let hub = Hub::with_reporter(cfg.clone(), "faux", None, None, reporter.clone(), StoreMode::Persistent).unwrap();
        let h = hub.create_session(&cfg.clients[0], SessionRequest::default()).unwrap();
        let task = hub.submit_task(&cfg.clients[0], &h.id, "hello".into(), &cfg.clients[0].resolved_webhooks).unwrap();
        let queue = hub.webhook_queue(task.task_id.clone(), target);
        queue.send(("task.completed", serde_json::json!({"ok": true}))).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while hub.store.pending().iter().all(|d| d.status.last_status != Some(500)) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let delivery_id = hub.store.pending()[0].status.delivery_id.clone();
        let weak = Arc::downgrade(&hub.store);
        drop(queue);
        drop(hub);
        assert!(weak.upgrade().is_none());
        healthy.store(true, Ordering::Relaxed);
        let hub = Hub::with_reporter(cfg.clone(), "faux", None, None, reporter.clone(), StoreMode::Persistent).unwrap();
        let restored = hub.get_task(&cfg.clients[0], &task.task_id).unwrap();
        assert_eq!(restored.status, TaskStatus::Failed);
        assert_eq!(restored.error.as_deref(), Some("interrupted by restart"));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if hub.get_task(&cfg.clients[0], &task.task_id).unwrap().delivery.is_some_and(|d| d.state == DeliveryState::Delivered) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        reporter.flush(Duration::from_secs(3)).await;
        hub.store.flush().await;
        let restored = hub.get_task(&cfg.clients[0], &task.task_id).unwrap();
        let delivered = restored.delivery.unwrap();
        assert_eq!(delivered.delivery_id, delivery_id);
        assert_eq!(delivered.attempts, 2);
        assert!(reporter.recent(Some("default"), 0, 100).iter().any(|e| e.kind == "task_interrupted" && e.scope.task_id.as_deref() == Some(&task.task_id)));
        let payloads = seen.lock().unwrap().clone();
        let completed: Vec<_> = payloads.iter().filter(|p| p["event"] == "task.completed").collect();
        assert_eq!(completed.len(), 2);
        assert!(completed.iter().all(|p| p["deliveryId"] == delivery_id));
        for name in ["tasks.jsonl", "deliveries.jsonl"] {
            assert!(!std::fs::read_to_string(dir.join("state").join(name)).unwrap().contains("PRIVATE-WEBHOOK-KEY"));
        }
        drop(hub);
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn removed_target_is_failed_and_shutdown_wait_is_bounded() {
        let dir = std::env::temp_dir().join(format!("sub-bot-removed-{}", uuid::Uuid::new_v4()));
        let mut cfg = config();
        cfg.data_dir = Some(dir.clone());
        cfg.shutdown_grace_secs = 0;
        let reporter = Reporter::new(&cfg.alerts, None, vec![]);
        let hub = Hub::with_reporter(cfg.clone(), "faux", None, None, reporter.clone(), StoreMode::Persistent).unwrap();
        let h = hub.create_session(&cfg.clients[0], SessionRequest::default()).unwrap();
        let task = hub.submit_task(&cfg.clients[0], &h.id, "hello".into(), &[]).unwrap();
        let queue = hub.webhook_queue(task.task_id.clone(), WebhookTarget { url: "http://127.0.0.1:1/removed".into(), secret: None });
        queue.send(("task.completed", serde_json::json!({}))).unwrap();
        tokio::time::timeout(Duration::from_secs(1), hub.abort_all()).await.unwrap();
        assert!(hub.is_shutting_down());
        assert!(!hub.store.pending().is_empty());
        drop(queue);
        drop(hub);
        let hub = Hub::with_reporter(cfg.clone(), "faux", None, None, reporter.clone(), StoreMode::Persistent).unwrap();
        tokio::time::timeout(Duration::from_secs(3), hub.store.flush()).await.unwrap();
        reporter.flush(Duration::from_secs(3)).await;
        let task = hub.get_task(&cfg.clients[0], &task.task_id).unwrap();
        let delivery = task.delivery.unwrap();
        assert_eq!(delivery.state, DeliveryState::Failed);
        assert_eq!(delivery.last_error.as_deref(), Some("target no longer configured"));
        assert!(reporter.recent(Some("default"), 0, 100).iter().any(|e| e.kind == "webhook_target_removed"));
        drop(hub);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn sync_chat_routes_issues_and_late_summary_only_to_owner() {
        use tower::ServiceExt;
        let seen = Arc::new(Mutex::new(Vec::<(String, serde_json::Value)>::new()));
        let app = axum::Router::new().route(
            "/{owner}",
            axum::routing::post({
                let seen = seen.clone();
                move |axum::extract::Path(owner): axum::extract::Path<String>, axum::Json(payload): axum::Json<serde_json::Value>| {
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push((owner, payload));
                        axum::http::StatusCode::NO_CONTENT
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut cfg = config();
        cfg.alerts.dedup_window_secs = 1;
        cfg.profiles.get_mut("default").unwrap().max_turns = 0;
        let target =
            |owner: &str, severity| ResolvedTarget { target: WebhookTarget { url: format!("{base}/{owner}"), secret: None }, events: Some(vec!["issues".into()]), min_severity: Some(severity) };
        cfg.clients[0].resolved_webhooks = vec![target("own", Severity::Warn), target("filtered", Severity::Fatal)];
        cfg.clients.push(Client { name: "other".into(), resolved_webhooks: vec![target("other", Severity::Warn)], ..cfg.clients[0].clone() });
        let reporter = Reporter::new(&cfg.alerts, None, vec![]);
        let hub = Hub::with_reporter(cfg.clone(), "faux", None, None, reporter.clone(), StoreMode::Memory).unwrap();
        let session = hub.create_session(&cfg.clients[0], SessionRequest::default()).unwrap();
        let app = crate::http::test_router(hub.clone());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/chat")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .body(axum::body::Body::from(serde_json::json!({"message": "hello", "sessionId": session.id}).to_string()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(3), async {
            while reporter.recent(Some("default"), 0, 100).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        session.report_issue(Severity::Warn, "max_turns", "runtime", "max_turns", serde_json::json!({}));
        tokio::time::sleep(Duration::from_millis(300)).await;
        reporter.unsubscribe(&crate::issues::ScopeKey::Session(session.id.clone()));
        // Exercise actual window expiration, not an explicit flush while the route is active.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if seen.lock().unwrap().iter().any(|(_, p)| p["data"]["count"].as_u64().is_some_and(|n| n > 1)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let payloads = seen.lock().unwrap().clone();
        assert_eq!(payloads.len(), 2);
        assert!(payloads.iter().all(|(owner, p)| owner == "own" && p["data"]["client"] == "default"));
        hub.abort_all().await;
        server.abort();
    }

    #[test]
    fn llm_http_status_is_generic_and_not_taken_from_body_numbers() {
        for (text, expected) in [
            ("Anthropic HTTP 404 Not Found: model missing", ("llm_error", Some(404))),
            ("OpenAI HTTP 500 Internal Server Error", ("llm_error", Some(500))),
            ("Anthropic HTTP 529: overloaded", ("llm_error", Some(529))),
            ("OpenAI HTTP 401 Unauthorized", ("llm_auth", Some(401))),
            ("OpenAI HTTP 403 Forbidden", ("llm_auth", Some(403))),
            ("OpenAI HTTP 429 Too Many Requests", ("llm_rate_limit", Some(429))),
            ("OpenAI HTTP 400: input has 429 tokens", ("llm_error", Some(400))),
            ("model-401-403-429 not found", ("llm_error", None)),
            ("HTTP 4040", ("llm_error", None)),
            ("HTTP 40", ("llm_error", None)),
            ("HTTP 错误", ("llm_error", None)),
        ] {
            assert_eq!(classify_llm_error(text), expected, "{text}");
        }
    }

    #[tokio::test]
    async fn llm_issue_summaries_are_informative_redacted_and_bounded() {
        let mut hub = Hub::new(config(), "faux", None, None).unwrap();
        Arc::get_mut(&mut hub).unwrap().issues = Reporter::new(&hub.config.alerts, None, vec!["configured-private-value".into()]);
        let long_message = format!("{}configured-private-value {}", "界".repeat(290), "尾".repeat(50));
        for (error, expected, status) in [
            (
                r#"Anthropic HTTP 404 Not Found: {"type":"error","error":{"type":"not_found_error","message":"model: claude-nonexistent-xyz"},"request_id":"unused"}"#,
                "model: claude-nonexistent-xyz".to_owned(),
                Some(404),
            ),
            (
                r#"OpenAI HTTP 404 Not Found (responses API): {"error":{"message":"Unknown model; sk-private-key configured-private-value","type":"invalid_request_error"}}"#,
                "Unknown model; [redacted] [redacted]".to_owned(),
                Some(404),
            ),
            (r#"Anthropic stream error: {"type":"api_error","message":"provider unavailable"}"#, "provider unavailable".to_owned(), None),
            ("OpenAI HTTP 502: upstream unavailable sk-private-key", "OpenAI HTTP 502: upstream unavailable [redacted]".to_owned(), Some(502)),
            (long_message.as_str(), format!("{}[redacted]", "界".repeat(290)), None),
            ("", "LLM request failed".to_owned(), None),
        ] {
            let h = hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
            h.report_event(&AgentEvent::MessageEnd { message: Message::Assistant(agent_ai::AssistantMessage::failed("test-model", agent_ai::StopReason::Error, error)) });
            hub.issues.flush(Duration::from_secs(2)).await;
            let events = hub.issues.recent(Some("default"), 0, 1);
            assert_eq!(events[0].kind, "llm_error");
            assert_eq!(events[0].message, expected);
            assert_eq!(events[0].details["status"], serde_json::json!(status));
            assert!(events[0].message.chars().count() <= 300);
        }
    }

    #[tokio::test]
    async fn llm_classification_and_safe_tool_issues() {
        for (text, expected) in [
            ("HTTP 401 secret", "llm_auth"),
            ("HTTP 403", "llm_auth"),
            ("invalid API key", "llm_auth"),
            ("HTTP 429", "llm_rate_limit"),
            ("context length exceeded", "llm_context_overflow"),
            ("stream idle timeout", "llm_stream_timeout"),
            ("Anthropic HTTP 401 Unauthorized: authentication failed", "llm_auth"),
            ("Anthropic HTTP 403 Forbidden: denied", "llm_auth"),
            (r#"Anthropic stream error: {"type":"authentication_error","message":"authentication failed"}"#, "llm_auth"),
            (r#"Anthropic stream error: {"type":"permission_error","message":"denied"}"#, "llm_auth"),
            (r#"Anthropic stream error: {"type":"rate_limit_error","message":"busy"}"#, "llm_rate_limit"),
            ("Anthropic HTTP 400 Bad Request: prompt is too long: 210000 tokens", "llm_context_overflow"),
            ("unknown", "llm_error"),
        ] {
            assert_eq!(classify_llm_error(text).0, expected);
        }
        let mut hub = Hub::new(config(), "faux", None, None).unwrap();
        Arc::get_mut(&mut hub).unwrap().issues = Reporter::new(&hub.config.alerts, None, vec![]);
        let h = hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
        for (text, kind) in [("[timed out] PRIVATE_OUTPUT", "tool_timeout"), ("[aborted] PRIVATE_OUTPUT", "tool_aborted"), ("PRIVATE_OUTPUT [exit code 7]", "tool_error")] {
            h.report_event(&AgentEvent::ToolExecutionEnd { tool_call_id: "call".into(), tool_name: "bash".into(), result: agent_core::ToolResult::error(text), is_error: true });
            hub.issues.flush(Duration::from_secs(2)).await;
            let events = hub.issues.recent(Some("default"), 0, 1);
            assert_eq!(events[0].kind, kind);
            assert!(!serde_json::to_string(&events).unwrap().contains("PRIVATE_OUTPUT"));
        }
        h.report_event(&AgentEvent::MessageRetry { attempt: 1, reason: "Anthropic HTTP 529: overloaded".into() });
        hub.issues.flush(Duration::from_secs(2)).await;
        let events = hub.issues.recent(Some("default"), 0, 1);
        assert_eq!(events[0].kind, "llm_retry");
        assert_eq!(events[0].details["attempt"], 1);
        assert_eq!(events[0].scope.provider.as_deref(), Some("faux"));
        h.report_lag(42);
        hub.issues.flush(Duration::from_secs(2)).await;
        let events = hub.issues.recent(Some("default"), 0, 1);
        assert_eq!(events[0].kind, "events_lagged");
        assert_eq!(events[0].details["skipped"], 42);
    }

    #[tokio::test]
    async fn task_webhook_failure_keeps_issue_routes_after_task_cleanup() {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let app = axum::Router::new()
            .route(
                "/failed",
                axum::routing::post({
                    let started = started.clone();
                    let release = release.clone();
                    move || {
                        let started = started.clone();
                        let release = release.clone();
                        async move {
                            started.notify_one();
                            release.notified().await;
                            axum::http::StatusCode::BAD_REQUEST
                        }
                    }
                }),
            )
            .route(
                "/issues",
                axum::routing::post({
                    let seen = seen.clone();
                    move |axum::Json(payload): axum::Json<serde_json::Value>| {
                        let seen = seen.clone();
                        async move {
                            seen.lock().unwrap().push(payload);
                            axum::http::StatusCode::NO_CONTENT
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut hub = Hub::new(config(), "faux", None, None).unwrap();
        Arc::get_mut(&mut hub).unwrap().issues = Reporter::new(&crate::config::AlertsConfig::default(), None, vec![]);
        let h = hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
        let targets: Vec<_> = ["failed", "issues"]
            .into_iter()
            .map(|path| ResolvedTarget { target: WebhookTarget { url: format!("{base}/{path}"), secret: None }, events: Some(vec!["issues".into()]), min_severity: None })
            .collect();
        let task = hub.submit_task(&hub.config.clients[0], &h.id, "hello".into(), &targets).unwrap();
        let queue = hub.webhook_queue(task.task_id.clone(), targets[0].target.clone());
        queue.send(("task.completed", serde_json::json!({}))).unwrap();
        tokio::time::timeout(Duration::from_secs(3), started.notified()).await.unwrap();
        hub.unsubscribe_task(&task.task_id, &h);
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if !seen.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        hub.issues.flush(Duration::from_secs(3)).await;
        let payloads = seen.lock().unwrap().clone();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["data"]["kind"], "webhook_delivery_failed");
        assert_eq!(payloads[0]["data"]["taskId"], task.task_id);
        assert_eq!(payloads[0]["data"]["sessionId"], h.id);
        assert_eq!(payloads[0]["data"]["client"], h.owner);
        drop(queue);
        hub.finish_task(&task.task_id, TaskStatus::Succeeded, serde_json::json!({}));
        hub.abort_all().await;
        server.abort();
    }

    #[tokio::test]
    async fn run_limits_emit_scoped_issues() {
        for kind in ["max_turns", "token_budget_exceeded", "run_timeout"] {
            let mut cfg = config();
            let profile = cfg.profiles.get_mut("default").unwrap();
            match kind {
                "max_turns" => profile.max_turns = 0,
                "token_budget_exceeded" => profile.token_budget = Some(0),
                _ => profile.run_timeout_secs = Some(0),
            }
            let mut hub = Hub::new(cfg, "faux", None, None).unwrap();
            Arc::get_mut(&mut hub).unwrap().issues = Reporter::new(&hub.config.alerts, None, vec![]);
            let h = hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
            let mut rx = h.subscribe();
            h.submit("/tool bash {\"command\":\"sleep 10\"}".into(), None);
            settled(&mut rx).await;
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if hub.issues.recent(Some("default"), 0, 100).iter().any(|e| e.kind == kind && e.scope.session_id.as_deref() == Some(&h.id)) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            hub.abort_all().await;
        }
    }

    #[tokio::test]
    async fn hub_and_sessions_share_installed_reporter() {
        crate::issues::install(Reporter::noop());
        let reporter = crate::issues::global().unwrap();
        let hub = Hub::new(config(), "faux", None, None).unwrap();
        assert!(Arc::ptr_eq(&hub.issues, &reporter));
        let session = hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
        assert!(Arc::ptr_eq(&session.issues, &reporter));
    }

    #[test]
    fn cap_is_typed_per_client_and_zero_is_unlimited() {
        let mut cfg = config();
        cfg.max_sessions_per_client = 1;
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let h = hub.create_session(client, SessionRequest::default()).unwrap();
        let err = hub.create_session(client, SessionRequest::default()).err().unwrap();
        assert!(err.is::<TooManySessions>());
        let other = Client { name: "other".into(), ..client.clone() };
        hub.create_session(&other, SessionRequest::default()).unwrap();
        assert!(hub.remove(client, &h.id));
        hub.create_session(client, SessionRequest::default()).unwrap();

        let mut cfg = config();
        cfg.max_sessions_per_client = 0;
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        for _ in 0..65 {
            hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
        }
    }

    #[test]
    fn cap_reclaims_only_owners_expired_sessions() {
        let mut cfg = config();
        cfg.max_sessions_per_client = 1;
        cfg.session_idle_ttl_secs = 10;
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let other = Client { name: "other".into(), ..client.clone() };
        let h = hub.create_session(client, SessionRequest::default()).unwrap();
        let unrelated = hub.create_session(&other, SessionRequest::default()).unwrap();
        let past = Instant::now() - Duration::from_secs(11);
        *h.last_activity.lock().unwrap() = past;
        *unrelated.last_activity.lock().unwrap() = past;
        hub.create_session(client, SessionRequest::default()).unwrap();
        assert!(hub.get(client, &h.id).is_none());
        assert!(hub.get(&other, &unrelated.id).is_some());
    }

    #[test]
    fn idle_eviction_uses_fake_time_and_per_session_ttl() {
        let hub = Hub::new(config(), "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let regular = hub.create_session(client, SessionRequest::default()).unwrap();
        let chat = hub.create_chat_session(client, SessionRequest::default()).unwrap();
        let now = Instant::now();
        *regular.last_activity.lock().unwrap() = now;
        *chat.last_activity.lock().unwrap() = now;
        assert_eq!(hub.evict_idle(now + Duration::from_secs(600)), 0);
        assert_eq!(hub.evict_idle(now + Duration::from_secs(601)), 1);
        assert!(hub.get(client, &chat.id).is_none());
        assert!(hub.get(client, &regular.id).is_some());
        assert_eq!(hub.evict_idle(now + Duration::from_secs(3601)), 1);
        assert_eq!(hub.evict_idle(now + Duration::from_secs(7200)), 0);
    }

    #[test]
    fn disabled_ttl_is_independent_of_chat_ttl() {
        let mut cfg = config();
        cfg.session_idle_ttl_secs = 0;
        cfg.chat_session_ttl_secs = 2;
        let hub = Hub::new(cfg.clone(), "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let regular = hub.create_session(client, SessionRequest::default()).unwrap();
        hub.create_chat_session(client, SessionRequest::default()).unwrap();
        assert_eq!(hub.sweep_interval(), Some(Duration::from_secs(2)));
        assert_eq!(hub.evict_idle(Instant::now() + Duration::from_secs(7200)), 1);
        assert!(hub.get(client, &regular.id).is_some());
        drop(hub);
        cfg.chat_session_ttl_secs = 0;
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        assert_eq!(hub.sweep_interval(), Some(Duration::from_secs(60)));
        hub.create_chat_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
        assert_eq!(hub.evict_idle(Instant::now() + Duration::from_secs(7200)), 0);
    }

    async fn settled(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let AgentEvent::AgentSettled { aborted } = rx.recv().await.unwrap() {
                    return aborted;
                }
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn abort_all_settles_persists_and_prevents_new_runs() {
        let dir = std::env::temp_dir().join(format!("sub-bot-shutdown-{}", uuid::Uuid::new_v4()));
        let hub = Hub::new(config(), "faux", Some(dir.clone()), None).unwrap();
        let client = &hub.config.clients[0];
        let mut handles = vec![];
        for _ in 0..2 {
            let h = hub.create_session(client, SessionRequest::default()).unwrap();
            let rx = h.subscribe();
            h.submit("hello".into(), None);
            h.submit("discard this queued message".into(), None);
            handles.push((h, rx));
        }
        tokio::time::timeout(Duration::from_secs(5), hub.abort_all()).await.unwrap();
        for (h, mut rx) in handles {
            assert!(settled(&mut rx).await);
            assert!(!h.agent.is_running());
            let path = h.store.lock().unwrap().as_ref().unwrap().path.clone();
            let persisted = Session::open(&path).unwrap().active_messages();
            assert!(!persisted.is_empty());
            assert_eq!(persisted, h.agent.messages().await);
            assert!(!persisted.contains(&Message::user_text("discard this queued message")));
            assert!(hub.submit(client, &h.id, "late".into(), None).is_none());
        }
        assert!(hub.create_session(client, SessionRequest::default()).err().unwrap().to_string().contains("shutting down"));
        hub.abort_all().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn remove_checks_owner_and_aborts_running_session() {
        let hub = Hub::new(config(), "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let h = hub.create_session(client, SessionRequest::default()).unwrap();
        let other = Client { name: "other".into(), ..client.clone() };
        let mut rx = h.subscribe();
        h.submit("hello".into(), None);
        assert!(h.agent.is_running());
        assert!(!hub.remove(&other, &h.id));
        assert!(hub.get(&other, &h.id).is_none());
        assert!(hub.remove(client, &h.id));
        assert!(hub.get(client, &h.id).is_none());
        assert!(!hub.remove(client, &h.id));
        assert!(hub.submit(client, &h.id, "late".into(), None).is_none());
        assert!(settled(&mut rx).await);
        assert!(!h.agent.is_running());
    }

    #[tokio::test]
    async fn running_sessions_survive_and_activity_refreshes_on_submit_and_settle() {
        let mut cfg = config();
        cfg.session_idle_ttl_secs = 10;
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let h = hub.create_session(client, SessionRequest::default()).unwrap();
        let old = Instant::now() - Duration::from_secs(20);
        *h.last_activity.lock().unwrap() = old;
        let mut rx = h.subscribe();
        h.submit("hello".into(), None);
        assert!(*h.last_activity.lock().unwrap() > old);
        assert_eq!(hub.evict_idle(Instant::now() + Duration::from_secs(100)), 0);
        // 模拟长运行，settle 应重新开启完整的空闲窗口。
        *h.last_activity.lock().unwrap() = old;
        assert!(!settled(&mut rx).await);
        let finished = *h.last_activity.lock().unwrap();
        assert!(finished > old);
        assert_eq!(hub.evict_idle(finished + Duration::from_secs(10)), 0);
        assert_eq!(hub.evict_idle(finished + Duration::from_secs(11)), 1);
    }
}
