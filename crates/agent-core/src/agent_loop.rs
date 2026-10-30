use crate::{event::AgentEvent, tool::*};
use agent_ai::*;
use futures::{future::join_all, StreamExt};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

pub type Emit = Arc<dyn Fn(AgentEvent) + Send + Sync>;
pub type Queue = Arc<Mutex<VecDeque<Message>>>;

#[derive(Clone)]
pub struct LoopConfig {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub reasoning: Option<ThinkingLevel>,
    pub api: agent_ai::Api,
    /// 每轮结束后注入（对应 `getSteeringMessages`）。
    pub steering: Queue,
    /// agent 将要停止时注入（对应 `getFollowUpMessages`）。
    pub follow_up: Queue,
    /// 安全上限，防止工具死循环。
    pub max_turns: usize,
    /// 每次模型调用的输出 token 上限（含推理 token）
    pub max_tokens: Option<u32>,
    /// 单次运行的总 token 预算：达到后再给模型一次“不带工具、直接给出最终答复”的机会，然后停止
    pub token_budget: Option<u64>,
    /// 单次运行的时间上限；None 表示不限时。
    pub run_timeout: Option<std::time::Duration>,
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
    let mut turns = 0usize;
    let mut first = true;
    let mut used_tokens = 0u64;
    let mut budget_forced = false;

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
                context.lock().unwrap().push(m.clone());
                new_messages.push(m);
            }

            turns += 1;
            let over_budget = cfg.token_budget.is_some_and(|b| used_tokens >= b);
            let assistant = if turns > cfg.max_turns {
                emit(AgentEvent::RunOutcome { kind: "max_turns".into() });
                AssistantMessage::failed(&cfg.model, StopReason::Error, "max_turns exceeded")
            } else if over_budget && budget_forced {
                AssistantMessage::failed(&cfg.model, StopReason::Error, "token budget exceeded")
            } else if over_budget {
                budget_forced = true;
                emit(AgentEvent::RunOutcome { kind: "token_budget_exceeded".into() });
                tracing::warn!(issue = "typed", used_tokens, budget = cfg.token_budget, "token budget reached; asking for a final answer without tools");
                let m = Message::user_text("[sub-bot] Token budget reached. Do not call any more tools; give your final answer now based on what you have.");
                emit(AgentEvent::MessageStart { message: m.clone() });
                emit(AgentEvent::MessageEnd { message: m.clone() });
                context.lock().unwrap().push(m.clone());
                new_messages.push(m);
                stream_assistant(context, cfg, cancel.clone(), &emit, false).await
            } else {
                stream_assistant(context, cfg, cancel.clone(), &emit, true).await
            };
            used_tokens += assistant.usage.total_tokens;
            let am = Message::Assistant(assistant.clone());
            context.lock().unwrap().push(am.clone());
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
                    context.lock().unwrap().push(m.clone());
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
    emit(AgentEvent::AgentEnd { messages: new_messages.clone() });
    new_messages
}

async fn stream_assistant(context: &Mutex<Vec<Message>>, cfg: &LoopConfig, cancel: CancellationToken, emit: &Emit, with_tools: bool) -> AssistantMessage {
    let req = Request {
        model: cfg.model.clone(),
        system_prompt: cfg.system_prompt.clone(),
        messages: context.lock().unwrap().clone(),
        tools: if with_tools { cfg.tools.iter().map(|t| t.spec()).collect() } else { vec![] },
        reasoning: cfg.reasoning,
        api: cfg.api,
        max_tokens: cfg.max_tokens,
    };
    emit(AgentEvent::MessageStart { message: Message::Assistant(AssistantMessage::empty(&cfg.model)) });
    let mut stream = cfg.provider.stream(req, cancel.clone());
    let mut final_msg = None;
    loop {
        let ev = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let message = AssistantMessage::failed(&cfg.model, StopReason::Aborted, "aborted");
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
            AssistantEvent::Done { message } | AssistantEvent::Error { message } => final_msg = Some(message.clone()),
            _ => {}
        }
        emit(AgentEvent::MessageUpdate { assistant_message_event: ev });
    }
    let msg = final_msg.unwrap_or_else(|| AssistantMessage::failed(&cfg.model, StopReason::Error, "provider stream ended without done/error"));
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
    run_one_inner(call, cfg, cancel, emit).instrument(span).await
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
            reasoning: None,
            api: Default::default(),
            steering: Default::default(),
            follow_up: Default::default(),
            max_turns: 5,
            max_tokens: None,
            token_budget: None,
            run_timeout: None,
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
            reasoning: None,
            api: Default::default(),
            steering: Default::default(),
            follow_up: Default::default(),
            max_turns: 50,
            max_tokens: None,
            token_budget: Some(250),
            run_timeout: None,
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
}
