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

## 工具调用审批（toolApproval）

- **运行时**（`agent_core::approval`）：`LoopConfig.approval: Option<Arc<ApprovalGate>>`。`run_one` 在工具 span 内、执行之前调用 `ApprovalGate::check`（只对存在的工具）：`ApprovalPolicy::evaluate(tool, args) -> Verdict{Allow|Ask|Deny}`；Deny 直接返回错误工具结果并发 `approval_resolved{by:policy}`；Ask 把 `(callId → oneshot)` 放进挂起表、发 `approval_required`，`select!` 等待调用方 / `cancel`（abort、runTimeout、删除会话）/ 超时。超时或取消与调用方决定竞争时以“谁从挂起表里取走条目”为准；最近 1024 个已决定 callId 用于区分 409 与 404。并行批次（`join_all`）里每个调用独立等待；顺序批次逐个等待。
- **策略**（`agent_tools::approval`）：`ToolApproval = Never | Always | Rules{default, tools, bash, timeoutSecs, onTimeout}`，自定义 Deserialize（解析即校验）。工具规则：精确名 > 最具体 glob > default；bash 规则先于工具规则：deny/ask 检查整条命令与每个片段，allow 要求每个片段命中且无命令替换。`ApprovalStack(profile, session)` 取最严格判定，因此会话级策略只能收紧；策略全为 allow 时不创建闸门。
- **服务侧**（`agentd::approvals`）：profile/全局策略 + `SessionRequest.toolApproval` ⇒ 闸门；`GET/POST /v1/sessions/{id}/approvals[/{callId}]`（handler 通过 `FromRef<AppState> for Arc<Hub>` 取 hub）、RPC `approve`、会话视图/get_state 的 `pendingApprovals`；异步任务另开一个订阅把 `approval_required` 投递为 `task.approval_required` webhook。超时在 `report_event` 里上报 warn issue `approval_timeout`（WARN 日志带 `issue="typed"`，不重复成 `log_warn`）。
- **CLI**：`ask` 在交互终端（stderr TTY + `/dev/tty`，非 `--json`）里用 `/dev/tty` 询问并 POST 决定，`--auto-approve/--auto-deny`；`--async` 轮询 approvals 列表。`run` 交互时询问，否则 ask ⇒ deny + 警告，`--auto-approve` 例外。

## v0.8.1 OpenAI 默认模型

- `config::DEFAULT_OPENAI_MODEL = "gpt-6.1-sol"`（全局 `defaultModel` 默认值与 CLI 回退），`DEFAULT_OPENAI_ALLOWED_MODELS = ["gpt-6.1-sol", "gpt-6-astra"]`。
- `Config::fallback_for`（无 `--config`）：provider 为 openai（或未指定且模型不是 `claude-*`）时 default profile 的 `allowedModels` 为上述两个，`--model` 显式指定的其它模型自动加入；anthropic/faux 不限制。显式配置文件仍可使用任何模型。
- `sub-bot models`：无 `--profile` 时按同 provider、设置了 `allowedModels` 的 profile 并集标记 `*`，JSON 增加 `allowed`。
- 推理强度调整：hub 的 `SessionDynamic::apply_effort` 按 (模型, 请求值) 去重后 warn 一次；provider 内部用 `resolve_effort_quiet`（debug）。`session_run` span 在路由后重录 `model` / `reasoning_effort`。

## v0.8.0 动态推理强度与 Claude 模型路由

