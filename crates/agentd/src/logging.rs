//! 日志：tracing + 分级过滤 + 按天滚动的文件日志（默认 JSON，每行带 span 字段：request_id / bot / session_id …）。
//! 约定：**绝不记录密钥**——API key、Bearer token、回调 token 都不进入任何字段；
//! 模型输入输出、工具参数与文件内容只记录长度/计数（debug 级也不记原文）。
use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing_appender::{non_blocking::WorkerGuard, rolling};
use tracing_subscriber::{
    fmt,
    layer::SubscriberExt,
    registry::{LookupSpan, SpanRef},
    util::SubscriberInitExt,
    EnvFilter, Layer,
};

pub fn default_log_dir() -> Option<PathBuf> {
    // Linux: ~/.local/share/sub-bot/logs；macOS: ~/Library/Application Support/sub-bot/logs
    Some(crate::config::Config::default_data_dir().join("logs"))
}

fn build_filter(rust_log: Option<&str>, level: &str) -> EnvFilter {
    let mut filter = rust_log.and_then(|value| EnvFilter::try_new(value).ok()).unwrap_or_else(|| EnvFilter::new(level));
    // 始终最后添加，覆盖用户的同名指令，避免请求头（包括 Authorization）进入日志。
    // rmcp（MCP SDK）的 debug 日志含完整 JSON-RPC 消息（工具参数 / 结果），同样压到 warn；MCP 的生命周期日志由 agentd::mcp 输出。
    for directive in ["hyper=warn", "hyper_util=warn", "reqwest=warn", "h2=warn", "rustls=warn", "tower=warn", "rmcp=warn"] {
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
        tracing_subscriber::registry().with(IssuesLayer).with(stderr).init();
        crate::config::warn_data_dir_fallback();
        return Ok(None);
    };
    crate::config::prepare_temp_data_dir(&dir, true)?;
    std::fs::create_dir_all(&dir)?;
    let appender = rolling::Builder::new().rotation(rolling::Rotation::DAILY).filename_prefix("sub-bot").filename_suffix("log").max_log_files(14).build(&dir)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file = fmt::layer().with_writer(writer).with_ansi(false);
    // text 文件层用独立的字段格式化类型：否则它和 stderr 层共用同一个 span 扩展，`span.record` 的字段会被追加两次
    let file = if format == "text" { file.fmt_fields(FileFields::default()).boxed() } else { file.json().with_current_span(true).with_span_list(true).boxed() };
    tracing_subscriber::registry().with(IssuesLayer).with(stderr).with(file.with_filter(filter())).init();
    crate::config::warn_data_dir_fallback();
    tracing::debug!(log_dir = %dir.display(), "file logging enabled");
    Ok(Some(guard))
}

/// WARN/ERROR from our crates only; webhook/reporter diagnostics must never loop.
struct IssuesLayer;
impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for IssuesLayer {
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let Some(reporter) = crate::issues::global() else {
            return;
        };
        report_event(&reporter, event, &ctx);
    }
}

/// Test-only IssuesLayer that reports into a given reporter instead of the global one.
#[cfg(test)]
pub(crate) struct IssuesTo(pub std::sync::Arc<crate::issues::Reporter>);
#[cfg(test)]
impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for IssuesTo {
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        report_event(&self.0, event, &ctx);
    }
}

/// 日志事件可以用 `issue` 字段声明它和问题上报的关系：
/// - `issue = "typed"`：同一事件已经由结构化来源（`Reporter::report`）上报，日志只做诊断；
/// - `issue = "none"`：纯诊断/访问日志，永远不成为 issue（例如 4xx 的 `request finished`）；
/// - `issue = "fallback"`：只有在所属 HTTP 请求里还没有更具体的 issue 时才上报（5xx 的 `request finished`）。
///
/// 其它值按普通字段处理。
#[derive(Debug)]
enum LogIssue {
    Ignore,
    Typed,
    Report(crate::issues::IssueEvent),
    Fallback(crate::issues::IssueEvent),
}

