//! 配置文件（JSON）：profiles（预设）+ clients（每个 bot 一个 token，映射到允许的 profile）。
//! 示例见仓库根目录 `sub-bot.example.json`。
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Once,
    },
};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub provider: Option<String>,
    /// 替换默认编码 agent 前言（对应 pi `customPrompt` / `--system-prompt`）
    pub system_prompt: Option<String>,
    /// 追加到系统提示末尾（对应 pi `appendSystemPrompt`）
    pub append_system_prompt: Option<String>,
    pub model: Option<String>,
    /// 允许调用方选择的模型（精确名或 `*` 通配，如 `gpt-5*`）；不填则不限制
    #[serde(default)]
    pub allowed_models: Option<Vec<String>>,
    /// 默认推理强度：none | minimal | low | medium | high | xhigh；不填则不发送
    #[serde(default)]
    pub reasoning_effort: Option<agent_ai::ThinkingLevel>,
    /// OpenAI API：auto | chat | responses；不填用全局 defaultApi
    #[serde(default)]
    pub api: Option<agent_ai::Api>,
    /// 单次模型调用的输出 token 上限（含推理 token）
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// 覆盖全局 LLM 瞬时错误重试次数；0 禁用。
    #[serde(default)]
    pub llm_max_retries: Option<u32>,
    /// 覆盖流空闲超时（秒，1..=7200）；优先级：请求 > profile > 账号 > provider > 全局。
    #[serde(default)]
    pub stream_idle_timeout_secs: Option<u64>,
    /// 单次运行（一个 prompt）的总 token 预算；达到后让模型不带工具给出最终答复
    #[serde(default)]
    pub token_budget: Option<u64>,
    /// 单次运行超时（秒）；默认不限时，超时等同 abort。
    #[serde(default)]
    pub run_timeout_secs: Option<u64>,
    /// 工具白名单：工具名或 `preset:<name>`；默认是 pi 的编码工具集
    #[serde(default = "default_tools")]
    pub tools: Vec<String>,
    /// 允许作为会话 cwd 的根目录；会话 cwd 必须位于其中之一（第一个是默认 cwd）
    pub workspace_roots: Vec<PathBuf>,
    /// 是否从 cwd 向上读取 AGENTS.md / CLAUDE.md 注入系统提示（对应 pi resource-loader.ts）
    #[serde(default = "yes")]
    pub load_context_files: bool,
    #[serde(default)]
    pub http_allow_hosts: Vec<String>,
    /// bash 子进程的环境变量策略：`{mode: inherit|allowlist, deny, allow}`；默认 inherit 并剔除内置密钥变量
    #[serde(default)]
    pub shell_env: agent_tools::ShellEnv,
    /// 是否允许调用方在会话里覆盖 system prompt
    #[serde(default)]
    pub allow_system_prompt_override: bool,
    /// 是否允许调用方注册回调工具
    #[serde(default)]
    pub allow_caller_tools: bool,
    /// 单次 run 的模型调用上限（回合预算，>= 1，默认 150）；调用方可按会话覆盖、运行期调整（没有上限封顶）
    #[serde(default = "default_max_turns")]
    pub max_turns: u64,
    /// 达到 maxTurns 时："stop"（默认，直接结束）| "ask"（暂停等待调用方延长或停止）| "summarize"（再给一次不带工具的总结调用）
    #[serde(default)]
    pub on_max_turns: agent_core::OnMaxTurns,
    /// onMaxTurns=ask 时等待调用方的秒数（>= 1，默认 600）；超时停止 run
    #[serde(default = "default_max_turns_ask_timeout_secs")]
    pub max_turns_ask_timeout_secs: u64,
    /// 深度模型（depth=deep 或自动路由判定为 deep 时使用）。anthropic profile 默认固定为 `claude-fable-5-1`；
    /// 也可写 `latest:<prefix>`，按 provider 模型目录解析为该前缀下版本号最新的模型。
    #[serde(default)]
    pub deep_model: Option<String>,
    /// 默认 depth=auto（第一次 prompt 时用 routerModel 做一次分类）；false 时默认 standard。
    #[serde(default)]
    pub auto_route: bool,
    /// 自动路由分类用的模型；默认是 profile 的基础模型。
    #[serde(default)]
    pub router_model: Option<String>,
    /// 会话默认账号（profile 解析出的 provider 下）；缺省 = providers.<id>.defaultAccount 或 "default"
    #[serde(default)]
    pub account: Option<String>,
    /// 调用方可以选择的账号；缺省只允许自身 account；`["*"]` = 该 provider 所有已加载账号
    #[serde(default)]
    pub allowed_accounts: Option<Vec<String>>,
    /// 主账号 429 / 额度耗尽时依次切换的账号（覆盖账号级 fallbackAccounts）
    #[serde(default)]
    pub fallback_accounts: Option<Vec<String>>,
    /// 工具调用审批："never"（默认）| "always" | `{default, tools, bash, timeoutSecs, onTimeout}`；不填用全局 toolApproval
    #[serde(default)]
    pub tool_approval: Option<agent_tools::ToolApproval>,
    /// 覆盖模型的上下文窗口（token，>= 1000），用于自动压缩阈值；默认按模型表（agent_ai::context_window）
    #[serde(default)]
    pub context_window: Option<u64>,
    /// 覆盖全局 `compaction`（逐字段）
    #[serde(default)]
    pub compaction: Option<crate::compaction::CompactionConfig>,
    /// 该 profile 可用的 MCP 服务器：全局 `mcpServers` 的名字、`{name, tools}`（引用并收窄工具）或内联定义；不填 = 没有 MCP 工具
    #[serde(default)]
    pub mcp_servers: Vec<crate::mcp::ProfileMcpServer>,
}

fn default_tools() -> Vec<String> {
    agent_tools::DEFAULT_TOOLS.iter().map(|s| s.to_string()).collect()
}
fn yes() -> bool {
    true
}
pub fn default_max_turns() -> u64 {
    150
}
pub fn default_max_turns_ask_timeout_secs() -> u64 {
    agent_core::turn_budget::DEFAULT_ASK_TIMEOUT_SECS
}

