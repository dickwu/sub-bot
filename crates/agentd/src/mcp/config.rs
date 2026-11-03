//! MCP 服务器配置：全局 `mcpServers`（名字 → 定义）+ profile 的 `mcpServers`（引用名字 / 内联定义）。
//!
//! ```json
//! "mcpServers": {
//!   "fs":     {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/srv/repo"]},
//!   "github": {"url": "https://api.githubcopilot.com/mcp/", "headersFromEnv": {"Authorization": "GITHUB_MCP_AUTHORIZATION"}, "tools": ["get_*", "search_*"]}
//! },
//! "profiles": {"coder": {"mcpServers": ["fs", {"name": "github", "tools": ["get_issue"]}, {"name": "local", "command": "my-mcp"}]}}
//! ```
//!
//! profile 列表的每一项：
//! - `"name"`：引用全局定义；
//! - `{"name": "x", "tools": [...]}`（没有 command / url）：引用全局定义，并在服务器 allow-list 之上再收窄工具；
//! - `{"name": "x", "command" | "url": ..., ...}`：内联定义（名字全局唯一：不能与全局定义或其它 profile 的内联定义重名）。
//!
//! profile 没有 `mcpServers` ⇒ 没有 MCP 工具（向后兼容）。
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_CALL_TIMEOUT_SECS: u64 = 120;
pub const MAX_STARTUP_TIMEOUT_SECS: u64 = 600;
pub const MAX_CALL_TIMEOUT_SECS: u64 = 3600;
/// 服务器名长度上限：`mcp__<server>__<tool>` 要放进 provider 的 64 字符工具名里
pub const MAX_SERVER_NAME_LEN: usize = 32;

fn yes() -> bool {
    true
}
fn default_startup_timeout() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_SECS
}
fn default_call_timeout() -> u64 {
    DEFAULT_CALL_TIMEOUT_SECS
}

/// 一个 MCP 服务器：stdio（`command`）或 streamable HTTP（`url`）二选一。
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerConfig {
    /// stdio：可执行文件（按 PATH 查找）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// stdio：额外环境变量（值支持 `${VAR}` 引用 sub-bot 自己的环境）；子进程继承 sub-bot 的环境但剔除 provider key 等内置密钥变量
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// stdio：工作目录（相对路径相对于配置文件所在目录）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// streamable HTTP：端点 URL（http/https，不能带 user:password）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// HTTP 请求头；值支持 `${VAR}`（如 `"Bearer ${GITHUB_TOKEN}"`），从不写入日志
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// 请求头名 → 环境变量名（整个值取自该环境变量），密钥不必出现在配置文件里
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers_from_env: BTreeMap<String, String>,
    /// false ⇒ 不启动、不提供工具
    #[serde(default = "yes")]
    pub enabled: bool,
    /// 连接（启动 + initialize + tools/list）超时，默认 30
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout_secs: u64,
    /// 单次 tools/call 超时，默认 120
    #[serde(default = "default_call_timeout")]
    pub call_timeout_secs: u64,
    /// 工具 allow-list（服务器原始工具名，支持 `*` 通配）；不填 = 全部
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// true ⇒ 启动失败时 serve / run / rpc 直接失败；默认 false（告警 + `mcp_server_failed` issue，继续运行）
    #[serde(default)]
    pub required: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            headers_from_env: BTreeMap::new(),
            enabled: true,
            startup_timeout_secs: DEFAULT_STARTUP_TIMEOUT_SECS,
            call_timeout_secs: DEFAULT_CALL_TIMEOUT_SECS,
            tools: None,
            required: false,
        }
    }
}

/// 不输出 env / header 的值（可能是密钥），也不输出 args（可能含路径/令牌）。
impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("transport", &self.transport())
            .field("command", &self.command)
            .field("args", &self.args.len())
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("headers", &self.headers.keys().chain(self.headers_from_env.keys()).collect::<Vec<_>>())
            .field("enabled", &self.enabled)
            .field("required", &self.required)
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

