//! 会话持久化 / 恢复 / 压缩的集成测试（faux 风格的脚本 provider，不访问网络）。
use crate::config::{sha256_hex, Client, Config};
use crate::hub::{Hub, SessionHandle, SessionRequest};
use crate::issues::Reporter;
use agent_ai::{AssistantEvent, AssistantMessage, Content, Message, Provider, Request, StopReason, ThinkingLevel, Usage};
use agent_core::AgentEvent;
use agent_session::{compaction as cp, Entry, Session};
use axum::{
    body::{to_bytes, Body},
    http::{Request as HttpRequest, StatusCode},
    Router,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tower::ServiceExt;

/// 脚本 provider：
/// - 摘要请求（系统提示为摘要提示）⇒ `SUMMARY-<n>`（用量 15），`fail_summary` 时返回错误；
/// - `/tool ...` 或工具结果之后 ⇒ 交给 FauxProvider；
/// - 其它 ⇒ 带签名 thinking 块 + `reply <n>`；`gate` 存在时等待一个 permit 再回复。
#[derive(Default)]
pub(crate) struct Script {
    pub seen: Mutex<Vec<Request>>,
    pub fail_summary: AtomicBool,
    pub gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    replies: AtomicUsize,
    summaries: AtomicUsize,
}

pub(crate) fn is_summary(req: &Request) -> bool {
    req.system_prompt.as_deref() == Some(cp::SUMMARIZATION_SYSTEM_PROMPT)
}

fn first_text(m: Option<&Message>) -> String {
    match m {
        Some(Message::User { content, .. }) => content.iter().find_map(|c| if let Content::Text { text } = c { Some(text.clone()) } else { None }).unwrap_or_default(),
        _ => String::new(),
    }
}

impl Script {
    pub fn summary_requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().iter().filter(|r| is_summary(r)).cloned().collect()
    }
    pub fn main_requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().iter().filter(|r| !is_summary(r)).cloned().collect()
    }
}

impl Provider for Script {
    fn id(&self) -> &str {
        "faux"
    }
    fn stream(&self, req: Request, cancel: tokio_util::sync::CancellationToken) -> agent_ai::EventStream {
        self.seen.lock().unwrap().push(req.clone());
        if is_summary(&req) {
            let n = self.summaries.fetch_add(1, Ordering::SeqCst) + 1;
            let event = if self.fail_summary.load(Ordering::SeqCst) {
                let mut m = AssistantMessage::failed(&req.model, StopReason::Error, "HTTP 500: summary backend down");
                m.usage.total_tokens = 3;
                AssistantEvent::Error { message: m }
            } else {
                let mut m = AssistantMessage::empty(&req.model);
                m.content.push(Content::Text { text: format!("SUMMARY-{n}") });
                m.usage = Usage { input: 10, output: 5, total_tokens: 15, ..Default::default() };
                AssistantEvent::Done { message: m }
            };
            return Box::pin(futures::stream::iter(vec![event]));
        }
        if matches!(req.messages.last(), Some(Message::ToolResult(_))) || first_text(req.messages.last()).starts_with("/tool ") {
            return agent_ai::faux::FauxProvider.stream(req, cancel);
        }
        let n = self.replies.fetch_add(1, Ordering::SeqCst) + 1;
        let gate = self.gate.lock().unwrap().clone();
        Box::pin(futures::stream::once(async move {
            if let Some(gate) = gate {
                if let Ok(permit) = gate.acquire().await {
                    permit.forget();
                }
            }
            let mut m = AssistantMessage::empty(&req.model);
            m.content = vec![Content::Thinking { thinking: format!("thinking {n}"), signature: Some(format!("sig-{n}")) }, Content::Text { text: format!("reply {n}") }];
            AssistantEvent::Done { message: m }
        }))
    }
}

