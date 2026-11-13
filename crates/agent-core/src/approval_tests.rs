//! 审批闸门与 agent 循环的集成测试：approve / deny / 策略拒绝 / 超时 / abort / 并行调用。
use crate::*;
use agent_ai::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// 第一次调用返回给定的工具调用；之后回显最后一批工具结果（模型“继续”）。
struct Calls {
    calls: Vec<(&'static str, &'static str)>,
    n: AtomicUsize,
}
impl Provider for Calls {
    fn id(&self) -> &str {
        "calls"
    }
    fn stream(&self, req: Request, _: CancellationToken) -> EventStream {
        let mut m = AssistantMessage::empty(&req.model);
        if self.n.fetch_add(1, Ordering::SeqCst) == 0 {
            for (id, name) in &self.calls {
                m.content.push(Content::ToolCall(ToolCall { id: (*id).into(), name: (*name).into(), arguments: serde_json::json!({"command": format!("run {id}")}) }));
            }
            m.stop_reason = StopReason::ToolUse;
        } else {
            let results: Vec<String> = req
                .messages
                .iter()
                .filter_map(|m| if let Message::ToolResult(r) = m { Some(r) } else { None })
                .map(|r| format!("{}={}", r.tool_call_id, r.content.iter().filter_map(|c| if let Content::Text { text } = c { Some(text.as_str()) } else { None }).collect::<String>()))
                .collect();
            m.content.push(Content::Text { text: format!("continued: {}", results.join("; ")) });
        }
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done { message: m }]))
    }
}

struct Recorder {
    name: &'static str,
    ran: Arc<Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl AgentTool for Recorder {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        self.name
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, id: &str, _: serde_json::Value, _: CancellationToken, _: UpdateSink) -> anyhow::Result<ToolResult> {
        self.ran.lock().unwrap().push(id.to_string());
        Ok(ToolResult::text(format!("ok {id}")))
    }
}

fn ask_all(tool: &str, _: &serde_json::Value) -> Verdict {
    if tool == "blocked" {
        Verdict::Deny { reason: "tools rule 'blocked'".into() }
    } else {
        Verdict::Ask { reason: format!("tool '{tool}' requires approval") }
    }
}

fn agent(calls: Vec<(&'static str, &'static str)>, timeout: Duration, approve_on_timeout: bool) -> (Arc<Agent>, Arc<ApprovalGate>, Arc<Mutex<Vec<String>>>) {
    let ran = Arc::new(Mutex::new(vec![]));
    let gate = ApprovalGate::new(Arc::new(ask_all), timeout, approve_on_timeout);
    let tools: Vec<Arc<dyn AgentTool>> = ["echo", "blocked"].into_iter().map(|name| Arc::new(Recorder { name, ran: ran.clone() }) as Arc<dyn AgentTool>).collect();
    let cfg = LoopConfig {
        provider: Arc::new(Calls { calls, n: AtomicUsize::new(0) }),
        model: "m".into(),
        system_prompt: None,
        tools,
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
        approval: Some(gate.clone()),
        hooks: None,
    };
    (Agent::new(cfg, vec![]), gate, ran)
}

async fn next_matching(rx: &mut broadcast::Receiver<AgentEvent>, f: impl Fn(&AgentEvent) -> bool) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let ev = rx.recv().await.unwrap();
            if f(&ev) {
                return ev;
            }
        }
    })
    .await
    .expect("event not received")
}

async fn required(rx: &mut broadcast::Receiver<AgentEvent>) -> String {
    match next_matching(rx, |e| matches!(e, AgentEvent::ApprovalRequired { .. })).await {
        AgentEvent::ApprovalRequired { call_id, .. } => call_id,
        _ => unreachable!(),
    }
}

/// 收集到 agent_settled 为止的事件。
async fn until_settled(rx: &mut broadcast::Receiver<AgentEvent>) -> (bool, Vec<AgentEvent>) {
    let mut seen = vec![];
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let ev = rx.recv().await.unwrap();
            if let AgentEvent::AgentSettled { aborted } = ev {
                return (aborted, seen);
            }
            seen.push(ev);
        }
    })
    .await
    .expect("run did not settle")
}

