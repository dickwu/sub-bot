# sub-bot

一个用 Rust 写的**本地编码 agent 服务**，架构仿照 [pi coding-agent](https://github.com/badlogic/pi-mono)。它内置 read / write / edit / ls / bash 等编码工具，不绑定具体项目，供其它 bot 通过 HTTP+SSE 或 stdio JSONL RPC 调用。

- 仅支持 macOS 和 Linux（Ubuntu 等），单个二进制，TLS 用 rustls，不依赖 OpenSSL
- 每个 bot 一个 token，token 映射到允许使用的 profile；会话互相隔离，每个会话有独立的工作目录（cwd）
- OpenAI / Anthropic API key 从文件、macOS Keychain 或环境变量读入进程内存，不会通过任何接口返回，也不会写进日志；`sub-bot keys set` 可代为保存 key
- 设计细节见 [DESIGN.md](DESIGN.md)

> 状态：v0.9.3（修复：Keychain 等待授权提示、可选账号加载失败、账号 failover 的 WARN 日志不再被重复上报为 `log_warn` issue，issues.log / webhook 每个事件只收到一条 typed issue）。v0.9.2（macOS Keychain 读取不再无限挂起：等待授权对话框超过 1 秒提示、`keychainTimeoutSecs`（默认 30 秒）后报明确错误；`/v1/chat` 顶层 `profile` / `model` 作为 `session.*` 的别名，未知字段返回 400）。v0.9.1（修复：只经由 profile 命名账号使用的 provider 不再要求其默认账号，`keys check` 同步；`serve` 启动时显示 key 配置错误原因而不是通用文案）。v0.9.0：新增 macOS Keychain key 来源（`apiKeyKeychain`）、`sub-bot keys set|list|delete|check`（macOS 存 Keychain，Linux 存 0600 受管 key 文件），以及同一 provider 多账号（`providers.<id>.accounts`、profile `account` / `allowedAccounts`、请求级 `account` 覆盖、OpenAI organization/project 请求头、429/额度耗尽时的账号 failover），见「密钥存储」与「同一 provider 多账号」。v0.8.1：OpenAI 默认模型 `gpt-6.1-sol`，内置配置默认只允许 `gpt-6.1-sol` / `gpt-6-astra`；`models` 标记 profile 允许的模型；推理强度调整只 warn 一次，`session_run` 日志记录路由后的实际模型。推理强度可在运行时按请求/会话动态修改（HTTP / CLI / RPC），Claude provider 支持 standard / deep / auto 模型路由（见「动态推理强度与模型路由」）。`ask` 同步支持 SSE 文本增量 / `--json` NDJSON；默认数据目录无 panic 回退并检查临时目录所有权；`/v1/issues` 即时更新去重计数。支持 Unix socket、本地身份认证、问题上报（issues）、多目标 webhook、异步任务、结果轮询和 webhook 投递。接入了 OpenAI Responses API 和 Chat Completions（按模型自动选择）。Claude（Anthropic）多 provider 配置见下文；会话默认持久化，可在重启后按 ID 恢复，长会话自动压缩上下文（见「会话持久化与恢复」「上下文压缩」）；grep/find 工具等功能见 DESIGN.md 的里程碑。

## Claude (Anthropic)

同一服务可同时加载 `openai`、`anthropic` 和离线 `faux`。例如在现有配置中加入：

```json
{
  "defaultProvider": "openai",
  "providers": {
    "openai": { "apiKeyFile": "keys/openai.key" },
    "anthropic": { "apiKeyFile": "keys/anthropic.key" }
  },
  "profiles": {
    "claude": {
      "provider": "anthropic",
      "model": "claude-sonnet-4-5",
      "reasoningEffort": "high",
      "workspaceRoots": ["./work"]
    }
  }
}
```

将 `claude` 加入调用方的 `clients[].profiles`。密钥文件建议 `chmod 600`；所有配置中的 `apiKeyFile` 相对路径都相对于配置文件目录。

- Anthropic key 优先级：`--anthropic-key-file` > `ANTHROPIC_API_KEY_FILE` > `providers.anthropic.apiKeyFile` / `apiKeyKeychain` > `ANTHROPIC_API_KEY` > 受管存储（`sub-bot keys set` 写入的位置）。
- OpenAI key 优先级：`--openai-key-file` > `OPENAI_API_KEY_FILE` > `apiKeyFile` / `providers.openai.apiKeyFile` / `providers.openai.apiKeyKeychain` > `OPENAI_API_KEY` > 受管存储。顶层 `apiKeyFile` 仍只代表 OpenAI，两个配置别名若同时设置必须一致；`apiKeyFile` 与 `apiKeyKeychain` 只能二选一。详见「API key」与「密钥存储：macOS Keychain 与 `sub-bot keys`」。
- 可选 `providers.<id>.baseUrl`；`OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL` 环境变量优先于配置。
- 选择顺序：会话 JSON `provider` > profile `provider` > 有效模型名以 `claude-` 开头时推断 Anthropic > 默认 provider。默认 provider 为显式 `--provider`（或 `SUB_BOT_PROVIDER`）> `defaultProvider` > `openai`。`run` / `rpc` / `ask` 的显式 `--provider` 也作为会话覆盖；已有 sessionId 的 ask 继续原会话。
- 启动必须能加载默认 provider 与全局默认模型推断出的 provider 的默认账号，以及每个 profile 解析出的 provider 下 profile 自己的账号（`account`，未写时为该 provider 的 `defaultAccount`）；缺少 key 即安全失败。只经由 profile 以命名 `account` 使用的 provider 不要求其默认账号。其它账号（其它 provider 声明了来源的 default 账号、未被 profile 引用的命名账号）加载失败只 warn 并上报 `account_unavailable`，无来源的 default 账号直接跳过。后续会话选择未加载 provider 返回 400，并提示配置来源；`faux` 始终可用。多账号见「同一 provider 多账号」。
- 未指定模型的 anthropic 会话默认用 `claude-opus-5-5`；`deepModel`（默认固定 `claude-fable-5-1`）/ `autoRoute` / `routerModel` 控制 standard / deep / auto 路由，见「动态推理强度与模型路由」。
- `api` / `defaultApi` 仅用于 OpenAI。Anthropic 按模型选择 adaptive effort、effort + extended-thinking budget 或纯 budget，详见「模型与推理强度」。

`sub-bot models --json` 与 `GET /v1/models` 合并所有已加载 provider 的目录（每个 provider 查其默认账号），每个模型都有 `provider` 与 `account` 字段。`models --provider anthropic` / `/v1/models?provider=anthropic` 只查询指定 provider；对话模型 / 日期快照过滤仅作用于 OpenAI，`--all` / `?all=true` 关闭过滤。`/v1/profiles` 返回每个 profile 解析后的 provider。LLM issue 的 scope 包含 provider，所有加载的 key 都加入 reporter 脱敏列表。

> Anthropic Messages API 已接入：支持 SSE 工具调用、签名 thinking 回放、prompt caching、分页模型目录及共享瞬态重试策略。

## 持久化、重启恢复与 issue 路由

仅 `serve` 打开持久化 journal store：异步 task 状态和待投递 webhook 自动写入 `<dataDir>/state` 下的 `tasks.jsonl` / `deliveries.jsonl`，不依赖 SQLite。`--data-dir PATH` 优先于配置；默认是系统数据目录的 `sub-bot`，不受 `--session-dir` 影响（会话 JSONL 默认在 `<dataDir>/sessions`，见「会话持久化与恢复」）。目录权限为 0700，文件为 0600；一个目录仅供一个服务实例使用。记录包含任务结果和 webhook payload，请按敏感业务数据保护。

默认目录按顺序选择：`BaseDirs::data_local_dir()/sub-bot`；系统目录不可用时使用非空且为绝对路径的 `$XDG_DATA_HOME/sub-bot`；否则 Unix 使用 `std::env::temp_dir()/sub-bot-<euid>`（非 Unix 为 `temp_dir()/sub-bot`）。回退时会警告一次，建议设置 `--data-dir` 或配置 `dataDir`（默认日志目录也使用该回退链，日志可用 `--log-dir` 单独指定）。临时回退目录按需以 0700 创建；Unix 上已有目录必须属于当前有效 UID，且不能允许组/其他用户写入，也不能是符号链接，否则 store、socket 和日志访问会返回错误，不会自动修复权限。临时目录可能被系统清理，持久服务应显式配置稳定的数据目录。

```json
{
  "dataDir": "/var/lib/sub-bot",
  "shutdownGraceSecs": 10
}
```

`run`、`rpc`、`models` 使用内存 store，不获取 dataDir 锁、不读取或写入这些 journal、不恢复任务或续投服务端 webhook，因此可与同一 dataDir 的 `serve` 并行运行。独立的日志输出和会话历史（`<dataDir>/sessions/<client>/`，每个会话一个文件，不需要锁）不受影响。

启动时压缩日志、清理超过 `taskTtlSecs` 的结果。最后一个不完整行会被忽略；完整行损坏时，原文件重命名为 `<name>.corrupt-<unix_ms>`（0600），恢复损坏行之前的记录，并上报 error issue `store_corrupt`。上次处于 queued/running 的任务不会重新执行工具，而是标记 failed，error 为 `interrupted by restart`，并上报 client/task 范围的 error issue `task_interrupted`。任务仍可由所属 client 通过 `GET /v1/tasks/{id}` 查询，直到 TTL 到期。

待投递 webhook 继续原来的 `deliveryId` 和尝试次数；接收端必须按 ID 去重（至少一次投递，不保证恰好一次）。日志不保存 webhook 签名密钥，重启时按 URL 从当前配置重新解析；目标已删除（包括未保留到配置的请求级 callback URL）则标记 failed 并上报 issue。请在当前 client 的 webhook 配置中保留需要恢复的 callback 目标。

SIGINT/SIGTERM 阻止新任务、取消运行中的 agent，并在 `shutdownGraceSecs`（默认 10 秒）内等待任务、issue reporter 和 webhook 投递队列；超时未完成的投递留在日志中，下次启动恢复。会话聊天历史逐条写入会话 JSONL，重启后按 ID 访问即可恢复（见「会话持久化与恢复」）；被中断的运行不会自动重新执行。

同步 `POST /v1/chat`、SSE 和会话运行也订阅所属 client 配置中接受 `issues` 的 webhook，遵守 `events` / `minSeverity`。运行结束后保留短暂订阅宽限期；取消订阅后路由缓存至少保留 `alerts.dedupWindowSecs`，已聚合 issue 还携带原始路由快照。因此窗口结束时的 `count > 1` 汇总仍到原 client，不会广播给其它 client（管理员全局 alerts 保持独立）。

## 会话持久化与恢复

### 存储位置与开关

`serve`、`run`、`rpc` 默认把每个会话写成一个 JSONL：`<sessionDir>/<client>/<UTC 时间>_<sessionId>.jsonl`。`<sessionDir>` 的选择顺序：`--no-persist-sessions`（不持久化）> `--session-dir` / `SUB_BOT_SESSION_DIR` > 配置 `persistSessions: false`（不持久化）> 配置 `sessionDir` > `<dataDir>/sessions`。

```json
{ "dataDir": "/var/lib/sub-bot", "sessionDir": null, "persistSessions": true }
```

- `<client>` 子目录：HTTP 为配置中的 client 名，`run` 固定为 `local`，`rpc` 固定为 `stdio`。`models`、`ask`、`keys` 等命令不读写会话目录。
- 目录 0700、文件 0600（与 dataDir 其它内容一致；临时回退 dataDir 同样检查属主和权限）。文件包含完整对话（工具输出、调用方覆盖的系统提示），请按敏感数据保护。
- 不自动清理：`DELETE /v1/sessions/{id}?purge=true` 删除单个会话，或自行定期清理（例如 `find <sessionDir> -name '*.jsonl' -mtime +30 -delete`）。`/v1/chat` 自动创建的临时会话同样落盘。
- 每条消息在加入上下文时立即追加写入（用户消息、助手消息、工具结果），不再等运行结束；进程崩溃最多丢失正在写的那一行。

文件格式沿用 pi 的会话树（`id` / `parentId`），向后兼容：
- header `{"type":"session","version":2,"id","profile","timestamp","meta":{...}}`；`meta` 记录重建会话所需的信息：`client`、`provider`、`account`、`model`、`explicitModel`、`api`、`cwd`、`requestedReasoningEffort`、`reasoningEffort`、`routing`、`tools`、`systemPrompt` / `appendSystemPrompt`（仅调用方覆盖时）、`systemPromptSha256`、`callerTools`（只有名字）、`ephemeral`、`title`（第一条用户消息预览）。
- `{"type":"session_info","id","timestamp","meta":{...}}`：元数据更新（模型路由确定、推理强度修改、标题），不进入树，后写覆盖先写。
- `{"type":"compaction",...,"summary","firstKeptEntryId","tokensBefore","tokensAfter"}`：见「上下文压缩」。
- v1 文件（没有 `meta` / `session_info` / token 字段）照常加载：profile 取 header，cwd 取 profile 的第一个 workspaceRoot，模型取最后一条助手消息。

### 重启后恢复（rehydrate）

HTTP 上任何会话路由（`GET/PATCH/DELETE /v1/sessions/{id}`、`/messages`、`/prompt`、`/events`、`/abort`、`/compact`，以及带 `sessionId` 的 `/v1/chat`）遇到不在内存里的 ID 时，会从**同一 client** 的会话目录加载它，然后照常处理；其它 client 的会话一律 404。ID 必须是规范的小写 UUID，否则直接 404、不访问磁盘（防止路径穿越）。成功时记 info 日志 `session rehydrated from disk`（`session_id`、`messages`、`compacted`、`repaired_tool_calls`、`elapsed_ms`）。

- 恢复的内容：上下文（压缩后的摘要 + 保留消息；Anthropic 签名 thinking 块与 tool_use / tool_result 配对原样保留）、profile、cwd（仍须位于 workspaceRoots 内）、provider / account、已确定的模型路由（不会重新路由，depth 不能再改）、请求的推理强度（按模型重新解析）、工具列表、调用方的系统提示覆盖、chat 会话的短 TTL。系统提示按当前 profile 与 AGENTS.md 重新构建（与创建时不同会记 info 日志）。之后的消息追加到同一个文件。
- 不恢复：调用方回调工具（`callbackUrl` / token 不落盘，记 WARN）、中断的运行（不会自动重新执行）。
- 恢复的会话计入 `maxSessionsPerClient`（满了返回 429）。不能恢复（文件损坏、profile / cwd / 模型 / 账号不再被允许、provider 未加载）返回 **409** `session cannot be resumed: ...`。
- **崩溃修复**：如果最后一条助手消息里有工具调用没有结果（例如工具执行中进程退出），恢复时为每个缺失的调用追加一条错误工具结果 `interrupted: sub-bot restarted before the tool finished`，记 WARN 并上报 warn 级 issue `tool_interrupted`；崩溃留下的不完整末行会被截掉（WARN）。
- 同一个会话不要同时在两个进程里使用（例如服务里的会话和 `run --resume` 同时写同一个文件）。

### CLI 与 RPC

```bash
sub-bot run --json "分析 src/ 的结构"          # {"sessionId":"…","persisted":true,"resumed":false,…}
sub-bot run "分析 src/ 的结构"                  # stdout 只有回复；stderr 打印 [session] <id>
sub-bot run --resume <id> "继续，补上测试"       # 读取 <sessionDir>/local/<…>_<id>.jsonl，追加写入
sub-bot rpc --resume <id>                       # <sessionDir>/stdio/；get_state 返回 sessionId / persisted
sub-bot ask --resume <id> "继续"                # --resume 是 --session 的别名；服务重启后由服务端按需恢复
```

`run --resume` / `rpc --resume` 时 profile、模型、provider、账号、depth 都来自记录（命令行同名参数被忽略），`--reasoning-effort` 作为粘性修改生效；`--no-persist-sessions` 或 `persistSessions: false` 时 `--resume` 直接报错。

### 列表与删除

```bash
curl -s 'localhost:7878/v1/sessions?limit=20' -H "Authorization: Bearer $T"
# {"sessions":[{"sessionId":"…","profile":"coder","provider":"anthropic","account":"default","model":"claude-opus-5-5","cwd":"/work/app",
#   "createdAt":1767225600000,"updatedAt":1767229200000,"messageCount":12,"inMemory":false,"running":false,"persisted":true,"ephemeral":false,
#   "title":"给 src/lib.rs 加单元测试…"}]}
curl -X DELETE "localhost:7878/v1/sessions/$SID" -H "Authorization: Bearer $T"              # {"deleted":true,"inMemory":true,"purged":false}
curl -X DELETE "localhost:7878/v1/sessions/$SID?purge=true" -H "Authorization: Bearer $T"   # 同时删除 JSONL：{"deleted":true,"inMemory":false,"purged":true}
```

- 列表合并内存会话与磁盘会话（按 ID 去重，内存优先），按 `updatedAt` 倒序，`limit` 默认 50（1–1000）；时间为 Unix 毫秒，磁盘会话的 `updatedAt` 是文件修改时间，`messageCount` 是当前上下文（压缩后）的消息数。列表只读文件，不会加载会话。
- `DELETE` 默认只删内存会话（运行中会被中止），JSONL 保留，之后按 ID 仍可恢复；只在磁盘上的会话也返回 200（`inMemory:false`）。`?purge=true` 同时删除 JSONL（之后 404）。

## 上下文压缩

长会话在接近模型上下文窗口时自动压缩：较早的消息被模型摘要成结构化总结，最近的消息原样保留（参照 pi coding-agent `core/compaction`）。

### 配置

```jsonc
{
  "compaction": {                 // 全局默认
    "enabled": true,              // 自动压缩开关（手动 compact 不受影响）
    "thresholdPct": 80,           // 估计 token > contextWindow × 80% 时压缩（1–100）
    "keepRecentTokens": 20000,    // 原样保留的最近消息量（token 估计，0–1000000）
    "model": null,                // 摘要用的模型（同一 provider），null = 会话当前模型
    "maxSummaryTokens": 4000      // 摘要调用的输出上限（256–64000）
  },
  "profiles": {
    "coder": {
      "contextWindow": 200000,              // 可选：覆盖内置窗口表（>= 1000）
      "compaction": {"thresholdPct": 70}    // 可选：逐字段覆盖全局设置
    }
  }
}
```

内置上下文窗口（保守值）：Claude（已列出的 4.x / 5.x）200k；gpt-5.x / gpt-6.x 400k；gpt-4.1 1M；o 系列 200k；其它 128k。窗口包含输出，阈值留出了回复空间。

### 触发与估计

每次模型调用之前（包括同一次运行中工具调用之后的调用）估计上下文 token：最近一条成功助手消息报告的用量（`totalTokens`，即 input + cacheRead + cacheWrite + output，OpenAI 的 input 已含缓存）加上之后消息的 chars/4 估计；没有可用用量（或用量来自压缩之前）时用系统提示 + 工具声明 + 全部消息的 chars/4。估计值超过阈值且存在可压缩的部分时压缩。

### 算法

- **切点**：从最新消息往回累计约 `keepRecentTokens`。只在用户消息或助手消息之前切，绝不切在工具结果上，切点之后的每个工具结果，其工具调用也必须在切点之后（不拆开 tool_use / tool_result）。优先切在用户消息边界（保留完整回合）；预算点之后没有用户边界时才切在助手消息之前（该回合开头的用户消息进入摘要）。整个上下文不超过 `keepRecentTokens` 或没有可切的位置时不压缩。
- **摘要**：用会话模型（或 `compaction.model`）做一次不带工具的调用，固定的摘要系统提示，要求输出结构化总结：Goal、Constraints & Preferences、Progress（Done / In Progress / Blocked）、Key Decisions、Files Touched、Open Questions、Next Steps、Critical Context。被摘要的消息序列化成纯文本：thinking 截断后作为纯文本，**签名和 redacted thinking 不会发给摘要模型**；工具结果截断到 2000 字符；附带从 read/edit/write 调用里提取的文件列表。已有摘要时把它放进 `<previous-summary>` 一并更新，摘要是累积的。
- **新上下文** = [摘要用户消息（`The conversation history before this point was compacted into the following summary: <summary>…</summary>`）] + 保留的消息（原样不动，包括带签名的 thinking 块）。JSONL 里旧消息保留，追加一条 `compaction` 条目（`firstKeptEntryId` = 保留部分第一条消息），重启后恢复得到同样的压缩后上下文。
- 摘要调用的用量在 compaction 结束事件的 `usage` 里，并计入 `run --json` 的 `totalUsage`。
- **自动压缩失败**：记 WARN、上报 warn 级 issue `compaction_failed`（details：`reason`、`model`、`tokensBefore`、脱敏后的错误摘要），本次调用不压缩、照常继续（若随后超出 provider 的上下文，仍按原样报 `llm_context_overflow`）；60 秒内不再自动重试。abort 会取消进行中的摘要调用。

### 手动压缩

```bash
curl -X POST localhost:7878/v1/sessions/$SID/compact -H "Authorization: Bearer $T" -d '{"instructions":"重点保留失败测试的信息"}'
# {"compacted":true,"tokensBefore":152340,"tokensAfter":23810,"summaryChars":3120,"firstKeptEntryId":"3f9a1c2e","durationMs":8421,"usage":{…}}
# RPC
{"id":"9","type":"compact","instructions":"重点保留失败测试的信息"}
```

请求体可省略。会话正在运行 ⇒ **409**（RPC 为 `success:false`）；没有可压缩的内容 ⇒ 200 `{"compacted":false,…}`；摘要调用失败 ⇒ **502**（上下文不变）。手动压缩期间会话视为运行中：新消息排队，压缩结束后按顺序执行。

### 事件与日志

SSE、RPC、`ask --json` 的事件流上：

```json
{"type":"compaction","reason":"auto","phase":"start","tokensBefore":165000}
{"type":"compaction","reason":"auto","phase":"end","tokensBefore":165000,"tokensAfter":24100,"durationMs":9120,"summaryChars":3402,"firstKeptEntryId":"3f9a1c2e","usage":{"input":141000,"output":900,"cacheRead":0,"totalTokens":141900,"reasoning":0}}
{"type":"compaction","reason":"manual","phase":"end","tokensBefore":52000,"durationMs":1200,"error":"HTTP 500: …"}
```

`reason` 为 `auto` / `manual`；失败时 `end` 带 `error`。`run` 文本模式在 stderr 打印 `[compaction] auto: ~165000 -> ~24100 tokens`。日志：`auto compaction triggered`、`compaction started`、`context compacted`（info：tokens_before / tokens_after / summary_chars / duration_ms）、`compaction failed; continuing without compaction`（WARN）。相关 issue：`compaction_failed`（warn）、`tool_interrupted`（warn）、`session_persist_failed`（error，JSONL 写入失败）。

## LLM 重试与进度

OpenAI Responses / Chat Completions 遇到连接失败、超时、流中断、HTTP 408/429/5xx 时自动重试。
全局或 profile 可设置 `llmMaxRetries`（默认 3 次额外尝试，0 禁用；profile 优先）。退避从 1 秒指数增长，加入 jitter，上限 30 秒；服务端 `Retry-After`（秒或 HTTP-date）作为最短等待时间（先限制到最多 60 秒）。abort 会立即取消请求或等待。
仅文本/思考的部分输出可以丢弃后重试；一旦发出工具调用事件就不重试，避免重复工具操作。SSE/JSONL 消费者收到 `message_retry {attempt, reason}` 时应清空当前未完成的助手消息；最终历史只含成功尝试。每次重试产生 warn `llm_retry` issue，最终失败仍产生 `llm_*` issue。

`task.progress` 不发送空白助手文本（工具执行进度仍保留）。Webhook 的 `Retry-After` 同时支持秒和 HTTP-date，过去日期按 0 秒处理，最长等待 60 秒。

## Install

### Homebrew

支持 macOS / Linux，arm64 / x86_64：

```bash
brew install dickwu/tap/sub-bot
```

示例配置安装在 `$(brew --prefix)/share/sub-bot/sub-bot.example.json`。

Formula 由 release workflow 自动更新；需要在本仓库配置 secret `HOMEBREW_TAP_TOKEN`，并授予它对 `dickwu/homebrew-tap` 的 push 权限。未配置时会跳过 tap 更新，不影响 Release 发布。也可以对本地 tap checkout 手动运行（版本号不带 `v`）：

```bash
scripts/update-tap.sh 0.4.0 /path/to/homebrew-tap
```

脚本下载 Release 的四个平台校验文件并生成 `Formula/sub-bot.rb`，不执行 commit 或 push。

### 手动安装 / 源码构建

从 [Releases](https://github.com/dickwu/sub-bot/releases) 下载对应平台的压缩包，并用 `.sha256` 校验：

| 平台 | 文件 |
|---|---|
| Linux x86_64 / arm64 | `sub-bot-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` / `sub-bot-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz` |
| macOS Intel / Apple Silicon | `sub-bot-vX.Y.Z-x86_64-apple-darwin.tar.gz` / `sub-bot-vX.Y.Z-aarch64-apple-darwin.tar.gz` |

解压后进入顶层目录，把 `sub-bot` 放到 PATH 里，并保留 `sub-bot.example.json` 作为配置模板。macOS 首次运行如果被拦截，执行 `xattr -d com.apple.quarantine sub-bot`。

从源码构建：`cargo build --release -p agentd`

## Local bots without tokens

本机 bot 可以通过 Unix domain socket 调用同一套 HTTP/SSE API，不必传 API token。Linux/macOS 的内核 peer credentials 提供 uid/gid/pid；`localClients` 将 uid 或用户名映射到已有 client，继续使用该 client 的 profile 权限和会话隔离。

将这些字段合并到现有配置（`dev-bot` 必须已在 `clients` 中定义；原有 `profiles` / `tokenSha256` 配置不变）：

```json
{
  "dataDir": "/home/peilin/.local/share/sub-bot",
  "unixSocket": "/home/peilin/.local/share/sub-bot/sub-bot.sock",
  "unixSocketMode": "0600",
  "localClients": [
    {"uid": 501, "client": "dev-bot"},
    {"username": "peilin", "client": "dev-bot"}
  ]
}
```

- `serve --unix-socket [<path>]` 开启 UDS；省略路径时用 `<dataDir>/sub-bot.sock`。`--data-dir` 覆盖配置 `dataDir`；没有覆盖时使用系统数据目录下的 `sub-bot`。路径不展开 `~`（CLI 可以用 shell 展开的 `~/...`），相对路径相对于进程工作目录。
- Linux 默认：`~/.local/share/sub-bot/sub-bot.sock`（遵循 XDG 数据目录）。macOS 默认：`~/Library/Application Support/sub-bot/sub-bot.sock`。
- TCP 默认仍是 `127.0.0.1:7878`，与 UDS 共用 router；`--bind none` 只启用 UDS。
- Socket 默认权限 `0600`；可设 `"unixSocketMode": "0660"` 供有权限的组使用（还要允许访问父目录，并管理好 socket 的组）。新建父目录默认 `0700`。启动只清理残留 socket，拒绝覆盖普通文件、符号链接或活动 listener；正常关闭时移除 socket。
- 每个 `localClients` 条目只能选择 `uid`、`username`、`"any": true` 之一，按配置顺序首个匹配生效。`{"any": true, "client": "dev-bot"}` 信任所有能访问 socket 的本地 uid。没有匹配时仍需 Bearer token。
- 可选 `"trustLoopback": {"client": "dev-bot"}`（默认关闭）允许 TCP loopback 无 token 请求，包括 `127.0.0.1`、`::1` 和 IPv4-mapped loopback。启用时启动日志会 WARN。**不信任 `X-Forwarded-For`**；代理连接按代理本身的 socket 地址判断，不能把不可信代理放在受信的 loopback 前面。
- **Authorization header 始终优先**：有效 Bearer token 覆盖本地身份；错误 token 或其他认证格式返回 401，不回退到本地信任。引用不存在的 client 会导致启动失败。

**安全取舍：任何以映射 uid 运行的进程，都能充当该 client；启用 `trustLoopback` 后，任何本地进程都能充当指定 client。** 这不是同一 uid 下不同 bot 的安全隔离。请保持 socket 和父目录权限严格，只映射可信用户；`any`/组共享会扩大信任范围。远程请求仍需要 token；不要把 loopback 信任当成反向代理后的远程认证。

```bash
# 使用配置中的身份映射；同时监听 TCP 和默认 UDS
sub-bot --config sub-bot.json serve --unix-socket
# 或仅监听 UDS
sub-bot --config sub-bot.json serve --unix-socket --bind none

curl --unix-socket ~/.local/share/sub-bot/sub-bot.sock http://localhost/v1/chat \
  -H 'Content-Type: application/json' -d '{"message":"请检查当前项目","session":{"profile":"coder"}}'
# macOS curl: --unix-socket "$HOME/Library/Application Support/sub-bot/sub-bot.sock"

sub-bot ask "请检查当前项目"
sub-bot ask --json "逐事件输出 NDJSON"
sub-bot ask --profile coder --async --json "运行测试并总结"
printf '继续上次的任务' | sub-bot ask --session SESSION_ID -   # --resume 是 --session 的别名；服务重启后同样有效
sub-bot ask --socket /path/to/sub-bot.sock "hello"
sub-bot ask --url http://127.0.0.1:7878 --token-file /path/to/token "hello"
```

`ask` 不启动模型/provider，也不读取 OpenAI key。默认优先使用数据目录中的默认 socket（若存在），否则连接 `http://127.0.0.1:7878`；自定义 socket 路径或 TCP 端口需显式传 `--socket` / `--url`（二者互斥）。`--config` 中的 `dataDir` 和 `--data-dir` 同样影响默认 socket 查找。Token 可来自 `--token-file`，否则读取 `SUB_BOT_TOKEN`，不会打印。默认同步请求使用 SSE，助手文本增量到达即写入并 flush stdout，结束时换行（工具事件不打印）。同步 `--json` 即时输出 NDJSON：第一行是从响应头 `x-session-id` 取得的 `{"type":"session","sessionId":"<id>"}`，可将该 ID 传给后续 `ask --session <id>` 继续会话（旧服务端未返回此头时省略该行）。之后每行一个服务端原始 JSON 对象，包含文本、工具、错误，最后一行仍是服务端最终 `agent_settled` 事件，不是最终响应对象；多行 SSE data 合并为单行 JSON。Unix socket 与 TCP 行为一致，纯文本 stdout 不变。`--async` 使用异步任务 API 并轮询到最终结果后打印最终文本，`--async --json` 仍打印完整最终 task 对象。HTTP/模型失败、abort 或流在 settled 前断开会向 stderr 报错并返回非零退出码，stdout 可能已包含部分输出。`--timeout <secs>`（默认 3600）限制从发出请求到取得最终结果的总等待时间，包括异步轮询和网络等待；超时返回明确错误和非零退出码，但不会自动取消服务端任务。

## 问题上报 (issues)

v0.5.0 将运行异常统一为 `IssueEvent`，写入日志目录下的 **`issues.log`（JSONL）**，并可投递到多个告警接收器。Webhook 沿用 v0.4 的 HMAC-SHA256 签名、独立 delivery ID、超时与重试；事件名为 **`issue`**，信封 `data` 是以下对象：

```json
{
  "id": "uuid", "ts": 1740000000000, "severity": "error",
  "kind": "tool_timeout", "component": "tool", "message": "operation timed out",
  "requestId": "optional", "client": "dev-bot", "sessionId": "optional",
  "taskId": "optional", "profile": "coder", "model": "optional", "tool": "bash",
  "details": {}, "count": 1
}
```

`ts` 为 Unix 毫秒；severity 为 `warn | error | fatal`。作用域字段和 `tool` 可省略。
运行侧来源（I2 接入）包括：LLM 错误、工具失败/超时、run timeout / max_turns / token budget、abort、持久化 I/O、广播 lag、关闭时仍有在途工作、429 容量拒绝、调用方工具 callback 失败。核心还接入 webhook 最终投递失败、启动/配置失败、panic，以及本项目 crate 的 WARN/ERROR 日志（跳过 reporter/webhook 自身，防止循环）。

### 配置

```json
{
  "alerts": {
    "webhookUrl": [
      "https://ops.example/alerts",
      {"url": "https://backup.example/alerts", "secretFile": "keys/backup", "minSeverity": "fatal"}
    ],
    "webhookSecretFile": "keys/alerts",
    "minSeverity": "error",
    "dedupWindowSecs": 60,
    "queueCapacity": 1000
  },
  "clients": [{
    "name": "dev-bot", "tokenSha256": "<sha256>", "profiles": ["coder"],
    "webhookSecretFile": "keys/client",
    "webhookUrl": [
      "https://bot.example/completed",
      {"url": "https://bot.example/events", "secretFile": "keys/events",
       "events": ["task.completed", "task.progress", "issues"], "minSeverity": "warn"}
    ]
  }]
}
```

- `webhookUrl` 保留单字符串兼容，也接受字符串/对象混合数组；每个列表最多 **10** 项，只允许无 URL 用户凭据的 HTTP/HTTPS 地址。
- `secretFile` 相对于配置文件目录，必须可读且非空；未指定则继承该 client/alerts 的 `webhookSecretFile`。密钥内容不进入 Debug。
- Client 条目省略 `events` 时只选 `task.completed`；必须显式列出 `issues` 才订阅问题，默认问题阈值 `warn`。订阅按 client + session/task 隔离。
- 全局 `alerts` 不受 client 作用域限制，忽略条目 `events`；默认阈值 `error`，条目 `minSeverity` 可覆盖。
- 请求 `callbackUrl` 的数组解析、任务订阅与 `GET /v1/issues` 由 I2 HTTP 接入；请求不能提供 `secretFile`，使用 client 默认 secret，所有 URL 必须匹配允许的 host。配置中的所有 webhook host 都隐式允许；请求 targets 替换该任务的默认 targets。

### CLI 与保证

```bash
sub-bot --config sub-bot.json issues test --severity error --message 'alert pipeline test'
sub-bot --config sub-bot.json issues tail -n 20
```

`issues test` 不需要初始化 LLM provider，向**所有** alert targets 发送一个 `test` 问题（诊断时忽略阈值），等待投递结果，只打印 host、state、attempts；任意失败退出非零。`issues tail` 打印 `issues.log` 最后 N 行。`--log-dir -` 禁用文件落盘，但不禁用告警。

- `report()` 不等待网络或文件 I/O；队列默认 1000，满时丢弃最旧项，随后输出带丢弃计数的 `issues_dropped`。内存保留最近 1000 个已处理问题。
- 同 `(kind, component, sessionId, client)` 在默认 60 秒窗口内首次立即发出，其余合并；窗口结束的摘要 `count = 被抑制次数 + 1`，**包含已经发出的首条，不应重复相加**。关闭 flush 会提前输出待发摘要。
- 各目标独立签名、delivery ID 和重试，最多 32 个网络投递并发；慢目标不阻塞其它目标/问题队列。失败生成 `webhook_delivery_failed`，仅去 alerts 和 JSONL，不回到订阅者；失败问题自身的投递不会递归生成问题。外部失败回调只有 host，保守排除同 host 的 alert targets；issue 内部失败可精确排除原 URL。
- 安全约定：来源不得传入 prompts/bodies/工具原文；details 递归剔除 prompt/content/body/text/messages/arguments/output 与密钥字段，掩盖 `sk-…`、Bearer、GitHub tokens、敏感 key=value 和已加载的 webhook secrets。message 最多 500 字符，details 超过 2 KiB 替换为 `{"truncated":true}`。不要将原始输入输出拼入自由文本 message；模式脱敏不能识别任意无标记的私密文本。
- JSONL I/O 失败只计数，不使服务失败；启动失败的同步告警最多等 5 秒，正常退出 flush 最多等 5 秒。panic 仅 best-effort 入队并调用原 hook；进程被强杀、超时退出或磁盘故障时不保证投递。问题不是持久化消息队列，接收端仍需按 delivery ID 幂等处理。

## 快速开始

```bash
sub-bot keys set --provider openai   # 推荐：无回显输入 key；macOS 存 Keychain，Linux 写 ~/.config/sub-bot/keys/openai-default.key（0600），无需改配置
# 或者自己管理 key 文件（600 权限），不进入环境变量，详见下文「API key」
mkdir -p ~/.config/sub-bot && install -m 600 /dev/null ~/.config/sub-bot/openai.key && $EDITOR ~/.config/sub-bot/openai.key
export OPENAI_API_KEY_FILE=~/.config/sub-bot/openai.key   # 或 --openai-key-file / 配置 apiKeyFile；也仍支持 OPENAI_API_KEY
sub-bot hash-token "$(openssl rand -hex 24)"   # 记下 token 原文给 bot，把输出的哈希填进配置
cp sub-bot.example.json sub-bot.json   # 编辑 profiles / clients
sub-bot --config sub-bot.json serve --bind 127.0.0.1:7878
```

不带 `--config` 时会使用单 client、单 `default` profile 的模式，以 `--workspace`（默认当前目录）为根，并自动将服务进程的有效 uid 映射到 client `default`。运行 `sub-bot serve --unix-socket` 后，同一用户即可用 `sub-bot ask "hello"` 零 token 调用；同一 uid 下的所有进程都获得该身份。TCP 仍需通过 `SUB_BOT_TOKEN` 或 `serve --token` 指定调用凭据；省略时生成的随机 token 不会打印。显式配置文件不会自动添加 uid 映射，需要自行配置 `localClients` 或 `trustLoopback`。

离线试用（不调用 OpenAI）：加上 `--provider faux`。

**默认 OpenAI 模型**：默认模型是 `gpt-6.1-sol`；推荐的 OpenAI 模型只有 `gpt-6.1-sol` 与 `gpt-6-astra`。不带 `--config` 且 provider 为 OpenAI 时，内置 `default` profile 的 `allowedModels` 只允许这两个（`--model` 显式指定的其它模型会自动加入）。需要其它 OpenAI 模型时，在配置文件 profile 的 `allowedModels` 中显式添加（或不填 `allowedModels` 表示不限制）。

`serve` 收到 Ctrl-C 或 SIGTERM 时停止接受新会话/提交，取消正在运行的会话，等运行收尾并写入 JSONL 后退出。

## 配置（`sub-bot.json`）

```jsonc
{
  "defaultModel": "gpt-6.1-sol",            // 默认 gpt-6.1-sol
  "sessionIdleTtlSecs": 3600,                 // 普通会话空闲 TTL，0 禁用
  "chatSessionTtlSecs": 600,                  // /v1/chat 自动创建会话的空闲 TTL，0 禁用（独立于普通 TTL）
  "maxSessionsPerClient": 64,                // 每个 client 的内存会话上限，0 不限制
  "sessionDir": null,                         // 会话 JSONL 目录（按 client 分子目录），默认 <dataDir>/sessions
  "persistSessions": true,                    // false 关闭会话持久化（serve / run / rpc），见「会话持久化与恢复」
  "compaction": {                             // 上下文压缩默认值，profile 的 compaction 逐字段覆盖，见「上下文压缩」
    "enabled": true, "thresholdPct": 80, "keepRecentTokens": 20000, "model": null, "maxSummaryTokens": 4000
  },
  "taskTtlSecs": 3600,                       // 已完成任务保留秒数（从完成时计时）；0 在下次清理时过期
  "webhookMaxAttempts": 6,                   // webhook 最大投递次数（含首次）
  "defaultReasoningEffort": "high",           // 可选：全局默认推理强度
  "defaultApi": "auto",                       // 可选：auto|chat|responses，默认 auto
  "streamIdleTimeoutSecs": 300,               // LLM 流连续无字节的超时；推理模型可能长时间静默
  "chatTimeoutSecs": 900,                     // /v1/chat 非流式默认等待上限
  "keychainTimeoutSecs": 30,                  // 每次 macOS Keychain 读取的等待上限（>= 1），见「Keychain 读取超时」
  "apiKeyFile": "/etc/sub-bot/openai.key",    // 可选：OpenAI key 文件，相对路径相对配置文件目录
  "profiles": {
    "coder": {
      "workspaceRoots": ["/home/me/code"],     // 会话的 cwd 必须位于这些目录之内
      "tools": ["read", "ls", "edit", "write", "bash"],  // 白名单，默认就是这些；也可写 "preset:readonly" / "preset:no-shell"
      "model": "gpt-6.1-sol",                  // profile 默认模型
      "allowedModels": ["gpt-6.1-sol", "gpt-6-astra"],  // 可选：bot 能切换到的模型（支持 * 通配），不填则不限制；其它 OpenAI 模型需在此显式添加
      "reasoningEffort": "high",               // 可选：none|minimal|low|medium|high|xhigh|max 或 provider 小写名称
      "api": "auto",                           // 可选：auto|chat|responses，覆盖 defaultApi
      "maxOutputTokens": 8000,                 // 可选：单次模型调用的输出上限
      "tokenBudget": 300000,                   // 可选：整次运行的 token 预算，用完后禁止再调工具、要求直接给结论
      "runTimeoutSecs": null,                 // 可选：整次运行的秒数上限，默认不限时；超时等同 abort
      "systemPrompt": null,                    // 替换默认编码前言
      "appendSystemPrompt": "...",             // 追加内容
      "loadContextFiles": true,                // 注入 cwd 往上的 AGENTS.md / CLAUDE.md
      "allowSystemPromptOverride": false,      // 是否允许 bot 在请求里覆盖或追加系统提示
      "allowCallerTools": false,               // 是否允许 bot 注册回调工具
      "maxTurns": 50,
      "contextWindow": null,                   // 可选：覆盖模型上下文窗口（token，>= 1000），用于自动压缩阈值
      "compaction": {"thresholdPct": 70},      // 可选：覆盖全局 compaction 的部分字段
      "deepModel": "claude-fable-5-1",         // 可选：depth=deep 用的模型；anthropic profile 默认固定为 claude-fable-5-1；"latest:claude-fable" 跟随最新版
      "autoRoute": false,                      // 可选：true 时默认 depth=auto（第一次 prompt 前用 routerModel 分类）
      "routerModel": null,                     // 可选：分类用的模型，默认 = profile 基础模型
      "shellEnv": {                            // 可选：bash 子进程的环境变量策略
        "mode": "inherit",                     // inherit（默认，继承后剔除）| allowlist（清空后只保留基础变量和 allow）
        "deny": ["AWS_*", "GITHUB_TOKEN"],     // 额外剔除，支持 * 通配
        "allow": []                            // inherit 下豁免 deny；allowlist 下额外保留
      }
    }
  },
  "clients": [
    {
      "name": "dev-bot", "tokenSha256": "<sub-bot hash-token 的输出>", "profiles": ["coder"],
      "webhookUrl": "https://bot.example/hooks/sub-bot", // 可选：默认结果投递地址，只允许 http/https
      "webhookSecretFile": "keys/webhook.key",          // 可选：HMAC key；相对配置文件目录，加载时读取并 trim
      "webhookAllowedHosts": ["other.example", "localhost:8080"] // 可选：允许请求覆盖的目标 host 或 host:port
    }
  ]
}
```

- **`sub-bot.example.json` 里的 `tokenSha256` 是占位符，必须替换**。配置里只存 token 的 SHA-256，不存原文；加载时要求恰好 64 个十六进制字符，并拒绝未知 profile 引用、空 `workspaceRoots`、未知工具或预设。
- 全局参数（也可以用环境变量设置）：
  - `--config`（`SUB_BOT_CONFIG`）
  - `--provider openai|anthropic|faux`
  - `--model`（`SUB_BOT_MODEL`）：`serve` 时覆盖全局默认模型；`rpc` / `run` 时指定本会话的模型
  - `--reasoning-effort` / `--effort`（`SUB_BOT_REASONING_EFFORT`）：同上，取值 `none|minimal|low|medium|high|xhigh|max` 或 provider 小写名称；`ask` 时作为 `/v1/chat` 顶层 `reasoningEffort` 发送（粘性）
  - `--api`（`SUB_BOT_API`）：`auto|chat|responses`，`serve` 时覆盖 `defaultApi`，`rpc` / `run` 时指定本会话
  - `--session-dir`（`SUB_BOT_SESSION_DIR`）：会话 JSONL 的持久化目录，按 bot 分子目录存放；优先于配置 `sessionDir`，默认 `<dataDir>/sessions`
  - `--no-persist-sessions`：不持久化会话（同 `persistSessions: false`），优先于 `--session-dir` / `SUB_BOT_SESSION_DIR`
  - `--openai-key-file`：从文件读取 OpenAI API key（见「API key」）
  - `--keychain-timeout-secs`（`SUB_BOT_KEYCHAIN_TIMEOUT_SECS`）：覆盖 `keychainTimeoutSecs`（>= 1），对 `serve` / `run` / `rpc` / `keys check` / `keys list` 等所有读取 Keychain 的命令生效
  - `--log-level`、`--log-dir`、`--log-format`

## 模型与推理强度

```bash
sub-bot models                       # 合并已加载 provider 的模型；OpenAI 默认去掉非对话模型和日期快照；行首 * = 被 profile 的 allowedModels 允许
sub-bot models --all --json          # 全部模型，JSON 输出
sub-bot --config sub-bot.json models --profile coder   # 只列 coder 的 allowedModels 允许的
sub-bot --model gpt-6.1-sol --reasoning-effort high run "解释一下这个仓库的结构"
sub-bot --model gpt-6-astra --reasoning-effort xhigh run --json "say hi"
```

`models` 显示 provider、模型 id、完整推理档位和 `default`；方括号标出允许的最高三档，例如 `low,medium,[high,xhigh,max] default=medium`。`-` 表示不支持，`?` 表示未知。JSON 保留完整 `reasoningEfforts`，新增 `allowedEfforts` 和 `defaultEffort`。

不带 `--profile` 时，若已加载配置中 OpenAI 相关 profile 设置了 `allowedModels`，文本输出在被任一 profile 允许的模型行首标 `*`（stderr 摘要注明 `* = allowed by profiles`，`--all` 同样标记）；`--json` 每个模型带布尔字段 `allowed`（任一 profile 允许即 true；没有任何 profile 限制时都为 true）。`--profile` 仍只列该 profile 允许的模型。

- 名称支持 `none`（别名 `off`，内置名称大小写不敏感）、`minimal`、`low`、`medium`、`high`、`xhigh`、`max`，以及 `[a-z0-9_-]{1,32}` 的未来 provider 名称（如 `ultra`）。未知名称不假定全局排序，只采用 provider 列表的顺序。
- 模型优先级：请求/命令行 > profile > `defaultModel`；`allowedModels` 仍是模型白名单。
- 强度优先级：请求/命令行 > profile > `defaultReasoningEffort`；均未设置则不发 effort/thinking，让 provider 使用默认值（目录中的 default 仅作说明，不自动填入）。
- **最高三档规则**：已知模型只允许完整目录中最高的三档，`none`/`minimal` 也计为档位。不足三档则全部允许。低于范围向上钳制；缺失/超高的内置名称取最近允许档位，平局取高；不在目录里的未知名称取最高档并标记 `unknown level`。例如 gpt-5 的 minimal → low，gpt-6-astra 的 low → high，sonnet-4-6 的 xhigh → max。所有调整记 warn；不支持的模型省略参数。
- 表外模型原样发送。OpenAI 400 的 `Supported values are: ...` 保留原始顺序和未知名称，缓存按（模型、API、是否带工具）区分；内置名称按列表中已知档位取最近（平局取高），无法定位的名称（如未列出的 `ultra`）取列表最高档重试，最多两次。
- Anthropic 4.7+/已列出的 5.x 用 `thinking: adaptive` + `output_config.effort`（low/medium/high/xhigh/max）；opus/sonnet-4-6、mythos-preview 同机制但没有 xhigh。opus-4-5 用 effort + enabled budget；sonnet/haiku-4-5 仅 enabled budget（low/medium/high → 8192/16384/32000），不发 effort。日期后缀不影响目录匹配。未知 Claude 用 adaptive + effort 原样传递。
- Adaptive 默认输出额度按**有效**档位为 low/medium 16000、high 32000、xhigh 64000、max 为模型上限；显式 `maxOutputTokens` 优先，最终不超过模型上限（已知 adaptive 128000、4.5/未知 64000）。Budget 机制还保证输出额度至少 budget+4096。启用 thinking 不发 temperature；签名 thinking 回放保持不变。

### 实测能力（不经过钳制）

2026-11-02 用 `probe-efforts` 对账号内全部模型逐档实测（none…max + ultra）：

| 模型 | 接受的档位 | 最高三档 |
|---|---|---|
| gpt-5 / 5-mini / 5-nano | minimal, low, medium, high | low, medium, high |
| gpt-5.1 | none, low, medium, high | low, medium, high |
| gpt-5.2 / 5.4 / 5.4-mini / 5.4-nano / 5.5 | none … xhigh | medium, high, xhigh |
| gpt-5.6-luna/sol/terra, gpt-6-luna/sol, gpt-6.1-terra | none, low … max | high, xhigh, max |
| gpt-6-astra, gpt-6.1-sol | low … max | high, xhigh, max |
| o3 | low, medium, high | low, medium, high |
| chat-latest | medium | medium |
| gpt-4.x / 3.5 | 不支持 reasoning_effort | - |
| claude fable-5/5-1, opus-4-7/4-8/5/5-5, sonnet-5/5-5, haiku-5-5 | low … max | high, xhigh, max |
| claude opus-4-6, sonnet-4-6 | low, medium, high, max | medium, high, max |
| claude opus-4-5 | low, medium, high（effort + budget） | low, medium, high |
| claude haiku-4-5 | 不支持 effort（仅 budget） | low, medium, high（映射为 budget） |

`ultra` 两家 API 都不接受：OpenAI 返回 `Supported values are: 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', and 'max'`，Anthropic 返回 `Input should be 'low', 'medium', 'high', 'xhigh' or 'max'`。`max` 在两家都高于 `xhigh`。


```bash
sub-bot probe-efforts --model gpt-6-astra --api responses --openai-key-file keys/openai.key
sub-bot --config sub-bot.json probe-efforts --model claude-sonnet-5-5 --provider anthropic --levels high,xhigh,max,ultra --json
sub-bot probe-efforts --model claude-opus-4-5 --thinking enabled --anthropic-key-file keys/anthropic.key
```

默认探测 `none,minimal,low,medium,high,xhigh,max,ultra`。每档一次无工具的 `Reply with OK`，绕过目录、运行时降级/缓存和瞬态重试。OpenAI 输出上限 none 为 256，其它档位 1024；Anthropic 为 2048。Anthropic 默认 adaptive；若 HTTP 400 指向 thinking 字段，再试一次 omit。显式 `--thinking adaptive|omit|enabled` 固定变体，不自动回退。沿用正常 key-file/config/base URL 解析，仅加载选定 provider。每档一行，或 `--json` 数组 `{model,provider,level,accepted,status,variant,error,supportedValues}`；错误先脱敏再截取 300 字符，未取得 HTTP 状态时为 null。探测失败也会继续后续档位；这是能力诊断，不是质量评估。运行时修改 effort 见下文「动态推理强度与模型路由」。

## 动态推理强度与模型路由（v0.8.0）

### 动态推理强度

每个会话保存一个**请求的**推理强度（`requestedReasoningEffort`）和按当前模型解析后的**实际**值（`reasoningEffort`）。所有修改都经过 `agent_ai::models::resolve_effort`（已知模型只允许目录中最高三档，超出时钳制并记 warn，原因写在 `effortAdjusted`）。修改是**粘性的**：从这条消息起一直生效，并且从**下一次模型调用**开始生效——运行中修改也一样（例如当前工具调用结束后的那次调用）。名称语法非法返回 400。

```bash
# 新建会话时指定
curl -X POST localhost:7878/v1/sessions -H "Authorization: Bearer $T" -d '{"profile":"claude","reasoningEffort":"high"}'
# 会话中途修改（运行中也可以），返回 200 + 会话视图
curl -X PATCH localhost:7878/v1/sessions/$SID -H "Authorization: Bearer $T" -d '{"reasoningEffort":"max"}'
# 随消息修改（粘性）
curl -X POST localhost:7878/v1/sessions/$SID/prompt -H "Authorization: Bearer $T" -d '{"message":"继续","reasoningEffort":"xhigh"}'
# /v1/chat 顶层字段：作用于新会话或 sessionId 指定的已有会话
curl -X POST localhost:7878/v1/chat -H "Authorization: Bearer $T" -d '{"message":"hi","sessionId":"'$SID'","reasoningEffort":"high"}'
# CLI：--effort 是 --reasoning-effort 的别名
sub-bot ask --effort high "解释这个仓库"
# RPC
{"id":"7","type":"set_reasoning_effort","level":"low"}
← {"id":"7","type":"response","command":"set_reasoning_effort","success":true,"data":{"requested":"low","effective":"medium","adjusted":"clamped to allowed top-three levels","allowedEfforts":["medium","high","max"]}}
```

每次修改在事件流上广播 `{"type":"effort_changed","requested":..,"effective":..,"adjusted":..}`，并记 `reasoning effort changed` info 日志（session_id / requested / effective）。

### Claude 模型路由（depth）

- **默认模型**：provider 为 anthropic 且请求和 profile 都没有指定模型时使用 `claude-opus-5-5`；`providers.anthropic.defaultModel` 可覆盖，profile `model` 再覆盖它（全局 `defaultModel` 本身是 `claude-*` 时也视为已配置）。
- **profile 字段**：`deepModel`（anthropic profile 默认**固定**为 `claude-fable-5-1`，不会自动跟随新版本）、`autoRoute`（默认 false）、`routerModel`（默认 = profile 基础模型）。想跟随最新版可写 `"deepModel": "latest:claude-fable"`：在 provider 模型目录（Hub 缓存，最多每 10 分钟刷新）中取该前缀下版本号最新的 id（`claude-fable-5-1` > `claude-fable-5`；同版本存在无日期 id 时忽略 `-20260101` 这类快照），记 `deep model alias resolved` 日志；解析失败时使用基础模型并在 `routing.reason` 中说明原因。
- **depth**：`standard`（基础模型）| `deep`（deepModel）| `auto`（分类器决定）。默认 `autoRoute ? auto : standard`。可在 `POST /v1/sessions`、`/v1/chat`（顶层或 `session` 内）、首个 prompt 之前的 `PATCH /v1/sessions/{id}`、`POST .../prompt`（仅首个）、`ask --deep` / `ask --depth <d>`、`rpc --depth <d>` / `run --depth <d>`、RPC `prompt` 的 `depth`（仅首个）中指定。路由在创建会话或**第一次 prompt** 时决定，之后**不会重新路由**；首个 prompt 之后再给 depth 返回 409（RPC 返回 `success:false`）。
- **优先级**：调用方显式 `model` 总是胜出（depth 被忽略，reason `explicit model`）> depth。
- **auto 分类器**：第一次 prompt 前用 routerModel 做一次便宜调用（允许时用 low 推理强度、否则不发；最多 200 输出 token、无工具、30 秒超时），固定系统提示要求只回答 `{"deep":true|false,"reason":"<=20 words"}`；回复中查找 JSON 对象。出错/超时/无法解析时使用 standard，reason 为 `router error: ...`，并上报 warn 级 issue `routing_error`。分类调用的用量单独记录在 `routing.usage`，并计入 `run --json` 的 `totalUsage`。没有配置 deepModel 时不调用分类器。
- 选定模型后按该模型的能力重新解析推理强度（例如 profile `medium` 在 fable 上会被钳制到 `high`），并广播 `{"type":"model_routed","routing":{...}}` 事件、记 `model routed` 日志（session_id / depth / model / reason）。

### 报告字段

会话视图（`POST/GET/PATCH /v1/sessions[/{id}]`）、`/v1/chat` 响应、task `result`、`run --json`、`ask --json`（最后一行 `{"type":"session_state",...}`）和 RPC `get_state` 都包含：

```json
{
  "model": "claude-fable-5-1",
  "reasoningEffort": "high",
  "requestedReasoningEffort": "medium",
  "effortAdjusted": "clamped to allowed top-three levels",
  "allowedEfforts": ["high", "xhigh", "max"],
  "routing": {"depth": "auto", "model": "claude-fable-5-1", "baseModel": "claude-opus-5-5", "deepModel": "claude-fable-5-1",
              "auto": true, "reason": "router: open-ended design task", "usage": {"input": 120, "output": 18, "...": 0}, "decided": true}
}
```

`/v1/models` 与 `sub-bot models --json` 的每个模型带 `allowedEfforts`（最高三档）和 `defaultEffort`；`/v1/profiles` 返回每个 profile 的 `reasoningEffort`、`allowedEfforts`、`defaultEffort` 和 `routing: {deepModel, autoRoute, routerModel, defaultDepth}`。

### 用哪个 API（`api`）

- `auto`（默认）：gpt-5.4 及以后、gpt-6.x、o 系列走 **Responses API**（`POST /v1/responses`，流式 SSE，`function_call` / `function_call_output`，`reasoning.effort`，usage 含 reasoning tokens），其它模型走 Chat Completions
- 优先级：请求/命令行的 `api` > profile 的 `api` > `defaultApi`
- 2026-10 实测：gpt-5.4 及以后在 Chat Completions 上不允许“函数工具 + reasoning_effort”，gpt-6.x 在 Chat Completions 上带工具完全不可用。`auto` 下遇到这种错误会自动切到 Responses 并记住；显式 `api: "chat"` 时只会去掉 reasoning_effort 重试一次（记 warn），仍然失败就直接报错并提示改用 `responses`
- `run --json` 输出 `sessionId`、`persisted`、`resumed`、`api`、`reasoningEffort`（**实际生效**的值）、`requestedReasoningEffort`（请求的值）、`usage`（最后一次调用）和 `totalUsage`（整次运行累计，含 `reasoning`、路由分类与压缩摘要调用）
- 日志里 `llm` span 的 `api` / `reasoning_effort` 是实际发送的值（`-` 表示未发送）

## 日志

- 级别：`--log-level info`（或 `SUB_BOT_LOG`）；`RUST_LOG=agentd=debug,agent_ai=trace` 可以按模块细调
- stderr 输出人类可读格式；文件日志按天滚动（`sub-bot.YYYY-MM-DD.log`，保留 14 天），默认为 JSON 格式（`--log-format text` 可改）
- 默认目录（`--log-dir <dir>` 可改，`--log-dir -` 关闭文件日志）：
  - Linux：`~/.local/share/sub-bot/logs`
  - macOS：`~/Library/Application Support/sub-bot/logs`
- 每行日志都带 span：`request_id`（沿用请求的 `X-Request-Id`，也会回写到响应头）/ `bot` / `session_id` / `profile` / `model` / `reasoning_effort` / `tool`
- 不会记录 API key、bot token 或回调 token；提示词、输出和文件内容只记录长度

## 其它 bot 如何调用

### HTTP + SSE

所有 `/v1/*` 接口（除 `/v1/health` 外）都需要认证：默认是 `Authorization: Bearer <bot token>`，也可以显式配置上面的 UDS uid 映射或 loopback 信任。

| 方法和路径 | 说明 |
|---|---|
| `GET /v1/health` | 健康检查（无需鉴权） |
| `GET /v1/profiles` | 当前 bot 可用的 profile（含 `model`、`allowedModels`、`reasoningEffort`、`allowedEfforts`、`defaultEffort`、`routing`） |
| `GET /v1/models?profile=&all=&provider=` | 实时模型列表（按 provider 缓存 10 分钟），每项带 `provider` / `reasoning` / `reasoningEfforts` / `allowedEfforts` / `defaultEffort`；`profile` 只返回该 profile 允许的 |
| `GET /v1/sessions?limit=` | 列出自己的会话（内存 + 已持久化，按 ID 去重，按 `updatedAt` 倒序，`limit` 默认 50、最大 1000）：`{sessions: [{sessionId, profile, provider, account, model, cwd, createdAt, updatedAt, messageCount, inMemory, running, persisted, ephemeral, title}]}`，时间为 Unix 毫秒 |
| `POST /v1/sessions` | 新建会话 `{profile, cwd?, model?, provider?, api?, account?, reasoningEffort?, depth?, tools?, systemPrompt?, appendSystemPrompt?, callerTools?}`（未知字段返回 400），返回 201 + 会话视图 `{sessionId, profile, cwd, provider, model, api, isStreaming, tools, persisted, reasoningEffort, requestedReasoningEffort, effortAdjusted, allowedEfforts, routing}` |
| `GET /v1/sessions/{id}` | 会话视图（同上）及运行状态；非 owner 或未知会话返回 404。所有 `/v1/sessions/{id}/*` 路由和带 `sessionId` 的 `/v1/chat` 在会话不在内存时会从磁盘恢复，见「会话持久化与恢复」 |
| `PATCH /v1/sessions/{id}` | `{reasoningEffort?, depth?}`：修改推理强度（下一次模型调用生效，运行中也可）；depth 仅在第一次 prompt 之前（否则 409）。返回 200 + 会话视图 |
| `DELETE /v1/sessions/{id}?purge=` | 删除自己的内存会话并中止运行，返回 `{"deleted":true, "inMemory":bool, "purged":bool}`；默认保留磁盘 JSONL（之后仍可按 ID 恢复），`?purge=true` 同时删除 JSONL；未知/非 owner 返回 404 |
| `POST /v1/sessions/{id}/prompt` | `{message, mode?: "steer"\|"follow_up", reasoningEffort?, depth?}`，返回 202；`reasoningEffort` 粘性，`depth` 仅首个 prompt |
| `GET /v1/sessions/{id}/events` | SSE 事件流 |
| `POST /v1/sessions/{id}/abort` | 中止当前运行 |
| `GET /v1/sessions/{id}/messages` | 消息历史（当前上下文；压缩后第一条是摘要消息） |
| `POST /v1/sessions/{id}/compact` | 立即压缩上下文，可选 `{instructions}`；返回 `{compacted, tokensBefore, tokensAfter, summaryChars, firstKeptEntryId, durationMs, usage}`；运行中 409，摘要调用失败 502 |
| `POST /v1/chat` | 一次性调用：`{message, sessionId?, session?, stream?, timeoutSecs?, async?, callbackUrl?, webhookEvents?, reasoningEffort?, depth?, account?, profile?, model?}`；顶层 `reasoningEffort` / `depth` 作用于新会话或 `sessionId` 指定的会话（depth 仅首个 prompt 之前）；顶层 `profile` / `model` 是 `session.profile` / `session.model` 的别名（见下文）；响应带 `model`、推理强度字段和 `routing` |
| `GET /v1/tasks/{id}` | 异步任务状态/结果/投递状态；未知、已过期或非 owner 返回 404 |

`/v1/chat` 请求体校验（v0.9.2）：
- 顶层 `profile` / `model` 等价于 `session.profile` / `session.model`（新会话使用；带 `sessionId` 时与 `session` 一样被忽略）。两处都给且值不同 ⇒ 400 `profile given both at top level and in session; put it in session.profile`（`model` 同理）；相同则接受。以前顶层 `profile` 会被静默忽略并落到默认 profile。
- 其它未知顶层字段 ⇒ 400。只能放在 `session` 里的会话选项（`provider`、`cwd`、`systemPrompt`、`appendSystemPrompt`、`api`、`tools`、`callerTools`）报 `unknown field 'cwd'; session options go under "session": {...}`；其它字段报 `unknown field '<name>'; expected one of: message, sessionId, session, ...`。错误只包含字段名，不回显字段值。
- `session` 对象（以及 `POST /v1/sessions` 请求体）里的未知字段同样返回 400（serde 报 ``unknown field `profiel`, expected one of ...``），拼写错误不再被静默忽略。
- 顶层 `reasoningEffort` / `depth` / `account` 的语义不变（`account` 顶层优先于 `session.account`）。

超时行为：
- `streamIdleTimeoutSecs`（全局，默认 300 秒）适用于 OpenAI Chat Completions 和 Responses；每次收到字节重置计时。超时产生 `stopReason: "error"`、`errorMessage: "stream idle timeout after Ns"`。连接超时为 30 秒，初始响应头等待上限为 120 秒。
- `runTimeoutSecs`（profile，默认 `null`）限制整次运行（含工具和排队消息），对 HTTP、RPC、CLI 都生效。超时取消运行、记录 `run timed out` 警告，并发送 `agent_settled{aborted:true}`。
- 非流式 `/v1/chat` 的 `timeoutSecs` 覆盖全局 `chatTimeoutSecs`（默认 900 秒）。超时 abort 后最多再等 5 秒收尾，返回 HTTP **504**：`{sessionId, error: "timeout", text, messages}`，保留已收到的部分文本和消息。`stream:true` 不使用此等待上限（仍受流空闲和运行超时限制）。

会话在创建、提交消息、运行结束时刷新空闲计时；读取信息/历史或订阅 SSE 不刷新计时。`serve` 每隔最多 60 秒（更短的非零 TTL 优先）清理超过各自 TTL 且未运行的会话，并记录日志；运行中的会话不会被空闲清理。删除/过期仅释放内存中的会话，JSONL 文件保留在磁盘，之后按旧 ID 访问会自动恢复（`DELETE ?purge=true` 才删除文件）。

每个 client 最多保留 `maxSessionsPerClient` 个会话（包括 chat 自动创建的会话）。达到上限时先清理该 client 已过期的空闲会话；仍满则创建返回 HTTP **429** `{"error":"too many sessions..."}`，可主动 DELETE 腾出名额。其它创建参数错误仍返回 400。

`/v1/chat` 不带 `sessionId` 时自动创建临时会话，使用独立的 `chatSessionTtlSecs`（默认 10 分钟），而非普通会话的 `sessionIdleTtlSecs`（默认 1 小时）。可用返回的 `sessionId` 继续对话；`stream:true` 时 ID 在响应头 `X-Session-Id` 中。继续对话会刷新计时，但不改变会话 TTL。三个配置字段都是顶层字段；两个 TTL 的 `0` 各自表示禁用过期，上限的 `0` 表示不限数量。

```bash
# 一次性调用，等待结果
curl -s localhost:7878/v1/chat -H "Authorization: Bearer $BOT_TOKEN" -H 'content-type: application/json' \
  -d '{"message":"给 src/lib.rs 加单元测试并运行 cargo test","session":{"profile":"coder","cwd":"my-project","model":"gpt-6-astra","reasoningEffort":"high"}}'

# 查询可用模型
curl -s 'localhost:7878/v1/models?profile=coder' -H "Authorization: Bearer $BOT_TOKEN"

# 流式返回：SSE 事件，直到 agent_settled
curl -sN localhost:7878/v1/chat -H "Authorization: Bearer $BOT_TOKEN" -H 'content-type: application/json' \
  -d '{"message":"列出文件","session":{"profile":"coder"},"stream":true}'
```

SSE 的 `event:` 名称与 pi 一致，依次有：
- `agent_start`
- `turn_start`
- `message_start` / `message_update`（`assistantMessageEvent`：`text_delta`、`toolcall_start/delta/end`、`done`、`error`）/ `message_end`
- `tool_execution_start` / `tool_execution_end`
- `turn_end`
- `agent_end`
- `agent_settled{aborted}`：本次运行结束的标志
- `compaction{reason, phase, tokensBefore, ...}`：上下文压缩开始/结束（见「上下文压缩」）

### 异步任务与 webhook（v0.4.0）

```bash
curl -i localhost:7878/v1/chat -H "Authorization: Bearer $BOT_TOKEN" -H 'content-type: application/json' \
  -d '{"message":"运行测试并总结","async":true,"callbackUrl":"https://bot.example/hooks/sub-bot","webhookEvents":"all"}'
# HTTP 202; Location: /v1/tasks/<id>
# {"taskId":"...","sessionId":"...","status":"running","statusUrl":"/v1/tasks/..."}
curl -s localhost:7878/v1/tasks/<id> -H "Authorization: Bearer $BOT_TOKEN"
```

- `async:true` 立即返回，客户端断开不取消任务；不能同时设置 `stream:true`。继续已有会话可传 `sessionId`；否则与同步 chat 一样创建临时会话。已有会话的排队/settled 语义与同步 chat 相同：结果覆盖本次订阅直到会话 settled 的消息。
- `callbackUrl` 覆盖 client 的 `webhookUrl`。仅接受 http/https（不允许 URL 用户名/密码）。覆盖地址的 host 必须匹配 `webhookAllowedHosts` 或默认 `webhookUrl` 的 host，否则 HTTP 400 `callback host not allowed`。`host` 允许任意端口，`host:port` 限定端口（省略端口时使用协议默认值）；不跟随 HTTP 重定向。默认 URL 的 host 隐式允许任意端口。请只允许可信目标，生产环境使用 HTTPS。
- 两个 URL 都未设置时仍能异步执行，只轮询、不投递。密钥文件不可读或 trim 后为空会阻止配置加载；密钥不进入 Debug、任务 JSON 或日志。webhook 日志不得含完整 URL、签名或正文。
- 任务 JSON 包括 `taskId`, `owner`, `sessionId`, `status`, `createdAt`/`finishedAt`（Unix 毫秒）, `result`, `error`, `delivery`。状态为 `running | succeeded | failed | timeout | aborted`；运行中结果/完成时间为 null。`result` 与同步 chat 响应一致，含 `text`, `messages`，正常结束还含 `usage`, `stopReason`, `error`, `aborted`。超时按 `timeoutSecs` / `chatTimeoutSecs` 取消运行，保留部分文本，任务状态为 `timeout`（轮询本身仍返回 200）。
- 任务与会话独立保留，完成后 `taskTtlSecs`（默认 3600 秒）由 sweeper 清理，运行中的任务不清理。任务和待投递队列仅在内存中，重启不恢复；关机中不接受新 chat，已有任务取消并收尾。投递不阻塞任务完成，进程退出不保证排空所有 webhook。
- `webhookEvents` 为 `final`（默认）或 `all`。完成（包括失败/超时/取消）发送 `task.completed`，`data` 为含结果的任务快照。`all` 另发送 `task.progress`：工具结束时 `{taskId,toolName,toolCallId,isError}`，assistant 消息结束时 `{taskId,text}`。每个任务使用一个队列按序投递，最终事件最后发送。`delivery` 是最近一次投递状态：`deliveryId,event,state`（pending/delivered/failed）, `attempts,lastStatus,lastError`；最终事件正文是投递前快照，最新投递状态请轮询。

Webhook 使用 `POST`、`Content-Type: application/json`，正文：

```json
{"deliveryId":"uuid","event":"task.completed","timestamp":1700000000,"data":{"taskId":"...","result":{"text":"...","messages":[]}}}
```

请求头：`X-SubBot-Event`、`X-SubBot-Delivery`（UUID）、`X-SubBot-Timestamp`（Unix 秒）、`User-Agent: sub-bot/0.4.0`。配置 secret 时另带 `X-SubBot-Signature: t=<秒>,v1=<小写十六进制 HMAC-SHA256>`；签名输入是 `"<t>."` 的 UTF-8 字节加**原始 HTTP 正文字节**（不要解析再序列化）。`t` 取自签名头，与 `X-SubBot-Timestamp` 相同；每次尝试重新取当前时间并签名，而 envelope 的 `timestamp`、delivery id 和原始正文在重试间保持不变，不能用正文里的时间戳代替 `t`。接收端可这样验证：

```python
import hashlib, hmac, time

def verify(secret: bytes, header: str, raw_body: bytes, tolerance=300):
    try:
        t, signature = header.split(",v1=", 1)
        if not t.startswith("t="):
            return False
        timestamp = t[2:]
        if abs(int(time.time()) - int(timestamp)) > tolerance:
            return False
        expected = hmac.new(secret, timestamp.encode() + b"." + raw_body,
                            hashlib.sha256).hexdigest()
        return hmac.compare_digest(expected, signature)
    except (ValueError, TypeError):
        return False
```

接收端尽快返回 2xx，并按 `X-SubBot-Delivery` 去重（重试使用相同 delivery id；每条 progress/final 事件各有自己的 id）。默认最多 6 次尝试（含首次），`webhookMaxAttempts` 可覆盖次数；每次尝试超时 10 秒（含错误响应体读取）。网络错误、超时、HTTP 408/429/5xx 会重试，其它 4xx 不重试；2xx 成功，3xx 不跟随重定向且不重试。指数退避从 1 秒开始逐次翻倍，加 0–10% 随机抖动，总等待上限 60 秒。429/503 响应的 `Retry-After` 若为非负整数秒，则优先使用（仍限制为最多 60 秒）；不支持 HTTP-date 格式，无效值回退到指数退避。

### stdio JSONL RPC（父进程直接拉起）

```bash
sub-bot --config sub-bot.json rpc --profile coder
```

stdin 每行一个命令，stdout 每行一个响应或事件（格式与 pi 的 `--mode rpc` 一致）：

```
→ {"id":"1","type":"prompt","message":"列出当前目录的文件"}
← {"id":"1","type":"response","command":"prompt","success":true,"data":{"disposition":"started"}}
← {"type":"tool_execution_start","toolName":"ls",...}
← {"type":"agent_settled","aborted":false}
```

模型和推理强度用全局参数指定：`sub-bot --model gpt-6.1-sol --reasoning-effort high rpc --profile coder`；模型路由用 `rpc --depth standard|deep|auto`。`rpc --resume <sessionId>` 恢复 `<sessionDir>/stdio/` 下的会话（ID 可从 `get_state` 取得）；ID 不存在时第一行输出 `{"type":"response","command":"start","success":false,...}` 并退出。

支持的命令：`prompt`（可选 `reasoningEffort`（粘性）、`depth`（仅首个 prompt））、`compact`（`{"instructions"?: string}` → 与 HTTP compact 相同的结果；运行中 `success:false`）、`set_reasoning_effort`（`{"level":"high"}` → `{requested, effective, adjusted, allowedEfforts}`）、`steer`、`follow_up`、`abort`、`get_messages`、`get_state`（含 `sessionId`、`persisted`、`model`、`reasoningEffort`、`requestedReasoningEffort`、`effortAdjusted`、`allowedEfforts`、`routing`）、`get_available_models`。**关闭 stdin 等于关机**：请等收到 `agent_settled` 后再关闭。

## API key

sub-bot 按以下顺序查找 OpenAI **default 账号**的 key，取第一个命中的（来源存在但读取失败会直接报错，不会继续往下找）。命名账号只看账号块里声明的来源和受管存储，见「同一 provider 多账号」：

1. `--openai-key-file <路径>`
2. `OPENAI_API_KEY_FILE` 环境变量（指向文件）
3. 配置文件里声明的来源：`apiKeyFile`（文件）或 `providers.openai.apiKeyKeychain`（macOS Keychain），二选一
4. `OPENAI_API_KEY` 环境变量
5. 受管存储：`sub-bot keys set` 写入的位置——macOS 是 Keychain 条目 `sub-bot` / `openai/default`，Linux 是 `~/.config/sub-bot/keys/openai-default.key`（遵循 `$XDG_CONFIG_HOME`）。只在前 4 级都没有时才探测，所以 `keys set` 之后不写配置也能直接用；仅对启动必需的 provider 探测（不会为了“是否配置”去读 Keychain）。

key 文件只放 key 本身，首尾空白和换行会被去掉；文件读不到、为空或中间含空白时会明确报错，但报错不会包含文件内容。文件权限对 group/other 可读时会打一条警告，建议 `chmod 600`。日志只记录 key 的来源（变量名、路径或 Keychain 的 service/account 名，例如 `key_source=keychain service=sub-bot account=openai/default`），不记录 key；无论来源是什么，加载的 key 都会加入 issue 上报的脱敏列表。`ANTHROPIC_API_KEY` / `ANTHROPIC_API_KEY_FILE` 使用同一套机制，优先级见上方 Claude (Anthropic) 节。

**bash 工具的环境变量**：bash 子进程启动时总会剔除 `OPENAI_API_KEY`、`ANTHROPIC_API_KEY`、`SUB_BOT_TOKEN` 及对应的 `*_FILE` 变量，`allow` 也无法放行。其它变量由 profile 的 `shellEnv` 决定：默认 `inherit` 会继承其余变量；`allowlist` 只保留 `PATH`、`HOME`、`USER`、`LOGNAME`、`SHELL`、`LANG`、`LC_*`、`TERM`、`TZ`、`TMPDIR` 等基础变量和 `allow` 里的变量。如果你的环境里还有别的密钥（如 `AWS_*`、`GITHUB_TOKEN`），请写进 `deny`，或者直接用 `allowlist`。

**局限（同用户）**：这只能阻止 key 通过环境变量直接传给子进程。bash 和 sub-bot 以同一个系统用户运行，所以：

- 如果 key 来自环境变量，子进程仍可以读 `/proc/<sub-bot 的 pid>/environ`（Linux），里面是 sub-bot 启动时的环境，进程内 `unsetenv` 也清不掉；
- 如果 key 来自文件，子进程可以直接读这个文件（只要知道路径）；
- 内容中含有 key 的其它环境变量（不是上面这些变量名）不会被识别，需要自己加进 `deny` 或用 `allowlist`。

因此**推荐用 key 文件**（不出现在任何进程的环境里），文件放在 `workspaceRoots` 之外。要真正隔离，请让 sub-bot 以单独的系统用户运行，或者把 bash 放进容器/沙箱。

## 密钥存储：macOS Keychain 与 `sub-bot keys`（v0.9.0）

除了手工维护 key 文件，sub-bot 可以自己保存 key：macOS 上存进登录钥匙串（Keychain，generic password），Linux 上写成权限 0600 的 key 文件。所有 `keys` 子命令都**绝不输出 key**：stdout、stderr、`--json` 和错误信息里只有路径、变量名或 Keychain 的 service/account 名。`keys` 不启动服务、不构造会话，也不创建日志目录（诊断只写 stderr）。

`--provider` 与 `--config` 是全局参数，放在 `keys` 前后都可以。

### 用 `sub-bot keys set` 保存 key

```bash
sub-bot keys set --provider openai                                  # 终端里交互输入，不回显
pbpaste | sub-bot keys set --provider anthropic                     # 管道：从 stdin 读到 EOF
sub-bot keys set --provider openai < ~/Downloads/openai.txt         # 重定向同理
sub-bot keys set --provider openai --file /etc/sub-bot/openai.key   # 写到指定文件（任何平台）
```

- **TTY 输入**：stdin 是终端时，在 stderr 提示 `Enter OpenAI API key for account 'default' (input hidden): `，关闭回显读取一行（支持退格、Ctrl-U 清空；Ctrl-C 取消，终端设置在任何退出路径上都会恢复）。不依赖 rpassword，直接用 termios。
- **管道 / 重定向输入**：读取全部 stdin。去掉首尾空白后必须恰好是一段不含空白的文本；多行或中间有空格报 `stdin must contain only the key`，为空报 `no key provided`。两种错误都不回显内容，读入的缓冲区用完即清零。不要把 key 写在命令行参数里（会进 shell 历史和 `ps`），`keys set` 也不接受位置参数。
- **存储位置**：`--file PATH` > macOS Keychain（service `sub-bot`，account `<provider>/<account>`，已存在则更新）> Linux 受管文件 `~/.config/sub-bot/keys/<provider>-<account>.key`（遵循 `$XDG_CONFIG_HOME`）。
- **Linux 文件权限**：受管目录以 **0700** 创建；已存在的目录必须属于当前用户、不是符号链接、group/other 无任何权限，否则拒绝写入并提示 `chmod 700`（不会擅自改权限）。key 文件是 **0600**：先在同目录写一个 0600 临时文件并 `fsync`，再 `rename` 覆盖目标（**原子写**，不会留下写了一半的 key）；目标是符号链接时拒绝覆盖。`--file` 的父目录不存在时同样以 0700 创建。
- `--service NAME` 只在 macOS 可用（Linux 上报 `--service is only supported on macOS`），且不能和 `--file` 同时使用；`--quiet` 只打印第一行、不打印配置片段；`--account NAME` 默认 `default`，命名账号的片段写成 `accounts.<name>`（见「同一 provider 多账号」）。`keys` 的 `--account` 不读 `SUB_BOT_ACCOUNT`。
- 成功后打印可直接粘贴的配置片段。macOS：

```text
Saved OpenAI key for account 'default' to keychain (service=sub-bot, account=openai/default).
Add to your config:
  "providers": { "openai": { "apiKeyKeychain": {} } }
(Without this config entry the key is still found automatically as the last-resort source for account 'default'.)
```

Linux：

```text
Saved OpenAI key for account 'default' to /home/u/.config/sub-bot/keys/openai-default.key (mode 0600).
Add to your config:
  "providers": { "openai": { "apiKeyFile": "/home/u/.config/sub-bot/keys/openai-default.key" } }
(Without this config entry the key is still found automatically as the last-resort source for account 'default'.)
```

写入受管位置（默认 service 的 Keychain 条目或受管目录）时，不加配置也能被「API key」一节的第 5 级来源找到；写进配置则来源更明确，并且优先级高于 `OPENAI_API_KEY`。`--file` 或自定义 `--service` 写入的 key 必须按片段写进配置才会被使用。

### 配置 `apiKeyKeychain`（仅 macOS）

```jsonc
{
  "providers": {
    // {} = service "sub-bot"，account "openai/default"（即 `sub-bot keys set --provider openai` 写入的条目）
    "openai": { "apiKeyKeychain": {} },
    // 指向自己创建的任意条目；service / account 都可以单独省略
    "anthropic": { "apiKeyKeychain": { "service": "my-keys", "account": "claude-prod" } }
  }
}
```

- 命名约定：service 默认 `sub-bot`，account 默认 `<provider>/<account>`（当前即 `openai/default`、`anthropic/default`）。`security find-generic-password -s sub-bot`（不加 `-w`，只看属性）或在“钥匙串访问”里搜索 `sub-bot` 就能看到全部条目。`service` / `account` 给出时不能为空白；不认识的字段会被拒绝。
- `apiKeyKeychain` 与 `apiKeyFile`（含顶层 `apiKeyFile`）只能设置一个：`providers.openai: set only one of apiKeyFile / apiKeyKeychain`。`faux` 不支持。
- 读取通过 Security.framework（`security-framework` crate）直接进行，不调用 `security` 命令行：key 不经过子进程管道，ACL 归属也是 sub-bot 本身而不是 `/usr/bin/security`。条目内容同样做 trim 与“只含 key”校验（否则报 `keychain item is not valid UTF-8 or is empty (...)`）。
- 配置声明的条目不存在会直接报 `keychain item not found (service=..., account=...)` 并启动失败，**不会**悄悄回落到 `OPENAI_API_KEY` 或受管存储。
- 非 macOS 构建里配置 `apiKeyKeychain`，加载配置时就会报错：``providers.openai: apiKeyKeychain is only supported on macOS (this build targets linux); use apiKeyFile or `sub-bot keys set` (writes a 0600 key file)``。
- **Linux 不支持 libsecret / Secret Service**：它需要 glib/dbus 动态链接，与 musl 静态发布冲突，而且无头服务器通常没有 Secret Service 守护进程。Linux 上请用 `sub-bot keys set` 生成的 0600 key 文件。

### `keys list` / `keys delete` / `keys check`

```bash
sub-bot --config sub-bot.json keys list               # --json 输出 JSON；--provider openai 只看一个 provider
sub-bot keys delete --provider openai                 # macOS 删 Keychain 条目，Linux 删受管文件
sub-bot keys delete --provider openai --file k.key    # 删指定文件
sub-bot --config sub-bot.json keys check              # 按配置实际加载一次 key（触发 Keychain 授权）
```

**`list`** 汇总这些来源：`--openai-key-file` / `--anthropic-key-file`、`*_API_KEY_FILE`、配置里每个账号声明的 `apiKeyFile` / `apiKeyKeychain` / `apiKeyEnv`（含 `accounts.*`）、`*_API_KEY`，以及受管存储里发现的条目（Keychain service `sub-bot` 下形如 `<provider>/<account>` 的条目，或受管目录里的 `<provider>-<account>.key`）。判断状态时**不读取 key**：文件只看 metadata（`present (mode 0600)` / `missing` / `unreadable`），Keychain 只做属性搜索（`present` / `missing`；非 macOS 为 `unsupported`），环境变量只看是否设置（`set`）。`list` 只解析配置里与 key 有关的字段，不做完整的配置加载。

macOS 示例（列宽随内容变化）：

```text
PROVIDER   ACCOUNT  SOURCE                                           STATUS
openai     default  keychain service=sub-bot account=openai/default  present
openai     work     keychain service=sub-bot account=openai/work     present
anthropic  default  keychain service=my-keys account=claude-prod     missing
anthropic  default  env ANTHROPIC_API_KEY                            set
```

Linux 示例：

```text
PROVIDER   ACCOUNT  SOURCE                                                            STATUS
openai     default  managed key file /home/u/.config/sub-bot/keys/openai-default.key  present (mode 0600)
anthropic  default  config apiKeyFile /etc/sub-bot/anthropic.key                      present (mode 0600)
anthropic  default  env ANTHROPIC_API_KEY                                             set
```

`--json` 输出数组，`inConfig` 表示该来源是否写在配置里：

```json
[
  {
    "provider": "openai",
    "account": "default",
    "source": "managed key file /home/u/.config/sub-bot/keys/openai-default.key",
    "status": "present (mode 0600)",
    "inConfig": false
  }
]
```

什么都没找到时 stdout 为空，stderr 提示 ``no keys found (use `sub-bot keys set --provider <id>`)``。

**`delete`** 只删除受管位置（macOS Keychain 条目，可配合 `--service`；Linux 受管文件）或显式 `--file`，不会去删配置里 `apiKeyFile` 指向的任意文件：

```text
Deleted OpenAI key for account 'default' from keychain (service=sub-bot, account=openai/default).
```

条目不存在时报 `no key stored for openai/default` 并以非零状态退出。

**`check`** 先完整加载配置（无 `--config` 时用内置默认配置），再对每个将在启动时加载的账号（启动必需的账号、所有 `accounts.*` 命名账号，以及显式声明了 key 来源的 default 账号）按完整优先级加载一次 key，只打印成功/失败和来源；用 `--provider` 时只检查该 provider（即使配置不需要它）。任一失败退出码为 1：

```text
openai/default: ok (keychain service=sub-bot account=openai/default)
anthropic/default: skipped (not used by this configuration)
```

```text
anthropic/default: FAILED: keychain access denied (service=sub-bot, account=anthropic/default): approve sub-bot in the Keychain prompt or re-run `sub-bot keys set` after upgrading the binary
Error: one or more API keys failed to load
```

### Keychain 错误码对照

sub-bot 把 Security.framework 返回的 OSStatus 映射成以下错误（括号里的 service/account 是实际条目名；错误信息从不包含条目内容）：

| OSStatus | 系统常量 | sub-bot 报错 | 常见原因与处理 |
|---|---|---|---|
| `-25300` | `errSecItemNotFound` | `keychain item not found (service=…, account=…)` | 条目不存在。配置里 `apiKeyKeychain` 指向的条目不存在 ⇒ 启动失败；第 5 级受管存储探测时视为“未命中”，继续报 key not found；`keys delete` 报 `no key stored for …`。用 `sub-bot keys set` 创建，或修正 service/account。 |
| `-25308` | `errSecInteractionNotAllowed` | `keychain interaction not allowed (…): the login keychain is locked and no UI is available; …` | 登录钥匙串已锁定且当前会话不能弹窗（ssh、无 GUI 的后台进程）。在同一会话先 `security unlock-keychain`，或从 GUI 登录会话 / LaunchAgent 启动。 |
| `-25293` | `errSecAuthFailed` | ``keychain access denied (…): approve sub-bot in the Keychain prompt or re-run `sub-bot keys set` after upgrading the binary`` | ACL 不信任当前二进制（典型：brew upgrade 之后），或授权失败。见下文“升级后重新授权”。 |
| `-128` | `errSecUserCanceled` | 同上（`keychain access denied (…)`） | 在授权弹窗里点了“拒绝”或取消。重新运行并选择“始终允许”。 |
| 其它 | — | `keychain error <code> (service=…, account=…)` | 用 `security error <code>` 查看含义。 |
| （超时） | — | `no answer from macOS Keychain within Ns (service=…, account=…): an authorization dialog is probably waiting …` | 读取在 `keychainTimeoutSecs` 内没有返回：多半是授权弹窗在等人点击（brew upgrade 换了二进制），或会话没有 GUI 显示弹窗。见下文「Keychain 读取超时」。 |

### 首次访问弹窗、升级与后台运行

- **首次 Keychain 访问弹窗**：由 sub-bot 自己 `keys set` 创建的条目，ACL 的“始终允许”列表里就是创建它的这个 sub-bot 二进制，之后同一个二进制读取**不会弹窗**。用“钥匙串访问”或 `security add-generic-password` 创建的条目，sub-bot 第一次读取时会弹出“sub-bot 想要使用钥匙串中的机密信息”：输入登录密码并点 **“始终允许”**，以后不再弹；点“拒绝”得到 `-128`。只做属性搜索的 `keys list` 不读条目内容，不会触发 ACL 弹窗；`keys check` 和服务启动会读取内容。两者都受下文「Keychain 读取超时」保护，不会无限挂起。
- **brew 升级后 ACL 失效，需要重新授权**：**每次** `brew upgrade` 都会替换 sub-bot 二进制。Homebrew 把每个版本装在不同的 Cellar 路径下，新二进制的路径和签名/hash 都和旧的不同，Keychain 视之为另一个应用，旧二进制创建或授权过的条目，原来的“始终允许”对新二进制不再生效（手动替换二进制同理），因此升级后授权弹窗会再次出现。GUI 会话里会再弹一次授权，而在有人点击之前读取一直阻塞（v0.9.2 起有超时，见下一节）；没有 GUI（ssh、后台进程）时读取失败，报 `keychain access denied (...)`（`-25293`）、`keychain interaction not allowed (...)`（`-25308`），或者等不到回应而超时。升级后、重启服务之前，在**图形界面登录会话的终端**里执行一次：
  - `sub-bot --config sub-bot.json keys check`，在弹窗里点 **“始终允许 / Always Allow”**（每次升级后点一次即可）；或
  - `sub-bot keys delete --provider openai && sub-bot keys set --provider openai`，让新二进制重新创建条目（ACL 随之归属新二进制）。
- **关于 `security add-generic-password -T`**：也可以用 `security add-generic-password -U -s sub-bot -a openai/<account> -T "$(brew --prefix)/bin/sub-bot" -w` 更新条目并把指定路径的程序加入信任列表（`-w` 后面不带值时会交互提示输入密码，**不要**把 key 直接写在命令行上，否则会进 shell 历史和 `ps`）。`-T` 信任的是指定路径上的程序，但 macOS 同时记录该程序当时的代码签名；Homebrew 升级会把它换成签名不同的新二进制，macOS 仍可能再次弹窗，所以这**不是**可靠的修复办法；升级后照样按上一条点一次“始终允许”。
- **ssh 会话下钥匙串锁定**：ssh 登录不会解锁登录钥匙串，也无法弹窗，读取返回 `-25308`。在同一个 ssh 会话里先解锁再运行：

  ```bash
  security unlock-keychain ~/Library/Keychains/login.keychain-db   # 交互输入登录密码
  sub-bot --config sub-bot.json keys check
  ```

  解锁不会给新二进制授权：如果条目 ACL 不信任当前二进制（例如刚升级），ssh 下仍会失败，需要按上一条在 GUI 会话里授权一次。
- **LaunchAgent（可用）vs LaunchDaemon（不可用）**：要让后台常驻的 `sub-bot serve` 使用 Keychain，请用 **LaunchAgent**：plist 放在 `~/Library/LaunchAgents/`，以登录用户身份在其会话中运行，能访问该用户（已解锁的）登录钥匙串，例如 `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.example.sub-bot.plist`。**LaunchDaemon**（`/Library/LaunchDaemons/`，系统级、通常以 root 运行、不属于任何用户登录会话）没有登录钥匙串可用，Keychain 来源会失败——这种部署请改用 `apiKeyFile` 或 `keys set --file` 写出的 0600 key 文件。Homebrew tap 的 formula 没有提供服务定义，launchd plist 需要自己编写。
- **无头部署（ssh、LaunchDaemon、没有人看屏幕的机器）**：推荐直接用 `apiKeyFile`（或 `keys set --file` 写出的 0600 key 文件），完全绕开 Keychain 授权弹窗。
- **启动失败即退出**：Keychain 读取只发生在启动阶段（不做运行期懒加载，避免服务运行中突然弹窗或挂起）。失败时 sub-bot 直接退出并上报 `startup_failed` issue，消息为上表中的 Keychain 错误文本（只含 service/account 名，不含 key）。同样，`provider ... is not configured`、`... API key not found for account ...`、`account '...' is not configured for provider ...` 这类 key 配置错误（只含 provider / 账号 / 环境变量名 / 配置路径）会原样显示，issue details 为 `{"reason": "key_config"}`；key 文件读取失败等其它错误只显示通用的 `service initialization failed (check provider and key configuration)`。部署 LaunchAgent 前先在 GUI 终端里跑一次 `sub-bot keys check`。

### Keychain 读取超时（v0.9.2）

授权弹窗出现时，Security.framework 的读取会一直阻塞到有人点击。v0.9.1 及以前，`serve` 和 `sub-bot keys check` 会因此无限挂起且没有任何日志。v0.9.2 起每次 Keychain 读取都放在独立线程上并限时等待：

- **超过 1 秒仍未返回**（正常读取远快于此，不会产生噪音）：记一条 WARN 日志 `waiting for macOS Keychain authorization for sub-bot/openai/default — check for a dialog on the Mac's screen; after brew upgrade choose "Always Allow"`（service/account 为实际条目名）；`serve` / `run` / `rpc` 还会上报一个 `keychain_waiting` issue（severity `warn`，details `{service, account}`）。`keys check` / `keys list` 把同一行提示打到 stderr（stdout 只放结果）。
- **超过 `keychainTimeoutSecs`（全局配置，默认 30，必须 >= 1；`--keychain-timeout-secs` / `SUB_BOT_KEYCHAIN_TIMEOUT_SECS` 覆盖）**：该次读取失败，报 `no answer from macOS Keychain within 30s (service=sub-bot, account=openai/default): an authorization dialog is probably waiting (the sub-bot binary changed, e.g. after `brew upgrade`) or this session has no GUI to show it; click "Always Allow" in the dialog or re-run `sub-bot keys set`, or use apiKeyFile for headless use (ssh / LaunchDaemon)`。处理方式与其它 Keychain 错误相同：启动必需的账号 ⇒ `serve` 退出并在 `startup_failed` issue 里显示这条消息（details `{"reason": "keychain"}`）；可选账号 ⇒ 只有该账号标记为不可用（`account_unavailable` issue），服务照常启动。`keys check` 把该账号报告为 `FAILED: no answer from macOS Keychain ...` 并以非零状态退出；`keys list` 的状态列显示 `unknown (no answer from macOS Keychain ...)`。
- 超时后被放弃的读取线程可能一直阻塞在 Security.framework 里，直到有人响应弹窗；sub-bot 不等待它，进程退出也不受影响。之后再点“始终允许”不会让本次已失败的启动恢复，重新启动即可。

## 同一 provider 多账号（v0.9.0）

同一个 provider 可以配置多份凭据（“账号”），例如公司 / 个人两个 OpenAI 组织、一个经代理转发的 key。每个账号在启动时各自建立一个 provider 实例（按 `(provider, account)` 区分），profile 决定默认账号和调用方能选哪些账号。账号名只能是 `[A-Za-z0-9_.-]{1,64}`；**账号名会出现在响应、日志和 issue 里，key 永远不会**。

### 配置 `accounts` / `defaultAccount`

```json
{
  "defaultProvider": "openai",
  "defaultModel": "gpt-6.1-sol",
  "apiKeyFile": "/etc/sub-bot/openai.key",
  "providers": {
    "openai": {
      "baseUrl": "https://api.openai.com/v1",
      "defaultAccount": "default",
      "failoverCooldownSecs": 120,
      "accounts": {
        "work": {
          "apiKeyFile": "/etc/sub-bot/openai-work.key",
          "organization": "org_abc123",
          "project": "proj_xyz789",
          "fallbackAccounts": ["default"]
        },
        "proxy": {
          "apiKeyEnv": "OPENAI_API_KEY_PROXY",
          "baseUrl": "http://localhost:9000/v1"
        },
        "personal": {}
      }
    },
    "anthropic": {
      "apiKeyFile": "/etc/sub-bot/anthropic.key",
      "accounts": {
        "ci": { "apiKeyFile": "keys/anthropic-ci.key" }
      }
    }
  },
  "profiles": {
    "coder": {
      "model": "gpt-6.1-sol",
      "account": "work",
      "allowedAccounts": ["work", "proxy"],
      "llmMaxRetries": 1,
      "workspaceRoots": ["./work"]
    },
    "reviewer": {
      "model": "gpt-6-astra",
      "tools": ["preset:readonly"],
      "workspaceRoots": ["./work"]
    },
    "claude-ci": {
      "provider": "anthropic",
      "account": "ci",
      "allowedAccounts": ["*"],
      "workspaceRoots": ["./work"]
    }
  },
  "clients": [
    { "name": "dev-bot", "tokenSha256": "REPLACE_ME", "profiles": ["coder", "reviewer", "claude-ci"] }
  ]
}
```

- **账号块** `providers.<id>.accounts.<name>`：key 来源 `apiKeyFile`（相对路径相对于配置文件目录）| `apiKeyKeychain`（仅 macOS，`{}` = service `sub-bot` / account `<provider>/<name>`）| `apiKeyEnv`（变量名须匹配 `^[A-Z_][A-Z0-9_]*$`，会自动加入 bash 子进程剔除列表）三选一；三者都省略（如上例 `personal: {}`）时使用受管存储，即 `sub-bot keys set --provider openai --account personal` 写入的位置。可选 `baseUrl`（覆盖 provider 级 `baseUrl`）、`organization` / `project`（仅 openai，作为 `OpenAI-Organization` / `OpenAI-Project` 请求头发送到对话、Responses 和 `/models` 请求）、`fallbackAccounts`（见下文 failover）。不认识的字段会被拒绝。
- **向后兼容**：旧写法全部属于账号 `default`——顶层 `apiKeyFile`、`providers.<id>.apiKeyFile` / `apiKeyKeychain`、`--openai-key-file` / `--anthropic-key-file`、`OPENAI_API_KEY[_FILE]` / `ANTHROPIC_API_KEY[_FILE]`、`OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL`。也可以显式写 `accounts.default`，但与旧字段同时出现会启动报错：`providers.openai: apiKeyFile/apiKeyKeychain and accounts.default are both set; keep one`。命名账号**不会**读取 CLI key 文件和 `*_API_KEY*` 环境变量。
- `defaultAccount`：没有指定账号时使用的账号（默认 `default`），必须是 `default` 或 `accounts` 里的名字。`faux` 只有 `default`，不支持 `accounts` / `defaultAccount`。
- **启动加载**：所有配置的账号都在启动时一次性加载（不懒加载）。被 profile 作为 `account` 引用的账号（含未写 `account` 时的默认账号）以及默认 provider、全局默认模型推断出的 provider 的默认账号是**必需**的，失败即启动失败（例如 anthropic 只被 `profiles.claude = {provider: anthropic, account: ci}` 使用时，只要求 `anthropic/ci`，`anthropic/default` 不必配置，`keys check` 也会将其显示为 skipped）；其余账号失败只 `warn`（日志 `account unavailable`）并上报 `account_unavailable` issue（scope 含 `provider` / `account`，details 只有失败类别），之后选择它的请求返回 400 `account 'x' of provider openai is unavailable (...)`（不含启动错误细节）。
- 每个加载成功的 key 都会加入 issue 脱敏列表；日志 `api key loaded` 带 `provider`、`account`、`key_source`（路径 / 变量名 / Keychain 条目名）。
- 校验错误示例：`providers.openai.accounts.work: set only one of apiKeyFile / apiKeyKeychain / apiKeyEnv`、`providers.anthropic.accounts.ci: organization/project are only supported for openai`、`providers.openai: defaultAccount 'x' is not defined in accounts`、`profile 'coder': account 'work' is not configured for provider openai`、`profile 'coder': allowedAccounts must include its own account 'work'`。

准备 key（macOS 存 Keychain，Linux 存 `~/.config/sub-bot/keys/<provider>-<account>.key`）：

```bash
sub-bot keys set --provider openai                      # default 账号
sub-bot keys set --provider openai --account personal   # 不写来源的命名账号直接用受管存储
sub-bot --config sub-bot.json keys list
sub-bot --config sub-bot.json keys check                # 按配置加载所有账号一次
```

### profile 的 `account` / `allowedAccounts` / `fallbackAccounts`

- `account`：会话默认账号，缺省为该 provider 的 `defaultAccount`。只对 profile 自己解析出的 provider 生效；请求把 provider 换成别的 provider 时，用那个 provider 的默认账号。
- `allowedAccounts`：调用方可以选择的账号。缺省只允许自身 `account`；`["*"]` 允许该 provider 所有已加载账号；显式列表必须包含自身账号。
- `fallbackAccounts`：覆盖账号级 `fallbackAccounts`（见 failover）。

### 会话 / 请求指定账号

| 入口 | 写法 |
|---|---|
| `POST /v1/sessions` | `{"profile": "coder", "account": "proxy"}` |
| `POST /v1/chat`（含 `async: true` 任务） | 顶层 `"account"` 或 `"session": {"account": ...}`（顶层优先）；带 `sessionId` 时忽略——账号是会话属性，不能中途修改 |
| CLI | `sub-bot run --account proxy "hi"`、`rpc --account`、`ask --account`、`probe-efforts --account`；环境变量 `SUB_BOT_ACCOUNT` |

不在 `allowedAccounts` 里的账号（包括根本不存在的账号，二者不区分，避免泄露账号是否存在）⇒ `AccountNotAllowed`：HTTP **403** `{"error": "account 'work' is not allowed in profile 'reviewer' (allowedAccounts)"}`；`run` 直接报错退出；`rpc` 先输出一行 `{"type":"response","command":"start","success":false,"error":"..."}` 再以非零状态退出。

CLI 的 `--account` 是 `run` / `rpc` / `ask` / `models` / `probe-efforts` 各自的参数（写在子命令之后），不是全局参数：`keys set|delete` 自己的 `--account` 表示要写入/删除的账号，且不受 `SUB_BOT_ACCOUNT` 影响。

### 响应与可观测性

- 会话视图（`POST/GET/PATCH /v1/sessions`，也即 `ask --json` 结尾的 `session_state` 行）、RPC `get_state`：`account` = 会话账号。
- `/v1/chat` 响应、`async` 任务的 `result`、`run --json`：`account` = 最近一次模型调用**实际使用**的账号（failover 后可能与会话账号不同）；每条 assistant 消息也带 `account`。
- `/v1/profiles`：每个 profile 多了 `account`、`allowedAccounts`（缺省展开为 `[account]`，`*` 展开为已加载账号）、`fallbackAccounts`。
- `/v1/models?provider=openai&account=work` / `sub-bot --provider openai models --account work`：只查该账号的模型目录；`account` 必须同时给 `provider`（否则 400 `account filter requires provider`），且必须是该 client 某个 profile 允许的账号（否则 403）。模型目录缓存按 provider+account 分开（10 分钟），`latest:` 深度模型别名按会话账号的目录解析。
- issue 的 scope 带 `account`；日志 `session created` 与 `session_run` span 带 `account`。

### 账号 failover（`fallbackAccounts`）

主账号遇到 **429 限流或额度耗尽（`insufficient_quota` / “exceeded your current quota”）**、且这次调用还没有输出任何内容时，自动用同一请求切换到 `fallbackAccounts` 中下一个已加载的账号（profile 的 `fallbackAccounts` 优先，否则用主账号的账号级 `fallbackAccounts`）。fallback 账号来自配置的显式列表，不需要出现在 `allowedAccounts` 里；未加载成功的 fallback 账号会被跳过。

- 认证错误（401/403）、Anthropic `overloaded_error`、其它 5xx 不切换；已经输出了内容之后的错误也不切换。
- 切换前先用完 provider 自身的重试（`llmMaxRetries`，默认 3 次指数退避）。配置了 fallback 的 profile 建议把 `llmMaxRetries` 调低（如 1）以便更快切换。
- 被切走的账号冷却 `providers.<id>.failoverCooldownSecs` 秒（默认 60）：冷却期内新的调用把它排到链尾；所有账号都在冷却时仍按原顺序尝试。
- 每次切换上报一条 warn issue `account_failover`（details `{from, to, reason: "rate_limit"|"quota", attempt}`，scope.account 更新为新账号），事件流里是一次 `message_retry`（reason `account failover: work -> default (quota)`），客户端应丢弃此前的部分输出。

## 工具资源上限

- `bash`：通过 `sh -c` 运行，独立进程组；取消、超时或清理时杀掉整个进程组（含子孙进程）。`timeoutSecs` 默认 120 秒，最多 1800 秒，等待输出管道 EOF 也计入超时。stdout/stderr 各捕获 256 KiB（合计 512 KiB），继续排空超出的输出并报告丢弃字节数；返回文本最多 50,000 字节（另加状态/截断提示）。
- `read`：最多读取 2 MiB，返回文本最多 50,000 字节，超出附截断提示。
- `ls`：最多保留 1000 个目录项并排序，附省略项数。
- `http_fetch`：响应体最多读取 2 MiB，返回文本最多 50,000 字节，超出附截断提示；连接超时 15 秒，总超时 60 秒（含响应体读取），支持取消。
- 调用方 `callback`：响应体最多 1 MiB，超出报错；总超时 120 秒（含响应体读取），支持取消。

## 安全提示

- 默认只监听 `127.0.0.1`。如果要跨机器访问，请放在带 TLS 的反向代理后面。
- 受管 key 目录（Linux `~/.config/sub-bot/keys`）必须是当前用户所有、权限 0700 的真实目录，key 文件为 0600；`sub-bot keys set` 遇到不满足的已有目录会拒绝写入而不是修改权限。
- `bash` 工具能执行任意命令，请只把它开放给可信的 bot，并用 `workspaceRoots` 限定目录。只读场景使用 `preset:readonly`。
