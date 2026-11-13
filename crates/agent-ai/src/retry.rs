//! Shared transient retry policy.
//!
//! A transient failure (connect/request error, body read error, idle timeout, headers timeout, premature EOF,
//! HTTP 408/429/5xx, Anthropic in-stream overloaded/api/rate-limit errors) is replayed with exponential backoff
//! plus jitter (max 30s, `Retry-After` as a floor). Replay is safe even after tool-call events were streamed:
//! tools only run after the final `Done`, and every consumer discards the partial message on `Retry`.
use crate::types::{AssistantEvent, AssistantMessage};
use futures::future::BoxFuture;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

pub(crate) struct CallError {
    pub reason: String,
    pub transient: bool,
    pub retry_after: Option<Duration>,
    /// `idle_timeout` | `stream_error` | `incomplete` | `headers_timeout` | `http_status` | `connect` | `overloaded` | `rate_limit` | `other`
    pub kind: &'static str,
}

impl CallError {
    pub(crate) fn transport(error: reqwest::Error) -> Self {
        let kind = if error.is_body() || error.is_decode() { "stream_error" } else { "connect" };
        Self { transient: error.is_connect() || error.is_timeout() || error.is_request() || error.is_body(), reason: crate::openai::redact(&error.to_string()), retry_after: None, kind }
    }

    /// Non-2xx HTTP response: only 408 / 429 / 5xx are transient; other 4xx are never retried.
    pub(crate) fn http(reason: String, status: reqwest::StatusCode, retry_after: Option<Duration>) -> Self {
        Self { reason, transient: matches!(status.as_u16(), 408 | 429 | 500..=599), retry_after, kind: "http_status" }
    }
}

/// Classification of the provider's plain-text failure reasons (prefix matches, so wording of the
/// configured timeout values does not matter).
pub(crate) fn classify_reason(reason: &str) -> Option<&'static str> {
    if reason.starts_with("stream idle timeout ") {
        Some("idle_timeout")
    } else if reason.starts_with("response headers timeout") {
        Some("headers_timeout")
    } else if reason == "stream body read error" {
        Some("stream_error")
    } else if reason == "OpenAI stream ended without finish_reason (connection dropped?)"
        || reason == "OpenAI responses stream ended without response.completed"
        || reason == "Anthropic stream ended without message_stop"
    {
        Some("incomplete")
    } else {
        None
    }
}

