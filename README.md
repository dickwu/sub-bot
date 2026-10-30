# sub-bot

一个用 Rust 写的**本地编码 agent 服务**，架构仿照 [pi coding-agent](https://github.com/badlogic/pi-mono)。它内置 read / write / edit / ls / bash 等编码工具，不绑定具体项目，供其它 bot 通过 HTTP+SSE 或 stdio JSONL RPC 调用。

- 仅支持 macOS 和 Linux（Ubuntu 等），单个二进制，TLS 用 rustls，不依赖 OpenSSL
- 每个 bot 一个 token，token 映射到允许使用的 profile；会话互相隔离，每个会话有独立的工作目录（cwd）
- OpenAI API key 只从环境变量读入进程内存，不会通过任何接口返回，也不会写进日志
- 设计细节见 [DESIGN.md](DESIGN.md)

> 状态：v0.3。接入了 OpenAI Responses API 和 Chat Completions（按模型自动选择）。Claude（Anthropic）、grep/find 工具、会话恢复和压缩等功能见 DESIGN.md 的里程碑。

## 安装

从 [Releases](https://github.com/dickwu/sub-bot/releases) 下载对应平台的压缩包，并用 `.sha256` 校验：

| 平台 | 文件 |
|---|---|
| Linux x86_64 / arm64 | `sub-bot-<ver>-x86_64-unknown-linux-musl.tar.gz` / `aarch64-unknown-linux-musl` |
| macOS Intel / Apple Silicon | `x86_64-apple-darwin` / `aarch64-apple-darwin` |

解压后把 `sub-bot` 放到 PATH 里。macOS 首次运行如果被拦截，执行 `xattr -d com.apple.quarantine sub-bot`。

从源码构建：`cargo build --release -p agentd`

## 快速开始

```bash
export OPENAI_API_KEY=...
sub-bot hash-token "$(openssl rand -hex 24)"   # 记下 token 原文给 bot，把输出的哈希填进配置
cp sub-bot.example.json sub-bot.json   # 编辑 profiles / clients
sub-bot --config sub-bot.json serve --bind 127.0.0.1:7878
```

不带 `--config` 时会使用单 client、单 `default` profile 的模式，以 `--workspace`（默认当前目录）为根。启动时会在控制台打印一次随机生成的 token，仅适合本机调试。

离线试用（不调用 OpenAI）：加上 `--provider faux`。

## 配置（`sub-bot.json`）

```jsonc
{
  "defaultModel": "gpt-4.1-mini",
  "defaultReasoningEffort": "low",            // 可选：全局默认推理强度
  "defaultApi": "auto",                       // 可选：auto|chat|responses，默认 auto
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
      "systemPrompt": null,                    // 替换默认编码前言
      "appendSystemPrompt": "...",             // 追加内容
      "loadContextFiles": true,                // 注入 cwd 往上的 AGENTS.md / CLAUDE.md
      "allowSystemPromptOverride": false,      // 是否允许 bot 在请求里覆盖或追加系统提示
      "allowCallerTools": false,               // 是否允许 bot 注册回调工具
      "maxTurns": 50
    }
  },
  "clients": [
    { "name": "dev-bot", "tokenSha256": "<sub-bot hash-token 的输出>", "profiles": ["coder"] }
  ]
}
```

- **`sub-bot.example.json` 里的 `tokenSha256` 是占位符，必须替换**。配置里只存 token 的 SHA-256，不存原文。
- 全局参数（也可以用环境变量设置）：
  - `--config`（`SUB_BOT_CONFIG`）
  - `--provider openai|faux`
  - `--model`（`SUB_BOT_MODEL`）：`serve` 时覆盖全局默认模型；`rpc` / `run` 时指定本会话的模型
  - `--reasoning-effort`（`SUB_BOT_REASONING_EFFORT`）：同上，取值 `none|minimal|low|medium|high|xhigh`
  - `--api`（`SUB_BOT_API`）：`auto|chat|responses`，`serve` 时覆盖 `defaultApi`，`rpc` / `run` 时指定本会话
  - `--session-dir`（`SUB_BOT_SESSION_DIR`）：会话 JSONL 的持久化目录，按 bot 分子目录存放
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
| `POST /v1/sessions/{id}/prompt` | `{message, mode?: "steer"\|"follow_up"}`，返回 202 |
| `GET /v1/sessions/{id}/events` | SSE 事件流 |
| `POST /v1/sessions/{id}/abort` | 中止当前运行 |
| `GET /v1/sessions/{id}/messages` | 消息历史 |
| `POST /v1/chat` | 一次性调用：`{message, sessionId?, session?, stream?}` |

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

## 安全提示

- 默认只监听 `127.0.0.1`。如果要跨机器访问，请放在带 TLS 的反向代理后面。
- `bash` 工具能执行任意命令，请只把它开放给可信的 bot，并用 `workspaceRoots` 限定目录。只读场景使用 `preset:readonly`。