/// 标记：所属 HTTP 请求 span 里已经上报过 issue（日志派生或 typed）。
struct RequestIssueReported;

/// 事件所在的 HTTP 请求 span（http.rs `trace_request` 创建，名为 `http`）。
fn request_span<'a, S: tracing::Subscriber + for<'l> LookupSpan<'l>>(event: &tracing::Event<'_>, ctx: &'a tracing_subscriber::layer::Context<'_, S>) -> Option<SpanRef<'a, S>> {
    ctx.event_scope(event)?.find(|span| span.name() == "http" && span.metadata().target().ends_with("::http"))
}

fn report_event<S: tracing::Subscriber + for<'a> LookupSpan<'a>>(reporter: &crate::issues::Reporter, event: &tracing::Event<'_>, ctx: &tracing_subscriber::layer::Context<'_, S>) {
    let mark = || {
        if let Some(span) = request_span(event, ctx) {
            span.extensions_mut().replace(RequestIssueReported);
        }
    };
    match classify_event(event) {
        LogIssue::Ignore => {}
        LogIssue::Typed => mark(),
        LogIssue::Report(issue) => {
            mark();
            reporter.report(issue);
        }
        LogIssue::Fallback(issue) => {
            // 同一请求已经有更具体的 issue（例如 "session rehydration failed"）时，通用的访问日志不再重复上报。
            if !request_span(event, ctx).is_some_and(|span| span.extensions().get::<RequestIssueReported>().is_some()) {
                reporter.report(issue);
            }
        }
    }
}

