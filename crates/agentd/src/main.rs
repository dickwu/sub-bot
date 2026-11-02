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

mod ask;
mod config;
mod http;
mod hub;
pub mod issues;
mod local;
#[cfg(test)]
mod local_tests;
mod logging;
mod prompt;
mod rpc;
mod server;
mod store;
pub mod webhook;

use agent_ai::{Message, ThinkingLevel};
use agent_core::AgentEvent;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
    /// 数据目录；默认系统数据目录/sub-bot
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
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
        bind: String,
        /// 同时监听 Unix socket；不带路径时使用 <dataDir>/sub-bot.sock
        #[cfg(unix)]
        #[arg(long, num_args = 0..=1)]
        unix_socket: Option<Option<PathBuf>>,
        /// 仅在没有配置文件时使用：单一 client 的 token；不填则随机生成（不打印）
        #[arg(long, env = "SUB_BOT_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// 向已运行的服务提问（默认优先使用本地 Unix socket）
    Ask(ask::Ask),
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
    /// 问题上报诊断
    Issues {
        #[command(subcommand)]
        command: IssuesCmd,
    },
    /// 打印 token 的 SHA-256，用于填写配置文件 clients[].tokenSha256
    HashToken { token: String },
}

#[derive(Subcommand)]
enum IssuesCmd {
    Test {
        #[arg(long, value_enum, default_value = "error")]
        severity: issues::Severity,
        #[arg(long, default_value = "sub-bot issues test")]
        message: String,
    },
    Tail {
        #[arg(short = 'n', default_value_t = 20)]
        n: usize,
    },
}

fn issue_log_dir(cli: &Cli) -> Option<PathBuf> {
    match &cli.log_dir {
        Some(p) if p == std::path::Path::new("-") => None,
        Some(p) => Some(p.clone()),
        None => logging::default_log_dir(),
    }
}

fn install_reporter(cfg: &config::Config, cli: &Cli) {
    let secrets = cfg.clients.iter().flat_map(|c| c.webhook_secret.iter().cloned().chain(c.resolved_webhooks.iter().filter_map(|t| t.target.secret.clone()))).collect();
    issues::install(issues::Reporter::new(&cfg.alerts, issue_log_dir(cli), secrets));
}

fn startup_failure(error: Option<&anyhow::Error>) -> (String, issues::IssueEvent) {
    let mut event = issues::IssueEvent::new(issues::Severity::Fatal, "startup_failed", "config", "configuration or service initialization failed");
    // Only this typed error is safe to expose; surrounding context may contain credentials.
    if let Some(locked) = error.and_then(|e| e.downcast_ref::<store::DataDirLocked>()) {
        let message = locked.to_string();
        event.message = message.clone();
        event = event.details(serde_json::json!({"reason": "data_dir_locked", "dataDir": locked.path.to_string_lossy()}));
        (message, event)
    } else {
        ("service initialization failed (check provider and key configuration)".into(), event)
    }
}

