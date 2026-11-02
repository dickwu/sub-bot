# sub-bot v0.9.0 设计：macOS Keychain key 来源 + 同 provider 多账号

状态：设计稿（未实现）。目标读者：直接实现的工程师。本文只描述改动，不改代码。

对照的现状（v0.8.1）：

- `agent-ai/src/secrets.rs`：`Secret`、`PROVIDER_KEYS`、`KeySource`、`read_key_file`、`load_provider_key(pk, cli_file, config_file, getenv)`，优先级 CLI 文件 > `*_API_KEY_FILE` > 配置 `apiKeyFile` > `*_API_KEY`。
- `agentd/src/hub.rs`：`load_providers()` 按 provider id 建一个实例，放进 `Hub.providers: BTreeMap<String, Arc<dyn Provider>>`；`cached_models(cache, id, provider)` 以 provider id 为缓存 key；`create_session_with_ttl()` 用 `select_provider()` 选 provider，再 `self.provider(id)`。
- `agentd/src/config.rs`：`ProviderConfig { api_key_file, base_url, default_model }`；`Config::load()` 把相对 `apiKeyFile` 换成相对配置文件目录。
- `agentd/src/probe.rs`：自己重复了一遍 key 加载逻辑。
- `agentd/src/main.rs`：全局 `--openai-key-file` / `--anthropic-key-file`，`KeyFiles`。
- `issues.rs`：`IssueScope { request_id, client, session_id, task_id, profile, model, provider }`，`Reporter::add_secret()` 用于脱敏。

设计原则：**key 永远只在 `Secret` 里；日志/错误/issue/响应只出现来源描述（路径、变量名、service/account 名）和账号名，绝不出现 key。** 向后兼容：现有配置、CLI、环境变量不改一字也能跑，等价于名为 `default` 的账号。

---

## 1. 术语与数据模型

- **provider**：`openai` | `anthropic` | `faux`（不变）。
- **account**：某个 provider 下的一份凭据 + 可选连接参数（baseUrl、OpenAI organization/project）。名称规则：`^[A-Za-z0-9_.-]{1,64}$`，不能含 `/`（Keychain account 字段用 `/` 分隔），`default` 是保留名。
- **provider 实例**：按 `(provider, account)` 建立一个 `Arc<dyn Provider>`。`faux` 只有 `default`。
- **key 来源（KeySource）**：CLI 文件、env 文件、配置文件、Keychain、env 变量、受管存储（`sub-bot keys set` 写入的位置）。

```rust
/// agent-ai/src/secrets.rs（新增）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    CliFile(PathBuf),
    EnvFile(&'static str, PathBuf),
    ConfigFile(PathBuf),
    /// 配置的 apiKeyEnv（账号级别），变量名是 String（运行时登记到 secret_env_vars）
    AccountEnv(String),
    Env(&'static str),
    /// macOS Keychain generic password
    Keychain { service: String, account: String },
    /// `sub-bot keys set` 写入的 0600 文件（Linux/非 Keychain 路径）
    ManagedFile(PathBuf),
}
// Display 追加：
//   Keychain => "keychain service={service} account={account}"
//   ManagedFile(p) => "managed key file {p}"
//   AccountEnv(v) => "env {v}"
```

---

## 2. Keychain key 来源（仅 macOS）

### 2.1 命名约定（已选定）

**service 固定 `sub-bot`，account = `<provider>/<account>`**，例如 `openai/default`、`anthropic/work`。

理由：
- Keychain 的 generic password 以 `(service, account)` 唯一，一个 service 下多个 account 天然对应“多账号”；
- `security find-generic-password -s sub-bot` / Keychain Access 搜索 `sub-bot` 一次看到全部条目，运维直观；
- `keys list` 用 service 做属性搜索（不读 data，不触发 ACL 提示）就能枚举；
- 相比 `service="sub-bot.<provider>"`，用户自定义 service 时不必记住拼接规则。

配置 `apiKeyKeychain` 里 `service`/`account` 都可省略，省略即用约定值；也允许指向用户自己创建的任意条目（此时首次读取会有 ACL 提示，见 2.5）。

### 2.2 配置

```jsonc
"providers": {
  "openai": {
    "apiKeyKeychain": {}                                   // = {service:"sub-bot", account:"openai/default"}
  },
  "anthropic": {
    "apiKeyKeychain": {"service": "my-keys", "account": "claude-prod"}
  }
}
```

```rust
/// agentd/src/config.rs
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KeychainRef {
    pub service: Option<String>,
    pub account: Option<String>,
}
impl KeychainRef {
    pub const DEFAULT_SERVICE: &'static str = "sub-bot";
    /// 解析为实际的 (service, account)；缺省 account = "<provider>/<account_name>"
    pub fn resolve(&self, provider: &str, account_name: &str) -> (String, String) {
        (self.service.clone().unwrap_or_else(|| Self::DEFAULT_SERVICE.into()),
         self.account.clone().unwrap_or_else(|| format!("{provider}/{account_name}")))
    }
}
```

`ProviderConfig` 与 `AccountConfig`（见 §4）都新增 `pub api_key_keychain: Option<KeychainRef>`。

校验（`Config::validate`）：
- `service`/`account` 若给出不得为空白；
- 同一账号块里 `apiKeyFile`、`apiKeyKeychain`、`apiKeyEnv` **最多一个**，否则：`providers.openai.accounts.work: set only one of apiKeyFile / apiKeyKeychain / apiKeyEnv`；
- 非 macOS 构建且配置了 `apiKeyKeychain`：`validate()` 直接报错（不等到加载）：
  `providers.openai: apiKeyKeychain is only supported on macOS (this build targets linux); use apiKeyFile or \`sub-bot keys set\` (writes a 0600 key file)`。

### 2.3 读取实现

依赖（`crates/agent-ai/Cargo.toml`）：

```toml
[target.'cfg(target_os = "macos")'.dependencies]
security-framework = "3"
```

`security-framework` 只链接 Security.framework，纯 Rust 绑定，对 x86_64/aarch64-apple-darwin 都可用；linux musl 目标不会编译它。不要 `std::process::Command("security")`：子进程路径会把 key 经过 stdout 管道，且 shell `security` 的 ACL 归属是 `/usr/bin/security` 而非 sub-bot。

**libsecret（Linux）结论：不做。** 原因：需要 glib/dbus 动态链接，与 musl 静态发布目标冲突；无头服务器通常没有 Secret Service 守护进程；用 0600 key 文件（`sub-bot keys set` 自动生成）已经满足。文档明确说明。

```rust
/// agent-ai/src/keychain.rs（新文件；整个模块 #[cfg(target_os = "macos")]，非 macOS 提供同签名的 stub 返回 Unsupported）
#[derive(Debug, thiserror::Error)]  // agent-ai 需加 thiserror.workspace = true；或手写 Display
pub enum KeychainError {
    #[error("keychain item not found (service={service}, account={account})")]
    NotFound { service: String, account: String },
    /// errSecInteractionNotAllowed (-25308)：login keychain 锁定且当前会话无法弹窗（ssh / launchd 无 GUI）
    #[error("keychain interaction not allowed (service={service}, account={account}): the login keychain is locked and no UI is available; run `security unlock-keychain` in this session or start sub-bot from a GUI login session")]
    InteractionNotAllowed { service: String, account: String },
    /// errSecAuthFailed (-25293) / errSecUserCanceled (-128)：用户在 ACL 提示里点了拒绝/取消，或二进制签名变化后未授权
    #[error("keychain access denied (service={service}, account={account}): approve sub-bot in the Keychain prompt or re-run `sub-bot keys set` after upgrading the binary")]
    Denied { service: String, account: String },
    #[error("keychain item is not valid UTF-8 or is empty (service={service}, account={account})")]
    Invalid { service: String, account: String },
    #[error("keychain is only supported on macOS")]
    Unsupported,
    #[error("keychain error {code} (service={service}, account={account})")]
    Other { code: i32, service: String, account: String },
}

/// 可注入的读取器（测试用 mock；生产用 SystemKeychain）
pub trait KeychainReader: Send + Sync {
    fn get(&self, service: &str, account: &str) -> Result<Secret, KeychainError>;
}
pub struct SystemKeychain;
impl KeychainReader for SystemKeychain {
    fn get(&self, service: &str, account: &str) -> Result<Secret, KeychainError> {
        // security_framework::passwords::get_generic_password(service, account)
        //   Ok(bytes) => String::from_utf8 → trim → 空则 Invalid → Secret::new
        //   Err(e) => match e.code() { -25300 => NotFound, -25308 => InteractionNotAllowed, -25293 | -128 => Denied, c => Other{code:c} }
        // bytes 用完立刻 zeroize（写 0），与 Secret::drop 同样做法
    }
}

/// 写入/删除/枚举只给 `keys` CLI 用：
pub fn set_generic(service: &str, account: &str, secret: &Secret) -> Result<(), KeychainError>;   // set_generic_password（存在则更新）
pub fn delete_generic(service: &str, account: &str) -> Result<(), KeychainError>;               // delete_generic_password；不存在 => NotFound
/// 按 service 枚举 account 名（ItemSearchOptions: class=generic_password, service=..., load_attributes=true, load_data=false, limit=1000）。不读 data ⇒ 不触发 ACL 提示。
pub fn list_accounts(service: &str) -> Result<Vec<String>, KeychainError>;
```

