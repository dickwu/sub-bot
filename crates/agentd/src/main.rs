//! sub-bot：仿 pi coding-agent 的本地编码 agent 服务，不绑定具体项目，供其它 bot 调用。
//! - `serve`：HTTP + SSE，每个 bot 一个 Bearer token，token -> 允许的 profiles（默认只监听 127.0.0.1）
//! - `rpc`：stdio JSONL RPC（对应 pi `--mode rpc`，见 coding-agent/src/modes/rpc/rpc-mode.ts）
//! - `run`：一次性执行一个 prompt 并打印结果
//! - `models`：实时列出 provider 支持的模型
//! - `hash-token`：生成配置文件里用的 token 哈希
//!
//! OpenAI key 只从环境变量读入进程内存，任何接口都不会回传。

#[cfg(not(unix))]
compile_error!("sub-bot only supports macOS and Linux");

mod config;
mod http;
mod hub;
mod logging;
mod prompt;
mod rpc;
mod webhook;

use agent_ai::{Message, ThinkingLevel};
use agent_core::AgentEvent;
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(name = "sub-bot", version, about = "General-purpose local coding-agent service for other bots")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// JSON 配置（profiles + clients）；不填则使用以 --workspace 为根的 `default` profile
    #[arg(long, global = true, env = "SUB_BOT_CONFIG")]
    config: Option<PathBuf>,
    /// 模型。serve：覆盖全局默认模型（profile 自己的 model 仍优先）；rpc/run：本会话使用的模型（受 allowedModels 限制）
    #[arg(long, global = true, env = "SUB_BOT_MODEL")]
    model: Option<String>,
    /// 推理强度 none|minimal|low|medium|high|xhigh。serve：全局默认；rpc/run：本会话。模型不支持时忽略并记 warn
    #[arg(long, global = true, env = "SUB_BOT_REASONING_EFFORT")]
    reasoning_effort: Option<ThinkingLevel>,
    /// OpenAI API：auto（默认：gpt-5.4+/gpt-6.x/o 系列走 Responses，其余走 Chat Completions）| chat | responses。serve：全局默认；rpc/run：本会话
    #[arg(long, global = true, env = "SUB_BOT_API")]
    api: Option<agent_ai::Api>,
    /// openai | faux（faux 不联网，用于测试）
    #[arg(long, global = true, default_value = "openai", env = "SUB_BOT_PROVIDER")]
    provider: String,
    /// 日志级别（RUST_LOG 优先），如 info、debug、agentd=debug,agent_ai=trace
    #[arg(long, global = true, default_value = "info", env = "SUB_BOT_LOG")]
    log_level: String,
    /// 滚动日志目录（按天切分，保留 14 天）；默认是系统数据目录下的 sub-bot/logs；传 `-` 关闭文件日志
    #[arg(long, global = true, env = "SUB_BOT_LOG_DIR")]
    log_dir: Option<PathBuf>,
    /// 文件日志格式：json | text
    #[arg(long, global = true, default_value = "json", env = "SUB_BOT_LOG_FORMAT")]
    log_format: String,
    /// 无配置文件时 default profile 的 workspaceRoot
    #[arg(long, global = true, default_value = ".")]
    workspace: PathBuf,
    /// 会话 JSONL 存放目录（按 client 分子目录）；不填则不持久化
    #[arg(long, global = true, env = "SUB_BOT_SESSION_DIR")]
    session_dir: Option<PathBuf>,
    /// 从文件读取 OpenAI API key（推荐：key 不进入进程环境）。优先级：本参数 > OPENAI_API_KEY_FILE > 配置 apiKeyFile > OPENAI_API_KEY
    #[arg(long, global = true)]
    openai_key_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动 HTTP 服务
    Serve {
        #[arg(long, default_value = "127.0.0.1:7878")]
        bind: SocketAddr,
        /// 仅在没有配置文件时使用：单一 client 的 token；不填则随机生成并打印到 stderr
        #[arg(long, env = "SUB_BOT_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// stdio JSONL RPC 模式（父进程即调用方，无需 token）
    Rpc {
        #[arg(long)]
        profile: Option<String>,
    },
    /// 一次性执行一个 prompt，打印最终回复后退出
    Run {
        #[arg(long)]
        profile: Option<String>,
        /// 输出 JSON（text、model、api、reasoningEffort（实际生效值）、requestedReasoningEffort、usage（最后一次调用）、totalUsage（整次运行）、turns、toolCalls、stopReason、error）
        #[arg(long)]
        json: bool,
        prompt: String,
    },
    /// 实时列出 provider 支持的模型（默认只列可对话的模型，不含日期快照）
    Models {
        /// 列出全部（含 embedding、语音、图片、快照等）
        #[arg(long)]
        all: bool,
        /// 只列该 profile 的 allowedModels 允许的模型
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 打印 token 的 SHA-256，用于填写配置文件 clients[].tokenSha256
    HashToken { token: String },
}

fn local_client(cfg: &config::Config) -> config::Client {
    config::Client { name: "local".into(), token_sha256: String::new(), profiles: cfg.profiles.keys().cloned().collect(), ..Default::default() }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Cmd::HashToken { ref token } = cli.cmd {
        println!("{}", config::sha256_hex(token));
        return Ok(());
    }
    let _log_guard = logging::init(&cli.log_level, cli.log_dir.as_deref(), &cli.log_format)?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = std::env::consts::OS, arch = std::env::consts::ARCH, provider = %cli.provider, "sub-bot starting");
    let load = |fallback_token: &str| -> anyhow::Result<config::Config> {
        match &cli.config {
            Some(p) => config::Config::load(p),
            None => Ok(config::Config::fallback(cli.model.clone().unwrap_or_else(|| "gpt-4.1-mini".into()), fallback_token, cli.workspace.clone())),
        }
    };
    let session_req = |profile: Option<String>| hub::SessionRequest { profile, model: cli.model.clone(), reasoning_effort: cli.reasoning_effort, api: cli.api, ..Default::default() };
    match cli.cmd {
        Cmd::HashToken { .. } => unreachable!(),
        Cmd::Serve { bind, ref token } => {
            let token = token.clone().unwrap_or_else(http::random_token);
            let mut cfg = load(&token)?;
            if let Some(m) = &cli.model {
                cfg.default_model = m.clone();
            }
            if cli.reasoning_effort.is_some() {
                cfg.default_reasoning_effort = cli.reasoning_effort;
            }
            if let Some(api) = cli.api {
                cfg.default_api = api;
            }
            if cli.config.is_none() {
                eprintln!("no --config: single client 'default', Authorization: Bearer {token}");
            }
            let hub = hub::Hub::new(cfg, &cli.provider, cli.session_dir.clone(), cli.openai_key_file.as_deref())?;
            eprintln!("sub-bot listening on http://{bind}");
            http::serve(hub, bind).await?;
        }
        Cmd::Rpc { ref profile } => {
            let hub = hub::Hub::new(load("")?, &cli.provider, cli.session_dir.clone(), cli.openai_key_file.as_deref())?;
            rpc::run(hub, session_req(profile.clone())).await?;
        }
        Cmd::Run { ref profile, json, ref prompt } => {
            let hub = hub::Hub::new(load("")?, &cli.provider, cli.session_dir.clone(), cli.openai_key_file.as_deref())?;
            let session = hub.create_session(&local_client(&hub.config), session_req(profile.clone()))?;
            let mut events = session.subscribe();
            session.submit(prompt.clone(), None);
            let mut last = None;
            let (mut total, mut turns, mut tool_calls) = (agent_ai::Usage::default(), 0usize, 0usize);
            let mut used: Option<(agent_ai::Api, Option<ThinkingLevel>)> = None;
            loop {
                match events.recv().await {
                    Ok(AgentEvent::MessageEnd { message: Message::Assistant(a) }) => {
                        turns += 1;
                        tool_calls += a.tool_calls().len();
                        total.input += a.usage.input;
                        total.output += a.usage.output;
                        total.cache_read += a.usage.cache_read;
                        total.reasoning += a.usage.reasoning;
                        total.total_tokens += a.usage.total_tokens;
                        if let Some(api) = a.api {
                            used = Some((api, a.reasoning_effort));
                        }
                        last = Some(a)
                    }
                    Ok(AgentEvent::ToolExecutionStart { tool_name, .. }) if !json => {
                        eprintln!("[tool] {tool_name}")
                    }
                    Ok(AgentEvent::AgentSettled { .. }) | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
            let last = last.ok_or_else(|| anyhow::anyhow!("no assistant reply"))?;
            if json {
                // reasoningEffort / api 是实际发送给模型的值（运行时降级后），requestedReasoningEffort 是请求值
                let (api, effort) = match used {
                    Some((a, e)) => (a.as_str().to_string(), e),
                    None => (hub.effective_api(&session.agent.config).to_string(), session.agent.config.reasoning),
                };
                println!(
                    "{}",
                    serde_json::json!({"sessionId": session.id, "model": session.agent.config.model, "api": api, "reasoningEffort": effort, "requestedReasoningEffort": session.requested_effort, "text": last.text(), "stopReason": last.stop_reason, "turns": turns, "toolCalls": tool_calls, "usage": last.usage, "totalUsage": total, "error": last.error_message})
                );
            } else {
                println!("{}", last.text());
            }
            if let Some(e) = last.error_message {
                anyhow::bail!(e);
            }
        }
        Cmd::Models { all, ref profile, json } => {
            let hub = hub::Hub::new(load("")?, &cli.provider, cli.session_dir.clone(), cli.openai_key_file.as_deref())?;
            let mut list = hub.list_models(all).await?;
            if let Some(name) = profile {
                let p = hub.config.profiles.get(name).ok_or_else(|| anyhow::anyhow!("unknown profile '{name}'"))?;
                list.retain(|m| config::Config::model_allowed(p, &m.id));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for m in &list {
                    let efforts = match (m.reasoning, m.reasoning_efforts.is_empty()) {
                        (Some(false), _) => "-".to_string(),
                        (_, false) => m.reasoning_efforts.iter().map(|l| l.as_str()).collect::<Vec<_>>().join(","),
                        _ => "?".to_string(),
                    };
                    println!("{:<32} {}", m.id, efforts);
                }
                eprintln!("{} models (reasoning efforts: '-' = unsupported, '?' = unknown, passed through)", list.len());
            }
        }
    }
    Ok(())
}
