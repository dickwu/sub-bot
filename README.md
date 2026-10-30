# sub-bot

一个用 Rust 写的**本地编码 agent 服务**，架构仿照 [pi coding-agent](https://github.com/badlogic/pi-mono)。它内置 read / write / edit / ls / bash 等编码工具，不绑定具体项目，供其它 bot 通过 HTTP+SSE 或 stdio JSONL RPC 调用。

- 仅支持 macOS 和 Linux（Ubuntu 等），单个二进制，TLS 用 rustls，不依赖 OpenSSL
- 每个 bot 一个 token，token 映射到允许使用的 profile；会话互相隔离，每个会话有独立的工作目录（cwd）
- OpenAI API key 只从环境变量读入进程内存，不会通过任何接口返回，也不会写进日志
- 设计细节见 [DESIGN.md](DESIGN.md)

> 状态：v0.5.0。支持问题上报（issues）、多目标 webhook、异步任务、结果轮询和 webhook 投递。接入了 OpenAI Responses API 和 Chat Completions（按模型自动选择）。Claude（Anthropic）、grep/find 工具、会话恢复和压缩等功能见 DESIGN.md 的里程碑。

## 持久化、重启恢复与 issue 路由

异步 task 状态和待投递 webhook 自动写入 `dataDir` 下的 `tasks.jsonl` / `deliveries.jsonl`，不依赖 SQLite。`--data-dir PATH` 优先于配置；默认是系统数据目录的 `sub-bot`，指定 `--session-dir` 时默认使用其 `state` 子目录。目录权限为 0700，文件为 0600；一个目录仅供一个服务实例使用。记录包含任务结果和 webhook payload，请按敏感业务数据保护。

```json
{
  "dataDir": "/var/lib/sub-bot",
  "shutdownGraceSecs": 10
}
```

启动时压缩日志、清理超过 `taskTtlSecs` 的结果。上次处于 queued/running 的任务不会重新执行工具，而是标记 failed，error 为 `interrupted by restart`，并上报 client/task 范围的 error issue `task_interrupted`。任务仍可由所属 client 通过 `GET /v1/tasks/{id}` 查询，直到 TTL 到期。

待投递 webhook 继续原来的 `deliveryId` 和尝试次数；接收端必须按 ID 去重（至少一次投递，不保证恰好一次）。日志不保存 webhook 签名密钥，重启时按 URL 从当前配置重新解析；目标已删除（包括未保留到配置的请求级 callback URL）则标记 failed 并上报 issue。请在当前 client 的 webhook 配置中保留需要恢复的 callback 目标。

SIGINT/SIGTERM 阻止新任务、取消运行中的 agent，并在 `shutdownGraceSecs`（默认 10 秒）内等待任务、issue reporter 和 webhook 投递队列；超时未完成的投递留在日志中，下次启动恢复。会话聊天历史仍由独立的 `--session-dir` 控制；这里不自动恢复 agent 执行。

同步 `POST /v1/chat`、SSE 和会话运行也订阅所属 client 配置中接受 `issues` 的 webhook，遵守 `events` / `minSeverity`。运行结束后保留短暂订阅宽限期；取消订阅后路由缓存至少保留 `alerts.dedupWindowSecs`，已聚合 issue 还携带原始路由快照。因此窗口结束时的 `count > 1` 汇总仍到原 client，不会广播给其它 client（管理员全局 alerts 保持独立）。

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
# 推荐：key 放文件（600 权限），不进入环境变量，详见下文「API key」
mkdir -p ~/.config/sub-bot && install -m 600 /dev/null ~/.config/sub-bot/openai.key && $EDITOR ~/.config/sub-bot/openai.key
export OPENAI_API_KEY_FILE=~/.config/sub-bot/openai.key   # 或 --openai-key-file / 配置 apiKeyFile；也仍支持 OPENAI_API_KEY
sub-bot hash-token "$(openssl rand -hex 24)"   # 记下 token 原文给 bot，把输出的哈希填进配置
cp sub-bot.example.json sub-bot.json   # 编辑 profiles / clients
sub-bot --config sub-bot.json serve --bind 127.0.0.1:7878
```

不带 `--config` 时会使用单 client、单 `default` profile 的模式，以 `--workspace`（默认当前目录）为根。启动时会在控制台打印一次随机生成的 token，仅适合本机调试。

离线试用（不调用 OpenAI）：加上 `--provider faux`。

`serve` 收到 Ctrl-C 或 SIGTERM 时停止接受新会话/提交，取消正在运行的会话，等运行收尾并写入 JSONL 后退出。

## 配置（`sub-bot.json`）

```jsonc
{
  "defaultModel": "gpt-4.1-mini",
  "sessionIdleTtlSecs": 3600,                 // 普通会话空闲 TTL，0 禁用
  "chatSessionTtlSecs": 600,                  // /v1/chat 自动创建会话的空闲 TTL，0 禁用（独立于普通 TTL）
  "maxSessionsPerClient": 64,                // 每个 client 的内存会话上限，0 不限制
  "taskTtlSecs": 3600,                       // 已完成任务保留秒数（从完成时计时）；0 在下次清理时过期
  "webhookMaxAttempts": 6,                   // webhook 最大投递次数（含首次）
  "defaultReasoningEffort": "low",            // 可选：全局默认推理强度
  "defaultApi": "auto",                       // 可选：auto|chat|responses，默认 auto
  "streamIdleTimeoutSecs": 300,               // LLM 流连续无字节的超时；推理模型可能长时间静默
  "chatTimeoutSecs": 900,                     // /v1/chat 非流式默认等待上限
  "apiKeyFile": "/etc/sub-bot/openai.key",    // 可选：OpenAI key 文件，相对路径相对配置文件目录
  "profiles": {
    "coder": {
      "workspaceRoots": ["/home/me/code"],     // 会话的 cwd 必须位于这些目录之内
      "tools": ["read", "ls", "edit", "write", "bash"],  // 白名单，默认就是这些；也可写 "preset:readonly" / "preset:no-shell"
      "model": "gpt-5.4-mini",                 // profile 默认模型
      "allowedModels": ["gpt-5*", "gpt-4.1*"],  // 可选：bot 能切换到的模型（支持 * 通配），不填则不限制
      "reasoningEffort": "medium",             // 可选：none|minimal|low|medium|high|xhigh
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
  - `--provider openai|faux`
  - `--model`（`SUB_BOT_MODEL`）：`serve` 时覆盖全局默认模型；`rpc` / `run` 时指定本会话的模型
  - `--reasoning-effort`（`SUB_BOT_REASONING_EFFORT`）：同上，取值 `none|minimal|low|medium|high|xhigh`
  - `--api`（`SUB_BOT_API`）：`auto|chat|responses`，`serve` 时覆盖 `defaultApi`，`rpc` / `run` 时指定本会话
  - `--session-dir`（`SUB_BOT_SESSION_DIR`）：会话 JSONL 的持久化目录，按 bot 分子目录存放
  - `--openai-key-file`：从文件读取 OpenAI API key（见「API key」）
  - `--log-level`、`--log-dir`、`--log-format`

## 模型与推理强度

```bash
sub-bot models                       # 实时调用 OpenAI /v1/models，只列可对话的模型（不含日期快照）
sub-bot models --all --json          # 全部模型，JSON 输出
sub-bot --config sub-bot.json models --profile coder   # 只列 coder 的 allowedModels 允许的
sub-bot --model gpt-5.4-mini --reasoning-effort high run "解释一下这个仓库的结构"
sub-bot --model gpt-5-mini --reasoning-effort minimal run --json "say hi"
```

`models` 的第二列是推理强度可选值：`-` 表示该模型不支持 `reasoning_effort`（如 gpt-4.1、gpt-4o），`?` 表示未知（原样传给 OpenAI）。

选择优先级：
- 模型：请求/命令行的 `model` > profile 的 `model` > `defaultModel`。如果 profile 设置了 `allowedModels`，不在列表里的模型会被拒绝（HTTP 400）
- 推理强度：请求/命令行的 `reasoningEffort` > profile 的 `reasoningEffort` > `defaultReasoningEffort`；都没有就不发送
- 模型不支持推理强度时，参数会被忽略，并记一条 warn 日志；模型不支持所选档位时（比如 gpt-5 没有 `none`、gpt-5.1 没有 `minimal`），换成最接近的档位并记 warn
- 内置能力表之外的新模型：先原样发送；如果 OpenAI 返回 400 说明不支持，会从错误信息里学到可选档位，缓存下来并自动重试（最多两次）
- 不支持的档位会被缓存，缓存按（模型, API, 是否带工具）区分，所以带工具会话里的降级不会影响不带工具的会话

### 用哪个 API（`api`）

- `auto`（默认）：gpt-5.4 及以后、gpt-6.x、o 系列走 **Responses API**（`POST /v1/responses`，流式 SSE，`function_call` / `function_call_output`，`reasoning.effort`，usage 含 reasoning tokens），其它模型走 Chat Completions
- 优先级：请求/命令行的 `api` > profile 的 `api` > `defaultApi`
- 2026-10 实测：gpt-5.4 及以后在 Chat Completions 上不允许“函数工具 + reasoning_effort”，gpt-6.x 在 Chat Completions 上带工具完全不可用。`auto` 下遇到这种错误会自动切到 Responses 并记住；显式 `api: "chat"` 时只会去掉 reasoning_effort 重试一次（记 warn），仍然失败就直接报错并提示改用 `responses`
- `run --json` 输出 `api`、`reasoningEffort`（**实际生效**的值）、`requestedReasoningEffort`（请求的值）、`usage`（最后一次调用）和 `totalUsage`（整次运行累计，含 `reasoning`）
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

所有 `/v1/*` 接口（除 `/v1/health` 外）都需要 `Authorization: Bearer <bot token>`。

| 方法和路径 | 说明 |
|---|---|
| `GET /v1/health` | 健康检查（无需鉴权） |
| `GET /v1/profiles` | 当前 bot 可用的 profile（含 `model`、`allowedModels`、`reasoningEffort`） |
| `GET /v1/models?profile=&all=` | 实时模型列表（缓存 10 分钟），每项带 `reasoning` / `reasoningEfforts`；`profile` 只返回该 profile 允许的 |
| `POST /v1/sessions` | 新建会话 `{profile, cwd?, model?, reasoningEffort?, tools?, systemPrompt?, appendSystemPrompt?, callerTools?}`，返回 `{sessionId, cwd, model, api, reasoningEffort, requestedReasoningEffort, tools}` |
| `GET /v1/sessions/{id}` | 会话信息及运行状态；非 owner 或未知会话返回 404 |
| `DELETE /v1/sessions/{id}` | 删除自己的内存会话并中止运行，返回 `{"deleted":true}`；未知/非 owner 返回 404；不删除磁盘 JSONL |
| `POST /v1/sessions/{id}/prompt` | `{message, mode?: "steer"\|"follow_up"}`，返回 202 |
| `GET /v1/sessions/{id}/events` | SSE 事件流 |
| `POST /v1/sessions/{id}/abort` | 中止当前运行 |
| `GET /v1/sessions/{id}/messages` | 消息历史 |
| `POST /v1/chat` | 一次性调用：`{message, sessionId?, session?, stream?, timeoutSecs?, async?, callbackUrl?, webhookEvents?}` |
| `GET /v1/tasks/{id}` | 异步任务状态/结果/投递状态；未知、已过期或非 owner 返回 404 |

超时行为：
- `streamIdleTimeoutSecs`（全局，默认 300 秒）适用于 OpenAI Chat Completions 和 Responses；每次收到字节重置计时。超时产生 `stopReason: "error"`、`errorMessage: "stream idle timeout after Ns"`。连接超时为 30 秒，初始响应头等待上限为 120 秒。
- `runTimeoutSecs`（profile，默认 `null`）限制整次运行（含工具和排队消息），对 HTTP、RPC、CLI 都生效。超时取消运行、记录 `run timed out` 警告，并发送 `agent_settled{aborted:true}`。
- 非流式 `/v1/chat` 的 `timeoutSecs` 覆盖全局 `chatTimeoutSecs`（默认 900 秒）。超时 abort 后最多再等 5 秒收尾，返回 HTTP **504**：`{sessionId, error: "timeout", text, messages}`，保留已收到的部分文本和消息。`stream:true` 不使用此等待上限（仍受流空闲和运行超时限制）。

会话在创建、提交消息、运行结束时刷新空闲计时；读取信息/历史或订阅 SSE 不刷新计时。`serve` 每隔最多 60 秒（更短的非零 TTL 优先）清理超过各自 TTL 且未运行的会话，并记录日志；运行中的会话不会被空闲清理。删除/过期仅释放内存中的会话，已有 JSONL 文件保留在磁盘，当前不支持按旧 ID 自动恢复。

每个 client 最多保留 `maxSessionsPerClient` 个会话（包括 chat 自动创建的会话）。达到上限时先清理该 client 已过期的空闲会话；仍满则创建返回 HTTP **429** `{"error":"too many sessions..."}`，可主动 DELETE 腾出名额。其它创建参数错误仍返回 400。

`/v1/chat` 不带 `sessionId` 时自动创建临时会话，使用独立的 `chatSessionTtlSecs`（默认 10 分钟），而非普通会话的 `sessionIdleTtlSecs`（默认 1 小时）。可用返回的 `sessionId` 继续对话；`stream:true` 时 ID 在响应头 `X-Session-Id` 中。继续对话会刷新计时，但不改变会话 TTL。三个配置字段都是顶层字段；两个 TTL 的 `0` 各自表示禁用过期，上限的 `0` 表示不限数量。

```bash
# 一次性调用，等待结果
curl -s localhost:7878/v1/chat -H "Authorization: Bearer $BOT_TOKEN" -H 'content-type: application/json' \
  -d '{"message":"给 src/lib.rs 加单元测试并运行 cargo test","session":{"profile":"coder","cwd":"my-project","model":"gpt-5.4","reasoningEffort":"high"}}'

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

模型和推理强度用全局参数指定：`sub-bot --model gpt-5.4-mini --reasoning-effort low rpc --profile coder`。

支持的命令：`prompt`、`steer`、`follow_up`、`abort`、`get_messages`、`get_state`（含 `model`、`reasoningEffort`）、`get_available_models`。**关闭 stdin 等于关机**：请等收到 `agent_settled` 后再关闭。

## API key

sub-bot 按以下顺序查找 OpenAI key，取第一个非空的：

1. `--openai-key-file <路径>`
2. `OPENAI_API_KEY_FILE` 环境变量（指向文件）
3. 配置文件里的 `apiKeyFile`
4. `OPENAI_API_KEY` 环境变量

key 文件只放 key 本身，首尾空白和换行会被去掉；文件读不到、为空或中间含空白时会明确报错，但报错不会包含文件内容。文件权限对 group/other 可读时会打一条警告，建议 `chmod 600`。日志只记录 key 的来源（变量名或路径），不记录 key。`ANTHROPIC_API_KEY` / `ANTHROPIC_API_KEY_FILE` 已登记在同一套机制里，供以后的 Anthropic provider 使用。

**bash 工具的环境变量**：bash 子进程启动时总会剔除 `OPENAI_API_KEY`、`ANTHROPIC_API_KEY`、`SUB_BOT_TOKEN` 及对应的 `*_FILE` 变量，`allow` 也无法放行。其它变量由 profile 的 `shellEnv` 决定：默认 `inherit` 会继承其余变量；`allowlist` 只保留 `PATH`、`HOME`、`USER`、`LOGNAME`、`SHELL`、`LANG`、`LC_*`、`TERM`、`TZ`、`TMPDIR` 等基础变量和 `allow` 里的变量。如果你的环境里还有别的密钥（如 `AWS_*`、`GITHUB_TOKEN`），请写进 `deny`，或者直接用 `allowlist`。

**局限（同用户）**：这只能阻止 key 通过环境变量直接传给子进程。bash 和 sub-bot 以同一个系统用户运行，所以：

- 如果 key 来自环境变量，子进程仍可以读 `/proc/<sub-bot 的 pid>/environ`（Linux），里面是 sub-bot 启动时的环境，进程内 `unsetenv` 也清不掉；
- 如果 key 来自文件，子进程可以直接读这个文件（只要知道路径）；
- 内容中含有 key 的其它环境变量（不是上面这些变量名）不会被识别，需要自己加进 `deny` 或用 `allowlist`。

因此**推荐用 key 文件**（不出现在任何进程的环境里），文件放在 `workspaceRoots` 之外。要真正隔离，请让 sub-bot 以单独的系统用户运行，或者把 bash 放进容器/沙箱。

## 工具资源上限

- `bash`：通过 `sh -c` 运行，独立进程组；取消、超时或清理时杀掉整个进程组（含子孙进程）。`timeoutSecs` 默认 120 秒，最多 1800 秒，等待输出管道 EOF 也计入超时。stdout/stderr 各捕获 256 KiB（合计 512 KiB），继续排空超出的输出并报告丢弃字节数；返回文本最多 50,000 字节（另加状态/截断提示）。
- `read`：最多读取 2 MiB，返回文本最多 50,000 字节，超出附截断提示。
- `ls`：最多保留 1000 个目录项并排序，附省略项数。
- `http_fetch`：响应体最多读取 2 MiB，返回文本最多 50,000 字节，超出附截断提示；连接超时 15 秒，总超时 60 秒（含响应体读取），支持取消。
- 调用方 `callback`：响应体最多 1 MiB，超出报错；总超时 120 秒（含响应体读取），支持取消。

## 安全提示

- 默认只监听 `127.0.0.1`。如果要跨机器访问，请放在带 TLS 的反向代理后面。
- `bash` 工具能执行任意命令，请只把它开放给可信的 bot，并用 `workspaceRoots` 限定目录。只读场景使用 `preset:readonly`。