**取值规则**：读出的 data 做 `trim()`，必须非空且不含内部空白（与 `read_key_file` 相同校验，复用一个 `validate_key_text(label, text) -> Result<Secret, String>` 私有函数）。

日志：`tracing::info!(provider, account = "work", key_source = %KeySource::Keychain{..}, "api key loaded")` —— 只有 service/account 名。错误信息同样不含 data。

### 2.4 加载顺序（纳入 `load_account_key`，完整见 §4.4）

Keychain 处于“配置文件来源”同一层级（它就是配置里声明的来源），优先于 `*_API_KEY` 环境变量，低于 CLI 文件和 `*_API_KEY_FILE`。

### 2.5 launchd / 后台 / ssh 下的 Keychain 行为（写入 README）

- **ACL 归属**：由 sub-bot 二进制自己 `keys set` 写入的条目，其 ACL 的“始终允许”列表包含该二进制（按代码签名要求匹配；未签名/ad-hoc 签名时按路径+hash）。同一个二进制读取**无需提示**。用 Keychain Access 或 `security add-generic-password` 创建的条目，第一次由 sub-bot 读取会弹窗（“允许 / 始终允许”），选“始终允许”后不再弹。
- **brew upgrade / 替换二进制**：路径不变但 hash/签名变化 ⇒ 系统视为新应用：GUI 会话里再次弹窗；无 GUI（launchd 用户代理无桌面会话、ssh）则返回 `errSecAuthFailed`/`errSecInteractionNotAllowed`。推荐处理：升级后在终端执行一次 `sub-bot keys set --provider openai`（更新条目并重写 ACL 为新二进制），或 `sub-bot keys check`（见 §3，读取一次触发并确认授权）。
- **锁定的 login keychain**：ssh 登录（没有登录 session 解锁 keychain）时读取返回 `-25308 errSecInteractionNotAllowed`。处理：`security unlock-keychain ~/Library/Keychains/login.keychain-db`（交互输入密码），或在 launchd plist 里以 **LaunchAgent（用户登录后）** 而非 LaunchDaemon 启动；LaunchDaemon（root、无登录 keychain）不支持，应改用 key 文件。
- **启动失败即退出**：Keychain 错误在启动时发生（§4.5 的“必需账号”），sub-bot 直接 fatal + `startup_failed` issue（message 为通用文本；详细错误只进日志 stderr，不含 key）。`startup_failure()` 的 typed allowlist 新增 `KeychainError`（它的 Display 已保证不含 key）。
- **不做运行期懒加载**：避免服务运行中突然弹 ACL 对话框或挂起。

### 2.6 读取超时（v0.9.2）

问题：`brew upgrade` 换了二进制后，条目 ACL 不再信任它，SecurityAgent 弹出“允许访问”对话框，`SecItemCopyMatching` 一直阻塞到有人点击；0.9.1 的 `serve` 与 `keys check` 因此无限挂起且无日志。

- `agent-ai::keychain::with_timeout(service, account, timeout, grace, on_wait, call)`：在独立的 `std::thread` 上执行读取，通过容量 1 的 channel 等待结果。`grace`（常量 `WAIT_HINT_GRACE` = 1s）内未返回 ⇒ 调用一次 `on_wait`；`timeout` 内未返回 ⇒ `KeychainError::Timeout { service, account, secs }`。超时后**不 join** 读取线程：它可能一直阻塞在 Security.framework 里，进程退出不等它；迟到的 `Secret` 随 channel 丢弃并在析构时清零。读取线程 panic 在调用方重新抛出。
- `TimedKeychain`（实现 `KeychainReader`）包装 `Arc<dyn KeychainReader>`，供启动加载与 `probe-efforts` 使用；`keys` CLI 用 `TimedStore`（`get` / `list` 限时，`set` / `delete` 透传——写入超时后结果不确定）。测试注入带延迟的 `MockKeychain`（`delayed(..)`），用 1s 超时 / 100ms grace 覆盖 慢读 ⇒ 提示 + Timeout、快读 ⇒ 无提示。
- 超时：全局配置 `keychainTimeoutSecs`（默认 30，校验 >= 1），`--keychain-timeout-secs` / `SUB_BOT_KEYCHAIN_TIMEOUT_SECS` 覆盖（对 `keys` 子命令同样生效）。
- 等待提示：`waiting for macOS Keychain authorization for <service>/<account> — check for a dialog on the Mac's screen; after brew upgrade choose "Always Allow"`。`serve` / `run` / `rpc`：WARN 日志 + issue `keychain_waiting`（Warn，component `config`，details `{service, account}`；reporter 在 Hub 构造前已安装，可直接上报）；`keys check` / `keys list`：打到 stderr。
- `Timeout` 是普通的 `KeychainError` 变体，走既有路径：必需账号 ⇒ 启动失败，`startup_failure` 显示其 Display（details `{"reason": "keychain"}`）；可选账号 ⇒ 仅该账号 `account_unavailable`；`keys check` ⇒ 该账号 `FAILED: ...`。Display 解释：N 秒内 Keychain 无应答；多半是授权对话框在等待（brew upgrade 后二进制变化）或会话没有 GUI；点“Always Allow”或重新 `sub-bot keys set`，无头部署用 apiKeyFile。

---

## 3. CLI：`sub-bot keys set|list|delete|check`

```rust
/// agentd/src/main.rs
Cmd::Keys { #[command(subcommand)] command: KeysCmd }

#[derive(Subcommand)]
enum KeysCmd {
    /// 从 stdin 读取 key 并保存（macOS: Keychain；Linux 或 --file: 0600 文件）。存在则覆盖。
    Set {
        #[arg(long)] provider: String,
        #[arg(long, default_value = "default")] account: String,
        /// 写到指定文件而不是 Keychain/默认目录
        #[arg(long)] file: Option<PathBuf>,
        /// Keychain service（默认 sub-bot）；非 macOS 上指定则报错
        #[arg(long)] service: Option<String>,
        /// 不打印配置片段
        #[arg(long)] quiet: bool,
    },
    /// 列出已知账号及其来源与是否存在，绝不输出 key
    List {
        #[arg(long)] provider: Option<String>,
        #[arg(long)] json: bool,
    },
    Delete {
        #[arg(long)] provider: String,
        #[arg(long, default_value = "default")] account: String,
        #[arg(long)] file: Option<PathBuf>,
        #[arg(long)] service: Option<String>,
    },
    /// 按当前配置尝试加载每个账号的 key（触发 Keychain 授权），只报告成功/失败与来源
    Check {
        #[arg(long)] provider: Option<String>,
    },
}
```

