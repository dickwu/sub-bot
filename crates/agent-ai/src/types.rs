use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 内容块。对应 pi-ai 的 TextContent / ThinkingContent / ImageContent / ToolCall。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Content {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        /// Anthropic signature, required when replaying extended thinking.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    Image {
        data: String,
        mime_type: String,
    },
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
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write: u64,
    pub total_tokens: u64,
    /// 推理 token（包含在 output 里）
    #[serde(default)]
    pub reasoning: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
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

/// 可扩展的推理强度；未知名称只在 provider 的有序列表中有序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Other(&'static str),
}

impl ThinkingLevel {
    pub const ALL: [ThinkingLevel; 7] = [Self::Off, Self::Minimal, Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Other(name) => name,
        }
    }
    pub fn rank_in(self, levels: &[ThinkingLevel]) -> Option<usize> {
        levels.iter().position(|level| *level == self)
    }
    /// 仅内置名称有全局顺序；Other 不能假定比任何档位高或低。
    pub fn builtin_rank(self) -> Option<usize> {
        self.rank_in(&Self::ALL)
    }
    /// 优先采用列表中的位置；缺失的内置名称按已知档位距离选择，平局取列表中较高的一档。
    pub fn nearest_in(self, supported: &[ThinkingLevel]) -> Option<ThinkingLevel> {
        if let Some(index) = self.rank_in(supported) {
            return Some(supported[index]);
        }
        let rank = self.builtin_rank()?;
        supported
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(index, level)| Some(((rank.abs_diff(level.builtin_rank()?), std::cmp::Reverse(index)), level)))
            .min_by_key(|(key, _)| *key)
            .map(|(_, level)| level)
    }
}

impl PartialOrd for ThinkingLevel {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        if self == other {
            Some(std::cmp::Ordering::Equal)
        } else {
            Some(self.builtin_rank()?.cmp(&other.builtin_rank()?))
        }
    }
}

impl Serialize for ThinkingLevel {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ThinkingLevel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
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
        // 内置名称保持 v0.7 的大小写不敏感（兼容旧配置/环境变量）；其它名称必须是小写。
        match s.to_ascii_lowercase().as_str() {
            "none" | "off" => return Ok(Self::Off),
            "minimal" => return Ok(Self::Minimal),
            "low" => return Ok(Self::Low),
            "medium" => return Ok(Self::Medium),
            "high" => return Ok(Self::High),
            "xhigh" => return Ok(Self::Xhigh),
            "max" => return Ok(Self::Max),
            _ => {}
        }
        match s {
            name if (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-') => {
                use std::{
                    collections::HashSet,
                    sync::{LazyLock, Mutex},
                };
                static NAMES: LazyLock<Mutex<HashSet<&'static str>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
                let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
                let interned = match names.get(name) {
                    Some(name) => *name,
                    None => {
                        let name: &'static str = Box::leak(name.to_owned().into_boxed_str());
                        names.insert(name);
                        name
                    }
                };
                Ok(Self::Other(interned))
            }
            _ => Err("reasoning effort must be a lowercase name [a-z0-9_-]{1,32} (e.g. none, low, high, xhigh, max)".into()),
        }
    }
}

#[cfg(test)]
mod effort_tests {
    use super::ThinkingLevel::*;
    use super::*;

    #[test]
    fn names_roundtrip_and_intern() {
        for name in ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "unknown", "a_1-z", &"a".repeat(32)] {
            let level: ThinkingLevel = name.parse().unwrap();
            assert_eq!(level.to_string(), name);
            assert_eq!(serde_json::to_string(&level).unwrap(), format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<ThinkingLevel>(&format!("\"{name}\"")).unwrap(), level);
            let again: ThinkingLevel = name.to_owned().parse().unwrap();
            assert!(std::ptr::eq(level.as_str(), again.as_str()));
        }
        assert_eq!("off".parse::<ThinkingLevel>().unwrap(), Off);
        assert_eq!(serde_json::from_str::<ThinkingLevel>("\"off\"").unwrap(), Off);
        for (name, level) in [("MAX", Max), ("High", High), ("OFF", Off), ("None", Off), ("XHigh", Xhigh)] {
            assert_eq!(name.parse::<ThinkingLevel>().unwrap(), level);
        }
        // Copy + 内存驻留：同名 Other 永远指向同一个 &'static str。
        fn assert_copy<T: Copy>() {}
        assert_copy::<ThinkingLevel>();
        let ultra: ThinkingLevel = "ultra".parse().unwrap();
        let copied = ultra;
        assert_eq!(ultra, copied);
        assert_eq!(ultra, Other("ultra"));
        assert_eq!(serde_json::from_str::<Vec<ThinkingLevel>>(r#"["max","ultra","none"]"#).unwrap(), [Max, Other("ultra"), Off]);
        for name in ["", "Ultra", "two words", "a.b", "é", &"a".repeat(33)] {
            assert!(name.parse::<ThinkingLevel>().is_err());
        }
    }

    #[test]
    fn only_provider_order_ranks_other_names() {
        assert!(Max > Xhigh);
        assert_eq!(Other("ultra").partial_cmp(&Max), None);
        let levels = [Low, Other("ultra"), High, Other("unknown"), Max];
        assert_eq!(Other("ultra").rank_in(&levels), Some(1));
        assert_eq!(Other("ultra").nearest_in(&levels), Some(Other("ultra")));
        assert_eq!(Other("absent").nearest_in(&levels), None);
        assert_eq!(Xhigh.nearest_in(&[Medium, High, Max]), Some(Max));
        assert_eq!(Minimal.nearest_in(&[Off, Low]), Some(Low));
        assert_eq!(High.nearest_in(&[]), None);
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