fn classify_event(event: &tracing::Event<'_>) -> LogIssue {
    let meta = event.metadata();
    let target = meta.target();
    if !matches!(*meta.level(), tracing::Level::WARN | tracing::Level::ERROR)
        || !(target.starts_with("agent_") || target.starts_with("agentd") || target.starts_with("sub_bot"))
        || ["agentd::issues", "agentd::webhook", "sub_bot::issues", "sub_bot::webhook"].iter().any(|p| target.starts_with(p))
    {
        return LogIssue::Ignore;
    }
    #[derive(Default, PartialEq)]
    enum Marker {
        #[default]
        Report,
        Typed,
        None,
        Fallback,
    }
    #[derive(Default)]
    struct Fields {
        message: String,
        fields: Vec<String>,
        marker: Marker,
    }
    impl tracing::field::Visit for Fields {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            match (field.name(), value) {
                ("issue", "typed") => self.marker = Marker::Typed,
                ("issue", "none") => self.marker = Marker::None,
                ("issue", "fallback") => self.marker = Marker::Fallback,
                _ => self.record_debug(field, &value),
            }
        }
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = format!("{value:?}");
            } else if !crate::issues::sensitive_field(field.name()) {
                self.fields.push(format!("{}={value:?}", field.name()));
            }
        }
    }
    let mut fields = Fields::default();
    event.record(&mut fields);
    match fields.marker {
        // Keep the diagnostic log, but let the structured source report the issue once.
        Marker::Typed => return LogIssue::Typed,
        Marker::None => return LogIssue::Ignore,
        Marker::Report | Marker::Fallback => {}
    }
    if !fields.fields.is_empty() {
        fields.message.push(' ');
        fields.message.push_str(&fields.fields.join(" "));
    }
    let (severity, kind) = if *meta.level() == tracing::Level::ERROR { (crate::issues::Severity::Error, "log_error") } else { (crate::issues::Severity::Warn, "log_warn") };
    let issue = crate::issues::IssueEvent::new(severity, kind, target, fields.message);
    if fields.marker == Marker::Fallback {
        LogIssue::Fallback(issue)
    } else {
        LogIssue::Report(issue)
    }
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
    fn issues_layer_filters_sources_and_sensitive_fields() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::{layer::SubscriberExt, Layer};
        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<crate::issues::IssueEvent>>>);
        impl<S: tracing::Subscriber> Layer<S> for Capture {
            fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
                if let super::LogIssue::Report(issue) = super::classify_event(event) {
                    self.0.lock().unwrap().push(issue);
                }
            }
        }
        let capture = Capture::default();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(capture.clone()), || {
            tracing::warn!(target: "agent_core::run", attempts = 3, api_key = "private-key", token = "private-token", body = "private-body", prompt = "private-prompt", "run warning");
            tracing::error!(target: "sub_bot::http", issue = "unclassified", "request error");
            tracing::warn!(target: "agent_core::run", issue = "typed", "typed warning");
            tracing::error!(target: "agent_ai::openai", issue = "typed", "typed error");
            tracing::warn!(target: "agentd::hub", issue = "typed", "typed assistant failure");
            tracing::warn!(target: "agentd::http", issue = "none", status = 404, "access log");
            tracing::error!(target: "agentd::http", issue = "fallback", status = 500, "fallback is decided per request span");
            tracing::warn!(target: "agentd::issues", "ignored");
            tracing::error!(target: "agentd::webhook", "ignored");
            tracing::warn!(target: "sub_bot::webhook", "ignored");
            tracing::error!(target: "sub_bot::issues", "ignored");
            tracing::warn!(target: "reqwest", "ignored");
            tracing::info!(target: "agentd", "ignored");
        });
        let issues = capture.0.lock().unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].kind, "log_warn");
        assert_eq!(issues[0].component, "agent_core::run");
        assert!(issues[0].message.contains("run warning"));
        assert!(issues[0].message.contains("attempts=3"));
        assert!(!issues[0].message.contains("private"));
        assert_eq!(issues[1].kind, "log_error");
    }

    /// `issue = "none"` 永不上报；`issue = "fallback"` 只在同一 HTTP 请求 span 里还没有其它 issue（含 typed）时上报。
    #[tokio::test]
    async fn request_fallback_is_reported_only_without_a_specific_issue() {
        use tracing_subscriber::layer::SubscriberExt;
        let reporter = crate::issues::Reporter::new(&crate::config::AlertsConfig { dedup_window_secs: 0, ..Default::default() }, None, vec![]);
        tracing::subscriber::with_default(tracing_subscriber::registry().with(super::IssuesTo(reporter.clone())), || {
            let request = |path: &str| tracing::info_span!(target: "sub_bot::http", "http", path = %path);
            // 只有通用访问日志：上报一次
            request("/plain").in_scope(|| tracing::error!(target: "sub_bot::http", issue = "fallback", status = 502, "request finished"));
            // 具体错误在前（日志派生）：访问日志不再上报；嵌套 span 里的事件也算
            request("/specific").in_scope(|| {
                tracing::info_span!("session_run").in_scope(|| tracing::error!(target: "sub_bot::http", error = "disk full", "session purge failed"));
                tracing::error!(target: "sub_bot::http", issue = "fallback", status = 500, "request finished");
            });
            // typed（结构化来源已上报）同样抑制访问日志
            request("/typed").in_scope(|| {
                tracing::warn!(target: "sub_bot::http", issue = "typed", "chat timed out");
                tracing::error!(target: "sub_bot::http", issue = "fallback", status = 504, "request finished");
            });
            // 4xx 访问日志永不上报；标记只属于各自的请求
            request("/client-error").in_scope(|| tracing::warn!(target: "sub_bot::http", issue = "none", status = 400, "request finished"));
            request("/after").in_scope(|| tracing::error!(target: "sub_bot::http", issue = "fallback", status = 503, "request finished"));
        });
        reporter.flush(std::time::Duration::from_secs(2)).await;
        let messages: Vec<_> = reporter.recent(None, 0, 100).into_iter().map(|e| e.message).collect();
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(messages[0].starts_with("request finished status=502"), "{messages:?}");
        assert!(messages[1].starts_with("session purge failed"), "{messages:?}");
        assert!(messages[2].starts_with("request finished status=503"), "{messages:?}");
        assert!(!messages.iter().any(|m| m.contains("issue=")), "{messages:?}");
    }

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