pub(crate) struct TempDir(pub PathBuf);
impl TempDir {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("sub-bot-sessions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    pub fn sessions(&self) -> PathBuf {
        self.0.join("sessions")
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn config(dir: &Path) -> Config {
    let mut cfg = Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
    cfg.data_dir = Some(dir.join("data"));
    cfg.profiles.get_mut("default").unwrap().load_context_files = false;
    cfg.clients.push(Client { name: "other".into(), token_sha256: sha256_hex("other-token"), profiles: vec!["default".into()], ..Default::default() });
    cfg
}

/// 持久化到 `<dir>/sessions` 的 hub；faux provider 换成脚本 provider。
pub(crate) fn hub(dir: &TempDir, edit: impl FnOnce(&mut Config)) -> (Arc<Hub>, Arc<Script>) {
    let mut cfg = config(&dir.0);
    edit(&mut cfg);
    let mut hub = Hub::new(cfg, "faux", Some(dir.sessions()), None).unwrap();
    let inner = Arc::get_mut(&mut hub).unwrap();
    let script = Arc::new(Script::default());
    inner.providers.insert("faux".into(), script.clone() as Arc<dyn Provider>);
    inner.issues = Reporter::new(&inner.config.alerts, None, vec![]);
    (hub, script)
}

/// 自动压缩总是触发（窗口 1000、阈值 1%），只保留最近的用户消息。
pub(crate) fn eager_compaction(cfg: &mut Config) {
    let p = cfg.profiles.get_mut("default").unwrap();
    p.context_window = Some(1000);
    p.compaction = Some(crate::compaction::CompactionConfig { threshold_pct: Some(1), keep_recent_tokens: Some(0), ..Default::default() });
}

/// 提交并收集事件直到 agent_settled。
pub(crate) async fn prompt(h: &Arc<SessionHandle>, text: &str) -> Vec<AgentEvent> {
    let mut rx = h.subscribe();
    h.submit(text.into(), None);
    collect(&mut rx).await
}

async fn collect(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = vec![];
        loop {
            match rx.recv().await {
                Ok(e @ AgentEvent::AgentSettled { .. }) => {
                    events.push(e);
                    return events;
                }
                Ok(e) => events.push(e),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return events,
            }
        }
    })
    .await
    .expect("settled")
}

fn compactions(events: &[AgentEvent]) -> Vec<Value> {
    events.iter().filter(|e| matches!(e, AgentEvent::Compaction { .. })).map(|e| serde_json::to_value(e).unwrap()).collect()
}

async fn request(app: &Router, method: &str, path: &str, token: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = HttpRequest::builder().method(method).uri(path).header("content-type", "application/json").header("authorization", format!("Bearer {token}"));
    let response = app.clone().oneshot(req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty)).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// 足够长的提示，压缩后的估计一定小于压缩前（摘要消息本身约 30 token）
fn long(word: &str) -> String {
    format!("{word} {}", "lorem ipsum ".repeat(60))
}

fn client(hub: &Hub, name: &str) -> Client {
    hub.config.clients.iter().find(|c| c.name == name).unwrap().clone()
}

fn file_entries(path: &Path) -> Vec<Entry> {
    Session::open(path).unwrap().entries().to_vec()
}

