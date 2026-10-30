//! 工具层。核心是 pi coding-agent 的内置编码工具（read / write / edit / ls / bash），
//! 对应 `packages/coding-agent/src/core/tools/*.ts`，每个会话按自己的工作目录（cwd）实例化。
//! 在此之上可选：`clock` / `http_fetch` 辅助工具，调用方回调工具（`callback`），以及后续的 MCP 工具。
pub mod builtin;
pub mod callback;
pub mod registry;

pub use callback::{CallbackTool, CallerToolSpec};
pub use registry::{preset, Risk, ToolContext, ToolRegistry, DEFAULT_TOOLS};
