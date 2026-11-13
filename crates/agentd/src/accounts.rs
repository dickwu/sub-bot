//! 同一 provider 多账号：账号键、冷却表与 failover 包装 provider。
//!
//! `FailoverProvider` 持有有序账号链 `[主账号, fallback...]`。主账号遇到 429 / 额度耗尽（insufficient_quota）
//! 且尚未向下游发出任何内容事件时，标记冷却并切换到下一个账号重放同一请求；切换以 `AssistantEvent::Retry`
//! （reason 前缀 [`FAILOVER_PREFIX`]）通知上层，agent_loop 对 Retry 已有“丢弃部分消息”的语义。
//! 每条最终消息都填上实际使用的账号名（`AssistantMessage.account`），只含账号名，绝不含 key。
use agent_ai::{AssistantEvent, EventStream, ModelInfo, Provider, Request, StopReason};
use futures::StreamExt;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// `(provider, account)`：provider 实例、模型目录缓存、冷却表的键。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountKey {
    pub provider: String,
    pub account: String,
}

impl AccountKey {
    pub fn new(provider: &str, account: &str) -> Self {
        Self { provider: provider.into(), account: account.into() }
    }

    /// `provider/account`，用于日志与错误信息。
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider, self.account)
    }
}

/// `"openai".into()` = openai 的 default 账号（兼容只有一个账号的旧代码/测试）。
impl From<&str> for AccountKey {
    fn from(provider: &str) -> Self {
        Self::new(provider, agent_ai::secrets::DEFAULT_ACCOUNT)
    }
}

/// Hub 级共享：被判定限流/额度耗尽的账号在冷却期内排到链尾。
#[derive(Default)]
pub struct Cooldowns(Mutex<HashMap<AccountKey, Instant>>);

impl Cooldowns {
    pub fn mark(&self, key: &AccountKey, until: Instant) {
        self.0.lock().unwrap().insert(key.clone(), until);
    }

    pub fn is_cooling(&self, key: &AccountKey, now: Instant) -> bool {
        let mut map = self.0.lock().unwrap();
        match map.get(key) {
            Some(until) if *until > now => true,
            Some(_) => {
                map.remove(key);
                false
            }
            None => false,
        }
    }
}

#[derive(Clone)]
pub struct AccountSlot {
    pub key: AccountKey,
    pub provider: Arc<dyn Provider>,
}

/// failover 切换事件的 Retry.reason 前缀：`account failover: <from> -> <to> (<class>)`。
pub const FAILOVER_PREFIX: &str = "account failover: ";

/// 从 Retry.reason 解析 `(from, to, class)`；不是 failover 事件时返回 None。
pub fn parse_failover(reason: &str) -> Option<(&str, &str, &str)> {
    let rest = reason.strip_prefix(FAILOVER_PREFIX)?;
    let (from, rest) = rest.split_once(" -> ")?;
    let (to, class) = rest.split_once(" (")?;
    Some((from, to, class.strip_suffix(')').unwrap_or(class)))
}

/// 只有 429 / 额度类错误才切换账号（认证错误是配置问题，overloaded 与账号无关）。
pub fn failover_class(error: &str) -> Option<&'static str> {
    let lower = error.to_ascii_lowercase();
    if lower.contains("insufficient_quota") || lower.contains("exceeded your current quota") {
        Some("quota")
    } else if crate::hub::classify_llm_error(error).0 == "llm_rate_limit" {
        Some("rate_limit")
    } else {
        None
    }
}

pub struct FailoverProvider {
    /// `[主账号, fallback...]`，长度 ≥ 1
    chain: Vec<AccountSlot>,
    cooldowns: Arc<Cooldowns>,
    cooldown: Duration,
}

impl FailoverProvider {
    pub fn new(chain: Vec<AccountSlot>, cooldowns: Arc<Cooldowns>, cooldown: Duration) -> Self {
        assert!(!chain.is_empty(), "account chain must not be empty");
        Self { chain, cooldowns, cooldown }
    }

    /// 冷却中的账号排到链尾（全部冷却时仍按原顺序尝试，避免“无账号可用”）。
    fn order(&self) -> Vec<AccountSlot> {
        let now = Instant::now();
        let (ready, cooling): (Vec<_>, Vec<_>) = self.chain.iter().cloned().partition(|s| !self.cooldowns.is_cooling(&s.key, now));
        ready.into_iter().chain(cooling).collect()
    }
}

fn tag(ev: AssistantEvent, account: &str) -> AssistantEvent {
    match ev {
        AssistantEvent::Done { mut message } => {
            message.account = Some(account.into());
            AssistantEvent::Done { message }
        }
        AssistantEvent::Error { mut message } => {
            message.account = Some(account.into());
            AssistantEvent::Error { message }
        }
        other => other,
    }
}

impl Provider for FailoverProvider {
    fn id(&self) -> &str {
        self.chain[0].provider.id()
    }