fn tool_results(messages: &[Message]) -> Vec<(String, bool, String)> {
    messages
        .iter()
        .filter_map(|m| if let Message::ToolResult(r) = m { Some(r) } else { None })
        .map(|r| (r.tool_call_id.clone(), r.is_error, r.content.iter().filter_map(|c| if let Content::Text { text } = c { Some(text.clone()) } else { None }).collect()))
        .collect()
}

fn resolved(events: &[AgentEvent]) -> Vec<serde_json::Value> {
    events.iter().filter(|e| matches!(e, AgentEvent::ApprovalResolved { .. })).map(|e| serde_json::to_value(e).unwrap()).collect()
}

#[tokio::test]
async fn approve_runs_tool_and_event_shapes() {
    let (agent, gate, ran) = agent(vec![("c1", "echo")], Duration::from_secs(30), false);
    let mut rx = agent.subscribe();
    agent.submit(Message::user_text("go"), None);
    let ev = next_matching(&mut rx, |e| matches!(e, AgentEvent::ApprovalRequired { .. })).await;
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["type"], "approval_required");
    assert_eq!(v["callId"], "c1");
    assert_eq!(v["tool"], "echo");
    assert_eq!(v["args"], serde_json::json!({"command": "run c1"}));
    assert_eq!(v["reason"], "tool 'echo' requires approval");
    assert_eq!(v["timeoutSecs"], 30);
    assert_eq!(v["expiresAt"].as_u64().unwrap() - v["requestedAt"].as_u64().unwrap(), 30_000);
    let pending = gate.pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].call_id, "c1");
    assert!(ran.lock().unwrap().is_empty(), "tool must not run before approval");
    assert_eq!(gate.resolve("nope", true, None), Err(ResolveError::NotFound));
    gate.resolve("c1", true, Some("looks fine".into())).unwrap();
    assert_eq!(gate.resolve("c1", true, None), Err(ResolveError::AlreadyResolved));
    let (aborted, events) = until_settled(&mut rx).await;
    assert!(!aborted);
    assert_eq!(*ran.lock().unwrap(), ["c1"]);
    let r = resolved(&events);
    assert_eq!(r.len(), 1);
    assert_eq!(
        (r[0]["decision"].clone(), r[0]["approved"].clone(), r[0]["by"].clone(), r[0]["reason"].clone()),
        (serde_json::json!("approved"), serde_json::json!(true), serde_json::json!("caller"), serde_json::json!("looks fine"))
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolExecutionEnd { is_error: false, .. })));
    assert_eq!(tool_results(&agent.messages().await), [("c1".to_string(), false, "ok c1".to_string())]);
    assert!(gate.pending().is_empty());
}

#[tokio::test]
async fn deny_returns_error_result_and_model_continues() {
    let (agent, gate, ran) = agent(vec![("c1", "echo")], Duration::from_secs(30), false);
    let mut rx = agent.subscribe();
    agent.submit(Message::user_text("go"), None);
    let id = required(&mut rx).await;
    gate.resolve(&id, false, Some("not now".into())).unwrap();
    let (aborted, events) = until_settled(&mut rx).await;
    assert!(!aborted);
    assert!(ran.lock().unwrap().is_empty());
    // 拒绝的调用不执行，也没有 tool_execution_* 事件
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::ToolExecutionStart { .. })));
    let messages = agent.messages().await;
    assert_eq!(tool_results(&messages), [("c1".to_string(), true, "tool call denied by user: not now".to_string())]);
    match messages.last() {
        Some(Message::Assistant(a)) => assert_eq!(a.text(), "continued: c1=tool call denied by user: not now"),
        other => panic!("model did not continue: {other:?}"),
    }
    let r = resolved(&events);
    assert_eq!((r[0]["decision"].clone(), r[0]["by"].clone()), (serde_json::json!("denied"), serde_json::json!("caller")));
}

#[tokio::test]
async fn policy_deny_never_asks() {
    let (agent, gate, ran) = agent(vec![("c1", "blocked"), ("c2", "missing")], Duration::from_secs(30), false);
    let mut rx = agent.subscribe();
    agent.submit(Message::user_text("go"), None);
    let (_, events) = until_settled(&mut rx).await;
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::ApprovalRequired { .. })));
    assert!(ran.lock().unwrap().is_empty());
    assert!(gate.pending().is_empty());
    let results = tool_results(&agent.messages().await);
    assert_eq!(results[0], ("c1".to_string(), true, "tool call denied by policy: tools rule 'blocked'".to_string()));
    // 未知工具不进入审批
    assert_eq!(results[1], ("c2".to_string(), true, "Tool missing not found".to_string()));
    let r = resolved(&events);
    assert_eq!(r.len(), 1);
    assert_eq!((r[0]["decision"].clone(), r[0]["by"].clone(), r[0]["waitMs"].clone()), (serde_json::json!("denied"), serde_json::json!("policy"), serde_json::json!(0)));
}

