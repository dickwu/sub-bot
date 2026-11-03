//! 上下文压缩的配置与摘要调用（对应 pi `coding-agent/src/core/compaction`）。切点选择与提示词在
//! `agent_session::compaction`（纯函数）；会话级编排（事件、日志、issue、持久化）在 `sessions.rs`。
use agent_ai::{AssistantEvent, Message, Provider, Request, Usage};
use agent_session::compaction::{self as cp, CutPoint};
use futures::StreamExt;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_THRESHOLD_PCT: u32 = 80;
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20_000;
pub const DEFAULT_MAX_SUMMARY_TOKENS: u32 = 4_000;
/// profile `contextWindow` 的下限
pub const MIN_CONTEXT_WINDOW: u64 = 1_000;

/// 配置里的 `compaction`（全局或 profile；profile 逐字段覆盖全局）。
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompactionConfig {
    /// 是否自动压缩（默认 true）；手动 compact 不受影响
    pub enabled: Option<bool>,
    /// 估计的上下文 token 超过窗口的百分之多少时压缩（1–100，默认 80）
    pub threshold_pct: Option<u32>,
    /// 原样保留的最近消息量（token 估计，默认 20000，<= 1000000）
    pub keep_recent_tokens: Option<u64>,
    /// 摘要用的模型（同一 provider）；默认会话当前模型
    pub model: Option<String>,
    /// 摘要调用的输出上限（256–64000，默认 4000）
    pub max_summary_tokens: Option<u32>,
}

impl CompactionConfig {
    pub fn validate(&self, at: &str) -> anyhow::Result<()> {
        if let Some(p) = self.threshold_pct {
            anyhow::ensure!((1..=100).contains(&p), "{at}.thresholdPct must be between 1 and 100");
        }
        if let Some(k) = self.keep_recent_tokens {
            anyhow::ensure!(k <= 1_000_000, "{at}.keepRecentTokens must be <= 1000000");
        }
        if let Some(m) = self.max_summary_tokens {
            anyhow::ensure!((256..=64_000).contains(&m), "{at}.maxSummaryTokens must be between 256 and 64000");
        }
        if let Some(model) = &self.model {
            anyhow::ensure!(!model.trim().is_empty(), "{at}.model must not be blank (use null for the session model)");
        }
        Ok(())
    }
}

/// 解析后的设置。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub threshold_pct: u32,
    pub keep_recent_tokens: u64,
    pub model: Option<String>,
    pub max_summary_tokens: u32,
}

impl CompactionSettings {
    pub fn resolve(global: &CompactionConfig, profile: Option<&CompactionConfig>) -> Self {
        let p = profile.cloned().unwrap_or_default();
        Self {
            enabled: p.enabled.or(global.enabled).unwrap_or(true),
            threshold_pct: p.threshold_pct.or(global.threshold_pct).unwrap_or(DEFAULT_THRESHOLD_PCT),
            keep_recent_tokens: p.keep_recent_tokens.or(global.keep_recent_tokens).unwrap_or(DEFAULT_KEEP_RECENT_TOKENS),
            model: p.model.or_else(|| global.model.clone()),
            max_summary_tokens: p.max_summary_tokens.or(global.max_summary_tokens).unwrap_or(DEFAULT_MAX_SUMMARY_TOKENS),
        }
    }

    /// 触发阈值（token）
    pub fn threshold(&self, window: u64) -> u64 {
        window.saturating_mul(u64::from(self.threshold_pct)) / 100
    }
}

/// 一次摘要调用的参数。
pub struct SummaryCall<'a> {
    pub provider: &'a dyn Provider,
    pub model: &'a str,
    pub api: agent_ai::Api,
    pub max_tokens: u32,
    pub llm_max_retries: Option<u32>,
}

/// 摘要调用失败：错误信息 + 已消耗的用量（计入 totalUsage）。
#[derive(Debug)]
pub struct SummaryError {
    pub message: String,
    pub usage: Option<Usage>,
}