pub const MAX_TARGETS: usize = 10;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TargetSpec {
    Url(String),
    Full {
        url: String,
        #[serde(rename = "secretFile")]
        secret_file: Option<PathBuf>,
        events: Option<Vec<String>>,
        #[serde(rename = "minSeverity")]
        min_severity: Option<crate::issues::Severity>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum Targets {
    #[default]
    #[serde(skip)]
    None,
    One(TargetSpec),
    Many(Vec<TargetSpec>),
}
impl Targets {
    pub fn specs(&self) -> Vec<&TargetSpec> {
        match self {
            Self::None => vec![],
            Self::One(t) => vec![t],
            Self::Many(ts) => ts.iter().collect(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.specs().is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub target: crate::webhook::WebhookTarget,
    pub events: Option<Vec<String>>,
    pub min_severity: Option<crate::issues::Severity>,
}
impl ResolvedTarget {
    pub fn wants(&self, event: &str) -> bool {
        self.events.as_ref().map_or(event == "task.completed", |events| events.iter().any(|e| e == event))
    }
    pub fn wants_issue(&self, sev: crate::issues::Severity) -> bool {
        self.wants("issues") && sev >= self.min_severity.unwrap_or(crate::issues::Severity::Warn)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AlertsConfig {
    pub webhook_url: Targets,
    pub webhook_secret_file: Option<PathBuf>,
    pub min_severity: crate::issues::Severity,
    pub dedup_window_secs: u64,
    pub queue_capacity: usize,
    #[serde(skip)]
    pub resolved: Vec<ResolvedTarget>,
}
impl Default for AlertsConfig {
    fn default() -> Self {
        Self { webhook_url: Targets::None, webhook_secret_file: None, min_severity: crate::issues::Severity::Error, dedup_window_secs: 60, queue_capacity: 1000, resolved: vec![] }
    }
}
impl AlertsConfig {
    pub fn resolve(&mut self, dir: Option<&std::path::Path>) -> anyhow::Result<()> {
        let secret = self.webhook_secret_file.as_deref().map(|p| read_secret(p, dir)).transpose()?;
        self.resolved = resolve_targets(&self.webhook_url, secret.as_deref(), dir)?;
        for target in &mut self.resolved {
            target.min_severity = Some(target.min_severity.unwrap_or(self.min_severity));
        }
        Ok(())
    }
}
fn read_secret(path: &std::path::Path, dir: Option<&std::path::Path>) -> anyhow::Result<String> {
    let path = if path.is_relative() { dir.unwrap_or_else(|| std::path::Path::new(".")).join(path) } else { path.to_owned() };
    let secret = std::fs::read_to_string(path).map_err(|_| anyhow::anyhow!("cannot read webhookSecretFile"))?;
    anyhow::ensure!(!secret.trim().is_empty(), "webhookSecretFile is empty");
    Ok(secret.trim().to_owned())
}
fn validate_targets(targets: &Targets) -> anyhow::Result<()> {
    anyhow::ensure!(targets.specs().len() <= MAX_TARGETS, "webhookUrl exceeds MAX_TARGETS (10)");
    for spec in targets.specs() {
        let url = match spec {
            TargetSpec::Url(url) | TargetSpec::Full { url, .. } => url,
        };
        anyhow::ensure!(webhook_url(url).is_some(), "webhookUrl must be an http/https URL");
    }
    Ok(())
}
fn resolve_targets(targets: &Targets, default_secret: Option<&str>, dir: Option<&std::path::Path>) -> anyhow::Result<Vec<ResolvedTarget>> {
    validate_targets(targets)?;
    targets
        .specs()
        .into_iter()
        .map(|spec| {
            let (url, secret, events, min_severity) = match spec {
                TargetSpec::Url(url) => (url, default_secret.map(str::to_owned), None, None),
                TargetSpec::Full { url, secret_file, events, min_severity } => {
                    (url, secret_file.as_deref().map(|p| read_secret(p, dir)).transpose()?.or_else(|| default_secret.map(str::to_owned)), events.clone(), *min_severity)
                }
            };
            Ok(ResolvedTarget { target: crate::webhook::WebhookTarget { url: url.clone(), secret }, events, min_severity })
        })
        .collect()
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub name: String,
    /// token 的 SHA-256（hex）。用 `sub-bot hash-token <token>` 生成，配置文件里不存明文。
    pub token_sha256: String,
    pub profiles: Vec<String>,
    #[serde(default)]
    pub webhook_url: Targets,
    #[serde(skip)]
    pub resolved_webhooks: Vec<ResolvedTarget>,
    pub webhook_secret_file: Option<PathBuf>,
    #[serde(default)]
    pub webhook_allowed_hosts: Vec<String>,
    #[serde(skip)]
    pub webhook_secret: Option<String>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("name", &self.name).field("profiles", &self.profiles).field("webhook_secret", &"<redacted>").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalClient {
    pub uid: Option<u32>,
    pub username: Option<String>,
    #[serde(default)]
    pub any: bool,
    pub client: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustLoopback {
    pub client: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    pub data_dir: Option<PathBuf>,
    pub unix_socket: Option<PathBuf>,
    pub unix_socket_mode: Option<String>,
    #[serde(default)]
    pub local_clients: Vec<LocalClient>,
    pub trust_loopback: Option<TrustLoopback>,
    #[serde(default = "default_shutdown_grace")]
    pub shutdown_grace_secs: u64,
    #[serde(default)]
    pub alerts: AlertsConfig,
    #[serde(default = "default_session_idle_ttl")]
    pub task_ttl_secs: u64,
    #[serde(default = "default_webhook_attempts")]
    pub webhook_max_attempts: u32,
    /// LLM 流连续无字节的超时（秒，1..=7200，默认 1800）；provider / 账号 / profile / 请求可逐级覆盖。
    #[serde(default = "default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
    #[serde(default = "default_llm_max_retries")]
    pub llm_max_retries: u32,
    /// 每次 macOS Keychain 读取的等待上限（秒，>= 1）；超时 ⇒ `KeychainError::Timeout`（多半是授权对话框无人响应）。
    #[serde(default = "default_keychain_timeout_secs")]
    pub keychain_timeout_secs: u64,
    /// /v1/chat 非流式等待上限（秒）。
    #[serde(default = "default_chat_timeout_secs")]
    pub chat_timeout_secs: u64,
    #[serde(default = "default_model")]
    pub default_model: String,
    /// 所有 profile 的默认推理强度（profile 自己的 reasoningEffort 优先）
    #[serde(default)]
    pub default_reasoning_effort: Option<agent_ai::ThinkingLevel>,
    /// 默认 OpenAI API：auto（默认，gpt-5.4+/gpt-6.x/o 系列走 Responses）| chat | responses
    #[serde(default)]
    pub default_api: agent_ai::Api,
    /// OpenAI API key 文件（相对路径相对于配置文件所在目录）。优先级低于 --openai-key-file 和 OPENAI_API_KEY_FILE，高于 OPENAI_API_KEY
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    /// 普通会话空闲过期秒数，0 禁用。
    #[serde(default = "default_session_idle_ttl")]
    pub session_idle_ttl_secs: u64,
    /// 自动创建的 chat 会话空闲过期秒数，0 禁用。
    #[serde(default = "default_chat_session_ttl")]
    pub chat_session_ttl_secs: u64,
    /// 每个 client 的内存会话上限，0 不限制。
    #[serde(default = "default_max_sessions")]
    pub max_sessions_per_client: usize,
    /// 未设置 toolApproval 的 profile 使用的工具审批策略；默认 "never"
    #[serde(default)]
    pub tool_approval: Option<agent_tools::ToolApproval>,
    /// 会话 JSONL 目录（按 client 分子目录）；默认 `<dataDir>/sessions`。`--session-dir` 优先
    #[serde(default)]
    pub session_dir: Option<PathBuf>,
    /// false 关闭会话持久化（serve / run / rpc）；默认 true。`--no-persist-sessions` 同效
    #[serde(default)]
    pub persist_sessions: Option<bool>,
    /// 上下文压缩默认设置（profile 的 `compaction` 逐字段覆盖）
    #[serde(default)]
    pub compaction: crate::compaction::CompactionConfig,
    /// MCP 服务器定义（名字 → stdio `command` / HTTP `url`）；profile 通过 `mcpServers` 引用，见 mcp/config.rs
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, crate::mcp::McpServerConfig>,
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub clients: Vec<Client>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub api_key_file: Option<PathBuf>,
    /// macOS Keychain generic password（与 apiKeyFile 二选一）；`{}` = service `sub-bot`、account `<provider>/default`
    #[serde(default)]
    pub api_key_keychain: Option<KeychainRef>,
    /// provider 级 baseUrl：账号未设置 baseUrl 时使用
    pub base_url: Option<String>,
    /// 该 provider 在没有配置/请求模型时的默认模型（anthropic 默认 `claude-opus-5-5`）
    #[serde(default)]
    pub default_model: Option<String>,
    /// 没有指定账号时使用的账号；默认 "default"
    #[serde(default)]
    pub default_account: Option<String>,
    /// 命名账号；旧的 apiKeyFile / apiKeyKeychain（以及 openai 的顶层 apiKeyFile）等价于账号 "default"
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountConfig>,
    /// failover 后主账号的冷却时间（秒），默认 60
    #[serde(default)]
    pub failover_cooldown_secs: Option<u64>,
    /// 该 provider 所有账号的流空闲超时（秒，1..=7200）；覆盖全局，账号级优先
    #[serde(default)]
    pub stream_idle_timeout_secs: Option<u64>,
}

/// 同一 provider 下的一个账号：一份凭据 + 可选连接参数。key 来源三选一，都省略时使用受管存储（`sub-bot keys set`）。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountConfig {
    pub api_key_file: Option<PathBuf>,
    pub api_key_keychain: Option<KeychainRef>,
    /// 账号级环境变量名（会登记到子进程剔除列表）
    pub api_key_env: Option<String>,
    /// 覆盖 provider 级 baseUrl
    pub base_url: Option<String>,
    /// 仅 openai：OpenAI-Organization 请求头
    pub organization: Option<String>,
    /// 仅 openai：OpenAI-Project 请求头
    pub project: Option<String>,
    /// 本账号 429 / 额度耗尽时依次切换的账号
    #[serde(default)]
    pub fallback_accounts: Vec<String>,
    /// 本账号的流空闲超时（秒，1..=7200）；覆盖 provider 级与全局
    pub stream_idle_timeout_secs: Option<u64>,
}

pub const DEFAULT_FAILOVER_COOLDOWN_SECS: u64 = 60;

impl ProviderConfig {
    pub fn default_account(&self) -> &str {
        self.default_account.as_deref().unwrap_or(agent_ai::secrets::DEFAULT_ACCOUNT)
    }
}

/// 配置里的 Keychain 条目引用；省略的字段用约定值（service `sub-bot`，account `<provider>/<account>`）。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KeychainRef {
    pub service: Option<String>,
    pub account: Option<String>,
}

impl KeychainRef {
    pub const DEFAULT_SERVICE: &'static str = agent_ai::keychain::DEFAULT_SERVICE;

    /// 解析为实际的 (service, account)；缺省 account = "<provider>/<account_name>"
    pub fn resolve(&self, provider: &str, account_name: &str) -> (String, String) {
        (self.service.clone().unwrap_or_else(|| Self::DEFAULT_SERVICE.into()), self.account.clone().unwrap_or_else(|| agent_ai::keychain::managed_account(provider, account_name)))
    }

    fn validate(&self, at: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            agent_ai::keychain::supported(),
            "{at}: apiKeyKeychain is only supported on macOS (this build targets {}); use apiKeyFile or `sub-bot keys set` (writes a 0600 key file)",
            std::env::consts::OS
        );
        for (name, value) in [("service", &self.service), ("account", &self.account)] {
            anyhow::ensure!(value.as_ref().is_none_or(|v| !v.trim().is_empty()), "{at}: apiKeyKeychain.{name} must not be blank");
        }
        Ok(())
    }
}

/// fallbackAccounts：每项必须存在于该 provider、不等于自身、不重复。
fn check_fallbacks(at: &str, own: &str, list: &[String], accounts: &BTreeMap<String, AccountConfig>) -> anyhow::Result<()> {
    for (i, a) in list.iter().enumerate() {
        anyhow::ensure!(accounts.contains_key(a), "{at}: fallbackAccounts entry '{a}' is not defined in accounts");
        anyhow::ensure!(a != own, "{at}: fallbackAccounts must not include the account itself ('{own}')");
        anyhow::ensure!(!list[..i].contains(a), "{at}: fallbackAccounts has duplicate entry '{a}'");
    }
    Ok(())
}

pub fn validate_provider(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(matches!(id, "openai" | "anthropic" | "faux"), "unknown provider {id}");
    Ok(())
}

fn default_shutdown_grace() -> u64 {
    10
}

fn default_webhook_attempts() -> u32 {
    6
}

fn default_session_idle_ttl() -> u64 {
    3600
}
fn default_chat_session_ttl() -> u64 {
    600
}
fn default_max_sessions() -> usize {
    64
}
fn default_llm_max_retries() -> u32 {
    3
}

fn default_stream_idle_timeout_secs() -> u64 {
    agent_ai::DEFAULT_STREAM_IDLE_TIMEOUT_SECS
}

/// `key` 是完整配置路径（如 `providers.openai.accounts.work.streamIdleTimeoutSecs`），出错时原样出现在错误里。
pub fn check_stream_idle_timeout(key: &str, secs: u64) -> anyhow::Result<()> {
    agent_ai::validate_stream_idle_timeout_secs(secs).map(drop).map_err(|e| anyhow::anyhow!("{}", e.replacen("streamIdleTimeoutSecs", key, 1)))
}
fn default_chat_timeout_secs() -> u64 {
    900
}
pub(crate) fn default_keychain_timeout_secs() -> u64 {
    agent_ai::keychain::DEFAULT_TIMEOUT_SECS
}

fn default_model() -> String {
    DEFAULT_OPENAI_MODEL.into()
}

/// 默认 OpenAI 模型（全局 defaultModel 的默认值）。
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-6.1-sol";
/// 内置 default profile（无配置文件、provider=openai）默认只允许的 OpenAI 模型；需要其它模型时在 profile 的 allowedModels 中显式添加。
pub const DEFAULT_OPENAI_ALLOWED_MODELS: &[&str] = &["gpt-6.1-sol", "gpt-6-astra"];

/// provider=anthropic 且没有配置/请求模型时使用的默认 Claude 模型。
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";
/// anthropic profile 的默认深度模型（固定版本；需要跟随最新版时显式配置 `latest:claude-fable`）。
pub const DEFAULT_CLAUDE_DEEP_MODEL: &str = "claude-fable-5-1";

/// URL errors deliberately omit the input (it may contain credentials).
pub fn webhook_url(value: &str) -> Option<reqwest::Url> {
    reqwest::Url::parse(value).ok().filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some() && url.username().is_empty() && url.password().is_none())
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

static DATA_DIR_FALLBACK_USED: AtomicBool = AtomicBool::new(false);
static DATA_DIR_FALLBACK_WARNING: Once = Once::new();

/// Logging may not yet be initialized when the default log directory is resolved.
pub(crate) fn warn_data_dir_fallback() {
    if DATA_DIR_FALLBACK_USED.load(Ordering::Relaxed) && tracing::enabled!(tracing::Level::WARN) {
        DATA_DIR_FALLBACK_WARNING.call_once(|| {
            tracing::warn!("system data directory unavailable; using a fallback directory; set --data-dir or dataDir to select a persistent data directory");
        });
    }
}

fn effective_uid() -> u32 {
    #[cfg(unix)]
    // SAFETY: geteuid has no arguments or memory preconditions.
    unsafe {
        libc::geteuid()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

fn temp_data_dir(tmp: PathBuf, euid: u32) -> PathBuf {
    #[cfg(unix)]
    {
        tmp.join(format!("sub-bot-{euid}"))
    }
    #[cfg(not(unix))]
    {
        let _ = euid;
        tmp.join("sub-bot")
    }
}

fn resolve_default_data_dir(base: Option<PathBuf>, xdg: Option<OsString>, tmp: PathBuf, euid: u32) -> PathBuf {
    if let Some(base) = base {
        return base.join("sub-bot");
    }
    if let Some(xdg) = xdg.filter(|p| !p.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute()) {
        return xdg.join("sub-bot");
    }
    temp_data_dir(tmp, euid)
}

/// Protect the predictable temporary fallback before any child is created/opened.
/// Explicit directories outside this tree retain their existing permission policy.
pub(crate) fn prepare_temp_data_dir(path: &Path, create: bool) -> anyhow::Result<()> {
    let euid = effective_uid();
    let root = temp_data_dir(std::env::temp_dir(), euid);
    if path.starts_with(&root) {
        check_temp_data_dir(&root, euid, create)?;
    }
    Ok(())
}

fn check_temp_data_dir(root: &Path, euid: u32, create: bool) -> anyhow::Result<()> {
    use anyhow::Context;
    if create {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        // Do not chmod an existing directory: reject hostile pre-created directories unchanged.
        match builder.create(root) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("cannot create fallback data directory {}", root.display())),
        }
    }
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cannot inspect fallback data directory {}", root.display())),
    };
    anyhow::ensure!(meta.is_dir() && !meta.file_type().is_symlink(), "unsafe fallback data directory {}: must be a directory, not a symlink", root.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(meta.uid() == euid, "unsafe fallback data directory {}: not owned by current euid {euid}", root.display());
        anyhow::ensure!(meta.mode() & 0o022 == 0, "unsafe fallback data directory {}: group/world writable", root.display());
    }
    #[cfg(not(unix))]
    let _ = euid;
    Ok(())
}

impl Config {
    pub fn default_data_dir() -> PathBuf {
        let base = directories::BaseDirs::new().map(|dirs| dirs.data_local_dir().to_owned());
        if base.is_none() {
            DATA_DIR_FALLBACK_USED.store(true, Ordering::Relaxed);
            warn_data_dir_fallback();
        }
        resolve_default_data_dir(base, std::env::var_os("XDG_DATA_HOME"), std::env::temp_dir(), effective_uid())
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(Self::default_data_dir)
    }

    /// 会话持久化目录：`persistSessions: false` ⇒ None；否则 `sessionDir` 或 `<dataDir>/sessions`。
    pub fn sessions_dir(&self) -> Option<PathBuf> {
        if self.persist_sessions == Some(false) {
            return None;
        }
        Some(self.session_dir.clone().unwrap_or_else(|| self.data_dir().join("sessions")))
    }

    pub fn socket_mode(&self) -> anyhow::Result<u32> {
        let value = self.unix_socket_mode.as_deref().unwrap_or("0600");
        anyhow::ensure!(!value.is_empty() && value.bytes().all(|b| matches!(b, b'0'..=b'7')), "unixSocketMode must be an octal permission string");
        let mode = u32::from_str_radix(value, 8)?;
        anyhow::ensure!(mode <= 0o777, "unixSocketMode must be <= 0777");
        Ok(mode)
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let mut cfg: Self = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        if let (Some(f), Some(dir)) = (&cfg.api_key_file, path.parent()) {
            if f.is_relative() {
                cfg.api_key_file = Some(dir.join(f));
            }
        }
        for provider in cfg.providers.values_mut() {
            let files = provider.api_key_file.iter_mut().chain(provider.accounts.values_mut().filter_map(|a| a.api_key_file.as_mut()));
            for file in files {
                if file.is_relative() {
                    *file = path.parent().unwrap_or_else(|| Path::new(".")).join(&*file);
                }
            }
        }
        for client in &mut cfg.clients {
            if let Some(file) = &mut client.webhook_secret_file {
                if file.is_relative() {
                    *file = path.parent().unwrap_or_else(|| std::path::Path::new(".")).join(&*file);
                }
                let secret = std::fs::read_to_string(file).map_err(|_| anyhow::anyhow!("cannot read webhookSecretFile"))?;
                anyhow::ensure!(!secret.trim().is_empty(), "webhookSecretFile is empty");
                client.webhook_secret = Some(secret.trim().to_string());
            }
            client.resolved_webhooks = resolve_targets(&client.webhook_url, client.webhook_secret.as_deref(), path.parent())?;
            for target in &client.resolved_webhooks {
                if let Some(url) = webhook_url(&target.target.url) {
                    let host = url.host_str().unwrap_or_default().to_owned();
                    if !client.webhook_allowed_hosts.contains(&host) {
                        client.webhook_allowed_hosts.push(host);
                    }
                }
            }
        }
        cfg.alerts.resolve(path.parent())?;
        crate::mcp::config::resolve_paths(&mut cfg.mcp_servers, &mut cfg.profiles, path.parent().unwrap_or_else(|| Path::new(".")));
        cfg.validate()?;
        Ok(cfg)
    }

    /// 只做结构/注册表校验，不访问工作目录或网络。
    pub fn validate(&self) -> anyhow::Result<()> {
        self.socket_mode()?;
        anyhow::ensure!(self.keychain_timeout_secs >= 1, "keychainTimeoutSecs must be >= 1");
        check_stream_idle_timeout("streamIdleTimeoutSecs", self.stream_idle_timeout_secs)?;
        for id in self.default_provider.iter().chain(self.providers.keys()).chain(self.profiles.values().filter_map(|p| p.provider.as_ref())) {
            validate_provider(id)?;
        }
        if let (Some(top), Some(alias)) = (&self.api_key_file, self.providers.get("openai").and_then(|p| p.api_key_file.as_ref())) {
            anyhow::ensure!(top == alias, "apiKeyFile and providers.openai.apiKeyFile must match");
        }
        for (id, provider) in &self.providers {
            let at = format!("providers.{id}");
            if let Some(secs) = provider.stream_idle_timeout_secs {
                check_stream_idle_timeout(&format!("{at}.streamIdleTimeoutSecs"), secs)?;
            }
            if let Some(keychain) = &provider.api_key_keychain {
                anyhow::ensure!(id != "faux", "{at}: apiKeyKeychain is not supported for faux");
                let file = provider.api_key_file.is_some() || (id == "openai" && self.api_key_file.is_some());
                anyhow::ensure!(!file, "{at}: set only one of apiKeyFile / apiKeyKeychain");
                keychain.validate(&at)?;
            }
            self.validate_accounts(id, provider)?;
        }
        for mapping in &self.local_clients {
            anyhow::ensure!(
                usize::from(mapping.uid.is_some()) + usize::from(mapping.username.is_some()) + usize::from(mapping.any) == 1,
                "localClients entry must select exactly one of uid, username, any:true"
            );
            anyhow::ensure!(self.clients.iter().any(|c| c.name == mapping.client), "localClients: unknown client '{}'", mapping.client);
        }
        if let Some(trust) = &self.trust_loopback {
            anyhow::ensure!(self.clients.iter().any(|c| c.name == trust.client), "trustLoopback: unknown client '{}'", trust.client);
        }
        self.compaction.validate("compaction")?;
        let registry = agent_tools::ToolRegistry::with_builtins();
        if let Some(policy) = &self.tool_approval {
            policy.validate().map_err(anyhow::Error::msg)?;
        }
        for (name, profile) in &self.profiles {
            anyhow::ensure!(!profile.workspace_roots.is_empty(), "profile '{name}': workspaceRoots must not be empty");
            anyhow::ensure!(profile.max_turns >= 1, "profile '{name}': maxTurns must be >= 1");
            anyhow::ensure!(profile.max_turns_ask_timeout_secs >= 1, "profile '{name}': maxTurnsAskTimeoutSecs must be >= 1");
            if let Some(secs) = profile.stream_idle_timeout_secs {
                check_stream_idle_timeout(&format!("profiles.{name}.streamIdleTimeoutSecs"), secs)?;
            }
            if let Some(policy) = &profile.tool_approval {
                policy.validate().map_err(|e| anyhow::anyhow!("profile '{name}': {e}"))?;
            }
            if let Some(c) = &profile.compaction {
                c.validate(&format!("profile '{name}': compaction"))?;
            }
            if let Some(w) = profile.context_window {
                anyhow::ensure!(w >= crate::compaction::MIN_CONTEXT_WINDOW, "profile '{name}': contextWindow must be >= {}", crate::compaction::MIN_CONTEXT_WINDOW);
            }
            self.validate_profile_accounts(name, profile)?;
            if let Some(deep) = &profile.deep_model {
                anyhow::ensure!(!deep.trim().is_empty() && deep.strip_prefix("latest:").is_none_or(|p| !p.trim().is_empty()), "profile '{name}': deepModel must be a model id or latest:<prefix>");
            }
            for tool in &profile.tools {
                let tools = match tool.strip_prefix("preset:") {
                    Some(preset) => agent_tools::preset(preset).ok_or_else(|| anyhow::anyhow!("profile '{name}': unknown preset '{preset}'"))?,
                    None => vec![tool.clone()],
                };
                for tool in tools {
                    anyhow::ensure!(registry.risk(&tool).is_some(), "profile '{name}': unknown tool '{tool}'");
                }
            }
        }
        crate::mcp::config::plan(&self.mcp_servers, &self.profiles)?;
        validate_targets(&self.alerts.webhook_url)?;
        for client in &self.clients {
            validate_targets(&client.webhook_url)?;
            anyhow::ensure!(client.token_sha256.len() == 64 && client.token_sha256.bytes().all(|b| b.is_ascii_hexdigit()), "client '{}': tokenSha256 must be exactly 64 hex characters", client.name);
            for profile in &client.profiles {
                anyhow::ensure!(self.profiles.contains_key(profile), "client '{}': unknown profile '{profile}'", client.name);
            }
        }
        Ok(())
    }

    /// provider 的完整账号表：旧字段（`providers.<id>.apiKeyFile` / `apiKeyKeychain`，openai 还有顶层 `apiKeyFile`）折叠为账号 "default"；
    /// 总是包含 "default"（可能是空块 = 只看 CLI / 环境变量 / 受管存储）。
    pub fn provider_accounts(&self, id: &str) -> BTreeMap<String, AccountConfig> {
        let settings = self.providers.get(id);
        let mut accounts = settings.map(|p| p.accounts.clone()).unwrap_or_default();
        let default = accounts.entry(agent_ai::secrets::DEFAULT_ACCOUNT.to_string()).or_default();
        if default.api_key_file.is_none() && default.api_key_keychain.is_none() && default.api_key_env.is_none() {
            default.api_key_file = settings.and_then(|p| p.api_key_file.clone()).or_else(|| if id == "openai" { self.api_key_file.clone() } else { None });
            default.api_key_keychain = settings.and_then(|p| p.api_key_keychain.clone());
        }
        accounts
    }

    /// 单个账号的配置（含折叠后的 default）。
    pub fn account_config(&self, id: &str, account: &str) -> Option<AccountConfig> {
        self.provider_accounts(id).remove(account)
    }

    /// provider 实例的流空闲超时（秒）：账号 > provider > 全局。profile / 请求级覆盖由会话按请求传入。
    pub fn account_stream_idle_timeout_secs(&self, id: &str, account: &AccountConfig) -> u64 {
        account.stream_idle_timeout_secs.or_else(|| self.providers.get(id).and_then(|p| p.stream_idle_timeout_secs)).unwrap_or(self.stream_idle_timeout_secs)
    }

    /// provider 的默认账号名（未配置时 "default"）。
    pub fn default_account_of(&self, id: &str) -> &str {
        self.providers.get(id).map_or(agent_ai::secrets::DEFAULT_ACCOUNT, |p| p.default_account())
    }

    /// profile 解析出的 provider（按配置的 defaultProvider；与启动时 needed provider 的判定一致）。
    pub fn profile_provider_id<'a>(&'a self, p: &'a Profile) -> &'a str {
        crate::hub::select_provider(None, p.provider.as_deref(), p.model.as_deref().unwrap_or(&self.default_model), self.default_provider.as_deref().unwrap_or("openai"))
    }

    fn validate_accounts(&self, id: &str, provider: &ProviderConfig) -> anyhow::Result<()> {
        let at = format!("providers.{id}");
        if id == "faux" {
            anyhow::ensure!(provider.accounts.is_empty() && provider.default_account.is_none(), "{at}: accounts/defaultAccount are not supported for faux");
            return Ok(());
        }
        let legacy = provider.api_key_file.is_some() || provider.api_key_keychain.is_some() || (id == "openai" && self.api_key_file.is_some());
        anyhow::ensure!(!(legacy && provider.accounts.contains_key(agent_ai::secrets::DEFAULT_ACCOUNT)), "{at}: apiKeyFile/apiKeyKeychain and accounts.default are both set; keep one");
        for (name, account) in &provider.accounts {
            anyhow::ensure!(agent_ai::secrets::valid_account_name(name), "{at}: invalid account name '{name}' (allowed: [A-Za-z0-9_.-]{{1,64}})");
            let at = format!("{at}.accounts.{name}");
            if let Some(secs) = account.stream_idle_timeout_secs {
                check_stream_idle_timeout(&format!("{at}.streamIdleTimeoutSecs"), secs)?;
            }
            let sources = [account.api_key_file.is_some(), account.api_key_keychain.is_some(), account.api_key_env.is_some()];
            anyhow::ensure!(sources.iter().filter(|s| **s).count() <= 1, "{at}: set only one of apiKeyFile / apiKeyKeychain / apiKeyEnv");
            if let Some(keychain) = &account.api_key_keychain {
                keychain.validate(&at)?;
            }
            if let Some(var) = &account.api_key_env {
                let mut bytes = var.bytes();
                let valid = bytes.next().is_some_and(|b| b.is_ascii_uppercase() || b == b'_') && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
                anyhow::ensure!(valid, "{at}: apiKeyEnv must match ^[A-Z_][A-Z0-9_]*$");
            }
            if account.organization.is_some() || account.project.is_some() {
                anyhow::ensure!(id == "openai", "{at}: organization/project are only supported for openai");
            }
            for (field, value) in [("organization", &account.organization), ("project", &account.project), ("baseUrl", &account.base_url)] {
                if let Some(v) = value {
                    anyhow::ensure!(!v.trim().is_empty() && v.bytes().all(|b| b.is_ascii_graphic()), "{at}: {field} must be a non-empty string without spaces or control characters");
                }
            }
        }
        let accounts = self.provider_accounts(id);
        let default = provider.default_account();
        anyhow::ensure!(accounts.contains_key(default), "{at}: defaultAccount '{default}' is not defined in accounts");
        for (name, account) in &accounts {
            check_fallbacks(&format!("{at}.accounts.{name}"), name, &account.fallback_accounts, &accounts)?;
        }
        Ok(())
    }

    fn validate_profile_accounts(&self, name: &str, p: &Profile) -> anyhow::Result<()> {
        if p.account.is_none() && p.allowed_accounts.is_none() && p.fallback_accounts.is_none() {
            return Ok(());
        }
        let id = self.profile_provider_id(p);
        let accounts = self.provider_accounts(id);
        let at = format!("profile '{name}'");
        let known = |a: &str| -> anyhow::Result<()> {
            anyhow::ensure!(accounts.contains_key(a), "{at}: account '{a}' is not configured for provider {id}");
            Ok(())
        };
        let own = p.account.as_deref().unwrap_or_else(|| self.default_account_of(id));
        known(own)?;
        if let Some(list) = &p.allowed_accounts {
            for a in list.iter().filter(|a| *a != "*") {
                known(a)?;
            }
            anyhow::ensure!(list.iter().any(|a| a == "*" || a == own), "{at}: allowedAccounts must include its own account '{own}'");
        }
        if let Some(list) = &p.fallback_accounts {
            check_fallbacks(&at, own, list, &accounts)?;
        }
        Ok(())
    }

    /// 无配置文件时的内置配置（CLI 入口）：provider 为 openai（或未指定、按模型推断为 openai）时，default profile 只允许
    /// [`DEFAULT_OPENAI_ALLOWED_MODELS`]；用户 `--model` 显式指定的其它模型也会加入，不会被拒绝。anthropic/faux 不限制。
    pub fn fallback_for(provider: Option<&str>, model: Option<String>, token: &str, workspace: PathBuf) -> Self {
        let explicit = model.clone();
        let model = model.unwrap_or_else(|| DEFAULT_OPENAI_MODEL.into());
        let mut cfg = Self::fallback(model.clone(), token, workspace);
        let openai = match provider {
            Some(p) => p == "openai",
            None => !model.starts_with("claude-"),
        };
        if openai {
            let mut allowed: Vec<String> = vec![];
            if let Some(m) = explicit.filter(|m| !DEFAULT_OPENAI_ALLOWED_MODELS.contains(&m.as_str())) {
                allowed.push(m);
            }
            allowed.extend(DEFAULT_OPENAI_ALLOWED_MODELS.iter().map(|m| m.to_string()));
            cfg.profiles.get_mut("default").expect("default profile").allowed_models = Some(allowed);
        }
        cfg
    }

    /// 无配置文件时的默认值：一个以 `workspace` 为根、启用默认编码工具的 `default` profile + 单一 client。
    pub fn fallback(model: String, token: &str, workspace: PathBuf) -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "default".into(),
            Profile {
                provider: None,
                system_prompt: None,
                append_system_prompt: None,
                model: None,
                allowed_models: None,
                reasoning_effort: None,
                api: None,
                max_output_tokens: None,
                llm_max_retries: None,
                stream_idle_timeout_secs: None,
                token_budget: None,
                run_timeout_secs: None,
                tools: default_tools(),
                workspace_roots: vec![workspace],
                load_context_files: true,
                http_allow_hosts: vec![],
                shell_env: Default::default(),
                allow_system_prompt_override: true,
                allow_caller_tools: true,
                max_turns: default_max_turns(),
                on_max_turns: Default::default(),
                max_turns_ask_timeout_secs: default_max_turns_ask_timeout_secs(),
                deep_model: None,
                auto_route: false,
                router_model: None,
                account: None,
                allowed_accounts: None,
                fallback_accounts: None,
                tool_approval: None,
                context_window: None,
                compaction: None,
                mcp_servers: vec![],
            },
        );
        Config {
            default_provider: None,
            providers: BTreeMap::new(),
            data_dir: None,
            unix_socket: None,
            unix_socket_mode: None,
            #[cfg(unix)]
            local_clients: vec![LocalClient {
                // SAFETY: geteuid has no arguments or memory preconditions.
                uid: Some(unsafe { libc::geteuid() }),
                username: None,
                any: false,
                client: "default".into(),
            }],
            #[cfg(not(unix))]
            local_clients: vec![],
            trust_loopback: None,
            shutdown_grace_secs: default_shutdown_grace(),
            alerts: AlertsConfig::default(),
            task_ttl_secs: default_session_idle_ttl(),
            webhook_max_attempts: default_webhook_attempts(),
            stream_idle_timeout_secs: default_stream_idle_timeout_secs(),
            llm_max_retries: default_llm_max_retries(),
            keychain_timeout_secs: default_keychain_timeout_secs(),
            chat_timeout_secs: default_chat_timeout_secs(),
            default_model: model,
            default_reasoning_effort: None,
            default_api: Default::default(),
            api_key_file: None,
            session_idle_ttl_secs: default_session_idle_ttl(),
            chat_session_ttl_secs: default_chat_session_ttl(),
            max_sessions_per_client: default_max_sessions(),
            tool_approval: None,
            session_dir: None,
            persist_sessions: None,
            compaction: Default::default(),
            mcp_servers: BTreeMap::new(),
            profiles,
            clients: vec![Client { name: "default".into(), token_sha256: sha256_hex(token), profiles: vec!["default".into()], ..Default::default() }],
        }
    }

    /// profile 实际使用的工具审批策略：profile.toolApproval > 全局 toolApproval > never。
    pub fn tool_approval_of(&self, p: &Profile) -> agent_tools::ToolApproval {
        p.tool_approval.clone().or_else(|| self.tool_approval.clone()).unwrap_or_default()
    }

    /// profile 是否允许使用该模型。
    pub fn model_allowed(p: &Profile, model: &str) -> bool {
        p.allowed_models.as_ref().is_none_or(|list| list.iter().any(|pat| agent_ai::models::model_matches(pat, model)))
    }

    /// 常量时间比较哈希，找出 token 对应的客户端。
    pub fn client_for_token(&self, token: &str) -> Option<&Client> {
        let h = sha256_hex(token);
        self.clients.iter().find(|c| c.token_sha256.len() == h.len() && c.token_sha256.bytes().zip(h.bytes()).fold(0u8, |a, (x, y)| a | (x.to_ascii_lowercase() ^ y)) == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_data_dir_resolution_chain() {
        let tmp = std::env::temp_dir();
        let base = tmp.join("system-data");
        let xdg = tmp.join("xdg-data");
        assert_eq!(resolve_default_data_dir(Some(base.clone()), Some(xdg.clone().into_os_string()), tmp.clone(), 42), base.join("sub-bot"));
        assert_eq!(resolve_default_data_dir(None, Some(xdg.clone().into_os_string()), tmp.clone(), 42), xdg.join("sub-bot"));
        for xdg in [None, Some(OsString::new()), Some(OsString::from("relative/data"))] {
            let resolved = resolve_default_data_dir(None, xdg, tmp.clone(), 42);
            #[cfg(unix)]
            assert_eq!(resolved, tmp.join("sub-bot-42"));
            #[cfg(not(unix))]
            assert_eq!(resolved, tmp.join("sub-bot"));
        }
        #[cfg(unix)]
        assert_ne!(resolve_default_data_dir(None, None, tmp.clone(), 42), resolve_default_data_dir(None, None, tmp, 43));
    }

    #[test]
    fn sha256_hex_matches_known_vectors() {
        // FIPS 180-2 test vectors: stored tokenSha256 values must stay stable across sha2 upgrades.
        assert_eq!(sha256_hex(""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[cfg(unix)]
    #[test]
    fn temporary_data_dir_rejects_unsafe_existing_directories() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let parent = std::env::temp_dir().join(format!("sub-bot-permissions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&parent).unwrap();
        let root = parent.join("fallback");
        let uid = effective_uid();
        check_temp_data_dir(&root, uid, false).unwrap();
        assert!(!root.exists(), "read-only socket lookup must not create the directory");
        check_temp_data_dir(&root, uid, true).unwrap();
        let meta = std::fs::symlink_metadata(&root).unwrap();
        assert_eq!(meta.uid(), uid);
        assert_eq!(meta.mode() & 0o777, 0o700);
        check_temp_data_dir(&root, uid, true).unwrap();
        // Inject a different expected uid: no root/chown privileges needed.
        assert!(check_temp_data_dir(&root, uid.wrapping_add(1), true).unwrap_err().to_string().contains("not owned by current euid"));
        for mode in [0o720, 0o702, 0o777] {
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(mode)).unwrap();
            for create in [false, true] {
                assert!(check_temp_data_dir(&root, uid, create).unwrap_err().to_string().contains("group/world writable"));
                assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, mode, "must reject, not chmod");
            }
        }
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = parent.join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(check_temp_data_dir(&link, uid, true).unwrap_err().to_string().contains("not a symlink"));
        let file = parent.join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(check_temp_data_dir(&file, uid, true).is_err());
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn multi_targets_defaults_caps_and_secrets() {
        let dir = std::env::temp_dir().join(format!("sub-bot-targets-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("default-key"), "default-private-key").unwrap();
        std::fs::write(dir.join("own-key"), "entry-private-key").unwrap();
        let path = dir.join("config.json");
        let mut value = serde_json::json!({"profiles":{}, "clients":[{"name":"bot", "tokenSha256":sha256_hex("token"), "profiles":[], "webhookSecretFile":"default-key",
            "webhookUrl":["https://one.example/hook", {"url":"http://two.example:9000/hook", "secretFile":"own-key", "events":["task.progress","issues"], "minSeverity":"fatal"}]}],
            "alerts":{"webhookUrl":["https://alerts.example/hook", {"url":"http://other.example", "minSeverity":"warn", "events":[]}], "webhookSecretFile":"own-key"}});
        std::fs::write(&path, value.to_string()).unwrap();
        let cfg = Config::load(&path).unwrap();
        let targets = &cfg.clients[0].resolved_webhooks;
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].target.secret.as_deref(), Some("default-private-key"));
        assert_eq!(targets[1].target.secret.as_deref(), Some("entry-private-key"));
        assert!(targets[0].wants("task.completed"));
        assert!(!targets[0].wants_issue(crate::issues::Severity::Fatal));
        assert!(targets[1].wants("task.progress"));
        assert!(!targets[1].wants_issue(crate::issues::Severity::Error));
        assert!(targets[1].wants_issue(crate::issues::Severity::Fatal));
        assert_eq!(cfg.alerts.resolved[0].min_severity, Some(crate::issues::Severity::Error));
        assert_eq!(cfg.alerts.resolved[1].min_severity, Some(crate::issues::Severity::Warn));
        assert!(cfg.clients[0].webhook_allowed_hosts.contains(&"two.example".to_string()));
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("default-private-key"));
        assert!(!debug.contains("entry-private-key"));
        value["clients"][0]["webhookUrl"] = serde_json::json!(vec!["https://example.com"; MAX_TARGETS + 1]);
        std::fs::write(&path, value.to_string()).unwrap();
        assert!(Config::load(&path).is_err());
        value["clients"][0]["webhookUrl"] = serde_json::json!(["ftp://example.com"]);
        std::fs::write(&path, value.to_string()).unwrap();
        assert!(Config::load(&path).is_err());
        assert!(serde_json::from_str::<Targets>("null").is_err());
        assert!(Targets::default().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn webhook_config_secret_loading_validation_and_redaction() {
        let dir = std::env::temp_dir().join(format!("sub-bot-webhook-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let secret = "test-webhook-key-do-not-log";
        std::fs::write(dir.join("secret"), format!("  {secret}\n")).unwrap();
        let mut value = serde_json::json!({"profiles": {}, "clients": [{"name": "bot", "tokenSha256": sha256_hex("token"), "profiles": [],
            "webhookUrl": "https://example.com/hook", "webhookSecretFile": "secret", "webhookAllowedHosts": ["localhost:1234"]}]});
        std::fs::write(&path, value.to_string()).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!((cfg.task_ttl_secs, cfg.webhook_max_attempts), (3600, 6));
        assert!(cfg.clients[0].webhook_secret.as_deref() == Some(secret));
        assert_eq!(cfg.clients[0].webhook_secret_file.as_deref(), Some(dir.join("secret").as_path()));
        let debug = format!("{cfg:?}");
        assert!(!debug.contains(secret));
        assert!(debug.contains("<redacted>"));
        for url in ["not a url", "ftp://example.com", "file:///tmp/hook", "https://user:password@example.com"] {
            value["clients"][0]["webhookUrl"] = serde_json::json!(url);
            std::fs::write(&path, value.to_string()).unwrap();
            assert!(Config::load(&path).unwrap_err().to_string().contains("webhookUrl"));
        }
        value["clients"][0]["webhookUrl"] = serde_json::json!("http://localhost");
        std::fs::write(&path, value.to_string()).unwrap();
        std::fs::write(dir.join("secret"), " \n").unwrap();
        assert_eq!(Config::load(&path).unwrap_err().to_string(), "webhookSecretFile is empty");
        std::fs::remove_file(dir.join("secret")).unwrap();
        assert_eq!(Config::load(&path).unwrap_err().to_string(), "cannot read webhookSecretFile");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn routing_fields_parse_validate_and_example_config_deserializes() {
        let cfg: Config = serde_json::from_str(r#"{"providers":{"anthropic":{"defaultModel":"claude-sonnet-5-5"}},"profiles":{"p":{"workspaceRoots":["."],"deepModel":"latest:claude-fable","autoRoute":true,"routerModel":"claude-haiku-5-5"}}}"#).unwrap();
        let p = &cfg.profiles["p"];
        assert_eq!((p.deep_model.as_deref(), p.auto_route, p.router_model.as_deref()), (Some("latest:claude-fable"), true, Some("claude-haiku-5-5")));
        assert_eq!(cfg.providers["anthropic"].default_model.as_deref(), Some("claude-sonnet-5-5"));
        cfg.validate().unwrap();
        let defaults: Config = serde_json::from_str(r#"{"profiles":{"p":{"workspaceRoots":["."]}}}"#).unwrap();
        assert_eq!((defaults.profiles["p"].deep_model.as_deref(), defaults.profiles["p"].auto_route), (None, false));
        for bad in ["latest:", " ", "latest: "] {
            let mut invalid = cfg.clone();
            invalid.profiles.get_mut("p").unwrap().deep_model = Some(bad.into());
            assert!(invalid.validate().unwrap_err().to_string().contains("deepModel"));
        }
        let example: Config = serde_json::from_str(include_str!("../../../sub-bot.example.json")).unwrap();
        assert_eq!(example.profiles["claude"].deep_model.as_deref(), Some(DEFAULT_CLAUDE_DEEP_MODEL));
        assert_eq!(example.default_model, DEFAULT_OPENAI_MODEL);
        assert_eq!(example.profiles["coder"].model.as_deref(), Some(DEFAULT_OPENAI_MODEL));
        assert_eq!(example.profiles["coder"].allowed_models.as_deref(), Some(&["gpt-6.1-sol".to_string(), "gpt-6-astra".to_string()][..]));
        assert_eq!(example.profiles["reviewer"].model.as_deref(), Some("gpt-6-astra"));
        assert!(Config::model_allowed(&example.profiles["coder"], "gpt-6-astra") && !Config::model_allowed(&example.profiles["coder"], "gpt-4.1-mini"));
    }

    #[test]
    fn default_openai_model_and_fallback_allowed_models() {
        assert_eq!(DEFAULT_OPENAI_MODEL, "gpt-6.1-sol");
        assert_eq!(DEFAULT_OPENAI_ALLOWED_MODELS, ["gpt-6.1-sol", "gpt-6-astra"]);
        let parsed: Config = serde_json::from_str(r#"{"profiles":{}}"#).unwrap();
        assert_eq!(parsed.default_model, DEFAULT_OPENAI_MODEL);
        let allowed = |cfg: &Config| cfg.profiles["default"].allowed_models.clone();
        let defaults: Vec<String> = DEFAULT_OPENAI_ALLOWED_MODELS.iter().map(|m| m.to_string()).collect();
        // 未指定 provider / 显式 openai，且没有 --model：只允许两个默认模型。
        for provider in [None, Some("openai")] {
            let cfg = Config::fallback_for(provider, None, "t", PathBuf::from("."));
            assert_eq!(cfg.default_model, DEFAULT_OPENAI_MODEL);
            assert_eq!(allowed(&cfg).as_ref(), Some(&defaults));
            assert!(Config::model_allowed(&cfg.profiles["default"], "gpt-6-astra"));
            assert!(!Config::model_allowed(&cfg.profiles["default"], "gpt-5.5"));
            cfg.validate().unwrap();
        }
        // --model 指定列表内模型：不重复；列表外模型：加入且不会被拒绝。
        let cfg = Config::fallback_for(None, Some("gpt-6-astra".into()), "t", PathBuf::from("."));
        assert_eq!((cfg.default_model.as_str(), allowed(&cfg)), ("gpt-6-astra", Some(defaults.clone())));
        let cfg = Config::fallback_for(Some("openai"), Some("gpt-5.5".into()), "t", PathBuf::from("."));
        assert_eq!(allowed(&cfg), Some(vec!["gpt-5.5".to_string(), "gpt-6.1-sol".into(), "gpt-6-astra".into()]));
        assert!(Config::model_allowed(&cfg.profiles["default"], "gpt-5.5"));
        // anthropic / faux（以及未指定 provider 但模型是 Claude）不受影响。
        for (provider, model) in [(Some("anthropic"), None), (Some("faux"), None), (Some("anthropic"), Some("claude-sonnet-5-5")), (None, Some("claude-sonnet-5-5"))] {
            let cfg = Config::fallback_for(provider, model.map(String::from), "t", PathBuf::from("."));
            assert_eq!(allowed(&cfg), None, "{provider:?} {model:?}");
        }
        assert_eq!(allowed(&Config::fallback("faux".into(), "t", PathBuf::from("."))), None);
    }

    #[test]
    fn llm_retry_defaults_and_profile_override() {
        let cfg: Config = serde_json::from_str(r#"{"profiles":{"p":{"workspaceRoots":["."]}}}"#).unwrap();
        assert_eq!(cfg.llm_max_retries, 3);
        assert_eq!(cfg.profiles["p"].llm_max_retries, None);
        let cfg: Config = serde_json::from_str(r#"{"llmMaxRetries":7,"profiles":{"p":{"workspaceRoots":["."],"llmMaxRetries":0}}}"#).unwrap();
        assert_eq!(cfg.llm_max_retries, 7);
        assert_eq!(cfg.profiles["p"].llm_max_retries, Some(0));
    }

    #[test]
    fn session_limits_defaults_and_overrides() {
        let cfg: Config = serde_json::from_str(r#"{"profiles":{}}"#).unwrap();
        assert_eq!((cfg.session_idle_ttl_secs, cfg.chat_session_ttl_secs, cfg.max_sessions_per_client), (3600, 600, 64));
        let cfg: Config = serde_json::from_str(r#"{"profiles":{},"sessionIdleTtlSecs":0,"chatSessionTtlSecs":5,"maxSessionsPerClient":0}"#).unwrap();
        assert_eq!((cfg.session_idle_ttl_secs, cfg.chat_session_ttl_secs, cfg.max_sessions_per_client), (0, 5, 0));
    }

    #[test]
    fn session_persistence_and_compaction_config() {
        let mut cfg: Config = serde_json::from_str(r#"{"profiles":{},"dataDir":"/data"}"#).unwrap();
        assert_eq!(cfg.sessions_dir(), Some(PathBuf::from("/data/sessions")));
        cfg.session_dir = Some("/elsewhere".into());
        assert_eq!(cfg.sessions_dir(), Some(PathBuf::from("/elsewhere")));
        cfg.persist_sessions = Some(false);
        assert_eq!(cfg.sessions_dir(), None);
        let base = serde_json::json!({"profiles":{"p":{"workspaceRoots":["."],"contextWindow":50000,"compaction":{"thresholdPct":60,"model":"cheap"}}},
            "compaction":{"keepRecentTokens":1000,"maxSummaryTokens":2000}});
        let cfg: Config = serde_json::from_value(base.clone()).unwrap();
        cfg.validate().unwrap();
        let s = crate::compaction::CompactionSettings::resolve(&cfg.compaction, cfg.profiles["p"].compaction.as_ref());
        assert_eq!((s.threshold_pct, s.keep_recent_tokens, s.max_summary_tokens, s.model.as_deref()), (60, 1000, 2000, Some("cheap")));
        for (pointer, value, message) in [
            ("/profiles/p/contextWindow", serde_json::json!(10), "contextWindow must be >= 1000"),
            ("/profiles/p/compaction/thresholdPct", serde_json::json!(0), "profile 'p': compaction.thresholdPct"),
            ("/compaction/maxSummaryTokens", serde_json::json!(1_000_000), "compaction.maxSummaryTokens"),
        ] {
            let mut v = base.clone();
            *v.pointer_mut(pointer).unwrap() = value;
            let cfg: Config = serde_json::from_value(v).unwrap();
            assert!(cfg.validate().unwrap_err().to_string().contains(message), "{pointer}");
        }
        assert!(serde_json::from_value::<Config>(serde_json::json!({"profiles":{},"compaction":{"bogus":1}})).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fallback_trusts_only_own_effective_uid_over_unix() {
        use crate::local::Peer;
        let cfg = Config::fallback("faux".into(), "test-token", PathBuf::from("."));
        // SAFETY: geteuid has no arguments or memory preconditions.
        let uid = unsafe { libc::geteuid() };
        assert_eq!(cfg.local_clients.len(), 1);
        assert_eq!(cfg.local_clients[0].uid, Some(uid));
        assert!(!cfg.local_clients[0].any);
        assert!(cfg.local_clients[0].username.is_none());
        assert_eq!(Peer::Unix { uid, gid: 0, pid: None }.client(&cfg).unwrap().name, "default");
        assert!(Peer::Unix { uid: uid.wrapping_add(1), gid: 0, pid: None }.client(&cfg).is_none());
        assert!(Peer::Tcp("127.0.0.1:1234".parse().unwrap()).client(&cfg).is_none());
        cfg.validate().unwrap();
        // Explicit configs remain opt-in, even with the same client definition.
        let explicit: Config = serde_json::from_value(serde_json::json!({
            "profiles": {}, "clients": [{"name": "default", "profiles": [], "tokenSha256": sha256_hex("test-token")}]
        }))
        .unwrap();
        assert!(explicit.local_clients.is_empty());
    }

    #[test]
    fn fallback_preserves_session_limits_and_timeouts() {
        let cfg = Config::fallback("faux".into(), "test-token", PathBuf::from("."));
        assert_eq!((cfg.session_idle_ttl_secs, cfg.chat_session_ttl_secs, cfg.max_sessions_per_client), (3600, 600, 64));
        assert_eq!((cfg.stream_idle_timeout_secs, cfg.chat_timeout_secs), (1800, 900));
        cfg.validate().unwrap();
        assert_eq!(cfg.profiles["default"].run_timeout_secs, None);
    }

    #[test]
    fn load_validates_profiles_tools_and_hashes() {
        let dir = std::env::temp_dir().join(format!("sub-bot-validation-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let valid = serde_json::json!({
            "profiles": {"p": {"workspaceRoots": ["."], "tools": ["preset:readonly", "preset:no-shell", "preset:default", "clock", "http_fetch"]}},
            "clients": [{"name": "bot", "tokenSha256": sha256_hex("test-token").to_uppercase(), "profiles": ["p"]}]
        });
        std::fs::write(&path, valid.to_string()).unwrap();
        assert!(Config::load(&path).unwrap().client_for_token("test-token").is_some());
        for (pointer, value, expected) in [
            ("/clients/0/profiles/0", serde_json::json!("missing"), "client 'bot': unknown profile 'missing'"),
            ("/profiles/p/workspaceRoots", serde_json::json!([]), "profile 'p': workspaceRoots must not be empty"),
            ("/profiles/p/tools/0", serde_json::json!("missing"), "profile 'p': unknown tool 'missing'"),
            ("/profiles/p/tools/0", serde_json::json!("preset:missing"), "profile 'p': unknown preset 'missing'"),
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            std::fs::write(&path, invalid.to_string()).unwrap();
            assert_eq!(Config::load(&path).unwrap_err().to_string(), expected);
        }
        for hash in [String::new(), "a".repeat(63), "a".repeat(65), "g".repeat(64), "é".repeat(32), format!("{} ", "a".repeat(63))] {
            let mut invalid = valid.clone();
            invalid["clients"][0]["tokenSha256"] = serde_json::json!(hash);
            std::fs::write(&path, invalid.to_string()).unwrap();
            assert_eq!(Config::load(&path).unwrap_err().to_string(), "client 'bot': tokenSha256 must be exactly 64 hex characters");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keychain_timeout_defaults_and_validation() {
        let cfg: Config = serde_json::from_value(serde_json::json!({"profiles": {}})).unwrap();
        assert_eq!(cfg.keychain_timeout_secs, 30);
        assert_eq!(Config::fallback("faux".into(), "t", ".".into()).keychain_timeout_secs, 30);
        let cfg: Config = serde_json::from_value(serde_json::json!({"keychainTimeoutSecs": 5, "profiles": {}})).unwrap();
        assert_eq!(cfg.keychain_timeout_secs, 5);
        cfg.validate().unwrap();
        let cfg: Config = serde_json::from_value(serde_json::json!({"keychainTimeoutSecs": 0, "profiles": {}})).unwrap();
        assert_eq!(cfg.validate().unwrap_err().to_string(), "keychainTimeoutSecs must be >= 1");
    }

    #[test]
    fn provider_config_paths_aliases_and_validation() {
        let dir = std::env::temp_dir().join(format!("sub-bot-provider-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let mut value = serde_json::json!({
            "defaultProvider": "anthropic",
            "apiKeyFile": "keys/openai",
            "providers": {
                "openai": {"apiKeyFile": "keys/openai", "baseUrl": "http://localhost:9000"},
                "anthropic": {"apiKeyFile": "keys/anthropic", "baseUrl": "http://localhost:9001"}
            },
            "profiles": {"claude": {"provider": "anthropic", "model": "claude-sonnet-4-5", "workspaceRoots": ["."]}}
        });
        std::fs::write(&path, value.to_string()).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("anthropic"));
        assert_eq!(cfg.profiles["claude"].provider.as_deref(), Some("anthropic"));
        assert_eq!(cfg.providers["anthropic"].api_key_file, Some(dir.join("keys/anthropic")));
        assert_eq!(cfg.providers["openai"].api_key_file, cfg.api_key_file);
        assert_eq!(cfg.providers["anthropic"].base_url.as_deref(), Some("http://localhost:9001"));
        value["providers"]["openai"]["apiKeyFile"] = "different".into();
        std::fs::write(&path, value.to_string()).unwrap();
        assert!(Config::load(&path).unwrap_err().to_string().contains("must match"));
        for invalid in [
            serde_json::json!({"defaultProvider": "unknown", "profiles": {}}),
            serde_json::json!({"providers": {"unknown": {}}, "profiles": {}}),
            serde_json::json!({"profiles": {"p": {"provider": "unknown", "workspaceRoots": ["."]}}}),
        ] {
            let cfg: Config = serde_json::from_value(invalid).unwrap();
            assert!(cfg.validate().unwrap_err().to_string().contains("unknown provider"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn keychain_config(providers: serde_json::Value) -> Config {
        serde_json::from_value(serde_json::json!({"providers": providers, "profiles": {}})).unwrap()
    }

    #[test]
    fn keychain_ref_parses_and_resolves() {
        let cfg = keychain_config(serde_json::json!({"openai": {"apiKeyKeychain": {}}, "anthropic": {"apiKeyKeychain": {"service": "my-keys", "account": "claude-prod"}}}));
        let openai = cfg.providers["openai"].api_key_keychain.as_ref().unwrap();
        assert_eq!(openai.resolve("openai", "default"), ("sub-bot".to_string(), "openai/default".to_string()));
        assert_eq!(openai.resolve("openai", "work"), ("sub-bot".to_string(), "openai/work".to_string()));
        let anthropic = cfg.providers["anthropic"].api_key_keychain.as_ref().unwrap();
        assert_eq!(anthropic.resolve("anthropic", "default"), ("my-keys".to_string(), "claude-prod".to_string()));
        let unknown: Result<Config, _> = serde_json::from_value(serde_json::json!({"providers": {"openai": {"apiKeyKeychain": {"svc": "x"}}}, "profiles": {}}));
        assert!(unknown.unwrap_err().to_string().contains("unknown field `svc`"));
    }

    #[test]
    fn keychain_validation_conflicts() {
        for (providers, top, expected) in [
            (serde_json::json!({"openai": {"apiKeyKeychain": {}, "apiKeyFile": "/k"}}), None, "providers.openai: set only one of apiKeyFile / apiKeyKeychain"),
            (serde_json::json!({"openai": {"apiKeyKeychain": {}}}), Some("/k"), "providers.openai: set only one of apiKeyFile / apiKeyKeychain"),
            (serde_json::json!({"faux": {"apiKeyKeychain": {}}}), None, "providers.faux: apiKeyKeychain is not supported for faux"),
        ] {
            let mut cfg = keychain_config(providers);
            cfg.api_key_file = top.map(PathBuf::from);
            assert_eq!(cfg.validate().unwrap_err().to_string(), expected);
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn keychain_rejected_on_non_macos() {
        let cfg = keychain_config(serde_json::json!({"anthropic": {"apiKeyKeychain": {}}}));
        assert_eq!(
            cfg.validate().unwrap_err().to_string(),
            format!("providers.anthropic: apiKeyKeychain is only supported on macOS (this build targets {}); use apiKeyFile or `sub-bot keys set` (writes a 0600 key file)", std::env::consts::OS)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_accepted_on_macos() {
        keychain_config(serde_json::json!({"openai": {"apiKeyKeychain": {}}, "anthropic": {"apiKeyKeychain": {"service": "s", "account": "a"}}})).validate().unwrap();
        for field in ["service", "account"] {
            let cfg = keychain_config(serde_json::json!({"openai": {"apiKeyKeychain": {field: " "}}}));
            assert_eq!(cfg.validate().unwrap_err().to_string(), format!("providers.openai: apiKeyKeychain.{field} must not be blank"));
        }
    }

    #[test]
    fn api_key_file_is_relative_to_config_and_shell_env_parses() {
        let dir = std::env::temp_dir().join(format!("sub-bot-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sub-bot.json");
        std::fs::write(&path, r#"{"apiKeyFile":"keys/openai","profiles":{"p":{"workspaceRoots":["."],"shellEnv":{"mode":"allowlist","allow":["CARGO_*"]}}}}"#).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.api_key_file.as_deref(), Some(dir.join("keys/openai").as_path()));
        assert_eq!(cfg.stream_idle_timeout_secs, 1800);
        assert_eq!(cfg.chat_timeout_secs, 900);
        assert_eq!(cfg.profiles["p"].run_timeout_secs, None);
        let custom: Config = serde_json::from_str(r#"{"streamIdleTimeoutSecs":42,"chatTimeoutSecs":17,"profiles":{"p":{"workspaceRoots":["."],"runTimeoutSecs":3}}}"#).unwrap();
        assert_eq!(custom.stream_idle_timeout_secs, 42);
        assert_eq!(custom.chat_timeout_secs, 17);
        assert_eq!(custom.profiles["p"].run_timeout_secs, Some(3));
        let se = &cfg.profiles["p"].shell_env;
        assert_eq!(se.mode, agent_tools::EnvMode::Allowlist);
        assert_eq!(se.allow, vec!["CARGO_*".to_string()]);
        std::fs::remove_dir_all(dir).ok();
    }

    fn accounts_config(value: serde_json::Value) -> Config {
        let mut value = value;
        if value.get("profiles").is_none() {
            value["profiles"] = serde_json::json!({});
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn accounts_parse_and_legacy_fields_fold_into_default() {
        let dir = std::env::temp_dir().join(format!("sub-bot-accounts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sub-bot.json");
        let value = serde_json::json!({
            "apiKeyFile": "keys/openai.key",
            "providers": {
                "openai": {
                    "baseUrl": "https://api.openai.com/v1",
                    "defaultAccount": "work",
                    "failoverCooldownSecs": 120,
                    "accounts": {
                        "work": {"apiKeyFile": "keys/openai-work.key", "organization": "org_abc", "project": "proj_xyz", "fallbackAccounts": ["default"]},
                        "proxy": {"apiKeyEnv": "OPENAI_API_KEY_PROXY", "baseUrl": "http://localhost:9000/v1"},
                        "managed": {}
                    }
                },
                "anthropic": {"apiKeyFile": "/abs/anthropic.key", "accounts": {"ci": {"apiKeyFile": "keys/anthropic-ci.key"}}}
            },
            "profiles": {
                "coder": {"model": "gpt-6.1-sol", "account": "work", "allowedAccounts": ["work", "proxy"], "fallbackAccounts": ["proxy"], "workspaceRoots": ["."]},
                "any": {"provider": "anthropic", "account": "ci", "allowedAccounts": ["*"], "workspaceRoots": ["."]},
                "plain": {"workspaceRoots": ["."]}
            }
        });
        std::fs::write(&path, value.to_string()).unwrap();
        let cfg = Config::load(&path).unwrap();
        let openai = cfg.provider_accounts("openai");
        assert_eq!(openai.keys().map(String::as_str).collect::<Vec<_>>(), ["default", "managed", "proxy", "work"]);
        // 顶层 apiKeyFile ⇒ default 账号；相对路径相对于配置文件目录（账号级 apiKeyFile 同样）
        assert_eq!(openai["default"].api_key_file.as_deref(), Some(dir.join("keys/openai.key").as_path()));
        assert_eq!(openai["work"].api_key_file.as_deref(), Some(dir.join("keys/openai-work.key").as_path()));
        assert_eq!((openai["work"].organization.as_deref(), openai["work"].project.as_deref()), (Some("org_abc"), Some("proj_xyz")));
        assert_eq!(openai["work"].fallback_accounts, ["default"]);
        assert_eq!(openai["proxy"].api_key_env.as_deref(), Some("OPENAI_API_KEY_PROXY"));
        assert_eq!(openai["managed"], AccountConfig::default());
        assert_eq!((cfg.default_account_of("openai"), cfg.default_account_of("anthropic"), cfg.default_account_of("faux")), ("work", "default", "default"));
        assert_eq!(cfg.providers["openai"].failover_cooldown_secs, Some(120));
        let anthropic = cfg.provider_accounts("anthropic");
        assert_eq!(anthropic["default"].api_key_file.as_deref(), Some(Path::new("/abs/anthropic.key")));
        assert_eq!(anthropic["ci"].api_key_file.as_deref(), Some(dir.join("keys/anthropic-ci.key").as_path()));
        let coder = &cfg.profiles["coder"];
        assert_eq!(
            (coder.account.as_deref(), coder.allowed_accounts.as_deref(), coder.fallback_accounts.as_deref()),
            (Some("work"), Some(&["work".to_string(), "proxy".into()][..]), Some(&["proxy".to_string()][..]))
        );
        assert_eq!((cfg.profiles["plain"].account.as_deref(), cfg.profiles["plain"].allowed_accounts.as_ref()), (None, None));
        // 没有任何 provider 配置时也总有 default 账号
        assert_eq!(accounts_config(serde_json::json!({})).provider_accounts("anthropic").keys().collect::<Vec<_>>(), ["default"]);
        // 显式 accounts.default 与旧字段等价（只要不同时出现）
        let explicit = accounts_config(serde_json::json!({"providers": {"openai": {"accounts": {"default": {"apiKeyFile": "/k"}}}}}));
        explicit.validate().unwrap();
        assert_eq!(explicit.provider_accounts("openai")["default"].api_key_file.as_deref(), Some(Path::new("/k")));
        // 未知字段被拒绝
        let unknown: Result<Config, _> = serde_json::from_value(serde_json::json!({"providers": {"openai": {"accounts": {"w": {"apiKey": "sk-x"}}}}, "profiles": {}}));
        assert!(unknown.unwrap_err().to_string().contains("unknown field `apiKey`"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn validate_rejects_invalid_account_configuration() {
        let ws = serde_json::json!(["."]);
        for (value, expected) in [
            (serde_json::json!({"providers": {"openai": {"accounts": {"a/b": {}}}}}), "providers.openai: invalid account name 'a/b' (allowed: [A-Za-z0-9_.-]{1,64})"),
            (
                serde_json::json!({"providers": {"openai": {"apiKeyFile": "/k", "accounts": {"default": {}}}}}),
                "providers.openai: apiKeyFile/apiKeyKeychain and accounts.default are both set; keep one",
            ),
            (
                serde_json::json!({"apiKeyFile": "/k", "providers": {"openai": {"accounts": {"default": {"apiKeyFile": "/k"}}}}}),
                "providers.openai: apiKeyFile/apiKeyKeychain and accounts.default are both set; keep one",
            ),
            (
                serde_json::json!({"providers": {"openai": {"accounts": {"work": {"apiKeyFile": "/k", "apiKeyEnv": "K"}}}}}),
                "providers.openai.accounts.work: set only one of apiKeyFile / apiKeyKeychain / apiKeyEnv",
            ),
            (serde_json::json!({"providers": {"anthropic": {"accounts": {"ci": {"organization": "org"}}}}}), "providers.anthropic.accounts.ci: organization/project are only supported for openai"),
            (
                serde_json::json!({"providers": {"openai": {"accounts": {"w": {"project": "a b"}}}}}),
                "providers.openai.accounts.w: project must be a non-empty string without spaces or control characters",
            ),
            (serde_json::json!({"providers": {"openai": {"accounts": {"proxy": {"apiKeyEnv": "openai_key"}}}}}), "providers.openai.accounts.proxy: apiKeyEnv must match ^[A-Z_][A-Z0-9_]*$"),
            (serde_json::json!({"providers": {"openai": {"accounts": {"proxy": {"apiKeyEnv": "1KEY"}}}}}), "providers.openai.accounts.proxy: apiKeyEnv must match ^[A-Z_][A-Z0-9_]*$"),
            (serde_json::json!({"providers": {"openai": {"defaultAccount": "x"}}}), "providers.openai: defaultAccount 'x' is not defined in accounts"),
            (serde_json::json!({"providers": {"openai": {"accounts": {"w": {"fallbackAccounts": ["w"]}}}}}), "providers.openai.accounts.w: fallbackAccounts must not include the account itself ('w')"),
            (
                serde_json::json!({"providers": {"openai": {"accounts": {"w": {"fallbackAccounts": ["nope"]}}}}}),
                "providers.openai.accounts.w: fallbackAccounts entry 'nope' is not defined in accounts",
            ),
            (
                serde_json::json!({"providers": {"openai": {"accounts": {"w": {"fallbackAccounts": ["default", "default"]}}}}}),
                "providers.openai.accounts.w: fallbackAccounts has duplicate entry 'default'",
            ),
            (serde_json::json!({"providers": {"faux": {"accounts": {"w": {}}}}}), "providers.faux: accounts/defaultAccount are not supported for faux"),
            (serde_json::json!({"providers": {"faux": {"defaultAccount": "default"}}}), "providers.faux: accounts/defaultAccount are not supported for faux"),
            (serde_json::json!({"profiles": {"coder": {"account": "work", "workspaceRoots": ws}}}), "profile 'coder': account 'work' is not configured for provider openai"),
            (
                serde_json::json!({"providers": {"openai": {"accounts": {"work": {}}}}, "profiles": {"coder": {"provider": "anthropic", "account": "work", "workspaceRoots": ws}}}),
                "profile 'coder': account 'work' is not configured for provider anthropic",
            ),
            (
                serde_json::json!({"providers": {"openai": {"accounts": {"work": {}, "proxy": {}}}}, "profiles": {"coder": {"account": "work", "allowedAccounts": ["proxy"], "workspaceRoots": ws}}}),
                "profile 'coder': allowedAccounts must include its own account 'work'",
            ),
            (
                serde_json::json!({"providers": {"openai": {"accounts": {"work": {}}}}, "profiles": {"coder": {"allowedAccounts": ["default", "ghost"], "workspaceRoots": ws}}}),
                "profile 'coder': account 'ghost' is not configured for provider openai",
            ),
            (serde_json::json!({"profiles": {"coder": {"fallbackAccounts": ["default"], "workspaceRoots": ws}}}), "profile 'coder': fallbackAccounts must not include the account itself ('default')"),
            (serde_json::json!({"profiles": {"f": {"provider": "faux", "account": "x", "workspaceRoots": ws}}}), "profile 'f': account 'x' is not configured for provider faux"),
        ] {
            assert_eq!(accounts_config(value.clone()).validate().unwrap_err().to_string(), expected, "{value}");
        }
        // 合法：* 通配、defaultAccount 指向命名账号、profile 不写 account 时用 defaultAccount
        accounts_config(serde_json::json!({"providers": {"openai": {"defaultAccount": "work", "accounts": {"work": {"fallbackAccounts": ["default"]}}}},
            "profiles": {"a": {"allowedAccounts": ["*"], "workspaceRoots": ws}, "b": {"allowedAccounts": ["work"], "fallbackAccounts": ["default"], "workspaceRoots": ws}}}))
        .validate()
        .unwrap();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn account_keychain_rejected_on_non_macos() {
        let cfg = accounts_config(serde_json::json!({"providers": {"openai": {"accounts": {"work": {"apiKeyKeychain": {}}}}}}));
        assert!(cfg.validate().unwrap_err().to_string().starts_with("providers.openai.accounts.work: apiKeyKeychain is only supported on macOS"));
    }

    #[test]
    fn example_config_has_linux_valid_multi_account_openai() {
        let mut example: Config = serde_json::from_str(include_str!("../../../sub-bot.example.json")).unwrap();
        // 示例里的 tokenSha256 是占位符
        for client in &mut example.clients {
            client.token_sha256 = sha256_hex(&client.name);
        }
        example.validate().unwrap();
        let openai = example.provider_accounts("openai");
        assert!(openai.len() >= 3, "{:?}", openai.keys().collect::<Vec<_>>());
        assert!(openai.values().all(|a| a.api_key_keychain.is_none()), "example must stay Linux-valid");
        assert!(openai.values().any(|a| a.api_key_env.is_some()) && openai.values().filter(|a| a.api_key_file.is_some()).count() >= 2);
        assert!(example.profiles.values().any(|p| p.allowed_accounts.is_some() && p.account.is_some()));
    }

    #[test]
    fn readme_multi_account_example_validates() {
        let readme = include_str!("../../../README.md");
        let section = &readme[readme.find("## 同一 provider 多账号").expect("README section")..];
        let start = section.find("```json\n").unwrap() + "```json\n".len();
        let json = &section[start..start + section[start..].find("```").unwrap()];
        let mut cfg: Config = serde_json::from_str(json).unwrap();
        for client in &mut cfg.clients {
            client.token_sha256 = sha256_hex(&client.name);
        }
        cfg.validate().unwrap();
        assert_eq!(cfg.provider_accounts("openai").keys().map(String::as_str).collect::<Vec<_>>(), ["default", "personal", "proxy", "work"]);
        assert_eq!(cfg.profiles["claude-ci"].account.as_deref(), Some("ci"));
    }

    #[test]
    fn tool_approval_parses_inherits_and_validates() {
        use agent_tools::ToolApproval;
        let base = serde_json::json!({
            "toolApproval": "always",
            "profiles": {
                "inherit": {"workspaceRoots": ["."]},
                "own": {"workspaceRoots": ["."], "toolApproval": {"default": "ask", "tools": {"read": "allow", "mcp__*": "deny"}, "bash": {"allow": ["git status*"]}, "timeoutSecs": 60, "onTimeout": "deny"}},
                "off": {"workspaceRoots": ["."], "toolApproval": "never"}
            },
            "clients": []
        });
        let cfg: Config = serde_json::from_value(base.clone()).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.tool_approval_of(&cfg.profiles["inherit"]), ToolApproval::Always);
        assert_eq!(cfg.tool_approval_of(&cfg.profiles["off"]), ToolApproval::Never);
        assert_eq!(cfg.tool_approval_of(&cfg.profiles["own"]).timeout_secs(), 60);
        // 不配置 ⇒ never（向后兼容）
        let fallback = Config::fallback("faux".into(), "t", PathBuf::from("."));
        assert_eq!(fallback.tool_approval_of(&fallback.profiles["default"]), ToolApproval::Never);
        for (pointer, value, expected) in [
            ("/toolApproval", serde_json::json!("sometimes"), "toolApproval: unknown mode 'sometimes'"),
            ("/profiles/own/toolApproval/timeoutSecs", serde_json::json!(0), "toolApproval.timeoutSecs must be between 1 and 86400"),
            ("/profiles/own/toolApproval/default", serde_json::json!("maybe"), "unknown variant `maybe`"),
            ("/profiles/own/toolApproval/onTimeout", serde_json::json!("ask"), "unknown variant `ask`, expected `allow` or `deny`"),
            ("/profiles/own/toolApproval/tools", serde_json::json!({"rm -rf": "deny"}), "toolApproval.tools: invalid tool name or pattern 'rm -rf'"),
        ] {
            let mut invalid = base.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            let error = serde_json::from_value::<Config>(invalid).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
        // 代码构造的配置也会被 validate 拒绝，并带上 profile 名
        let mut cfg = cfg;
        cfg.profiles.get_mut("own").unwrap().tool_approval = Some(ToolApproval::Rules(agent_tools::approval::ApprovalRules { timeout_secs: 0, ..Default::default() }));
        assert_eq!(cfg.validate().unwrap_err().to_string(), "profile 'own': toolApproval.timeoutSecs must be between 1 and 86400");
    }

    #[test]
    fn max_turns_defaults_and_validation() {
        let base = serde_json::json!({"profiles": {"p": {"workspaceRoots": ["."]}}, "clients": []});
        let cfg: Config = serde_json::from_value(base.clone()).unwrap();
        cfg.validate().unwrap();
        let p = &cfg.profiles["p"];
        assert_eq!((p.max_turns, p.on_max_turns, p.max_turns_ask_timeout_secs), (150, agent_core::OnMaxTurns::Stop, 600));
        let fallback = Config::fallback("faux".into(), "t", PathBuf::from("."));
        let d = &fallback.profiles["default"];
        assert_eq!((d.max_turns, d.on_max_turns, d.max_turns_ask_timeout_secs), (150, agent_core::OnMaxTurns::Stop, 600));
        let mut custom = base.clone();
        custom["profiles"]["p"] = serde_json::json!({"workspaceRoots": ["."], "maxTurns": 7, "onMaxTurns": "ask", "maxTurnsAskTimeoutSecs": 30});
        let cfg: Config = serde_json::from_value(custom).unwrap();
        cfg.validate().unwrap();
        let p = &cfg.profiles["p"];
        assert_eq!((p.max_turns, p.on_max_turns, p.max_turns_ask_timeout_secs), (7, agent_core::OnMaxTurns::Ask, 30));
        for (field, value, expected) in [
            ("maxTurns", serde_json::json!(0), "profile 'p': maxTurns must be >= 1"),
            ("maxTurnsAskTimeoutSecs", serde_json::json!(0), "profile 'p': maxTurnsAskTimeoutSecs must be >= 1"),
            ("onMaxTurns", serde_json::json!("extend"), "unknown variant `extend`"),
            ("maxTurns", serde_json::json!(-1), "invalid value"),
        ] {
            let mut invalid = base.clone();
            invalid["profiles"]["p"][field] = value;
            let error = match serde_json::from_value::<Config>(invalid) {
                Ok(cfg) => cfg.validate().unwrap_err().to_string(),
                Err(e) => e.to_string(),
            };
            assert!(error.contains(expected), "{field}: {error}");
        }
    }

    #[test]
    fn readme_tool_approval_example_validates() {
        let readme = include_str!("../../../README.md");
        let section = &readme[readme.find("## 工具调用审批").expect("README section")..];
        let start = section.find("```json\n").unwrap() + "```json\n".len();
        let json = &section[start..start + section[start..].find("```").unwrap()];
        let mut cfg: Config = serde_json::from_str(json).unwrap();
        for client in &mut cfg.clients {
            client.token_sha256 = sha256_hex(&client.name);
        }
        cfg.validate().unwrap();
        assert_eq!(cfg.tool_approval_of(&cfg.profiles["careful"]), agent_tools::ToolApproval::Always);
        let coder = cfg.tool_approval_of(&cfg.profiles["coder"]);
        let verdict = |tool: &str, args: serde_json::Value| coder.evaluate(tool, &args);
        assert!(matches!(verdict("bash", serde_json::json!({"command": "git status && cargo test"})), agent_core::Verdict::Allow { .. }));
        assert!(matches!(verdict("bash", serde_json::json!({"command": "git push --force origin"})), agent_core::Verdict::Deny { .. }));
        assert!(matches!(verdict("bash", serde_json::json!({"command": "make"})), agent_core::Verdict::Ask { .. }));
        assert!(matches!(verdict("mcp__github__delete_repo", serde_json::json!({})), agent_core::Verdict::Deny { .. }));
        assert!(matches!(verdict("read", serde_json::json!({})), agent_core::Verdict::Allow { .. }));
        // 示例配置里的策略同样有效
        let example: Config = serde_json::from_str(include_str!("../../../sub-bot.example.json")).unwrap();
        assert!(example.tool_approval_of(&example.profiles["coder"]).is_active());
    }

    #[test]
    fn stream_idle_timeout_levels_validate_and_resolve() {
        let base = serde_json::json!({
            "streamIdleTimeoutSecs": 600,
            "providers": {
                "openai": {"streamIdleTimeoutSecs": 1200, "accounts": {"default": {}, "work": {"apiKeyEnv": "WORK_KEY", "streamIdleTimeoutSecs": 3600}}},
                "anthropic": {}
            },
            "profiles": {"p": {"workspaceRoots": ["."], "streamIdleTimeoutSecs": 7200}},
            "clients": []
        });
        let cfg: Config = serde_json::from_value(base.clone()).unwrap();
        cfg.validate().unwrap();
        let account = |id: &str, name: &str| cfg.account_config(id, name).unwrap();
        // account > provider > global
        assert_eq!(cfg.account_stream_idle_timeout_secs("openai", &account("openai", "work")), 3600);
        assert_eq!(cfg.account_stream_idle_timeout_secs("openai", &account("openai", "default")), 1200);
        assert_eq!(cfg.account_stream_idle_timeout_secs("anthropic", &account("anthropic", "default")), 600);
        assert_eq!(cfg.profiles["p"].stream_idle_timeout_secs, Some(7200));
        for (pointer, value) in [
            ("/streamIdleTimeoutSecs", 0),
            ("/providers/openai/streamIdleTimeoutSecs", 7201),
            ("/providers/openai/accounts/work/streamIdleTimeoutSecs", 0),
            ("/profiles/p/streamIdleTimeoutSecs", 86400),
        ] {
            let mut json = base.clone();
            *json.pointer_mut(pointer).unwrap() = serde_json::json!(value);
            let cfg: Config = serde_json::from_value(json).unwrap();
            let key = pointer.trim_start_matches('/').replace('/', ".");
            assert_eq!(cfg.validate().unwrap_err().to_string(), format!("{key} must be between 1 and 7200 (2 hours), got {value}"));
        }
        // Config::load reports the same key path.
        let dir = std::env::temp_dir().join(format!("sub-bot-idle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sub-bot.json");
        std::fs::write(&path, r#"{"providers":{"anthropic":{"accounts":{"default":{"streamIdleTimeoutSecs":0}}}},"profiles":{"p":{"workspaceRoots":["."]}}}"#).unwrap();
        let error = format!("{:#}", Config::load(&path).unwrap_err());
        assert!(error.contains("providers.anthropic.accounts.default.streamIdleTimeoutSecs must be between 1 and 7200 (2 hours)"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
