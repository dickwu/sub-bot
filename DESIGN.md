# sub-bot 设计文档：用 Rust 仿 pi coding-agent 做一个可被其它 bot 调用的本地编码 agent

> 目标：我要用 Rust 写一个**以编码为核心**的 agent，架构上尽量贴近 Mario Zechner 的 pi coding-agent（`badlogic/pi-mono`）。
> 它的核心能力是编码：读、写、编辑文件，执行 shell 命令。
> “通用”的意思是它**不绑定某个项目或任务**，不同的 bot 可以拿它在不同的目录里做不同的编码工作。
> 它在 macOS 和 Linux（Ubuntu）上本地运行（不支持 Windows），使用我的 OpenAI API Key，并通过 HTTP、SSE 或 stdio JSONL RPC 给其它 bot 调用。
> API Key 只留在 agent 进程里，绝不外传。
>
> 参考源码：`/workspace/pi-mono`（浅克隆）。本文提到的 pi 路径都相对 `packages/`。
> 脚手架代码：`/workspace/rust-agent`。

---

## (a) pi 的架构

pi 分为三层，依赖方向是单向的：`ai` ← `agent` ← `coding-agent`。

### 1. `ai/`（pi-ai）：统一的 LLM 接口
- `ai/src/types.ts` 定义了以下类型：
  - `Message`：只有 `user`、`assistant`、`toolResult` 三种角色；
  - 内容块：`text`、`thinking`、`image`、`toolCall`；
  - `AssistantMessageEvent` 流事件：`start`，`text_start/_delta/_end`，`thinking_*`，`toolcall_*`，`done`，`error`；
  - `StopReason`：`stop`、`length`、`toolUse`、`error`、`aborted`。
- `ai/src/models.ts` 的 `stream` 和 `streamSimple` 按模型的 `api` 字段分派到 `ai/src/api/*.ts`，例如：
  - `openai-responses.ts`
  - `openai-completions.ts`
  - `anthropic-messages.ts`
  - `google-generative-ai.ts`
- `ai/src/providers/*.ts` 是各家厂商的入口和模型表；`providers/faux.ts` 是测试用的假 provider。
- **关键约定**：流永远不抛异常，失败会被编码成 `stopReason: "error" | "aborted"` 的最终消息。上层因此只需要处理一种结局。

### 2. `agent/`（pi-agent-core）：与具体任务无关的 agent 循环
- `agent/src/agent-loop.ts` 中的 `runLoop`（约第 172 到 340 行）是一个**双层循环**：
  - 外层循环处理 follow-up 消息；
  - 内层循环执行以下步骤：调用 LLM，执行工具（`executeToolCalls`，按工具声明串行或并行），注入 steering 消息，再继续，直到没有工具调用为止。
  - 如果 `stopReason=length`，就把被截断的工具调用全部标记为失败（`failToolCallsFromTruncatedMessage`）。
- 钩子：`convertToLlm`、`transformContext`、`beforeToolCall`、`afterToolCall`、`prepareRequest`、`getSteeringMessages`、`getFollowUpMessages`。
- `agent/src/agent.ts` 中的 `Agent` 类是有状态的外壳，提供 `prompt`、`continue`、`steer`、`followUp`、`abort`、`waitForIdle` 和 `subscribe`。
- `AgentEvent` 包括：`agent_start/end`、`turn_start/end`、`message_start/update/end`、`tool_execution_start/update/end`。
- `agent/src/proxy.ts` 是精简的代理事件，用于“服务端持有密钥”的场景。

### 3. `coding-agent/`：编码 agent 本体
- **会话**：
  - `core/agent-session.ts`（`AgentSession`）把 Agent、工具、会话存储、压缩和扩展粘合在一起；
  - `core/sdk.ts` 的 `createAgentSession` 是对外的 SDK 入口。
- **核心工具**：`core/tools/` 目录下有
  - `read.ts`、`write.ts`、`edit.ts`（精确替换 `edits[{oldText,newText}]`，`oldText` 必须唯一匹配）和 `edit-diff.ts`；
  - `ls.ts`、`grep.ts`、`find.ts`；
  - `bash.ts`（`powershell.ts` 不移植：不支持 Windows）；
  - `file-mutation-queue.ts`（同一文件的写操作串行化）、`truncate.ts`、`output-accumulator.ts`、`path-utils.ts`。
  - `tools/index.ts` 用 `createToolDefinition(name, cwd, …)` 和 `createCodingTools(cwd)` **按 cwd 实例化**这些工具。
  - 默认启用的工具是 `read/bash/edit/write`（见 `core/agent-session.ts` 中的 `defaultTools`）。