实现放在新文件 `agentd/src/keys.rs`（`pub async fn run(cli: &Cli, cfg: Option<&Config>, cmd: &KeysCmd) -> anyhow::Result<()>`）。`keys` 子命令**不**构造 Hub、不读其它 secret 文件（像 `ask` 一样只解析配置的只读部分；`check` 需要完整 `Config::load`）。

### 3.1 `set`

读取 key：
- `stdin` 是 TTY：打印提示 `Enter <Provider> API key for account '<name>' (input hidden): ` 到 stderr，关闭回显读取一行。**不引入 rpassword**：`libc` 已是 unix 依赖，自己实现 `read_hidden_line()`：`tcgetattr(0)` → 清 `ECHO`（保留 `ICANON`）→ `tcsetattr(TCSAFLUSH)` → 读一行 → 守卫 `Drop` 恢复原 termios（Ctrl-C 时由 `Drop` 恢复，不必装信号处理）→ 向 stderr 输出换行。约 25 行，少一个依赖。
- 非 TTY：读到 EOF，`trim()`，必须恰好一段无空白内容（同 `read_key_file` 规则），错误：`stdin must contain only the key`（不回显内容）。
- 空 ⇒ `no key provided`。

存储目标选择：
1. `--file PATH`：写文件；
2. 否则 macOS：Keychain `(service|sub-bot, "<provider>/<account>")`，`set_generic` 存在即更新；
3. 否则（Linux）：受管文件 `managed_key_path(provider, account)`。

```rust
/// agent-ai/src/secrets.rs
/// Linux：`$XDG_CONFIG_HOME/sub-bot/keys/<provider>-<account>.key`（`directories::BaseDirs::config_dir()`，通常 ~/.config/sub-bot/keys）。
/// macOS：受管存储是 Keychain，此函数不参与默认路径（只有显式 `--file` 才写文件）；为保持 cfg 简单仍返回 config_dir 下同样的路径，但 §4.4 第 5 级在 macOS 只查 Keychain。
/// 选 config 目录而非 data 目录：data 目录可能落到 /tmp 回退路径（见 config.rs temp_data_dir），key 不能放那里。
pub fn managed_key_path(provider: &str, account: &str) -> Option<PathBuf>;
/// 创建目录 0700（已存在不改权限、拒绝符号链接），写临时文件 0600 后 rename；返回最终路径。
pub fn write_key_file(path: &Path, key: &Secret) -> Result<(), String>;
```

写入 `managed_key_path` 时用 `prepare`-style 检查：目录必须属于当前 euid、非 symlink、mode & 0o077 == 0（和 `check_temp_data_dir` 同思路，可抽一个 `ensure_private_dir(path)` 共用）。

成功输出（stdout；`--quiet` 时省略）：

```
Saved OpenAI key for account 'work' to keychain (service=sub-bot, account=openai/work).
Add to your config:
  "providers": { "openai": { "accounts": { "work": { "apiKeyKeychain": {} } } } }
```

或文件：
```
Saved OpenAI key for account 'default' to /home/u/.config/sub-bot/keys/openai-default.key (mode 0600).
Add to your config:
  "providers": { "openai": { "apiKeyFile": "/home/u/.config/sub-bot/keys/openai-default.key" } }
```
（`default` 账号的片段用 `providers.<id>.apiKeyFile` / `apiKeyKeychain`，非 default 用 `accounts.<name>`。写入受管位置时也提示：“不加配置也会作为最后一级来源被找到”，见 §4.4 第 6 级。）

错误示例（不含 key）：`cannot write key file /path: Permission denied`、`keychain access denied (...)`、`--service is only supported on macOS`。

### 3.2 `list`

数据来源 = 配置里声明的账号（`providers.*` 隐式 default + `accounts.*`）∪ 受管存储里发现的条目（macOS：`list_accounts("sub-bot")` 解析 `<provider>/<account>`；Linux：扫描 `keys/` 目录 `<provider>-<account>.key`）。每行：

```
PROVIDER   ACCOUNT   SOURCE                                   STATUS
openai     default   keychain service=sub-bot account=openai/default   present
openai     work      config apiKeyFile /etc/sub-bot/openai-work.key    present (mode 0600)
anthropic  default   env ANTHROPIC_API_KEY                             set
anthropic  ci        keychain service=sub-bot account=anthropic/ci     missing
```

`STATUS` 判定不读取 secret 数据：文件 → `metadata` 存在性与 mode；Keychain → 属性搜索命中；env → `getenv(var).is_some()`。`--json` 输出 `[{provider, account, source, status, inConfig: bool}]`。**任何字段都不是 key。**

### 3.3 `delete`

macOS 默认删 Keychain 条目（`delete_generic`），`--file` 删指定文件，Linux 删受管文件。不存在 ⇒ 非零退出 `no key stored for openai/work`。删除配置里声明为 `apiKeyFile` 的任意路径不在职责内（只允许受管路径或显式 `--file`）。

### 3.4 `check`

`Config::load` 后对每个将被加载的账号（§4.5 规则）调用 `load_account_key`，打印 `openai/work: ok (keychain service=sub-bot account=openai/work)`、`FAILED: <错误>` 或 `skipped (not used by this configuration)`；任何失败 ⇒ 退出码 1。Keychain 读取限时（§2.6）：等待超过 1s 先在 stderr 打印提示，超时后该账号 `FAILED`。用途：launchd 部署前在 GUI 终端里触发并确认 ACL 授权。

### 3.5 CLI 全局 `--account`

```rust
/// 会话使用的账号（run/rpc/ask/models/probe-efforts）；受 profile.allowedAccounts 限制
#[arg(long, global = true, env = "SUB_BOT_ACCOUNT")]
account: Option<String>,
```
`session_req` 里填 `account: cli.account.clone()`；`ask` 把它放进 `session.account`；`models` 作为 `list_models_filtered(all, provider, account)` 的过滤；`probe-efforts` 用它选账号。

---

## 4. 同 provider 多账号

### 4.1 配置 JSON

```jsonc
{
  "apiKeyFile": "/etc/sub-bot/openai.key",            // 仍等价于 providers.openai.accounts.default.apiKeyFile
  "providers": {
    "openai": {
      "apiKeyFile": "/etc/sub-bot/openai.key",        // 旧写法 = accounts.default（二者必须一致，规则不变）
      "baseUrl": "https://api.openai.com/v1",        // 账号未设 baseUrl 时的 provider 默认
      "defaultModel": "gpt-6.1-sol",
      "defaultAccount": "default",                    // 可选；默认 "default"
      "failoverCooldownSecs": 60,                     // 可选；§5
      "accounts": {
        "work": {
          "apiKeyKeychain": {},                        // = service sub-bot / account openai/work
          "organization": "org_abc",                   // → OpenAI-Organization 头
          "project": "proj_xyz",                       // → OpenAI-Project 头
          "fallbackAccounts": ["default"]              // 可选；§5
        },
        "proxy": {
          "apiKeyEnv": "OPENAI_API_KEY_PROXY",          // 账号级环境变量；会自动加入子进程剔除列表
          "baseUrl": "http://localhost:9000/v1"
        }
      }
    },
    "anthropic": {
      "accounts": {
        "ci": { "apiKeyFile": "keys/anthropic-ci.key" } // 相对配置文件目录
      }
    }
  },
  "profiles": {
    "coder": {
      "model": "gpt-6.1-sol",
      "account": "work",                              // 可选；缺省 = provider.defaultAccount
      "allowedAccounts": ["work", "proxy"],           // 可选；缺省 = 只允许 account/默认账号；["*"] 全部
      "fallbackAccounts": ["proxy"],                  // 可选；覆盖账号级 fallbackAccounts；§5
      "workspaceRoots": ["./work"]
    }
  }
}
```

