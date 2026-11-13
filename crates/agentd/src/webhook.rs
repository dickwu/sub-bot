//! Signed webhook delivery. Dropping `deliver` cancels its request/backoff; no background task is spawned.
//! Receivers should verify the raw request bytes and deduplicate by `X-SubBot-Delivery`.

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct WebhookTarget {
    pub url: String,
    pub secret: Option<String>,
}

impl fmt::Debug for WebhookTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookTarget").field("url", &self.url).field("secret", &self.secret.as_ref().map(|_| "<redacted>")).finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Pending,
    Delivered,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryStatus {
    pub delivery_id: String,
    pub event: String,
    pub state: DeliveryState,
    pub attempts: u32,
    pub last_status: Option<u16>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub attempt_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 6, initial_backoff: Duration::from_secs(1), max_backoff: Duration::from_secs(60), attempt_timeout: Duration::from_secs(10) }
    }
}

pub struct Deliverer {
    // Keep construction fallible internally: the shared constructor cannot return a Result.
    http: Result<reqwest::Client, reqwest::Error>,
    policy: RetryPolicy,
}

impl Deliverer {
    pub fn new(_http: reqwest::Client, policy: RetryPolicy) -> Self {
        // A supplied client may follow redirects (and leak signed payloads to another origin).
        Self { http: agent_ai::http::client_builder().redirect(reqwest::redirect::Policy::none()).build(), policy }
    }

    /// Delivers one event, with a stable UUID and body across retries. Calls `on_update` after
    /// every attempt, including the final one. Cancellation drops all in-flight work.
    pub async fn deliver(&self, target: &WebhookTarget, event: &str, payload: &serde_json::Value, on_update: &(dyn Fn(&DeliveryStatus) + Send + Sync)) -> DeliveryStatus {
        let status = DeliveryStatus { delivery_id: uuid::Uuid::new_v4().to_string(), event: event.to_owned(), state: DeliveryState::Pending, attempts: 0, last_status: None, last_error: None };
        self.deliver_status(target, payload, status, on_update, false).await
    }

    /// Resume a journaled delivery without resetting its identity or attempt budget.
    pub async fn resume(&self, target: &WebhookTarget, payload: &serde_json::Value, status: DeliveryStatus, on_update: &(dyn Fn(&DeliveryStatus) + Send + Sync)) -> DeliveryStatus {
        self.deliver_status(target, payload, status, on_update, true).await
    }

