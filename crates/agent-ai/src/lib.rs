//! 统一 LLM 层，对应 pi-mono `packages/ai`。
//! - `types`：消息/内容块/用量（对应 `packages/ai/src/types.ts`）
//! - `provider`：Provider trait 与注册表（对应 `models.ts` 的 `streamSimple` 分发 + `api/*`）
//! - `openai`：OpenAI provider，按模型选择 Chat Completions（`api/openai-completions.ts`）或 Responses API（`responses.rs`，`api/openai-responses.ts`）
//! - `secrets`：`Secret`（打码、析构清零）、provider key 加载（文件优先）与子进程需剔除的密钥变量清单
//! - `http`：共享 reqwest 客户端构建器（rustls + ring + webpki-roots），所有出站客户端都经它构建
//! - `keychain`：macOS Keychain key 来源（其它平台为 stub）
//! - `faux`：离线假 provider，用于测试（对应 `providers/faux.ts`）
pub mod anthropic;
pub mod faux;
pub mod http;
pub mod keychain;
pub mod models;
pub mod openai;
pub mod provider;
pub mod responses;
mod retry;
pub mod secrets;
pub mod types;

/// Maximum server-requested retry delay for LLM calls and webhook delivery.
pub const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// Upper bound for `streamIdleTimeoutSecs` (2 hours); the lower bound is 1.
pub const MAX_STREAM_IDLE_TIMEOUT_SECS: u64 = 7200;
/// Default stream idle timeout (max silence between streamed body bytes) when nothing is configured.
pub const DEFAULT_STREAM_IDLE_TIMEOUT_SECS: u64 = 1800;
/// Upper bound for per-request / per-profile `llmMaxRetries`.
pub const MAX_LLM_MAX_RETRIES: u32 = 20;
/// TCP keepalive for LLM connections: first probe after 30s idle, then every 10s, give up after 5 probes.
/// (No HTTP/2 PINGs: TLS ALPN only offers http/1.1, see [`http`].)
pub const LLM_TCP_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(30);

/// Validates a stream idle timeout in seconds (1..=[`MAX_STREAM_IDLE_TIMEOUT_SECS`]).
pub fn validate_stream_idle_timeout_secs(secs: u64) -> Result<u64, String> {
    if (1..=MAX_STREAM_IDLE_TIMEOUT_SECS).contains(&secs) {
        Ok(secs)
    } else {
        Err(format!("streamIdleTimeoutSecs must be between 1 and {MAX_STREAM_IDLE_TIMEOUT_SECS} (2 hours), got {secs}"))
    }
}

pub use models::{allowed_efforts, effort_caps, resolve_effort, resolve_effort_quiet, EffortCaps, EffortMechanism, EffortResolution, ModelInfo, ReasoningSupport};
pub use models::{context_window, default_api};
pub use provider::{EventStream, ProbeThinking, Provider, ProviderRegistry, Request};
pub use secrets::Secret;
pub use types::*;
