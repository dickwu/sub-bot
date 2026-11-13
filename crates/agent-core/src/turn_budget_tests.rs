//! 回合预算与 agent 循环的集成测试：运行期提高 / 降低上限、低预算提醒、ask（extend / stop / timeout / cancel）、summarize。
use crate::*;
use agent_ai::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// 带工具时每次都返回一个工具调用（永不结束）；不带工具时返回 "summary"。记录每次调用是否提供了工具。
#[derive(Default)]
struct Looping {
    with_tools: Mutex<Vec<bool>>,
}
impl Provider for Looping {
    fn id(&self) -> &str {
        "looping"
    }
    fn stream(&self, req: Request, _: CancellationToken) -> EventStream {
        let mut m = AssistantMessage::empty(&req.model);
        let mut seen = self.with_tools.lock().unwrap();
        seen.push(!req.tools.is_empty());
        if req.tools.is_empty() {
            m.content.push(Content::Text { text: "summary".into() });
        } else {
            m.content.push(Content::ToolCall(ToolCall { id: format!("c{}", seen.len()), name: "step".into(), arguments: serde_json::json!({}) }));
            m.stop_reason = StopReason::ToolUse;
        }
        Box::pin(futures::stream::iter(vec![AssistantEvent::Done { message: m }]))
    }
}

type Hook = Arc<dyn Fn(usize) + Send + Sync>;

/// 第 n 次执行时调用 hook(n)（模拟调用方在运行中调整预算）。
struct Step {
    n: AtomicUsize,
    hook: Hook,
}
#[async_trait::async_trait]
impl AgentTool for Step {
    fn name(&self) -> &str {
        "step"
    }
    fn description(&self) -> &str {
        "step"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _: &str, _: serde_json::Value, _: CancellationToken, _: UpdateSink) -> anyhow::Result<ToolResult> {
        (self.hook)(self.n.fetch_add(1, Ordering::SeqCst) + 1);
        Ok(ToolResult::text("ok"))
    }
}

struct Setup {
    agent: Arc<Agent>,
    provider: Arc<Looping>,
}

impl Setup {
    fn calls(&self) -> Vec<bool> {
        self.provider.with_tools.lock().unwrap().clone()
    }
    fn adjust(&self, change: TurnChange) -> Adjusted {
        self.agent.config.turn_gate.adjust(&self.agent.config.max_turns, change)
    }
}

/// hook 拿到 (max_turns, gate) 以便在工具执行时调整。
fn setup(max_turns: u64, on_max: OnMaxTurns, timeout: Duration, hook: impl Fn(usize, &Shared<u64>, &TurnGate) + Send + Sync + 'static) -> Setup {
    let provider = Arc::new(Looping::default());
    let max = Shared::new(max_turns);
    let gate = Arc::new(TurnGate::new(on_max, timeout));
    let (m, g) = (max.clone(), gate.clone());
    let hook: Hook = Arc::new(move |n| hook(n, &m, &g));
    let cfg = LoopConfig {
        provider: provider.clone(),
        model: "m".into(),
        system_prompt: None,
        tools: vec![Arc::new(Step { n: AtomicUsize::new(0), hook })],
        reasoning: Default::default(),
        api: Default::default(),
        steering: Default::default(),
        follow_up: Default::default(),
        max_turns: max,
        turn_gate: gate,
        max_tokens: None,
        llm_max_retries: None,
        token_budget: None,
        run_timeout: None,
        before_run: None,
        approval: None,
        hooks: None,
    };
    Setup { agent: Agent::new(cfg, vec![]), provider }
}

fn no_hook(_: usize, _: &Shared<u64>, _: &TurnGate) {}

/// 收集到 agent_settled 为止的事件。
async fn until_settled(rx: &mut broadcast::Receiver<AgentEvent>) -> (bool, Vec<AgentEvent>) {
    let mut seen = vec![];
    tokio::time::timeout(Duration::from_secs(5), async {
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

async fn exhausted(rx: &mut broadcast::Receiver<AgentEvent>) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ev = rx.recv().await.unwrap();
            if matches!(ev, AgentEvent::TurnBudgetExhausted { .. }) {
                return ev;
            }
        }
    })
    .await
    .expect("turn budget not exhausted")
}

