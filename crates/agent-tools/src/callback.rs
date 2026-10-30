use agent_core::{AgentTool, ToolResult, UpdateSink};
use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// 调用方在创建会话时提交的工具声明。模型调用它时，agent 把参数 POST 到 `callbackUrl`，
/// 调用方返回 `{"content": "...", "isError": false}`。这样 bot 的业务能力留在 bot 自己那边。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallerToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub callback_url: String,
    /// 回调时带上的 Authorization 头（由调用方提供，agent 只转发）
    pub callback_token: Option<String>,
}

pub struct CallbackTool {
    pub spec: CallerToolSpec,
    pub session_id: String,
    pub http: reqwest::Client,
}

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallbackReply {
    content: String,
    #[serde(default)]
    is_error: bool,
}

#[async_trait::async_trait]
impl AgentTool for CallbackTool {
    fn name(&self) -> &str {
        &self.spec.name
    }
    fn description(&self) -> &str {
        &self.spec.description
    }
    fn parameters(&self) -> Value {
        self.spec.parameters.clone()
    }
    async fn execute(&self, call_id: &str, args: Value, cancel: CancellationToken, _u: UpdateSink) -> anyhow::Result<ToolResult> {
        let mut req = self.http.post(&self.spec.callback_url).json(&json!({
            "sessionId": self.session_id, "toolCallId": call_id, "toolName": self.spec.name, "arguments": args,
        }));
        if let Some(t) = &self.spec.callback_token {
            req = req.bearer_auth(t);
        }
        let request = async {
            let resp = req.send().await?.error_for_status()?;
            let (body, truncated) = crate::response::read_body(resp, MAX_RESPONSE_BYTES).await?;
            anyhow::ensure!(!truncated, "callback response exceeds {MAX_RESPONSE_BYTES} bytes");
            Ok::<CallbackReply, anyhow::Error>(serde_json::from_slice(&body)?)
        };
        let reply = tokio::select! {
            r = tokio::time::timeout(CALLBACK_TIMEOUT, request) => r.context("callback timed out")??,
            _ = cancel.cancelled() => anyhow::bail!("aborted"),
        };
        let mut r = ToolResult::text(reply.content);
        r.is_error = reply.is_error;
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::tests::server;
    use std::sync::Arc;

    fn tool(url: String) -> CallbackTool {
        CallbackTool {
            spec: CallerToolSpec { name: "test".into(), description: String::new(), parameters: json!({}), callback_url: url, callback_token: None },
            session_id: "test".into(),
            http: reqwest::Client::new(),
        }
    }

    #[tokio::test]
    async fn callback_caps_body() {
        let (url, _, server) = server(Some(vec![b'x'; MAX_RESPONSE_BYTES + 1])).await;
        let error = tool(url).execute("1", json!({}), CancellationToken::new(), Arc::new(|_| {})).await.unwrap_err();
        assert!(error.to_string().contains("callback response exceeds"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn callback_timeout_includes_body() {
        let (url, headers, server) = server(None).await;
        let call = tokio::spawn(async move { tool(url).execute("1", json!({}), CancellationToken::new(), Arc::new(|_| {})).await });
        headers.await.unwrap();
        // 先完成真实网络握手，再快进时钟，避免测试等待 120 秒。
        tokio::time::pause();
        tokio::time::advance(CALLBACK_TIMEOUT + std::time::Duration::from_secs(1)).await;
        let error = call.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("callback timed out"));
        server.abort();
    }
}
