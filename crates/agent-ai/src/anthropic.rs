// STUB - replaced by v070-a
use crate::{AssistantEvent, AssistantMessage, EventStream, Provider, Request, Secret, StopReason};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct AnthropicProvider;
impl AnthropicProvider {
    pub fn new(api_key: Secret, base_url: Option<String>) -> Self {
        Self::with_stream_idle_timeout(api_key, base_url, Duration::from_secs(300))
    }
    pub fn with_stream_idle_timeout(_api_key: Secret, _base_url: Option<String>, _stream_idle_timeout: Duration) -> Self {
        Self
    }
}
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }
    fn stream(&self, req: Request, _cancel: CancellationToken) -> EventStream {
        let mut message = AssistantMessage::empty(&req.model);
        message.stop_reason = StopReason::Error;
        message.error_message = Some("anthropic stub".into());
        Box::pin(futures::stream::iter([AssistantEvent::Error { message }]))
    }
    fn list_models(&self) -> futures::future::BoxFuture<'_, Result<Vec<crate::ModelInfo>, String>> {
        Box::pin(async { Err("anthropic stub".into()) })
    }
}
