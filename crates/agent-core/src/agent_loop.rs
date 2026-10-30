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
    /// 每轮结束后注入（对应 `getSteeringMessages`）。
    pub steering: Queue,
    /// agent 将要停止时注入（对应 `getFollowUpMessages`）。
    pub follow_up: Queue,
    /// 安全上限，防止工具死循环。
    pub max_turns: usize,
}

fn drain(q: &Queue) -> Vec<Message> {
    q.lock().unwrap().drain(..).collect()
}

/// 对应 `agent-loop.ts::runLoop`：外层循环处理 follow-up，内层循环处理工具调用与 steering。
/// `context` 会被就地追加；返回本次运行新增的消息。
pub async fn run_loop(context: &mut Vec<Message>, prompts: Vec<Message>, cfg: &LoopConfig, cancel: CancellationToken, emit: Emit) -> Vec<Message> {
    let mut new_messages = vec![];
    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);
    let mut pending = prompts;
    pending.extend(drain(&cfg.steering));
    let mut turns = 0usize;
    let mut first = true;

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
                context.push(m.clone());
                new_messages.push(m);
            }

            turns += 1;
            let assistant =
                if turns > cfg.max_turns { AssistantMessage::failed(&cfg.model, StopReason::Error, "max_turns exceeded") } else { stream_assistant(context, cfg, cancel.clone(), &emit).await };
            let am = Message::Assistant(assistant.clone());
            context.push(am.clone());
            new_messages.push(am.clone());

            if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                emit(AgentEvent::TurnEnd { message: am, tool_results: vec![] });
                break 'outer;
            }

            let calls: Vec<ToolCall> = assistant.tool_calls().into_iter().cloned().collect();
            let mut results = vec![];
            more_tool_calls = false;
            if !calls.is_empty() {
                let (msgs, terminate) = if assistant.stop_reason == StopReason::Length {
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
                    context.push(m.clone());
                    new_messages.push(m);
                }
                results = msgs;
            }
            emit(AgentEvent::TurnEnd { message: am, tool_results: results });
            if cancel.is_cancelled() {
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

async fn stream_assistant(context: &[Message], cfg: &LoopConfig, cancel: CancellationToken, emit: &Emit) -> AssistantMessage {
    let req = Request {
        model: cfg.model.clone(),
        system_prompt: cfg.system_prompt.clone(),
        messages: context.to_vec(),
        tools: cfg.tools.iter().map(|t| t.spec()).collect(),
        reasoning: cfg.reasoning,
        max_tokens: None,
    };
    emit(AgentEvent::MessageStart { message: Message::Assistant(AssistantMessage::empty(&cfg.model)) });
    let mut stream = cfg.provider.stream(req, cancel);
    let mut final_msg = None;
    while let Some(ev) = stream.next().await {
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
        tracing::warn!(latency_ms, "tool failed");
    } else {
        tracing::info!(latency_ms, "tool finished");
    }
    emit(AgentEvent::ToolExecutionEnd { tool_call_id: call.id.clone(), tool_name: call.name.clone(), result: result.clone(), is_error: result.is_error });
    result
}

/// 对应 executeToolCalls：全部可并行则并发执行，否则顺序执行；结果按模型给出的顺序返回。
async fn execute_tool_calls(calls: &[ToolCall], cfg: &LoopConfig, cancel: CancellationToken, emit: &Emit) -> (Vec<ToolResultMessage>, bool) {
    let all_parallel = calls.iter().all(|c| cfg.tools.iter().find(|t| t.name() == c.name).map_or(true, |t| t.execution_mode() == ExecutionMode::Parallel));
    let results: Vec<ToolResult> = if all_parallel {
        join_all(calls.iter().map(|c| run_one(c.clone(), cfg, cancel.clone(), emit.clone()))).await
    } else {
        let mut v = vec![];
        for c in calls {
            v.push(run_one(c.clone(), cfg, cancel.clone(), emit.clone()).await);
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
            steering: Default::default(),
            follow_up: Default::default(),
            max_turns: 5,
        };
        let events = Arc::new(Mutex::new(vec![]));
        let ev = events.clone();
        let emit: Emit = Arc::new(move |e| ev.lock().unwrap().push(serde_json::to_value(&e).unwrap()["type"].as_str().unwrap().to_string()));
        let mut ctx = vec![];
        let new = run_loop(&mut ctx, vec![Message::user_text("hi")], &cfg, CancellationToken::new(), emit).await;
        assert_eq!(new.len(), 4); // user, assistant(toolCall), toolResult, assistant(text)
        let ev = events.lock().unwrap();
        assert_eq!(ev.iter().filter(|t| *t == "turn_start").count(), 2);
        assert!(ev.contains(&"tool_execution_end".to_string()));
        assert_eq!(ev.last().unwrap(), "agent_end");
    }
}
