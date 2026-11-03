use crate::*;
use agent_ai::*;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{broadcast, Barrier, Notify};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn openai_stream_retries_keep_only_successful_message() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for api in [Api::Chat, Api::Responses] {
        for scenario in ["500", "text", "idle", "tool", "401", "400", "408", "429", "exhaust", "cancel"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let requests = count.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let index = requests.fetch_add(1, Ordering::SeqCst);
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") {
                        headers.push(socket.read_u8().await.unwrap());
                    }
                    let headers = String::from_utf8(headers).unwrap();
                    let length: usize = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").map(|s| s.parse().unwrap())).unwrap();
                    socket.read_exact(&mut vec![0; length]).await.unwrap();
                    if scenario == "exhaust" || (index == 0 && matches!(scenario, "500" | "401" | "400" | "408" | "429" | "cancel")) {
                        let code = if scenario == "exhaust" || scenario == "cancel" { "500" } else { scenario };
                        socket.write_all(format!("HTTP/1.1 {code} Test\r\nContent-Length: 0\r\nRetry-After: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                        continue;
                    }
                    let failed = index == 0;
                    let text = if failed { "discard me" } else { "successful" };
                    let delta = match (api, failed && scenario == "tool") {
                        (Api::Chat, true) => serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"read","arguments":"{"}}]}}]}),
                        (Api::Responses, true) => {
                            serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call","name":"read","arguments":""}})
                        }
                        (Api::Chat, false) => serde_json::json!({"choices":[{"delta":{"content":text}}]}),
                        _ => serde_json::json!({"type":"response.output_text.delta","delta":text}),
                    };
                    let mut body = format!("data: {delta}\n\n");
                    if !failed {
                        let done = if api == Api::Chat {
                            serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
                        } else {
                            serde_json::json!({"type":"response.completed","response":{"status":"completed"}})
                        };
                        body.push_str(&format!("data: {done}\n\n"));
                    }
                    // An incomplete Content-Length forces a real mid-body read failure.
                    let length = body.len() + if failed { 100 } else { 0 };
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
                    if failed && scenario == "idle" {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    socket.shutdown().await.unwrap();
                }
            });
            let provider = agent_ai::openai::OpenAiProvider::with_stream_idle_timeout(agent_ai::Secret::new("test"), Some(format!("http://{addr}")), Duration::from_millis(150));
            let mut cfg = config(Arc::new(provider));
            cfg.api = api;
            cfg.llm_max_retries = Some(1);
            let context = std::sync::Mutex::new(vec![]);
            let events = Arc::new(std::sync::Mutex::new(vec![]));
            let captured = events.clone();
            let cancel = CancellationToken::new();
            let abort = cancel.clone();
            let emit: crate::agent_loop::Emit = Arc::new(move |ev| {
                if scenario == "cancel" && matches!(ev, AgentEvent::MessageRetry { .. }) {
                    abort.cancel();
                }
                captured.lock().unwrap().push(ev);
            });
            let messages = tokio::time::timeout(Duration::from_secs(8), crate::agent_loop::run_loop(&context, vec![Message::user_text("test")], &cfg, cancel, emit)).await.unwrap();
            server.abort();
            let success = matches!(scenario, "500" | "text" | "idle" | "408" | "429");
            assert_eq!(count.load(Ordering::SeqCst), if success || scenario == "exhaust" { 2 } else { 1 }, "{api:?} {scenario}");
            let assistants: Vec<_> = messages.iter().filter_map(|m| if let Message::Assistant(a) = m { Some(a) } else { None }).collect();
            assert_eq!(assistants.len(), 1);
            assert_eq!(assistants[0].text(), if success { "successful" } else { "" });
            assert_eq!(
                assistants[0].stop_reason,
                if success {
                    StopReason::Stop
                } else if scenario == "cancel" {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                }
            );
            assert_eq!(*context.lock().unwrap(), messages);
            let events = events.lock().unwrap();
            assert_eq!(events.iter().filter(|e| matches!(e, AgentEvent::MessageRetry { attempt: 1, .. })).count(), usize::from(success || scenario == "exhaust" || scenario == "cancel"));
            if scenario == "text" {
                assert!(events.iter().any(|e| matches!(e, AgentEvent::MessageUpdate { assistant_message_event: AssistantEvent::TextDelta { delta, .. } } if delta == "discard me")));
            }
        }
    }
}

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
        reasoning: Default::default(),
        api: Default::default(),
        steering: Default::default(),
        follow_up: Default::default(),
        max_turns: 10,
        max_tokens: None,
        llm_max_retries: None,
        token_budget: None,
        run_timeout: None,
        before_run: None,
        approval: None,
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
