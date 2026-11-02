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
mod probe;
mod prompt;
mod routing;
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
    /// 推理强度 none|minimal|low|medium|high|xhigh|max 或 provider 的小写名称；已知模型限制为最高三档（别名 --effort）
    #[arg(long, visible_alias = "effort", global = true, env = "SUB_BOT_REASONING_EFFORT")]
    reasoning_effort: Option<ThinkingLevel>,
    /// OpenAI API：auto（默认：gpt-5.4+/gpt-6.x/o 系列走 Responses，其余走 Chat Completions）| chat | responses。serve：全局默认；rpc/run：本会话
    #[arg(long, global = true, env = "SUB_BOT_API")]
    api: Option<agent_ai::Api>,
    /// openai | anthropic | faux; models 时作为筛选，其它命令作为默认/会话覆盖
    #[arg(long, global = true, env = "SUB_BOT_PROVIDER")]
    provider: Option<String>,
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
    /// Anthropic key: CLI > ANTHROPIC_API_KEY_FILE > providers.anthropic.apiKeyFile > ANTHROPIC_API_KEY
    #[arg(long, global = true)]
    anthropic_key_file: Option<PathBuf>,
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
    ///
    /// 默认同步流式输出助手文本并在结束时换行；--json 逐事件输出 NDJSON。
    /// --async 轮询后输出最终文本，--async --json 输出最终 task JSON。
    /// HTTP/模型错误、取消或超时会向 stderr 报错并返回非零退出码。
    Ask(ask::Ask),
    /// stdio JSONL RPC 模式（父进程即调用方，无需 token）
    Rpc {
        #[arg(long)]
        profile: Option<String>,
        /// 模型路由：standard | deep | auto（显式 --model 时忽略）
        #[arg(long)]
        depth: Option<routing::Depth>,
    },
    /// 一次性执行一个 prompt，打印最终回复后退出
    Run {
        #[arg(long)]
        profile: Option<String>,
        /// 模型路由：standard | deep | auto（显式 --model 时忽略）
        #[arg(long)]
        depth: Option<routing::Depth>,
        /// 输出 JSON（text、model、api、reasoningEffort（实际生效值）、requestedReasoningEffort、effortAdjusted、allowedEfforts、routing、usage（最后一次调用）、totalUsage（整次运行，含路由分类调用）、turns、toolCalls、stopReason、error）
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
    /// 实测各 effort 是否被 API 接受（绕过目录/降级；需要 --model）
    ProbeEfforts(probe::Probe),
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
        return ask::run(args, cli.data_dir.clone().or(configured_dir).unwrap_or_else(config::Config::default_data_dir), cli.provider.as_deref(), cli.reasoning_effort).await;
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
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = std::env::consts::OS, arch = std::env::consts::ARCH, provider = ?cli.provider, "sub-bot starting");
    let load = |fallback_token: &str| -> anyhow::Result<config::Config> {
        match &cli.config {
            Some(p) => config::Config::load(p),
            None => Ok(config::Config::fallback_for(cli.provider.as_deref(), cli.model.clone(), fallback_token, cli.workspace.clone())),
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
    if let Cmd::ProbeEfforts(ref args) = cli.cmd {
        return probe::run(&cli, &loaded, args).await;
    }
    install_reporter(&loaded, &cli);
    let new_hub = |cfg: config::Config| {
        let provider = cli.provider.clone().or_else(|| cfg.default_provider.clone()).unwrap_or_else(|| "openai".into());
        let mode = if matches!(cli.cmd, Cmd::Serve { .. }) { hub::StoreMode::Persistent } else { hub::StoreMode::Memory };
        if cli.anthropic_key_file.is_some() {
            hub::Hub::with_provider_keys(
                cfg,
                &provider,
                cli.session_dir.clone(),
                hub::KeyFiles { openai: cli.openai_key_file.as_deref(), anthropic: cli.anthropic_key_file.as_deref() },
                issues::global().expect("installed reporter"),
                mode,
            )
        } else if matches!(cli.cmd, Cmd::Serve { .. }) {
            hub::Hub::with_reporter(cfg, &provider, cli.session_dir.clone(), cli.openai_key_file.as_deref(), issues::global().expect("installed reporter"), mode)
        } else {
            hub::Hub::new(cfg, &provider, cli.session_dir.clone(), cli.openai_key_file.as_deref())
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
    let session_req = |profile: Option<String>, depth: Option<routing::Depth>| hub::SessionRequest {
        provider: cli.provider.clone(),
        profile,
        model: cli.model.clone(),
        reasoning_effort: cli.reasoning_effort,
        depth,
        api: cli.api,
        ..Default::default()
    };
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
        Cmd::HashToken { .. } | Cmd::Ask(_) | Cmd::ProbeEfforts(_) => unreachable!(),
        Cmd::Serve { bind, .. } => {
            let bind = if bind == "none" { None } else { Some(bind.parse().map_err(|_| anyhow::anyhow!("--bind must be a socket address or 'none'"))?) };
            if cli.config.is_none() {
                eprintln!("no --config: single client 'default'; same-user Unix socket access enabled (use --unix-socket); TCP requires a token");
            }
            let hub = prepared_hub.clone().expect("prepared hub");
            http::serve(hub, bind).await?;
        }
        Cmd::Rpc { ref profile, depth } => {
            let hub = prepared_hub.clone().expect("prepared hub");
            rpc::run(hub, session_req(profile.clone(), depth)).await?;
        }
        Cmd::Run { ref profile, depth, json, ref prompt } => {
            let hub = prepared_hub.clone().expect("prepared hub");
            let session = hub.create_session(&local_client(&hub.config), session_req(profile.clone(), depth))?;
            let mut events = session.subscribe();
            session.submit(prompt.clone(), None);
            let mut last = None;
            let (mut total, mut turns, mut tool_calls) = (agent_ai::Usage::default(), 0usize, 0usize);
            let mut used: Option<(Option<agent_ai::Api>, Option<ThinkingLevel>)> = None;
            loop {
                match events.recv().await {
                    Ok(AgentEvent::MessageEnd { message: Message::Assistant(a) }) => {
                        turns += 1;
                        tool_calls += a.tool_calls().len();
                        add_usage(&mut total, &a.usage);
                        if a.api.is_some() || session.agent.config.provider.id() == "anthropic" {
                            used = Some((a.api, a.reasoning_effort));
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
                    Some((a, e)) => (a.map(|a| a.as_str()).unwrap_or("n/a").to_string(), e),
                    None => (hub.effective_api(&session.agent.config).to_string(), session.agent.config.reasoning.get()),
                };
                if let Some(u) = &session.routing().usage {
                    add_usage(&mut total, u);
                }
                let mut out = serde_json::json!({"sessionId": session.id, "model": session.model(), "api": api, "text": last.text(), "stopReason": last.stop_reason, "turns": turns, "toolCalls": tool_calls, "usage": last.usage, "totalUsage": total, "error": last.error_message});
                out.as_object_mut().unwrap().extend(session.effort_fields());
                out["reasoningEffort"] = serde_json::json!(effort);
                println!("{out}");
            } else {
                println!("{}", last.text());
            }
            if let Some(e) = last.error_message {
                anyhow::bail!(e);
            }
        }
        Cmd::Models { all, ref profile, json } => {
            let hub = prepared_hub.clone().expect("prepared hub");
            let mut list = hub.list_models_filtered(all, cli.provider.as_deref()).await?;
            if let Some(name) = profile {
                let p = hub.config.profiles.get(name).ok_or_else(|| anyhow::anyhow!("unknown profile '{name}'"))?;
                list.retain(|m| config::Config::model_allowed(p, &m.model.id));
            }
            // --profile 过滤后剩下的都被该 profile 允许；否则按所有 profile 的 allowedModels 标记。
            let (allowed, restricted) =
                if profile.is_some() { (vec![true; list.len()], false) } else { allowed_by_profiles(&list, hub.config.profiles.values().map(|p| (hub.profile_provider(p), p))) };
            if json {
                let out: Vec<serde_json::Value> = list
                    .iter()
                    .zip(&allowed)
                    .map(|(m, &ok)| {
                        let mut v = serde_json::to_value(m)?;
                        v["allowed"] = serde_json::json!(ok);
                        Ok(v)
                    })
                    .collect::<serde_json::Result<_>>()?;
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for (m, &ok) in list.iter().zip(&allowed) {
                    let efforts = format_efforts(&m.model);
                    let mark = if !restricted {
                        ""
                    } else if ok {
                        "* "
                    } else {
                        "  "
                    };
                    println!("{mark}{:<12} {:<32} {} default={}", m.provider, m.model.id, efforts, m.model.default_effort.map(|l| l.as_str()).unwrap_or("?"));
                }
                let marker = if restricted { ", * = allowed by profiles" } else { "" };
                eprintln!("{} models (reasoning efforts: [...] = allowed top-three, '-' = unsupported, '?' = unknown, passed through{marker})", list.len());
            }
        }
    }
    Ok(())
}

fn add_usage(total: &mut agent_ai::Usage, u: &agent_ai::Usage) {
    total.input += u.input;
    total.output += u.output;
    total.cache_read += u.cache_read;
    total.cache_write += u.cache_write;
    total.reasoning += u.reasoning;
    total.total_tokens += u.total_tokens;
}

/// 每个模型是否被 profile 的 allowedModels 允许：只看与模型同 provider 且设置了 allowedModels 的 profile，任一允许即 true；
/// 该 provider 没有任何 profile 限制时为 true。第二个返回值表示列表涉及的 provider 是否存在 allowedModels 限制（决定是否显示 `*` 标记）。
fn allowed_by_profiles<'a>(list: &[hub::ProviderModel], profiles: impl IntoIterator<Item = (&'a str, &'a config::Profile)>) -> (Vec<bool>, bool) {
    let restricting: Vec<(&str, &config::Profile)> = profiles.into_iter().filter(|(_, p)| p.allowed_models.is_some()).collect();
    let mut restricted = false;
    let allowed = list
        .iter()
        .map(|m| {
            let mut relevant = restricting.iter().filter(|(provider, _)| *provider == m.provider).peekable();
            if relevant.peek().is_none() {
                return true;
            }
            restricted = true;
            relevant.any(|(_, p)| config::Config::model_allowed(p, &m.model.id))
        })
        .collect();
    (allowed, restricted)
}

fn format_efforts(model: &agent_ai::ModelInfo) -> String {
    if model.reasoning == Some(false) {
        return "-".into();
    }
    if model.reasoning_efforts.is_empty() {
        return "?".into();
    }
    let mut parts = vec![];
    for level in &model.reasoning_efforts {
        let mut name = level.to_string();
        if model.allowed_efforts.first() == Some(level) {
            name.insert(0, '[');
        }
        if model.allowed_efforts.last() == Some(level) {
            name.push(']');
        }
        parts.push(name);
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_and_extensible_effort_cli() {
        for name in ["max", "ultra", "future_1-level"] {
            let cli = Cli::try_parse_from(["sub-bot", "run", "--reasoning-effort", name, "hi"]).unwrap();
            assert_eq!(cli.reasoning_effort.unwrap().as_str(), name);
        }
        let cli = Cli::try_parse_from([
            "sub-bot",
            "probe-efforts",
            "--model",
            "claude-sonnet-5-5",
            "--provider",
            "anthropic",
            "--levels",
            "max,ultra",
            "--thinking",
            "omit",
            "--api",
            "responses",
            "--json",
            "--anthropic-key-file",
            "claude.key",
        ])
        .unwrap();
        let Cmd::ProbeEfforts(args) = cli.cmd else { panic!("probe command") };
        assert!(args.json && matches!(args.thinking, Some(probe::Thinking::Omit)));
        assert_eq!(args.levels, [ThinkingLevel::Max, ThinkingLevel::Other("ultra")]);
        assert_eq!(cli.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(cli.api, Some(agent_ai::Api::Responses));
        let cli = Cli::try_parse_from(["sub-bot", "probe-efforts", "--model", "gpt-6-astra"]).unwrap();
        let Cmd::ProbeEfforts(args) = cli.cmd else { panic!("probe command") };
        assert_eq!(args.levels.len(), 8);
        assert!(Cli::try_parse_from(["sub-bot", "probe-efforts", "--levels", "INVALID"]).is_err());
        assert!(Cli::try_parse_from(["sub-bot", "probe-efforts", "--thinking", "invalid"]).is_err());
    }

    #[test]
    fn effort_alias_and_depth_flags() {
        for flag in ["--effort", "--reasoning-effort"] {
            let cli = Cli::try_parse_from(["sub-bot", "ask", flag, "high", "--deep", "hi"]).unwrap();
            assert_eq!(cli.reasoning_effort, Some(ThinkingLevel::High));
            let Cmd::Ask(ask) = cli.cmd else { panic!("ask") };
            assert!(ask.deep && ask.depth.is_none());
        }
        let Cmd::Ask(ask) = Cli::try_parse_from(["sub-bot", "ask", "--depth", "auto", "hi"]).unwrap().cmd else { panic!("ask") };
        assert_eq!(ask.depth, Some(routing::Depth::Auto));
        assert!(Cli::try_parse_from(["sub-bot", "ask", "--depth", "auto", "--deep", "hi"]).is_err());
        assert!(Cli::try_parse_from(["sub-bot", "ask", "--depth", "huge", "hi"]).is_err());
        let Cmd::Rpc { depth, .. } = Cli::try_parse_from(["sub-bot", "rpc", "--depth", "deep"]).unwrap().cmd else { panic!("rpc") };
        assert_eq!(depth, Some(routing::Depth::Deep));
    }

    #[test]
    fn models_mark_allowed_top_three() {
        assert_eq!(format_efforts(&agent_ai::models::info_for("gpt-6-astra".into(), None, None)), "low,medium,[high,xhigh,max]");
        let model = agent_ai::models::info_for("claude-sonnet-4-6".into(), None, None);
        assert_eq!(format_efforts(&model), "low,[medium,high,max]");
        assert_eq!(model.default_effort, Some(ThinkingLevel::High));
    }

    #[test]
    fn models_allowed_by_profiles_marks_union_per_provider() {
        let model = |provider: &str, id: &str| hub::ProviderModel { provider: provider.into(), model: agent_ai::models::info_for(id.into(), None, None) };
        let list = vec![model("openai", "gpt-6.1-sol"), model("openai", "gpt-6-astra"), model("openai", "gpt-5.5"), model("anthropic", "claude-opus-5-5")];
        let profile = |allowed: Option<&[&str]>| {
            let mut p = config::Config::fallback("faux".into(), "t", ".".into()).profiles.remove("default").unwrap();
            p.allowed_models = allowed.map(|l| l.iter().map(|s| s.to_string()).collect());
            p
        };
        // 没有任何 allowedModels：全部 allowed，不显示标记。
        let open = profile(None);
        assert_eq!(allowed_by_profiles(&list, [("openai", &open)]), (vec![true; 4], false));
        assert_eq!(allowed_by_profiles(&list, []), (vec![true; 4], false));
        // openai profile 的并集；anthropic 不受 openai profile 限制。
        let sol = profile(Some(&["gpt-6.1-sol"]));
        let astra = profile(Some(&["gpt-6-astra"]));
        assert_eq!(allowed_by_profiles(&list, [("openai", &sol), ("openai", &astra), ("openai", &open)]), (vec![true, true, false, true], true));
        let defaults = profile(Some(config::DEFAULT_OPENAI_ALLOWED_MODELS));
        assert_eq!(allowed_by_profiles(&list, [("openai", &defaults)]).0, [true, true, false, true]);
        // 只有 anthropic profile 限制时 openai 模型不受影响。
        let claude = profile(Some(&["claude-sonnet*"]));
        assert_eq!(allowed_by_profiles(&list, [("anthropic", &claude)]), (vec![true, true, true, false], true));
    }

    #[test]
    fn provider_cli_options_are_optional_and_global() {
        let cli = Cli::try_parse_from(["sub-bot", "models"]).unwrap();
        assert!(cli.provider.is_none());
        for args in [
            vec!["sub-bot", "models", "--provider", "anthropic", "--anthropic-key-file", "claude.key"],
            vec!["sub-bot", "--provider", "anthropic", "--anthropic-key-file", "claude.key", "ask", "hello"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(cli.provider.as_deref(), Some("anthropic"));
            assert_eq!(cli.anthropic_key_file.as_deref(), Some(std::path::Path::new("claude.key")));
        }
    }

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
