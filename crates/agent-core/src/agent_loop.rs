use crate::{
    event::AgentEvent,
    tool::*,
    turn_budget::{LastRun, OnMaxTurns, Resolution},
};
use agent_ai::*;
use futures::{future::join_all, StreamExt};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

pub type Emit = Arc<dyn Fn(AgentEvent) + Send + Sync>;
pub type Queue = Arc<Mutex<VecDeque<Message>>>;
/// 每次 run 开始前（第一次模型调用之前）调用，参数是本次 run 的初始消息；可用于异步路由模型等。
pub type BeforeRun = Arc<dyn Fn(Vec<Message>, Emit) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// 会话层对上下文的钩子：逐条持久化与模型调用前的压缩（对应 pi AgentSession 的 session 写入与自动压缩）。
pub trait ContextHooks: Send + Sync {
    /// 每条消息加入上下文后同步调用（用户消息、助手消息、工具结果），用于增量持久化。
    fn on_message(&self, _message: &Message) {}
    /// 每次模型调用之前调用，参数是当前上下文快照；返回 Some 时用它替换上下文（压缩）。
    /// 取消时应尽快返回 None；返回 None 表示不修改。
    fn before_call(&self, _context: Vec<Message>, _emit: Emit, _cancel: CancellationToken) -> futures::future::BoxFuture<'_, Option<Vec<Message>>> {
        Box::pin(async { None })
    }
}

/// 运行期可修改的共享值：每次模型调用时读取快照，修改从下一次调用生效（包括正在进行的 run）。
pub struct Shared<T>(Arc<Mutex<T>>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Default> Default for Shared<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.lock().unwrap().fmt(f)
    }
}

impl<T> Shared<T> {
    pub fn new(value: T) -> Self {
        Self(Arc::new(Mutex::new(value)))
    }
    /// 替换并返回旧值。
    pub fn set(&self, value: T) -> T {
        std::mem::replace(&mut *self.0.lock().unwrap(), value)
    }
}

impl<T: Clone> Shared<T> {
    pub fn get(&self) -> T {
        self.0.lock().unwrap().clone()
    }
}

impl<T> From<T> for Shared<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl From<&str> for Shared<String> {
    fn from(value: &str) -> Self {
        Self::new(value.to_string())
    }
}

#[derive(Clone)]
pub struct LoopConfig {
    pub provider: Arc<dyn Provider>,
    /// 运行期可修改（路由）；每次模型调用读取一次。
    pub model: Shared<String>,
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// 运行期可修改（动态推理强度）；每次模型调用读取一次。
    pub reasoning: Shared<Option<ThinkingLevel>>,
    pub api: agent_ai::Api,
    /// 每轮结束后注入（对应 `getSteeringMessages`）。
    pub steering: Queue,
    /// agent 将要停止时注入（对应 `getFollowUpMessages`）。
    pub follow_up: Queue,
    /// 单次 run 的模型调用上限（回合预算）；运行期可由调用方修改，每次模型调用前读取。
    pub max_turns: Shared<u64>,
    /// 达到 max_turns 时的处理（stop / summarize / ask）与运行期状态；调用方通过同一个 Arc 调整 / 停止。
    pub turn_gate: Arc<crate::turn_budget::TurnGate>,
    /// 每次模型调用的输出 token 上限（含推理 token）
    pub max_tokens: Option<u32>,
    /// 瞬态错误的额外重试次数（重试不消耗回合）；None = provider 默认（3）。
    pub llm_max_retries: Option<u32>,
    /// 覆盖 provider 实例的流空闲超时（会话 / profile 级）；None = provider 实例默认（账号 > provider > 全局）。
    pub stream_idle_timeout: Option<std::time::Duration>,
    /// 单次运行的总 token 预算：达到后再给模型一次“不带工具、直接给出最终答复”的机会，然后停止
    pub token_budget: Option<u64>,
    /// 单次运行的时间上限；None 表示不限时。
    pub run_timeout: Option<std::time::Duration>,
    /// 见 [`BeforeRun`]；None 时不调用。
    pub before_run: Option<BeforeRun>,
    /// 工具调用审批；None 表示不审批（全部直接执行）。
    pub approval: Option<Arc<crate::approval::ApprovalGate>>,
    /// 见 [`ContextHooks`]；None 时不持久化、不压缩。
    pub hooks: Option<Arc<dyn ContextHooks>>,
}