#[tokio::test]
async fn timeout_follows_on_timeout() {
    for approve_on_timeout in [false, true] {
        let (agent, gate, ran) = agent(vec![("c1", "echo")], Duration::from_millis(50), approve_on_timeout);
        let mut rx = agent.subscribe();
        agent.submit(Message::user_text("go"), None);
        let (aborted, events) = until_settled(&mut rx).await;
        assert!(!aborted);
        let r = resolved(&events);
        assert_eq!((r[0]["decision"].clone(), r[0]["by"].clone(), r[0]["approved"].clone()), (serde_json::json!("timeout"), serde_json::json!("timeout"), serde_json::json!(approve_on_timeout)));
        assert!(r[0]["reason"].as_str().unwrap().starts_with("approval timed out after"));
        let results = tool_results(&agent.messages().await);
        if approve_on_timeout {
            assert_eq!(*ran.lock().unwrap(), ["c1"]);
            assert!(!results[0].1);
        } else {
            assert!(ran.lock().unwrap().is_empty());
            assert!(results[0].1 && results[0].2.starts_with("tool call denied: approval timed out after"), "{results:?}");
        }
        // 超时后再决定 ⇒ 409 语义
        assert_eq!(gate.resolve("c1", true, None), Err(ResolveError::AlreadyResolved));
    }
}

#[tokio::test]
async fn abort_while_waiting_resolves_cleanly() {
    let (agent, gate, ran) = agent(vec![("c1", "echo"), ("c2", "echo")], Duration::from_secs(600), false);
    let mut rx = agent.subscribe();
    agent.submit(Message::user_text("go"), None);
    required(&mut rx).await;
    required(&mut rx).await;
    assert_eq!(gate.pending().len(), 2);
    agent.abort();
    let (aborted, events) = until_settled(&mut rx).await;
    assert!(aborted);
    assert!(!agent.is_running());
    assert!(gate.pending().is_empty());
    assert!(ran.lock().unwrap().is_empty());
    let r = resolved(&events);
    assert_eq!(r.len(), 2);
    assert!(r.iter().all(|r| r["decision"] == "cancelled" && r["by"] == "abort" && r["approved"] == false));
    assert_eq!(gate.resolve("c1", true, None), Err(ResolveError::AlreadyResolved));
    let results = tool_results(&agent.messages().await);
    assert!(results.iter().all(|(_, err, text)| *err && text == "aborted while waiting for approval"), "{results:?}");
}

#[tokio::test]
async fn parallel_calls_are_decided_independently_in_any_order() {
    let (agent, gate, ran) = agent(vec![("a", "echo"), ("b", "echo"), ("c", "echo")], Duration::from_secs(30), false);
    let mut rx = agent.subscribe();
    agent.submit(Message::user_text("go"), None);
    let mut ids = vec![required(&mut rx).await, required(&mut rx).await, required(&mut rx).await];
    ids.sort();
    assert_eq!(ids, ["a", "b", "c"]);
    assert_eq!(gate.pending().iter().map(|p| p.call_id.as_str()).collect::<Vec<_>>().len(), 3);
    gate.resolve("c", true, None).unwrap();
    gate.resolve("b", false, None).unwrap();
    // 先决定的调用可以先执行，不必等其它审批
    tokio::time::timeout(Duration::from_secs(3), async {
        while ran.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*ran.lock().unwrap(), ["c"]);
    gate.resolve("a", true, None).unwrap();
    until_settled(&mut rx).await;
    assert_eq!(*ran.lock().unwrap(), ["c", "a"]);
    let results = tool_results(&agent.messages().await);
    assert_eq!(results.iter().map(|r| (r.0.as_str(), r.1)).collect::<Vec<_>>(), [("a", false), ("b", true), ("c", false)]);
    assert_eq!(results[1].2, "tool call denied by user: no reason given");
}
