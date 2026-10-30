//! 统一 LLM 层，对应 pi-mono `packages/ai`。
//! - `types`：消息/内容块/用量（对应 `packages/ai/src/types.ts`）
//! - `provider`：Provider trait 与注册表（对应 `models.ts` 的 `streamSimple` 分发 + `api/*`）
//! - `openai`：OpenAI Chat Completions 流式实现（对应 `api/openai-completions.ts`）
//! - `faux`：离线假 provider，用于测试（对应 `providers/faux.ts`）
pub mod faux;
pub mod models;
pub mod openai;
pub mod provider;
pub mod types;

pub use models::{ModelInfo, ReasoningSupport};
pub use provider::{EventStream, Provider, ProviderRegistry, Request};
pub use types::*;