/// 运行一次 prompt 直到 settled；返回 (aborted, 事件, 最后一条助手消息)。
async fn run(s: &Setup) -> (bool, Vec<AgentEvent>, AssistantMessage) {
    let mut rx = s.agent.subscribe();
    s.agent.submit(Message::user_text("go"), None);
    let (aborted, events) = until_settled(&mut rx).await;
    (aborted, events, last_assistant(&s.agent).await)
}

async fn last_assistant(agent: &Agent) -> AssistantMessage {
    agent.messages().await.into_iter().rev().find_map(|m| if let Message::Assistant(a) = m { Some(a) } else { None }).unwrap()
}

fn outcomes(events: &[AgentEvent]) -> Vec<String> {
    events.iter().filter_map(|e| if let AgentEvent::RunOutcome { kind } = e { Some(kind.clone()) } else { None }).collect()
}

fn lows(events: &[AgentEvent]) -> Vec<(u64, u64, u64)> {
    events.iter().filter_map(|e| if let AgentEvent::TurnBudgetLow { turns_used, max_turns, remaining } = e { Some((*turns_used, *max_turns, *remaining)) } else { None }).collect()
}

fn resolutions(events: &[AgentEvent]) -> Vec<(TurnBudgetDecision, u64)> {
    events.iter().filter_map(|e| if let AgentEvent::TurnBudgetResolved { decision, max_turns, .. } = e { Some((*decision, *max_turns)) } else { None }).collect()
}

#[test]
fn change_apply_clamps_and_low_threshold_rounds_up() {
    assert_eq!(TurnChange::Set(0).apply(10), 1);
    assert_eq!(TurnChange::Set(7).apply(10), 7);
    assert_eq!(TurnChange::Add(-100).apply(10), 1);
    assert_eq!(TurnChange::Add(5).apply(10), 15);
    assert_eq!(TurnChange::Add(i64::MAX).apply(u64::MAX - 1), u64::MAX);
    for (max, threshold) in [(1, 1), (2, 2), (5, 4), (7, 6), (9, 8), (10, 8), (150, 120), (200, 160)] {
        assert_eq!(crate::turn_budget::low_threshold(max), threshold, "max {max}");
    }
}

#[tokio::test]
async fn stop_reports_turns_and_records_last_run() {
    let s = setup(3, OnMaxTurns::Stop, Duration::from_secs(5), no_hook);
    let (aborted, events, last) = run(&s).await;
    assert!(!aborted);
    assert_eq!(s.calls(), [true; 3]);
    assert_eq!(last.error_message.as_deref(), Some("max_turns reached (3/3 turns used)"));
    assert_eq!(outcomes(&events), ["max_turns"]);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnBudgetExhausted { turns_used: 3, max_turns: 3, action: OnMaxTurns::Stop, timeout_secs: None, expires_at: None })));
    assert_eq!(s.agent.config.turn_gate.last_run(), Some(LastRun { turns_used: 3, max_turns: 3, limit_reached: true }));
    let view = s.agent.config.turn_gate.view(&s.agent.config.max_turns);
    assert!(!view.running && !view.paused && view.turns_used == 0);
    // 没有暂停 ⇒ stop 失败（HTTP 409）
    assert_eq!(s.agent.config.turn_gate.stop(), Err(NothingPending));
}

#[tokio::test]
async fn raise_mid_run_continues() {
    // 第一次工具执行时（已用 1 回合）把上限从 2 提高到 4
    let s = setup(2, OnMaxTurns::Stop, Duration::from_secs(5), |n, max, gate| {
        if n == 1 {
            let adjusted = gate.adjust(max, TurnChange::Set(4));
            assert_eq!((adjusted.previous, adjusted.max_turns, adjusted.turns_used, adjusted.running, adjusted.paused), (2, 4, 1, true, false));
        }
    });
    let (_, events, last) = run(&s).await;
    assert_eq!(s.calls().len(), 4);
    assert_eq!(last.error_message.as_deref(), Some("max_turns reached (4/4 turns used)"));
    assert_eq!(outcomes(&events), ["max_turns"]);
    // 修改保留给之后的 run
    assert_eq!(s.agent.config.max_turns.get(), 4);
}

#[tokio::test]
async fn lower_below_used_stops_at_next_check() {
    let s = setup(10, OnMaxTurns::Stop, Duration::from_secs(5), |n, max, gate| {
        if n == 2 {
            gate.adjust(max, TurnChange::Add(-9));
        }
    });
    let (_, events, last) = run(&s).await;
    assert_eq!(s.calls().len(), 2);
    assert_eq!(last.error_message.as_deref(), Some("max_turns reached (2/1 turns used)"));
    assert_eq!(outcomes(&events), ["max_turns"]);
}

