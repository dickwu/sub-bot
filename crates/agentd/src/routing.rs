//! 模型路由：每个会话在创建时或第一次 prompt 时选择基础模型（standard）或深度模型（deep），之后不再改变。
//! - `depth=standard|deep|auto`；调用方显式给出 `model` 时总是优先（depth 被忽略）。
//! - `auto`：用 routerModel 做一次便宜的分类调用（低推理强度、≤200 输出 token、无工具、30 秒超时）。
//! - `deepModel: "latest:<prefix>"`：按 provider 模型目录解析为该前缀下版本号最新的模型。
use agent_ai::{AssistantEvent, Message, Provider, Request, ThinkingLevel, Usage};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Depth {
    #[default]
    Standard,
    Deep,
    Auto,
}

impl Depth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Deep => "deep",
            Self::Auto => "auto",
        }
    }
}

impl std::str::FromStr for Depth {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "standard" => Ok(Self::Standard),
            "deep" => Ok(Self::Deep),
            "auto" => Ok(Self::Auto),
            _ => Err(format!("invalid depth '{s}' (expected standard|deep|auto)")),
        }
    }
}

/// 对外报告的路由结果（会话视图、chat 响应、task 结果、ask --json、RPC get_state）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Routing {
    pub depth: Depth,
    /// 选中的模型（第一次 prompt 之前 auto 时为基础模型）
    pub model: String,
    pub base_model: String,
    /// 深度模型：已解析的 id；`latest:` 别名在解析前保持原样
    pub deep_model: Option<String>,
    /// 是否经过自动分类
    pub auto: bool,
    pub reason: String,
    /// 分类调用的用量（单独统计）
    pub usage: Option<Usage>,
    /// 路由是否已确定（第一次 prompt 之后不再改变）
    pub decided: bool,
    #[serde(skip)]
    pub explicit: bool,
}

pub const ROUTER_TIMEOUT: Duration = Duration::from_secs(30);
pub const ROUTER_MAX_TOKENS: u32 = 200;
/// 只把任务开头交给分类器，控制成本。
const ROUTER_INPUT_CHARS: usize = 8000;

pub const ROUTER_SYSTEM_PROMPT: &str = "You are a request router for a coding agent. Classify the user's task.\n\
Answer deep=true when the task is ambiguous, open-ended or underspecified, or needs deep multi-step reasoning, architecture/design work, or research.\n\
Answer deep=false when the task is clear, concrete and well-scoped (a direct question, a small edit, a lookup, a command to run).\n\
Do not perform the task. Respond with ONLY a single JSON object and nothing else, in the form \
{\"deep\": true or false, \"reason\": \"at most 20 words\"}.";

/// 在回复里找第一个含布尔 `deep` 字段的 JSON 对象（容忍代码块、前后缀文字）。
pub fn parse_router_reply(text: &str) -> Result<(bool, String), String> {
    for (start, _) in text.match_indices('{') {
        let mut values = serde_json::Deserializer::from_str(&text[start..]).into_iter::<serde_json::Value>();
        if let Some(Ok(value)) = values.next() {
            if let Some(deep) = value.get("deep").and_then(serde_json::Value::as_bool) {
                let reason = value.get("reason").and_then(serde_json::Value::as_str).unwrap_or("").split_whitespace().take(20).collect::<Vec<_>>().join(" ");
                return Ok((deep, reason.chars().take(200).collect()));
            }
        }
    }
    // 不回显模型输出：可能包含用户任务内容，而错误原因会进入 issue。
    Err(format!("router reply contained no JSON decision ({} chars)", text.chars().count()))
}

/// 路由分类请求使用的推理强度：模型允许 low 时用 low，否则不发送（避免被钳制成高强度）。
pub fn router_effort(provider_id: &str, model: &str) -> Option<ThinkingLevel> {
    let caps = agent_ai::effort_caps(provider_id, model);
    agent_ai::allowed_efforts(&caps).contains(&ThinkingLevel::Low).then_some(ThinkingLevel::Low)
}

pub struct Classification {
    pub deep: bool,
    pub reason: String,
}

