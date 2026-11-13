//! Agent 循环，对应 pi-mono `packages/agent`。
//! - `tool`：AgentTool trait（对应 `types.ts` 的 `AgentTool`）
//! - `event`：AgentEvent（对应 `types.ts` 的 `AgentEvent` 联合类型）
//! - `agent_loop`：双层循环（对应 `agent-loop.ts::runLoop`）
//! - `agent`：带状态的 Agent，含 steer/follow-up 队列与 abort（对应 `agent.ts`）
//! - `approval`：工具调用审批闸门（执行前 allow / ask / deny，ask 时等待调用方决定）
//! - `turn_budget`：回合预算（maxTurns 运行期调整、达到上限时 stop / summarize / ask）
pub mod agent;
pub mod agent_loop;
pub mod approval;
pub mod event;
pub mod tool;
pub mod turn_budget;

pub use agent::{Agent, Disposition, ExclusiveGuard};
pub use agent_loop::{run_loop, BeforeRun, ContextHooks, Emit, LoopConfig, Shared};
pub use approval::{ApprovalDecision, ApprovalGate, ApprovalPolicy, ApprovedBy, PendingApproval, ResolveError, Verdict};
pub use event::AgentEvent;
pub use tool::{AgentTool, ExecutionMode, ToolResult, UpdateSink};
pub use turn_budget::{Adjusted, LastRun, NothingPending, OnMaxTurns, TurnBudgetChangedBy, TurnBudgetDecision, TurnBudgetView, TurnChange, TurnGate, TurnPause};

#[cfg(test)]
mod approval_tests;
#[cfg(test)]
mod reliability_tests;
#[cfg(test)]
mod turn_budget_tests;