/// 用会话模型（或配置的摘要模型）做一次无工具调用，返回摘要文本与用量。
pub async fn summarize(call: &SummaryCall<'_>, prompt: String, cancel: CancellationToken) -> Result<(String, Usage), SummaryError> {
    let req = Request {
        model: call.model.into(),
        system_prompt: Some(cp::SUMMARIZATION_SYSTEM_PROMPT.into()),
        messages: vec![Message::user_text(prompt)],
        tools: vec![],
        reasoning: None,
        raw_effort: false,
        thinking: None,
        api: call.api,
        max_tokens: Some(call.max_tokens),
        llm_max_retries: call.llm_max_retries,
    };
    let mut stream = call.provider.stream(req, cancel.clone());
    while let Some(event) = stream.next().await {
        match event {
            AssistantEvent::Done { message } => {
                let text = message.text().trim().to_string();
                if text.is_empty() {
                    return Err(SummaryError { message: "summary model returned no text".into(), usage: Some(message.usage) });
                }
                return Ok((text, message.usage));
            }
            AssistantEvent::Error { message } => {
                return Err(SummaryError { message: message.error_message.clone().unwrap_or_else(|| "summary call failed".into()), usage: Some(message.usage) });
            }
            _ => {}
        }
    }
    Err(SummaryError { message: "summary stream ended without a result".into(), usage: None })
}

/// 压缩结果：新上下文 = [摘要消息] + `messages[cut.cut..]`。
pub struct Outcome {
    pub context: Vec<Message>,
    pub cut: CutPoint,
    pub summary: String,
    pub tokens_after: u64,
    pub usage: Usage,
}

/// 摘要 `messages[cut.start..cut.cut]`（上下文开头已有摘要时把它作为 previous summary，使摘要累积），重建上下文。
/// 保留部分的消息去掉 thinking / redacted thinking 块（Anthropic 把 thinking 签名绑定到之前的对话，摘要改写前缀后
/// 旧签名失效，见 [`cp::strip_thinking`]）；其它内容（文本、工具调用、工具结果）原样不动。JSONL 里的原消息不变，
/// 从 JSONL 重建（`Session::active_context`）做同样的处理。
pub async fn compact(call: &SummaryCall<'_>, messages: &[Message], cut: CutPoint, overhead_tokens: u64, instructions: Option<&str>, cancel: CancellationToken) -> Result<Outcome, SummaryError> {
    let previous = (cut.start == 1).then(|| cp::summary_of(&messages[0])).flatten();
    let prompt = cp::summary_prompt(&messages[cut.start..cut.cut], previous, instructions);
    let (summary, usage) = summarize(call, prompt, cancel).await?;
    let mut context = Vec::with_capacity(messages.len() - cut.cut + 1);
    context.push(cp::summary_message(&summary));
    context.extend(cp::strip_kept_thinking(&messages[cut.cut..]));
    let tokens_after = overhead_tokens + context.iter().map(cp::estimate_tokens).sum::<u64>();
    Ok(Outcome { context, cut, summary, tokens_after, usage })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_merge_and_validate() {
        let global = CompactionConfig { threshold_pct: Some(70), keep_recent_tokens: Some(5000), ..Default::default() };
        let profile = CompactionConfig { threshold_pct: Some(50), model: Some("cheap".into()), ..Default::default() };
        let s = CompactionSettings::resolve(&global, Some(&profile));
        assert_eq!(s, CompactionSettings { enabled: true, threshold_pct: 50, keep_recent_tokens: 5000, model: Some("cheap".into()), max_summary_tokens: 4000 });
        assert_eq!(s.threshold(200_000), 100_000);
        let d = CompactionSettings::resolve(&CompactionConfig::default(), None);
        assert_eq!((d.enabled, d.threshold_pct, d.keep_recent_tokens, d.model, d.max_summary_tokens), (true, 80, 20_000, None, 4000));
        for (bad, msg) in [
            (CompactionConfig { threshold_pct: Some(0), ..Default::default() }, "thresholdPct"),
            (CompactionConfig { threshold_pct: Some(101), ..Default::default() }, "thresholdPct"),
            (CompactionConfig { keep_recent_tokens: Some(2_000_000), ..Default::default() }, "keepRecentTokens"),
            (CompactionConfig { max_summary_tokens: Some(10), ..Default::default() }, "maxSummaryTokens"),
            (CompactionConfig { model: Some(" ".into()), ..Default::default() }, "model"),
        ] {
            assert!(bad.validate("compaction").unwrap_err().to_string().contains(msg));
        }
        assert!(serde_json::from_value::<CompactionConfig>(serde_json::json!({"thresholdPCT": 3})).is_err());
        let parsed: CompactionConfig = serde_json::from_value(serde_json::json!({"enabled": false, "thresholdPct": 80, "keepRecentTokens": 20000, "model": null, "maxSummaryTokens": 4000})).unwrap();
        assert_eq!(parsed.enabled, Some(false));
    }
}
