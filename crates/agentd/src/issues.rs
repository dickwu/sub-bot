//! 有界、脱敏的问题上报；内部错误不使用 tracing，避免递归。
use crate::{
    config::{AlertsConfig, ResolvedTarget},
    webhook::{Deliverer, DeliveryState, DeliveryStatus, RetryPolicy},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, Semaphore};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warn,
    Error,
    Fatal,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueScope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueEvent {
    pub id: String,
    pub ts: u64,
    pub severity: Severity,
    pub kind: String,
    pub component: String,
    pub message: String,
    #[serde(flatten)]
    pub scope: IssueScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub details: Value,
    pub count: u64,
    /// Internal routing metadata, never serialized into the issue payload.
    #[serde(skip)]
    excluded_target: Option<String>,
}
impl IssueEvent {
    pub fn new(severity: Severity, kind: &str, component: &str, message: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            ts: now_ms(),
            severity,
            kind: kind.into(),
            component: component.into(),
            message: message.into(),
            scope: IssueScope::default(),
            tool: None,
            details: json!({}),
            count: 1,
            excluded_target: None,
        }
    }
    pub fn scope(mut self, scope: IssueScope) -> Self {
        self.scope = scope;
        self
    }
    pub fn tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = Some(tool.into());
        self
    }
    pub fn details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScopeKey {
    Session(String),
    Task(String),
}

static GLOBAL: OnceLock<Arc<Reporter>> = OnceLock::new();
pub fn global() -> Option<Arc<Reporter>> {
    GLOBAL.get().cloned()
}
pub fn install(r: Arc<Reporter>) {
    let _ = GLOBAL.set(r);
}
pub fn report(event: IssueEvent) {
    if let Some(r) = global() {
        r.report(event);
    }
}

/// Central redaction, also used for retained webhook response snippets.
pub fn redact(input: &str, secrets: &[String]) -> String {
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let mut text = input.to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(secret, "[redacted]");
    }
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"(?i)\bsk-[a-z0-9_.*-]+",
            r"(?i)\bgh[a-z0-9]*_[a-z0-9_]+",
            r#"(?i)\bBearer\s+[^\s"\x27,;}]+"#,
            r#"(?i)["']?(?:api[_-]?key|token|secret|password|authorization)["']?\s*[:=]\s*(?:"[^"]*"|'[^']*'|[^\s,;}]+)"#,
        ]
        .into_iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect()
    });
    for pattern in patterns {
        text = pattern.replace_all(&text, "[redacted]").into_owned();
    }
    text
}
pub fn sensitive_field(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["secret", "token", "password", "authorization", "api_key", "apikey", "api-key", "prompt", "content", "body", "text", "messages", "arguments", "output"].iter().any(|s| key.contains(s))
}
fn clean_details(value: &mut Value, secrets: &[String]) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !sensitive_field(key));
            // Keys can contain credentials too.
            let old = std::mem::take(map);
            for (key, mut value) in old {
                clean_details(&mut value, secrets);
                map.insert(redact(&key, secrets), value);
            }
        }
        Value::Array(values) => {
            for value in values {
                clean_details(value, secrets);
            }
        }
        Value::String(text) => *text = redact(text, secrets),
        _ => {}
    }
}

