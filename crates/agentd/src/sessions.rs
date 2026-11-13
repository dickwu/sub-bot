//! 会话持久化、重启后恢复（rehydrate）、列表 / 清除，以及会话级上下文压缩编排。
//! - [`SessionHooks`]：Agent 的 [`ContextHooks`] —— 每条消息加入上下文时追加到 JSONL；每次模型调用前估计上下文，
//!   超过阈值时自动压缩（摘要 + 保留最近消息），写入 `compaction` 条目。
//! - [`Hub::get_or_load`] / [`Hub::load_persisted`]：内存里没有、但在同一 client 会话目录里的会话按需恢复。
//! - [`Hub::list_sessions`] / [`Hub::delete_session`]：列出（内存 + 磁盘去重）与删除（可选 purge JSONL）。
//! - [`SessionHandle::compact`]：手动压缩（运行中 ⇒ [`CompactError::Busy`]）。
use crate::compaction::{CompactionSettings, Outcome, SummaryCall, SummaryError};
use crate::config::Client;
use crate::hub::{unix_ms, Hub, Restore, SessionDynamic, SessionHandle, SessionRequest, TooManySessions};
use crate::issues::{IssueEvent, Severity};
use crate::routing::Routing;
use agent_ai::{Content, Message, Usage};
use agent_core::{AgentEvent, AgentTool, ContextHooks, Emit};
use agent_session::{compaction as cp, compaction::CutPoint, Session, SessionMeta};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// 自动压缩失败后，这段时间内不再自动尝试（避免每次模型调用都重复一次失败的摘要调用）。
const AUTO_RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);
const TITLE_CHARS: usize = 80;
/// `GET /v1/sessions` 默认条数
pub const DEFAULT_LIST_LIMIT: usize = 50;

/// 会话不能从磁盘恢复（文件损坏、profile/cwd/模型不再允许、provider 未加载等）。HTTP 409。
#[derive(Debug)]
pub struct ResumeFailed(pub String);

impl std::fmt::Display for ResumeFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session cannot be resumed: {}", self.0)
    }
}

impl std::error::Error for ResumeFailed {}

#[derive(Debug)]
pub enum CompactError {
    /// 会话正在运行（HTTP 409）
    Busy,
    /// 摘要调用失败 / 被中止（HTTP 502）
    Failed(String),
}

impl std::fmt::Display for CompactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("session is running; abort it or wait until it settles before compacting"),
            Self::Failed(e) => write!(f, "compaction failed: {e}"),
        }
    }
}

impl std::error::Error for CompactError {}

/// 一次压缩的结果（HTTP / RPC 返回）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionReport {
    pub compacted: bool,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub summary_chars: usize,
    pub first_kept_entry_id: Option<String>,
    pub duration_ms: u64,
    pub usage: Option<Usage>,
}

/// 一次压缩的输入：`context[cut.start..cut.cut]` 被摘要。
pub(crate) struct CompactionRun<'a> {
    pub reason: &'static str,
    pub context: &'a [Message],
    pub cut: CutPoint,
    pub tokens_before: u64,
    pub instructions: Option<&'a str>,
}

pub(crate) struct HooksInit {
    pub store: Option<Session>,
    pub dynamic: Arc<SessionDynamic>,
    pub settings: CompactionSettings,
    pub context_window: Option<u64>,
    pub overhead_tokens: u64,
    pub api: agent_ai::Api,
    pub llm_max_retries: Option<u32>,
    pub created_at: u64,
    pub usage_floor: usize,
    pub title: Option<String>,
}

pub(crate) struct SessionHooks {
    store: Mutex<Option<Session>>,
    dynamic: Arc<SessionDynamic>,
    pub(crate) settings: CompactionSettings,
    context_window: Option<u64>,
    /// 系统提示 + 工具声明的 token 估计（没有用量报告时加到启发式上）
    overhead_tokens: u64,
    api: agent_ai::Api,
    llm_max_retries: Option<u32>,
    /// 上下文中这个下标之前的助手用量已因压缩失效
    usage_floor: Mutex<usize>,
    auto_failed_at: Mutex<Option<Instant>>,
    created_at: u64,
    updated_at: AtomicU64,
    title: Mutex<Option<String>>,
    compaction_usage: Mutex<Usage>,
}