```rust
/// agentd/src/config.rs
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountConfig {
    pub api_key_file: Option<PathBuf>,
    pub api_key_keychain: Option<KeychainRef>,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    /// 仅 openai：OpenAI-Organization / OpenAI-Project 请求头
    pub organization: Option<String>,
    pub project: Option<String>,
    #[serde(default)]
    pub fallback_accounts: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub api_key_file: Option<PathBuf>,          // 旧：= accounts.default.apiKeyFile
    pub api_key_keychain: Option<KeychainRef>,  // 新：= accounts.default.apiKeyKeychain
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    #[serde(default)]
    pub default_account: Option<String>,
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountConfig>,
    #[serde(default)]
    pub failover_cooldown_secs: Option<u64>,
}

impl ProviderConfig {
    pub fn default_account(&self) -> &str { self.default_account.as_deref().unwrap_or("default") }
    /// 把旧字段折叠进去后的完整账号表：总是包含 "default"（可能是空块）。
    pub fn effective_accounts(&self, top_level_api_key_file: Option<&Path> /* 仅 openai 传入 */) -> BTreeMap<String, AccountConfig>;
}

// Profile 新增：
pub account: Option<String>,
#[serde(default)] pub allowed_accounts: Option<Vec<String>>,
#[serde(default)] pub fallback_accounts: Option<Vec<String>>,
```

`Config::load()` 相对路径处理：对 `providers.*.accounts.*.apiKeyFile` 也做“相对配置目录”转换（和现有 `provider.api_key_file` 循环并列）。

`Config::validate()` 新增规则（错误文案）：
- 账号名：`providers.openai: invalid account name 'a/b' (allowed: [A-Za-z0-9_.-]{1,64})`。
- `accounts.default` 与旧字段同时存在：`providers.openai: apiKeyFile/apiKeyKeychain and accounts.default are both set; keep one`。
- 顶层 `apiKeyFile` 与 `providers.openai.apiKeyFile` 必须一致（原规则保留）。
- 每个账号 `apiKeyFile`/`apiKeyKeychain`/`apiKeyEnv` 最多一个（§2.2）。
- `organization`/`project` 只允许 openai：`providers.anthropic.accounts.ci: organization/project are only supported for openai`。
- `apiKeyEnv` 必须匹配 `^[A-Z_][A-Z0-9_]*$`。
- `defaultAccount` 必须是 `default` 或 `accounts` 里的名字：`providers.openai: defaultAccount 'x' is not defined in accounts`。
- `fallbackAccounts` 每项必须存在于该 provider、不等于自身、不重复。
- `faux` 不允许 `accounts`/`defaultAccount`。
- profile：`account` / `allowedAccounts`（非 `*`）/ `fallbackAccounts` 的每个名字必须在该 profile 解析出的 provider 下存在（provider 由 `select_provider(None, p.provider, p.model|default_model, self.default_provider|"openai")` 得到）：`profile 'coder': account 'work' is not configured for provider openai`。`allowedAccounts` 若给出则必须包含 `account`（或默认账号）：`profile 'coder': allowedAccounts must include its own account 'work'`。
- 非 macOS 构建出现任何 `apiKeyKeychain` ⇒ §2.2 错误。

### 4.2 Hub 数据结构

```rust
/// agentd/src/hub.rs
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountKey { pub provider: String, pub account: String }
impl AccountKey {
    pub fn new(provider: &str, account: &str) -> Self;
    pub fn label(&self) -> String { format!("{}/{}", self.provider, self.account) }   // 日志/缓存/错误用
}

pub struct Hub {
    ...
    /// 已加载的 provider 实例，按 (provider, account)
    pub(crate) providers: BTreeMap<AccountKey, Arc<dyn Provider>>,
    /// 配置了但启动时加载失败的账号 → 失败原因（已脱敏，可直接放进 400 错误）
    unavailable: BTreeMap<AccountKey, String>,
    /// 每个 provider 的默认账号名（faux → "default"）
    default_accounts: BTreeMap<String, String>,
    failover: Arc<crate::accounts::Cooldowns>,   // §5
    ...
}

type ModelsCache = tokio::sync::Mutex<BTreeMap<AccountKey, (Instant, Vec<ModelInfo>)>>;
async fn cached_models(cache: &ModelsCache, key: &AccountKey, provider: &dyn Provider) -> Result<Vec<ModelInfo>>;
// 错误文案："provider openai/work: {e}"

impl Hub {
    pub fn provider(&self, id: &str, account: &str) -> Result<Arc<dyn Provider>>;   // 见 4.6 错误
    pub fn default_account(&self, id: &str) -> &str;
    pub fn accounts_of(&self, id: &str) -> Vec<String>;   // 已加载的账号名，排序
    pub fn profile_account<'a>(&'a self, p: &'a Profile, provider_id: &str) -> &'a str;  // p.account 或 default_account(provider)
    pub fn account_allowed(&self, p: &Profile, provider_id: &str, account: &str) -> bool;
}
```

`KeyFiles` 不变（仍只影响 `default` 账号）。`Hub::new / with_reporter / with_provider_keys` 签名不变。

### 4.3 Provider 实例构造

```rust
/// agent-ai/src/openai.rs
#[derive(Default, Clone, Debug)]
pub struct OpenAiOptions {
    pub base_url: Option<String>,
    pub organization: Option<String>,
    pub project: Option<String>,
    pub stream_idle_timeout: Option<Duration>,
}
impl OpenAiProvider {
    pub fn with_options(api_key: Secret, opts: OpenAiOptions) -> Self;   // 现有 new / with_stream_idle_timeout 改为调用它
}
```
内部保存 `extra_headers: Vec<(&'static str, String)>`（`OpenAI-Organization`、`OpenAI-Project`），在 `run()`（chat/responses POST）与 `list_models()` 的 request builder 上统一 `.headers(...)`。`CallCtx` 加同一字段。Anthropic 不变（只加 base_url 已有）。

### 4.4 key 加载函数（替换 `load_provider_key` 的调用方；旧函数保留给兼容测试或直接删除并改测试）

```rust
/// agent-ai/src/secrets.rs
pub struct AccountKeySpec<'a> {
    pub pk: &'a ProviderKey,
    pub account: &'a str,
    /// 仅 default 账号：CLI --<provider>-key-file
    pub cli_file: Option<&'a Path>,
    /// 账号块（或折叠后的 default 块）里声明的来源，最多一个
    pub config_file: Option<&'a Path>,
    pub keychain: Option<(String, String)>,   // 已 resolve 的 (service, account)
    pub account_env: Option<&'a str>,
}

/// 优先级（取第一个命中的；“命中”= 来源存在且非空；来源存在但读取失败 ⇒ 直接 Err，不继续向下）：
///   default 账号：
///     1. cli_file                                  → CliFile
///     2. <P>_API_KEY_FILE 环境变量                   → EnvFile
///     3. 配置声明的来源（config_file | keychain | account_env，三选一） → ConfigFile | Keychain | AccountEnv
///     4. <P>_API_KEY 环境变量                        → Env
///     5. 受管存储：macOS keychain (sub-bot, "<p>/default")；否则 managed_key_path(p, "default") 存在时 → Keychain | ManagedFile
///   其它账号：
///     3. 配置声明的来源（三选一）
///     5. 受管存储 (sub-bot, "<p>/<name>") / managed_key_path(p, name)
///   （命名账号不看 CLI 文件和 <P>_API_KEY* 环境变量：它们属于 default。）
/// 受管存储只在 1–4 都未命中时探测；NotFound 视为未命中，其它 Keychain 错误 ⇒ Err。
pub fn load_account_key(spec: &AccountKeySpec<'_>, getenv: impl Fn(&str) -> Option<String>, keychain: &dyn KeychainReader) -> Result<(Secret, KeySource), String>;
```

未找到的错误文案：
- default：`OpenAI API key not found for account 'default': use --openai-key-file, OPENAI_API_KEY_FILE, providers.openai.apiKeyFile / apiKeyKeychain, OPENAI_API_KEY, or \`sub-bot keys set --provider openai\``
- 其它：`OpenAI API key not found for account 'work': set providers.openai.accounts.work.apiKeyFile / apiKeyKeychain / apiKeyEnv, or run \`sub-bot keys set --provider openai --account work\``

`apiKeyEnv` 变量名在加载时 `register_secret_var(name)`（已存在的函数），保证 bash 子进程剔除。

`probe.rs` 改为调用同一个 `load_account_key`（去掉它自己的复制逻辑），并接受 `--account`。