#[derive(Default)]
struct State {
    queue: VecDeque<IssueEvent>,
    recent: VecDeque<IssueEvent>,
    subscriptions: HashMap<ScopeKey, (String, Vec<ResolvedTarget>)>,
    dropped: u64,
    busy: bool,
}
type DedupKey = (String, String, Option<String>, Option<String>);
struct Aggregate {
    first: IssueEvent,
    deadline: tokio::time::Instant,
    suppressed: u64,
}
pub struct Reporter {
    state: Mutex<State>,
    notify: Notify,
    capacity: usize,
    window: Duration,
    alerts: Vec<ResolvedTarget>,
    secrets: Vec<String>,
    log_path: Option<PathBuf>,
    io_errors: AtomicU64,
    inflight: AtomicUsize,
    flushing: AtomicBool,
    summaries: AtomicUsize,
    semaphore: Arc<Semaphore>,
    deliverer: Deliverer,
    disabled: bool,
}
impl Reporter {
    pub fn noop() -> Arc<Self> {
        Arc::new(Self::build(&AlertsConfig::default(), None, vec![], RetryPolicy::default(), true))
    }
    pub fn new(cfg: &AlertsConfig, log_dir: Option<PathBuf>, secrets: Vec<String>) -> Arc<Self> {
        Self::with_policy(cfg, log_dir, secrets, RetryPolicy::default())
    }
    fn build(cfg: &AlertsConfig, log_dir: Option<PathBuf>, mut secrets: Vec<String>, policy: RetryPolicy, disabled: bool) -> Self {
        secrets.extend(cfg.resolved.iter().filter_map(|t| t.target.secret.clone()));
        Self {
            state: Mutex::new(State::default()),
            notify: Notify::new(),
            capacity: cfg.queue_capacity.max(1),
            window: Duration::from_secs(cfg.dedup_window_secs),
            alerts: cfg.resolved.clone(),
            secrets,
            log_path: log_dir.map(|p| p.join("issues.log")),
            io_errors: AtomicU64::new(0),
            inflight: AtomicUsize::new(0),
            flushing: AtomicBool::new(false),
            summaries: AtomicUsize::new(0),
            semaphore: Arc::new(Semaphore::new(32)),
            deliverer: Deliverer::new(reqwest::Client::new(), policy),
            disabled,
        }
    }
    fn with_policy(cfg: &AlertsConfig, log_dir: Option<PathBuf>, secrets: Vec<String>, policy: RetryPolicy) -> Arc<Self> {
        let r = Arc::new(Self::build(cfg, log_dir, secrets, policy, false));
        if let Some(path) = &r.log_path {
            if path.parent().is_some_and(|p| std::fs::create_dir_all(p).is_err()) || std::fs::OpenOptions::new().create(true).append(true).open(path).is_err() {
                r.io_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        let weak = Arc::downgrade(&r);
        tokio::spawn(async move {
            let mut dedup: HashMap<DedupKey, Aggregate> = HashMap::new();
            loop {
                let Some(r) = weak.upgrade() else { break };
                r.drain(&mut dedup);
                // Do not retain the reporter forever when the owner disappears.
                let notify = r.notify.notified();
                tokio::select! { _ = notify => {}, _ = tokio::time::sleep(Duration::from_millis(25)) => {} }
                if Arc::strong_count(&r) == 1 {
                    break;
                }
            }
        });
        r
    }
    fn sanitize(&self, mut event: IssueEvent) -> IssueEvent {
        event.message = redact(&event.message, &self.secrets).chars().take(500).collect();
        for text in [&mut event.kind, &mut event.component] {
            *text = redact(text, &self.secrets).chars().take(200).collect();
        }
        for text in [&mut event.scope.request_id, &mut event.scope.client, &mut event.scope.session_id, &mut event.scope.task_id, &mut event.scope.profile, &mut event.scope.model, &mut event.tool]
            .into_iter()
            .flatten()
        {
            *text = redact(text, &self.secrets).chars().take(200).collect();
        }
        clean_details(&mut event.details, &self.secrets);
        if serde_json::to_vec(&event.details).map_or(true, |v| v.len() > 2048) {
            event.details = json!({"truncated": true});
        }
        event
    }
    /// Never awaits network or file I/O. Overflow evicts the oldest queued event.
    pub fn report(&self, event: IssueEvent) {
        if self.disabled {
            return;
        }
        let event = self.sanitize(event);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.queue.len() >= self.capacity {
            state.queue.pop_front();
            state.dropped = state.dropped.saturating_add(1);
        }
        state.queue.push_back(event);
        drop(state);
        self.notify.notify_one();
    }
    pub fn subscribe(&self, key: ScopeKey, client: String, targets: Vec<ResolvedTarget>) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).subscriptions.insert(key, (client, targets));
    }
    pub fn unsubscribe(&self, key: &ScopeKey) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).subscriptions.remove(key);
    }
    pub fn recent(&self, client: Option<&str>, since_ms: u64, limit: usize) -> Vec<IssueEvent> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut events: Vec<_> = state.recent.iter().rev().filter(|e| e.ts >= since_ms && client.is_none_or(|c| e.scope.client.as_deref() == Some(c))).take(limit.min(1000)).cloned().collect();
        events.reverse();
        events
    }
    fn drain(self: &Arc<Self>, dedup: &mut HashMap<DedupKey, Aggregate>) {
        let (events, dropped) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.busy = true;
            (std::mem::take(&mut state.queue), std::mem::take(&mut state.dropped))
        };
        let now = tokio::time::Instant::now();
        let flushing = self.flushing.load(Ordering::SeqCst);
        let expired: Vec<_> = dedup.iter().filter(|(_, a)| flushing || a.deadline <= now).map(|(k, _)| k.clone()).collect();
        for key in expired {
            if let Some(mut a) = dedup.remove(&key) {
                if a.suppressed > 0 {
                    // Summary count includes the first, already delivered event (suppressed + 1).
                    a.first.count = a.suppressed.saturating_add(1);
                    a.first.id = uuid::Uuid::new_v4().to_string();
                    a.first.ts = now_ms();
                    self.emit(a.first);
                }
            }
        }
        if dropped > 0 {
            let mut event = IssueEvent::new(Severity::Warn, "issues_dropped", "runtime", "issue queue overflow; oldest events dropped").details(json!({"dropped": dropped}));
            event.count = dropped;
            self.emit(event);
        }
        for event in events {
            let key = (event.kind.clone(), event.component.clone(), event.scope.session_id.clone(), event.scope.client.clone());
            if let Some(a) = dedup.get_mut(&key) {
                a.suppressed = a.suppressed.saturating_add(1);
                // Do not suppress a severity escalation below a receiver's threshold.
                if event.severity > a.first.severity {
                    a.first.severity = event.severity;
                    self.emit(event);
                }
                continue;
            }
            if !self.window.is_zero() && !flushing && event.kind != "webhook_delivery_failed" {
                // Bound dedup bookkeeping too. Evicting a window emits its pending summary.
                if dedup.len() >= 1000 {
                    if let Some(key) = dedup.keys().next().cloned() {
                        if let Some(mut a) = dedup.remove(&key) {
                            if a.suppressed > 0 {
                                a.first.count = a.suppressed.saturating_add(1);
                                a.first.id = uuid::Uuid::new_v4().to_string();
                                self.emit(a.first);
                            }
                        }
                    }
                }
                if let Some(deadline) = now.checked_add(self.window) {
                    dedup.insert(key, Aggregate { first: event.clone(), deadline, suppressed: 0 });
                }
            }
            self.emit(event);
        }
        self.summaries.store(dedup.values().filter(|a| a.suppressed > 0).count(), Ordering::SeqCst);
        self.state.lock().unwrap_or_else(|e| e.into_inner()).busy = false;
    }
    fn emit(self: &Arc<Self>, event: IssueEvent) {
        let failure = event.kind == "webhook_delivery_failed";
        let excluded_host = if failure && event.excluded_target.is_none() { event.details.get("host").and_then(Value::as_str) } else { None };
        let mut targets: Vec<_> = self
            .alerts
            .iter()
            .filter(|t| event.severity >= t.min_severity.unwrap_or(Severity::Error) && event.excluded_target.as_ref() != Some(&t.target.url) && excluded_host.is_none_or(|h| host(&t.target.url) != h))
            .cloned()
            .collect();
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !failure {
                for key in [event.scope.session_id.as_ref().map(|s| ScopeKey::Session(s.clone())), event.scope.task_id.as_ref().map(|s| ScopeKey::Task(s.clone()))].into_iter().flatten() {
                    if let Some((client, ts)) = state.subscriptions.get(&key) {
                        if event.scope.client.as_ref() == Some(client) {
                            targets.extend(ts.iter().filter(|t| t.wants_issue(event.severity)).cloned());
                        }
                    }
                }
            }
            if state.recent.len() == 1000 {
                state.recent.pop_front();
            }
            state.recent.push_back(event.clone());
        }
        if let Some(path) = &self.log_path {
            let result = (|| -> std::io::Result<()> {
                let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                let mut line = serde_json::to_vec(&event)?;
                line.push(b'\n');
                file.write_all(&line)
            })();
            if result.is_err() {
                self.io_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        for target in targets {
            let r = self.clone();
            let event = event.clone();
            self.inflight.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let _permit = r.semaphore.acquire().await;
                let status = r.deliverer.deliver(&target.target, "issue", &json!(event), &|_| {}).await;
                if !failure && status.state == DeliveryState::Failed {
                    let mut failure = Self::delivery_failure(&status, &host(&target.target.url), event.scope.client.as_deref());
                    failure.excluded_target = Some(target.target.url.clone());
                    r.report(failure);
                }
                r.inflight.fetch_sub(1, Ordering::SeqCst);
                r.notify.notify_one();
            });
        }
    }
    /// Failure issues route only to alerts, excluding the failed host, and never recurse.
    pub fn on_delivery_failed(&self, status: &DeliveryStatus, host: &str, client: Option<&str>) {
        if status.state != DeliveryState::Failed {
            return;
        }
        self.report(Self::delivery_failure(status, host, client));
    }
    fn delivery_failure(status: &DeliveryStatus, host: &str, client: Option<&str>) -> IssueEvent {
        IssueEvent::new(Severity::Error, "webhook_delivery_failed", "webhook", "webhook delivery exhausted retries")
            .scope(IssueScope { client: client.map(str::to_owned), ..Default::default() })
            .details(json!({"event": status.event, "host": host, "attempts": status.attempts, "lastStatus": status.last_status}))
    }
    pub async fn flush(&self, timeout: Duration) {
        if self.disabled {
            return;
        }
        self.flushing.store(true, Ordering::SeqCst);
        self.notify.notify_one();
        let _ = tokio::time::timeout(timeout, async {
            loop {
                let empty = {
                    let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.queue.is_empty() && s.dropped == 0 && !s.busy
                };
                if empty && self.inflight.load(Ordering::SeqCst) == 0 && self.summaries.load(Ordering::SeqCst) == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        self.flushing.store(false, Ordering::SeqCst);
    }
    /// Synchronous CLI/startup path: still records JSONL/recent, without duplicate background delivery.
    pub async fn test_alerts(&self, event: IssueEvent) -> Vec<(String, DeliveryStatus)> {
        let event = self.sanitize(event);
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.recent.len() == 1000 {
                state.recent.pop_front();
            }
            state.recent.push_back(event.clone());
        }
        if let Some(path) = &self.log_path {
            let result = (|| -> std::io::Result<()> {
                let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                writeln!(f, "{}", json!(event))
            })();
            if result.is_err() {
                self.io_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        futures::future::join_all(self.alerts.iter().map(|target| {
            let event = &event;
            async move {
                let status = self.deliverer.deliver(&target.target, "issue", &json!(event), &|_| {}).await;
                if status.state == DeliveryState::Failed && event.kind != "webhook_delivery_failed" {
                    let mut failure = Self::delivery_failure(&status, &host(&target.target.url), event.scope.client.as_deref());
                    failure.excluded_target = Some(target.target.url.clone());
                    self.report(failure);
                }
                (host(&target.target.url), status)
            }
        }))
        .await
    }
}
pub fn host(url: &str) -> String {
    reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webhook::{verify, WebhookTarget};
    use axum::{
        body::Bytes,
        http::{HeaderMap, StatusCode},
        routing::post,
        Router,
    };

    fn event(session: &str, severity: Severity) -> IssueEvent {
        IssueEvent::new(severity, "tool_timeout", "tool", "operation timed out").scope(IssueScope { session_id: Some(session.into()), client: Some("bot".into()), ..Default::default() })
    }
    fn policy() -> RetryPolicy {
        RetryPolicy { max_attempts: 2, initial_backoff: Duration::from_millis(5), max_backoff: Duration::from_millis(10), attempt_timeout: Duration::from_secs(1) }
    }
    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    async fn receiver(code: StatusCode, secret: &str) -> (ResolvedTarget, Arc<Mutex<Vec<Value>>>, Server) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let records = seen.clone();
        let key = secret.to_owned();
        let app = Router::new().route(
            "/hook",
            post(move |headers: HeaderMap, body: Bytes| {
                let records = records.clone();
                let key = key.clone();
                async move {
                    assert!(verify(key.as_bytes(), headers["X-SubBot-Signature"].to_str().unwrap(), &body, (now_ms() / 1000) as i64, 5));
                    let value: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(value["event"], "issue");
                    records.lock().unwrap().push(value);
                    code
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = ResolvedTarget {
            target: WebhookTarget { url: format!("http://{}/hook", listener.local_addr().unwrap()), secret: Some(secret.into()) },
            events: Some(vec!["issues".into()]),
            min_severity: Some(Severity::Warn),
        };
        (
            target,
            seen,
            Server(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            })),
        )
    }

    #[tokio::test]
    async fn redaction_truncation_and_jsonl() {
        let dir = std::env::temp_dir().join(format!("sub-bot-issues-{}", uuid::Uuid::new_v4()));
        let r = Reporter::new(&AlertsConfig::default(), Some(dir.clone()), vec!["configured-private-value".into()]);
        let message = "sk-private Bearer bearer-value ghp_githubvalue api_key=keyvalue token=tokvalue secret='hidden value' password=pw authorization=auth configured-private-value";
        r.report(IssueEvent::new(Severity::Error, "redaction", "test", message).details(json!({"nested": {"prompt": "private", "content": "private", "body":"private", "text":"private", "messages": [], "arguments": {}, "output":"private", "secret": "private", "apiKey": "private", "safe": "sk-details"}})));
        r.report(IssueEvent::new(Severity::Warn, "long", "test", "界".repeat(1000)).details(json!({"safe": "x".repeat(2048)})));
        r.flush(Duration::from_secs(2)).await;
        let recent = r.recent(None, 0, 100);
        assert_eq!(recent.len(), 2);
        for secret in ["sk-private", "bearer-value", "githubvalue", "keyvalue", "tokvalue", "hidden value", "pw", "configured-private-value"] {
            assert!(!recent[0].message.contains(secret), "{secret}");
        }
        assert_eq!(recent[0].details, json!({"nested": {"safe": "[redacted]"}}));
        assert_eq!(recent[1].message.chars().count(), 500);
        assert_eq!(recent[1].details, json!({"truncated":true}));
        let lines = std::fs::read_to_string(dir.join("issues.log")).unwrap();
        assert_eq!(lines.lines().count(), 2);
        assert!(!lines.contains("private"));
        assert!(r.recent(Some("other"), 0, 10).is_empty());
        assert!(r.recent(None, u64::MAX, 10).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn dedupe_aggregates_at_window_end() {
        let r = Reporter::new(&AlertsConfig { dedup_window_secs: 2, ..Default::default() }, None, vec![]);
        r.report(event("one", Severity::Error));
        tokio::task::yield_now().await;
        assert_eq!(r.recent(None, 0, 100).len(), 1);
        r.report(event("one", Severity::Error));
        r.report(event("one", Severity::Error));
        tokio::task::yield_now().await;
        assert_eq!(r.recent(None, 0, 100).len(), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        let events = r.recent(None, 0, 100);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].count, 3); // suppressed + first, not three new occurrences
        assert_ne!(events[0].id, events[1].id);
        r.report(event("one", Severity::Error));
        tokio::task::yield_now().await;
        assert_eq!(r.recent(None, 0, 100).len(), 3);
    }

    #[tokio::test]
    async fn drop_oldest_counts_and_noop() {
        let r = Reporter::new(&AlertsConfig { queue_capacity: 2, dedup_window_secs: 0, ..Default::default() }, None, vec![]);
        for n in 0..5 {
            r.report(event(&n.to_string(), Severity::Error));
        }
        r.flush(Duration::from_secs(2)).await;
        let events = r.recent(None, 0, 100);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, "issues_dropped");
        assert_eq!(events[0].count, 3);
        assert_eq!(events[1].scope.session_id.as_deref(), Some("3"));
        assert_eq!(events[2].scope.session_id.as_deref(), Some("4"));
        let noop = Reporter::noop();
        noop.report(event("x", Severity::Fatal));
        noop.flush(Duration::ZERO).await;
        assert!(noop.recent(None, 0, 100).is_empty());
    }

    #[tokio::test]
    async fn scope_severity_and_per_target_signatures() {
        let (mut alert, all, _a) = receiver(StatusCode::NO_CONTENT, "alert-key").await;
        let (mut target, own, _b) = receiver(StatusCode::NO_CONTENT, "session-key").await;
        alert.min_severity = Some(Severity::Error);
        target.min_severity = Some(Severity::Fatal);
        let r = Reporter::with_policy(&AlertsConfig { resolved: vec![alert], dedup_window_secs: 0, ..Default::default() }, None, vec![], policy());
        r.subscribe(ScopeKey::Session("one".into()), "bot".into(), vec![target.clone()]);
        r.subscribe(ScopeKey::Task("task".into()), "other-client".into(), vec![target]);
        r.report(event("one", Severity::Warn));
        r.report(event("one", Severity::Error));
        r.report(event("one", Severity::Fatal));
        r.report(event("two", Severity::Fatal).scope(IssueScope { session_id: Some("two".into()), task_id: Some("task".into()), client: Some("bot".into()), ..Default::default() }));
        r.flush(Duration::from_secs(3)).await;
        assert_eq!(all.lock().unwrap().len(), 3);
        assert_eq!(own.lock().unwrap().len(), 1);
        assert_eq!(own.lock().unwrap()[0]["data"]["sessionId"], "one");
        r.unsubscribe(&ScopeKey::Session("one".into()));
        r.report(event("one", Severity::Fatal));
        r.flush(Duration::from_secs(3)).await;
        assert_eq!(own.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn independent_fanout_failure_alert_no_recursion() {
        let (alert, all, _a) = receiver(StatusCode::NO_CONTENT, "alert-key").await;
        let (good, healthy, _b) = receiver(StatusCode::NO_CONTENT, "good-key").await;
        let (bad, failing, _c) = receiver(StatusCode::INTERNAL_SERVER_ERROR, "bad-key").await;
        let dir = std::env::temp_dir().join(format!("sub-bot-fanout-{}", uuid::Uuid::new_v4()));
        let r = Reporter::with_policy(&AlertsConfig { resolved: vec![alert], dedup_window_secs: 0, ..Default::default() }, Some(dir.clone()), vec![], policy());
        r.subscribe(ScopeKey::Session("one".into()), "bot".into(), vec![bad, good]);
        r.report(event("one", Severity::Error));
        r.flush(Duration::from_secs(3)).await;
        assert_eq!(healthy.lock().unwrap().len(), 1);
        assert_eq!(failing.lock().unwrap().len(), 2);
        let all = all.lock().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1]["data"]["kind"], "webhook_delivery_failed");
        assert_eq!(all[1]["data"]["details"]["attempts"], 2);
        assert_eq!(all[1]["data"]["details"]["lastStatus"], 500);
        assert_eq!(all[1]["data"]["details"]["host"], "127.0.0.1");
        assert_ne!(all[0]["deliveryId"], healthy.lock().unwrap()[0]["deliveryId"]);
        assert_eq!(std::fs::read_to_string(dir.join("issues.log")).unwrap().lines().count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn task_subscription_and_cli_ignores_threshold() {
        let (mut target, seen, _server) = receiver(StatusCode::NO_CONTENT, "task-key").await;
        target.min_severity = Some(Severity::Fatal);
        let r = Reporter::with_policy(&AlertsConfig { resolved: vec![target.clone()], dedup_window_secs: 0, ..Default::default() }, None, vec![], policy());
        let results = r.test_alerts(IssueEvent::new(Severity::Warn, "test", "runtime", "diagnostic")).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.state, DeliveryState::Delivered);
        assert_eq!(seen.lock().unwrap().len(), 1);
        r.subscribe(ScopeKey::Task("task".into()), "bot".into(), vec![target]);
        r.report(event("one", Severity::Fatal).scope(IssueScope { task_id: Some("task".into()), client: Some("bot".into()), ..Default::default() }));
        r.flush(Duration::from_secs(3)).await;
        assert_eq!(seen.lock().unwrap().len(), 3); // independent alert and task receivers
    }

    #[tokio::test]
    async fn multiple_failing_alerts_stop_after_failure_notifications() {
        let (one, first, _a) = receiver(StatusCode::INTERNAL_SERVER_ERROR, "one-key").await;
        let (two, second, _b) = receiver(StatusCode::INTERNAL_SERVER_ERROR, "two-key").await;
        let r = Reporter::with_policy(&AlertsConfig { resolved: vec![one, two], dedup_window_secs: 0, ..Default::default() }, None, vec![], policy());
        r.report(event("one", Severity::Error));
        r.flush(Duration::from_secs(3)).await;
        assert_eq!(r.recent(None, 0, 10).len(), 3);
        assert_eq!(first.lock().unwrap().len(), 4);
        assert_eq!(second.lock().unwrap().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_has_a_deadline() {
        let r = Reporter::new(&AlertsConfig::default(), None, vec![]);
        r.inflight.store(1, Ordering::SeqCst);
        let start = tokio::time::Instant::now();
        r.flush(Duration::from_secs(1)).await;
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        r.inflight.store(0, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn failing_alerts_do_not_recurse_and_io_errors_are_nonfatal() {
        let (bad, failing, _a) = receiver(StatusCode::INTERNAL_SERVER_ERROR, "bad-key").await;
        let r = Reporter::with_policy(&AlertsConfig { resolved: vec![bad], dedup_window_secs: 0, ..Default::default() }, Some(PathBuf::from("/dev/null/impossible")), vec![], policy());
        r.report(event("one", Severity::Error));
        r.flush(Duration::from_secs(3)).await;
        assert_eq!(failing.lock().unwrap().len(), 2);
        assert_eq!(r.recent(None, 0, 10).len(), 2);
        assert!(r.io_errors.load(Ordering::Relaxed) >= 1);
    }
}