/// 系统提示 + 工具声明的 token 估计（chars/4）。
pub(crate) fn overhead_tokens(system_prompt: &str, tools: &[Arc<dyn AgentTool>]) -> u64 {
    let tool_chars: usize = tools.iter().map(|t| serde_json::to_string(&t.spec()).map(|s| s.len()).unwrap_or(0)).sum();
    cp::estimate_text_tokens(system_prompt) + (tool_chars as u64).div_ceil(4)
}

/// 用户消息（非压缩摘要）的标题预览：空白折叠，最多 80 字符。
pub(crate) fn title_of(message: &Message) -> Option<String> {
    let Message::User { content, .. } = message else { return None };
    if cp::summary_of(message).is_some() {
        return None;
    }
    let text = content.iter().filter_map(|c| if let Content::Text { text } = c { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join(" ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then(|| {
        let mut title: String = text.chars().take(TITLE_CHARS).collect();
        if text.chars().count() > TITLE_CHARS {
            title.push('…');
        }
        title
    })
}

fn add_usage(total: &mut Usage, u: &Usage) {
    total.input += u.input;
    total.output += u.output;
    total.cache_read += u.cache_read;
    total.cache_write += u.cache_write;
    total.reasoning += u.reasoning;
    total.total_tokens += u.total_tokens;
}

fn compaction_event(reason: &str, phase: &str, tokens_before: u64) -> AgentEvent {
    AgentEvent::Compaction {
        reason: reason.into(),
        phase: phase.into(),
        tokens_before,
        tokens_after: None,
        duration_ms: None,
        summary_chars: None,
        first_kept_entry_id: None,
        usage: None,
        error: None,
    }
}

impl SessionHooks {
    pub(crate) fn new(i: HooksInit) -> Self {
        Self {
            store: Mutex::new(i.store),
            dynamic: i.dynamic,
            settings: i.settings,
            context_window: i.context_window,
            overhead_tokens: i.overhead_tokens,
            api: i.api,
            llm_max_retries: i.llm_max_retries,
            usage_floor: Mutex::new(i.usage_floor),
            auto_failed_at: Mutex::new(None),
            created_at: i.created_at,
            updated_at: AtomicU64::new(unix_ms()),
            title: Mutex::new(i.title),
            compaction_usage: Mutex::new(Usage::default()),
        }
    }

    /// JSONL 文件路径（未持久化 ⇒ None）
    pub fn path(&self) -> Option<PathBuf> {
        self.store.lock().unwrap().as_ref().map(|s| s.path.clone())
    }

    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    pub fn updated_at(&self) -> u64 {
        self.updated_at.load(Ordering::Relaxed)
    }

    pub fn title(&self) -> Option<String> {
        self.title.lock().unwrap().clone()
    }

    /// 本会话所有压缩调用累计的用量
    pub fn compaction_usage(&self) -> Usage {
        self.compaction_usage.lock().unwrap().clone()
    }

    /// 停止持久化（purge 之前），返回文件路径；之后的消息不会再写盘（也不会重新创建文件）。
    pub fn detach(&self) -> Option<PathBuf> {
        self.store.lock().unwrap().take().map(|s| s.path)
    }

    fn report(&self, severity: Severity, kind: &str, message: &str, details: Value) {
        let scope = self.dynamic.issue_scope.lock().unwrap().clone();
        self.dynamic.issues.report(IssueEvent::new(severity, kind, "session", message).scope(scope).details(details));
    }

    fn persist_failed(&self, what: &str, e: &anyhow::Error) {
        tracing::error!(issue = "typed", session_id = %self.dynamic.session_id, what, error = %e, "persist failed");
        self.report(Severity::Error, "session_persist_failed", "Session persistence failed", json!({"what": what}));
    }

    /// 把可变元数据（当前模型、推理强度、路由、标题）同步到 JSONL；没有变化时不写。
    pub fn sync_meta(&self) {
        let d = &self.dynamic;
        let patch = SessionMeta {
            model: Some(d.model.get()),
            requested_reasoning_effort: d.effort.lock().unwrap().0,
            reasoning_effort: d.reasoning.get(),
            routing: serde_json::to_value(&*d.routing.lock().unwrap()).ok(),
            title: self.title(),
            ..Default::default()
        };
        let mut store = self.store.lock().unwrap();
        if let Some(s) = store.as_mut() {
            match s.update_meta(&patch) {
                Ok(true) => tracing::debug!(session_id = %d.session_id, "session metadata persisted"),
                Ok(false) => {}
                Err(e) => {
                    drop(store);
                    self.persist_failed("metadata", &e);
                }
            }
        }
    }

    /// 当前模型的上下文窗口（profile `contextWindow` 优先）
    pub fn window(&self) -> u64 {
        self.context_window.unwrap_or_else(|| agent_ai::context_window(&self.dynamic.provider_id, &self.dynamic.model.get()))
    }

    pub fn estimate(&self, context: &[Message]) -> cp::ContextEstimate {
        cp::estimate_context_tokens(context, *self.usage_floor.lock().unwrap(), self.overhead_tokens)
    }

    /// 把压缩写入 JSONL：first_kept_entry_id 取保留部分第一条消息的 entry id。文件与内存上下文不一致时不写（warn）。
    fn persist_compaction(&self, context: &[Message], outcome: &Outcome, tokens_before: u64) -> Option<String> {
        let mut guard = self.store.lock().unwrap();
        let store = guard.as_mut()?;
        let ids = store.active_context();
        let in_sync = ids.len() == context.len() && ids.last().map(|(_, m)| m) == context.last();
        let first_kept = ids.get(outcome.cut.cut).and_then(|(id, _)| id.clone()).filter(|_| in_sync);
        let Some(first_kept) = first_kept else {
            drop(guard);
            tracing::warn!(issue = "typed", session_id = %self.dynamic.session_id, persisted = ids.len(), in_memory = context.len(), "session file is out of sync with the in-memory context; compaction applied in memory only");
            self.report(Severity::Warn, "session_persist_failed", "Compaction could not be persisted (session file out of sync)", json!({"what": "compaction"}));
            return None;
        };
        match store.append_compaction(outcome.summary.clone(), first_kept.clone(), Some(tokens_before), Some(outcome.tokens_after)) {
            Ok(_) => Some(first_kept),
            Err(e) => {
                drop(guard);
                self.persist_failed("compaction", &e);
                None
            }
        }
    }

    /// 执行一次压缩（auto / manual 共用）：事件、日志、issue、用量与持久化。成功返回新上下文。
    pub(crate) async fn run_compaction(&self, req: CompactionRun<'_>, emit: &Emit, cancel: CancellationToken) -> Result<(Vec<Message>, CompactionReport), String> {
        let CompactionRun { reason, context, cut, tokens_before, instructions } = req;
        let session_id = &self.dynamic.session_id;
        let started = Instant::now();
        emit(compaction_event(reason, "start", tokens_before));
        let model = self.settings.model.clone().unwrap_or_else(|| self.dynamic.model.get());
        tracing::info!(session_id = %session_id, reason, tokens_before, summarize_messages = cut.cut - cut.start, keep_messages = context.len() - cut.cut, split_turn = cut.split_turn, cumulative = cut.start == 1, model = %model, "compaction started");
        let call = SummaryCall {
            provider: &*self.dynamic.provider,
            model: &model,
            api: self.api,
            max_tokens: self.settings.max_summary_tokens,
            llm_max_retries: self.llm_max_retries,
            stream_idle_timeout: self.dynamic.llm.stream_idle_timeout_override,
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(SummaryError { message: "aborted".into(), usage: None }),
            r = crate::compaction::compact(&call, context, cut, self.overhead_tokens, instructions, cancel.clone()) => r,
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(outcome) => {
                let first_kept_entry_id = self.persist_compaction(context, &outcome, tokens_before);
                *self.usage_floor.lock().unwrap() = outcome.context.len();
                add_usage(&mut self.compaction_usage.lock().unwrap(), &outcome.usage);
                self.updated_at.store(unix_ms(), Ordering::Relaxed);
                let summary_chars = outcome.summary.chars().count();
                tracing::info!(session_id = %session_id, reason, tokens_before, tokens_after = outcome.tokens_after, summary_chars, duration_ms, input_tokens = outcome.usage.input, output_tokens = outcome.usage.output, first_kept_entry_id = first_kept_entry_id.as_deref().unwrap_or("-"), "context compacted");
                emit(AgentEvent::Compaction {
                    reason: reason.into(),
                    phase: "end".into(),
                    tokens_before,
                    tokens_after: Some(outcome.tokens_after),
                    duration_ms: Some(duration_ms),
                    summary_chars: Some(summary_chars),
                    first_kept_entry_id: first_kept_entry_id.clone(),
                    usage: Some(outcome.usage.clone()),
                    error: None,
                });
                let report = CompactionReport { compacted: true, tokens_before, tokens_after: outcome.tokens_after, summary_chars, first_kept_entry_id, duration_ms, usage: Some(outcome.usage) };
                Ok((outcome.context, report))
            }
            Err(e) => {
                if let Some(u) = &e.usage {
                    add_usage(&mut self.compaction_usage.lock().unwrap(), u);
                }
                if cancel.is_cancelled() {
                    tracing::info!(session_id = %session_id, reason, duration_ms, "compaction aborted");
                } else {
                    let summary = crate::hub::llm_error_summary(&e.message, &self.dynamic.issues);
                    tracing::warn!(issue = "typed", session_id = %session_id, reason, duration_ms, error = %summary, "compaction failed; continuing without compaction");
                    self.report(
                        Severity::Warn,
                        "compaction_failed",
                        "Context compaction failed; continuing without compaction",
                        json!({"reason": reason, "model": model, "tokensBefore": tokens_before, "error": summary}),
                    );
                }
                let mut event = compaction_event(reason, "end", tokens_before);
                if let AgentEvent::Compaction { duration_ms: d, usage, error, .. } = &mut event {
                    *d = Some(duration_ms);
                    *usage = e.usage.clone();
                    *error = Some(e.message.clone());
                }
                emit(event);
                Err(e.message)
            }
        }
    }
}

impl ContextHooks for SessionHooks {
    fn on_message(&self, message: &Message) {
        self.updated_at.store(unix_ms(), Ordering::Relaxed);
        let mut new_title = false;
        if let Some(t) = title_of(message) {
            let mut title = self.title.lock().unwrap();
            if title.is_none() {
                *title = Some(t);
                new_title = true;
            }
        }
        let mut store = self.store.lock().unwrap();
        if let Some(s) = store.as_mut() {
            if let Err(e) = s.append_message(message.clone()) {
                drop(store);
                self.persist_failed("message", &e);
                return;
            }
        }
        drop(store);
        if new_title || matches!(message, Message::Assistant(_)) {
            self.sync_meta();
        }
    }

    fn before_call(&self, context: Vec<Message>, emit: Emit, cancel: CancellationToken) -> futures::future::BoxFuture<'_, Option<Vec<Message>>> {
        Box::pin(async move {
            if !self.settings.enabled || self.auto_failed_at.lock().unwrap().is_some_and(|t| t.elapsed() < AUTO_RETRY_AFTER_FAILURE) {
                return None;
            }
            let window = self.window();
            let threshold = self.settings.threshold(window);
            let estimate = self.estimate(&context);
            if estimate.tokens <= threshold {
                tracing::debug!(session_id = %self.dynamic.session_id, tokens = estimate.tokens, from_usage = estimate.from_usage, threshold, window, "context below compaction threshold");
                return None;
            }
            let Some(cut) = cp::find_cut_point(&context, self.settings.keep_recent_tokens) else {
                tracing::debug!(session_id = %self.dynamic.session_id, tokens = estimate.tokens, threshold, "context above compaction threshold but nothing can be compacted");
                return None;
            };
            tracing::info!(session_id = %self.dynamic.session_id, tokens = estimate.tokens, from_usage = estimate.from_usage, threshold, window, "auto compaction triggered");
            let run = CompactionRun { reason: "auto", context: &context, cut, tokens_before: estimate.tokens, instructions: None };
            match self.run_compaction(run, &emit, cancel).await {
                Ok((compacted, _)) => {
                    *self.auto_failed_at.lock().unwrap() = None;
                    Some(compacted)
                }
                Err(_) => {
                    *self.auto_failed_at.lock().unwrap() = Some(Instant::now());
                    None
                }
            }
        })
    }
}

