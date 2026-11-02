# sub-bot

一个用 Rust 写的**本地编码 agent 服务**，架构仿照 [pi coding-agent](https://github.com/badlogic/pi-mono)。它内置 read / write / edit / ls / bash 等编码工具，不绑定具体项目，供其它 bot 通过 HTTP+SSE 或 stdio JSONL RPC 调用。

- 仅支持 macOS 和 Linux（Ubuntu 等），单个二进制，TLS 用 rustls，不依赖 OpenSSL
- 每个 bot 一个 token，token 映射到允许使用的 profile；会话互相隔离，每个会话有独立的工作目录（cwd）
- OpenAI / Anthropic API key 从文件或环境变量读入进程内存，不会通过任何接口返回，也不会写进日志
- 设计细节见 [DESIGN.md](DESIGN.md)

> 状态：v0.8.0。推理强度可在运行时按请求/会话动态修改（HTTP / CLI / RPC），Claude provider 支持 standard / deep / auto 模型路由（见「动态推理强度与模型路由」）。`ask` 同步支持 SSE 文本增量 / `--json` NDJSON；默认数据目录无 panic 回退并检查临时目录所有权；`/v1/issues` 即时更新去重计数。支持 Unix socket、本地身份认证、问题上报（issues）、多目标 webhook、异步任务、结果轮询和 webhook 投递。接入了 OpenAI Responses API 和 Chat Completions（按模型自动选择）。Claude（Anthropic）多 provider 配置见下文；grep/find 工具、会话恢复和压缩等功能见 DESIGN.md 的里程碑。

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

- Anthropic key 优先级：`--anthropic-key-file` > `ANTHROPIC_API_KEY_FILE` > `providers.anthropic.apiKeyFile` > `ANTHROPIC_API_KEY`。
- OpenAI key 优先级保持不变：`--openai-key-file` > `OPENAI_API_KEY_FILE` > `apiKeyFile` / `providers.openai.apiKeyFile` > `OPENAI_API_KEY`。顶层 `apiKeyFile` 仍只代表 OpenAI，两个配置别名若同时设置必须一致。
- 可选 `providers.<id>.baseUrl`；`OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL` 环境变量优先于配置。
- 选择顺序：会话 JSON `provider` > profile `provider` > 有效模型名以 `claude-` 开头时推断 Anthropic > 默认 provider。默认 provider 为显式 `--provider`（或 `SUB_BOT_PROVIDER`）> `defaultProvider` > `openai`。`run` / `rpc` / `ask` 的显式 `--provider` 也作为会话覆盖；已有 sessionId 的 ask 继续原会话。
- 启动必须能加载默认 provider，以及 profile / 默认模型引用或推断的 provider；缺少 key 即安全失败。其它 provider 有 key 来源则加载，无来源则跳过。后续会话选择未加载 provider 返回 400，并提示配置来源；`faux` 始终可用。
- 未指定模型的 anthropic 会话默认用 `claude-opus-5-5`；`deepModel`（默认固定 `claude-fable-5-1`）/ `autoRoute` / `routerModel` 控制 standard / deep / auto 路由，见「动态推理强度与模型路由」。
- `api` / `defaultApi` 仅用于 OpenAI。Anthropic 按模型选择 adaptive effort、effort + extended-thinking budget 或纯 budget，详见「模型与推理强度」。

`sub-bot models --json` 与 `GET /v1/models` 合并所有已加载 provider 的目录，每个模型都有 `provider` 字段。`models --provider anthropic` / `/v1/models?provider=anthropic` 只查询指定 provider；对话模型 / 日期快照过滤仅作用于 OpenAI，`--all` / `?all=true` 关闭过滤。`/v1/profiles` 返回每个 profile 解析后的 provider。LLM issue 的 scope 包含 provider，所有加载的 key 都加入 reporter 脱敏列表。

> Anthropic Messages API 已接入：支持 SSE 工具调用、签名 thinking 回放、prompt caching、分页模型目录及共享瞬态重试策略。

## 持久化、重启恢复与 issue 路由

仅 `serve` 打开持久化 journal store：异步 task 状态和待投递 webhook 自动写入 `<dataDir>/state` 下的 `tasks.jsonl` / `deliveries.jsonl`，不依赖 SQLite。`--data-dir PATH` 优先于配置；默认是系统数据目录的 `sub-bot`，不受 `--session-dir` 影响。目录权限为 0700，文件为 0600；一个目录仅供一个服务实例使用。记录包含任务结果和 webhook payload，请按敏感业务数据保护。

默认目录按顺序选择：`BaseDirs::data_local_dir()/sub-bot`；系统目录不可用时使用非空且为绝对路径的 `$XDG_DATA_HOME/sub-bot`；否则 Unix 使用 `std::env::temp_dir()/sub-bot-<euid>`（非 Unix 为 `temp_dir()/sub-bot`）。回退时会警告一次，建议设置 `--data-dir` 或配置 `dataDir`（默认日志目录也使用该回退链，日志可用 `--log-dir` 单独指定）。临时回退目录按需以 0700 创建；Unix 上已有目录必须属于当前有效 UID，且不能允许组/其他用户写入，也不能是符号链接，否则 store、socket 和日志访问会返回错误，不会自动修复权限。临时目录可能被系统清理，持久服务应显式配置稳定的数据目录。

```json
{
  "dataDir": "/var/lib/sub-bot",
  "shutdownGraceSecs": 10
}
```

`run`、`rpc`、`models` 使用内存 store，不获取 dataDir 锁、不读取或写入这些 journal、不恢复任务或续投服务端 webhook，因此可与同一 dataDir 的 `serve` 并行运行。独立的日志输出和 `--session-dir` 会话历史设置不受影响。

启动时压缩日志、清理超过 `taskTtlSecs` 的结果。最后一个不完整行会被忽略；完整行损坏时，原文件重命名为 `<name>.corrupt-<unix_ms>`（0600），恢复损坏行之前的记录，并上报 error issue `store_corrupt`。上次处于 queued/running 的任务不会重新执行工具，而是标记 failed，error 为 `interrupted by restart`，并上报 client/task 范围的 error issue `task_interrupted`。任务仍可由所属 client 通过 `GET /v1/tasks/{id}` 查询，直到 TTL 到期。

待投递 webhook 继续原来的 `deliveryId` 和尝试次数；接收端必须按 ID 去重（至少一次投递，不保证恰好一次）。日志不保存 webhook 签名密钥，重启时按 URL 从当前配置重新解析；目标已删除（包括未保留到配置的请求级 callback URL）则标记 failed 并上报 issue。请在当前 client 的 webhook 配置中保留需要恢复的 callback 目标。

SIGINT/SIGTERM 阻止新任务、取消运行中的 agent，并在 `shutdownGraceSecs`（默认 10 秒）内等待任务、issue reporter 和 webhook 投递队列；超时未完成的投递留在日志中，下次启动恢复。会话聊天历史仍由独立的 `--session-dir` 控制；这里不自动恢复 agent 执行。

同步 `POST /v1/chat`、SSE 和会话运行也订阅所属 client 配置中接受 `issues` 的 webhook，遵守 `events` / `minSeverity`。运行结束后保留短暂订阅宽限期；取消订阅后路由缓存至少保留 `alerts.dedupWindowSecs`，已聚合 issue 还携带原始路由快照。因此窗口结束时的 `count > 1` 汇总仍到原 client，不会广播给其它 client（管理员全局 alerts 保持独立）。

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
printf '继续上次的任务' | sub-bot ask --session SESSION_ID -
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
# 推荐：key 放文件（600 权限），不进入环境变量，详见下文「API key」
mkdir -p ~/.config/sub-bot && install -m 600 /dev/null ~/.config/sub-bot/openai.key && $EDITOR ~/.config/sub-bot/openai.key
export OPENAI_API_KEY_FILE=~/.config/sub-bot/openai.key   # 或 --openai-key-file / 配置 apiKeyFile；也仍支持 OPENAI_API_KEY
sub-bot hash-token "$(openssl rand -hex 24)"   # 记下 token 原文给 bot，把输出的哈希填进配置
cp sub-bot.example.json sub-bot.json   # 编辑 profiles / clients
sub-bot --config sub-bot.json serve --bind 127.0.0.1:7878
```

不带 `--config` 时会使用单 client、单 `default` profile 的模式，以 `--workspace`（默认当前目录）为根，并自动将服务进程的有效 uid 映射到 client `default`。运行 `sub-bot serve --unix-socket` 后，同一用户即可用 `sub-bot ask "hello"` 零 token 调用；同一 uid 下的所有进程都获得该身份。TCP 仍需通过 `SUB_BOT_TOKEN` 或 `serve --token` 指定调用凭据；省略时生成的随机 token 不会打印。显式配置文件不会自动添加 uid 映射，需要自行配置 `localClients` 或 `trustLoopback`。

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
      "reasoningEffort": "medium",             // 可选：none|minimal|low|medium|high|xhigh|max 或 provider 小写名称
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
  - `--session-dir`（`SUB_BOT_SESSION_DIR`）：会话 JSONL 的持久化目录，按 bot 分子目录存放
  - `--openai-key-file`：从文件读取 OpenAI API key（见「API key」）
  - `--log-level`、`--log-dir`、`--log-format`

## 模型与推理强度

```bash
sub-bot models                       # 合并已加载 provider 的模型；OpenAI 默认去掉非对话模型和日期快照
sub-bot models --all --json          # 全部模型，JSON 输出
sub-bot --config sub-bot.json models --profile coder   # 只列 coder 的 allowedModels 允许的
sub-bot --model gpt-5.4-mini --reasoning-effort high run "解释一下这个仓库的结构"
sub-bot --model gpt-5-mini --reasoning-effort minimal run --json "say hi"
```

`models` 显示 provider、模型 id、完整推理档位和 `default`；方括号标出允许的最高三档，例如 `low,medium,[high,xhigh,max] default=medium`。`-` 表示不支持，`?` 表示未知。JSON 保留完整 `reasoningEfforts`，新增 `allowedEfforts` 和 `defaultEffort`。

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

所有 `/v1/*` 接口（除 `/v1/health` 外）都需要认证：默认是 `Authorization: Bearer <bot token>`，也可以显式配置上面的 UDS uid 映射或 loopback 信任。

| 方法和路径 | 说明 |
|---|---|
| `GET /v1/health` | 健康检查（无需鉴权） |
| `GET /v1/profiles` | 当前 bot 可用的 profile（含 `model`、`allowedModels`、`reasoningEffort`、`allowedEfforts`、`defaultEffort`、`routing`） |
| `GET /v1/models?profile=&all=&provider=` | 实时模型列表（按 provider 缓存 10 分钟），每项带 `provider` / `reasoning` / `reasoningEfforts` / `allowedEfforts` / `defaultEffort`；`profile` 只返回该 profile 允许的 |
| `POST /v1/sessions` | 新建会话 `{profile, cwd?, model?, reasoningEffort?, depth?, tools?, systemPrompt?, appendSystemPrompt?, callerTools?}`，返回 201 + 会话视图 `{sessionId, profile, cwd, provider, model, api, isStreaming, tools, reasoningEffort, requestedReasoningEffort, effortAdjusted, allowedEfforts, routing}` |
| `GET /v1/sessions/{id}` | 会话视图（同上）及运行状态；非 owner 或未知会话返回 404 |
| `PATCH /v1/sessions/{id}` | `{reasoningEffort?, depth?}`：修改推理强度（下一次模型调用生效，运行中也可）；depth 仅在第一次 prompt 之前（否则 409）。返回 200 + 会话视图 |
| `DELETE /v1/sessions/{id}` | 删除自己的内存会话并中止运行，返回 `{"deleted":true}`；未知/非 owner 返回 404；不删除磁盘 JSONL |
| `POST /v1/sessions/{id}/prompt` | `{message, mode?: "steer"\|"follow_up", reasoningEffort?, depth?}`，返回 202；`reasoningEffort` 粘性，`depth` 仅首个 prompt |
| `GET /v1/sessions/{id}/events` | SSE 事件流 |
| `POST /v1/sessions/{id}/abort` | 中止当前运行 |
| `GET /v1/sessions/{id}/messages` | 消息历史 |
| `POST /v1/chat` | 一次性调用：`{message, sessionId?, session?, stream?, timeoutSecs?, async?, callbackUrl?, webhookEvents?, reasoningEffort?, depth?}`；顶层 `reasoningEffort` / `depth` 作用于新会话或 `sessionId` 指定的会话（depth 仅首个 prompt 之前）；响应带 `model`、推理强度字段和 `routing` |
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

模型和推理强度用全局参数指定：`sub-bot --model gpt-5.4-mini --reasoning-effort low rpc --profile coder`；模型路由用 `rpc --depth standard|deep|auto`。

支持的命令：`prompt`（可选 `reasoningEffort`（粘性）、`depth`（仅首个 prompt））、`set_reasoning_effort`（`{"level":"high"}` → `{requested, effective, adjusted, allowedEfforts}`）、`steer`、`follow_up`、`abort`、`get_messages`、`get_state`（含 `model`、`reasoningEffort`、`requestedReasoningEffort`、`effortAdjusted`、`allowedEfforts`、`routing`）、`get_available_models`。**关闭 stdin 等于关机**：请等收到 `agent_settled` 后再关闭。

## API key

sub-bot 按以下顺序查找 OpenAI key，取第一个非空的：

1. `--openai-key-file <路径>`
2. `OPENAI_API_KEY_FILE` 环境变量（指向文件）
3. 配置文件里的 `apiKeyFile`
4. `OPENAI_API_KEY` 环境变量

key 文件只放 key 本身，首尾空白和换行会被去掉；文件读不到、为空或中间含空白时会明确报错，但报错不会包含文件内容。文件权限对 group/other 可读时会打一条警告，建议 `chmod 600`。日志只记录 key 的来源（变量名或路径），不记录 key。`ANTHROPIC_API_KEY` / `ANTHROPIC_API_KEY_FILE` 使用同一套机制，优先级见上方 Claude (Anthropic) 节。

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
