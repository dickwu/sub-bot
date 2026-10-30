use agent_core::{AgentTool, ToolResult, UpdateSink};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// HTTP GET，仅允许 profile 白名单里的主机（空白名单 = 全部拒绝）。
pub struct HttpFetchTool {
    pub allow_hosts: Vec<String>,
    pub http: reqwest::Client,
}

#[async_trait::async_trait]
impl AgentTool for HttpFetchTool {
    fn name(&self) -> &str {
        "http_fetch"
    }
    fn description(&self) -> &str {
        "HTTP GET a URL on an allow-listed host and return the body as text."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]})
    }
    async fn execute(&self, _id: &str, args: Value, cancel: CancellationToken, _u: UpdateSink) -> anyhow::Result<ToolResult> {
        let url = reqwest::Url::parse(args["url"].as_str().context("missing url")?)?;
        let host = url.host_str().unwrap_or_default().to_string();
        if !matches!(url.scheme(), "http" | "https") || !self.allow_hosts.iter().any(|h| h == &host) {
            bail!("host '{host}' is not allowed for this profile");
        }
        let resp = tokio::select! {
            r = self.http.get(url).send() => r?,
            _ = cancel.cancelled() => bail!("aborted"),
        };
        let status = resp.status();
        let mut body = resp.text().await?;
        if body.len() > 50_000 {
            let mut cut = 50_000;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
            body.push_str("\n[truncated]");
        }
        let mut r = ToolResult::text(format!("HTTP {status}\n{body}"));
        r.is_error = !status.is_success();
        Ok(r)
    }
}
