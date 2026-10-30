//! pi-agentd：仿 pi coding-agent 的本地编码 agent 服务，不绑定具体项目，供其它 bot 调用。
//! - `serve`：HTTP + SSE，每个 bot 一个 Bearer token，token -> 允许的 profiles（默认只监听 127.0.0.1）
//! - `rpc`：stdio JSONL RPC（对应 pi `--mode rpc`，见 coding-agent/src/modes/rpc/rpc-mode.ts）
//! - `hash-token`：生成配置文件里用的 token 哈希
//! OpenAI key 只从环境变量读入进程内存，任何接口都不会回传。
mod config;
mod http;
mod hub;
mod logging;
mod prompt;
mod rpc;

use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(name = "pi-agentd", version, about = "General-purpose local agent service for other bots")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// JSON 配置（profiles + clients）；不填则使用以 --workspace 为根的 `default` profile
    #[arg(long, global = true, env = "PI_AGENTD_CONFIG")]
    config: Option<PathBuf>,
    /// 无配置文件时的默认模型
    #[arg(long, global = true, default_value = "gpt-4.1-mini", env = "PI_AGENTD_MODEL")]
    model: String,
    /// openai | faux（faux 不联网，用于测试）
    #[arg(long, global = true, default_value = "openai", env = "PI_AGENTD_PROVIDER")]
    provider: String,
    /// 日志级别（RUST_LOG 优先），如 info、debug、agentd=debug,agent_ai=trace
    #[arg(long, global = true, default_value = "info", env = "PI_AGENTD_LOG")]
    log_level: String,
    /// 滚动日志目录（按天切分，保留 14 天）；默认是系统数据目录下的 pi-agentd/logs；传 `-` 关闭文件日志
    #[arg(long, global = true, env = "PI_AGENTD_LOG_DIR")]
    log_dir: Option<PathBuf>,
    /// 文件日志格式：json | text
    #[arg(long, global = true, default_value = "json", env = "PI_AGENTD_LOG_FORMAT")]
    log_format: String,
    /// 无配置文件时 default profile 的 workspaceRoot
    #[arg(long, global = true, default_value = ".")]
    workspace: PathBuf,
    /// 会话 JSONL 存放目录（按 client 分子目录）；不填则不持久化
    #[arg(long, global = true, env = "PI_AGENTD_SESSION_DIR")]
    session_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动 HTTP 服务
    Serve {
        #[arg(long, default_value = "127.0.0.1:7878")]
        bind: SocketAddr,
        /// 仅在没有配置文件时使用：单一 client 的 token；不填则随机生成并打印到 stderr
        #[arg(long, env = "PI_AGENTD_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// stdio JSONL RPC 模式（父进程即调用方，无需 token）
    Rpc {
        #[arg(long)]
        profile: Option<String>,
    },
    /// 打印 token 的 SHA-256，用于填写配置文件 clients[].tokenSha256
    HashToken { token: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _log_guard = logging::init(&cli.log_level, cli.log_dir.as_deref(), &cli.log_format)?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = std::env::consts::OS, arch = std::env::consts::ARCH, provider = %cli.provider, "pi-agentd starting");
    let load = |fallback_token: &str| -> anyhow::Result<config::Config> {
        match &cli.config {
            Some(p) => config::Config::load(p),
            None => Ok(config::Config::fallback(cli.model.clone(), fallback_token, cli.workspace.clone())),
        }
    };
    match cli.cmd {
        Cmd::HashToken { ref token } => println!("{}", config::sha256_hex(token)),
        Cmd::Serve { bind, ref token } => {
            let token = token.clone().unwrap_or_else(http::random_token);
            let cfg = load(&token)?;
            if cli.config.is_none() {
                eprintln!("no --config: single client 'default', Authorization: Bearer {token}");
            }
            let hub = hub::Hub::new(cfg, &cli.provider, cli.session_dir.clone())?;
            eprintln!("pi-agentd listening on http://{bind}");
            http::serve(hub, bind).await?;
        }
        Cmd::Rpc { ref profile } => {
            let cfg = load("")?;
            let hub = hub::Hub::new(cfg, &cli.provider, cli.session_dir.clone())?;
            rpc::run(hub, profile.clone()).await?;
        }
    }
    Ok(())
}