### 4.5 启动加载规则（`load_providers` 重写）

```rust
pub(crate) fn required_accounts(config: &Config, default: &str) -> BTreeSet<AccountKey>;
fn load_providers(config: &Config, default: &str, files: KeyFiles<'_>, issues: &Reporter, getenv: impl Fn(&str) -> Option<String>,
                  keychain: &dyn KeychainReader /* 生产：TimedKeychain（§2.6） */, managed: &ManagedStore) -> Result<LoadedProviders /* providers + unavailable */>;
```

> v0.9.1 起的规则（实现见 `hub.rs` 的 `required_accounts` / `load_providers`；下文取代 v0.9.0 最初设计里“每个 needed provider 的默认账号都必需”“fallbackAccounts 也必需”的写法）。

1. **required accounts**（加载失败 ⇒ 启动失败），`required_accounts(config, default)` 计算：
   - **默认 provider** 的默认账号（`providers.<id>.defaultAccount`，缺省 `default`）；
   - **全局默认模型（`defaultModel`）推断出的 provider** 的默认账号（`claude-*` ⇒ anthropic，否则为默认 provider）；
   - **每个 profile 自己的账号**：profile 解析出的 provider（`profile.provider` > 按 `profile.model`（缺省全局默认模型）推断 > 默认 provider）下，profile 显式写的 `account`，未写则为该 provider 的 `defaultAccount`；
   - `faux` 不计入。
   - **只经由 profile 使用、且这些 profile 都钉了命名账号的 provider，其默认账号不是必需的**（例如 `anthropic` 只被 `{"provider":"anthropic","account":"ci"}` 的 profile 使用 ⇒ 只要求 `anthropic/ci`，不要求 `anthropic/default`）。profile 不写 `account` 时该 provider 的默认账号重新变为必需。
   - `fallbackAccounts` 里的账号**不**自动成为必需（它们按 optional 加载，失败时 failover 跳过）。
   - required 账号在其 provider 下根本没有声明（例如 CLI `--provider` 改变了 profile 解析出的 provider，而 `profile.account` 只在原 provider 下定义）⇒ 启动失败：`account '<name>' is not configured for provider <id>`。
2. **失败的类型**：缺失/未找到的必需账号以 typed `KeyConfigError` 返回（`provider <id> is not configured (...)` / `<Label> API key not found for account '<name>' (...)` / `account '...' is not configured for provider ...`），文案只含 provider、账号、环境变量名与配置路径；`main.rs` 的 `startup_failure` 将其原样作为启动错误与 `startup_failed` issue 消息（details `{"reason": "key_config"}`）。Keychain 错误（含 v0.9.2 的 `Timeout`）保留为 typed `KeychainError`（details `{"reason": "keychain"}`）；其它来源错误（可能回显路径/读取细节）只显示通用文案。
3. **optional accounts**：provider 配置里其余显式声明的 `accounts.*`，以及非必需的 default 账号在“有显式来源”时（CLI 文件 / `*_API_KEY_FILE` / 配置声明 / `*_API_KEY`；**不**为了判定是否配置去探测受管存储或 Keychain）。
   - 加载成功 ⇒ 进 `providers`；
   - 加载失败 ⇒ `tracing::warn!(provider, account, error = %e, "account unavailable")` + issue `account_unavailable`（Warn，component `config`，details `{provider, account, reason}`，reason ∈ `not_found` / `source_error` / `keychain`），进 `unavailable`。
   - 非必需且没有显式来源的 default 账号 ⇒ 跳过（debug 日志）。
4. **`keys check` 使用同一规则**：调用同一个 `required_accounts`；所有声明的命名账号都检查；非必需且没有显式来源的 default 账号打印 `<provider>/default: skipped (not used by this configuration)`（`--provider` 显式筛选时不跳过）。任何失败 ⇒ 退出码 1。
5. **不懒加载**：启动后集合固定；运行期选择未加载账号直接报错（§4.6）。
6. provider 既没有必需账号也没有任何显式账号来源 ⇒ 跳过。
7. 每个加载成功的 key：`issues.add_secret(key.expose().to_owned())`；日志 `info!(provider, account, key_source = %src, "api key loaded")`。
8. 实例：`openai` ⇒ `OpenAiProvider::with_options(key, OpenAiOptions{ base_url: env <P>_BASE_URL(仅 default) > account.baseUrl > provider.baseUrl, organization, project, stream_idle_timeout })`；`anthropic` ⇒ 现有构造；`faux` ⇒ `("faux","default")`。

### 4.6 会话选择账号（`create_session_with_ttl`）

```rust
pub struct SessionRequest { ..., pub account: Option<String> }   // 新增

// 在 provider_id 确定之后：
let account = match req.account.as_deref() {
    Some(a) => {
        if !self.account_allowed(p, &provider_id, a) { bail!(AccountNotAllowed { account: a.into(), profile: profile_name.clone() }); }
        a.to_string()
    }
    None => self.profile_account(p, &provider_id).to_string(),
};
let provider = self.provider(&provider_id, &account)?;   // 加上 failover 包装，见 §5
```

`account_allowed(p, provider, a)`：
- `p.allowed_accounts == None` ⇒ 仅允许 `profile_account(p, provider)`；
- 列表含 `"*"` ⇒ 允许该 provider 下所有**已加载**账号；
- 否则 ⇒ `a` 在列表内（且 provider 匹配）。

```rust
#[derive(Debug)] pub struct AccountNotAllowed { pub account: String, pub profile: String }
// Display: "account 'work' is not allowed in profile 'coder' (allowedAccounts)"
```

`Hub::provider(id, account)` 错误：
- provider 未知：`unknown provider x`（现有）；
- `(id, account)` 在 `unavailable`：`account 'work' of provider openai is unavailable: <startup error>`；
- 不存在：`account 'work' is not configured for provider openai`（若 provider 连 default 都没加载则沿用 `not_configured(id)` 文案，并追加 `; accounts: [...]`）。

HTTP 映射（`http.rs::create_error`）：`AccountNotAllowed` ⇒ **403**；其余不变（400 / 429）。RPC：`{"success":false,"error":"..."}`；CLI run/rpc：启动时直接报错退出。

`/v1/chat` 的 `session.account` 走 SessionRequest；已有 `sessionId` 时忽略（账号是会话属性，不可中途修改，就像 provider）。

### 4.7 账号信息的对外暴露

- `SessionHandle`：`pub account: String`（配置/请求解析出的）；`SessionDynamic.account_key: AccountKey`，`SessionDynamic.catalog` 调用 `cached_models(&self.catalog, &self.account_key, ...)`。
- `session_view()`：加 `"account": h.account`。
- `/v1/chat` 响应、`run --json`、RPC `get_state`：加 `"account"`（= 最近一次调用实际使用的账号，failover 后可能不同于会话账号；来源 `AssistantMessage.account`，无调用时 = 会话账号）。
- `AssistantMessage` 新增 `#[serde(default, skip_serializing_if = "Option::is_none")] pub account: Option<String>`，由 §5 的包装 provider 在 `Done`/`Error` 上填充（基础 provider 不知道账号名，保持不动）。`usage` 字段本身不变，账号名与它并列。
- `IssueScope` 新增 `account: Option<String>`；`create_session` 填 `Some(account)`；failover 时 `SessionDynamic` 更新 scope.account 为当前账号。
- 日志：`session_run` span 加 `account`；`session created` 日志加 `account`；`api key loaded` 加 `account`。
- `/v1/profiles`：每个 profile 加 `"account": <profile_account>`, `"allowedAccounts": <resolved list>`（把 `None` 展开成 `[account]`，`["*"]` 展开成该 provider 已加载账号列表），`"fallbackAccounts": [...]`。
- `/v1/models`：`ModelsQuery` 加 `account: Option<String>`；`list_models_filtered(all, provider: Option<&str>, account: Option<&str>)`：
  - 无 `account` ⇒ 每个 provider 只查默认账号（目录通常一致，避免 N 倍请求）；
  - `account=work` ⇒ 要求同时给 `provider`（否则 400 `account filter requires provider`），只查该账号；
  - `ProviderModel` 加 `pub account: String`。
  - `sub-bot models --account` 同义。

