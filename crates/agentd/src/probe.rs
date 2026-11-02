//! 原始 effort 探测：不建立会话，不启用工具，不应用目录策略或重试缓存。
use agent_ai::{Api, AssistantEvent, Message, ProbeThinking, Provider, Request, StopReason, ThinkingLevel};
use clap::{Args, ValueEnum};
use futures::StreamExt;
use serde::Serialize;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Args)]
pub struct Probe {
    #[arg(long, value_delimiter = ',', default_value = "none,minimal,low,medium,high,xhigh,max,ultra")]
    pub levels: Vec<ThinkingLevel>,
    /// Anthropic thinking 变体；不指定时 adaptive 被 thinking 字段拒绝后再试一次 omit
    #[arg(long, value_enum)]
    pub thinking: Option<Thinking>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Thinking {
    Adaptive,
    Omit,
    Enabled,
}
impl Thinking {
    fn variant(self) -> &'static str {
        match self {
            Self::Adaptive => "adaptive",
            Self::Omit => "omit",
            Self::Enabled => "enabled",
        }
    }
    fn request(self) -> ProbeThinking {
        match self {
            Self::Adaptive => ProbeThinking::Adaptive,
            Self::Omit => ProbeThinking::Omit,
            Self::Enabled => ProbeThinking::Enabled,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    model: String,
    provider: String,
    level: ThinkingLevel,
    accepted: bool,
    status: Option<u16>,
    variant: String,
    error: Option<String>,
    supported_values: Vec<ThinkingLevel>,
}

impl ProbeResult {
    fn line(&self) -> String {
        format!(
            "{} {} {}: {} status={} variant={} error={} supportedValues={}",
            self.provider,
            self.model,
            self.level,
            if self.accepted { "accepted" } else { "rejected" },
            self.status.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
            self.variant,
            serde_json::to_string(&self.error).expect("string JSON"),
            serde_json::to_string(&self.supported_values).expect("levels JSON")
        )
    }
}

fn error_message(error: &str) -> String {
    let body = error.find('{').and_then(|index| serde_json::from_str::<serde_json::Value>(&error[index..]).ok());
    body.as_ref().and_then(|v| v.pointer("/error/message").or_else(|| v.get("message"))).and_then(|v| v.as_str()).unwrap_or(error).to_string()
}

async fn attempt(provider: &dyn Provider, req: Request, variant: &str) -> (ProbeResult, bool) {
    let mut result = ProbeResult {
        model: req.model.clone(),
        provider: provider.id().into(),
        level: req.reasoning.expect("probe level"),
        accepted: false,
        status: None,
        variant: variant.into(),
        error: None,
        supported_values: vec![],
    };
    let mut stream = provider.stream(req, CancellationToken::new());
    let mut error = "provider stream ended without done/error".to_string();
    while let Some(event) = stream.next().await {
        match event {
            AssistantEvent::Start => result.status = Some(200),
            AssistantEvent::Done { message } if !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) && message.error_message.is_none() => {
                result.accepted = true;
                result.status = Some(200);
                return (result, false);
            }
            AssistantEvent::Error { message } | AssistantEvent::Done { message } => {
                error = message.error_message.unwrap_or_else(|| "provider request failed".into());
                break;
            }
            _ => {}
        }
    }
    result.status = crate::hub::classify_llm_error(&error).1.or(result.status);
    let error = agent_ai::openai::redact(&error_message(&error));
    result.supported_values = agent_ai::openai::supported_values(&error);
    let thinking_rejected = result.status == Some(400) && error.to_ascii_lowercase().contains("thinking");
    result.error = Some(error.chars().take(300).collect());
    (result, thinking_rejected)
}

async fn probe_level(provider: &dyn Provider, model: &str, api: Api, level: ThinkingLevel, thinking: Option<Thinking>) -> ProbeResult {
    let anthropic = provider.id() == "anthropic";
    let variant = thinking.unwrap_or(Thinking::Adaptive);
    let mut req = Request {
        model: model.into(),
        system_prompt: None,
        messages: vec![Message::user_text("Reply with OK")],
        tools: vec![],
        reasoning: Some(level),
        raw_effort: true,
        thinking: Some(variant.request()),
        api,
        max_tokens: Some(if anthropic {
            2048
        } else if level == ThinkingLevel::Off {
            256
        } else {
            1024
        }),
        llm_max_retries: Some(0),
    };
    let (result, thinking_rejected) = attempt(provider, req.clone(), if anthropic { variant.variant() } else { api.as_str() }).await;
    if anthropic && thinking.is_none() && thinking_rejected {
        req.thinking = Some(ProbeThinking::Omit);
        attempt(provider, req, "omit").await.0
    } else {
        result
    }
}

pub async fn run(cli: &crate::Cli, cfg: &crate::config::Config, args: &Probe) -> anyhow::Result<()> {
    let model = cli.model.as_deref().ok_or_else(|| anyhow::anyhow!("probe-efforts requires --model <id>"))?;
    let id = crate::hub::select_provider(cli.provider.as_deref(), None, model, cfg.default_provider.as_deref().unwrap_or("openai"));
    anyhow::ensure!(matches!(id, "openai" | "anthropic"), "probe-efforts --provider must be openai or anthropic");
    anyhow::ensure!(cli.api != Some(Api::Auto), "probe-efforts --api must be chat or responses");
    let api = cli.api.unwrap_or_else(|| agent_ai::default_api(model));
    let settings = cfg.providers.get(id);
    let cli_file = if id == "anthropic" { cli.anthropic_key_file.as_deref() } else { cli.openai_key_file.as_deref() };
    let config_file = settings.and_then(|p| p.api_key_file.as_deref()).or_else(|| if id == "openai" { cfg.api_key_file.as_deref() } else { None });
    let (key, _) = agent_ai::secrets::load_provider_key(agent_ai::secrets::provider_key(id).expect("known provider"), cli_file, config_file, |k| std::env::var(k).ok())
        .map_err(|_| anyhow::anyhow!("cannot load {id} API key (check key-file/configuration)"))?;
    let base = std::env::var(format!("{}_BASE_URL", id.to_ascii_uppercase())).ok().filter(|s| !s.trim().is_empty()).or_else(|| settings.and_then(|p| p.base_url.clone()));
    let timeout = Duration::from_secs(cfg.stream_idle_timeout_secs);
    let provider: Box<dyn Provider> = match id {
        "anthropic" => Box::new(agent_ai::anthropic::AnthropicProvider::with_stream_idle_timeout(key, base, timeout)),
        _ => Box::new(agent_ai::openai::OpenAiProvider::with_stream_idle_timeout(key, base, timeout)),
    };
    let mut results = vec![];
    for level in &args.levels {
        let result = probe_level(provider.as_ref(), model, api, *level, args.thinking).await;
        if !args.json {
            println!("{}", result.line());
        }
        results.push(result);
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn faux_result_and_json() {
        let result = probe_level(&agent_ai::faux::FauxProvider, "test", Api::Chat, ThinkingLevel::Max, None).await;
        assert!(result.accepted);
        assert!(result.line().contains("max: accepted status=200 variant=chat error=null supportedValues=[]"));
        let json = serde_json::to_value(result).unwrap();
        assert_eq!(json, serde_json::json!({"model":"test", "provider":"faux", "level":"max", "accepted":true, "status":200, "variant":"chat", "error":null, "supportedValues":[]}));
    }

    struct Rejected;
    impl Provider for Rejected {
        fn id(&self) -> &str {
            "openai"
        }
        fn stream(&self, req: Request, _: CancellationToken) -> agent_ai::EventStream {
            assert!(req.raw_effort && req.tools.is_empty());
            assert_eq!(req.reasoning, Some(ThinkingLevel::Other("ultra")));
            assert_eq!(req.max_tokens, Some(1024));
            let error = format!("OpenAI HTTP 400: {{\"error\":{{\"message\":\"sk-secret Supported values are: 'max', 'ultra', 'unknown'. {}\"}}}}", "长".repeat(400));
            Box::pin(futures::stream::iter([AssistantEvent::Error { message: agent_ai::AssistantMessage::failed(&req.model, StopReason::Error, error) }]))
        }
    }

    #[tokio::test]
    async fn rejected_result_redacts_truncates_and_preserves_order() {
        let result = probe_level(&Rejected, "test", Api::Responses, "ultra".parse().unwrap(), None).await;
        assert!(!result.accepted);
        assert_eq!(result.status, Some(400));
        assert_eq!(result.error.as_ref().unwrap().chars().count(), 300);
        assert!(!result.line().contains("sk-secret"));
        assert_eq!(result.supported_values, [ThinkingLevel::Max, ThinkingLevel::Other("ultra"), ThinkingLevel::Other("unknown")]);
    }
}
