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
        let resp = tokio::select! {
            r = tokio::time::timeout(std::time::Duration::from_secs(120), req.send()) => r.context("callback timed out")??,
            _ = cancel.cancelled() => anyhow::bail!("aborted"),
        };
        let reply: CallbackReply = resp.error_for_status()?.json().await?;
        let mut r = ToolResult::text(reply.content);
        r.is_error = reply.is_error;
        Ok(r)
    }
}