- **运行期可变配置**：`agent_core::LoopConfig` 的 `model` 与 `reasoning` 改为 `Shared<T>`（`Arc<Mutex<T>>` 包装）。`stream_assistant` 每次模型调用读取一次快照，所以修改从**下一次调用**生效，包括正在进行的 run（工具结果之后那次调用）。`LoopConfig.before_run: Option<BeforeRun>` 是一个异步 hook，Agent 在每次 run 的第一次模型调用之前调用（与 abort 竞争：取消时不再等待）；在 hook 执行期间 Agent 已处于 running，新的提交照常排队。`Agent::emit` 允许在会话事件流上广播运行期事件。新增事件 `effort_changed {requested, effective, adjusted}` 与 `model_routed {routing}`。
- **SessionDynamic**（hub.rs）：会话中运行期可变的部分——provider、`Shared` model/reasoning、请求的 effort 与调整原因、`Routing`、issue scope、最近一次调用、`prompted` 标志。它不引用 Agent / Hub，因此可以被 before_run hook 持有而不形成循环引用；模型目录缓存改为 `Arc<ModelsCache>` 供 hook 共享。
- **推理强度**：所有入口（创建、PATCH、prompt/chat 顶层字段、RPC `set_reasoning_effort` / `prompt.reasoningEffort`、CLI `--effort`）都调用 `SessionHandle::set_reasoning_effort` → `resolve_effort(provider, 当前模型, requested)`（top-3 规则、钳制记 warn）。修改是粘性的；实际值变化时清空 last_call，使 `reasoningEffort` 报告新值直到下一次调用回写实际发送值（含 OpenAI 运行时降级）。每次修改广播 `effort_changed` 并记 info 日志。路由改变模型后用同一请求值重新解析。
- **默认 Claude 模型**：`Hub::default_model_for(provider)`：`providers.<id>.defaultModel` > （anthropic 且全局 `defaultModel` 不是 `claude-*`）`claude-opus-5-5` > `defaultModel`；只在请求和 profile 都没有 model 时使用。
- **路由**（routing.rs）：`depth = standard | deep | auto`，默认 `autoRoute ? auto : standard`。显式 `model` 总是胜出（reason `explicit model`）。anthropic profile 的 `deepModel` 默认**固定**为 `claude-fable-5-1`（默认配置不随新模型漂移）；`latest:<prefix>` 在路由时用 provider 模型目录（10 分钟缓存）解析：按 id 中前缀之后的数字段排序（`5-1` > `5`），含非数字段（如 `-preview`）的 id 不参与；同版本存在无日期 id 时忽略 8 位日期快照，否则取最新日期。解析失败 → 基础模型 + reason。
  - standard / deep（固定 id）在创建和首个 prompt 之前的 PATCH 时同步规划（会话视图立即显示目标模型）；auto 与别名在 before_run hook 中决定。第一次 run 时 hook 设置 `decided=true` 并最终确定模型，之后不再路由；`prompted` 在第一次 submit 时置位，此后修改 depth 返回 `DepthLocked`（HTTP 409）。
  - auto 分类器：routerModel（默认基础模型）一次调用，固定系统提示，用户消息为首个 prompt 文本（前 8000 字符），无工具，`max_tokens=200`，30 秒超时，1 次瞬态重试；推理强度为 low（仅当 low 在该模型允许的 top-3 中，否则不发送，避免被钳制成高强度）。在回复中逐个 `{` 尝试流式解析，取第一个含布尔 `deep` 的对象，reason 截为 20 个词。失败 → standard、reason `router error: ...`、warn issue `routing_error`（不包含模型输出或 prompt）。分类用量记在 `routing.usage`，`run --json` 的 `totalUsage` 包含它。未配置 deepModel 时跳过分类。
- **报告**：`SessionHandle::effort_fields()` 统一输出 `reasoningEffort`、`requestedReasoningEffort`、`effortAdjusted`、`allowedEfforts`、`routing`；用于会话视图（POST/GET/PATCH）、`/v1/chat` 响应与 task result、`run --json`、RPC `get_state`；`ask --json` 在结束时追加一行 `{"type":"session_state", ...会话视图}`。`/v1/profiles` 增加 `allowedEfforts` / `defaultEffort` / `routing{deepModel, autoRoute, routerModel, defaultDepth}`。
- **HTTP**：新增 `PATCH /v1/sessions/{id}`（`{reasoningEffort?, depth?}`，未知字段 400）。JSON 请求体用 `ApiJson` 提取器，解析错误（含非法 effort/depth 名称）统一返回 400 而非 axum 默认 422；`POST /v1/sessions` 仍允许空请求体。
- **RPC**：`run_io` 抽象 stdin/stdout 以便测试；新增 `set_reasoning_effort`，`prompt` 支持 `reasoningEffort` / `depth`；`rpc --depth`、`run --depth`。

## 多 provider 集成（v0.7.0）