/// 一次分类调用；返回 (结果或错误, 用量)。
pub async fn classify(provider: &dyn Provider, model: &str, task: &str) -> (Result<Classification, String>, Option<Usage>) {
    let req = Request {
        model: model.into(),
        system_prompt: Some(ROUTER_SYSTEM_PROMPT.into()),
        messages: vec![Message::user_text(task.chars().take(ROUTER_INPUT_CHARS).collect::<String>())],
        tools: vec![],
        reasoning: router_effort(provider.id(), model),
        raw_effort: false,
        thinking: None,
        api: agent_ai::Api::Auto,
        max_tokens: Some(ROUTER_MAX_TOKENS),
        llm_max_retries: Some(1),
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    let call = async {
        let mut stream = provider.stream(req, cancel.clone());
        while let Some(event) = stream.next().await {
            match event {
                AssistantEvent::Done { message } => return Ok(message),
                AssistantEvent::Error { message } => return Err((message.error_message.clone().unwrap_or_else(|| "router call failed".into()), Some(message.usage))),
                _ => {}
            }
        }
        Err(("router stream ended without a result".into(), None))
    };
    match tokio::time::timeout(ROUTER_TIMEOUT, call).await {
        Err(_) => (Err(format!("timed out after {}s", ROUTER_TIMEOUT.as_secs())), None),
        Ok(Err((error, usage))) => (Err(error.chars().take(200).collect()), usage),
        Ok(Ok(message)) => (parse_router_reply(&message.text()).map(|(deep, reason)| Classification { deep, reason }), Some(message.usage)),
    }
}

/// 解析模型 id 中前缀之后的版本号；返回 (版本号, 日期快照)。含非数字段（如 `-preview`）的 id 不参与排名。
fn version_of(id: &str, prefix: &str) -> Option<(Vec<u64>, Option<u64>)> {
    let rest = id.strip_prefix(prefix)?;
    if !(rest.is_empty() || rest.starts_with('-') || rest.starts_with('.')) {
        return None;
    }
    let mut parts: Vec<&str> = rest.split(['-', '.']).filter(|p| !p.is_empty()).collect();
    let date = match parts.last() {
        Some(last) if last.len() == 8 && last.bytes().all(|b| b.is_ascii_digit()) => {
            let date = last.parse().ok();
            parts.pop();
            date
        }
        _ => None,
    };
    let version = parts.iter().map(|p| p.parse::<u64>().ok()).collect::<Option<Vec<_>>>()?;
    Some((version, date))
}

/// `latest:<prefix>`：选版本号最高的模型（claude-fable-5-1 > claude-fable-5）；同一版本存在无日期 id 时忽略日期快照，
/// 只有快照时取日期最新的一个。
pub fn resolve_latest<'a>(prefix: &str, ids: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let candidates: Vec<(&str, Vec<u64>, Option<u64>)> = ids.into_iter().filter_map(|id| version_of(id, prefix).map(|(v, d)| (id, v, d))).collect();
    candidates
        .iter()
        .filter(|(_, version, date)| date.is_none() || !candidates.iter().any(|(_, v, d)| d.is_none() && v == version))
        // 版本相同：无日期（当前别名）优先，其次日期更新的快照。
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.is_none().cmp(&b.2.is_none())).then_with(|| a.2.cmp(&b.2)))
        .map(|(id, _, _)| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_alias_ranking() {
        let ids = ["claude-fable-5", "claude-fable-5-20260101", "claude-fable-5-1", "claude-opus-5-5", "claude-fable-4-9", "claude-fablex-9", "claude-fable-preview"];
        assert_eq!(resolve_latest("claude-fable", ids).as_deref(), Some("claude-fable-5-1"));
        assert_eq!(resolve_latest("claude-fable", ["claude-fable-5", "claude-fable-5-20260101"]).as_deref(), Some("claude-fable-5"));
        assert_eq!(resolve_latest("claude-fable", ["claude-fable-5-20260101", "claude-fable-5-20260301", "claude-fable-4"]).as_deref(), Some("claude-fable-5-20260301"));
        assert_eq!(resolve_latest("claude-fable", ["claude-fable-5-10", "claude-fable-5-9"]).as_deref(), Some("claude-fable-5-10"));
        assert_eq!(resolve_latest("claude-fable", ["claude-opus-5-5"]), None);
        assert_eq!(resolve_latest("gpt-5", ["gpt-5.4", "gpt-5.10", "gpt-5"]).as_deref(), Some("gpt-5.10"));
    }

    #[test]
    fn router_reply_parsing_is_robust() {
        for (text, deep) in [
            (r#"{"deep":true,"reason":"open-ended design"}"#, true),
            ("```json\n{\"deep\": false, \"reason\": \"small edit\"}\n```", false),
            (r#"Sure. {not json} then {"reason":"x","deep":true} trailing"#, true),
        ] {
            assert_eq!(parse_router_reply(text).unwrap().0, deep, "{text}");
        }
        let long = format!(r#"{{"deep":true,"reason":"{}"}}"#, "word ".repeat(40));
        assert_eq!(parse_router_reply(&long).unwrap().1.split_whitespace().count(), 20);
        for text in ["", "deep", r#"{"deep":"yes"}"#, "{"] {
            assert!(parse_router_reply(text).is_err(), "{text}");
        }
        assert_eq!("auto".parse::<Depth>().unwrap(), Depth::Auto);
        assert!("Deep".parse::<Depth>().is_err());
    }

    #[test]
    fn router_effort_never_clamps_up() {
        assert_eq!(router_effort("openai", "gpt-5"), Some(ThinkingLevel::Low));
        assert_eq!(router_effort("anthropic", "claude-opus-5-5"), None);
        assert_eq!(router_effort("anthropic", "claude-sonnet-4-5"), Some(ThinkingLevel::Low));
        assert_eq!(router_effort("faux", "faux"), None);
    }
}