impl SessionHandle {
    /// 手动压缩（`POST /v1/sessions/{id}/compact`、RPC `compact`）。运行中 ⇒ Busy；无可压缩内容 ⇒ `compacted: false`。
    /// 期间会话视为运行中：新消息排队，结束后按顺序重新提交；abort 取消摘要调用。
    pub async fn compact(self: &Arc<Self>, instructions: Option<String>) -> Result<CompactionReport, CompactError> {
        let guard = self.agent.try_exclusive().ok_or(CompactError::Busy)?;
        let this = self.clone();
        // 独立任务：HTTP 客户端断开不会中途丢下独占状态
        let task = tokio::spawn(tracing::Instrument::in_current_span(async move {
            let context = this.agent.messages().await;
            let estimate = this.hooks.estimate(&context);
            let result = match cp::find_cut_point(&context, this.hooks.settings.keep_recent_tokens) {
                None => {
                    tracing::info!(session_id = %this.id, tokens = estimate.tokens, messages = context.len(), "manual compaction: nothing to compact");
                    Ok(CompactionReport { compacted: false, tokens_before: estimate.tokens, tokens_after: estimate.tokens, summary_chars: 0, first_kept_entry_id: None, duration_ms: 0, usage: None })
                }
                Some(cut) => {
                    let agent = this.agent.clone();
                    let emit: Emit = Arc::new(move |e| agent.emit(e));
                    let run = CompactionRun { reason: "manual", context: &context, cut, tokens_before: estimate.tokens, instructions: instructions.as_deref() };
                    match this.hooks.run_compaction(run, &emit, guard.token.clone()).await {
                        Ok((compacted, report)) => {
                            this.agent.replace_messages(compacted);
                            this.hooks.sync_meta();
                            Ok(report)
                        }
                        Err(e) => Err(CompactError::Failed(e)),
                    }
                }
            };
            *this.last_activity.lock().unwrap() = Instant::now();
            for (message, mode) in guard.release() {
                this.submit_message(message, Some(mode));
            }
            result
        }));
        task.await.unwrap_or_else(|_| Err(CompactError::Failed("internal error".into())))
    }
}

