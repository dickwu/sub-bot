//! 日志：tracing + 分级过滤 + 按天滚动的文件日志（默认 JSON，每行带 span 字段：request_id / bot / session_id …）。
//! 约定：**绝不记录密钥**——API key、Bearer token、回调 token 都不进入任何字段；
//! 模型输入输出、工具参数与文件内容只记录长度/计数（debug 级也不记原文）。
use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing_appender::{non_blocking::WorkerGuard, rolling};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

pub fn default_log_dir() -> Option<PathBuf> {
    // Linux: ~/.local/share/sub-bot/logs；macOS: ~/Library/Application Support/sub-bot/logs
    directories::ProjectDirs::from("", "", "sub-bot").map(|d| d.data_local_dir().join("logs"))
}

pub fn init(level: &str, dir: Option<&Path>, format: &str) -> Result<Option<WorkerGuard>> {
    // 依赖库（hyper/reqwest/h2/rustls）在 trace 级可能输出请求头，默认压到 warn，防止 Authorization 头进日志
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(format!("{level},hyper=warn,hyper_util=warn,reqwest=warn,h2=warn,rustls=warn,tower=warn")));
    // stderr：人类可读（stdout 留给 RPC 协议，绝不写日志）
    let stderr = fmt::layer().with_writer(std::io::stderr).with_target(false).with_filter(filter());
    let dir = match dir {
        Some(d) if d == Path::new("-") => None,
        Some(d) => Some(d.to_path_buf()),
        None => default_log_dir(),
    };
    let Some(dir) = dir else {
        tracing_subscriber::registry().with(stderr).init();
        return Ok(None);
    };
    std::fs::create_dir_all(&dir)?;
    let appender = rolling::Builder::new().rotation(rolling::Rotation::DAILY).filename_prefix("sub-bot").filename_suffix("log").max_log_files(14).build(&dir)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file = fmt::layer().with_writer(writer).with_ansi(false);
    let file = if format == "text" { file.boxed() } else { file.json().with_current_span(true).with_span_list(true).boxed() };
    tracing_subscriber::registry().with(stderr).with(file.with_filter(filter())).init();
    tracing::debug!(log_dir = %dir.display(), "file logging enabled");
    Ok(Some(guard))
}
