//! 统一 LLM 层，对应 pi-mono `packages/ai`。
//! - `types`：消息/内容块/用量（对应 `packages/ai/src/types.ts`）
//! - `provider`：Provider trait 与注册表（对应 `models.ts` 的 `streamSimple` 分发 + `api/*`）
//! - `openai`：OpenAI provider，按模型选择 Chat Completions（`api/openai-completions.ts`）或 Responses API（`responses.rs`，`api/openai-responses.ts`）
//! - `secrets`：`Secret`（打码、析构清零）、provider key 加载（文件优先）与子进程需剔除的密钥变量清单
//! - `keychain`：macOS Keychain key 来源（其它平台为 stub）
//! - `faux`：离线假 provider，用于测试（对应 `providers/faux.ts`）
pub mod anthropic;
pub mod faux;
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

pub use models::{allowed_efforts, effort_caps, resolve_effort, resolve_effort_quiet, EffortCaps, EffortMechanism, EffortResolution, ModelInfo, ReasoningSupport};
pub use models::{context_window, default_api};
pub use provider::{EventStream, ProbeThinking, Provider, ProviderRegistry, Request};
pub use secrets::Secret;
pub use types::*;