### 4.8 向后兼容清单

| 旧配置/用法 | 新语义 |
|---|---|
| 顶层 `apiKeyFile` | `providers.openai.accounts.default.apiKeyFile` |
| `providers.<id>.apiKeyFile` / `baseUrl` | `accounts.default.apiKeyFile`；`baseUrl` 作为 provider 级默认 |
| `--openai-key-file` / `--anthropic-key-file` | 只作用于 default 账号，优先级不变 |
| `OPENAI_API_KEY[_FILE]` 等 | 只作用于 default 账号，优先级不变 |
| `OPENAI_BASE_URL` 等 | 只作用于 default 账号 |
| 响应/日志 | 多出 `account: "default"` 字段 |
| `Hub.providers` 测试里 `.insert("anthropic".into(), ...)` | 改为 `AccountKey::new("anthropic","default")` |
| `/v1/models` 输出 | 多 `account` 字段 |

---

## 5. 可选 failover（账号切换）

### 5.1 评估

当前链路：`agent_loop::stream_assistant` → `cfg.provider.stream(req)` → provider 内部 `retry::run_with_retries` 处理瞬态错误（429 是 transient，重试耗尽后以 `AssistantEvent::Error{message.error_message}` 结束流）。关键事实：**429 / insufficient_quota 在拿到 HTTP 状态时就失败，发生在 `AssistantEvent::Start` 之前**，因此此时流上除 `Retry` 外没有任何内容事件 ⇒ 切换账号重放请求对消费者是安全的（agent_loop 对 `Retry` 已有“丢弃部分消息”的语义）。

三个候选实现点：
1. `agent_core` 循环里重试 —— 要把账号概念带进 agent-core，侵入最大，否。
2. `retry.rs` 里支持多 key —— provider 内部结构是单 key `CallCtx`，改动中等且 Anthropic/OpenAI 各改一次，否。
3. **在 agentd 做一个 `Provider` 包装器**（`FailoverProvider`），持有有序账号链 `Vec<(String, Arc<dyn Provider>)>`，在流层面做切换 —— 不动 agent-core/agent-ai 的 provider 实现，只需 `AssistantMessage.account` 一个字段 + hub 对 `MessageRetry` 的 issue 分类。**选 3，可在 B 阶段一并完成；代价约 150 行。**

### 5.2 设计

```rust
/// agentd/src/accounts.rs（新文件）
pub struct Cooldowns(Mutex<HashMap<AccountKey, Instant>>);   // Hub 级共享：某账号被判定额度/限流后 cooldown 期间跳过
impl Cooldowns {
    pub fn mark(&self, key: &AccountKey, until: Instant);
    pub fn is_cooling(&self, key: &AccountKey, now: Instant) -> bool;
}

pub struct AccountSlot { pub key: AccountKey, pub provider: Arc<dyn Provider> }

pub struct FailoverProvider {
    chain: Vec<AccountSlot>,          // [主账号, fallback1, fallback2 ...]，长度 ≥ 1
    cooldowns: Arc<Cooldowns>,
    cooldown: Duration,               // providers.<id>.failoverCooldownSecs，默认 60
}

impl Provider for FailoverProvider {
    fn id(&self) -> &str { self.chain[0].provider.id() }
    fn list_models(&self) -> BoxFuture<...> { self.chain[0].provider.list_models() }
    fn stream(&self, req: Request, cancel: CancellationToken) -> EventStream {
        // 伪代码（用 async_stream 风格的 mpsc + spawn，与 openai.rs 一致）：
        // let order = 把 chain 按 cooldown 过滤（全在冷却中则不过滤，按原顺序）
        // let mut attempt = 0;
        // for (i, slot) in order.iter().enumerate() {
        //     let mut inner = slot.provider.stream(req.clone(), cancel.clone());
        //     let mut forwarded_content = false;
        //     while let Some(ev) = inner.next().await {
        //         match ev {
        //             AssistantEvent::Done{mut message} => { message.account = Some(slot.key.account.clone()); send(Done{message}); return; }
        //             AssistantEvent::Error{mut message} => {
        //                 message.account = Some(slot.key.account.clone());
        //                 let eligible = failover_eligible(message.error_message.as_deref().unwrap_or(""));
        //                 let has_next = i + 1 < order.len();
        //                 if eligible && !forwarded_content && has_next && !cancel.is_cancelled() {
        //                     cooldowns.mark(&slot.key, now + cooldown);
        //                     attempt += 1;
        //                     send(AssistantEvent::Retry{ attempt, reason: format!("account failover: {} -> {} ({})", slot.key.account, order[i+1].key.account, reason_class) });
        //                     break; // 下一个账号
        //                 }
        //                 send(Error{message}); return;
        //             }
        //             AssistantEvent::Retry{..} => send(ev),           // 内层瞬态重试原样透传
        //             other => { forwarded_content = true; send(other); }
        //         }
        //     }
        // }
    }
}

/// 429 / 额度类错误才切换（认证错误是配置问题，不切）：
pub fn failover_eligible(error: &str) -> bool {
    let (kind, _) = crate::hub::classify_llm_error(error);
    kind == "llm_rate_limit" || error.contains("insufficient_quota") || error.contains("exceeded your current quota")
}
```

- `Retry.reason` 固定前缀 `account failover: ` ⇒ `SessionHandle::report_event` 对 `MessageRetry` 分支：若 reason 以该前缀开头，上报 issue **kind `account_failover`**（Severity::Warn，component `llm`，details `{from, to, reason}`），并更新 `issue_scope.account = to`；否则保持 `llm_retry`。
- 单账号（无 fallback）时 hub 仍用 `FailoverProvider`（链长 1）以统一填充 `AssistantMessage.account`；开销可忽略（事件透传）。
- 链的来源：`profile.fallbackAccounts`（若设）否则 `accounts.<主账号>.fallbackAccounts`；只包含已加载账号（§4.5 已要求它们必须加载成功）。
- 内层 provider 自身的 429 重试（`llmMaxRetries`，默认 3 次指数退避）先耗尽再切换；README 提示：配置了 fallback 的 profile 可以把 `llmMaxRetries` 调低（如 1）以加快切换。
- Anthropic 的 `overloaded_error` 不触发切换（与账号无关）。
- 冷却：`is_cooling` 的账号排到链尾而不是剔除（全部冷却时仍按原序尝试），避免“所有账号都冷却 ⇒ 无账号可用”。
- `/v1/chat`、`run --json` 响应里的 `account` 即实际成功账号；`usage` 归属一目了然。

### 5.3 若时间不够的简化版

去掉流内切换，只做“**下一次调用起切换**”：`SessionHandle::report_event` 在收到 `llm_rate_limit` 类错误且有 fallback 时，把 `SessionDynamic.provider` 换成下一个账号的实例（`LoopConfig.provider` 需改成 `Shared<Arc<dyn Provider>>`，agent-core 一处改动）并上报 `account_failover`。当前调用仍失败。本设计推荐 5.2，5.3 仅作备选。

---

## 6. 测试计划

### 6.1 单元（agent-ai，无真实 Keychain）

`secrets.rs`
- `load_account_key_precedence_default_account`：cli > env file > config file / keychain(mock) / account env > env > managed（mock keychain 返回固定值；managed file 用 temp 目录注入——`managed_key_path` 需可注入根目录：加 `managed_key_path_in(root, p, a)`，生产版包一层）。
- `load_account_key_named_account_ignores_default_sources`：设置 `OPENAI_API_KEY`、cli_file，命名账号必须报 not found。
- `keychain_errors_never_include_contents`：mock 返回 `Denied/InteractionNotAllowed/Other` ⇒ Err 文案含 service/account 名，不含 key；`NotFound` 在第 3 级（配置声明）⇒ Err `keychain item not found`，在第 5 级 ⇒ 继续/not found。
- `keychain_value_is_trimmed_and_validated`：mock 返回 `" sk-x \n"` ⇒ `sk-x`；`"a b"` ⇒ Err only-the-key；空 ⇒ Err。
- `key_source_display_has_no_secret`。
- `write_key_file_sets_0600_and_private_dir`（unix）：mode 检查、拒绝 symlink 目录、覆盖写。
- `account_env_is_registered_for_subprocess_stripping`：加载后 `secret_env_vars()` 含该变量。