#[tokio::test]
async fn low_fires_once_per_run_and_rearms_after_change() {
    let s = setup(5, OnMaxTurns::Stop, Duration::from_secs(5), no_hook);
    let (_, events, _) = run(&s).await;
    assert_eq!(lows(&events), [(4, 5, 1)]);
    // 低预算提醒已发出之后提高上限：再次越过阈值时重新提醒
    let s = setup(5, OnMaxTurns::Stop, Duration::from_secs(5), |n, max, gate| {
        if n == 5 {
            gate.adjust(max, TurnChange::Set(10));
        }
    });
    let (_, events, last) = run(&s).await;
    assert_eq!(lows(&events), [(4, 5, 1), (8, 10, 2)]);
    assert_eq!(last.error_message.as_deref(), Some("max_turns reached (10/10 turns used)"));
}

#[tokio::test]
async fn ask_extend_then_stop() {
    let s = setup(1, OnMaxTurns::Ask, Duration::from_secs(10), no_hook);
    let mut rx = s.agent.subscribe();
    s.agent.submit(Message::user_text("go"), None);
    match exhausted(&mut rx).await {
        AgentEvent::TurnBudgetExhausted { turns_used, max_turns, action, timeout_secs, expires_at } => {
            assert_eq!((turns_used, max_turns, action, timeout_secs), (1, 1, OnMaxTurns::Ask, Some(10)));
            assert!(expires_at.unwrap() > 0);
        }
        _ => unreachable!(),
    }
    let gate = &s.agent.config.turn_gate;
    let view = gate.view(&s.agent.config.max_turns);
    assert!(view.paused && view.running && view.turns_used == 1);
    assert_eq!(view.pause.as_ref().map(|p| (p.turns_used, p.max_turns, p.timeout_secs)), Some((1, 1, 10)));
    assert!(s.agent.is_running());
    // 不高于已用回合：继续暂停
    let adjusted = s.adjust(TurnChange::Set(1));
    assert!(adjusted.paused);
    let adjusted = s.adjust(TurnChange::Add(2));
    assert_eq!((adjusted.previous, adjusted.max_turns, adjusted.turns_used, adjusted.paused), (1, 3, 1, false));
    // 用完新预算后再次暂停，这次停止
    exhausted(&mut rx).await;
    assert_eq!(s.calls().len(), 3);
    let info = gate.stop().unwrap();
    assert_eq!((info.turns_used, info.max_turns), (3, 3));
    let (aborted, events) = until_settled(&mut rx).await;
    assert!(!aborted);
    assert_eq!(resolutions(&events), [(TurnBudgetDecision::Stopped, 3)]);
    assert_eq!(outcomes(&events), ["max_turns"]);
    assert_eq!(last_assistant(&s.agent).await.error_message.as_deref(), Some("max_turns reached (3/3 turns used)"));
    assert_eq!(gate.last_run(), Some(LastRun { turns_used: 3, max_turns: 3, limit_reached: true }));
    assert_eq!(gate.stop(), Err(NothingPending));
}

#[tokio::test]
async fn ask_extend_resolves_extended() {
    let s = setup(1, OnMaxTurns::Ask, Duration::from_secs(10), no_hook);
    let mut rx = s.agent.subscribe();
    s.agent.submit(Message::user_text("go"), None);
    exhausted(&mut rx).await;
    s.adjust(TurnChange::Set(2));
    match tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ev @ AgentEvent::TurnBudgetResolved { .. } = rx.recv().await.unwrap() {
                return ev;
            }
        }
    })
    .await
    .unwrap()
    {
        AgentEvent::TurnBudgetResolved { decision, max_turns, .. } => assert_eq!((decision, max_turns), (TurnBudgetDecision::Extended, 2)),
        _ => unreachable!(),
    }
    exhausted(&mut rx).await;
    s.agent.config.turn_gate.stop().unwrap();
    until_settled(&mut rx).await;
    assert_eq!(s.calls().len(), 2);
}