/// 把消息加入上下文并通知钩子（持久化）。
fn push(context: &Mutex<Vec<Message>>, cfg: &LoopConfig, m: &Message) {
    context.lock().unwrap().push(m.clone());
    if let Some(hooks) = &cfg.hooks {
        hooks.on_message(m);
    }
}

fn drain(q: &Queue) -> Vec<Message> {
    q.lock().unwrap().drain(..).collect()
}

/// 对应 `agent-loop.ts::runLoop`：外层循环处理 follow-up，内层循环处理工具调用与 steering。
/// `context` 是随消息追加更新的快照；只在 clone/push 时短暂持锁。
pub async fn run_loop(context: &Mutex<Vec<Message>>, prompts: Vec<Message>, cfg: &LoopConfig, cancel: CancellationToken, emit: Emit) -> Vec<Message> {
    let mut new_messages = vec![];
    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);
    let mut pending = prompts;
    pending.extend(drain(&cfg.steering));
    let mut turns = 0u64;
    let mut first = true;
    let mut used_tokens = 0u64;
    // 最后一次调用已强制（token 预算或 summarize）：本轮结束后停止
    let mut budget_forced = false;
    let gate = &cfg.turn_gate;
    let _running = gate.begin();
    // 计入预算的模型调用数（summarize 的总结调用不计入）
    let mut turns_used = 0u64;
    let mut limit_reached = false;
    let mut low_fired = false;
    let mut seen_changes = gate.changes();

    'outer: loop {
        let mut more_tool_calls = true;
        while more_tool_calls || !pending.is_empty() {
            if !first {
                emit(AgentEvent::TurnStart);
            }
            first = false;
            for m in pending.drain(..) {
                emit(AgentEvent::MessageStart { message: m.clone() });
                emit(AgentEvent::MessageEnd { message: m.clone() });
                push(context, cfg, &m);
                new_messages.push(m);
            }

            turns += 1;
            let over_budget = cfg.token_budget.is_some_and(|b| used_tokens >= b);
            let mut max_turns = cfg.max_turns.get();
            let mut turn_limit = None;
            if turns > max_turns {
                let used = turns - 1;
                turn_limit = match gate.on_max() {
                    OnMaxTurns::Ask => match gate.wait(&cfg.max_turns, used, &cancel, &emit).await {
                        Resolution::Extended(n) => {
                            max_turns = n;
                            None
                        }
                        Resolution::Stopped | Resolution::Timeout => Some(TurnLimit::Stop),
                        Resolution::Cancelled => Some(TurnLimit::Cancelled),
                    },
                    action => {
                        tracing::info!(turns_used = used, max_turns, action = action.as_str(), "turn budget exhausted");
                        emit(AgentEvent::TurnBudgetExhausted { turns_used: used, max_turns, action, timeout_secs: None, expires_at: None });
                        Some(if action == OnMaxTurns::Summarize { TurnLimit::Summarize } else { TurnLimit::Stop })
                    }
                };
            }
            if turn_limit.is_none() {
                // 预算被修改后重新武装低预算提醒
                let changes = gate.changes();
                if changes != seen_changes {
                    seen_changes = changes;
                    low_fired = false;
                }
                let used = turns - 1;
                if !low_fired && max_turns > used && used >= crate::turn_budget::low_threshold(max_turns) {
                    low_fired = true;
                    let remaining = max_turns - used;
                    tracing::info!(turns_used = used, max_turns, remaining, "turn budget low");
                    emit(AgentEvent::TurnBudgetLow { turns_used: used, max_turns, remaining });
                }
            }
            let assistant = if let Some(limit) = turn_limit {
                let used = turns - 1;
                let model = cfg.model.get();
                match limit {
                    TurnLimit::Cancelled => AssistantMessage::failed(&model, StopReason::Aborted, "aborted"),
                    TurnLimit::Stop => {
                        limit_reached = true;
                        emit(AgentEvent::RunOutcome { kind: "max_turns".into() });
                        AssistantMessage::failed(&model, StopReason::Error, format!("max_turns reached ({used}/{} turns used)", cfg.max_turns.get()))
                    }
                    TurnLimit::Summarize => {
                        limit_reached = true;
                        budget_forced = true;
                        emit(AgentEvent::RunOutcome { kind: "max_turns".into() });
                        let m = Message::user_text("[sub-bot] Turn limit reached. Do not call tools; summarize progress, what remains, and next steps.");
                        emit(AgentEvent::MessageStart { message: m.clone() });
                        emit(AgentEvent::MessageEnd { message: m.clone() });
                        push(context, cfg, &m);
                        new_messages.push(m);
                        stream_assistant(context, cfg, cancel.clone(), &emit, false).await
                    }
                }
            } else if over_budget && budget_forced {
                AssistantMessage::failed(&cfg.model.get(), StopReason::Error, "token budget exceeded")
            } else if over_budget {
                budget_forced = true;
                emit(AgentEvent::RunOutcome { kind: "token_budget_exceeded".into() });
                tracing::warn!(issue = "typed", used_tokens, budget = cfg.token_budget, "token budget reached; asking for a final answer without tools");
                let m = Message::user_text("[sub-bot] Token budget reached. Do not call any more tools; give your final answer now based on what you have.");
                emit(AgentEvent::MessageStart { message: m.clone() });
                emit(AgentEvent::MessageEnd { message: m.clone() });
                push(context, cfg, &m);
                new_messages.push(m);
                stream_assistant(context, cfg, cancel.clone(), &emit, false).await
            } else {
                stream_assistant(context, cfg, cancel.clone(), &emit, true).await
            };
            if turn_limit.is_none() {
                turns_used = turns;
                gate.set_used(turns_used);
            }
            used_tokens += assistant.usage.total_tokens;
            let am = Message::Assistant(assistant.clone());
            push(context, cfg, &am);
            new_messages.push(am.clone());

            if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) && assistant.tool_calls().is_empty() {
                emit(AgentEvent::TurnEnd { message: am, tool_results: vec![] });
                break 'outer;
            }

            let calls: Vec<ToolCall> = assistant.tool_calls().into_iter().cloned().collect();
            let mut results = vec![];
            more_tool_calls = false;
            if !calls.is_empty() {
                let (msgs, terminate) = if cancel.is_cancelled() || matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                    (calls.iter().map(|c| tool_result_msg(c, &ToolResult::error("aborted before execution"))).collect(), true)
                } else if assistant.stop_reason == StopReason::Length {
                    // 被截断的参数不可信，全部判失败（对应 failToolCallsFromTruncatedMessage）
                    (calls.iter().map(|c| tool_result_msg(c, &ToolResult::error("tool call truncated by length limit"))).collect(), false)
                } else {
                    execute_tool_calls(&calls, cfg, cancel.clone(), &emit).await
                };
                more_tool_calls = !terminate;
                for r in &msgs {
                    let m = Message::ToolResult(r.clone());
                    emit(AgentEvent::MessageStart { message: m.clone() });
                    emit(AgentEvent::MessageEnd { message: m.clone() });
                    push(context, cfg, &m);
                    new_messages.push(m);
                }
                results = msgs;
            }
            emit(AgentEvent::TurnEnd { message: am, tool_results: results });
            if budget_forced {
                break 'outer;
            }
            if cancel.is_cancelled() || matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                break 'outer;
            }
            pending = drain(&cfg.steering);
        }
        let follow = drain(&cfg.follow_up);
        if follow.is_empty() {
            break;
        }
        pending = follow;
    }
    gate.finish(LastRun { turns_used, max_turns: cfg.max_turns.get(), limit_reached });
    emit(AgentEvent::AgentEnd { messages: new_messages.clone() });
    new_messages
}