    fn list_models(&self) -> futures::future::BoxFuture<'_, Result<Vec<ModelInfo>, String>> {
        self.chain[0].provider.list_models()
    }

    fn stream(&self, req: Request, cancel: CancellationToken) -> EventStream {
        if self.chain.len() == 1 {
            let account = self.chain[0].key.account.clone();
            return Box::pin(self.chain[0].provider.stream(req, cancel).map(move |ev| tag(ev, &account)));
        }
        let order = self.order();
        // 第一个账号在调用方任务里同步建立（与直接调用 provider 一样：构造期 panic 由 agent 的运行时捕获）
        let first = order[0].provider.stream(req.clone(), cancel.clone());
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let (cooldowns, cooldown) = (self.cooldowns.clone(), self.cooldown);
        tokio::spawn(async move {
            let mut inner = Some(first);
            let mut attempt = 0u32;
            for (i, slot) in order.iter().enumerate() {
                let mut stream = inner.take().unwrap_or_else(|| slot.provider.stream(req.clone(), cancel.clone()));
                let mut content = false;
                let mut switched = false;
                while let Some(ev) = stream.next().await {
                    let ev = match ev {
                        AssistantEvent::Error { message } if message.stop_reason == StopReason::Error => {
                            let class = message.error_message.as_deref().and_then(failover_class);
                            if let (Some(class), false) = (class, content) {
                                cooldowns.mark(&slot.key, Instant::now() + cooldown);
                                if let Some(next) = order.get(i + 1).filter(|_| !cancel.is_cancelled()) {
                                    attempt += 1;
                                    // typed：hub 收到下面的 Retry 后上报 account_failover issue，日志不再重复转成 log_warn
                                    tracing::warn!(issue = "typed", provider = %slot.key.provider, from = %slot.key.account, to = %next.key.account, class, "account failover");
                                    let reason = format!("{FAILOVER_PREFIX}{} -> {} ({class})", slot.key.account, next.key.account);
                                    // failover 只发生在还没有任何内容之前：没有要丢弃的工具调用，也不等待
                                    let retry = AssistantEvent::Retry { attempt, reason, max_retries: (order.len() - 1) as u32, delay_ms: 0, kind: "account_failover".into(), discarded_tool_calls: 0 };
                                    if tx.send(retry).await.is_err() {
                                        return;
                                    }
                                    switched = true;
                                    break;
                                }
                            }
                            AssistantEvent::Error { message }
                        }
                        ev @ (AssistantEvent::Start | AssistantEvent::Retry { .. } | AssistantEvent::Done { .. } | AssistantEvent::Error { .. }) => ev,
                        ev => {
                            content = true;
                            ev
                        }
                    };
                    let last = matches!(ev, AssistantEvent::Done { .. } | AssistantEvent::Error { .. });
                    if tx.send(tag(ev, &slot.key.account)).await.is_err() || last {
                        return;
                    }
                }
                if !switched {
                    // 内层流没有 Done/Error 就结束：交给 agent_loop 报 “provider stream ended without done/error”
                    return;
                }
            }
        });
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ai::{AssistantMessage, Message};

    /// 按脚本回复：`Err(text)` = 以该错误结束；`Ok(text)` = 发 TextDelta 后 Done。`content_then_error` = 先发内容再 429。
    struct Scripted {
        calls: Arc<Mutex<usize>>,
        reply: Result<&'static str, &'static str>,
        content_then_error: bool,
    }

    impl Provider for Scripted {
        fn id(&self) -> &str {
            "openai"
        }
        fn stream(&self, req: Request, _: CancellationToken) -> EventStream {
            *self.calls.lock().unwrap() += 1;
            let mut events = vec![AssistantEvent::Start];
            match self.reply {
                Ok(text) => {
                    events.push(AssistantEvent::TextDelta { content_index: 0, delta: text.into() });
                    let mut m = AssistantMessage::empty(&req.model);
                    m.content.push(agent_ai::Content::Text { text: text.into() });
                    events.push(AssistantEvent::Done { message: m });
                }
                Err(e) => {
                    if self.content_then_error {
                        events.push(AssistantEvent::TextDelta { content_index: 0, delta: "partial".into() });
                    }
                    events.push(AssistantEvent::Error { message: AssistantMessage::failed(&req.model, StopReason::Error, e) });
                }
            }
            Box::pin(futures::stream::iter(events))
        }
    }

    fn slot(account: &str, reply: Result<&'static str, &'static str>, content_then_error: bool) -> (AccountSlot, Arc<Mutex<usize>>) {
        let calls = Arc::new(Mutex::new(0));
        (AccountSlot { key: AccountKey::new("openai", account), provider: Arc::new(Scripted { calls: calls.clone(), reply, content_then_error }) }, calls)
    }

    fn req() -> Request {
        Request {
            model: "m".into(),
            system_prompt: None,
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            reasoning: None,
            raw_effort: false,
            thinking: None,
            api: agent_ai::Api::Auto,
            max_tokens: None,
            llm_max_retries: Some(0),
            stream_idle_timeout: None,
        }
    }

    async fn run(p: &FailoverProvider) -> Vec<AssistantEvent> {
        tokio::time::timeout(Duration::from_secs(3), p.stream(req(), CancellationToken::new()).collect::<Vec<_>>()).await.unwrap()
    }

    fn last(events: &[AssistantEvent]) -> &AssistantMessage {
        match events.last() {
            Some(AssistantEvent::Done { message } | AssistantEvent::Error { message }) => message,
            other => panic!("unexpected {other:?}"),
        }
    }

    const QUOTA: &str = r#"OpenAI HTTP 429 Too Many Requests (chat API): {"error":{"type":"insufficient_quota","message":"You exceeded your current quota"}}"#;

    #[tokio::test]
    async fn failover_switches_on_quota_and_cools_down() {
        let (main, main_calls) = slot("work", Err(QUOTA), false);
        let (backup, backup_calls) = slot("backup", Ok("from backup"), false);
        let cooldowns = Arc::new(Cooldowns::default());
        let p = FailoverProvider::new(vec![main, backup], cooldowns.clone(), Duration::from_secs(60));
        let events = run(&p).await;
        assert_eq!(last(&events).account.as_deref(), Some("backup"));
        assert_eq!(last(&events).text(), "from backup");
        let retries: Vec<_> = events.iter().filter_map(|e| if let AssistantEvent::Retry { reason, .. } = e { Some(reason.clone()) } else { None }).collect();
        assert_eq!(retries, ["account failover: work -> backup (quota)"]);
        assert_eq!(parse_failover(&retries[0]), Some(("work", "backup", "quota")));
        assert!(cooldowns.is_cooling(&AccountKey::new("openai", "work"), Instant::now()));
        // 冷却期内直接从 backup 开始，主账号不再被调用
        let events = run(&p).await;
        assert_eq!(last(&events).account.as_deref(), Some("backup"));
        assert_eq!((*main_calls.lock().unwrap(), *backup_calls.lock().unwrap()), (1, 2));
        assert!(!events.iter().any(|e| matches!(e, AssistantEvent::Retry { .. })));
        // 冷却结束后恢复主账号优先
        cooldowns.mark(&AccountKey::new("openai", "work"), Instant::now());
        run(&p).await;
        assert_eq!(*main_calls.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn plain_rate_limit_switches_but_auth_errors_and_post_content_errors_do_not() {
        let (main, _) = slot("work", Err("OpenAI HTTP 429 Too Many Requests: slow down"), false);
        let (backup, _) = slot("backup", Ok("ok"), false);
        let p = FailoverProvider::new(vec![main, backup], Default::default(), Duration::from_secs(60));
        let events = run(&p).await;
        assert_eq!(last(&events).account.as_deref(), Some("backup"));
        assert!(events.iter().any(|e| matches!(e, AssistantEvent::Retry { reason, .. } if reason.ends_with("(rate_limit)"))));
        for (error, content) in [("OpenAI HTTP 401 Unauthorized: invalid api key", false), (QUOTA, true), ("Anthropic HTTP 529: overloaded_error", false)] {
            let (main, _) = slot("work", Err(error), content);
            let (backup, backup_calls) = slot("backup", Ok("ok"), false);
            let p = FailoverProvider::new(vec![main, backup], Default::default(), Duration::from_secs(60));
            let events = run(&p).await;
            let m = last(&events);
            assert_eq!((m.account.as_deref(), m.stop_reason), (Some("work"), StopReason::Error), "{error}");
            assert_eq!(*backup_calls.lock().unwrap(), 0, "{error}");
            assert!(!events.iter().any(|e| matches!(e, AssistantEvent::Retry { .. })));
        }
    }

    #[tokio::test]
    async fn last_account_error_is_reported_with_its_account_and_single_chain_passes_through() {
        let (a, _) = slot("work", Err(QUOTA), false);
        let (b, _) = slot("backup", Err(QUOTA), false);
        let p = FailoverProvider::new(vec![a, b], Default::default(), Duration::from_secs(60));
        let events = run(&p).await;
        let m = last(&events);
        assert_eq!((m.account.as_deref(), m.error_message.as_deref()), (Some("backup"), Some(QUOTA)));
        let (only, calls) = slot("default", Ok("hello"), false);
        let p = FailoverProvider::new(vec![only], Default::default(), Duration::from_secs(60));
        assert_eq!(p.id(), "openai");
        let events = run(&p).await;
        assert_eq!(last(&events).account.as_deref(), Some("default"));
        assert!(events.iter().any(|e| matches!(e, AssistantEvent::TextDelta { delta, .. } if delta == "hello")));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn classes_and_labels() {
        assert_eq!(failover_class(QUOTA), Some("quota"));
        assert_eq!(failover_class(r#"Anthropic stream error: {"type":"rate_limit_error","message":"busy"}"#), Some("rate_limit"));
        assert_eq!(failover_class("OpenAI HTTP 500"), None);
        assert_eq!(AccountKey::new("openai", "work").label(), "openai/work");
        assert_eq!(AccountKey::from("anthropic"), AccountKey::new("anthropic", "default"));
        assert_eq!(parse_failover("Anthropic HTTP 529: overloaded"), None);
    }
}
