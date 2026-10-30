use agent_ai::{Content, ToolSpec};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Sequential,
    Parallel,
}

/// 工具执行结果（对应 `AgentToolResult`）：`content` 给模型，`details` 给 UI/调用方。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub content: Vec<Content>,
    pub details: Value,
    pub is_error: bool,
    /// 本批次所有工具都置 true 时提前结束循环（对应 `terminate`）。
    pub terminate: bool,
}

impl ToolResult {
    pub fn text(t: impl Into<String>) -> Self {
        Self { content: vec![Content::Text { text: t.into() }], ..Default::default() }
    }
    pub fn error(t: impl Into<String>) -> Self {
        Self { is_error: true, ..Self::text(t) }
    }
}

/// 工具流式进度回调（对应 `AgentToolUpdateCallback`）。
pub type UpdateSink = Arc<dyn Fn(ToolResult) + Send + Sync>;

#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema（pi 用 TypeBox；Rust 可用 schemars 生成）。
    fn parameters(&self) -> Value;
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }
    /// 失败可以返回 Err（转成 isError 的工具结果），不会中断循环。
    async fn execute(&self, call_id: &str, args: Value, cancel: CancellationToken, on_update: UpdateSink) -> anyhow::Result<ToolResult>;

    fn spec(&self) -> ToolSpec {
        ToolSpec { name: self.name().into(), description: self.description().into(), parameters: self.parameters() }
    }
}