- Hub 持有按 id 索引的 provider registry；每个 Agent 的 LoopConfig 持有选定 provider 的 Arc，不依赖服务全局单一 provider。选择顺序为 session.provider > profile.provider > 有效模型 `claude-` 推断 > CLI/config 默认（最终 openai）。已建会话不会因后续请求而换 provider。
- 启动计算 needed 集合：默认 provider + 默认模型推断 + 每个 profile 的显式/推断选择。needed 缺 key 即失败；其它 provider 有来源才加载，无来源静默跳过（debug）。faux 无需 key、始终注册。显式配置但无效的 key 文件仍是错误，不降级到低优先级来源。
- `providers.<id>` 保存 apiKeyFile/baseUrl；顶层 apiKeyFile 是 OpenAI 别名，两者冲突拒绝。key 文件按配置目录解析。加载优先级（v0.9.0 起，`secrets::load_account_key`）：CLI `--<provider>-key-file` > `<P>_API_KEY_FILE` > 配置声明的来源（`apiKeyFile` / `apiKeyKeychain` 二选一）> `<P>_API_KEY` > 受管存储（`sub-bot keys set` 写入：macOS Keychain `sub-bot` / `<provider>/default`，Linux `~/.config/sub-bot/keys/<provider>-default.key`）。来源存在但读取失败即报错，不降级。base URL 环境变量优先于配置。Hub 加载时只记录 key_source，并将所有 key 注册到共享 Reporter 的脱敏列表。
- 模型目录按 provider 独立缓存 10 分钟，输出扁平模型信息 + provider；筛选只请求选定 provider，OpenAI 对话/快照过滤不影响其它 provider。目录请求失败整体返回错误（不返回不完整目录）。profiles 暴露解析后的 provider；LLM issue scope 带 provider，用于归因和去重。
- 非 OpenAI 忽略 API 选择，reasoning 按 provider/model 的 effort catalog 解析。Hub 使用 `AnthropicProvider::with_stream_idle_timeout(Secret, Option<String>, Duration)` 连接真实 Messages API，传入已加载的 key、base URL 和 idle timeout。支持 tool_result 合并、签名 thinking 回放、最多三个缓存断点及分页模型目录；message_start / message_delta 的 usage 按字段保留最新非 null 值。
- ask 的同步 SSE 错误、异步提交/轮询 HTTP 错误包含服务端 JSON error；非 JSON 错误保留 HTTP 状态摘要。

## v0.7.0 流式客户端与可靠性改进

- `ask` 同步使用 SSE，实时输出文本 delta；`--json` 逐事件输出 NDJSON，异步轮询保持不变。
- 默认数据目录不再 panic：依次使用 BaseDirs、绝对路径 XDG_DATA_HOME、temp/sub-bot-<euid>；临时目录检查所有权、权限和符号链接，显式与自动发现的 ask socket 同样受保护。
- `/v1/issues` 在 worker 处理重复 issue 后立即更新同一条目的 count / lastTs / severity，不等待窗口结束；webhook 汇总不重复写入本地查询结果。

## v0.6.0 本地调用传输与信任边界