/// 达到回合上限后的本轮处理。
#[derive(Clone, Copy)]
enum TurnLimit {
    Stop,
    Summarize,
    Cancelled,
}

async fn stream_assistant(context: &Mutex<Vec<Message>>, cfg: &LoopConfig, cancel: CancellationToken, emit: &Emit, with_tools: bool) -> AssistantMessage {
    if let Some(hooks) = &cfg.hooks {
        let snapshot = context.lock().unwrap().clone();
        let replaced = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            r = hooks.before_call(snapshot, emit.clone(), cancel.clone()) => r,
        };
        if let Some(compacted) = replaced {
            *context.lock().unwrap() = compacted;
        }
    }
    let model = cfg.model.get();
    let req = Request {
        model: model.clone(),
        system_prompt: cfg.system_prompt.clone(),
        messages: context.lock().unwrap().clone(),
        tools: if with_tools { cfg.tools.iter().map(|t| t.spec()).collect() } else { vec![] },
        reasoning: cfg.reasoning.get(),
        raw_effort: false,
        thinking: None,
        api: cfg.api,
        max_tokens: cfg.max_tokens,
        llm_max_retries: cfg.llm_max_retries,
        stream_idle_timeout: cfg.stream_idle_timeout,
    };
    emit(AgentEvent::MessageStart { message: Message::Assistant(AssistantMessage::empty(&model)) });
    let mut stream = cfg.provider.stream(req, cancel.clone());
    let mut final_msg = None;
    loop {
        let ev = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let message = AssistantMessage::failed(&model, StopReason::Aborted, "aborted");
                emit(AgentEvent::MessageUpdate { assistant_message_event: AssistantEvent::Error { message: message.clone() } });
                final_msg = Some(message);
                break;
            }
            ev = stream.next() => match ev {
                Some(ev) => ev,
                None => break,
            },
        };
        match &ev {
            AssistantEvent::Retry { attempt, reason, max_retries, delay_ms, kind, discarded_tool_calls } => {
                // 同一次模型调用的重放：不消耗回合，也不再触发 before_call（压缩）。
                emit(AgentEvent::MessageRetry {
                    attempt: *attempt,
                    reason: reason.clone(),
                    max_retries: *max_retries,
                    delay_ms: *delay_ms,
                    kind: kind.clone(),
                    discarded_tool_calls: *discarded_tool_calls,
                });
                continue;
            }
            AssistantEvent::Done { message } | AssistantEvent::Error { message } => final_msg = Some(message.clone()),
            _ => {}
        }
        emit(AgentEvent::MessageUpdate { assistant_message_event: ev });
    }
    let msg = final_msg.unwrap_or_else(|| AssistantMessage::failed(&model, StopReason::Error, "provider stream ended without done/error"));
    emit(AgentEvent::MessageEnd { message: Message::Assistant(msg.clone()) });
    msg
}

