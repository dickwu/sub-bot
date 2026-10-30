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
    /// 推理 token（包含在 output 里）
    #[serde(default)]
    pub reasoning: u64,
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
    /// 实际使用的 API（chat / responses）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<Api>,
    /// 实际发送给模型的推理强度（None = 未发送）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ThinkingLevel>,
}

impl AssistantMessage {
    pub fn empty(model: &str) -> Self {
        Self { content: vec![], model: model.into(), stop_reason: StopReason::Stop, usage: Usage::default(), error_message: None, timestamp: now_ms(), api: None, reasoning_effort: None }
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

/// OpenAI 的 API 选择：`auto` 按模型决定（gpt-5.4+、gpt-6.x、o 系列用 Responses，其余用 Chat Completions）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    #[default]
    Auto,
    Chat,
    Responses,
}

impl Api {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Chat => "chat",
            Self::Responses => "responses",
        }
    }
}

impl std::fmt::Display for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Api {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "chat" | "chat-completions" | "completions" => Ok(Self::Chat),
            "responses" => Ok(Self::Responses),
            o => Err(format!("unknown api '{o}' (auto|chat|responses)")),
        }
    }
}

/// 推理强度（OpenAI `reasoning_effort`）。`Off` 序列化为 `"none"`（也接受 `"off"`）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[serde(rename = "none", alias = "off")]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ThinkingLevel {
    pub const ALL: [ThinkingLevel; 6] = [Self::Off, Self::Minimal, Self::Low, Self::Medium, Self::High, Self::Xhigh];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
    /// 在模型支持的取值里挑最接近的（距离相同取更高的一档）。
    pub fn nearest_in(self, supported: &[ThinkingLevel]) -> Option<ThinkingLevel> {
        let idx = |l: ThinkingLevel| l as i32;
        supported.iter().copied().min_by_key(|s| ((idx(*s) - idx(self)).abs(), -idx(*s)))
    }
}

impl std::fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ThinkingLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "none" | "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            o => Err(format!("unknown reasoning effort '{o}' (none|minimal|low|medium|high|xhigh)")),
        }
    }
}

/// 流式事件。对应 `AssistantMessageEvent`（去掉每条事件里的 partial，参照 `packages/agent/src/proxy.ts` 的瘦身格式）。
/// 约定：流**永不抛错**，最后一个事件必为 `Done` 或 `Error`，并携带完整 AssistantMessage。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AssistantEvent {
    Start,
    /// Discard all text/thinking from the previous attempt of this call.
    Retry {
        attempt: u32,
        reason: String,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ToolcallStart {
        content_index: usize,
        id: String,
        tool_name: String,
    },
    ToolcallDelta {
        content_index: usize,
        delta: String,
    },
    ToolcallEnd {
        content_index: usize,
        tool_call: ToolCall,
    },
    Done {
        message: AssistantMessage,
    },
    Error {
        message: AssistantMessage,
    },
}
