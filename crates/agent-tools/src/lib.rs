//! 工具层。核心是 pi coding-agent 的内置编码工具（read / write / edit / ls / grep / find / bash），
//! 对应 `packages/coding-agent/src/core/tools/*.ts`，每个会话按自己的工作目录（cwd）实例化。
//! grep / find 内嵌 ripgrep 引擎（ripgrep 的 grep-* / ignore / globset 库 crate），不依赖外部 `rg`。
//! `shell_env`：bash 子进程的环境变量策略（默认剔除 provider key 等密钥变量）。
//! 在此之上可选：`clock` / `http_fetch` 辅助工具，调用方回调工具（`callback`），以及后续的 MCP 工具。
pub mod builtin;
pub mod callback;
pub mod registry;
mod response;
pub mod shell_env;

pub use callback::{CallbackTool, CallerToolSpec};
pub use registry::{preset, Risk, ToolContext, ToolRegistry, DEFAULT_TOOLS};
pub use shell_env::{EnvMode, ShellEnv};
