use crate::*;
use agent_ai::*;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{broadcast, Barrier, Notify};
use tokio_util::sync::CancellationToken;

struct BlockedProvider {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    calls: AtomicUsize,
}
impl Provider for BlockedProvider {
    fn id(&self) -> &str {
        "blocked"
    }
    fn stream(&self, req: Request, _: CancellationToken) -> EventStream {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(futures::stream::once(async move {
            if first {
                entered.notify_one();
                release.notified().await;
            }
            AssistantEvent::Done { message: AssistantMessage::empty(&req.model) }
        }))
    }
}

fn config(provider: Arc<dyn Provider>) -> LoopConfig {
    LoopConfig {
        provider,
        model: "test".into(),
        system_prompt: None,
        tools: vec![],
        reasoning: None,
        api: Default::default(),
        steering: Default::default(),
        follow_up: Default::default(),
        max_turns: 10,
        max_tokens: None,
        token_budget: None,
        run_timeout: None,
    }
}

fn blocked() -> (Arc<Agent>, Arc<Notify>, Arc<Notify>) {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let provider = Arc::new(BlockedProvider { entered: entered.clone(), release: release.clone(), calls: AtomicUsize::new(0) });
    (Agent::new(config(provider), vec![]), entered, release)
}

async fn settled(rx: &mut broadcast::Receiver<AgentEvent>) -> bool {
    let aborted = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let AgentEvent::AgentSettled { aborted } = rx.recv().await.unwrap() {
                break aborted;
            }
        }
    })
    .await
    .expect("run did not settle");
    // Exactly one settled event for this running period.
    while let Ok(ev) = rx.try_recv() {
        assert!(!matches!(ev, AgentEvent::AgentSettled { .. }));
    }
    aborted
}

async fn assert_users(agent: &Agent, expected: &[&str]) {
    let messages = agent.messages().await;
    let users: Vec<_> = messages.iter().filter(|m| matches!(m, Message::User { .. })).map(|m| serde_json::to_value(m).unwrap()).collect();
    assert_eq!(users.len(), expected.len());
    for text in expected {
        let expected = serde_json::to_value(Message::user_text(*text)).unwrap();
        assert!(users.iter().any(|v| v["content"] == expected["content"]));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_submits_are_atomic() {
    for mode in [None, Some("steer")] {
        let (agent, entered, release) = blocked();
        let mut rx = agent.subscribe();
        let barrier = Arc::new(Barrier::new(3));
        let mut tasks = vec![];
        for text in ["one", "two"] {
            let agent = agent.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                agent.submit(Message::user_text(text), mode)
            }));
        }
        barrier.wait().await;
        let a = tasks.remove(0).await.unwrap();
        let b = tasks.remove(0).await.unwrap();
        assert_eq!([a, b].iter().filter(|d| **d == Disposition::Started).count(), 1);
        assert!([a, b].contains(&if mode.is_some() { Disposition::QueuedSteer } else { Disposition::QueuedFollowUp }));
        entered.notified().await;
        release.notify_one();
        settled(&mut rx).await;
        assert_users(&agent, &["one", "two"]).await;
    }
}

#[tokio::test]
async fn run_timeout_cancels_uncooperative_provider() {
    let provider = Arc::new(BlockedProvider { entered: Arc::new(Notify::new()), release: Arc::new(Notify::new()), calls: AtomicUsize::new(0) });
    let mut cfg = config(provider);
    cfg.run_timeout = Some(Duration::from_millis(100));
    let agent = Agent::new(cfg, vec![]);
    let mut rx = agent.subscribe();
    agent.submit(Message::user_text("stall"), None);
    assert!(settled(&mut rx).await);
    assert!(!agent.is_running());
    assert!(matches!(agent.messages().await.last(), Some(Message::Assistant(a)) if a.stop_reason == StopReason::Aborted));
    // A settled run's timer must not affect the next run.
    agent.submit(Message::user_text("fresh"), None);
    assert!(!settled(&mut rx).await);
}

#[tokio::test]
async fn submit_while_finishing_is_processed() {
    let (agent, entered, release) = blocked();
    let mut rx = agent.subscribe();
    assert_eq!(agent.submit(Message::user_text("one"), None), Disposition::Started);
    entered.notified().await;
    assert_eq!(agent.submit(Message::user_text("two"), None), Disposition::QueuedFollowUp);
    release.notify_one();
    settled(&mut rx).await;
    assert_users(&agent, &["one", "two"]).await;
}