#[tokio::test]
async fn resume_after_restart_restores_context_signatures_and_settings() {
    let dir = TempDir::new();
    let (hub1, _) = hub(&dir, |_| {});
    let me = client(&hub1, "default");
    let h = hub1.create_session(&me, SessionRequest { reasoning_effort: Some(ThinkingLevel::High), ..Default::default() }).unwrap();
    prompt(&h, "hello there").await;
    let (id, before, path) = (h.id.clone(), h.agent.messages().await, h.hooks.path().unwrap());
    assert_eq!(before.len(), 2);
    // 增量写入：文件内容与内存上下文一致，元数据含 cwd / 模型 / 推理强度 / 路由 / 标题
    let stored = Session::open(&path).unwrap();
    assert_eq!(stored.active_messages(), before);
    let meta = stored.meta();
    assert_eq!(
        (meta.client.as_deref(), meta.model.as_deref(), meta.requested_reasoning_effort, meta.title.as_deref()),
        (Some("default"), Some("faux"), Some(ThinkingLevel::High), Some("hello there"))
    );
    assert_eq!(meta.cwd.as_deref(), Some(h.cwd.as_path()));
    assert_eq!(meta.routing.as_ref().unwrap()["decided"], true);
    assert!(meta.system_prompt_sha256.is_some());
    drop((h, hub1));

    let (hub2, script) = hub(&dir, |_| {});
    let me = client(&hub2, "default");
    assert!(hub2.get(&me, &id).is_none());
    // 其它 client 看不到（也不会被加载）
    assert!(hub2.get_or_load(&client(&hub2, "other"), &id).await.unwrap().is_none());
    assert!(hub2.get(&me, &id).is_none());
    let h = hub2.get_or_load(&me, &id).await.unwrap().expect("rehydrated");
    assert_eq!(h.id, id);
    assert_eq!(h.agent.messages().await, before);
    assert_eq!(h.requested_effort(), Some(ThinkingLevel::High));
    assert!(h.routing().decided);
    // 已收到过 prompt：depth 不能再改
    assert!(h.set_depth(crate::routing::Depth::Deep).is_err());
    prompt(&h, "again").await;
    // 签名 thinking 原样回放
    let replay = &script.main_requests()[0].messages;
    let Message::Assistant(a) = &replay[1] else { panic!("assistant") };
    assert_eq!(a.content[0], Content::Thinking { thinking: "thinking 1".into(), signature: Some("sig-1".into()) });
    // 追加到同一个文件
    assert_eq!(Session::open(&path).unwrap().active_messages(), h.agent.messages().await);
    assert_eq!(h.agent.messages().await.len(), 4);
    assert_eq!(agent_session::list_files(&dir.sessions().join("default")).len(), 1);
}