- **系统提示**：`core/system-prompt.ts` 的 `buildSystemPromptSections` 按顺序拼接以下几段：
  1. preamble（“expert coding assistant …”，可以用 `customPrompt` 替换）；
  2. 工具列表；
  3. 规则；
  4. `appendSystemPrompt`；
  5. 项目上下文文件；
  6. skills；
  7. cwd。
- **项目上下文**：`core/resource-loader.ts` 从 cwd 向上查找 `AGENTS.override.md`、`AGENTS.md` 和 `CLAUDE.md`。`core/project-trust.ts` 和 `trust-manager.ts` 负责项目信任。
- **会话存储**：`core/session-manager.ts` 把会话存为 JSONL 树。
  - 每个条目都有 `id` 和 `parentId`，叶子指针表示当前分支。条目类型有 `session`、`message`、`model_change`、`thinking_level_change`、`compaction`、`branch_summary`、`label` 等。
  - 文件路径是 `~/.pi/agent/sessions/--<cwd>--/<ts>_<uuid>.jsonl`（格式见 `docs/session-format.md`）。
- **压缩**：`core/compaction/compaction.ts` 和 `branch-summarization.ts` 负责压缩。
- **扩展**：`core/extensions/`、`core/skills.ts`、`core/prompt-templates.ts` 和 `core/mcp-servers.ts`。MCP 客户端在独立包 `mcp/src`（client.ts 和 transports）。
- **运行模式**：`modes/interactive`（TUI）、`modes/print-mode.ts`、`modes/json-event.ts`、`modes/rpc/`。
  - `modes/rpc/` 包含 `rpc-mode.ts`、`rpc-types.ts`、`rpc-client.ts` 和 `jsonl.ts`。
  - RPC 是严格以 LF 分帧的 JSONL：命令格式为 `{id?, type:"prompt"|"steer"|"follow_up"|"abort"|"get_state"|"get_messages"|"set_model"|"compact"|"bash"|"fork"…}`，响应格式为 `{id, type:"response", command, success, data|error}`，事件与 `AgentEvent` 相同，空闲时额外发送 `agent_settled`。
  - 参考文档：`docs/rpc.md`、`docs/rpc-commands.md`、`docs/json.md`。

---

## (b) pi 组件到 Rust crate 的映射