`openai.rs`
- `organization_and_project_headers_are_sent`：现有本地 TCP mock（`openai.rs` 测试里已有 socket 读 headers 的模式）断言 `OpenAI-Organization` / `OpenAI-Project` 出现在 chat、responses、`/models` 三类请求；未配置时不出现。

`keychain.rs`（macOS，`#[ignore]`）
- `real_keychain_roundtrip`：`set_generic("sub-bot-test", "openai/t-<pid>")` → `get` → `list_accounts` 含 → `delete` → `get` = NotFound。CI 不跑；本地 `cargo test -p agent-ai -- --ignored keychain`。

### 6.2 配置（agentd config.rs）

- `accounts_parse_and_legacy_fields_fold_into_default`：顶层 `apiKeyFile` / `providers.openai.apiKeyFile` ⇒ `effective_accounts()["default"]`；相对路径转换覆盖 `accounts.*.apiKeyFile`。
- `validate_rejects_*`：账号名非法、default 双写、三选一冲突、anthropic 带 organization、`defaultAccount` 未定义、`fallbackAccounts` 自引用/未定义、profile.account 未定义、allowedAccounts 不含自身、faux 带 accounts、`apiKeyEnv` 非法名。每条断言精确错误文案。
- `keychain_rejected_on_non_macos`（`#[cfg(not(target_os = "macos"))]`）与 `keychain_accepted_on_macos`（`#[cfg(target_os = "macos")]`）。
- `example_config_deserializes`：更新 `sub-bot.example.json` 后现有测试继续通过。

### 6.3 Hub（agentd hub.rs）

`load_providers` 测试注入 `KeychainReader` mock（`struct MockKeychain(HashMap<(String,String), Result<String, KeychainError>>)`）：
- `required_account_failure_fails_startup`（profile.account 指向加载失败的账号）。
- `optional_account_failure_is_warn_issue_and_marked_unavailable`：Reporter 收到 `account_unavailable`；`hub.provider("openai","x")` 错误含 `unavailable`。
- `loads_all_declared_accounts_not_lazily`：3 个账号全部出现在 `providers`。
- `all_loaded_keys_are_redacted`（扩展现有 `optional_keys_load_and_all_loaded_keys_are_redacted`：每个账号的 key 都被 `reporter.redact` 替换）。
- `session_account_selection_and_allowed_accounts`：缺省 ⇒ profile.account；请求越权 ⇒ `AccountNotAllowed`；`["*"]` 允许所有已加载；请求 provider 与 profile 不同且账号不存在 ⇒ 错误文案。
- `models_cache_is_per_account`：两个 `CatalogProvider` 不同目录，`list_models_filtered(_, Some("openai"), Some("work"))` 返回对应目录，默认只查 default。
- `session_view_and_scope_include_account`。
- `failover_switches_on_rate_limit_and_tags_account`：主账号 `RecordingProvider` 返回 `Error("OpenAI HTTP 429 ...insufficient_quota")`，fallback 返回正常 Done ⇒ 最终消息 `account == "backup"`，事件序列含 `MessageRetry{reason: "account failover: ..."}`，issue `account_failover` 一条；cooldown 内第二次调用直接从 backup 开始（主 provider `seen` 不增长）。
- `failover_not_triggered_on_auth_error_or_after_content`：401 不切换；主流先发 `TextDelta` 再 Error ⇒ 不切换。
- `single_account_chain_passes_through`。

### 6.4 HTTP / RPC / CLI

- `http.rs`：`POST /v1/sessions {"account":"x"}` 越权 ⇒ 403 + `{"error": "...allowedAccounts"}`；`/v1/profiles` 含 `account/allowedAccounts/fallbackAccounts`；`/v1/models?provider=openai&account=work` 与缺 provider 的 400；`/v1/chat` 响应 `account` 字段；已有 sessionId 时 `session.account` 被忽略。
- `rpc.rs`：`get_state` 含 `account`。
- `main.rs`：`Cli::try_parse_from(["sub-bot","--account","work","run","hi"])`；`keys set --provider openai --account work`、`keys list --json`、`keys delete`、`keys check` 解析；`--service` 在非 macOS 报错（单元测试 `keys::storage_target()` 纯函数）。
- `keys.rs`：`read_key_from_reader(non_tty)` trim/多段/空的判定；`set` 用 `--file` 到 temp 目录 ⇒ mode 0600、内容正确、输出片段含路径且不含 key；`list` 对 temp 受管目录枚举且输出不含 key（用 capture 断言不包含写入的假 key 字符串）。
- `probe.rs`：`--account` 选择的账号被用于构造 provider（mock 加载器）。

### 6.5 手工验收（macOS）

1. `sub-bot keys set --provider openai`（TTY 无回显）→ Keychain Access 可见 `sub-bot / openai/default`。
2. 配置 `apiKeyKeychain: {}`，`sub-bot keys check` ⇒ ok；`sub-bot run "hi"` 无弹窗。
3. `cp target/release/sub-bot /tmp/sb2 && /tmp/sb2 keys check` ⇒ 弹窗（或 ssh 下 Denied），验证文案。
4. `ssh localhost sub-bot keys check` 在锁定 keychain 时 ⇒ `interaction not allowed` 文案。
5. Linux：`keys set` ⇒ `~/.config/sub-bot/keys/openai-default.key` 0600，`run` 无配置也能找到（第 5 级来源），日志 `key_source=managed key file ...`。

---

## 7. 分阶段实现

### 阶段 A：key 来源抽象 + Keychain + `keys` CLI（不含多账号；对外行为仅新增）

| 文件 | 改动 |
|---|---|
| `Cargo.toml` (workspace) | `thiserror` 已在 workspace；版本 `0.9.0` |
| `crates/agent-ai/Cargo.toml` | `[target.'cfg(target_os = "macos")'.dependencies] security-framework = "3"`；`thiserror.workspace = true`；`directories.workspace = true`（managed_key_path） |
| `crates/agent-ai/src/keychain.rs`（新） | `KeychainError`、`KeychainReader`、`SystemKeychain`、`set_generic`/`delete_generic`/`list_accounts`；非 macOS stub（`SystemKeychain::get` ⇒ `Unsupported`，list ⇒ `Ok(vec![])`） |
| `crates/agent-ai/src/lib.rs` | `pub mod keychain;` |
| `crates/agent-ai/src/secrets.rs` | `KeySource` 新变体；`validate_key_text`；`managed_key_path[_in]`、`write_key_file`、`ensure_private_dir`；`AccountKeySpec` + `load_account_key`（本阶段 `account` 固定 `"default"`，但函数签名已是最终版）；`load_provider_key` 改为薄包装或删除 |
| `crates/agentd/src/config.rs` | `KeychainRef`；`ProviderConfig.api_key_keychain`；validate：三选一、非 macOS 拒绝 |
| `crates/agentd/src/hub.rs` | `load_providers` 改调 `load_account_key(spec, getenv, &SystemKeychain)`，参数加 `keychain: &dyn KeychainReader`（`with_provider_keys` 传 `&SystemKeychain`，测试传 mock）；日志加 `key_source` 的新变体 |
| `crates/agentd/src/probe.rs` | 改用 `load_account_key` |
| `crates/agentd/src/keys.rs`（新） | `KeysCmd` 实现：`read_hidden_line`（termios）、`read_key_from_reader`、`storage_target`、`set/list/delete/check`、配置片段输出 |
| `crates/agentd/src/main.rs` | `Cmd::Keys`，早于日志初始化分派（像 `HashToken`/`Ask`：stderr-only logging，不建 Hub）；`startup_failure` typed allowlist 加 `KeychainError` |
| `README.md` | §8 的「Keychain」「keys 子命令」「launchd/ssh」节；「API key」节加第 5 级来源 |
| `sub-bot.example.json` | 加注释式示例（JSON 无注释：在 README 示例中展示，example 里 anthropic 改 `apiKeyKeychain` 不合适因为 Linux 会校验失败——**example 保持 apiKeyFile**，Keychain 示例只放 README） |

