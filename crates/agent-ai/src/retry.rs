//! Shared transient retry policy. Never replay a request after a tool event escapes.
use crate::types::{AssistantEvent, AssistantMessage};
use futures::future::BoxFuture;
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) struct CallError {
    pub reason: String,
    pub transient: bool,
    pub retry_after: Option<Duration>,
}

impl CallError {
    pub(crate) fn transport(error: reqwest::Error) -> Self {
        Self { transient: error.is_connect() || error.is_timeout() || error.is_request() || error.is_body(), reason: crate::openai::redact(&error.to_string()), retry_after: None }
    }
}

impl From<String> for CallError {
    fn from(reason: String) -> Self {
        let transient = reason.starts_with("stream idle timeout ")
            || reason == "response headers timeout after 120s"
            || reason == "stream body read error"
            || reason == "OpenAI stream ended without finish_reason (connection dropped?)"
            || reason == "OpenAI responses stream ended without response.completed"
            || reason == "Anthropic stream ended without message_stop";
        Self { reason, transient, retry_after: None }
    }
}

pub(crate) fn retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| httpdate::parse_http_date(value).ok().map(|date| date.duration_since(std::time::SystemTime::now()).unwrap_or_default()))
        .map(|delay| delay.min(crate::MAX_RETRY_AFTER))
}

/// Fresh parser/message per attempt; the caller's cancellation select drops requests and sleeps.
pub(crate) async fn run_with_retries<S, F>(state: &mut S, max_retries: u32, tx: &mpsc::Sender<AssistantEvent>, mut run: F) -> Result<AssistantMessage, String>
where
    F: for<'a> FnMut(&'a mut S, mpsc::Sender<AssistantEvent>) -> BoxFuture<'a, Result<AssistantMessage, CallError>>,
{
    for retry in 0..=max_retries {
        let (attempt_tx, mut rx) = mpsc::channel(64);
        let mut tool_started = false;
        let forward = |ev: &AssistantEvent, tool_started: &mut bool| {
            *tool_started |= matches!(ev, AssistantEvent::ToolcallStart { .. } | AssistantEvent::ToolcallDelta { .. } | AssistantEvent::ToolcallEnd { .. });
        };
        let result = {
            let call = run(state, attempt_tx);
            tokio::pin!(call);
            loop {
                tokio::select! {
                    Some(ev) = rx.recv() => {
                        forward(&ev, &mut tool_started);
                        if tx.send(ev).await.is_err() { return Err("consumer closed".into()); }
                    },
                    result = &mut call => break result,
                }
            }
        };
        // Inspect buffered events before deciding whether replay is safe.
        while let Ok(ev) = rx.try_recv() {
            forward(&ev, &mut tool_started);
            if tx.send(ev).await.is_err() {
                return Err("consumer closed".into());
            }
        }
        match result {
            Ok(message) => return Ok(message),
            Err(error) if error.transient && !tool_started && retry < max_retries => {
                let base = Duration::from_secs(1u64 << retry.min(5));
                let jitter = Duration::from_millis(rand::random_range(0..=250));
                let delay = (base + jitter).min(Duration::from_secs(30)).max(error.retry_after.unwrap_or_default());
                tracing::warn!(issue = "typed", attempt = retry + 1, delay_ms = delay.as_millis() as u64, "retrying transient LLM failure; discarding partial message");
                if tx.send(AssistantEvent::Retry { attempt: retry + 1, reason: error.reason }).await.is_err() {
                    return Err("consumer closed".into());
                }
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error.reason),
        }
    }
    Err("retry attempts exhausted".into())
}