| pi | Rust（`/workspace/rust-agent/crates/`） | 状态 |
|---|---|---|
| `ai/src/types.ts` | `agent-ai/src/types.rs`：`Message`、`Content`、`AssistantMessage`、`StopReason`、`AssistantEvent`，serde 字段名与 pi 的 JSON 一致 | ✅ |
| `ai/src/models.ts` + `api/*` | `agent-ai/src/provider.rs`：`Provider` trait（`stream(req, cancel) -> EventStream`，**不返回错误**）和 `ProviderRegistry` | ✅ |
| `api/openai-completions.ts` | `agent-ai/src/openai.rs`：Chat Completions 流式调用（按字节缓冲 SSE，累积 tool call 增量，读取 usage 和 cached_tokens，处理流中途的 error，流没有 finish_reason 就提前结束时判为错误，错误信息里的 key 会被打码，支持取消） | ✅ 已用 gpt-4.1-mini 实测 |
| `api/openai-responses.ts` | `agent-ai/src/responses.rs`：Responses API 流式调用（`POST /v1/responses`，`store:false`；输入项 message / `function_call` / `function_call_output`；处理 `output_text.delta`、`function_call_arguments.delta/done`、`output_item.added/done`、`response.completed/incomplete/failed`、`error`；usage 读取 cached_tokens 和 reasoning_tokens）。`openai.rs` 按 `api: auto\|chat\|responses` 分派 | ✅ v0.3，已用 gpt-5.4、gpt-6-astra 实测 |
| `api/anthropic-messages.ts` | `agent-ai/src/anthropic.rs`：Claude（Anthropic Messages API 流式：`message_start`、`content_block_start/delta/stop`、`message_delta`、`message_stop`，其中 `input_json_delta` 用来累积工具参数，`thinking` 块对应 Thinking），实现同一个 `Provider` trait。每个 profile 用 `"provider": "openai" \| "anthropic"` 加 `model` 选择；key 只从 `ANTHROPIC_API_KEY` 环境变量读取。目前没有 Anthropic key，只做 faux 和单测（离线校验请求体构建与 SSE 解析） | 计划（M1.5） |
| `providers/faux.ts` | `agent-ai/src/faux.rs`：离线回显；输入 `/tool <name> <json>` 会触发一次工具调用 | ✅ |
| `agent/src/agent-loop.ts` | `agent-core/src/agent_loop.rs`：`run_loop` 双层循环、steering 和 follow-up 队列、串行或并行执行工具、`length` 截断处理 | ✅（已有单测） |
| `agent/src/agent.ts` | `agent-core/src/agent.rs`：`Agent`（broadcast 订阅，`prompt`、`steer`、`follow_up`、`abort`） | ✅ |
| `AgentTool` / `AgentEvent` | `agent-core/src/{tool,event}.rs`，事件名与 `docs/json.md` 一致 | ✅ |
| 钩子 `beforeToolCall`/`afterToolCall`/`transformContext` | 在 `LoopConfig` 上加 `Arc<dyn Hooks>` | M3 |
| `coding-agent/core/tools/*` | `agent-tools/src/builtin/fs.rs`：**核心工具** `read`、`write`、`edit`、`ls`、`bash`（PowerShell），全部限定在会话 cwd 内 | ✅（edit 已有单测） |
| `tools/grep.ts`、`find.ts`、`file-mutation-queue.ts`、`truncate.ts` | `agent-tools/src/builtin/{grep,find}.rs`（用 `ignore` 和 `grep-searcher` crate），`mutation_queue.rs` | M1 |
| `tools/index.ts` 的 `createCodingTools(cwd)` | `agent-tools/src/registry.rs`：`ToolRegistry::with_builtins()` 加上 `build(names, ToolContext{cwd})`；`DEFAULT_TOOLS = read, ls, edit, write, bash`；预设有 `default`、`readonly`、`no-shell` | ✅ |
| `core/system-prompt.ts` + `resource-loader.ts` 的 AGENTS.md 部分 | `agentd/src/prompt.rs`：preamble 或自定义内容、工具列表、规则、append、AGENTS.md/CLAUDE.md（从 cwd 向上查找到 workspaceRoot 为止）、cwd | ✅ |
| `core/session-manager.ts` | `agent-session/src/lib.rs`：JSONL 树（`id`/`parentId`、叶子指针、`active_messages`） | ✅ 读写；恢复会话在 M2 |
| `core/compaction/*` | `agent-compaction` crate | M2 |
| `core/skills.ts`、`extensions/`、`prompt-templates.ts` | `agent-resources` crate（只做 skills 和 prompt templates；扩展改成 MCP 或回调） | M4 |
| `mcp/`（pi-mcp）、`core/mcp-servers.ts` | `agent-mcp` crate（可以基于 `rmcp`），MCP 工具注册进 `ToolRegistry` | M4 |
| `modes/rpc/*` | `agentd/src/rpc.rs`：stdio JSONL，命令和响应格式与 `rpc-types.ts` 一致 | ✅ 部分命令 |
| `core/sdk.ts` / `AgentSession` | `agentd/src/hub.rs`：`Hub::create_session` 校验 profile、工具白名单和 cwd，并组装 Agent、工具、系统提示和存储 | ✅ |
| （pi 没有）多 bot 的 HTTP 服务 | `agentd/src/http.rs`（axum + SSE）和 `config.rs`（profiles、clients、token 哈希） | ✅ |
| interactive TUI | 不做（这个项目面向 bot） | — |

设计上的取舍：
- 核心编码工具总是注册的，profile 只用白名单选择启用哪些。
- `clock`、`http_fetch`、调用方回调工具（`callback.rs`）和以后的 MCP 工具都是**附加**的，不是核心。

---

## (c) 给其它 bot 的接口

