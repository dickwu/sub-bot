# sub-bot（pi-agentd）

一个用 Rust 写的**本地编码 agent 服务**，架构仿照 [pi coding-agent](https://github.com/badlogic/pi-mono)。它内置 read / write / edit / ls / bash 等编码工具，不绑定具体项目，供其它 bot 通过 HTTP+SSE 或 stdio JSONL RPC 调用。

- 支持 Windows / macOS / Linux 的单个二进制，TLS 用 rustls，不依赖 OpenSSL
- 每个 bot 一个 token，token 映射到允许使用的 profile；会话互相隔离，每个会话有独立的工作目录（cwd）
- OpenAI API key 只从环境变量读入进程内存，不会通过任何接口返回，也不会写进日志
- 设计细节见 [DESIGN.md](DESIGN.md)

> 状态：v0.1 MVP。目前接入了 OpenAI Chat Completions。Claude（Anthropic）、Responses API、grep/find 工具、会话恢复和压缩等功能见 DESIGN.md 的里程碑。

## 安装

从 [Releases](https://github.com/dickwu/sub-bot/releases) 下载对应平台的压缩包，并用 `.sha256` 校验：

| 平台 | 文件 |
|---|---|
| Linux x86_64 / arm64 | `pi-agentd-<ver>-x86_64-unknown-linux-musl.tar.gz` / `aarch64-unknown-linux-musl` |
| macOS Intel / Apple Silicon | `x86_64-apple-darwin` / `aarch64-apple-darwin` |
| Windows x64 / ARM64 | `x86_64-pc-windows-msvc.zip` / `aarch64-pc-windows-msvc.zip` |

解压后把 `pi-agentd`（Windows 上是 `pi-agentd.exe`）放到 PATH 里。macOS 首次运行如果被拦截，执行 `xattr -d com.apple.quarantine pi-agentd`。

从源码构建：`cargo build --release -p agentd`

## 快速开始

```bash
export OPENAI_API_KEY=...            # Windows PowerShell: $env:OPENAI_API_KEY="..."
pi-agentd hash-token "$(openssl rand -hex 24)"   # 记下 token 原文给 bot，把输出的哈希填进配置
cp agentd.example.json agentd.json   # 编辑 profiles / clients
pi-agentd --config agentd.json serve --bind 127.0.0.1:7878
```

不带 `--config` 时会使用单 client、单 `default` profile 的模式，以 `--workspace`（默认当前目录）为根。启动时会在控制台打印一次随机生成的 token，仅适合本机调试。

离线试用（不调用 OpenAI）：加上 `--provider faux`。

## 配置（`agentd.json`）

```jsonc
{
  "defaultModel": "gpt-4.1-mini",
  "profiles": {
    "coder": {
      "workspaceRoots": ["/home/me/code"],     // 会话的 cwd 必须位于这些目录之内
      "tools": ["read", "ls", "edit", "write", "bash"],  // 白名单，默认就是这些；也可写 "preset:readonly" / "preset:no-shell"
      "model": "gpt-4.1",
      "systemPrompt": null,                    // 替换默认编码前言
      "appendSystemPrompt": "...",             // 追加内容
      "loadContextFiles": true,                // 注入 cwd 往上的 AGENTS.md / CLAUDE.md
      "allowSystemPromptOverride": false,      // 是否允许 bot 在请求里覆盖或追加系统提示
      "allowCallerTools": false,               // 是否允许 bot 注册回调工具
      "maxTurns": 50
    }
  },
  "clients": [
    { "name": "dev-bot", "tokenSha256": "<pi-agentd hash-token 的输出>", "profiles": ["coder"] }
  ]
}
```

- **`agentd.example.json` 里的 `tokenSha256` 是占位符，必须替换**。配置里只存 token 的 SHA-256，不存原文。
- 全局参数（也可以用环境变量设置）：
  - `--config`（`PI_AGENTD_CONFIG`）
  - `--provider openai|faux`
  - `--model`
  - `--session-dir`（`PI_AGENTD_SESSION_DIR`）：会话 JSONL 的持久化目录，按 bot 分子目录存放
  - `--log-level`、`--log-dir`、`--log-format`

## 日志

- 级别：`--log-level info`（或 `PI_AGENTD_LOG`）；`RUST_LOG=agentd=debug,agent_ai=trace` 可以按模块细调
- stderr 输出人类可读格式；文件日志按天滚动（`pi-agentd.YYYY-MM-DD.log`，保留 14 天），默认为 JSON 格式（`--log-format text` 可改）
- 默认目录（`--log-dir <dir>` 可改，`--log-dir -` 关闭文件日志）：
  - Linux：`~/.local/share/pi-agentd/logs`
  - macOS：`~/Library/Application Support/pi-agentd/logs`
  - Windows：`%LOCALAPPDATA%\pi-agentd\data\logs`
- 每行日志都带 span：`request_id`（沿用请求的 `X-Request-Id`，也会回写到响应头）/ `bot` / `session_id` / `profile` / `model` / `tool`
- 不会记录 API key、bot token 或回调 token；提示词、输出和文件内容只记录长度

## 其它 bot 如何调用

### HTTP + SSE

所有 `/v1/*` 接口（除 `/v1/health` 外）都需要 `Authorization: Bearer <bot token>`。

| 方法和路径 | 说明 |
|---|---|
| `GET /v1/health` | 健康检查（无需鉴权） |
| `GET /v1/profiles` | 当前 bot 可用的 profile |
| `POST /v1/sessions` | 新建会话 `{profile, cwd?, model?, tools?, systemPrompt?, appendSystemPrompt?, callerTools?}`，返回 `{sessionId, cwd, tools}` |
| `POST /v1/sessions/{id}/prompt` | `{message, mode?: "steer"\|"follow_up"}`，返回 202 |
| `GET /v1/sessions/{id}/events` | SSE 事件流 |
| `POST /v1/sessions/{id}/abort` | 中止当前运行 |
| `GET /v1/sessions/{id}/messages` | 消息历史 |
| `POST /v1/chat` | 一次性调用：`{message, sessionId?, session?, stream?}` |

```bash
# 一次性调用，等待结果
curl -s localhost:7878/v1/chat -H "Authorization: Bearer $BOT_TOKEN" -H 'content-type: application/json' \
  -d '{"message":"给 src/lib.rs 加单元测试并运行 cargo test","session":{"profile":"coder","cwd":"my-project"}}'

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
pi-agentd --config agentd.json rpc --profile coder
```

stdin 每行一个命令，stdout 每行一个响应或事件（格式与 pi 的 `--mode rpc` 一致）：

```
→ {"id":"1","type":"prompt","message":"列出当前目录的文件"}
← {"id":"1","type":"response","command":"prompt","success":true,"data":{"disposition":"started"}}
← {"type":"tool_execution_start","toolName":"ls",...}
← {"type":"agent_settled","aborted":false}
```

支持的命令：`prompt`、`steer`、`follow_up`、`abort`、`get_messages`、`get_state`。**关闭 stdin 等于关机**：请等收到 `agent_settled` 后再关闭。

## 安全提示

- 默认只监听 `127.0.0.1`。如果要跨机器访问，请放在带 TLS 的反向代理后面。
- `bash` 工具能执行任意命令，请只把它开放给可信的 bot，并用 `workspaceRoots` 限定目录。只读场景使用 `preset:readonly`。