- `server.rs` 管理 TCP + Unix socket 双 listener，共用 `http::app` 的 axum router 和 shutdown 生命周期。TCP 可通过 `--bind none` 关闭；UDS 通过 `serve --unix-socket [path]` 或配置 `unixSocket` 开启。`Config::data_dir()` 统一解析 `dataDir` / `--data-dir`，默认系统数据目录/sub-bot；默认 socket 为其中的 `sub-bot.sock`。
- `local.rs` 的 `Connected` 实现将内核连接信息放入 `ConnectInfo<Peer>`：`Tcp(SocketAddr)` 或 `Unix { uid, gid, pid }`。Tokio `peer_cred()` 封装 Linux/macOS 差异，获取失败时不授予本地身份；用户名用 reentrant `getpwuid_r` 查询。HTTP headers 不能提供或覆盖 peer credentials。
- `http::auth` 优先验证显式 Authorization；无 header 时才按 `localClients` 顺序匹配 uid/username/any，或在 `trustLoopback` 开启时信任实际 TCP loopback（含 IPv4-mapped IPv6）。最终仍插入相同 `Arc<Client>`，handler 的 profile/owner 检查不变。无匹配回退到 401，错误 Bearer 不得回退到本地信任。启动验证所有引用的 client，并对 loopback 信任发 WARN；忽略 X-Forwarded-For。
- UDS 默认 `0600`，`unixSocketMode` 接受 octal permission string；新目录为 `0700`。先在私有临时目录 bind/chmod，再原子发布 socket，避免短暂宽权限。只清理 connection-refused 的旧 socket，保留普通文件/符号链接/活动 listener。RAII guard 按 inode/dev 检查后在关闭/错误退出时清理自身 socket；SIGKILL 的残留在下次启动清理。
- `ask.rs` 是独立轻量客户端，不构建 Hub 或 provider。UDS 用 Tokio UnixStream + hyper HTTP/1.1，TCP/HTTPS 用 reqwest（无代理、无重定向）；两者使用同一 `/v1/chat` payload。同步请求设置 `stream:true`，增量解析 SSE（支持分块、multi-line data 和 keep-alive），逐文本 delta 写入并 flush stdout，结束换行；同步 `--json` 第一行输出 `{"type":"session","sessionId":"<id>"}`（来自响应头 `x-session-id`，旧服务端缺失此头时省略），供后续 `ask --session <id>` 继续会话；然后逐事件输出服务端 JSON 的 NDJSON（含工具、错误），最后一行仍为服务端最终 `agent_settled` 事件，不会等待最终响应。UDS/TCP 一致，纯文本 stdout 不受 session 元数据影响。`--async` 通过 `/v1/tasks/{id}` 轮询后输出最终文本，`--async --json` 保留完整最终 task 响应。HTTP/模型错误、abort、缺少 settled 或整体 `--timeout` 到期均向 stderr 报错并非零退出；超时不取消服务端任务。认证只从 token file 或 SUB_BOT_TOKEN 取值，不打印。
- 信任粒度是操作系统 uid，而非进程/程序：映射 uid 的所有进程可充当对应 client；`trustLoopback` 扩大到所有本地进程。socket 权限和父目录访问权限应保持严格；远程访问仍需 Bearer token。不要把向不可信网络转发的本地代理纳入 loopback 信任。
- Unix 测试覆盖实际 UDS/TCP、uid/username/any 映射、无匹配 401、Bearer 优先级、IPv6/mapped loopback、伪造转发头、socket mode/残留清理/文件保护、faux provider 下同步和异步 ask。系统调用使用 Unix cfg，适用于 Linux/macOS CI。
## 持久化与恢复（agentd/store.rs）

- 仅 `serve` 显式选择 `StoreMode::Persistent`。`Hub::new` 默认使用 `Store::memory()`（`StoreMode::Memory`），供 `run` / `rpc` / `models` 使用：不打开文件或锁、不加载 journal，因此没有重启恢复或历史 pending delivery 续投，可与同一 dataDir 的服务共存。内存 store 保留当前进程的任务/投递状态；日志与可选 session-dir 历史独立配置。
- `Store` 管理 append-only `tasks.jsonl` 和 `deliveries.jsonl`，每次状态更新追加完整快照并 sync_data。启动按 ID 取最新记录，再通过临时文件 + rename + 目录 fsync 压缩；仅忽略崩溃留下的最后一个不完整行；完整行损坏时将原文件重命名为 `<name>.corrupt-<unix_ms>`（0600），保留损坏行之前的记录，reporter 挂载后报告 error 级 `store_corrupt`。目录/文件权限分别为 0700/0600；`Store::open` 在读取/压缩前对 `<dataDir>/state/lock` 获取非阻塞排他 flock，文件句柄持有到 Store 释放，进程退出（含崩溃）自动解锁；同一 dataDir 的第二个服务启动失败，lock 文件不删除以避免 inode 竞争。
- `dataDir` / CLI `--data-dir` 由 `Config::data_dir()` 统一解析，默认系统数据目录/sub-bot；journal 存放在 `<dataDir>/state`，不受 session-dir 影响。`shutdownGraceSecs` 默认 10，限制退出等待。task TTL 使用持久化的墙钟完成时间，重启不会延长 TTL。
- task 入队和运行前记录 queued/running，完成记录结果/error 和各目标 delivery 状态。启动发现 queued/running 时转 failed（`interrupted by restart`），发出 error 级 `task_interrupted`，不重放工具副作用。结果只允许 owner 查询。
- webhook 入队先落盘再启动网络请求；保留 URL、event、payload、client、task/index、deliveryId、attempts 和状态，不序列化 WebhookTarget/签名 secret。恢复时仅在原 client 当前配置和管理员 alerts 中解析目标 URL；找不到则终止投递并报告。请求级 callback 若希望跨重启恢复，应将 URL 配置在原 client 下。
- `Deliverer::resume` 接受既有 DeliveryStatus，保持 ID 与尝试预算；网络前记录尝试，响应后记录状态。接收端按 ID 去重，崩溃发生在接收成功与本地确认之间时可能重复。每个 task/URL 独立串行通道，失败目标不会阻塞其它目标。
- worker 使用 Weak<Store>，Store drop 取消后台重试，不让 worker 保活 hub；SIGINT/SIGTERM 沿现有 abort_all 路径阻止 admission、abort agent，并有界 flush reporter 与持久化投递。超时仍 pending 的记录下次恢复。数据含业务 payload，权限控制不能替代磁盘加密/备份策略。

