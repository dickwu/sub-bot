use agent_core::{AgentTool, ToolResult, UpdateSink};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// 无副作用的示例工具：返回当前 UTC 时间。
pub struct ClockTool;

#[async_trait::async_trait]
impl AgentTool for ClockTool {
    fn name(&self) -> &str { "clock" }
    fn description(&self) -> &str { "Return the current date and time in UTC (RFC 3339)." }
    fn parameters(&self) -> Value { json!({"type":"object","properties":{}}) }
    async fn execute(&self, _id: &str, _a: Value, _c: CancellationToken, _u: UpdateSink) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::text(chrono::Utc::now().to_rfc3339()))
    }
}