#[tokio::test]
async fn ask_timeout_stops() {
    let s = setup(1, OnMaxTurns::Ask, Duration::from_millis(50), no_hook);
    let (aborted, events, last) = run(&s).await;
    assert!(!aborted);
    assert_eq!(resolutions(&events), [(TurnBudgetDecision::Timeout, 1)]);
    assert_eq!(outcomes(&events), ["max_turns"]);
    assert_eq!(last.error_message.as_deref(), Some("max_turns reached (1/1 turns used)"));
    assert!(s.agent.config.turn_gate.paused().is_none());
}

#[tokio::test]
async fn ask_cancel_aborts() {
    let s = setup(1, OnMaxTurns::Ask, Duration::from_secs(10), no_hook);
    let mut rx = s.agent.subscribe();
    s.agent.submit(Message::user_text("go"), None);
    exhausted(&mut rx).await;
    s.agent.abort();
    let (aborted, events) = until_settled(&mut rx).await;
    assert!(aborted);
    assert_eq!(resolutions(&events), [(TurnBudgetDecision::Cancelled, 1)]);
    assert!(outcomes(&events).is_empty());
    let last = last_assistant(&s.agent).await;
    assert_eq!(last.stop_reason, StopReason::Aborted);
    assert_eq!(s.agent.config.turn_gate.last_run().map(|r| r.limit_reached), Some(false));
    assert!(s.agent.config.turn_gate.paused().is_none());
    assert_eq!(s.calls().len(), 1);
}

#[tokio::test]
async fn summarize_makes_one_toolless_call() {
    let s = setup(2, OnMaxTurns::Summarize, Duration::from_secs(5), no_hook);
    let (aborted, events, last) = run(&s).await;
    assert!(!aborted);
    assert_eq!(s.calls(), [true, true, false]);
    assert_eq!(last.text(), "summary");
    assert!(last.error_message.is_none());
    assert_eq!(outcomes(&events), ["max_turns"]);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnBudgetExhausted { action: OnMaxTurns::Summarize, turns_used: 2, max_turns: 2, .. })));
    let messages = s.agent.messages().await;
    let injected: Vec<_> = messages
        .iter()
        .filter_map(|m| if let Message::User { content, .. } = m { content.iter().find_map(|c| if let Content::Text { text } = c { Some(text.clone()) } else { None }) } else { None })
        .collect();
    assert_eq!(injected.last().unwrap(), "[sub-bot] Turn limit reached. Do not call tools; summarize progress, what remains, and next steps.");
    assert_eq!(s.agent.config.turn_gate.last_run(), Some(LastRun { turns_used: 2, max_turns: 2, limit_reached: true }));
}

#[test]
fn events_serialize_with_camel_case_fields() {
    let v = serde_json::to_value(AgentEvent::TurnBudgetExhausted { turns_used: 3, max_turns: 3, action: OnMaxTurns::Ask, timeout_secs: Some(600), expires_at: Some(9) }).unwrap();
    assert_eq!(v, serde_json::json!({"type": "turn_budget_exhausted", "turnsUsed": 3, "maxTurns": 3, "action": "ask", "timeoutSecs": 600, "expiresAt": 9}));
    let v = serde_json::to_value(AgentEvent::TurnBudgetExhausted { turns_used: 3, max_turns: 3, action: OnMaxTurns::Stop, timeout_secs: None, expires_at: None }).unwrap();
    assert_eq!(v, serde_json::json!({"type": "turn_budget_exhausted", "turnsUsed": 3, "maxTurns": 3, "action": "stop"}));
    let v = serde_json::to_value(AgentEvent::TurnBudgetChanged { max_turns: 5, previous: 3, turns_used: 2, by: TurnBudgetChangedBy::Caller }).unwrap();
    assert_eq!(v, serde_json::json!({"type": "turn_budget_changed", "maxTurns": 5, "previous": 3, "turnsUsed": 2, "by": "caller"}));
    let v = serde_json::to_value(AgentEvent::TurnBudgetResolved { decision: TurnBudgetDecision::Extended, max_turns: 5, wait_ms: 1 }).unwrap();
    assert_eq!(v, serde_json::json!({"type": "turn_budget_resolved", "decision": "extended", "maxTurns": 5, "waitMs": 1}));
    let v = serde_json::to_value(AgentEvent::TurnBudgetLow { turns_used: 8, max_turns: 10, remaining: 2 }).unwrap();
    assert_eq!(v, serde_json::json!({"type": "turn_budget_low", "turnsUsed": 8, "maxTurns": 10, "remaining": 2}));
}