### 鉴权与隔离
- 每个 bot 一个 token。配置里**只存 SHA-256**（用 `sub-bot hash-token <token>` 生成），比较时用常量时间比较。
- 请求头为 `Authorization: Bearer <token>`。
- 每个 client 有一个 profile 白名单（`clients[].profiles`）。
- 会话按 owner 隔离：别的 bot 访问时一律返回 404。会话文件存放在 `<sessionDir>/<client>/` 下。
- 每个 profile 有 `workspaceRoots`。会话可以指定 `cwd`（相对路径相对第一个 root，也可以写绝对路径），服务端 canonicalize 之后必须落在某个 root 之内，否则返回 400（已测试 `/tmp` 和 `..` 都会被拒）。
- 工具内部还会做一次 `resolve_in_root` 校验（拒绝绝对路径和 `..`）。
- 请求里的 `tools` 只能在 profile 白名单范围内**收窄**，不能扩大。
- `systemPrompt`/`appendSystemPrompt` 覆盖需要 profile 设置 `allowSystemPromptOverride`；调用方回调工具需要 `allowCallerTools`。
- **OpenAI Key（v0.3.1）**：`agent_ai::secrets` 负责加载，顺序为 `--openai-key-file` > `OPENAI_API_KEY_FILE` > 配置 `apiKeyFile` > `OPENAI_API_KEY`。读文件时去掉首尾空白；出错时只报路径和原因，不带内容。key 在内存里用 `Secret` 包装：`Debug`/`Display` 输出 `[redacted]`，drop 时清零，只在设置 `Authorization` 头时 `expose()`。任何响应、事件和日志里都不会出现 key，日志只记录来源。
- **bash 子进程环境**：spawn 的地方只有一个（`builtin/fs.rs` 的 `BashTool`），这里会应用 `agent_tools::ShellEnv`。
  - 内置密钥变量表 `secrets::secret_env_vars()` 包括各 provider 的 `*_API_KEY`/`*_API_KEY_FILE`（登记在 `PROVIDER_KEYS`，以后加 provider 时在这里加一行），以及 `SUB_BOT_TOKEN(_FILE)`。运行时还能用 `register_secret_var` 追加。这些变量总会被 `env_remove`。
  - profile 的 `shellEnv {mode: inherit|allowlist, deny, allow}` 再做一层过滤：`inherit` 剔除 `deny` 中且不在 `allow` 中的变量；`allowlist` 先 `env_clear`，再只放回 `BASE_ALLOWLIST` 加 `allow`，最后减去 `deny`。
  - 没有用进程内 `remove_var`：它清不掉 `/proc/<pid>/environ`，多线程下还不安全。同用户的子进程依然可以读 key 文件或 `/proc/<ppid>/environ`，所以 README 推荐用文件提供 key，并建议用独立用户或沙箱做真正隔离。
- 默认只监听 `127.0.0.1:7878`。如果要跨机器访问，建议经过反向代理加 TLS（M3 会内建 rustls 支持）。

### HTTP API（`/v1`）
| 方法和路径 | 说明 |
|---|---|
| `GET /health` | 无需鉴权 |
| `GET /profiles` | 当前 bot 可用的 profile，包括 model、allowedModels、reasoningEffort、tools 和 workspaceRoots |
| `GET /models?profile=&all=` | 调用 provider 的 `/v1/models` 实时取模型列表（Hub 缓存 10 分钟），默认过滤掉非对话模型（embedding/tts/image/realtime/codex/-pro 等）和日期快照；每项带 `reasoning`、`reasoningEfforts` |
| `POST /sessions` | 请求体为 `{profile, cwd?, model?, reasoningEffort?, tools?, systemPrompt?, appendSystemPrompt?, callerTools?}`，返回 `{sessionId, profile, cwd, model, reasoningEffort, tools}`；model 不在 profile 的 `allowedModels` 内返回 400 |
| `GET /sessions/{id}` | 会话信息及运行状态；非 owner 或未知会话返回 404 |
| `DELETE /sessions/{id}` | owner 删除内存会话并 abort 当前运行，返回 `{"deleted":true}`；未知/非 owner 返回 404，持久化 JSONL 保留并记日志 |
| `POST /sessions/{id}/prompt` | 请求体为 `{message, mode?: "steer"|"follow_up"}`，返回 202 和 `started`/`queuedSteer`/`queuedFollowUp`（语义同 pi：运行中只能 steer 或 follow_up） |
| `GET /sessions/{id}/events` | SSE，`event:` 名称与 pi 的 `AgentEvent.type` 一致，额外有 `agent_settled{aborted}` |
| `POST /sessions/{id}/abort` | 中止当前运行 |
| `GET /sessions/{id}/messages` | 返回当前分支的消息 |
| `POST /chat` | 一次性调用：`{message, sessionId?, session?: {...}, stream?}`。不流式时返回 `{sessionId, text, stopReason, error, aborted, messages}`；`stream:true` 时返回 SSE，直到 `agent_settled` |

示例：
```bash
curl -X POST localhost:7878/v1/chat -H 'Authorization: Bearer <你的 bot token>' -H 'content-type: application/json' \
  -d '{"message":"修复 src/lib.rs 里的编译错误并运行 cargo test","session":{"profile":"coder","cwd":"projA"}}'
```

### 会话生命周期（v0.3.2）
- 顶层配置 `sessionIdleTtlSecs` 默认 3600；无 `sessionId` 的 `/chat` 自动创建临时会话，使用独立的 `chatSessionTtlSecs`（默认 600）。两个字段各自的 0 表示禁用该类过期。
- 创建、submit、运行完成后更新单调时钟时间；GET/SSE 不刷新。`serve` 启动 sweeper，周期为最短非零 TTL 与 60 秒的较小值，仅清理未运行且超过其 TTL 的会话。`Hub::evict_idle(now)` 可注入时间测试，每次清理记 info 日志。
- `maxSessionsPerClient` 默认 64，0 不限制，包括普通和临时会话。创建时在同一 map 锁内检查/插入；满额先回收该 client 已过期的空闲会话，仍满返回专用错误（HTTP 429），其它参数错误返回 400。
- HTTP 提交、删除、清理使用同一个 map 锁，避免删除后旧 handle 再启动运行。DELETE 会取消正在运行的 Agent；清理与删除都保留已有 JSONL，当前不自动恢复旧 ID。
- 临时会话可通过返回的 ID 继续（流式响应使用 `X-Session-Id` 头），续聊刷新计时但不改变 TTL。
- RPC 只有一个进程生命周期内的会话，无需 delete 命令或 sweeper；关闭 stdin 会 abort 并退出。