fn tool_result_msg(call: &ToolCall, r: &ToolResult) -> ToolResultMessage {
    ToolResultMessage { tool_call_id: call.id.clone(), tool_name: call.name.clone(), content: r.content.clone(), is_error: r.is_error, timestamp: now_ms() }
}

async fn run_one(call: ToolCall, cfg: &LoopConfig, cancel: CancellationToken, emit: Emit) -> ToolResult {
    // 即使并行 future 已构造，轮到 poll 时也可能已取消。
    if cancel.is_cancelled() {
        return ToolResult::error("aborted before execution");
    }
    use tracing::Instrument;
    let span = tracing::info_span!("tool", tool = %call.name, tool_call_id = %call.id);
    async move {
        // 只审批存在的工具；未知工具照常返回 not found。
        if let Some(gate) = cfg.approval.as_ref().filter(|_| cfg.tools.iter().any(|t| t.name() == call.name)) {
            if let Err(text) = gate.check(&call, &cancel, &emit).await {
                return ToolResult::error(text);
            }
            if cancel.is_cancelled() {
                return ToolResult::error("aborted before execution");
            }
        }
        run_one_inner(call, cfg, cancel, emit).await
    }
    .instrument(span)
    .await
}

async fn run_one_inner(call: ToolCall, cfg: &LoopConfig, cancel: CancellationToken, emit: Emit) -> ToolResult {
    let started = std::time::Instant::now();
    // 只记参数大小，不记原文（可能含文件内容或敏感信息）
    tracing::debug!(args_bytes = call.arguments.to_string().len(), "tool started");
    emit(AgentEvent::ToolExecutionStart { tool_call_id: call.id.clone(), tool_name: call.name.clone(), args: call.arguments.clone() });
    let result = match cfg.tools.iter().find(|t| t.name() == call.name) {
        None => ToolResult::error(format!("Tool {} not found", call.name)),
        Some(tool) => {
            let (id, name, e2) = (call.id.clone(), call.name.clone(), emit.clone());
            let sink: UpdateSink = Arc::new(move |p| e2(AgentEvent::ToolExecutionUpdate { tool_call_id: id.clone(), tool_name: name.clone(), partial_result: p }));
            match tool.execute(&call.id, call.arguments.clone(), cancel, sink).await {
                Ok(r) => r,
                Err(e) => ToolResult::error(e.to_string()),
            }
        }
    };
    let latency_ms = started.elapsed().as_millis() as u64;
    if result.is_error {
        tracing::warn!(issue = "typed", latency_ms, "tool failed");
    } else {
        tracing::info!(latency_ms, "tool finished");
    }
    emit(AgentEvent::ToolExecutionEnd { tool_call_id: call.id.clone(), tool_name: call.name.clone(), result: result.clone(), is_error: result.is_error });
    result
}