async fn startup_failed(cli: &Cli, event: issues::IssueEvent) {
    let reporter = match issues::global() {
        Some(r) => r,
        None => {
            // Parse alerts independently: a bad profile/client must not hide startup failures.
            let mut alerts = cli
                .config
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("alerts").cloned())
                .and_then(|v| serde_json::from_value::<config::AlertsConfig>(v).ok())
                .unwrap_or_default();
            if alerts.resolve(cli.config.as_deref().and_then(|p| p.parent())).is_err() {
                alerts.resolved.clear();
            }
            issues::Reporter::new(&alerts, issue_log_dir(cli), vec![])
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        reporter.test_alerts(event).await;
        reporter.flush(std::time::Duration::from_secs(1)).await;
    })
    .await;
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
    if let Cmd::Ask(ref args) = cli.cmd {
        // ask only needs stderr diagnostics; never create a log directory or store.
        logging::init(&cli.log_level, Some(std::path::Path::new("-")), &cli.log_format)?;
        // Read only dataDir: ask must not load provider/webhook secret files or initialize a Hub.
        let configured_dir = if cli.data_dir.is_none() {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Location {
                data_dir: Option<PathBuf>,
            }
            cli.config
                .as_ref()
                .map(|p| -> anyhow::Result<Option<PathBuf>> {
                    let text = std::fs::read_to_string(p).map_err(|_| anyhow::anyhow!("cannot read configuration"))?;
                    Ok(serde_json::from_str::<Location>(&text).map_err(|_| anyhow::anyhow!("invalid configuration"))?.data_dir)
                })
                .transpose()?
                .flatten()
        } else {
            None
        };
        return ask::run(args, cli.data_dir.clone().or(configured_dir).unwrap_or_else(config::Config::default_data_dir)).await;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info.payload().downcast_ref::<String>().map(String::as_str).or_else(|| info.payload().downcast_ref::<&str>().copied()).unwrap_or("panic");
        issues::report(
            issues::IssueEvent::new(issues::Severity::Fatal, "panic", "runtime", message)
                .details(serde_json::json!({"location": info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))})),
        );
        previous(info);
    }));
    let _log_guard = logging::init(&cli.log_level, cli.log_dir.as_deref(), &cli.log_format)?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = std::env::consts::OS, arch = std::env::consts::ARCH, provider = %cli.provider, "sub-bot starting");
    let load = |fallback_token: &str| -> anyhow::Result<config::Config> {
        match &cli.config {
            Some(p) => config::Config::load(p),
            None => Ok(config::Config::fallback(cli.model.clone().unwrap_or_else(|| "gpt-4.1-mini".into()), fallback_token, cli.workspace.clone())),
        }
    };
    let fallback_token = match &cli.cmd {
        Cmd::Serve { token, .. } => token.clone().unwrap_or_else(http::random_token),
        _ => String::new(),
    };
    let mut loaded = match load(&fallback_token) {
        Ok(cfg) => cfg,
        Err(_) => {
            startup_failed(&cli, startup_failure(None).1).await;
            anyhow::bail!("configuration load failed (check config and secret files)");
        }
    };
    if let Some(dir) = &cli.data_dir {
        loaded.data_dir = Some(dir.clone());
    }
    #[cfg(unix)]
    if let Cmd::Serve { unix_socket: Some(path), .. } = &cli.cmd {
        loaded.unix_socket = Some(path.clone().unwrap_or_else(|| loaded.data_dir().join("sub-bot.sock")));
    }
    install_reporter(&loaded, &cli);
    let new_hub = |cfg: config::Config| {
        if matches!(cli.cmd, Cmd::Serve { .. }) {
            hub::Hub::with_reporter(cfg, &cli.provider, cli.session_dir.clone(), cli.openai_key_file.as_deref(), issues::global().expect("installed reporter"), hub::StoreMode::Persistent)
        } else {
            hub::Hub::new(cfg, &cli.provider, cli.session_dir.clone(), cli.openai_key_file.as_deref())
        }
    };
    // Construct before dispatch so initialization failures get the short synchronous alert path.
    let prepared_hub = if matches!(cli.cmd, Cmd::Issues { .. }) {
        None
    } else {
        let mut cfg = loaded.clone();
        if matches!(cli.cmd, Cmd::Serve { .. }) {
            if let Some(m) = &cli.model {
                cfg.default_model = m.clone();
            }
            if cli.reasoning_effort.is_some() {
                cfg.default_reasoning_effort = cli.reasoning_effort;
            }
            if let Some(api) = cli.api {
                cfg.default_api = api;
            }
        }
        match new_hub(cfg) {
            Ok(hub) => Some(hub),
            Err(error) => {
                let (message, event) = startup_failure(Some(&error));
                startup_failed(&cli, event).await;
                anyhow::bail!(message);
            }
        }
    };
    let session_req = |profile: Option<String>| hub::SessionRequest { profile, model: cli.model.clone(), reasoning_effort: cli.reasoning_effort, api: cli.api, ..Default::default() };
    match cli.cmd {
        Cmd::Issues { ref command } => match command {
            IssuesCmd::Test { severity, message } => {
                let reporter = issues::global().expect("installed reporter");
                let results = reporter.test_alerts(issues::IssueEvent::new(*severity, "test", "runtime", message.clone())).await;
                let mut failed = false;
                for (host, status) in results {
                    println!("{host}: {:?}, attempts={}", status.state, status.attempts);
                    failed |= status.state != webhook::DeliveryState::Delivered;
                }
                reporter.flush(std::time::Duration::from_secs(5)).await;
                anyhow::ensure!(!failed, "one or more alert deliveries failed");
            }
            IssuesCmd::Tail { n } => {
                let path = issue_log_dir(&cli).ok_or_else(|| anyhow::anyhow!("file logging disabled"))?.join("issues.log");
                let text = std::fs::read_to_string(path)?;
                let lines: Vec<_> = text.lines().collect();
                for line in &lines[lines.len().saturating_sub(*n)..] {
                    println!("{line}");
                }
            }
        },
        Cmd::HashToken { .. } | Cmd::Ask(_) => unreachable!(),
        Cmd::Serve { bind, .. } => {
            let bind = if bind == "none" { None } else { Some(bind.parse().map_err(|_| anyhow::anyhow!("--bind must be a socket address or 'none'"))?) };
            if cli.config.is_none() {
                eprintln!("no --config: single client 'default'; same-user Unix socket access enabled (use --unix-socket); TCP requires a token");
            }
            let hub = prepared_hub.clone().expect("prepared hub");
            http::serve(hub, bind).await?;
        }
        Cmd::Rpc { ref profile } => {
            let hub = prepared_hub.clone().expect("prepared hub");
            rpc::run(hub, session_req(profile.clone())).await?;
        }
        Cmd::Run { ref profile, json, ref prompt } => {
            let hub = prepared_hub.clone().expect("prepared hub");
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
            let hub = prepared_hub.clone().expect("prepared hub");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_failure_exposes_only_typed_lock_error_through_context() {
        let path = PathBuf::from("/tmp/sub-bot-test-data");
        let error = anyhow::Error::new(store::DataDirLocked { path: path.clone() }).context("sensitive provider context").context("sensitive initialization context");
        let (message, event) = startup_failure(Some(&error));
        assert_eq!(message, format!("dataDir {} is in use by another sub-bot server", path.display()));
        assert_eq!(event.message, message);
        assert_eq!(event.kind, "startup_failed");
        assert_eq!(event.severity, issues::Severity::Fatal);
        assert_eq!(event.details, serde_json::json!({"reason": "data_dir_locked", "dataDir": path}));
    }

    #[test]
    fn startup_failure_keeps_untyped_errors_generic() {
        // Even a lookalike message must not bypass the typed allowlist.
        let error = anyhow::anyhow!("dataDir sensitive-value is in use by another sub-bot server").context("sensitive provider context");
        for error in [None, Some(&error)] {
            let (message, event) = startup_failure(error);
            assert_eq!(message, "service initialization failed (check provider and key configuration)");
            assert_eq!(event.message, "configuration or service initialization failed");
            assert_eq!(event.kind, "startup_failed");
            assert_eq!(event.details, serde_json::json!({}));
        }
    }
}