### stdio JSONL RPC（`sub-bot rpc --profile coder`）
- 父进程直接拉起 agent，命令和响应格式与 pi 的 `modes/rpc/rpc-types.ts` 一致：
  ```
  → {"id":"1","type":"prompt","message":"..."}
  ← {"id":"1","type":"response","command":"prompt","success":true}
  ← {"type":"message_update","assistantMessageEvent":{...}} ... {"type":"agent_settled","aborted":false}
  ```
- 目前支持的命令：`prompt`、`steer`、`follow_up`、`abort`、`get_messages`、`get_state`。
- M2 加入 `set_model`、`compact`、`fork`、`new_session`、`switch_session`。
- 关闭 stdin 等于关机（与 pi 相同）。
- 本地 stdio 被视为可信，可以使用全部 profile。

### 调用方工具（附加能力）
- `callerTools: [{name, description, parameters, callbackUrl, callbackToken}]`。
- 工具被调用时，服务端 POST `{sessionId, toolCallId, toolName, arguments}` 到 `callbackUrl`，期待返回 `{content, isError}`。
- M4 计划改为：除了 HTTP 回调，也可以让 bot 在 SSE/RPC 上通过 `tool_result` 命令回填结果，或者挂载 MCP server。

---

## (d) 跨平台构建与分发

- **纯 Rust TLS**：`reqwest` 使用 `rustls-tls`，不依赖 OpenSSL，所以各平台都能静态或近静态链接。
- **Shell**：
  - `bash` 工具运行 `sh -c`（只支持 macOS / Linux，`agentd` 在非 unix 平台上会直接编译失败）；
  - 进程设置 `kill_on_drop`，有超时，并支持取消。
- **路径**：统一用 `PathBuf` 加 `canonicalize`，比较 root 时两边都 canonicalize。系统提示里直接使用 unix 路径。
- **目标平台**：
  - `x86_64-apple-darwin`、`aarch64-apple-darwin`（可以合成 universal 二进制）
  - `x86_64-unknown-linux-musl`、`aarch64-unknown-linux-musl`
- **发布（已确定）**：
  - 源码放在这台机器的 `/workspace/rust-agent`；
  - 由 GitHub Actions CI 构建 macOS 和 Linux 的预编译二进制（v0.1.1 起不再提供 Windows），每台 PC 直接下载安装。
  - 草稿工作流在 `.github/workflows/release.yml`：
    - 推送 `v*` tag 时，先在 Ubuntu 和 macOS 上跑 `cargo test --locked`；
    - 然后矩阵构建 4 个目标（Linux musl 用 `cross`）；
    - 打包成 `tar.gz`，附带 `.sha256`，上传到 GitHub Release。
  - 仓库：https://github.com/dickwu/sub-bot ；推送到 main 时另由 `ci.yml` 跑测试。
  - 以后可以换成 `cargo-dist`，额外生成安装脚本（sh）和 Homebrew formula。
  - release profile 已经开启 `lto="thin"` 和 `strip=true`。
- **作为服务运行**：
  - Linux 用 systemd user unit；
  - macOS 用 launchd plist（`~/Library/LaunchAgents`）；
  - M5 提供 `sub-bot service install` 命令。
- **配置和数据目录**：用 `directories` crate 按操作系统决定：
  - Linux：`~/.config/sub-bot`，数据放在 `~/.local/share`；
  - macOS：`~/Library/Application Support/sub-bot`；
- **API Key 来源**：环境变量，或者系统钥匙串（`keyring` crate，M3）。Key 不写进配置文件。

---

## (d2) 日志（tracing）

- **框架**：`tracing`，加上 `tracing-subscriber`（`EnvFilter`）和 `tracing-appender`。实现见 `agentd/src/logging.rs`。
- **级别**：用 `--log-level`（环境变量 `SUB_BOT_LOG`，默认 `info`）设置；`RUST_LOG` 优先，可以按模块细调，例如 `agentd=debug,agent_ai=trace`。
  - 依赖库 `hyper`、`reqwest`、`h2`、`rustls` 默认压到 `warn`，防止请求头在 trace 级被打印出来。