## Issue 的 owner 路由

SessionHandle 在 submit 前订阅 `ScopeKey::Session`，覆盖同步 chat、SSE、普通 session 及 RPC 运行；async task 使用 `ScopeKey::Task`。settled 后 250ms 再取消 session 订阅，并用运行代数防止旧运行清理新订阅。Reporter 的路由始终校验 scope.client 与订阅 owner，按 events/minSeverity 过滤；取消订阅后保存至少 dedupWindowSecs 的缓存。入队 issue 捕获路由快照，窗口汇总不会因 task/session 已结束而丢失路由；dedup key 包含 task ID，避免同一 session 连续任务混并。管理员 alerts 是独立路由，不参与 client 间广播。issue webhook 在 `serve` 中同样进入持久化队列，其它命令使用内存队列，失败通知禁止递归。
## LLM transient retry / webhook progress

OpenAI / Anthropic provider 共享重试策略，在每次尝试内新建流解析器，通过单独事件通道转发增量并跟踪工具调用边界。连接/请求超时、body read 错误、未完成 EOF、idle timeout、HTTP 408/429/5xx 可重试；其它 HTTP 4xx（包括认证、上下文溢出）不可重试。文本或思考增量允许回滚，工具事件一旦出现则禁止重试。
`llmMaxRetries` 全局默认 3，profile 可覆盖（0 禁用）；1s 指数退避加 jitter，最大 30s，`Retry-After` 是额外的最短等待约束（先限制到最多 60s）。整个重试循环在取消 select 内运行，abort 同时取消网络和 sleep。
`AssistantEvent::Retry` 转为 `AgentEvent::MessageRetry`，消费者清理当前部分输出，hub 只增加 `llm_retry` warn 映射；最终失败沿用 `llm_*`。日志标注 `issue="typed"` 防止重复上报。只有最终消息进入 context/AgentEnd。
异步任务过滤空白助手 progress；webhook Retry-After 支持 delta-seconds 和 HTTP-date，过去日期为零、上限 60s。

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
| `api/anthropic-messages.ts` | `agent-ai/src/anthropic.rs`：Claude（Anthropic Messages API 流式：`message_start`、`content_block_start/delta/stop`、`message_delta`、`message_stop`，其中 `input_json_delta` 用来累积工具参数，`thinking` 块对应 Thinking），实现同一个 `Provider` trait。每个 profile 用 `"provider": "openai" \| "anthropic"` 加 `model` 选择；key 与 OpenAI 共用 `agent_ai::secrets` 的加载优先级（见下文「鉴权与隔离」中的 OpenAI Key 一条）。目前没有 Anthropic key，只做 faux 和单测（离线校验请求体构建与 SSE 解析） | 计划（M1.5） |
| `providers/faux.ts` | `agent-ai/src/faux.rs`：离线回显；输入 `/tool <name> <json>` 会触发一次工具调用 | ✅ |
| `agent/src/agent-loop.ts` | `agent-core/src/agent_loop.rs`：`run_loop` 双层循环、steering 和 follow-up 队列、串行或并行执行工具、`length` 截断处理 | ✅（已有单测） |
| `agent/src/agent.ts` | `agent-core/src/agent.rs`：`Agent`（broadcast 订阅，`prompt`、`steer`、`follow_up`、`abort`） | ✅ |
| `AgentTool` / `AgentEvent` | `agent-core/src/{tool,event}.rs`，事件名与 `docs/json.md` 一致 | ✅ |
| 钩子 `beforeToolCall`/`afterToolCall`/`transformContext` | 在 `LoopConfig` 上加 `Arc<dyn Hooks>` | M3 |
| `coding-agent/core/tools/*` | `agent-tools/src/builtin/fs.rs`：**核心工具** `read`、`write`、`edit`、`ls`、`bash`（PowerShell），全部限定在会话 cwd 内 | ✅（edit 已有单测） |
| `tools/grep.ts`、`find.ts` | `agent-tools/src/builtin/search.rs`：进程内嵌 ripgrep 引擎（`grep-searcher` / `grep-regex` / `ignore` / `globset`，不依赖外部 `rg`），结果按路径排序、输出有界 | ✅ |
| `file-mutation-queue.ts`、`truncate.ts` | `mutation_queue.rs` | M1 |
| `tools/index.ts` 的 `createCodingTools(cwd)` | `agent-tools/src/registry.rs`：`ToolRegistry::with_builtins()` 加上 `build(names, ToolContext{cwd})`；`DEFAULT_TOOLS = read, ls, grep, find, edit, write, bash`；预设有 `default`、`readonly`（read/ls/grep/find）、`no-shell`（再加 edit/write） | ✅ |
| `core/system-prompt.ts` + `resource-loader.ts` 的 AGENTS.md 部分 | `agentd/src/prompt.rs`：preamble 或自定义内容、工具列表、规则、append、AGENTS.md/CLAUDE.md（从 cwd 向上查找到 workspaceRoot 为止）、cwd | ✅ |
| `core/session-manager.ts` | `agent-session/src/lib.rs`：JSONL 树（`id`/`parentId`、叶子指针、`active_context` 含压缩）、header `meta` 与 `session_info` 元数据、按 id 查找、崩溃修复；`agentd/src/sessions.rs`：`ContextHooks` 逐条写入、按需恢复、列表/purge | ✅ 读写与恢复；fork/分支切换在 M2 |
| `core/compaction/*` | `agent-session/src/compaction.rs`（估计、切点、序列化与提示词，纯函数）+ `agentd/src/compaction.rs`（配置、摘要调用）；模型调用前经 `ContextHooks::before_call` 自动压缩，`POST /v1/sessions/{id}/compact` / RPC `compact` 手动压缩 | ✅ |
| `core/skills.ts`、`extensions/`、`prompt-templates.ts` | `agent-resources` crate（只做 skills 和 prompt templates；扩展改成 MCP 或回调） | M4 |
| `mcp/`（pi-mcp）、`core/mcp-servers.ts` | 已实现：`agentd/src/mcp.rs`（基于官方 SDK `rmcp`；stdio / streamable HTTP），按 profile 的 `mcpServers` 把 `mcp__<server>__<tool>` 加进会话工具 | M4 |
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
- **OpenAI Key（v0.3.1）**：`agent_ai::secrets` 负责加载，顺序为 `--openai-key-file` > `OPENAI_API_KEY_FILE` > 配置 `apiKeyFile` / `apiKeyKeychain`（v0.9.0，macOS Keychain，二选一）> `OPENAI_API_KEY` > 受管存储（v0.9.0，`sub-bot keys set` 写入的 Keychain 条目或 0600 key 文件；只在前几级都未命中时探测）。Anthropic 同理（`ANTHROPIC_*`）。macOS Keychain 条目（v0.10.3 起）经 Apple 的 `/usr/bin/security` 读/写/删（`agent_ai::keychain::security_tool`：绝对路径、不经 shell；写入走 `security -i` 的 stdin + `-X <hex>`，secret 不进 argv；先删后建再读回校验），条目 ACL 因此归属 `security` 工具而不是 sub-bot 二进制的 cdhash，`brew upgrade` 后不再弹授权对话框；`sub-bot keys reauth` 一次性迁移旧版本创建的条目；`keys list` 的属性搜索仍用 `security-framework`。读文件时去掉首尾空白；出错时只报路径和原因，不带内容。key 在内存里用 `Secret` 包装：`Debug`/`Display` 输出 `[redacted]`，drop 时清零，只在设置 `Authorization` 头时 `expose()`。任何响应、事件和日志里都不会出现 key，日志只记录来源。
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
- HTTP 提交、删除、清理使用同一个 map 锁，避免删除后旧 handle 再启动运行。DELETE 会取消正在运行的 Agent；清理与删除都保留已有 JSONL（`?purge=true` 才删除），之后按旧 ID 访问时从同一 client 的会话目录恢复（README「会话持久化与恢复」）。
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

