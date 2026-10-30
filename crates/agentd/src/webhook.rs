//! Temporary W1 integration stub; replaced by the retrying deliverer at merge.
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct WebhookTarget {
    pub url: String,
    pub secret: Option<String>,
}

impl std::fmt::Debug for WebhookTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookTarget").field("host", &reqwest::Url::parse(&self.url).ok().and_then(|u| u.host_str().map(str::to_owned))).field("secret", &"<redacted>").finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Pending,
    Delivered,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize)]
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
#[allow(dead_code)] // Shared interface; retry fields are used by the replacement implementation.
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
    http: reqwest::Client,
    policy: RetryPolicy,
}

impl Deliverer {
    pub fn new(http: reqwest::Client, policy: RetryPolicy) -> Self {
        Self { http, policy }
    }

    pub async fn deliver(&self, target: &WebhookTarget, event: &str, payload: &serde_json::Value, on_update: &(dyn Fn(&DeliveryStatus) + Send + Sync)) -> DeliveryStatus {
        let mut status = DeliveryStatus { delivery_id: uuid::Uuid::new_v4().to_string(), event: event.into(), state: DeliveryState::Pending, attempts: 0, last_status: None, last_error: None };
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        let body = serde_json::json!({"deliveryId": status.delivery_id, "event": event, "timestamp": timestamp, "data": payload}).to_string().into_bytes();
        let mut request = self
            .http
            .post(&target.url)
            .timeout(self.policy.attempt_timeout)
            .header("content-type", "application/json")
            .header("user-agent", concat!("sub-bot/", env!("CARGO_PKG_VERSION")))
            .header("X-SubBot-Event", event)
            .header("X-SubBot-Delivery", &status.delivery_id)
            .header("X-SubBot-Timestamp", timestamp.to_string());
        if let Some(secret) = &target.secret {
            request = request.header("X-SubBot-Signature", sign(secret.as_bytes(), timestamp, &body));
        }
        status.attempts = 1;
        match request.body(body).send().await {
            Ok(response) => {
                status.last_status = Some(response.status().as_u16());
                status.state = if response.status().is_success() { DeliveryState::Delivered } else { DeliveryState::Failed };
                if status.state == DeliveryState::Failed {
                    status.last_error = Some("webhook HTTP error".into());
                }
            }
            Err(_) => {
                status.state = DeliveryState::Failed;
                status.last_error = Some("webhook request failed".into());
            }
        }
        on_update(&status);
        status
    }
}

pub fn sign(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else { return String::new() };
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(body);
    let hex: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
    format!("t={timestamp},v1={hex}")
}

#[allow(dead_code)] // Public receiver helper, not called by the sender.
pub fn verify(secret: &[u8], header: &str, body: &[u8], now: i64, tolerance_secs: i64) -> bool {
    let Some((timestamp, signature)) = header.strip_prefix("t=").and_then(|s| s.split_once(",v1=")) else { return false };
    let Ok(timestamp) = timestamp.parse::<i64>() else { return false };
    if tolerance_secs < 0 || now.abs_diff(timestamp) > tolerance_secs as u64 || signature.len() != 64 {
        return false;
    }
    let expected = sign(secret, timestamp, body);
    let Some((_, expected)) = expected.split_once(",v1=") else { return false };
    expected.bytes().zip(signature.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}