- **输出**：
  - stderr 输出人类可读格式。stdout 只留给 RPC 协议，永远不写日志。
  - 文件日志按天滚动，保留 14 天，文件名为 `sub-bot.YYYY-MM-DD.log`，默认是 JSON 格式（`--log-format json|text`）。
  - 默认目录：
    - Linux：`~/.local/share/sub-bot/logs`
    - macOS：`~/Library/Application Support/sub-bot/logs`
  - 用 `--log-dir <dir>` 改目录，传 `-` 关闭文件日志。
- **Span 层级**（JSON 日志的每一行都带完整的 span 列表）：
  - `http{request_id, method, path, bot}`：request_id 沿用调用方的 `X-Request-Id`，否则自动生成，并回写到响应头；
  - 其下是 `session_run{session_id, bot, profile, model, reasoning_effort}`；
  - 再往下是 `llm{provider, model, api, reasoning_effort}`（实际发送的值）和 `tool{tool, tool_call_id}`。
- **关键事件**：
  - 请求结束（status、latency_ms，4xx 记为 warn，5xx 记为 error）；
  - `session created`（cwd、model、tools）；
  - `run started/finished`（耗时、消息数、工具调用数、输入和输出 token、stop_reason）；
  - `llm call finished/failed`（耗时、token、cached_tokens）；
  - `tool finished/failed`（耗时）；
  - 鉴权失败；
  - 会话持久化失败。
- **绝不记录密钥**：
  - API key、Bearer token、回调 token 都不进入任何字段，鉴权失败也只记录“token 缺失或未知”；
  - 上游错误信息会先打码，形如 `sk-…` 的内容替换为 `[redacted]`；
  - 提示词、模型输出、工具参数和文件内容只记录长度或计数。
  - 例外：没有配置文件时，`serve` 会把随机生成的 token 用 eprintln 打到 stderr 一次，方便本机使用；这条不经过 tracing，也不进日志文件。
- 已实测：日志文件和会话 JSONL 里都搜不到 key、`sk-` 或 bot token。

---

### 模型选择与推理强度（v0.2）
- `ThinkingLevel`：`none`（别名 `off`）/ `minimal` / `low` / `medium` / `high` / `xhigh`，对应 OpenAI 的 `reasoning_effort`。
- 优先级：会话请求（HTTP `model`/`reasoningEffort`、CLI `--model`/`--reasoning-effort`）> profile > 全局默认；`allowedModels` 支持 `*` 通配。
- `agent-ai/src/models.rs` 有一张静态能力表（2026-10 实测）：gpt-4.x/gpt-4o 不支持；gpt-5/-mini/-nano 为 minimal..high；gpt-5.1 为 none/low/medium/high；gpt-5.2 及以后加 xhigh；o3 为 low..xhigh；chat-latest 只有 medium。建会话时按表处理：不支持就忽略并 warn，档位不支持就取最接近的并 warn。
- 实测：gpt-5.4 及以后在 Chat Completions 上拒绝“函数工具 + reasoning_effort”（只能 none），gpt-6.x 带工具时完全不可用（要求 /v1/responses）。v0.3 起 `api: auto` 让这些模型默认走 Responses API；`auto` 下遇到该错误也会切换并缓存。推理强度降级缓存的键是（模型, API, 是否带工具），每次调用最多重试 2 次，且不会重复尝试同一组 (API, 强度)。
- 实际生效的 API 和推理强度写在每条 `AssistantMessage` 的 `api` / `reasoningEffort` 上，`run --json`、会话响应和 RPC `get_state` 都报告实际值。
- 预算：profile 的 `maxOutputTokens` 限制单次输出；`tokenBudget` 限制整次运行，超出后注入一条提示并做最后一次不带工具的调用，让模型直接给出结论。
- 表外模型（如 gpt-6.x）先原样发送；OpenAI 返回 400 时 `openai.rs` 解析错误（`Unrecognized request argument` → 不支持；`Supported values are: ...` → 可选档位），按模型缓存，warn 后自动重试一次。

## v0.3.2 reliability