impl McpServerConfig {
    pub fn transport(&self) -> &'static str {
        if self.url.is_some() {
            "http"
        } else {
            "stdio"
        }
    }

    /// 工具是否在 allow-list 里（未设置 = 全部）。
    pub fn tool_allowed(&self, tool: &str) -> bool {
        self.tools.as_ref().is_none_or(|list| list.iter().any(|p| glob(p, tool)))
    }

    fn validate(&self, at: &str) -> anyhow::Result<()> {
        match (&self.command, &self.url) {
            (Some(_), Some(_)) => anyhow::bail!("{at}: set only one of command (stdio) / url (streamable HTTP)"),
            (None, None) => anyhow::bail!("{at}: command (stdio) or url (streamable HTTP) is required"),
            (Some(command), None) => {
                anyhow::ensure!(!command.trim().is_empty(), "{at}: command must not be empty");
                anyhow::ensure!(self.headers.is_empty() && self.headers_from_env.is_empty(), "{at}: headers / headersFromEnv are only valid for url servers");
                for (k, v) in &self.env {
                    anyhow::ensure!(valid_env_name(k), "{at}: env name '{k}' must match ^[A-Za-z_][A-Za-z0-9_]*$");
                    check_refs(v).map_err(|e| anyhow::anyhow!("{at}: env.{k}: {e}"))?;
                }
            }
            (None, Some(url)) => {
                anyhow::ensure!(self.args.is_empty() && self.env.is_empty() && self.cwd.is_none(), "{at}: args / env / cwd are only valid for command servers");
                let parsed = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("{at}: url must be an http/https URL"))?;
                anyhow::ensure!(matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some(), "{at}: url must be an http/https URL");
                anyhow::ensure!(parsed.username().is_empty() && parsed.password().is_none(), "{at}: url must not contain credentials (use headers / headersFromEnv)");
                for (name, value) in &self.headers {
                    check_header_name(name).map_err(|e| anyhow::anyhow!("{at}: headers: {e}"))?;
                    check_refs(value).map_err(|e| anyhow::anyhow!("{at}: headers.{name}: {e}"))?;
                }
                for (name, var) in &self.headers_from_env {
                    check_header_name(name).map_err(|e| anyhow::anyhow!("{at}: headersFromEnv: {e}"))?;
                    anyhow::ensure!(valid_env_name(var), "{at}: headersFromEnv.{name} must be an environment variable name");
                    anyhow::ensure!(!self.headers.keys().any(|h| h.eq_ignore_ascii_case(name)), "{at}: header '{name}' is set in both headers and headersFromEnv");
                }
            }
        }
        anyhow::ensure!((1..=MAX_STARTUP_TIMEOUT_SECS).contains(&self.startup_timeout_secs), "{at}: startupTimeoutSecs must be 1..={MAX_STARTUP_TIMEOUT_SECS}");
        anyhow::ensure!((1..=MAX_CALL_TIMEOUT_SECS).contains(&self.call_timeout_secs), "{at}: callTimeoutSecs must be 1..={MAX_CALL_TIMEOUT_SECS}");
        validate_tool_patterns(at, self.tools.as_deref())
    }
}

/// profile `mcpServers` 的一项。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ProfileMcpServer {
    /// 引用全局定义；`tools` 在服务器 allow-list 之上再收窄
    Ref {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<String>>,
    },
    /// 内联定义
    Inline {
        name: String,
        #[serde(flatten)]
        server: Box<McpServerConfig>,
    },
}

impl ProfileMcpServer {
    pub fn name(&self) -> &str {
        match self {
            Self::Ref { name, .. } | Self::Inline { name, .. } => name,
        }
    }
    /// profile 级的工具收窄（只有引用形式有）
    pub fn narrow(&self) -> Option<&[String]> {
        match self {
            Self::Ref { tools, .. } => tools.as_deref(),
            Self::Inline { .. } => None,
        }
    }
}

impl<'de> Deserialize<'de> for ProfileMcpServer {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match Value::deserialize(d)? {
            Value::String(name) => Ok(Self::Ref { name, tools: None }),
            Value::Object(mut map) => {
                let name = match map.remove("name") {
                    Some(Value::String(n)) => n,
                    _ => return Err(D::Error::custom("mcpServers entry needs a string \"name\"")),
                };
                if !map.contains_key("command") && !map.contains_key("url") {
                    // 引用：只允许 tools
                    if let Some(k) = map.keys().find(|k| *k != "tools") {
                        return Err(D::Error::custom(format!("mcpServers entry '{name}': '{k}' is only valid in an inline definition (with command or url); a reference may only set \"tools\"")));
                    }
                    let tools = map.remove("tools").map(serde_json::from_value).transpose().map_err(|e| D::Error::custom(format!("mcpServers entry '{name}': tools: {e}")))?;
                    return Ok(Self::Ref { name, tools });
                }
                let server = serde_json::from_value(Value::Object(map)).map_err(|e| D::Error::custom(format!("mcpServers entry '{name}': {e}")))?;
                Ok(Self::Inline { name, server: Box::new(server) })
            }
            _ => Err(D::Error::custom("mcpServers entry must be a server name or an object")),
        }
    }
}

/// 名字：`[a-zA-Z0-9_-]{1,32}`，不含 `__`（工具名用 `__` 分隔服务器与工具）。
pub fn valid_server_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_SERVER_NAME_LEN && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')) && !name.contains("__")
}

