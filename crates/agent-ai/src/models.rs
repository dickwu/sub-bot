//! 模型目录：从 provider 实时拉取的模型列表 + 按模型名推断的能力（是否可用于 Chat Completions、支持哪些 reasoning_effort）。
//! 推断表只是“尽力而为”：运行时若 OpenAI 返回 reasoning_effort 不支持，openai.rs 会解析错误信息自动降级并缓存。
use crate::types::ThinkingLevel::{self, *};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    /// 是否支持 reasoning_effort；`None` = 未知
    pub reasoning: Option<bool>,
    /// 推断出的可选推理强度（未知时为空）
    #[serde(default)]
    pub reasoning_efforts: Vec<ThinkingLevel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningSupport {
    /// 明确不支持 reasoning_effort（如 gpt-4.1 / gpt-4o）
    Unsupported,
    /// 已知支持的取值
    Levels(&'static [ThinkingLevel]),
    /// 不认识的模型：照原样发送，失败时由运行时降级
    Unknown,
}

const GPT5: &[ThinkingLevel] = &[Minimal, Low, Medium, High];
const GPT51: &[ThinkingLevel] = &[Off, Low, Medium, High];
const GPT52P: &[ThinkingLevel] = &[Off, Low, Medium, High, Xhigh];
const O3: &[ThinkingLevel] = &[Low, Medium, High, Xhigh];
const O_SERIES: &[ThinkingLevel] = &[Low, Medium, High];
const MEDIUM_ONLY: &[ThinkingLevel] = &[Medium];

/// 去掉日期快照后缀（`-2025-08-07`）。
fn base(id: &str) -> &str {
    let b = id.as_bytes();
    if id.len() > 11 && b[id.len() - 11] == b'-' && id[id.len() - 10..].chars().enumerate().all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() }) {
        &id[..id.len() - 11]
    } else {
        id
    }
}

/// 带日期的快照：`-YYYY-MM-DD` 或老式的 `-MMDD`（如 gpt-4-0613）。
pub fn is_snapshot(id: &str) -> bool {
    let four = id.len() > 5 && id.as_bytes()[id.len() - 5] == b'-' && id[id.len() - 4..].chars().all(|c| c.is_ascii_digit());
    base(id) != id || four
}

/// /v1/models 仍会返回、但调用时已提示 deprecated 的模型（2026-10 实测），默认列表里隐藏。
pub fn is_deprecated(id: &str) -> bool {
    matches!(id, "o1" | "o3-mini" | "o4-mini") || id.ends_with("-chat-latest")
}

/// 按模型名推断 reasoning_effort 支持情况（2026-10 实测 OpenAI 的结果）。
pub fn reasoning_support(model: &str) -> ReasoningSupport {
    let m = base(model);
    if m.starts_with("gpt-4") || m.starts_with("gpt-3.5") || m.starts_with("chatgpt-4o") {
        return ReasoningSupport::Unsupported;
    }
    if m == "chat-latest" {
        return ReasoningSupport::Levels(MEDIUM_ONLY);
    }
    if m == "o3" {
        return ReasoningSupport::Levels(O3);
    }
    if m.len() > 1 && m.starts_with('o') && m.as_bytes()[1].is_ascii_digit() {
        return ReasoningSupport::Levels(O_SERIES);
    }
    if let Some(rest) = m.strip_prefix("gpt-5") {
        return match rest {
            "" | "-mini" | "-nano" => ReasoningSupport::Levels(GPT5),
            _ if rest.starts_with(".1") => ReasoningSupport::Levels(GPT51),
            _ if rest.starts_with('.') => ReasoningSupport::Levels(GPT52P),
            _ => ReasoningSupport::Unknown,
        };
    }
    ReasoningSupport::Unknown
}

/// 能否用 Chat Completions 对话（排除 embedding / 语音 / 图像 / realtime / 仅 Responses API 的 codex、pro、deep-research 等）。
pub fn is_chat_model(id: &str) -> bool {
    let chat_prefix = id.starts_with("gpt-") || id.starts_with("chatgpt-") || id == "chat-latest" || (id.len() > 1 && id.starts_with('o') && id.as_bytes()[1].is_ascii_digit());
    const EXCLUDE: &[&str] = &[
        "embedding", "tts", "whisper", "transcribe", "audio", "realtime", "image", "search", "moderation", "instruct", "codex", "-pro", "deep-research", "computer-use", "live", "translate", "dall-e", "sora",
    ];
    chat_prefix && !EXCLUDE.iter().any(|x| id.contains(x)) && !is_deprecated(id)
}

pub fn info_for(id: String, owned_by: Option<String>, created: Option<i64>) -> ModelInfo {
    let (reasoning, reasoning_efforts) = match reasoning_support(&id) {
        ReasoningSupport::Unsupported => (Some(false), vec![]),
        ReasoningSupport::Levels(l) => (Some(true), l.to_vec()),
        ReasoningSupport::Unknown => (None, vec![]),
    };
    ModelInfo { id, owned_by, created, reasoning, reasoning_efforts }
}

/// 模型白名单匹配：精确匹配，或 `*` 通配（如 `gpt-5*`、`*-mini`）。
pub fn model_matches(pattern: &str, model: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == model,
        Some((pre, post)) => model.len() >= pre.len() + post.len() && model.starts_with(pre) && model.ends_with(post),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capabilities() {
        assert_eq!(reasoning_support("gpt-4.1-mini"), ReasoningSupport::Unsupported);
        assert_eq!(reasoning_support("gpt-5-mini-2025-08-07"), ReasoningSupport::Levels(GPT5));
        assert_eq!(reasoning_support("gpt-5.1"), ReasoningSupport::Levels(GPT51));
        assert_eq!(reasoning_support("gpt-5.4-mini"), ReasoningSupport::Levels(GPT52P));
        assert_eq!(reasoning_support("o4-mini"), ReasoningSupport::Levels(O_SERIES));
        assert_eq!(reasoning_support("gpt-6-sol"), ReasoningSupport::Unknown);
        assert!(is_chat_model("gpt-5.4-mini") && is_chat_model("o3") && is_chat_model("gpt-4.1"));
        assert!(!is_chat_model("gpt-5.3-codex") && !is_chat_model("text-embedding-3-small") && !is_chat_model("gpt-5.5-pro") && !is_chat_model("gpt-realtime"));
        assert!(is_snapshot("gpt-4.1-2025-04-14") && !is_snapshot("gpt-4.1"));
        assert!(is_snapshot("gpt-4-0613") && !is_snapshot("gpt-3.5-turbo-16k") && !is_chat_model("o4-mini") && !is_chat_model("gpt-5.2-chat-latest") && is_chat_model("chat-latest"));
        assert!(model_matches("gpt-5*", "gpt-5.4-mini") && model_matches("*-mini", "gpt-4.1-mini") && !model_matches("gpt-5*", "gpt-4.1"));
        assert_eq!(Off.nearest_in(GPT5), Some(Minimal));
        assert_eq!(Minimal.nearest_in(GPT51), Some(Low));
        assert_eq!(Off.nearest_in(O3), Some(Low));
        assert_eq!(Xhigh.nearest_in(GPT5), Some(High));
    }
}