### 模型选择与推理强度（v0.8.0 phase 1，未升版本）
- `ThinkingLevel` 保持 Copy：内置顺序 none（别名 off）< minimal < low < medium < high < xhigh < max；`Other(&'static str)` 接受 `[a-z0-9_-]{1,32}`，全局 Mutex<HashSet> 去重后 Box::leak。serde/Display 输出原始名称；Other 没有全局 rank，`rank_in` 只使用 provider 有序列表的位置。
- `agent-ai::models::{effort_caps, allowed_efforts, resolve_effort}` 是统一能力/策略入口；`EffortCaps` 带完整 levels、mechanism、default、max_output。OpenAI 精确匹配列出的 gpt-5/5.1..5.6/6.x（不对未来型号做前缀猜测），保留实测 o3 的 xhigh；Claude 去掉日期后缀匹配。文档来源 URL 留在代码注释中。
- `allowed_efforts` 取完整 levels 的最高三档（none/minimal 计数）；请求低于范围向上钳制，缺档/超高内置名称取最近允许档位（平局取高），未列出的 Other 取最高档并标记 `unknown level`。Unsupported 省略，Unknown 原样；未指定保持 None，不填入目录 default。`EffortResolution` 保留 requested/effective/adjusted，调整记 warn。
- Anthropic 已列出的 Full/NoXhigh 模型用 adaptive + output_config.effort（上限 128000）；opus-4-5 用 effort + enabled budget；sonnet/haiku-4-5 只用 budget，三档映射 8192/16384/32000（上限 64000）。未知 Claude 用 adaptive + effort 原样发送，上限 64000。None 不发 thinking/effort。Adaptive 默认 max_tokens 按有效档位 16000/16000/32000/64000/模型上限；显式额度优先；budget 模式至少 budget+4096；全部封顶模型上限。不发 temperature，保留 signed/redacted thinking 回放。
- 优先级仍为请求/CLI > profile > 全局默认，`allowedModels` 仍是模型白名单。Hub 只委托 agent-ai 解析；provider 也对直接调用执行策略。`ModelInfo` 保留完整 reasoningEfforts，增加 allowedEfforts/defaultEffort；`models` 用方括号标记允许三档并打印 default。
- OpenAI 运行时 `Supported values are: ...` 解析任意名称并保留顺序；内置名称按已知档位取最近（平局取高），无法定位的名称取最高档。缓存键（模型、API、是否带工具）和最多两次能力调整不变；auto 的工具冲突仍可切换 Responses。实际 API/effort 写入 AssistantMessage 元数据。
- `sub-bot probe-efforts --model <id> [--provider openai|anthropic] [--levels none,minimal,low,medium,high,xhigh,max,ultra] [--api chat|responses] [--json]` 只加载选定 provider 的正常配置/key，不创建 Hub/会话。`Request.raw_effort` 绕过目录、缓存、降级和瞬态重试；每档一次无工具短请求（OpenAI none 256、其余 1024，Anthropic 2048）。Anthropic 默认 adaptive，thinking 字段被 400 拒绝时重试一次 omit；`--thinking adaptive|omit|enabled` 固定变体。结果含 accepted/status/variant、脱敏后最多 300 字符 error、原序 supportedValues。CLI/faux/本地 HTTP 测试覆盖原样发送与回退。
- 仅 phase 1：不增加动态 effort HTTP/RPC API，不升版本。profile 的 maxOutputTokens、整次运行 tokenBudget 语义保持原有边界（Anthropic 额外应用上述模型预算规则）。

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
- `webhook::Deliverer` 实现完整重试：默认最多 6 次（含首次，webhookMaxAttempts 可覆盖），每次超时 10 秒（含错误响应体读取）。网络错误/超时/408/429/5xx 重试，其它 4xx 不重试；2xx 成功，3xx 不重试且不跟随重定向（Deliverer 自建禁重定向 client）。指数退避从 1 秒起逐次翻倍，加 0–10% 随机抖动，总等待上限 60 秒；429/503 的 `Retry-After` 优先（支持非负整数秒和 HTTP-date，过去日期为零，同样最多 60 秒），普通及恢复投递共用解析路径，无效值回退到指数退避。取消 delivery future 会停止请求和退避，不另起后台任务。
- 测试使用 faux provider、router oneshot 和本地 axum receiver，覆盖 202/Location、轮询/owner/TTL、poll-only、白名单、超时和有序事件投递、签名验证、关机及 secret 加载打码。