impl From<String> for CallError {
    fn from(reason: String) -> Self {
        let kind = classify_reason(&reason);
        Self { transient: kind.is_some(), kind: kind.unwrap_or("other"), reason, retry_after: None }
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

/// Per-attempt stream statistics for retry diagnostics (bytes received, time since the last byte).
struct AttemptStats {
    started: Instant,
    bytes: AtomicU64,
    last_byte: Mutex<Option<Instant>>,
}

impl AttemptStats {
    fn new() -> Self {
        Self { started: Instant::now(), bytes: AtomicU64::new(0), last_byte: Mutex::new(None) }
    }

    /// Seconds since the last body byte (or since the attempt started when nothing arrived).
    fn idle_secs(&self) -> f64 {
        self.last_byte.lock().unwrap().unwrap_or(self.started).elapsed().as_secs_f64()
    }
}

tokio::task_local! {
    static STATS: Arc<AttemptStats>;
}

/// Called by the stream parsers for every body chunk; a no-op outside [`run_with_retries`].
pub(crate) fn record_bytes(n: usize) {
    let _ = STATS.try_with(|stats| {
        stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        *stats.last_byte.lock().unwrap() = Some(Instant::now());
    });
}

/// Backoff before retry number `retry + 1` (0-based `retry`): 1s, 2s, 4s … (max 32s base) plus up to 250ms
/// jitter, capped at 30s; `Retry-After` is a floor.
pub(crate) fn backoff(retry: u32, retry_after: Option<Duration>) -> Duration {
    let base = Duration::from_secs(1u64 << retry.min(5));
    let jitter = Duration::from_millis(rand::random_range(0..=250));
    (base + jitter).min(Duration::from_secs(30)).max(retry_after.unwrap_or_default())
}

/// Labels for retry logs (the provider's tracing span carries provider/model too, but logs are often filtered).
pub(crate) struct RetryCtx<'a> {
    pub provider: &'a str,
    pub model: &'a str,
}

/// Fresh parser/message per attempt; the caller's cancellation select drops requests and sleeps.
/// Partial events (text, thinking, tool-call deltas) are forwarded live; on a transient failure a
/// [`AssistantEvent::Retry`] tells consumers to discard them before the replay starts.
pub(crate) async fn run_with_retries<S, F>(state: &mut S, max_retries: u32, tx: &mpsc::Sender<AssistantEvent>, ctx: RetryCtx<'_>, mut run: F) -> Result<AssistantMessage, String>
where
    F: for<'a> FnMut(&'a mut S, mpsc::Sender<AssistantEvent>) -> BoxFuture<'a, Result<AssistantMessage, CallError>>,
{
    for retry in 0..=max_retries {
        let (attempt_tx, mut rx) = mpsc::channel(64);
        let stats = Arc::new(AttemptStats::new());
        let mut tool_calls = 0u32;
        let count = |ev: &AssistantEvent, tool_calls: &mut u32| {
            if matches!(ev, AssistantEvent::ToolcallStart { .. }) {
                *tool_calls += 1;
            }
        };
        let result = {
            let call = STATS.scope(stats.clone(), run(state, attempt_tx));
            tokio::pin!(call);
            loop {
                tokio::select! {
                    Some(ev) = rx.recv() => {
                        count(&ev, &mut tool_calls);
                        if tx.send(ev).await.is_err() { return Err("consumer closed".into()); }
                    },
                    result = &mut call => break result,
                }
            }
        };
        // Forward buffered events so the Retry below follows everything it discards.
        while let Ok(ev) = rx.try_recv() {
            count(&ev, &mut tool_calls);
            if tx.send(ev).await.is_err() {
                return Err("consumer closed".into());
            }
        }
        let (bytes, idle_secs) = (stats.bytes.load(Ordering::Relaxed), stats.idle_secs());
        match result {
            Ok(message) => {
                if retry > 0 {
                    tracing::info!(provider = ctx.provider, model = ctx.model, attempts = retry + 1, retries = retry, "LLM call succeeded after retrying transient failures");
                }
                return Ok(message);
            }
            Err(error) if error.transient && retry < max_retries => {
                let delay = backoff(retry, error.retry_after);
                let delay_ms = delay.as_millis() as u64;
                // typed: agentd reports the llm_retry issue from the MessageRetry event.
                tracing::warn!(
                    issue = "typed",
                    provider = ctx.provider,
                    model = ctx.model,
                    attempt = retry + 1,
                    max_retries,
                    kind = error.kind,
                    delay_ms,
                    bytes_received = bytes,
                    secs_since_last_byte = idle_secs,
                    discarded_tool_calls = tool_calls,
                    reason = %error.reason,
                    "retrying transient LLM failure; discarding partial message"
                );
                let event = AssistantEvent::Retry { attempt: retry + 1, reason: error.reason, max_retries, delay_ms, kind: error.kind.into(), discarded_tool_calls: tool_calls };
                if tx.send(event).await.is_err() {
                    return Err("consumer closed".into());
                }
                tokio::time::sleep(delay).await;
            }
            Err(error) => {
                if error.transient && max_retries > 0 {
                    // typed: the final error message is reported by agentd (llm_* issue).
                    tracing::error!(
                        issue = "typed",
                        provider = ctx.provider,
                        model = ctx.model,
                        attempts = retry + 1,
                        max_retries,
                        kind = error.kind,
                        bytes_received = bytes,
                        secs_since_last_byte = idle_secs,
                        reason = %error.reason,
                        "LLM retries exhausted"
                    );
                }
                return Err(error.reason);
            }
        }
    }
    Err("retry attempts exhausted".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_classified_by_prefix() {
        for (reason, kind) in [
            ("stream idle timeout after 1800s", Some("idle_timeout")),
            ("response headers timeout after 120s", Some("headers_timeout")),
            ("response headers timeout after 7200s", Some("headers_timeout")),
            ("stream body read error", Some("stream_error")),
            ("Anthropic stream ended without message_stop", Some("incomplete")),
            ("OpenAI stream ended without finish_reason (connection dropped?)", Some("incomplete")),
            ("OpenAI responses stream ended without response.completed", Some("incomplete")),
            ("OpenAI HTTP 400: bad", None),
        ] {
            assert_eq!(classify_reason(reason), kind, "{reason}");
            let error = CallError::from(reason.to_string());
            assert_eq!((error.transient, error.kind), (kind.is_some(), kind.unwrap_or("other")), "{reason}");
        }
        for (status, transient) in [(408, true), (429, true), (500, true), (529, true), (400, false), (401, false), (404, false), (422, false)] {
            let error = CallError::http("x".into(), reqwest::StatusCode::from_u16(status).unwrap(), None);
            assert_eq!((error.transient, error.kind), (transient, "http_status"), "{status}");
        }
    }

    #[test]
    fn backoff_is_bounded_and_honours_retry_after() {
        assert!(backoff(0, None) >= Duration::from_secs(1) && backoff(0, None) <= Duration::from_millis(1250));
        assert_eq!(backoff(10, None), Duration::from_secs(30));
        assert_eq!(backoff(0, Some(Duration::from_secs(45))), Duration::from_secs(45));
    }
}