#[tokio::test]
async fn http_rehydrates_lists_purges_and_rejects_bad_ids() {
    let dir = TempDir::new();
    let (hub1, _) = hub(&dir, |_| {});
    let app1 = crate::http::test_router(hub1.clone());
    let (_, a) = request(&app1, "POST", "/v1/chat", "test-token", Some(json!({"message": "first task"}))).await;
    let a = a["sessionId"].as_str().unwrap().to_string();
    let (status, b) = request(&app1, "POST", "/v1/sessions", "test-token", None).await;
    assert_eq!(status, StatusCode::CREATED);
    let b = b["sessionId"].as_str().unwrap().to_string();
    let (_, listed) = request(&app1, "GET", "/v1/sessions", "test-token", None).await;
    assert!(listed["sessions"].as_array().unwrap().iter().all(|s| s["inMemory"] == true && s["persisted"] == true));
    hub1.abort_all().await;
    drop((app1, hub1));

    let (hub2, _) = hub(&dir, |_| {});
    let app = crate::http::test_router(hub2.clone());
    let (status, listed) = request(&app, "GET", "/v1/sessions", "test-token", None).await;
    assert_eq!(status, StatusCode::OK);
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    let row_a = sessions.iter().find(|s| s["sessionId"] == a.as_str()).unwrap();
    assert_eq!((row_a["inMemory"].clone(), row_a["running"].clone(), row_a["persisted"].clone()), (json!(false), json!(false), json!(true)));
    assert_eq!((row_a["title"].clone(), row_a["messageCount"].clone(), row_a["ephemeral"].clone(), row_a["profile"].clone()), (json!("first task"), json!(2), json!(true), json!("default")));
    assert!(row_a["createdAt"].as_u64().unwrap() > 0 && row_a["updatedAt"].as_u64().unwrap() > 0);
    // 按 updatedAt 倒序；limit
    let updated: Vec<u64> = sessions.iter().map(|s| s["updatedAt"].as_u64().unwrap()).collect();
    assert!(updated.windows(2).all(|w| w[0] >= w[1]));
    assert_eq!(request(&app, "GET", "/v1/sessions?limit=1", "test-token", None).await.1["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(request(&app, "GET", "/v1/sessions?limit=0", "test-token", None).await.0, StatusCode::BAD_REQUEST);
    // owner 隔离：其它 client 列表为空，访问 404 且不会加载
    assert_eq!(request(&app, "GET", "/v1/sessions", "other-token", None).await.1["sessions"], json!([]));
    for path in [format!("/v1/sessions/{b}"), format!("/v1/sessions/{b}/messages"), format!("/v1/sessions/{b}/events")] {
        assert_eq!(request(&app, "GET", &path, "other-token", None).await.0, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(request(&app, "DELETE", &format!("/v1/sessions/{b}?purge=true"), "other-token", None).await.0, StatusCode::NOT_FOUND);
    assert!(hub2.get(&client(&hub2, "default"), &b).is_none());
    // 非法 id：路径穿越 / 非规范 UUID ⇒ 404，不访问磁盘
    std::fs::write(dir.0.join("secret.jsonl"), "{}").unwrap();
    for bad in ["..%2F..%2Fsecret", "..%2Fsecret.jsonl", "%2E%2E", &a.to_uppercase(), &a.replace('-', ""), &format!("{a}%2F..")] {
        for path in [format!("/v1/sessions/{bad}"), format!("/v1/sessions/{bad}/messages")] {
            assert_eq!(request(&app, "GET", &path, "test-token", None).await.0, StatusCode::NOT_FOUND, "{path}");
        }
        assert_eq!(request(&app, "POST", "/v1/chat", "test-token", Some(json!({"message": "x", "sessionId": bad}))).await.0, StatusCode::NOT_FOUND);
    }
    // 任何会话路由都按需恢复；继续对话写入同一文件
    let (status, messages) = request(&app, "GET", &format!("/v1/sessions/{a}/messages"), "test-token", None).await;
    assert_eq!((status, messages["messages"].as_array().unwrap().len()), (StatusCode::OK, 2));
    let (status, reply) = request(&app, "POST", "/v1/chat", "test-token", Some(json!({"message": "continue", "sessionId": a}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reply["sessionId"], a.as_str());
    let (status, view) = request(&app, "GET", &format!("/v1/sessions/{b}"), "test-token", None).await;
    assert_eq!((status, view["sessionId"].clone()), (StatusCode::OK, json!(b)));
    let listed = request(&app, "GET", "/v1/sessions", "test-token", None).await.1;
    let row_a = listed["sessions"].as_array().unwrap().iter().find(|s| s["sessionId"] == a.as_str()).unwrap().clone();
    assert_eq!((row_a["inMemory"].clone(), row_a["messageCount"].clone()), (json!(true), json!(4)));
    // DELETE：默认只删内存（之后仍可恢复）；purge 删除 JSONL
    let path_a = hub2.get(&client(&hub2, "default"), &a).unwrap().hooks.path().unwrap();
    let (status, body) = request(&app, "DELETE", &format!("/v1/sessions/{a}"), "test-token", None).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"deleted": true, "inMemory": true, "purged": false})));
    assert!(path_a.exists());
    let (status, body) = request(&app, "DELETE", &format!("/v1/sessions/{a}"), "test-token", None).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"deleted": true, "inMemory": false, "purged": false})));
    let (status, body) = request(&app, "DELETE", &format!("/v1/sessions/{a}?purge=true"), "test-token", None).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"deleted": true, "inMemory": false, "purged": true})));
    assert!(!path_a.exists());
    assert_eq!(request(&app, "GET", &format!("/v1/sessions/{a}"), "test-token", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(request(&app, "DELETE", &format!("/v1/sessions/{a}"), "test-token", None).await.0, StatusCode::NOT_FOUND);
    // 内存中的会话 + purge
    let (status, body) = request(&app, "DELETE", &format!("/v1/sessions/{b}?purge=true"), "test-token", None).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"deleted": true, "inMemory": true, "purged": true})));
    assert!(agent_session::list_files(&dir.sessions().join("default")).is_empty());
    assert!(dir.0.join("secret.jsonl").exists());
}

