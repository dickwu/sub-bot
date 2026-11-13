use agent_core::{AgentTool, ToolResult, UpdateSink};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
        let request = async {
            let resp = self.http.get(url).timeout(REQUEST_TIMEOUT).send().await?;
            let status = resp.status();
            let (body, truncated) = crate::response::read_body(resp, MAX_BODY_BYTES).await?;
            Ok::<_, reqwest::Error>((status, body, truncated))
        };
        let (status, bytes, truncated) = tokio::select! {
            r = tokio::time::timeout(REQUEST_TIMEOUT, request) => r.context("http_fetch timed out")??,
            _ = cancel.cancelled() => bail!("aborted"),
        };
        let mut body = String::from_utf8_lossy(&bytes).into_owned();
        if body.len() > 50_000 {
            let mut cut = 50_000;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
            body.push_str("\n[truncated]");
        }
        if truncated {
            body.push_str(&format!("\n[truncated: response exceeds {MAX_BODY_BYTES} bytes]"));
        }
        let mut r = ToolResult::text(format!("HTTP {status}\n{body}"));
        r.is_error = !status.is_success();
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::tests::server;
    use std::sync::Arc;

    #[tokio::test]
    async fn http_fetch_caps_body() {
        let (url, _, server) = server(Some(vec![b'x'; MAX_BODY_BYTES + 1])).await;
        let tool = HttpFetchTool { allow_hosts: vec!["127.0.0.1".into()], http: agent_ai::http::client() };
        let result = tool.execute("1", json!({"url": url}), CancellationToken::new(), Arc::new(|_| {})).await.unwrap();
        assert!(!result.is_error);
        let agent_ai::Content::Text { text } = &result.content[0] else { panic!("expected text") };
        assert!(text.len() < 51_000);
        assert!(text.contains("truncated: response exceeds"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_fetch_body_read_is_cancellable() {
        let (url, headers, server) = server(None).await;
        let tool = HttpFetchTool { allow_hosts: vec!["127.0.0.1".into()], http: agent_ai::http::client() };
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let call = tokio::spawn(async move { tool.execute("1", json!({"url": url}), token, Arc::new(|_| {})).await });
        headers.await.unwrap();
        cancel.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), call).await.unwrap().unwrap().unwrap_err();
        assert!(error.to_string().contains("aborted"));
        server.abort();
    }
}