#[tokio::test]
async fn abort_discards_queues_and_next_submit_starts_fresh() {
    let (agent, entered, release) = blocked();
    let mut rx = agent.subscribe();
    assert_eq!(agent.submit(Message::user_text("one"), None), Disposition::Started);
    entered.notified().await;
    assert_eq!(agent.submit(Message::user_text("discard follow-up"), None), Disposition::QueuedFollowUp);
    assert_eq!(agent.submit(Message::user_text("discard steer"), Some("steer")), Disposition::QueuedSteer);
    agent.abort();
    release.notify_one();
    assert!(settled(&mut rx).await);
    assert!(!agent.is_running());
    assert_users(&agent, &["one"]).await;
    assert!(agent.config.steering.lock().unwrap().is_empty());
    assert!(agent.config.follow_up.lock().unwrap().is_empty());

    assert_eq!(agent.submit(Message::user_text("fresh"), None), Disposition::Started);
    assert!(!settled(&mut rx).await);
    assert_users(&agent, &["one", "fresh"]).await;
}

#[tokio::test]
async fn submit_in_final_handoff_is_processed() {
    let (agent, entered, release) = blocked();
    let mut rx = agent.subscribe();
    let weak = Arc::downgrade(&agent);
    let once = AtomicBool::new(false);
    agent.submit_with(Message::user_text("one"), None, move |_| {
        // This callback runs after run_loop's final queue drain, before clearing running.
        if !once.swap(true, Ordering::SeqCst) {
            assert_eq!(weak.upgrade().unwrap().submit(Message::user_text("two"), None), Disposition::QueuedFollowUp);
        }
    });
    entered.notified().await;
    release.notify_one();
    settled(&mut rx).await;
    assert_users(&agent, &["one", "two"]).await;
    assert!(!agent.is_running());
}

#[tokio::test]
async fn history_is_readable_during_provider_stream() {
    let (agent, entered, release) = blocked();
    let mut rx = agent.subscribe();
    agent.submit(Message::user_text("one"), None);
    entered.notified().await;
    let history = tokio::time::timeout(Duration::from_millis(100), agent.messages()).await.expect("snapshot blocked on run");
    assert_eq!(history.len(), 1);
    assert!(matches!(history[0], Message::User { .. }));
    release.notify_one();
    settled(&mut rx).await;
}

struct TwoTools;
impl Provider for TwoTools {
    fn id(&self) -> &str {
        "two_tools"
    }
    fn stream(&self, req: Request, _: CancellationToken) -> EventStream {
        let mut message = AssistantMessage::empty(&req.model);
        message.stop_reason = StopReason::ToolUse;
        for name in ["blocking", "write"] {
            message.content.push(Content::ToolCall(ToolCall { id: name.into(), name: name.into(), arguments: serde_json::json!({}) }));
        }
        Box::pin(futures::stream::iter([AssistantEvent::Done { message }]))
    }
}
struct TestTool {
    name: &'static str,
    entered: Arc<Notify>,
    wrote: Arc<AtomicBool>,
}
#[async_trait::async_trait]
impl AgentTool for TestTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        self.name
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }
    async fn execute(&self, _: &str, _: serde_json::Value, cancel: CancellationToken, _: UpdateSink) -> anyhow::Result<ToolResult> {
        if self.name == "blocking" {
            self.entered.notify_one();
            cancel.cancelled().await;
            Ok(ToolResult::error("aborted"))
        } else {
            self.wrote.store(true, Ordering::SeqCst);
            Ok(ToolResult::text("written"))
        }
    }
}

#[tokio::test]
async fn abort_skips_remaining_sequential_tools() {
    let entered = Arc::new(Notify::new());
    let wrote = Arc::new(AtomicBool::new(false));
    let mut cfg = config(Arc::new(TwoTools));
    cfg.tools = ["blocking", "write"].into_iter().map(|name| Arc::new(TestTool { name, entered: entered.clone(), wrote: wrote.clone() }) as Arc<dyn AgentTool>).collect();
    let agent = Agent::new(cfg, vec![]);
    let mut rx = agent.subscribe();
    agent.submit(Message::user_text("go"), None);
    entered.notified().await;
    agent.abort();
    settled(&mut rx).await;
    assert!(!wrote.load(Ordering::SeqCst));
    let messages = agent.messages().await;
    let results: Vec<_> = messages.iter().filter_map(|m| if let Message::ToolResult(r) = m { Some(r) } else { None }).collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[1].tool_call_id, "write");
    assert!(results[1].is_error);
    assert!(matches!(&results[1].content[0], Content::Text { text } if text == "aborted before execution"));
}