#[tokio::test]
async fn auto_compaction_is_cumulative_persisted_and_reloads() {
    let dir = TempDir::new();
    let (hub1, script) = hub(&dir, eager_compaction);
    let h = hub1.create_session(&client(&hub1, "default"), SessionRequest::default()).unwrap();
    // 只有一条消息：没有可压缩的内容
    assert!(compactions(&prompt(&h, &long("one")).await).is_empty());
    let events = prompt(&h, &long("two")).await;
    let c = compactions(&events);
    assert_eq!(c.len(), 2, "{c:?}");
    assert_eq!((c[0]["type"].clone(), c[0]["reason"].clone(), c[0]["phase"].clone()), (json!("compaction"), json!("auto"), json!("start")));
    assert_eq!((c[1]["phase"].clone(), c[1]["summaryChars"].clone()), (json!("end"), json!(9)));
    assert!(c[1]["tokensBefore"].as_u64().unwrap() > c[1]["tokensAfter"].as_u64().unwrap());
    assert!(c[1]["firstKeptEntryId"].is_string() && c[1]["durationMs"].is_u64() && c[1].get("error").is_none());
    assert_eq!(c[1]["usage"]["totalTokens"], 15);
    // 摘要请求：无工具、固定系统提示，不发送签名
    let summaries = script.summary_requests();
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0].tools.is_empty() && summaries[0].messages.len() == 1);
    let prompt_text = first_text(summaries[0].messages.first());
    assert!(prompt_text.contains("[User]: one") && prompt_text.contains("[Assistant thinking]: thinking 1") && !prompt_text.contains("sig-1"));
    assert!(!prompt_text.contains("previous-summary"));
    // 模型看到的是 [摘要, two]
    let sent = script.main_requests().last().unwrap().messages.clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], cp::summary_message("SUMMARY-1"));
    assert_eq!(first_text(sent.get(1)), long("two"));
    // 第二次压缩是累积的：带上上一次摘要
    let c = compactions(&prompt(&h, &long("three")).await);
    assert_eq!(c.len(), 2);
    let second = first_text(script.summary_requests()[1].messages.first());
    assert!(second.contains("<previous-summary>\nSUMMARY-1\n</previous-summary>") && second.contains("[User]: two"), "{second}");
    let context = h.agent.messages().await;
    assert_eq!(context[0], cp::summary_message("SUMMARY-2"));
    assert_eq!(context.len(), 3);
    assert_eq!(h.hooks.compaction_usage().total_tokens, 30);
    // JSONL：两条 compaction 条目；重启后得到压缩后的上下文
    let path = h.hooks.path().unwrap();
    let entries = file_entries(&path);
    let written: Vec<_> = entries
        .iter()
        .filter_map(|e| if let Entry::Compaction { summary, tokens_before, tokens_after, .. } = e { Some((summary.clone(), tokens_before.is_some(), tokens_after.is_some())) } else { None })
        .collect();
    assert_eq!(written, [("SUMMARY-1".to_string(), true, true), ("SUMMARY-2".to_string(), true, true)]);
    let id = h.id.clone();
    drop((h, hub1));
    let (hub2, script2) = hub(&dir, eager_compaction);
    let h = hub2.get_or_load(&client(&hub2, "default"), &id).await.unwrap().unwrap();
    assert_eq!(h.agent.messages().await, context);
    // 恢复后的第三次压缩仍然累积
    prompt(&h, "four").await;
    assert!(first_text(script2.summary_requests()[0].messages.first()).contains("<previous-summary>\nSUMMARY-2\n</previous-summary>"));
}