## (e) MVP 里程碑

| 里程碑 | 内容 | 验收 |
|---|---|---|
| **M0（已完成，已用 OpenAI 实测）** | 5 个 crate；双层 agent loop；OpenAI Chat Completions 流式调用；核心工具 read/write/edit/ls/bash；会话 cwd 与 workspaceRoots 校验；AGENTS.md 注入；JSONL 会话；HTTP、SSE、stdio RPC；每个 bot 一个 token 和 profile | `cargo check` 无警告；3 个单测通过；faux 冒烟测试通过（edit 改文件，越权 cwd 和工具被拒，401/404 隔离） |
| **M1：编码能力补齐** | ✅ OpenAI Responses API（v0.3）；✅ `grep`、`find`（内嵌 ripgrep，遵守 .gitignore）；file mutation queue；pi 的 `truncate` 输出截断策略；`read` 支持 offset/limit；edit 返回 diff | 用真实 key 跑一个“修 bug 并跑测试”的端到端任务 |
| **M1.5：Claude** | `agent-ai/src/anthropic.rs` 接入 Anthropic Messages API；profile 增加 `provider` 字段；Hub 持有多个 provider，按 profile 分派 | 先用离线单测覆盖 SSE 解析和请求体，有 key 后再做端到端测试 |
| **M2：会话** | ✅ 恢复会话（`sessionId` 重开后加载 JSONL，`run`/`rpc --resume`）；✅ `compaction`（自动压缩和 `compact` 命令）；fork 和分支切换；`set_model`；RPC 命令补齐 | 长会话超过上下文后能自动压缩并继续 |
| **M3：安全与治理** | `beforeToolCall` 审批钩子（profile 可设 bash 需要调用方确认，确认通过 SSE/RPC 往返）；bash 命令允许和拒绝规则；每个 bot 的限流和并发上限；审计日志；内建 TLS；keyring | 只读 profile 无法写文件，也无法执行命令；审批流程可用 |
| **M4：扩展** | MCP 客户端（stdio/HTTP）；skills 和 prompt templates；项目信任（对应 `project-trust.ts`）；调用方工具支持通过 RPC 回填 | 接入一个 MCP server 后能调用其工具 |
| **M5：分发** | cargo-dist 三平台六个目标；`service install`；按操作系统决定配置目录；安装文档 | 在 Win、mac、Linux 上一键安装并作为服务运行 |

