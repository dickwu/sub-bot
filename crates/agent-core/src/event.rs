use crate::approval::{ApprovalDecision, ApprovedBy};
use crate::tool::ToolResult;
use crate::turn_budget::{OnMaxTurns, TurnBudgetChangedBy, TurnBudgetDecision};
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
    /// 工具调用等待审批（toolApproval 判定为 ask）；用 callId 决定。时间为 Unix 毫秒。
    ApprovalRequired {
        call_id: String,
        tool: String,
        args: Value,
        reason: String,
        requested_at: u64,
        expires_at: u64,
        timeout_secs: u64,
    },
    /// 审批结束（调用方决定 / 策略拒绝 / 超时 / abort）；`approved` 表示工具是否会执行。
    ApprovalResolved {
        call_id: String,
        tool: String,
        decision: ApprovalDecision,
        approved: bool,
        reason: String,
        by: ApprovedBy,
        wait_ms: u64,
    },
    /// 回合上限被调用方修改（立即作用于正在进行的 run，并保留给会话之后的 run）。
    TurnBudgetChanged {
        max_turns: u64,
        previous: u64,
        /// 当前 run 已用回合；空闲时 0
        turns_used: u64,
        by: TurnBudgetChangedBy,
    },
    /// 已用回合达到 ceil(0.8·maxTurns) 且仍有剩余；每个 run 一次，预算修改后可再次触发。
    TurnBudgetLow {
        turns_used: u64,
        max_turns: u64,
        remaining: u64,
    },
    /// 达到回合上限（模型调用之前）；`action` 为会话的 onMaxTurns。ask 时带等待时长与截止时间（Unix 毫秒）。
    TurnBudgetExhausted {
        turns_used: u64,
        max_turns: u64,
        action: OnMaxTurns,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<u64>,
    },
    /// ask 等待结束：extended（继续）/ stopped / timeout / cancelled。
    TurnBudgetResolved {
        decision: TurnBudgetDecision,
        max_turns: u64,
        wait_ms: u64,
    },
}