fn valid_env_name(name: &str) -> bool {
    let mut b = name.bytes();
    b.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_') && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn check_header_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_ok(), "invalid header name '{name}'");
    // rmcp 自己管理这些请求头
    for reserved in ["accept", "mcp-session-id", "last-event-id", "mcp-protocol-version", "content-type"] {
        anyhow::ensure!(!name.eq_ignore_ascii_case(reserved), "header '{name}' is managed by the MCP transport");
    }
    Ok(())
}

fn validate_tool_patterns(at: &str, tools: Option<&[String]>) -> anyhow::Result<()> {
    for p in tools.unwrap_or_default() {
        anyhow::ensure!(!p.is_empty() && p.len() <= 128 && !p.chars().any(char::is_whitespace), "{at}: invalid tools pattern '{p}'");
    }
    Ok(())
}

/// `*` 通配（与 toolApproval 一致）。
pub fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// 检查 `${VAR}` 语法（不检查变量是否存在：缺失时只影响该服务器的连接）。
fn check_refs(value: &str) -> anyhow::Result<()> {
    expand(value, |_| Some(String::new())).map(|_| ()).map_err(anyhow::Error::msg)
}

/// 展开 `${VAR}`；`$` 后面不是 `{` 时原样保留。变量缺失 ⇒ Err（错误里只有变量名）。
pub fn expand(value: &str, getenv: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let end = after.find('}').ok_or_else(|| "unterminated ${...} reference".to_string())?;
        let var = &after[..end];
        if !valid_env_name(var) {
            return Err(format!("invalid variable name in ${{{var}}}"));
        }
        out.push_str(&getenv(var).ok_or_else(|| format!("environment variable {var} is not set"))?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// 全局 + 内联定义合并后的服务器表，以及每个 profile 引用的服务器。
#[derive(Debug, Clone, Default)]
pub struct McpPlan {
    pub servers: BTreeMap<String, McpServerConfig>,
    pub profiles: BTreeMap<String, Vec<ProfileMcpServer>>,
}

/// 校验并合并（`Config::validate` 调用）。
pub fn plan(global: &BTreeMap<String, McpServerConfig>, profiles: &BTreeMap<String, crate::config::Profile>) -> anyhow::Result<McpPlan> {
    let mut servers = BTreeMap::new();
    for (name, server) in global {
        anyhow::ensure!(valid_server_name(name), "mcpServers: invalid server name '{name}' (allowed: [A-Za-z0-9_-]{{1,{MAX_SERVER_NAME_LEN}}}, no \"__\")");
        server.validate(&format!("mcpServers.{name}"))?;
        servers.insert(name.clone(), server.clone());
    }
    let mut inline_owner: BTreeMap<String, String> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for (profile, p) in profiles {
        let at = format!("profile '{profile}': mcpServers");
        let mut seen = vec![];
        for entry in &p.mcp_servers {
            let name = entry.name();
            anyhow::ensure!(valid_server_name(name), "{at}: invalid server name '{name}' (allowed: [A-Za-z0-9_-]{{1,{MAX_SERVER_NAME_LEN}}}, no \"__\")");
            anyhow::ensure!(!seen.contains(&name), "{at}: duplicate server '{name}'");
            seen.push(name);
            match entry {
                ProfileMcpServer::Ref { tools, .. } => {
                    anyhow::ensure!(global.contains_key(name), "{at}: unknown server '{name}' (define it in the global mcpServers)");
                    validate_tool_patterns(&format!("{at}.{name}"), tools.as_deref())?;
                }
                ProfileMcpServer::Inline { server, .. } => {
                    anyhow::ensure!(!global.contains_key(name), "{at}: inline server '{name}' collides with the global mcpServers entry of the same name");
                    if let Some(owner) = inline_owner.get(name) {
                        anyhow::bail!("{at}: inline server '{name}' is already defined by profile '{owner}'; move it to the global mcpServers and reference it by name");
                    }
                    server.validate(&format!("{at}.{name}"))?;
                    inline_owner.insert(name.to_string(), profile.clone());
                    servers.insert(name.to_string(), (**server).clone());
                }
            }
        }
        if !p.mcp_servers.is_empty() {
            out.insert(profile.clone(), p.mcp_servers.clone());
        }
    }
    Ok(McpPlan { servers, profiles: out })
}

/// 相对 cwd 解析为相对配置文件目录（`Config::load` 调用）。
pub fn resolve_paths(global: &mut BTreeMap<String, McpServerConfig>, profiles: &mut BTreeMap<String, crate::config::Profile>, dir: &Path) {
    let servers = global.values_mut().chain(profiles.values_mut().flat_map(|p| p.mcp_servers.iter_mut()).filter_map(|e| match e {
        ProfileMcpServer::Inline { server, .. } => Some(&mut **server),
        ProfileMcpServer::Ref { .. } => None,
    }));
    for server in servers {
        if let Some(cwd) = server.cwd.as_mut().filter(|c| c.is_relative()) {
            *cwd = dir.join(&*cwd);
        }
    }
}
