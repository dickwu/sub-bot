use crate::tool::ToolResult;
use agent_ai::{AssistantEvent, Message, ThinkingLevel, ToolResultMessage, Usage};
use serde::Serialize;
use serde_json::Value;

/// 生命周期事件，字段与 pi 的 JSON 事件流（`coding-agent/docs/json.md`）保持同名，方便复用客户端。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AgentEvent {
    AgentStart,
    /// Runtime outcomes that cannot be inferred from provider messages.
    RunOutcome {
        kind: String,
    },
    AgentEnd {
        messages: Vec<Message>,
    },
    /// 会话级：不会再自动继续（对应 RPC 的 `agent_settled`）。
    AgentSettled {
        aborted: bool,
    },
    /// 会话推理强度被修改（从下一次模型调用生效）。
    EffortChanged {
        requested: Option<ThinkingLevel>,
        effective: Option<ThinkingLevel>,
        adjusted: Option<String>,
    },
    /// 会话模型路由已确定（创建时或第一次 prompt 时，只发生一次）。
    ModelRouted {
        routing: Value,
    },
    /// 上下文压缩：`phase` 为 start / end；`reason` 为 auto（模型调用前超过阈值）/ manual（compact 命令）。
    /// end 时带压缩后估计、耗时、摘要调用用量；失败时带 `error`（上下文保持不变）。
    Compaction {
        reason: String,
        phase: String,
        tokens_before: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens_after: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary_chars: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        first_kept_entry_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    TurnStart,
    TurnEnd {
        message: Message,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: Message,
    },
    /// Discard the partial assistant message; attempt is the 1-based retry number.
    MessageRetry {
        attempt: u32,
        reason: String,
    },
    MessageUpdate {
        assistant_message_event: AssistantEvent,
    },
    MessageEnd {
        message: Message,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        partial_result: ToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: ToolResult,
        is_error: bool,
    },
}