---

## v0.5.0 issues

`config::Targets` 支持单 URL 和最多 10 项的混合数组，加载时解析相对 secretFile，生成 `ResolvedTarget`；alerts 全局路由，client targets 通过 session/task 订阅隔离。`Reporter` 为一次安装的 Arc 全局服务（无运行时测试/RPC 可用 noop）：同步脱敏后写有界 drop-oldest 队列，Notify 唤醒 worker；按 `(kind, component, session_id, client, task_id)` 做 Tokio 时钟窗口聚合，摘要 count 包括首条。JSONL + 最近 1000 条 ring 是本地 sinks，网络复用 v0.4 Deliverer，每目标独立任务、签名与重试，Semaphore 限制 32 个并发请求。

`report()` 只入队，drain 处理首条时写入 ring；同一窗口内每次抑制立即按首条 id 更新其 count（suppressed + 1）、可选 lastTs（最近一次 occurrence 的时间）和最高 severity，保留首条 ts/id。`recent()` / `GET /v1/issues` 因而每窗口只有一条 live aggregate，不必等待窗口结束；可见性以 worker 处理队列为界，since 仍按首条 ts 过滤。窗口结束、flush 或 dedup 容量淘汰的 webhook 摘要仍使用独立 id，count = suppressed + 1，但不再次写入 ring/JSONL，避免重复计数；severity 升级仍立即投递，也不新增本地条目。JSONL 在每次合并时追加相同 id 的完整快照，读取聚合视图应按 id 保留最后一条（count 是累计值，不相加）。ring 淘汰的条目不会因合并重新插入。Store 只持久化 task 和 webhook delivery 快照，不持久化独立 issues 查询表；不改写已发送/待重试的首条 delivery，后续 summary delivery 携带最终累计 count。

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