- `agent-core::Agent::submit` 原子地启动或排队，返回 `Disposition`（`started` / `queued_steer` / `queued_follow_up`）；提交与收尾交接共用 state 锁，避免丢消息或重复启动。
- 消息历史使用 `std::sync::Mutex`；模型请求读取快照，只在更新/复制时短暂持锁，不跨 `await`。工具调用前检查取消（含并行分支）；abort 后丢弃尚未消费的排队消息，持久化完成后才发送 `agent_settled`。
- RPC 的长命令（历史快照、模型列表）放进 `JoinSet` 后台执行，stdin 循环仍可及时处理 abort/steer。
- 会话 TTL、owner 删除和容量限制见上文「会话生命周期」；`serve` 同时处理 Ctrl-C / SIGTERM，关机时禁止新建/提交，abort 全部会话并等待收尾持久化。
- 超时分三层：LLM 流空闲、profile 整次运行、非流式 chat 等待；chat 超时取消运行并返回 504 和部分结果。bash 使用独立进程组，取消/超时/清理时杀掉子孙进程；工具输出和网络响应均有资源上限，数值见 README。
- 日志过滤在解析 `RUST_LOG` 后追加网络依赖的 `warn` 抑制指令，覆盖同名用户指令，避免请求头进入 trace 日志。
- CI：fmt 检查、全 workspace/all-targets clippy（`-D warnings`），Ubuntu/macOS 跑 locked 测试。release 先检查 tag 与 workspace 版本一致并通过相同门禁，再构建四个目标、打包和发布校验和。

## v0.4.0 webhooks

- `/v1/chat` 支持 `async:true`（与 stream 互斥），返回 202、Location 和 taskId/sessionId/statusUrl；后台任务不依赖请求连接。同步/异步共用 `http::collect_chat`，沿用临时会话、事件快照修复、timeoutSecs/chatTimeoutSecs、部分文本和 5 秒 abort 收尾语义。
- Hub 持有 owner 隔离的 `Mutex<HashMap<taskId, Task>>`，记录 running/succeeded/failed/timeout/aborted、Unix 毫秒时间、结果和投递状态。`GET /v1/tasks/{id}` 对未知/非 owner 返回 404。完成后独立按 taskTtlSecs（默认 3600）清理；单调时钟控制过期，既有 sweeper 同时处理任务。关机取消运行并等待任务状态收尾。
- client 配置 webhookUrl/webhookSecretFile/webhookAllowedHosts；secret 相对配置目录加载、trim、不可读/空值报错，Debug 打码。回调 URL 限 http/https 且匹配 host/host:port 白名单（默认 URL 的 host 隐式允许），禁重定向，错误不回显 URL。
- 每个有目标的任务一个 mpsc 队列/worker，串行调用 Hub 共享的 `webhook::Deliverer`；`all` 模式的工具结束/assistant 消息结束生成 progress，完成后排入 task.completed（含任务结果快照）。慢投递不阻塞 chat 的超时处理；on_update 更新最近投递状态。任务/队列不持久化，进程退出不保证排空，广播过载仍可能丢进度事件（最终结果用 AgentEnd 快照补齐）。
- 投递协议：POST、`Content-Type: application/json`、`User-Agent: sub-bot/0.4.0`，JSON envelope `{deliveryId,event,timestamp,data}`；请求头为 `X-SubBot-Event`、`X-SubBot-Delivery`、`X-SubBot-Timestamp`（Unix 秒）。配置 secret 时附 `X-SubBot-Signature: t=<秒>,v1=<小写十六进制 HMAC-SHA256>`，签名输入为 `<t>.<raw body>`；`t` 来自签名头，与 `X-SubBot-Timestamp` 相同，每次尝试重新取当前时间并签名。delivery id、原始正文和正文中的 timestamp 在重试间不变，验签不能用正文 timestamp 代替 `t`。接收端使用原始字节验签、校验时间窗口并按 delivery id 去重。
- `webhook::Deliverer` 实现完整重试：默认最多 6 次（含首次，webhookMaxAttempts 可覆盖），每次超时 10 秒（含错误响应体读取）。网络错误/超时/408/429/5xx 重试，其它 4xx 不重试；2xx 成功，3xx 不重试且不跟随重定向（Deliverer 自建禁重定向 client）。指数退避从 1 秒起逐次翻倍，加 0–10% 随机抖动，总等待上限 60 秒；429/503 的非负整数秒 `Retry-After` 优先（同样最多 60 秒），不支持 HTTP-date，无效值回退到指数退避。取消 delivery future 会停止请求和退避，不另起后台任务。
- 测试使用 faux provider、router oneshot 和本地 axum receiver，覆盖 202/Location、轮询/owner/TTL、poll-only、白名单、超时和有序事件投递、签名验证、关机及 secret 加载打码。

## (e) MVP 里程碑