### 阶段 B：多账号 + 请求覆盖 + 缓存 + failover

| 文件 | 改动 |
|---|---|
| `crates/agent-ai/src/types.rs` | `AssistantMessage.account: Option<String>`（`empty()` 置 None） |
| `crates/agent-ai/src/openai.rs` | `OpenAiOptions`、`with_options`、`extra_headers` 注入三处请求 |
| `crates/agent-ai/src/secrets.rs` | `load_account_key` 对非 default 账号的分支（跳过 CLI/env 来源）；`AccountEnv` 登记 |
| `crates/agentd/src/config.rs` | `AccountConfig`、`ProviderConfig.{default_account, accounts, failover_cooldown_secs}`、`effective_accounts()`、`Profile.{account, allowed_accounts, fallback_accounts}`、`load()` 相对路径、`validate()` 全部新规则 |
| `crates/agentd/src/accounts.rs`（新） | `AccountKey`（或放 hub.rs）、`Cooldowns`、`FailoverProvider`、`failover_eligible` |
| `crates/agentd/src/hub.rs` | `providers: BTreeMap<AccountKey, _>`、`unavailable`、`default_accounts`、`failover`；`load_providers` 的 needed/required/optional 规则；`cached_models` 按 `AccountKey`；`Hub::provider(id, account)`、`default_account`、`accounts_of`、`profile_account`、`account_allowed`；`SessionRequest.account`；`AccountNotAllowed`；`create_session_with_ttl` 选账号 + 包装 `FailoverProvider`；`SessionHandle.account`、`SessionDynamic.account_key`；`report_event` 的 `account_failover` 分类；`session_view`、`list_models_filtered(all, provider, account)`、`ProviderModel.account`；`session created`/`session_run` 日志加 account；`not_configured` 文案加账号列表 |
| `crates/agentd/src/issues.rs` | `IssueScope.account` |
| `crates/agentd/src/http.rs` | `create_error` 403 映射；`profiles()` 新字段；`ModelsQuery.account` + 校验；`collect_chat` body 加 `account` |
| `crates/agentd/src/rpc.rs` | `state()` 加 `account` |
| `crates/agentd/src/main.rs` | 全局 `--account`；`session_req` 传入；`models --account`；`run --json` 加 `account`；`allowed_by_profiles` 不变 |
| `crates/agentd/src/ask.rs` | `Ask`/client 透传 `account` 到 `session.account` |
| `crates/agentd/src/probe.rs` | `--account` |
| `crates/agentd/src/keys.rs` | `list` 合并配置声明的全部账号 |
| `README.md`、`sub-bot.example.json` | §8 |
| 现有测试 | `providers.insert("anthropic".into(), ..)` ⇒ `AccountKey`；`/v1/models` / `/v1/profiles` 断言加字段 |

建议顺序：A 合并并发布 0.9.0-alpha（Keychain 可独立使用），B 完成后发 0.9.0。

---

## 8. README 新增章节大纲与完整示例

### 8.1 大纲（插在「API key」节之后、「工具资源上限」之前）

```
## API key                                 （现有；补充：第 5 级“受管存储”、账号说明指向下文）
## 密钥存储：macOS Keychain 与 `sub-bot keys`（v0.9.0）
### 用 `sub-bot keys set` 保存 key          （TTY/管道两种输入；macOS→Keychain，Linux→~/.config/sub-bot/keys/*.key 0600；--file）
### 配置 apiKeyKeychain                      （命名约定 sub-bot / <provider>/<account>；自定义 service/account；非 macOS 报错；不支持 libsecret 的原因）
### keys list / delete / check               （输出样例；绝不打印 key）
### launchd、ssh 与 Keychain 授权            （ACL 归属；brew upgrade 后重新 keys set/check；锁定 keychain 的 -25308；LaunchAgent vs LaunchDaemon；启动失败即退出）
## 同一 provider 多账号（v0.9.0）
### 配置 accounts / defaultAccount           （旧写法 = default 账号；账号级 apiKeyFile|apiKeyKeychain|apiKeyEnv、baseUrl、organization/project）
### profile 的 account / allowedAccounts     （缺省只允许自身；`*`；越权 403）
### 会话/请求指定账号                        （/v1/sessions、/v1/chat session.account、rpc、run/ask --account、SUB_BOT_ACCOUNT）
### 响应与可观测性                           （account 字段；/v1/profiles；/v1/models?account=；issue scope；日志）
### 账号 failover（fallbackAccounts）         （触发条件 429/insufficient_quota；先耗尽 llmMaxRetries；cooldown；issue account_failover；建议调低 llmMaxRetries）
```

同时更新「Claude (Anthropic)」节里的优先级描述（加“以上均为 default 账号的规则”）和「安全提示」（受管 key 文件目录权限）。

### 8.2 完整配置示例（README 用；`sub-bot.example.json` 采用去掉 Keychain 的 Linux 可校验版本）

```json
{
  "dataDir": "/var/lib/sub-bot",
  "defaultProvider": "openai",
  "defaultModel": "gpt-6.1-sol",
  "providers": {
    "openai": {
      "apiKeyKeychain": {},
      "defaultAccount": "default",
      "failoverCooldownSecs": 120,
      "accounts": {
        "work": {
          "apiKeyKeychain": {},
          "organization": "org_abc123",
          "project": "proj_xyz789",
          "fallbackAccounts": ["default"]
        },
        "proxy": {
          "apiKeyEnv": "OPENAI_API_KEY_PROXY",
          "baseUrl": "http://localhost:9000/v1"
        }
      }
    },
    "anthropic": {
      "apiKeyFile": "/etc/sub-bot/anthropic.key",
      "defaultModel": "claude-opus-5-5",
      "accounts": {
        "ci": { "apiKeyKeychain": {"service": "sub-bot", "account": "anthropic/ci"} }
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
    {
      "name": "dev-bot",
      "tokenSha256": "REPLACE_ME",
      "profiles": ["coder", "reviewer", "claude-ci"]
    }
  ]
}
```

对应的准备步骤（README 里给出）：

```bash
sub-bot keys set --provider openai                      # default 账号 → Keychain sub-bot / openai/default
sub-bot keys set --provider openai --account work
sub-bot keys set --provider anthropic --account ci
sub-bot keys list
sub-bot keys check --config sub-bot.json                # 在 GUI 终端执行一次，完成 Keychain 授权
sub-bot --config sub-bot.json run --account proxy "hi"  # reviewer 未设 allowedAccounts ⇒ 只允许 default；coder 允许 proxy
```

请求示例：

```
POST /v1/sessions   {"profile":"coder","account":"proxy"}        → 201，view 含 "account":"proxy"
POST /v1/sessions   {"profile":"reviewer","account":"work"}      → 403 {"error":"account 'work' is not allowed in profile 'reviewer' (allowedAccounts)"}
POST /v1/chat       {"message":"hi","session":{"profile":"coder"}} → 200 {..., "account":"work", "usage":{...}}  （failover 后可能为 "default"）
GET  /v1/models?provider=openai&account=work
GET  /v1/profiles   → profiles[].account / allowedAccounts / fallbackAccounts
```

---

## 9. 未决/有意不做

- libsecret / Windows Credential Manager：不做（musl 静态、无 GUI 服务器、Windows 不支持）。
- 运行期热加载新账号、懒加载：不做；改配置需重启。
- 账号级 `defaultModel` / `allowedModels`：不做，模型策略留在 profile。
- `OPENAI_API_KEY_<ACCOUNT>` 自动约定：不做，用显式 `apiKeyEnv`，避免隐式变量名进入子进程剔除规则之外。
- Anthropic 的 failover 触发同 OpenAI（`rate_limit_error`/429）；`overloaded_error` 不切换。
