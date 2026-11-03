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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 账号名（多账号；从不包含 key）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueEvent {
    pub id: String,
    pub ts: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ts: Option<u64>,
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
    /// Snapshot for task deliveries that can outlive their subscriptions.
    #[serde(skip)]
    subscription_targets: Option<Vec<ResolvedTarget>>,
}
impl IssueEvent {
    pub fn new(severity: Severity, kind: &str, component: &str, message: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            ts: now_ms(),
            last_ts: None,
            severity,
            kind: kind.into(),
            component: component.into(),
            message: message.into(),
            scope: IssueScope::default(),
            tool: None,
            details: json!({}),
            count: 1,
            excluded_target: None,
            subscription_targets: None,
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

/// `Bearer <value>` 里的 value 是否像凭据：至少 8 个字符，且含非字母字符（数字或 `-._~+/=*` 等：JWT、sk-…、
/// gh*_…、base64/hex 不透明 token、被截断回显的 `abc***xyz`），或者长度 >= 20（无符号的长随机串）。
/// "bearer token"、"Bearer authentication." 这类普通单词不算（句末标点先去掉）。
fn credential_like(value: &str) -> bool {
    let len = value.chars().count();
    len >= 8 && (len >= 20 || value.chars().any(|c| !c.is_alphabetic()))
}

/// Central redaction, also used for retained webhook response snippets.
///
/// Masks configured secret values, `sk-…`, GitHub tokens, JWTs, `Bearer <credential>` and
/// `api_key|token|secret|password|authorization = <value>` assignments (incl. JSON), but keeps
/// plain phrases such as "missing or unknown bearer token", "api key", "token expired" readable.
pub fn redact(input: &str, secrets: &[String]) -> String {
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    static BEARER: OnceLock<regex::Regex> = OnceLock::new();
    let mut text = input.to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(secret, "[redacted]");
    }
    // The value must look like a credential, not an English word ("bearer token").
    let bearer = BEARER.get_or_init(|| regex::Regex::new(r#"(?i)\b(Bearer)\s+([^\s"'`(),;<>\[\]{}]+)"#).expect("valid bearer pattern"));
    text = bearer
        .replace_all(&text, |c: &regex::Captures<'_>| {
            let value = &c[2];
            let core = value.trim_end_matches(['.', ',', ':', '!', '?']);
            if credential_like(core) {
                format!("{} [redacted]{}", &c[1], &value[core.len()..])
            } else {
                c[0].to_owned()
            }
        })
        .into_owned();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"(?i)\bsk-[a-z0-9_.*-]+",
            // GitHub: ghp_ / gho_ / ghu_ / ghs_ / ghr_ and fine-grained github_pat_ (not arbitrary gh*_ identifiers).
            r"(?i)\b(?:gh[pousr]_[a-z0-9_]{8,}|github_pat_[a-z0-9_]{8,})",
            // JWT: base64url(header).base64url(payload).signature
            r"\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]*",
            // Assignments, also JSON (`"api_key":"…"`) and `Authorization: Bearer|Basic|Token <value>`.
            r#"(?i)["']?(?:api[_ -]?key|token|secret|password|authorization)["']?\s*[:=]\s*(?:(?:bearer|basic|token)\s+)?(?:"[^"]*"|'[^']*'|[^\s,;}]+)"#,
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
    routes: HashMap<ScopeKey, (String, Vec<ResolvedTarget>, tokio::time::Instant)>,
    subscriptions: HashMap<ScopeKey, (String, Vec<ResolvedTarget>)>,
    dropped: u64,
    busy: bool,
}
type DedupKey = (String, String, Option<String>, Option<String>, Option<String>);
struct Aggregate {
    first: IssueEvent,
    deadline: tokio::time::Instant,
    suppressed: u64,
}
pub struct Reporter {
    store: Mutex<Option<std::sync::Weak<crate::store::Store>>>,
    state: Mutex<State>,
    notify: Notify,
    capacity: usize,
    window: Duration,
    alerts: Vec<ResolvedTarget>,
    secrets: Mutex<Vec<String>>,
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
    pub(crate) fn attach_store(&self, store: &Arc<crate::store::Store>) {
        *self.store.lock().unwrap() = Some(Arc::downgrade(store));
        store.report_corruption(self);
    }

    pub fn noop() -> Arc<Self> {
        Arc::new(Self::build(&AlertsConfig::default(), None, vec![], RetryPolicy::default(), true))
    }
    pub fn new(cfg: &AlertsConfig, log_dir: Option<PathBuf>, secrets: Vec<String>) -> Arc<Self> {
        Self::with_policy(cfg, log_dir, secrets, RetryPolicy::default())
    }
    fn build(cfg: &AlertsConfig, log_dir: Option<PathBuf>, mut secrets: Vec<String>, policy: RetryPolicy, disabled: bool) -> Self {
        secrets.extend(cfg.resolved.iter().filter_map(|t| t.target.secret.clone()));
        Self {
            store: Mutex::new(None),
            state: Mutex::new(State::default()),
            notify: Notify::new(),
            capacity: cfg.queue_capacity.max(1),
            window: Duration::from_secs(cfg.dedup_window_secs),
            alerts: cfg.resolved.clone(),
            secrets: Mutex::new(secrets),
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
    pub fn add_secret(&self, secret: String) {
        self.secrets.lock().unwrap().push(secret);
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        redact(text, &self.secrets.lock().unwrap())
    }

    fn sanitize(&self, mut event: IssueEvent) -> IssueEvent {
        let secrets = self.secrets.lock().unwrap();
        event.message = redact(&event.message, &secrets).chars().take(500).collect();
        for text in [&mut event.kind, &mut event.component] {
            *text = redact(text, &secrets).chars().take(200).collect();
        }
        for text in [
            &mut event.scope.request_id,
            &mut event.scope.client,
            &mut event.scope.session_id,
            &mut event.scope.task_id,
            &mut event.scope.profile,
            &mut event.scope.model,
            &mut event.scope.provider,
            &mut event.scope.account,
            &mut event.tool,
        ]
        .into_iter()
        .flatten()
        {
            *text = redact(text, &secrets).chars().take(200).collect();
        }
        clean_details(&mut event.details, &secrets);
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
        let mut event = self.sanitize(event);
        if event.subscription_targets.is_none() {
            event.subscription_targets = Some(self.subscription_targets(&event.scope));
        }
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
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((client, targets)) = state.subscriptions.remove(key) {
            if let Some(deadline) = tokio::time::Instant::now().checked_add(self.window) {
                state.routes.insert(key.clone(), (client, targets, deadline));
            }
        }
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
            state.routes.retain(|_, (_, _, deadline)| *deadline > tokio::time::Instant::now());
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
                    self.deliver(a.first);
                }
            }
        }
        if dropped > 0 {
            let mut event = IssueEvent::new(Severity::Warn, "issues_dropped", "runtime", "issue queue overflow; oldest events dropped").details(json!({"dropped": dropped}));
            event.count = dropped;
            self.emit(event);
        }
        for event in events {
            let key = (event.kind.clone(), event.component.clone(), event.scope.session_id.clone(), event.scope.client.clone(), event.scope.task_id.clone());
            if let Some(a) = dedup.get_mut(&key) {
                a.suppressed = a.suppressed.saturating_add(1);
                a.first.count = a.suppressed.saturating_add(1);
                a.first.last_ts = Some(event.ts.max(a.first.last_ts.unwrap_or(a.first.ts)));
                // Escalations still reach receivers immediately, but belong to the same stored issue.
                let escalated = event.severity > a.first.severity;
                a.first.severity = a.first.severity.max(event.severity);
                {
                    let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(stored) = state.recent.iter_mut().find(|stored| stored.id == a.first.id) {
                        *stored = a.first.clone();
                    }
                }
                // JSONL snapshots are upserts by the first issue's id, not additional occurrences.
                self.append_log(&a.first);
                if escalated {
                    self.deliver(event);
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
                                self.deliver(a.first);
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
    pub fn subscription_targets(&self, scope: &IssueScope) -> Vec<ResolvedTarget> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.routes.retain(|_, (_, _, deadline)| *deadline > tokio::time::Instant::now());
        let mut targets = Vec::new();
        for key in [scope.session_id.as_ref().map(|s| ScopeKey::Session(s.clone())), scope.task_id.as_ref().map(|s| ScopeKey::Task(s.clone()))].into_iter().flatten() {
            let route = state.subscriptions.get(&key).map(|(client, targets)| (client, targets)).or_else(|| state.routes.get(&key).map(|(c, ts, _)| (c, ts)));
            if let Some((client, ts)) = route {
                if scope.client.as_ref() == Some(client) {
                    targets.extend(ts.iter().cloned());
                }
            }
        }
        targets
    }
    fn emit(self: &Arc<Self>, event: IssueEvent) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.recent.len() == 1000 {
                state.recent.pop_front();
            }
            state.recent.push_back(event.clone());
        }
        self.append_log(&event);
        self.deliver(event);
    }
    fn append_log(&self, event: &IssueEvent) {
        if let Some(path) = &self.log_path {
            let result = (|| -> std::io::Result<()> {
                let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                let mut line = serde_json::to_vec(event)?;
                line.push(b'\n');
                file.write_all(&line)
            })();
            if result.is_err() {
                self.io_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    /// Delivery snapshots (including summaries) must not create additional recent issues.
    fn deliver(self: &Arc<Self>, event: IssueEvent) {
        let failure = event.kind == "webhook_delivery_failed";
        let mut targets: Vec<_> = self.alerts.iter().filter(|t| event.severity >= t.min_severity.unwrap_or(Severity::Error)).cloned().collect();
        let subscriptions = event.subscription_targets.clone().unwrap_or_else(|| self.subscription_targets(&event.scope));
        targets.extend(subscriptions.into_iter().filter(|t| t.wants_issue(event.severity)));
        targets.retain(|t| event.excluded_target.as_ref().is_none_or(|url| *url != normalize_url(&t.target.url)));
        for target in targets {
            if let Some(store) = self.store.lock().unwrap().as_ref().and_then(std::sync::Weak::upgrade) {
                let delivery = crate::store::Delivery {
                    url: target.target.url.clone(),
                    payload: json!(event),
                    client: event.scope.client.clone(),
                    session_id: event.scope.session_id.clone(),
                    task_id: event.scope.task_id.clone(),
                    index: None,
                    status: DeliveryStatus { delivery_id: uuid::Uuid::new_v4().to_string(), event: "issue".into(), state: DeliveryState::Pending, attempts: 0, last_status: None, last_error: None },
                };
                if store.save_delivery(&delivery).is_ok() {
                    store.start(delivery, Some(target.target), self, RetryPolicy::default());
                } else {
                    self.io_errors.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            let r = self.clone();
            let event = event.clone();
            self.inflight.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let _permit = r.semaphore.acquire().await;
                let status = r.deliverer.deliver(&target.target, "issue", &json!(event), &|_| {}).await;
                if !failure && status.state == DeliveryState::Failed {
                    r.report(Self::delivery_failure(&status, &target.target.url, event.scope.client.as_deref()));
                }
                r.inflight.fetch_sub(1, Ordering::SeqCst);
                r.notify.notify_one();
            });
        }
    }
    /// Task delivery failures reach alerts and the captured owning-client subscriptions, never the failed URL.
    pub fn on_delivery_failed(&self, status: &DeliveryStatus, url: &str, scope: IssueScope, targets: &[ResolvedTarget]) {
        if status.state != DeliveryState::Failed || status.event == "issue" {
            return;
        }
        let mut failure = Self::delivery_failure(status, url, scope.client.as_deref()).scope(scope);
        failure.subscription_targets = Some(targets.to_vec());
        self.report(failure);
    }
    pub(crate) fn delivery_failure(status: &DeliveryStatus, url: &str, client: Option<&str>) -> IssueEvent {
        let mut failure = IssueEvent::new(Severity::Error, "webhook_delivery_failed", "webhook", "webhook delivery exhausted retries")
            .scope(IssueScope { client: client.map(str::to_owned), ..Default::default() })
            .details(json!({"event": status.event, "host": host(url), "attempts": status.attempts, "lastStatus": status.last_status}));
        failure.excluded_target = Some(normalize_url(url));
        failure
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
                    let store = self.store.lock().unwrap().as_ref().and_then(std::sync::Weak::upgrade);
                    if let Some(store) = store {
                        store.flush().await;
                        // Delivery failures may have enqueued new issues while we waited.
                        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                        if !state.queue.is_empty() || state.busy {
                            continue;
                        }
                    }
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
                    self.report(Self::delivery_failure(&status, &target.target.url, event.scope.client.as_deref()));
                }
                (host(&target.target.url), status)
            }
        }))
        .await
    }
}
fn normalize_url(url: &str) -> String {
    reqwest::Url::parse(url).map_or_else(|_| url.to_owned(), |url| url.to_string())
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

    #[test]
    fn redaction_keeps_plain_phrases_readable() {
        for text in [
            "unauthorized request (missing or unknown bearer token)",
            "Bearer token expired",
            "invalid bearer tokens.",
            "Bearer authentication failed",
            "api key missing; set an API key",
            "token expired, refresh the token",
            "Authorization header missing",
            "secret rotation is due",
            "max_tokens: 64001 > 64000, input_tokens=12",
            "ghost_session closed",
            "risk-free task-runner",
        ] {
            assert_eq!(redact(text, &[]), text);
        }
    }

    #[test]
    fn redaction_masks_real_secrets() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let cases = [
            ("Authorization: Bearer abcDEF1234567890ghiJKL".to_string(), "abcDEF"),
            ("authorization=Bearer abcDEF1234567890ghiJKL".to_string(), "abcDEF"),
            ("curl -H 'Authorization: Bearer abcDEF1234567890' failed".to_string(), "abcDEF"),
            ("sent Bearer sk-proj-abc123def456".to_string(), "abc123"),
            (format!("rejected Bearer {jwt}"), "eyJhbGci"),
            (format!("id_token {jwt}"), "dozjgNry"),
            ("Bearer opaqueTokenWithoutDigitsXYZ".to_string(), "opaqueToken"),
            ("Bearer abc***xyz".to_string(), "abc***"),
            ("Bearer ghp_abcdefghijklmnop1234".to_string(), "abcdefghijklmnop"),
            ("leaked ghp_abcdefghijklmnop1234 and github_pat_11ABCDEFG_xyz123456".to_string(), "abcdefghijklmnop"),
            ("Incorrect API key provided: sk-proj-abc***xyz. See".to_string(), "sk-proj"),
            ("token=tokvalue&x=1".to_string(), "tokvalue"),
            ("access_token: abcdef".to_string(), "abcdef"),
            ("password = 'hunter 2'".to_string(), "hunter"),
            ("api key: plainvalue".to_string(), "plainvalue"),
            (r#"{"api_key":"json-value","apiKey": "camel-value"}"#.to_string(), "-value"),
            ("client secret configured-private-value echoed".to_string(), "configured-private-value"),
        ];
        for (text, secret) in cases {
            let redacted = redact(&text, &["configured-private-value".into()]);
            assert!(!redacted.contains(secret), "{text} -> {redacted}");
            assert!(redacted.contains("[redacted]"), "{text} -> {redacted}");
        }
        // Readable context around a masked value is kept.
        assert_eq!(redact("Authorization: Bearer abcDEF1234567890ghiJKL rejected", &[]), "[redacted] rejected");
        assert_eq!(redact("got Bearer abcDEF1234567890. retry", &[]), "got Bearer [redacted]. retry");
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
    async fn dedupe_updates_recent_before_window_end() {
        let dir = std::env::temp_dir().join(format!("sub-bot-dedup-{}", uuid::Uuid::new_v4()));
        let r = Reporter::new(&AlertsConfig { dedup_window_secs: 2, ..Default::default() }, Some(dir.clone()), vec![]);
        let first = event("one", Severity::Error);
        let id = first.id.clone();
        let ts = first.ts;
        r.report(first);
        tokio::task::yield_now().await;
        assert_eq!(r.recent(None, 0, 100).len(), 1);
        assert!(json!(r.recent(None, 0, 100)[0]).get("lastTs").is_none());
        for offset in 1..=2 {
            let mut next = event("one", Severity::Error);
            next.ts = ts + offset;
            r.report(next);
        }
        tokio::task::yield_now().await;
        let events = r.recent(Some("bot"), 0, 100);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, id);
        assert_eq!(events[0].ts, ts);
        assert_eq!(events[0].count, 3);
        assert_eq!(events[0].last_ts, Some(ts + 2));
        let before = serde_json::to_value(&events).unwrap();
        let log = std::fs::read_to_string(dir.join("issues.log")).unwrap();
        let snapshots: Vec<Value> = log.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(snapshots.len(), 3);
        for (i, snapshot) in snapshots.iter().enumerate() {
            assert_eq!(snapshot["id"], id);
            assert_eq!(snapshot["count"], i + 1);
        }
        assert_eq!(snapshots[2], before[0]);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(serde_json::to_value(r.recent(None, 0, 100)).unwrap(), before);
        assert_eq!(std::fs::read_to_string(dir.join("issues.log")).unwrap(), log);
        r.report(event("one", Severity::Error));
        tokio::task::yield_now().await;
        let events = r.recent(None, 0, 100);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].count, 3);
        assert_eq!(events[1].count, 1);
        assert_ne!(events[0].id, events[1].id);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn dedupe_webhook_summary_agrees_with_recent() {
        // Exercise both direct deliveries and the store-backed delivery path.
        for persisted in [false, true] {
            let (target, seen, _server) = receiver(StatusCode::NO_CONTENT, "dedup-key").await;
            let dir = std::env::temp_dir().join(format!("sub-bot-dedup-store-{}", uuid::Uuid::new_v4()));
            let store = persisted.then(|| crate::store::Store::open(&dir, Duration::from_secs(3600)).unwrap());
            let r = Reporter::with_policy(&AlertsConfig { resolved: vec![target], dedup_window_secs: 1, ..Default::default() }, None, vec![], policy());
            if let Some(store) = &store {
                r.attach_store(store);
            }
            for _ in 0..3 {
                r.report(event("one", Severity::Error));
            }
            tokio::task::yield_now().await;
            let recent = r.recent(None, 0, 100);
            assert_eq!(recent.len(), 1);
            assert_eq!(recent[0].count, 3);
            tokio::time::timeout(Duration::from_secs(5), async {
                while seen.lock().unwrap().len() < 2 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            r.flush(Duration::from_secs(2)).await;
            let deliveries = seen.lock().unwrap().clone();
            assert_eq!(deliveries.len(), 2);
            assert_eq!(deliveries[0]["data"]["count"], 1);
            assert_eq!(deliveries[1]["data"]["count"], 3);
            assert_ne!(deliveries[0]["data"]["id"], deliveries[1]["data"]["id"]);
            assert_eq!(serde_json::to_value(r.recent(None, 0, 100)).unwrap(), json!(recent));
            if persisted {
                drop(r);
                drop(store);
                std::fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn dedupe_escalation_updates_same_recent_issue() {
        let (mut target, seen, _server) = receiver(StatusCode::NO_CONTENT, "escalation-key").await;
        target.min_severity = Some(Severity::Fatal);
        let r = Reporter::with_policy(&AlertsConfig { resolved: vec![target], dedup_window_secs: 60, ..Default::default() }, None, vec![], policy());
        let first = event("one", Severity::Warn);
        let id = first.id.clone();
        r.report(first);
        r.report(event("one", Severity::Fatal));
        tokio::task::yield_now().await;
        let recent = r.recent(None, 0, 100);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].id, id);
        assert_eq!(recent[0].count, 2);
        assert_eq!(recent[0].severity, Severity::Fatal);
        r.flush(Duration::from_secs(3)).await;
        let deliveries = seen.lock().unwrap();
        assert_eq!(deliveries.len(), 2); // immediate escalation plus summary
        assert!(deliveries.iter().any(|d| d["data"]["count"] == 1));
        assert!(deliveries.iter().any(|d| d["data"]["count"] == 2));
        assert_eq!(serde_json::to_value(r.recent(None, 0, 100)).unwrap(), json!(recent));
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
    async fn task_delivery_failure_routes_to_alerts_and_own_subscriptions_after_unsubscribe() {
        for key in [ScopeKey::Task("task".into()), ScopeKey::Session("session".into())] {
            let (alert, alerts, _a) = receiver(StatusCode::NO_CONTENT, "alert-key").await;
            let (mut failed, excluded, _b) = receiver(StatusCode::NO_CONTENT, "failed-key").await;
            let (subscribed, own, _c) = receiver(StatusCode::NO_CONTENT, "own-key").await;
            let (mut filtered, ignored, _d) = receiver(StatusCode::NO_CONTENT, "filtered-key").await;
            failed.target.url.push_str("?token=private-route");
            let mut fatal_only = filtered.clone();
            fatal_only.min_severity = Some(Severity::Fatal);
            filtered.events = Some(vec!["task.completed".into()]);
            let scope = IssueScope { client: Some("bot".into()), task_id: Some("task".into()), session_id: Some("session".into()), ..Default::default() };
            let r = Reporter::with_policy(&AlertsConfig { resolved: vec![alert, failed.clone()], dedup_window_secs: 0, ..Default::default() }, None, vec![], policy());
            r.subscribe(key.clone(), "bot".into(), vec![failed.clone(), subscribed, filtered.clone(), fatal_only]);
            // The other scope belongs to a different client and must not be included.
            let other_key = match &key {
                ScopeKey::Task(_) => ScopeKey::Session("session".into()),
                ScopeKey::Session(_) => ScopeKey::Task("task".into()),
            };
            let mut other = filtered;
            other.events = Some(vec!["issues".into()]);
            r.subscribe(other_key, "other-client".into(), vec![other]);
            let targets = r.subscription_targets(&scope);
            r.unsubscribe(&key);
            let mut status = DeliveryStatus { delivery_id: "delivery".into(), event: "task.completed".into(), state: DeliveryState::Failed, attempts: 2, last_status: Some(500), last_error: None };
            r.on_delivery_failed(&status, &failed.target.url, scope.clone(), &targets);
            // Issue deliveries and non-final failures cannot enter the task failure path.
            status.event = "issue".into();
            r.on_delivery_failed(&status, &failed.target.url, scope.clone(), &targets);
            status.event = "task.progress".into();
            status.state = DeliveryState::Pending;
            r.on_delivery_failed(&status, &failed.target.url, scope, &targets);
            r.flush(Duration::from_secs(3)).await;
            assert_eq!(alerts.lock().unwrap().len(), 1);
            assert_eq!(own.lock().unwrap().len(), 1);
            assert!(excluded.lock().unwrap().is_empty());
            assert!(ignored.lock().unwrap().is_empty());
            let events = r.recent(None, 0, 10);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].kind, "webhook_delivery_failed");
            assert_eq!(events[0].scope.task_id.as_deref(), Some("task"));
            assert_eq!(events[0].scope.session_id.as_deref(), Some("session"));
            assert_eq!(events[0].details["host"], "127.0.0.1");
            let payload = serde_json::to_string(&events).unwrap();
            assert!(!payload.contains("private-route"));
            assert!(!payload.contains("/hook"));
            assert!(!payload.contains("excludedTarget"));
            assert!(!payload.contains("subscriptionTargets"));
        }
    }

    #[tokio::test]
    async fn failure_exclusion_compares_normalized_full_url_not_host_or_path() {
        let (target, seen, _server) = receiver(StatusCode::NO_CONTENT, "same-endpoint-key").await;
        let r = Reporter::with_policy(&AlertsConfig { resolved: vec![target.clone()], dedup_window_secs: 0, ..Default::default() }, None, vec![], policy());
        let status = DeliveryStatus { delivery_id: "delivery".into(), event: "task.completed".into(), state: DeliveryState::Failed, attempts: 1, last_status: Some(500), last_error: None };
        let normalized = target.target.url.replacen("http://", "HTTP://", 1);
        r.on_delivery_failed(&status, &normalized, IssueScope::default(), &[]);
        r.flush(Duration::from_secs(3)).await;
        assert!(seen.lock().unwrap().is_empty());
        for url in [format!("{}?private-query", target.target.url), target.target.url.replace("/hook", "/private-path")] {
            r.on_delivery_failed(&status, &url, IssueScope::default(), &[]);
        }
        r.flush(Duration::from_secs(3)).await;
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert!(!serde_json::to_string(&*seen.lock().unwrap()).unwrap().contains("private-"));
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
