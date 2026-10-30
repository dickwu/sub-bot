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

fn build_filter(rust_log: Option<&str>, level: &str) -> EnvFilter {
    let mut filter = rust_log.and_then(|value| EnvFilter::try_new(value).ok()).unwrap_or_else(|| EnvFilter::new(level));
    // 始终最后添加，覆盖用户的同名指令，避免请求头（包括 Authorization）进入日志。
    for directive in ["hyper=warn", "hyper_util=warn", "reqwest=warn", "h2=warn", "rustls=warn", "tower=warn"] {
        filter = filter.add_directive(directive.parse().expect("valid suppression directive"));
    }
    filter
}

pub fn init(level: &str, dir: Option<&Path>, format: &str) -> Result<Option<WorkerGuard>> {
    let rust_log = std::env::var("RUST_LOG").ok();
    let filter = || build_filter(rust_log.as_deref(), level);
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
    // text 文件层用独立的字段格式化类型：否则它和 stderr 层共用同一个 span 扩展，`span.record` 的字段会被追加两次
    let file = if format == "text" { file.fmt_fields(FileFields::default()).boxed() } else { file.json().with_current_span(true).with_span_list(true).boxed() };
    tracing_subscriber::registry().with(stderr).with(file.with_filter(filter())).init();
    tracing::debug!(log_dir = %dir.display(), "file logging enabled");
    Ok(Some(guard))
}

/// 与 DefaultFields 输出相同，但类型不同，使每个 fmt 层拥有各自的 span 字段缓存。
#[derive(Default)]
struct FileFields(fmt::format::DefaultFields);

impl<'w> fmt::FormatFields<'w> for FileFields {
    fn format_fields<R: tracing_subscriber::field::RecordFields>(&self, writer: fmt::format::Writer<'w>, fields: R) -> std::fmt::Result {
        self.0.format_fields(writer, fields)
    }
}

#[cfg(test)]
mod tests {
    use super::build_filter;

    #[test]
    fn dependency_suppression_overrides_user_filters() {
        for rust_log in [Some("trace"), Some("hyper=trace"), Some("hyper=trace,hyper_util=trace,reqwest=trace,h2=trace,rustls=trace,tower=trace"), None] {
            let filter = build_filter(rust_log, "trace").to_string();
            let directives: Vec<_> = filter.split(',').collect();
            for target in ["hyper", "hyper_util", "reqwest", "h2", "rustls", "tower"] {
                assert!(directives.contains(&format!("{target}=warn").as_str()), "{filter}");
                assert!(!directives.contains(&format!("{target}=trace").as_str()), "{filter}");
            }
        }
    }

    #[test]
    fn invalid_rust_log_falls_back_to_level_with_suppression() {
        assert_eq!(build_filter(Some("hyper=invalid"), "debug").to_string(), build_filter(None, "debug").to_string());
    }
}
