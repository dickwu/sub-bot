//! Agent 循环，对应 pi-mono `packages/agent`。
//! - `tool`：AgentTool trait（对应 `types.ts` 的 `AgentTool`）
//! - `event`：AgentEvent（对应 `types.ts` 的 `AgentEvent` 联合类型）
//! - `agent_loop`：双层循环（对应 `agent-loop.ts::runLoop`）
//! - `agent`：带状态的 Agent，含 steer/follow-up 队列与 abort（对应 `agent.ts`）
pub mod agent;
pub mod agent_loop;
pub mod event;
pub mod tool;

pub use agent::{Agent, Disposition};
pub use agent_loop::{run_loop, LoopConfig};
pub use event::AgentEvent;
pub use tool::{AgentTool, ExecutionMode, ToolResult, UpdateSink};

#[cfg(test)]
mod reliability_tests;