    async fn deliver_status(
        &self,
        target: &WebhookTarget,
        payload: &serde_json::Value,
        mut status: DeliveryStatus,
        on_update: &(dyn Fn(&DeliveryStatus) + Send + Sync),
        journal: bool,
    ) -> DeliveryStatus {
        let event_name = status.event.clone();
        let event = event_name.as_str();
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Envelope<'a> {
            delivery_id: &'a str,
            event: &'a str,
            timestamp: i64,
            data: &'a serde_json::Value,
        }
        let body = serde_json::to_vec(&Envelope { delivery_id: &status.delivery_id, event, timestamp: unix_seconds(), data: payload });
        let host = reqwest::Url::parse(&target.url).ok().and_then(|url| url.host_str().map(str::to_owned)).unwrap_or_default();
        let mut backoff = self.policy.initial_backoff.min(self.policy.max_backoff);
        // Even a zero-attempt policy gets a terminal result and a store notification.
        let max_attempts = self.policy.max_attempts.max(1);
        if status.attempts >= max_attempts {
            status.state = DeliveryState::Failed;
            on_update(&status);
            return status;
        }
        for attempt in status.attempts.saturating_add(1)..=max_attempts {
            status.attempts = attempt;
            status.last_status = None;
            status.last_error = None;
            if journal {
                on_update(&status);
            }
            let mut retry_after = None;
            let retry = match (&self.http, &body) {
                (Ok(http), Ok(body)) => {
                    let timestamp = unix_seconds();
                    let mut request = http
                        .post(&target.url)
                        .header(reqwest::header::CONTENT_TYPE, "application/json")
                        .header(reqwest::header::USER_AGENT, concat!("sub-bot/", env!("CARGO_PKG_VERSION")))
                        .header("X-SubBot-Event", event)
                        .header("X-SubBot-Delivery", &status.delivery_id)
                        .header("X-SubBot-Timestamp", timestamp.to_string())
                        .body(body.clone());
                    if let Some(secret) = &target.secret {
                        request = request.header("X-SubBot-Signature", sign(secret.as_bytes(), timestamp, body));
                    }
                    let operation = async {
                        let response = request.send().await?;
                        let code = response.status();
                        status.last_status = Some(code.as_u16());
                        if matches!(code.as_u16(), 429 | 503) {
                            retry_after =
                                response.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| parse_retry_after(v, SystemTime::now(), self.policy.max_backoff));
                        }
                        if !code.is_success() {
                            let snippet = response_snippet(response).await?;
                            let secrets = target.secret.iter().cloned().collect::<Vec<_>>();
                            let snippet: String = crate::issues::redact(&snippet, &secrets).chars().take(200).collect();
                            status.last_error = Some(format!("HTTP {}: {snippet}", code.as_u16()));
                        }
                        Ok::<(), reqwest::Error>(())
                    };
                    match tokio::time::timeout(self.policy.attempt_timeout, operation).await {
                        Ok(Ok(())) => status.last_status.is_some_and(retryable),
                        Ok(Err(error)) => {
                            // reqwest errors can embed the full URL; never retain or log them.
                            status.last_error = Some(if error.is_timeout() { "attempt timed out" } else { "webhook request failed" }.into());
                            !error.is_builder() && status.last_status.is_none_or(retryable)
                        }
                        Err(_) => {
                            status.last_error = Some("attempt timed out".into());
                            status.last_status.is_none_or(retryable)
                        }
                    }
                }
                (Err(_), _) => {
                    status.last_error = Some("webhook HTTP client initialization failed".into());
                    false
                }
                (_, Err(_)) => {
                    status.last_error = Some("webhook serialization failed".into());
                    false
                }
            };
            status.state = if status.last_status.is_some_and(|code| (200..300).contains(&code)) {
                DeliveryState::Delivered
            } else if retry && attempt < max_attempts {
                DeliveryState::Pending
            } else {
                DeliveryState::Failed
            };
            if status.state == DeliveryState::Delivered {
                tracing::info!(delivery_id = %status.delivery_id, event, host, attempt, status = ?status.last_status, "webhook delivered");
            } else {
                tracing::warn!(issue = "typed", delivery_id = %status.delivery_id, event, host, attempt, status = ?status.last_status, "webhook attempt failed");
            }
            // A consumer callback must not unwind the delivery future.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_update(&status)));
            if status.state != DeliveryState::Pending {
                break;
            }
            let jitter = (backoff / 1000).saturating_mul(rand::random_range(0..=100));
            let delay = retry_after.unwrap_or_else(|| backoff.saturating_add(jitter).min(self.policy.max_backoff));
            if let Some(deadline) = tokio::time::Instant::now().checked_add(delay) {
                tokio::time::sleep_until(deadline).await;
            } else {
                std::future::pending::<()>().await;
            }
            backoff = backoff.saturating_mul(2).min(self.policy.max_backoff);
        }
        status
    }
}

fn parse_retry_after(value: &str, now: SystemTime, max_backoff: Duration) -> Option<Duration> {
    let value = value.trim();
    let delay = value.parse::<u64>().ok().map(Duration::from_secs).or_else(|| httpdate::parse_http_date(value).ok().map(|date| date.duration_since(now).unwrap_or_default()))?;
    Some(delay.min(max_backoff).min(agent_ai::MAX_RETRY_AFTER))
}

fn retryable(code: u16) -> bool {
    matches!(code, 408 | 429 | 500..=599)
}

fn unix_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)).unwrap_or(0)
}

async fn response_snippet(mut response: reqwest::Response) -> Result<String, reqwest::Error> {
    let mut bytes = Vec::with_capacity(4096);
    while bytes.len() < 4096 {
        let Some(chunk) = response.chunk().await? else { break };
        bytes.extend_from_slice(&chunk[..chunk.len().min(4096 - bytes.len())]);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn signature_mac(secret: &[u8], timestamp: i64, body: &[u8]) -> Option<Hmac<Sha256>> {
    // HMAC accepts keys of any length; retain error handling instead of panicking.
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    Some(mac)
}

/// `t=<unix secs>,v1=<lowercase hex HMAC-SHA256(secret, "<t>.<raw body>")>`.
pub fn sign(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let Some(mac) = signature_mac(secret, timestamp, body) else { return String::new() };
    let mut result = format!("t={timestamp},v1=");
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in mac.finalize().into_bytes() {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 15) as usize] as char);
    }
    result
}