| 里程碑 | 内容 | 验收 |
|---|---|---|
| **M0（已完成，已用 OpenAI 实测）** | 5 个 crate；双层 agent loop；OpenAI Chat Completions 流式调用；核心工具 read/write/edit/ls/bash；会话 cwd 与 workspaceRoots 校验；AGENTS.md 注入；JSONL 会话；HTTP、SSE、stdio RPC；每个 bot 一个 token 和 profile | `cargo check` 无警告；3 个单测通过；faux 冒烟测试通过（edit 改文件，越权 cwd 和工具被拒，401/404 隔离） |
| **M1：编码能力补齐** | ✅ OpenAI Responses API（v0.3）；`grep`、`find`（遵守 .gitignore）；file mutation queue；pi 的 `truncate` 输出截断策略；`read` 支持 offset/limit；edit 返回 diff | 用真实 key 跑一个“修 bug 并跑测试”的端到端任务 |
| **M1.5：Claude** | `agent-ai/src/anthropic.rs` 接入 Anthropic Messages API；profile 增加 `provider` 字段；Hub 持有多个 provider，按 profile 分派 | 先用离线单测覆盖 SSE 解析和请求体，有 key 后再做端到端测试 |
| **M2：会话** | 恢复会话（`sessionId` 重开后加载 JSONL）；fork 和分支切换；`compaction`（自动压缩和 `compact` 命令）；`set_model`；RPC 命令补齐 | 长会话超过上下文后能自动压缩并继续 |
| **M3：安全与治理** | `beforeToolCall` 审批钩子（profile 可设 bash 需要调用方确认，确认通过 SSE/RPC 往返）；bash 命令允许和拒绝规则；每个 bot 的限流和并发上限；审计日志；内建 TLS；keyring | 只读 profile 无法写文件，也无法执行命令；审批流程可用 |
| **M4：扩展** | MCP 客户端（stdio/HTTP）；skills 和 prompt templates；项目信任（对应 `project-trust.ts`）；调用方工具支持通过 RPC 回填 | 接入一个 MCP server 后能调用其工具 |
| **M5：分发** | cargo-dist 三平台六个目标；`service install`；按操作系统决定配置目录；安装文档 | 在 Win、mac、Linux 上一键安装并作为服务运行 |

---

## v0.5.0 issues

`config::Targets` 支持单 URL 和最多 10 项的混合数组，加载时解析相对 secretFile，生成 `ResolvedTarget`；alerts 全局路由，client targets 通过 session/task 订阅隔离。`Reporter` 为一次安装的 Arc 全局服务（无运行时测试/RPC 可用 noop）：同步脱敏后写有界 drop-oldest 队列，Notify 唤醒 worker；按 `(kind, component, session_id, client)` 做 Tokio 时钟窗口聚合，摘要 count 包括首条。JSONL + 最近 1000 条 ring 是本地 sinks，网络复用 v0.4 Deliverer，每目标独立任务、签名与重试，Semaphore 限制 32 个并发请求。

问题投递事件名为 `issue`。最终 webhook 失败转换为 `webhook_delivery_failed`，携带不序列化的失败 URL，按规范化后的完整 URL 排除失败接收器（不排除同 host 的其它端口/路径），details 仅保留 host。外部任务失败入口 `on_delivery_failed(status, url, scope, targets)` 同时投递 alerts 和所属 client 的 task/session issue targets，遵守 events/minSeverity；Hub 在创建投递 worker 时快照订阅，避免重试结束前订阅已清理。失败通知本身的投递失败不再产生次级失败问题，保留防递归 guard。tracing Layer 只收本项目 WARN/ERROR，跳过 issues/webhook 以及携带 `issue = "typed"` 的已分类诊断日志；panic hook 尽力入队，配置/Hub 初始化失败使用最多 5 秒同步投递；退出 flush 也有超时。I2 在 hub/http 接入运行事件、任务多目标 fan-out 与查询 API。所有来源只传元数据，Reporter 集中掩码密钥、剔除敏感 details 键、限制消息/详情长度；落盘/网络失败不传播到业务运行。

## 附：脚手架用法
```bash
cd /workspace/rust-agent
cargo build --release
export OPENAI_API_KEY_FILE=~/.config/sub-bot/openai.key   # 推荐；也可用 --openai-key-file 或 OPENAI_API_KEY
./target/release/sub-bot hash-token my-bot-token   # 把结果填进配置的 tokenSha256
./target/release/sub-bot --config sub-bot.example.json serve           # HTTP 服务
./target/release/sub-bot --workspace ~/code rpc                       # stdio RPC
./target/release/sub-bot --provider faux --config sub-bot.example.json serve   # 离线测试，不调用 OpenAI
```
`sub-bot.example.json` 中有三个 profile：
- `coder`：全部核心工具；
- `reviewer`：只读，`preset:readonly`；
- `docs-writer`：不能执行 shell，外加 `http_fetch`。

另有两个 client：`ci-bot` 只能用 reviewer；`dev-bot` 三个都能用。示例里的 `tokenSha256` 是占位符，必须换成 `sub-bot hash-token <随机 token>` 的输出。