/// 恢复时的路由：元数据里的路由；旧文件/崩溃前未写入时，若历史里已有助手消息，则沿用最后一次使用的模型。
fn restored_routing(meta: &SessionMeta, history: &[Message]) -> Option<Routing> {
    let routing = meta.routing.clone().and_then(|v| serde_json::from_value::<Routing>(v).ok());
    if routing.as_ref().is_some_and(|r| r.decided) {
        return routing;
    }
    let model = history.iter().rev().find_map(|m| if let Message::Assistant(a) = m { Some(a.model.clone()) } else { None })?;
    let mut r = routing.unwrap_or(Routing {
        depth: Default::default(),
        model: model.clone(),
        base_model: meta.model.clone().unwrap_or_else(|| model.clone()),
        deep_model: None,
        auto: false,
        reason: String::new(),
        usage: None,
        decided: true,
        explicit: false,
    });
    r.model = model;
    r.decided = true;
    r.reason = "restored from session history".into();
    Some(r)
}

/// 删除结果
pub struct DeleteOutcome {
    pub in_memory: bool,
    pub purged: bool,
}

impl Hub {
    /// client 的会话目录（未持久化 ⇒ None）
    pub(crate) fn client_session_dir(&self, client: &str) -> Option<PathBuf> {
        self.session_dir.as_ref().map(|d| d.join(client))
    }