/// 对应 executeToolCalls：全部可并行则并发执行，否则顺序执行；结果按模型给出的顺序返回。
async fn execute_tool_calls(calls: &[ToolCall], cfg: &LoopConfig, cancel: CancellationToken, emit: &Emit) -> (Vec<ToolResultMessage>, bool) {
    if cancel.is_cancelled() {
        return (calls.iter().map(|c| tool_result_msg(c, &ToolResult::error("aborted before execution"))).collect(), false);
    }
    let all_parallel = calls.iter().all(|c| cfg.tools.iter().find(|t| t.name() == c.name).is_none_or(|t| t.execution_mode() == ExecutionMode::Parallel));
    let results: Vec<ToolResult> = if all_parallel {
        join_all(calls.iter().map(|c| async {
            if cancel.is_cancelled() {
                ToolResult::error("aborted before execution")
            } else {
                run_one(c.clone(), cfg, cancel.clone(), emit.clone()).await
            }
        }))
        .await
    } else {
        let mut v = vec![];
        for c in calls {
            v.push(if cancel.is_cancelled() { ToolResult::error("aborted before execution") } else { run_one(c.clone(), cfg, cancel.clone(), emit.clone()).await });
        }
        v
    };
    let terminate = !results.is_empty() && results.iter().all(|r| r.terminate);
    (calls.iter().zip(&results).map(|(c, r)| tool_result_msg(c, r)).collect(), terminate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 第一次返回一个工具调用，第二次返回文本。
    struct Scripted(AtomicUsize);
    impl Provider for Scripted {
        fn id(&self) -> &str {
            "scripted"
        }
        fn stream(&self, req: Request, _c: CancellationToken) -> EventStream {
            let mut m = AssistantMessage::empty(&req.model);
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                m.content.push(Content::ToolCall(ToolCall { id: "c1".into(), name: "echo".into(), arguments: serde_json::json!({"x": 1}) }));
                m.stop_reason = StopReason::ToolUse;
            } else {
                m.content.push(Content::Text { text: "done".into() });
            }
            Box::pin(futures::stream::iter(vec![AssistantEvent::Done { message: m }]))
        }
    }

    struct Echo;
    #[async_trait::async_trait]
    impl AgentTool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _: &str, a: serde_json::Value, _: CancellationToken, _: UpdateSink) -> anyhow::Result<ToolResult> {
            Ok(ToolResult::text(a.to_string()))
        }
    }

    #[tokio::test]
    async fn tool_then_text() {
        let cfg = LoopConfig {
            provider: Arc::new(Scripted(AtomicUsize::new(0))),
            model: "m".into(),
            system_prompt: None,
            tools: vec![Arc::new(Echo)],
            reasoning: Default::default(),
            api: Default::default(),
            steering: Default::default(),
            follow_up: Default::default(),
            max_turns: 5.into(),
            turn_gate: Default::default(),
            max_tokens: None,
            llm_max_retries: None,
            stream_idle_timeout: None,
            token_budget: None,
            run_timeout: None,
            before_run: None,
            approval: None,
            hooks: None,
        };
        let events = Arc::new(Mutex::new(vec![]));
        let ev = events.clone();
        let emit: Emit = Arc::new(move |e| ev.lock().unwrap().push(serde_json::to_value(&e).unwrap()["type"].as_str().unwrap().to_string()));
        let ctx = Mutex::new(vec![]);
        let new = run_loop(&ctx, vec![Message::user_text("hi")], &cfg, CancellationToken::new(), emit).await;
        assert_eq!(new.len(), 4); // user, assistant(toolCall), toolResult, assistant(text)
        let ev = events.lock().unwrap();
        assert_eq!(ev.iter().filter(|t| *t == "turn_start").count(), 2);
        assert!(ev.contains(&"tool_execution_end".to_string()));
        assert_eq!(ev.last().unwrap(), "agent_end");
    }

    /// 只要给了工具就一直调用工具，每次消耗 100 token；没给工具时回答文本。
    struct Greedy;
    impl Provider for Greedy {
        fn id(&self) -> &str {
            "greedy"
        }
        fn stream(&self, req: Request, _c: CancellationToken) -> EventStream {
            let mut m = AssistantMessage::empty(&req.model);
            m.usage.total_tokens = 100;
            if req.tools.is_empty() {
                m.content.push(Content::Text { text: "final".into() });
            } else {
                m.content.push(Content::ToolCall(ToolCall { id: "c".into(), name: "echo".into(), arguments: serde_json::json!({}) }));
                m.stop_reason = StopReason::ToolUse;
            }
            Box::pin(futures::stream::iter(vec![AssistantEvent::Done { message: m }]))
        }
    }

    #[tokio::test]
    async fn token_budget_forces_final_answer() {
        let cfg = LoopConfig {
            provider: Arc::new(Greedy),
            model: "m".into(),
            system_prompt: None,
            tools: vec![Arc::new(Echo)],
            reasoning: Default::default(),
            api: Default::default(),
            steering: Default::default(),
            follow_up: Default::default(),
            max_turns: 50.into(),
            turn_gate: Default::default(),
            max_tokens: None,
            llm_max_retries: None,
            stream_idle_timeout: None,
            token_budget: Some(250),
            run_timeout: None,
            before_run: None,
            approval: None,
            hooks: None,
        };
        let emit: Emit = Arc::new(|_| {});
        let ctx = Mutex::new(vec![]);
        let new = run_loop(&ctx, vec![Message::user_text("hi")], &cfg, CancellationToken::new(), emit).await;
        let assistants: Vec<&AssistantMessage> = new.iter().filter_map(|m| if let Message::Assistant(a) = m { Some(a) } else { None }).collect();
        // 3 次带工具（100+100+100 >= 250），然后 1 次不带工具的最终答复
        assert_eq!(assistants.len(), 4);
        assert_eq!(assistants.last().unwrap().text(), "final");
        assert_eq!(assistants.last().unwrap().stop_reason, StopReason::Stop);
    }

    /// 记录每次请求的消息条数；第一次调用前把上下文替换为一条摘要消息。
    struct Hooks {
        persisted: Mutex<Vec<Message>>,
        calls: AtomicUsize,
    }
    impl ContextHooks for Hooks {
        fn on_message(&self, m: &Message) {
            self.persisted.lock().unwrap().push(m.clone());
        }
        fn before_call(&self, context: Vec<Message>, _emit: Emit, _cancel: CancellationToken) -> futures::future::BoxFuture<'_, Option<Vec<Message>>> {
            let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move { first.then(|| vec![Message::user_text("SUMMARY"), context.last().unwrap().clone()]) })
        }
    }

    struct Recording(Mutex<Vec<usize>>);
    impl Provider for Recording {
        fn id(&self) -> &str {
            "recording"
        }
        fn stream(&self, req: Request, _c: CancellationToken) -> EventStream {
            self.0.lock().unwrap().push(req.messages.len());
            let mut m = AssistantMessage::empty(&req.model);
            m.content.push(Content::Text { text: "ok".into() });
            Box::pin(futures::stream::iter(vec![AssistantEvent::Done { message: m }]))
        }
    }

    #[tokio::test]
    async fn hooks_persist_each_message_and_can_replace_context() {
        let provider = Arc::new(Recording(Mutex::new(vec![])));
        let hooks = Arc::new(Hooks { persisted: Mutex::new(vec![]), calls: AtomicUsize::new(0) });
        let cfg = LoopConfig {
            provider: provider.clone(),
            model: "m".into(),
            system_prompt: None,
            tools: vec![],
            reasoning: Default::default(),
            api: Default::default(),
            steering: Default::default(),
            follow_up: Default::default(),
            max_turns: 5.into(),
            turn_gate: Default::default(),
            max_tokens: None,
            llm_max_retries: None,
            stream_idle_timeout: None,
            token_budget: None,
            run_timeout: None,
            before_run: None,
            approval: None,
            hooks: Some(hooks.clone()),
        };
        let ctx = Mutex::new(vec![Message::user_text("old 1"), Message::user_text("old 2"), Message::user_text("old 3")]);
        let new = run_loop(&ctx, vec![Message::user_text("hi")], &cfg, CancellationToken::new(), Arc::new(|_| {})).await;
        assert_eq!(new.len(), 2);
        // 压缩后模型只看到 [摘要, hi]
        assert_eq!(*provider.0.lock().unwrap(), [2]);
        assert_eq!(ctx.lock().unwrap().len(), 3);
        assert_eq!(*hooks.persisted.lock().unwrap(), new);
    }
}
