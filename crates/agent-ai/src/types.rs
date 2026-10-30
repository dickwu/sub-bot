use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 内容块。对应 pi-ai 的 TextContent / ThinkingContent / ImageContent / ToolCall。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Content {
    Text { text: String },
    Thinking { thinking: String },
    Image { data: String, mime_type: String },
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<Content>,
    pub model: String,
    pub stop_reason: StopReason,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

impl AssistantMessage {
    pub fn empty(model: &str) -> Self {
        Self { content: vec![], model: model.into(), stop_reason: StopReason::Stop, usage: Usage::default(), error_message: None, timestamp: now_ms() }
    }
    pub fn failed(model: &str, reason: StopReason, msg: impl Into<String>) -> Self {
        Self { stop_reason: reason, error_message: Some(msg.into()), ..Self::empty(model) }
    }
    pub fn text(&self) -> String {
        self.content.iter().filter_map(|c| if let Content::Text { text } = c { Some(text.as_str()) } else { None }).collect()
    }
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.content.iter().filter_map(|c| if let Content::ToolCall(t) = c { Some(t) } else { None }).collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<Content>,
    pub is_error: bool,
    pub timestamp: i64,
}

/// LLM 可见的消息。对应 pi-ai `Message`（user / assistant / toolResult）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User { content: Vec<Content>, timestamp: i64 },
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message::User { content: vec![Content::Text { text: text.into() }], timestamp: now_ms() }
    }
}

/// 发给模型的工具声明（JSON Schema 参数）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
}

/// 流式事件。对应 `AssistantMessageEvent`（去掉每条事件里的 partial，参照 `packages/agent/src/proxy.ts` 的瘦身格式）。
/// 约定：流**永不抛错**，最后一个事件必为 `Done` 或 `Error`，并携带完整 AssistantMessage。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AssistantEvent {
    Start,
    TextDelta { content_index: usize, delta: String },
    ThinkingDelta { content_index: usize, delta: String },
    ToolcallStart { content_index: usize, id: String, tool_name: String },
    ToolcallDelta { content_index: usize, delta: String },
    ToolcallEnd { content_index: usize, tool_call: ToolCall },
    Done { message: AssistantMessage },
    Error { message: AssistantMessage },
}
