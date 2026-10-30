use crate::tool::ToolResult;
use agent_ai::{AssistantEvent, Message, ToolResultMessage};
use serde::Serialize;
use serde_json::Value;

/// 生命周期事件，字段与 pi 的 JSON 事件流（`coding-agent/docs/json.md`）保持同名，方便复用客户端。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<Message>,
    },
    /// 会话级：不会再自动继续（对应 RPC 的 `agent_settled`）。
    AgentSettled {
        aborted: bool,
    },
    TurnStart,
    TurnEnd {
        message: Message,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: Message,
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
