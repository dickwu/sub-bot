use crate::types::*;
use futures::Stream;
use std::{collections::HashMap, pin::Pin, sync::Arc};
use tokio_util::sync::CancellationToken;

pub type EventStream = Pin<Box<dyn Stream<Item = AssistantEvent> + Send>>;

/// 一次模型请求（对应 pi 的 TranscriptContext + SimpleStreamOptions）。
#[derive(Debug, Clone)]
pub struct Request {
    pub model: String,
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub reasoning: Option<ThinkingLevel>,
    /// 探测专用：绕过目录、能力缓存、降级及瞬态重试。
    pub raw_effort: bool,
    /// 仅 raw_effort 时覆盖 Anthropic thinking；默认 adaptive。
    pub thinking: Option<ProbeThinking>,
    /// 走哪个 OpenAI API（其它 provider 忽略）
    pub api: Api,
    pub max_tokens: Option<u32>,
    /// Additional transient-error attempts; None uses the default (3).
    pub llm_max_retries: Option<u32>,
}

/// 原始能力探测的 Anthropic thinking 变体，不改变正常请求的模型机制。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeThinking {
    Adaptive,
    Omit,
    Enabled,
}

/// Provider 抽象，对应 pi-ai 中每个 `api/*.ts` 的 `streamSimple`。
/// 实现方必须把失败编码进事件流（`AssistantEvent::Error`），不能 panic / 返回 Err。
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn stream(&self, req: Request, cancel: CancellationToken) -> EventStream;
    /// 实时拉取 provider 的模型列表（未过滤）。默认不支持。
    fn list_models(&self) -> futures::future::BoxFuture<'_, Result<Vec<crate::models::ModelInfo>, String>> {
        Box::pin(async { Err("this provider does not support listing models".to_string()) })
    }
}

/// provider 注册表，对应 `models.ts` 里按 model.api 分发。
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn register(&mut self, p: Arc<dyn Provider>) {
        self.providers.insert(p.id().to_string(), p);
    }
    pub fn get(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(id).cloned()
    }
}
