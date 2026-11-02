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
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
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
}

fn default_tools() -> Vec<String> {
    agent_tools::DEFAULT_TOOLS.iter().map(|s| s.to_string()).collect()
}
fn yes() -> bool {
    true
}
fn default_max_turns() -> usize {
    50
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
    /// LLM 流连续无字节的超时（秒）。
    #[serde(default = "default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
    #[serde(default = "default_llm_max_retries")]
    pub llm_max_retries: u32,
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
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub clients: Vec<Client>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub api_key_file: Option<PathBuf>,
    pub base_url: Option<String>,
    /// 该 provider 在没有配置/请求模型时的默认模型（anthropic 默认 `claude-opus-5-5`）
    #[serde(default)]
    pub default_model: Option<String>,
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
    300
}
fn default_chat_timeout_secs() -> u64 {
    900
}

fn default_model() -> String {
    "gpt-4.1-mini".into()
}

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
            if let Some(file) = &mut provider.api_key_file {
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
        cfg.validate()?;
        Ok(cfg)
    }

    /// 只做结构/注册表校验，不访问工作目录或网络。
    pub fn validate(&self) -> anyhow::Result<()> {
        self.socket_mode()?;
        for id in self.default_provider.iter().chain(self.providers.keys()).chain(self.profiles.values().filter_map(|p| p.provider.as_ref())) {
            validate_provider(id)?;
        }
        if let (Some(top), Some(alias)) = (&self.api_key_file, self.providers.get("openai").and_then(|p| p.api_key_file.as_ref())) {
            anyhow::ensure!(top == alias, "apiKeyFile and providers.openai.apiKeyFile must match");
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
        let registry = agent_tools::ToolRegistry::with_builtins();
        for (name, profile) in &self.profiles {
            anyhow::ensure!(!profile.workspace_roots.is_empty(), "profile '{name}': workspaceRoots must not be empty");
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
                token_budget: None,
                run_timeout_secs: None,
                tools: default_tools(),
                workspace_roots: vec![workspace],
                load_context_files: true,
                http_allow_hosts: vec![],
                shell_env: Default::default(),
                allow_system_prompt_override: true,
                allow_caller_tools: true,
                max_turns: 30,
                deep_model: None,
                auto_route: false,
                router_model: None,
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
            chat_timeout_secs: default_chat_timeout_secs(),
            default_model: model,
            default_reasoning_effort: None,
            default_api: Default::default(),
            api_key_file: None,
            session_idle_ttl_secs: default_session_idle_ttl(),
            chat_session_ttl_secs: default_chat_session_ttl(),
            max_sessions_per_client: default_max_sessions(),
            profiles,
            clients: vec![Client { name: "default".into(), token_sha256: sha256_hex(token), profiles: vec!["default".into()], ..Default::default() }],
        }
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
        assert_eq!((cfg.stream_idle_timeout_secs, cfg.chat_timeout_secs), (300, 900));
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

    #[test]
    fn api_key_file_is_relative_to_config_and_shell_env_parses() {
        let dir = std::env::temp_dir().join(format!("sub-bot-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sub-bot.json");
        std::fs::write(&path, r#"{"apiKeyFile":"keys/openai","profiles":{"p":{"workspaceRoots":["."],"shellEnv":{"mode":"allowlist","allow":["CARGO_*"]}}}}"#).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.api_key_file.as_deref(), Some(dir.join("keys/openai").as_path()));
        assert_eq!(cfg.stream_idle_timeout_secs, 300);
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
}