    /// 内存里的会话；没有时从该 client 的会话目录恢复。`Ok(None)`：未知 id、非法 id、或属于其它 client（一律 404）。
    pub async fn get_or_load(self: &Arc<Self>, client: &Client, id: &str) -> anyhow::Result<Option<Arc<SessionHandle>>> {
        if let Some(h) = self.get(client, id) {
            return Ok(Some(h));
        }
        // 关机中不再加载（与内存会话已清空时一样返回 404）
        if self.session_dir.is_none() || self.is_shutting_down() || !agent_session::valid_id(id) || self.sessions.lock().unwrap().contains_key(id) {
            return Ok(None);
        }
        let (hub, client, id) = (self.clone(), client.clone(), id.to_string());
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || span.in_scope(|| hub.load_persisted(&client, &id))).await.map_err(|_| anyhow::anyhow!("session load task failed"))?
    }

    /// 同步恢复（`run --resume` / `rpc --resume` 与 [`Self::get_or_load`]）。
    pub fn load_persisted(&self, client: &Client, id: &str) -> anyhow::Result<Option<Arc<SessionHandle>>> {
        if let Some(h) = self.get(client, id) {
            return Ok(Some(h));
        }
        let Some(dir) = self.client_session_dir(&client.name) else { return Ok(None) };
        let Some(path) = agent_session::find(&dir, id) else { return Ok(None) };
        let started = Instant::now();
        let mut store = Session::open(&path).map_err(|e| ResumeFailed(format!("cannot read session file: {e}")))?;
        if store.torn_tail {
            tracing::warn!(issue = "typed", session_id = %id, "session file ended with an incomplete line (crash during write); truncated it");
        }
        let repaired = store.repair_interrupted_tools().map_err(|e| ResumeFailed(format!("cannot repair session file: {e}")))?;
        let meta = store.meta().clone();
        let history = store.active_messages();
        let compacted = store.is_compacted();
        let prompted = history.iter().any(|m| matches!(m, Message::User { .. }));
        let routing = restored_routing(&meta, &history);
        let created_at = agent_session::timestamp_ms(store.created_at()).unwrap_or_else(unix_ms);
        // 会话级审批策略随会话持久化：恢复后与当前 profile 策略叠加（不能因为重启而放宽）
        let tool_approval = match meta.tool_approval.clone() {
            Some(v) => {
                let policy = serde_json::from_value::<agent_tools::ToolApproval>(v).map_err(|e| ResumeFailed(format!("invalid toolApproval in session file: {e}")))?;
                policy.validate().map_err(|e| ResumeFailed(format!("invalid toolApproval in session file: {e}")))?;
                Some(policy)
            }
            None => None,
        };
        let req = SessionRequest {
            provider: meta.provider.clone(),
            profile: Some(store.profile().to_string()),
            cwd: meta.cwd.clone(),
            system_prompt: meta.system_prompt.clone(),
            append_system_prompt: meta.append_system_prompt.clone(),
            model: if meta.explicit_model == Some(true) { meta.model.clone() } else { None },
            reasoning_effort: meta.requested_reasoning_effort,
            depth: routing.as_ref().map(|r| r.depth),
            api: meta.api.as_deref().and_then(|a| a.parse().ok()),
            account: meta.account.clone(),
            tools: meta.tools.clone(),
            caller_tools: vec![],
            tool_approval,
            max_turns: meta.max_turns,
            on_max_turns: meta.on_max_turns.as_deref().map(str::parse).transpose().map_err(|e: String| ResumeFailed(format!("invalid onMaxTurns in session file: {e}")))?,
            max_turns_ask_timeout_secs: meta.max_turns_ask_timeout_secs,
            stream_idle_timeout_secs: meta.stream_idle_timeout_secs,
            llm_max_retries: meta.llm_max_retries,
        };
        if let Some(names) = meta.caller_tools.as_ref().filter(|n| !n.is_empty()) {
            tracing::warn!(issue = "typed", session_id = %id, caller_tools = ?names, "caller tools are not restored (callback URLs and tokens are not persisted)");
        }
        let messages = history.len();
        let h = self.create_inner(client, req, meta.ephemeral.unwrap_or(false), Some(Restore { store, history, routing, prompted, compacted, created_at })).map_err(|e| {
            if e.is::<TooManySessions>() {
                e
            } else {
                ResumeFailed(e.to_string()).into()
            }
        })?;
        if !repaired.is_empty() {
            tracing::warn!(issue = "typed", session_id = %id, tool_call_ids = ?repaired, "interrupted tool calls repaired with synthetic error results");
            h.report_issue(Severity::Warn, "tool_interrupted", "session", "Tool calls interrupted by a restart were closed with error results", json!({"toolCalls": repaired.len()}));
        }
        tracing::info!(session_id = %id, bot = %client.name, messages, compacted, repaired_tool_calls = repaired.len(), elapsed_ms = started.elapsed().as_millis() as u64, "session rehydrated from disk");
        Ok(Some(h))
    }

    /// 调用方的会话：内存 + 磁盘（按 id 去重，内存优先），按 updatedAt 倒序，最多 `limit` 条。
    /// 排序键就是返回的 `updatedAt`：内存会话取一次快照，磁盘会话取 max(最后条目时间戳, 文件 mtime)。
    pub async fn list_sessions(&self, client: &Client, limit: usize) -> Vec<Value> {
        let memory: Vec<Arc<SessionHandle>> = self.sessions.lock().unwrap().values().filter(|h| h.owner == client.name).cloned().collect();
        let mut rows: Vec<(u64, Result<Arc<SessionHandle>, agent_session::SessionFile>)> = memory.iter().map(|h| (h.hooks.updated_at(), Ok(h.clone()))).collect();
        if let Some(dir) = self.client_session_dir(&client.name) {
            let loaded: HashSet<String> = memory.iter().map(|h| h.id.clone()).collect();
            let files = tokio::task::spawn_blocking(move || {
                agent_session::list_files(&dir)
                    .into_iter()
                    .filter(|f| !loaded.contains(&f.id))
                    .map(|f| {
                        // 文件 mtime 的精度/时钟可能落后于条目时间戳（同一时钟节拍内写入的文件 mtime 相同）：取两者较大值，
                        // 并且在排序前就确定，保证列表顺序与返回的 updatedAt 一致
                        let modified = f.modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                        (agent_session::last_timestamp_ms(&f.path).unwrap_or(0).max(modified), f)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            rows.extend(files.into_iter().map(|(t, f)| (t, Err(f))));
        }
        rows.sort_by_key(|row| std::cmp::Reverse(row.0));
        rows.truncate(limit);
        let disk: Vec<(u64, agent_session::SessionFile)> = rows.iter().filter_map(|(t, r)| r.as_ref().err().map(|f| (*t, f.clone()))).collect();
        let mut views: HashMap<String, Value> = tokio::task::spawn_blocking(move || disk.into_iter().filter_map(|(t, f)| disk_view(&f, t).map(|v| (f.id, v))).collect()).await.unwrap_or_default();
        let mut out = vec![];
        for (updated_at, row) in rows {
            match row {
                Ok(h) => out.push(json!({
                    "sessionId": h.id, "profile": h.profile, "provider": h.provider_id(), "account": h.account, "model": h.model(), "cwd": h.cwd,
                    "createdAt": h.hooks.created_at(), "updatedAt": updated_at, "messageCount": h.agent.messages().await.len(),
                    "inMemory": true, "running": h.agent.is_running(), "persisted": h.hooks.path().is_some(), "ephemeral": h.ephemeral, "title": h.hooks.title(),
                })),
                Err(f) => out.extend(views.remove(&f.id)),
            }
        }
        out
    }

    /// 删除会话：内存里的会话被移除并中止（JSONL 保留）；`purge` 时同时删除 JSONL。
    /// None：该 client 既没有这个内存会话、也没有这个会话文件（404）。
    pub fn delete_session(&self, client: &Client, id: &str, purge: bool) -> anyhow::Result<Option<DeleteOutcome>> {
        let removed = self.remove_handle(client, id);
        let file = match &removed {
            Some(h) if purge => h.hooks.detach(),
            Some(h) => h.hooks.path(),
            None => self.client_session_dir(&client.name).and_then(|d| agent_session::find(&d, id)),
        };
        if removed.is_none() && file.is_none() {
            return Ok(None);
        }
        let mut purged = false;
        if purge {
            if let Some(path) = &file {
                std::fs::remove_file(path)?;
                purged = true;
                tracing::info!(session_id = %id, bot = %client.name, "session JSONL purged");
            }
        }
        Ok(Some(DeleteOutcome { in_memory: removed.is_some(), purged }))
    }
}

/// 磁盘会话的列表项（只读解析；损坏文件跳过并 warn）。`updated_at` 是排序时用的键，原样返回。
fn disk_view(f: &agent_session::SessionFile, updated_at: u64) -> Option<Value> {
    let s = match Session::read(&f.path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(session_id = %f.id, error = %e, "unreadable session file skipped in listing");
            return None;
        }
    };
    let meta = s.meta();
    let messages = s.active_messages();
    let model = messages.iter().rev().find_map(|m| if let Message::Assistant(a) = m { Some(a.model.clone()) } else { None }).or_else(|| meta.model.clone());
    let title = meta.title.clone().or_else(|| messages.iter().find_map(title_of));
    Some(json!({
        "sessionId": s.id, "profile": s.profile(), "provider": meta.provider, "account": meta.account, "model": model, "cwd": meta.cwd,
        "createdAt": agent_session::timestamp_ms(s.created_at()), "updatedAt": updated_at, "messageCount": messages.len(),
        "inMemory": false, "running": false, "persisted": true, "ephemeral": meta.ephemeral.unwrap_or(false), "title": title,
    }))
}