/// Verify the raw body before JSON parsing, with a constant-time MAC comparison.
/// `now` and the nonnegative tolerance are Unix seconds. Also reject timestamps too far
/// in the future. Receivers must separately deduplicate the delivery ID to prevent replay.
pub fn verify(secret: &[u8], header: &str, body: &[u8], now: i64, tolerance_secs: i64) -> bool {
    let Some((timestamp, hex)) = header.strip_prefix("t=").and_then(|value| value.split_once(",v1=")) else { return false };
    let Ok(time) = timestamp.parse::<i64>() else { return false };
    if tolerance_secs < 0 || time.abs_diff(now) > tolerance_secs as u64 || timestamp != time.to_string() || hex.len() != 64 {
        return false;
    }
    let mut signature = [0u8; 32];
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
    for (out, pair) in signature.iter_mut().zip(hex.as_bytes().as_chunks::<2>().0) {
        let (Some(high), Some(low)) = (digit(pair[0]), digit(pair[1])) else { return false };
        *out = (high << 4) | low;
    }
    signature_mac(secret, time, body).is_some_and(|mac| mac.verify_slice(&signature).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
        Router,
    };
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
    };

    #[test]
    fn retry_after_dates_and_seconds() {
        let now = httpdate::parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        let max = Duration::from_secs(60);
        for (value, seconds) in [("0", 0), (" 12 ", 12), ("9999", 60), ("Wed, 21 Oct 2015 07:27:59 GMT", 0), ("Wed, 21 Oct 2015 07:28:25 GMT", 25), ("Wed, 21 Oct 2015 07:30:00 GMT", 60)] {
            assert_eq!(parse_retry_after(value, now, max), Some(Duration::from_secs(seconds)), "{value}");
        }
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:25 GMT", now, Duration::from_secs(5)), Some(Duration::from_secs(5)));
        for invalid in ["", "-1", "garbage"] {
            assert_eq!(parse_retry_after(invalid, now, max), None);
        }
    }

    #[test]
    fn signatures() {
        let body = br#"{"hello":"world"}"#;
        let header = sign(b"key", 100, body);
        assert!(verify(b"key", &header, body, 100, 5));
        assert!(!verify(b"wrong", &header, body, 100, 5));
        assert!(!verify(b"key", &header, b"tampered", 100, 5));
        assert!(!verify(b"key", &header, body, 106, 5));
        assert!(!verify(b"key", &header, body, 94, 5));
        assert!(!verify(b"key", &header, body, 100, -1));
        assert!(!verify(b"key", &sign(b"key", i64::MIN, body), body, i64::MAX, i64::MAX));
        for malformed in ["", "t=100", "t=x,v1=abc", "t=100,v1=00", "v1=00,t=100", "t=100,v1=💥", &format!("{header},v1=00"), &format!("t=0100,v1={}", &header[9..])] {
            assert!(!verify(b"key", malformed, body, 100, 5), "{malformed}");
        }
        // Independent, fixed test vector for HMAC-SHA256("key", "100.hello").
        assert_eq!(sign(b"key", 100, b"hello"), "t=100,v1=5a46cef0459382e88dbfdb0db20e5ebd835692143c632f206bc70416245b78fa");
    }

    #[test]
    fn target_debug_redacts_secret() {
        let target = WebhookTarget { url: "http://localhost/".into(), secret: Some("super-private-key".into()) };
        let debug = format!("{target:?}");
        assert!(!debug.contains("super-private-key"));
        assert!(debug.contains("<redacted>"));
    }

    #[derive(Clone)]
    struct Receiver {
        codes: Vec<u16>,
        delay: Duration,
        secret: Option<String>,
        retry_after: Option<&'static str>,
        seen: Arc<Mutex<Vec<(String, Bytes)>>>,
        processed: Arc<Mutex<HashSet<String>>>,
    }

    impl Receiver {
        fn new(codes: Vec<u16>) -> Self {
            Self { codes, delay: Duration::ZERO, secret: Some("receiver-key".into()), retry_after: None, seen: Arc::default(), processed: Arc::default() }
        }
    }

    async fn receive(State(receiver): State<Receiver>, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
        let data: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let id = headers["X-SubBot-Delivery"].to_str().unwrap().to_owned();
        assert_eq!(data["deliveryId"], id);
        assert_eq!(data["event"], "task.done");
        assert_eq!(data["data"], serde_json::json!({"ok": true}));
        assert!(data["timestamp"].is_i64());
        assert_eq!(headers["X-SubBot-Event"], "task.done");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["user-agent"], concat!("sub-bot/", env!("CARGO_PKG_VERSION")));
        let timestamp = headers["X-SubBot-Timestamp"].to_str().unwrap();
        if let Some(secret) = &receiver.secret {
            let signature = headers["X-SubBot-Signature"].to_str().unwrap();
            assert!(signature.starts_with(&format!("t={timestamp},")));
            assert!(verify(secret.as_bytes(), signature, &body, unix_seconds(), 5));
        } else {
            assert!(!headers.contains_key("X-SubBot-Signature"));
        }
        let index = {
            let mut seen = receiver.seen.lock().unwrap();
            let index = seen.len();
            seen.push((id.clone(), body));
            index
        };
        receiver.processed.lock().unwrap().insert(id);
        tokio::time::sleep(receiver.delay).await;
        let code = receiver.codes[index.min(receiver.codes.len() - 1)];
        let mut response_headers = HeaderMap::new();
        if let Some(value) = receiver.retry_after {
            response_headers.insert("retry-after", value.parse().unwrap());
        }
        if code == 302 {
            response_headers.insert("location", "/redirected".parse().unwrap());
        }
        (StatusCode::from_u16(code).unwrap(), response_headers, "失败".repeat(3000))
    }

    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn server(receiver: Receiver) -> (WebhookTarget, Server) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = WebhookTarget { url: format!("http://{}/hook", listener.local_addr().unwrap()), secret: receiver.secret.clone() };
        let app = Router::new().route("/hook", post(receive)).route("/redirected", post(receive)).with_state(receiver);
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (target, Server(task))
    }

    fn policy() -> RetryPolicy {
        RetryPolicy { max_attempts: 3, initial_backoff: Duration::from_millis(10), max_backoff: Duration::from_millis(30), attempt_timeout: Duration::from_millis(200) }
    }

    async fn deliver(receiver: Receiver, policy: RetryPolicy) -> (DeliveryStatus, Vec<DeliveryStatus>) {
        let (target, _server) = server(receiver).await;
        let updates = Mutex::new(Vec::new());
        let deliverer = Deliverer::new(agent_ai::http::client(), policy);
        let status = deliverer.deliver(&target, "task.done", &serde_json::json!({"ok": true}), &|status| updates.lock().unwrap().push(status.clone())).await;
        (status, updates.into_inner().unwrap())
    }

    #[tokio::test]
    async fn retries_signed_body_and_deduplicates() {
        let receiver = Receiver::new(vec![500, 500, 200]);
        let (status, updates) = deliver(receiver.clone(), policy()).await;
        assert_eq!(status.state, DeliveryState::Delivered);
        assert_eq!(status.attempts, 3);
        assert_eq!(status.last_status, Some(200));
        assert!(status.last_error.is_none());
        assert_eq!(updates.iter().map(|s| (s.attempts, s.state)).collect::<Vec<_>>(), vec![(1, DeliveryState::Pending), (2, DeliveryState::Pending), (3, DeliveryState::Delivered)]);
        let seen = receiver.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        for (id, body) in seen.iter() {
            assert_eq!(id, &status.delivery_id);
            assert_eq!(body, &seen[0].1);
        }
        assert_eq!(receiver.processed.lock().unwrap().len(), 1);
        let id = uuid::Uuid::parse_str(&status.delivery_id).unwrap();
        assert_eq!(id.get_version_num(), 4);
    }

    #[tokio::test]
    async fn bad_request_is_terminal_and_error_is_bounded() {
        let (status, updates) = deliver(Receiver::new(vec![400]), policy()).await;
        assert_eq!(status.state, DeliveryState::Failed);
        assert_eq!(status.attempts, 1);
        assert_eq!(status.last_status, Some(400));
        assert_eq!(updates.len(), 1);
        assert_eq!(status.last_error.unwrap().strip_prefix("HTTP 400: ").unwrap().chars().count(), 200);
    }

    #[tokio::test]
    async fn exhausts_attempts() {
        let (status, updates) = deliver(Receiver::new(vec![500]), policy()).await;
        assert_eq!(status.state, DeliveryState::Failed);
        assert_eq!(status.attempts, 3);
        assert_eq!(status.last_status, Some(500));
        assert_eq!(updates.len(), 3);
        assert_eq!(updates[2].state, DeliveryState::Failed);
    }

    #[tokio::test]
    async fn timeout_is_retried() {
        let mut receiver = Receiver::new(vec![200]);
        receiver.delay = Duration::from_secs(1);
        let mut policy = policy();
        policy.attempt_timeout = Duration::from_millis(30);
        let (status, updates) = deliver(receiver, policy).await;
        assert_eq!(status.state, DeliveryState::Failed);
        assert_eq!(status.attempts, 3);
        assert_eq!(updates.len(), 3);
        assert!(updates.iter().all(|s| s.last_error.as_deref() == Some("attempt timed out")));
    }

    #[tokio::test]
    async fn retry_after_zero_and_request_timeout_status() {
        for code in [408, 429, 503] {
            let mut receiver = Receiver::new(vec![code, 200]);
            receiver.retry_after = Some("0");
            let (status, _) = deliver(receiver, policy()).await;
            assert_eq!(status.state, DeliveryState::Delivered);
            assert_eq!(status.attempts, 2);
        }
    }

    #[tokio::test]
    async fn retry_after_http_date_applies_to_fresh_and_resumed_deliveries() {
        for code in [429, 503] {
            for resume in [false, true] {
                let mut receiver = Receiver::new(vec![code, 200]);
                // A past HTTP-date means zero delay, not the 30-second fallback backoff.
                receiver.retry_after = Some("Wed, 21 Oct 2015 07:28:00 GMT");
                let (target, _server) = server(receiver.clone()).await;
                let policy = RetryPolicy { initial_backoff: Duration::from_secs(30), max_backoff: Duration::from_secs(60), ..policy() };
                let deliverer = Deliverer::new(agent_ai::http::client(), policy);
                let updates = Mutex::new(Vec::new());
                let on_update = |status: &DeliveryStatus| updates.lock().unwrap().push(status.clone());
                let payload = serde_json::json!({"ok": true});
                let status = tokio::time::timeout(Duration::from_secs(3), async {
                    if resume {
                        let saved = DeliveryStatus {
                            delivery_id: "journaled-delivery".into(),
                            event: "task.done".into(),
                            state: DeliveryState::Pending,
                            attempts: 1,
                            last_status: Some(500),
                            last_error: None,
                        };
                        deliverer.resume(&target, &payload, saved, &on_update).await
                    } else {
                        deliverer.deliver(&target, "task.done", &payload, &on_update).await
                    }
                })
                .await
                .expect("HTTP-date must override fallback backoff, including after restart");
                assert_eq!(status.state, DeliveryState::Delivered);
                assert_eq!(status.attempts, if resume { 3 } else { 2 });
                if resume {
                    assert_eq!(status.delivery_id, "journaled-delivery");
                    assert_eq!(updates.lock().unwrap().iter().map(|s| s.attempts).collect::<Vec<_>>(), vec![2, 2, 3, 3]);
                }
                let seen = receiver.seen.lock().unwrap();
                assert_eq!(seen.len(), 2);
                assert!(seen.iter().all(|(id, body)| id == &status.delivery_id && body == &seen[0].1));
            }
        }
    }

    #[tokio::test]
    async fn unsigned_delivery() {
        let mut receiver = Receiver::new(vec![204]);
        receiver.secret = None;
        let (status, _) = deliver(receiver, policy()).await;
        assert_eq!(status.state, DeliveryState::Delivered);
        assert_eq!(status.attempts, 1);
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let receiver = Receiver::new(vec![302, 200]);
        let (status, _) = deliver(receiver.clone(), policy()).await;
        assert_eq!(status.state, DeliveryState::Failed);
        assert_eq!(status.last_status, Some(302));
        assert_eq!(receiver.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancellation_stops_retries() {
        let mut receiver = Receiver::new(vec![500]);
        receiver.delay = Duration::from_millis(100);
        let (target, _server) = server(receiver.clone()).await;
        let task = tokio::spawn(async move { Deliverer::new(agent_ai::http::client(), policy()).deliver(&target, "task.done", &serde_json::json!({"ok": true}), &|_| {}).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while receiver.seen.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(receiver.seen.lock().unwrap().len(), 1);
    }
}