#[tokio::test]
async fn compaction_keeps_signed_thinking_and_tool_pairs_verbatim() {
    let script = Script::default();
    let mut a1 = AssistantMessage::empty("m");
    a1.content = vec![
        Content::Thinking { thinking: "t1".into(), signature: Some("SIG-ONE".into()) },
        Content::ToolCall(agent_ai::ToolCall { id: "c1".into(), name: "read".into(), arguments: json!({"path": "a.rs"}) }),
    ];
    a1.stop_reason = StopReason::ToolUse;
    let mut a2 = AssistantMessage::empty("m");
    a2.content = vec![Content::Thinking { thinking: "t2".into(), signature: Some("SIG-TWO".into()) }, Content::RedactedThinking { data: "OPAQUE".into() }, Content::Text { text: "done".into() }];
    let result = |id: &str| {
        Message::ToolResult(agent_ai::ToolResultMessage { tool_call_id: id.into(), tool_name: "read".into(), content: vec![Content::Text { text: "x".repeat(400) }], is_error: false, timestamp: 0 })
    };
    let mut a3 = a1.clone();
    a3.content[1] = Content::ToolCall(agent_ai::ToolCall { id: "c2".into(), name: "edit".into(), arguments: json!({"path": "b.rs"}) });
    let messages = vec![Message::user_text("q1"), Message::Assistant(a1.clone()), result("c1"), Message::Assistant(a2.clone()), Message::user_text("q2"), Message::Assistant(a3.clone()), result("c2")];
    // 预算落在最后的工具结果上 ⇒ 切在发出调用的助手消息前（拆开回合），工具配对完整保留
    let cut = cp::find_cut_point(&messages, 50).unwrap();
    assert_eq!((cut.cut, cut.split_turn), (5, true));
    let call = crate::compaction::SummaryCall { provider: &script, model: "m", api: agent_ai::Api::Auto, max_tokens: 1000, llm_max_retries: None };
    let outcome = crate::compaction::compact(&call, &messages, cut, 0, Some("keep file names"), Default::default()).await.unwrap();
    assert_eq!(outcome.context, vec![cp::summary_message("SUMMARY-1"), Message::Assistant(a3), result("c2")]);
    let prompt_text = first_text(script.summary_requests()[0].messages.first());
    assert!(prompt_text.contains("[User]: q2") && prompt_text.contains("keep file names") && prompt_text.contains("read: a.rs"));
    for secret in ["SIG-ONE", "SIG-TWO", "OPAQUE"] {
        assert!(!prompt_text.contains(secret));
    }
    // 用户边界优先：预算点之后有用户消息时整轮保留
    let cut = cp::find_cut_point(&messages[..5], 1).unwrap();
    assert_eq!((cut.cut, cut.split_turn), (4, false));
}

#[tokio::test]
async fn auto_compaction_failure_warns_and_run_continues() {
    let dir = TempDir::new();
    let (hub, script) = hub(&dir, eager_compaction);
    script.fail_summary.store(true, Ordering::SeqCst);
    let h = hub.create_session(&client(&hub, "default"), SessionRequest::default()).unwrap();
    prompt(&h, "one").await;
    let events = prompt(&h, "two").await;
    let c = compactions(&events);
    assert_eq!(c.len(), 2);
    assert_eq!(c[1]["error"], "HTTP 500: summary backend down");
    assert_eq!(c[1]["usage"]["totalTokens"], 3);
    // 不压缩，照常回复
    let context = h.agent.messages().await;
    assert_eq!(context.len(), 4);
    let Message::Assistant(last) = context.last().unwrap() else { panic!("assistant") };
    assert_eq!((last.text(), last.error_message.clone()), ("reply 2".to_string(), None));
    assert!(!file_entries(&h.hooks.path().unwrap()).iter().any(|e| matches!(e, Entry::Compaction { .. })));
    // 失败后短时间内不再自动尝试
    prompt(&h, "three").await;
    assert_eq!(script.summary_requests().len(), 1);
    assert_eq!(h.hooks.compaction_usage().total_tokens, 3);
    hub.issues.flush(Duration::from_secs(1)).await;
    let issues = hub.issues.recent(None, 0, 100);
    let failed: Vec<_> = issues.iter().filter(|e| e.kind == "compaction_failed").collect();
    assert_eq!(failed.len(), 1, "{issues:?}");
    assert_eq!(failed[0].severity, crate::issues::Severity::Warn);
    assert_eq!(failed[0].details["reason"], "auto");
    assert!(!issues.iter().any(|e| e.kind == "log_warn"), "{issues:?}");
}

