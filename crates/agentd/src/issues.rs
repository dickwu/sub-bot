//! TEMPORARY I2 interface stub: replaced by I1's bounded/redacting Reporter at merge.
#![allow(dead_code)]
use crate::config::ResolvedTarget;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    pub details: serde_json::Value,
    pub count: u64,
}
impl IssueEvent {
    pub fn new(severity: Severity, kind: &str, component: &str, message: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            ts: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
            severity,
            kind: kind.into(),
            component: component.into(),
            message: message.into(),
            scope: IssueScope::default(),
            tool: None,
            details: serde_json::json!({}),
            count: 1,
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
    pub fn details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScopeKey {
    Session(String),
    Task(String),
}
#[derive(Default)]
pub struct Reporter {
    events: Mutex<Vec<IssueEvent>>,
    subscriptions: Mutex<std::collections::HashMap<ScopeKey, (String, Vec<ResolvedTarget>)>>,
}
impl Reporter {
    // Stub intentionally records in tests; production noop drops events.
    pub fn noop() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn report(&self, event: IssueEvent) {
        if !cfg!(test) {
            return;
        }
        self.events.lock().unwrap().push(event.clone());
        let subscriptions = self.subscriptions.lock().unwrap();
        let mut sent = std::collections::HashSet::new();
        for (key, (client, targets)) in subscriptions.iter() {
            let matches = match key {
                ScopeKey::Session(id) => event.scope.session_id.as_ref() == Some(id),
                ScopeKey::Task(id) => event.scope.task_id.as_ref() == Some(id),
            };
            if !matches || event.scope.client.as_ref() != Some(client) {
                continue;
            }
            for target in targets.iter().filter(|t| t.wants_issue(event.severity)) {
                if !sent.insert(target.target.url.clone()) {
                    continue;
                }
                let target = target.target.clone();
                let payload = serde_json::json!(event);
                tokio::spawn(async move {
                    crate::webhook::Deliverer::new(reqwest::Client::new(), crate::webhook::RetryPolicy::default()).deliver(&target, "issue", &payload, &|_| {}).await;
                });
            }
        }
    }
    pub fn subscribe(&self, key: ScopeKey, client: String, targets: Vec<ResolvedTarget>) {
        self.subscriptions.lock().unwrap().insert(key, (client, targets));
    }
    pub fn unsubscribe(&self, key: &ScopeKey) {
        self.subscriptions.lock().unwrap().remove(key);
    }
    pub fn recent(&self, client: Option<&str>, since_ms: u64, limit: usize) -> Vec<IssueEvent> {
        self.events.lock().unwrap().iter().rev().filter(|e| e.ts >= since_ms && client.is_none_or(|c| e.scope.client.as_deref() == Some(c))).take(limit).cloned().collect()
    }
    pub fn on_delivery_failed(&self, status: &crate::webhook::DeliveryStatus, host: &str, client: Option<&str>) {
        self.report(
            IssueEvent::new(Severity::Error, "webhook_delivery_failed", "webhook", "Webhook delivery failed")
                .scope(IssueScope { client: client.map(str::to_owned), ..Default::default() })
                .details(serde_json::json!({"host": host, "attempts": status.attempts, "status": status.last_status})),
        );
    }
}
static GLOBAL: OnceLock<Arc<Reporter>> = OnceLock::new();
pub fn global() -> Option<Arc<Reporter>> {
    GLOBAL.get().cloned()
}
pub fn install(reporter: Arc<Reporter>) {
    let _ = GLOBAL.set(reporter);
}
pub fn report(event: IssueEvent) {
    if let Some(r) = global() {
        r.report(event);
    }
}