#[tokio::test]
async fn manual_compact_endpoint_conflict_success_and_noop() {
    let dir = TempDir::new();
    let (hub, script) = hub(&dir, |cfg| {
        cfg.profiles.get_mut("default").unwrap().compaction = Some(crate::compaction::CompactionConfig { enabled: Some(false), keep_recent_tokens: Some(0), ..Default::default() });
    });
    let app = crate::http::test_router(hub.clone());
    let (_, created) = request(&app, "POST", "/v1/sessions", "test-token", None).await;
    let id = created["sessionId"].as_str().unwrap().to_string();
    let path = format!("/v1/sessions/{id}/compact");
    // 什么都没有：200，compacted false
    let (status, body) = request(&app, "POST", &path, "test-token", None).await;
    assert_eq!((status, body["compacted"].clone()), (StatusCode::OK, json!(false)));
    assert_eq!(request(&app, "POST", &path, "test-token", Some(json!({"bogus": 1}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(request(&app, "POST", &path, "other-token", None).await.0, StatusCode::NOT_FOUND);
    let h = hub.get(&client(&hub, "default"), &id).unwrap();
    prompt(&h, &long("one")).await;
    // 运行中 ⇒ 409
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *script.gate.lock().unwrap() = Some(gate.clone());
    let mut rx = h.subscribe();
    h.submit(long("two"), None);
    let (status, body) = request(&app, "POST", &path, "test-token", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("running"));
    gate.add_permits(1);
    collect(&mut rx).await;
    *script.gate.lock().unwrap() = None;
    // 空闲 ⇒ 200；enabled:false 只关闭自动压缩
    assert!(script.summary_requests().is_empty());
    let mut rx = h.subscribe();
    let (status, body) = request(&app, "POST", &path, "test-token", Some(json!({"instructions": "focus on the failing test"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["compacted"], true);
    assert!(body["tokensBefore"].as_u64().unwrap() > body["tokensAfter"].as_u64().unwrap());
    assert_eq!(body["summaryChars"], 9);
    let first_kept = body["firstKeptEntryId"].as_str().unwrap().to_string();
    assert!(first_text(script.summary_requests()[0].messages.first()).contains("focus on the failing test"));
    let mut events = vec![];
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    let c = compactions(&events);
    assert_eq!((c.len(), c[0]["reason"].clone(), c[1]["firstKeptEntryId"].clone()), (2, json!("manual"), json!(first_kept)));
    // 保留了最后一条助手消息（keepRecentTokens 0 ⇒ 预算落在最后一条消息上）
    let context = h.agent.messages().await;
    assert_eq!(context[0], cp::summary_message("SUMMARY-1"));
    assert_eq!(Session::open(&h.hooks.path().unwrap()).unwrap().active_messages(), context);
    assert!(!h.agent.is_running());
    // 压缩后会话照常继续
    prompt(&h, "three").await;
    assert_eq!(script.main_requests().last().unwrap().messages[0], cp::summary_message("SUMMARY-1"));
}

#[tokio::test]
async fn prompts_submitted_during_manual_compaction_run_afterwards() {
    let dir = TempDir::new();
    let (hub, script) = hub(&dir, |cfg| {
        cfg.profiles.get_mut("default").unwrap().compaction = Some(crate::compaction::CompactionConfig { enabled: Some(false), keep_recent_tokens: Some(0), ..Default::default() });
    });
    let h = hub.create_session(&client(&hub, "default"), SessionRequest::default()).unwrap();
    prompt(&h, "one").await;
    prompt(&h, "two").await;
    let guard = h.agent.try_exclusive().unwrap();
    assert!(h.agent.is_running());
    assert!(matches!(h.compact(None).await, Err(crate::sessions::CompactError::Busy)));
    assert_eq!(h.submit("queued".into(), None), agent_core::Disposition::QueuedFollowUp);
    let mut rx = h.subscribe();
    for (m, mode) in guard.release() {
        h.submit_message(m, Some(mode));
    }
    collect(&mut rx).await;
    assert_eq!(first_text(script.main_requests().last().unwrap().messages.last()), "queued");
}

#[tokio::test]
async fn interrupted_tool_call_is_repaired_on_resume() {
    let dir = TempDir::new();
    let (hub1, _) = hub(&dir, |_| {});
    let h = hub1.create_session(&client(&hub1, "default"), SessionRequest::default()).unwrap();
    let (id, path) = (h.id.clone(), h.hooks.path().unwrap());
    drop((h, hub1));
    // 模拟崩溃：助手发出工具调用后进程退出，没有工具结果
    let mut s = Session::open(&path).unwrap();
    s.append_message(Message::user_text("run it")).unwrap();
    let mut a = AssistantMessage::empty("faux");
    a.content.push(Content::ToolCall(agent_ai::ToolCall { id: "call-1".into(), name: "bash".into(), arguments: json!({"command": "sleep 100"}) }));
    a.stop_reason = StopReason::ToolUse;
    s.append_message(Message::Assistant(a)).unwrap();
    drop(s);
    let (hub2, script) = hub(&dir, |_| {});
    let h = hub2.get_or_load(&client(&hub2, "default"), &id).await.unwrap().unwrap();
    let context = h.agent.messages().await;
    let Some(Message::ToolResult(r)) = context.last() else { panic!("tool result expected: {context:?}") };
    assert_eq!((r.tool_call_id.as_str(), r.is_error), ("call-1", true));
    assert_eq!(r.content, vec![Content::Text { text: agent_session::INTERRUPTED_TOOL_TEXT.into() }]);
    assert_eq!(Session::open(&path).unwrap().active_messages(), context);
    hub2.issues.flush(Duration::from_secs(1)).await;
    assert!(hub2.issues.recent(None, 0, 100).iter().any(|e| e.kind == "tool_interrupted"));
    // 继续：模型收到配对完整的 tool_use / tool_result
    prompt(&h, "status?").await;
    let sent = &script.main_requests()[0].messages;
    assert!(matches!(&sent[1], Message::Assistant(a) if a.tool_calls()[0].id == "call-1"));
    assert!(matches!(&sent[2], Message::ToolResult(r) if r.tool_call_id == "call-1"));
}

#[tokio::test]
async fn resume_failures_and_persistence_disabled() {
    let dir = TempDir::new();
    let (hub1, _) = hub(&dir, |_| {});
    let h = hub1.create_session(&client(&hub1, "default"), SessionRequest::default()).unwrap();
    let (id, path) = (h.id.clone(), h.hooks.path().unwrap());
    drop((h, hub1));
    // profile 不再允许原 cwd ⇒ 409（ResumeFailed）
    let (hub2, _) = hub(&dir, |cfg| cfg.profiles.get_mut("default").unwrap().workspace_roots = vec![std::env::temp_dir()]);
    let app = crate::http::test_router(hub2.clone());
    let (status, body) = request(&app, "GET", &format!("/v1/sessions/{id}"), "test-token", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().starts_with("session cannot be resumed: "));
    // 损坏文件 ⇒ 409，不 panic
    std::fs::write(&path, "not json\n{}\n").unwrap();
    let (hub3, _) = hub(&dir, |_| {});
    assert!(hub3.get_or_load(&client(&hub3, "default"), &id).await.err().unwrap().is::<crate::sessions::ResumeFailed>());
    // 不持久化：没有文件，也不能恢复
    let mut hub4 = Hub::new(config(&dir.0), "faux", None, None).unwrap();
    Arc::get_mut(&mut hub4).unwrap().providers.insert("faux".into(), Arc::new(Script::default()) as Arc<dyn Provider>);
    let h = hub4.create_session(&client(&hub4, "default"), SessionRequest::default()).unwrap();
    prompt(&h, "x").await;
    assert!(h.hooks.path().is_none());
    assert!(hub4.get_or_load(&client(&hub4, "default"), &id).await.unwrap().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn session_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let (hub, _) = hub(&dir, |_| {});
    let h = hub.create_session(&client(&hub, "default"), SessionRequest::default()).unwrap();
    prompt(&h, "x").await;
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir.sessions()), 0o700);
    assert_eq!(mode(&dir.sessions().join("default")), 0o700);
    assert_eq!(mode(&h.hooks.path().unwrap()), 0o600);
}
