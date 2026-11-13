//! MCP 客户端（官方 Rust SDK `rmcp`）：连接配置的 MCP 服务器（stdio 子进程 / streamable HTTP），把它们的工具以
//! `mcp__<server>__<tool>` 暴露给会话。
//!
//! 生命周期：`serve` / `run` / `rpc` 启动时并行连接（每个服务器 `startupTimeoutSecs`）；`required` 服务器失败 ⇒ 启动失败，
//! 其它失败 ⇒ WARN + `mcp_server_failed` issue，继续运行（没有它的工具）。工具列表在会话创建时快照；
//! `notifications/tools/list_changed` 会刷新列表（之后创建的会话生效）。
//!
//! 崩溃 / 断线：进行中的调用返回明确错误（`MCP server 'x' exited (...); restarting`），WARN + `mcp_server_crashed` issue；
//! 下一次使用时按退避（1s、2s、5s…）重连，5 分钟内最多 5 次。stdio 子进程的 stderr 以 debug 级别写入日志，最后几行附在错误 / issue 里。
//!
//! stdio 子进程由这里自己 spawn（独立进程组，便于整组结束 `npx` 之类的包装进程，并拿到退出状态），
//! 用 rmcp 的 async-rw 传输接入；HTTP 用 rmcp 的 streamable HTTP 客户端（reqwest 0.13 + rustls/ring + webpki-roots）。
pub mod config;
#[cfg(test)]
mod tests;
pub mod tool;

pub use config::{McpServerConfig, ProfileMcpServer};
use tool::{McpTool, ToolInfo};

use crate::{
    config::Client,
    hub::Hub,
    issues::{IssueEvent, Reporter, Severity},
};
use agent_core::AgentTool;
use rmcp::{
    model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation, ProtocolVersion},
    service::{NotificationContext, RunningService, ServiceError},
    transport::{streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport},
    ClientHandler, Peer, RoleClient, ServiceExt,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

/// 重启退避（按连续失败次数），最后一档重复
const BACKOFF_SECS: &[u64] = &[1, 2, 5];
/// 每个窗口内最多重启次数
const MAX_RESTARTS: usize = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(300);
/// 保留的 stderr 行数 / 每行长度
const STDERR_LINES: usize = 20;
const STDERR_LINE_MAX: usize = 500;
/// 错误 / issue 里附带的 stderr 行数
const STDERR_IN_ERROR: usize = 5;
/// 主动关闭时等子进程退出的时间
const KILL_GRACE: Duration = Duration::from_secs(2);

/// 必需（`required: true`）的 MCP 服务器启动失败。文案只含服务器名与失败原因（不含请求头 / 环境变量的值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStartupError {
    pub server: String,
    pub reason: String,
}

impl std::fmt::Display for McpStartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "required MCP server '{}' failed to start: {}", self.server, self.reason)
    }
}

impl std::error::Error for McpStartupError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// `enabled: false`
    Disabled,
    /// 尚未连接（未调用 start，例如测试或诊断以外的命令）
    Idle,
    Connecting,
    Ready,
    /// 连接 / 重启失败
    Failed,
    /// 运行中退出 / 断线，下次使用时重启
    Crashed,
}

struct State {
    status: Status,
    tools: Arc<Vec<ToolInfo>>,
    server_info: Option<(String, String)>,
    last_error: Option<String>,
    connect_ms: Option<u64>,
    connected_at: Option<u64>,
    /// 成功的重启次数
    restarts: u32,
    /// 窗口内的重启尝试时间
    attempts: VecDeque<Instant>,
    /// 连续失败（崩溃或连接失败）次数，决定退避
    failures: u32,
    last_failure: Option<Instant>,
    /// 当前连接的代号（0 = 无）
    live: u64,
    /// 已上报过崩溃的代号（进程退出监视与进行中的调用只报一次）
    crash_reported: u64,
    /// 曾经尝试过连接（之后的连接算作重启）
    started: bool,
}

/// stdio 子进程句柄：drop ⇒ 结束整个进程组。
struct ChildHandle {
    pid: Option<u32>,
    exit: watch::Receiver<Option<String>>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    closing: Arc<AtomicBool>,
    kill: Option<oneshot::Sender<()>>,
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        // 监视任务收到后先 SIGTERM 整组，宽限后 SIGKILL
        drop(self.kill.take());
    }
}

struct Conn {
    generation: u64,
    service: RunningService<RoleClient, Handler>,
    child: Option<ChildHandle>,
}

impl Conn {
    fn peer(&self) -> Peer<RoleClient> {
        self.service.peer().clone()
    }
    /// 已死亡的原因（进程退出 / 连接关闭）；活着 ⇒ None
    fn dead_reason(&self) -> Option<String> {
        if let Some(status) = self.child.as_ref().and_then(|c| c.exit.borrow().clone()) {
            return Some(format!("exited ({status})"));
        }
        if self.service.is_closed() || self.service.peer().is_transport_closed() {
            return Some("connection closed".into());
        }
        None
    }
    fn lease(&self) -> Lease {
        Lease { peer: self.peer(), generation: self.generation, exit: self.child.as_ref().map(|c| c.exit.clone()), stderr: self.child.as_ref().map(|c| c.stderr.clone()) }
    }
}

/// 一次调用借用的连接。
struct Lease {
    peer: Peer<RoleClient>,
    generation: u64,
    exit: Option<watch::Receiver<Option<String>>>,
    stderr: Option<Arc<Mutex<VecDeque<String>>>>,
}

pub(crate) struct Server {
    pub(crate) name: String,
    cfg: McpServerConfig,
    issues: Arc<Reporter>,
    weak: Weak<Server>,
    slot: tokio::sync::Mutex<Option<Conn>>,
    state: Mutex<State>,
    generation: AtomicU64,
    /// 已登记到 issue 脱敏表的密钥（避免重复登记）
    secrets: Mutex<HashSet<String>>,
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_default()
}

fn tail(lines: &Mutex<VecDeque<String>>, n: usize) -> Vec<String> {
    let lines = lines.lock().unwrap();
    lines.iter().skip(lines.len().saturating_sub(n)).cloned().collect()
}

fn with_stderr(message: String, stderr: &[String]) -> String {
    if stderr.is_empty() {
        message
    } else {
        format!("{message}\nstderr (last lines):\n{}", stderr.join("\n"))
    }
}

impl Server {
    fn new(name: String, cfg: McpServerConfig, issues: Arc<Reporter>) -> Arc<Self> {
        let status = if cfg.enabled { Status::Idle } else { Status::Disabled };
        Arc::new_cyclic(|weak| Self {
            name,
            cfg,
            issues,
            weak: weak.clone(),
            slot: tokio::sync::Mutex::new(None),
            state: Mutex::new(State {
                status,
                tools: Arc::new(vec![]),
                server_info: None,
                last_error: None,
                connect_ms: None,
                connected_at: None,
                restarts: 0,
                attempts: VecDeque::new(),
                failures: 0,
                last_failure: None,
                live: 0,
                crash_reported: 0,
                started: false,
            }),
            generation: AtomicU64::new(0),
            secrets: Mutex::new(HashSet::new()),
        })
    }

    fn status(&self) -> Status {
        self.state.lock().unwrap().status
    }

    pub(crate) fn tools(&self) -> Arc<Vec<ToolInfo>> {
        self.state.lock().unwrap().tools.clone()
    }

    fn add_secret(&self, value: &str) {
        // 太短的值替换会误伤普通文本
        if value.len() >= 8 && self.secrets.lock().unwrap().insert(value.to_string()) {
            self.issues.add_secret(value.to_string());
        }
    }

    /// 初次连接（启动时）。不计入重启次数。
    async fn start(&self) -> Result<(), String> {
        if !self.cfg.enabled {
            tracing::info!(server = %self.name, "MCP server disabled; skipping");
            return Ok(());
        }
        let mut slot = self.slot.lock().await;
        if slot.is_some() {
            return Ok(());
        }
        self.state.lock().unwrap().started = true;
        match self.connect().await {
            Ok(conn) => {
                *slot = Some(conn);
                Ok(())
            }
            Err(e) => {
                self.fail(&e, false);
                Err(e)
            }
        }
    }

    /// 记录一次连接失败；`restart` 区分启动失败与重启失败（日志 / issue 文案）。
    fn fail(&self, error: &str, restart: bool) {
        {
            let mut st = self.state.lock().unwrap();
            st.status = Status::Failed;
            st.last_error = Some(error.to_string());
            st.failures += 1;
            st.last_failure = Some(Instant::now());
            st.live = 0;
        }
        let what = if restart { "restart" } else { "startup" };
        tracing::warn!(issue = "typed", server = %self.name, transport = self.cfg.transport(), required = self.cfg.required, error = %error, "MCP server {what} failed; continuing without its tools");
        self.issues.report(
            IssueEvent::new(Severity::Warn, "mcp_server_failed", "mcp", format!("MCP server '{}' {what} failed: {error}", self.name))
                .details(json!({"server": self.name, "transport": self.cfg.transport(), "phase": what, "required": self.cfg.required})),
        );
    }

    /// 记录一次崩溃 / 断线（同一代号只报一次）。返回给调用方的错误文案。
    fn crashed(&self, generation: u64, reason: &str, stderr: &[String]) -> String {
        let message = format!("MCP server '{}' {reason}; restarting", self.name);
        let first = {
            let mut st = self.state.lock().unwrap();
            let first = st.crash_reported != generation && st.live == generation;
            if first {
                st.crash_reported = generation;
                st.status = Status::Crashed;
                st.last_error = Some(with_stderr(message.clone(), stderr));
                st.failures += 1;
                st.last_failure = Some(Instant::now());
                st.live = 0;
            }
            first
        };
        if first {
            tracing::warn!(issue = "typed", server = %self.name, transport = self.cfg.transport(), reason, stderr = ?stderr, "MCP server crashed; will restart on next use");
            self.issues.report(
                IssueEvent::new(Severity::Warn, "mcp_server_crashed", "mcp", with_stderr(message.clone(), stderr))
                    .details(json!({"server": self.name, "transport": self.cfg.transport(), "reason": reason, "stderr": stderr})),
            );
        }
        with_stderr(message, stderr)
    }

    /// 下次重启前要等多久；超过重启上限 ⇒ Err。
    fn restart_delay(&self) -> Result<Duration, String> {
        let mut st = self.state.lock().unwrap();
        let now = Instant::now();
        while st.attempts.front().is_some_and(|t| now.duration_since(*t) >= RESTART_WINDOW) {
            st.attempts.pop_front();
        }
        if st.attempts.len() >= MAX_RESTARTS {
            let last = st.last_error.clone().unwrap_or_default();
            return Err(format!("MCP server '{}' is unavailable: restart limit reached ({MAX_RESTARTS} attempts in {} min); last error: {last}", self.name, RESTART_WINDOW.as_secs() / 60));
        }
        if st.failures == 0 {
            return Ok(Duration::ZERO);
        }
        let backoff = Duration::from_secs(BACKOFF_SECS[(st.failures as usize - 1).min(BACKOFF_SECS.len() - 1)]);
        Ok(st.last_failure.map_or(Duration::ZERO, |t| backoff.saturating_sub(now.duration_since(t))))
    }

    /// 取得可用连接：活着就复用；死了 / 没连上 ⇒ 按退避重启。
    async fn ensure(&self, cancel: &CancellationToken) -> Result<Lease, String> {
        if !self.cfg.enabled {
            return Err(format!("MCP server '{}' is disabled", self.name));
        }
        let mut slot = tokio::select! {
            s = self.slot.lock() => s,
            _ = cancel.cancelled() => return Err("aborted".into()),
        };
        if let Some(conn) = slot.as_ref() {
            match conn.dead_reason() {
                None => return Ok(conn.lease()),
                Some(reason) => {
                    let stderr = conn.child.as_ref().map(|c| tail(&c.stderr, STDERR_IN_ERROR)).unwrap_or_default();
                    self.crashed(conn.generation, &reason, &stderr);
                    *slot = None;
                }
            }
        }
        let restart = std::mem::replace(&mut self.state.lock().unwrap().started, true);
        if restart {
            let delay = self.restart_delay()?;
            if !delay.is_zero() {
                tracing::debug!(server = %self.name, delay_ms = delay.as_millis() as u64, "waiting before MCP server restart");
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.cancelled() => return Err("aborted".into()),
                }
            }
            self.state.lock().unwrap().attempts.push_back(Instant::now());
            tracing::info!(server = %self.name, transport = self.cfg.transport(), "restarting MCP server");
        }
        let connected = tokio::select! {
            r = self.connect() => r,
            _ = cancel.cancelled() => return Err("aborted".into()),
        };
        match connected {
            Ok(conn) => {
                if restart {
                    self.state.lock().unwrap().restarts += 1;
                }
                let lease = conn.lease();
                *slot = Some(conn);
                Ok(lease)
            }
            Err(e) => {
                self.fail(&e, restart);
                Err(format!("MCP server '{}' is unavailable: {e}", self.name))
            }
        }
    }

    /// 后台重连（会话创建时发现服务器不可用）：只在退避已过、没有其它连接进行中时尝试，不等待。
    fn reconnect_in_background(&self) {
        if !matches!(self.status(), Status::Failed | Status::Crashed) || !self.restart_delay().is_ok_and(|d| d.is_zero()) {
            return;
        }
        let (Some(server), Ok(handle)) = (self.weak.upgrade(), tokio::runtime::Handle::try_current()) else { return };
        handle.spawn(async move {
            if server.slot.try_lock().is_err() {
                return;
            }
            let _ = server.ensure(&CancellationToken::new()).await;
        });
    }

    /// 建立一个新连接：spawn / HTTP 握手 + initialize + tools/list（分页），整体受 startupTimeoutSecs 限制。
    async fn connect(&self) -> Result<Conn, String> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.state.lock().unwrap().status = Status::Connecting;
        let started = Instant::now();
        let timeout = Duration::from_secs(self.cfg.startup_timeout_secs);
        let handler = Handler { server: self.weak.clone(), generation };
        let (child, handshake) = match self.cfg.transport() {
            "stdio" => {
                let (child, stdout, stdin) = self.spawn(generation)?;
                (Some(child), tokio::time::timeout(timeout, async { handler.serve((stdout, stdin)).await.map_err(|e| e.to_string()) }).await)
            }
            _ => {
                let transport = self.http_transport()?;
                (None, tokio::time::timeout(timeout, async { handler.serve(transport).await.map_err(|e| e.to_string()) }).await)
            }
        };
        let result = match handshake {
            // 没有声明 tools 能力的服务器：没有工具，但连接本身有效
            Ok(Ok(service)) if service.peer().peer_info().is_some_and(|i| i.capabilities.tools.is_none()) => {
                tracing::info!(server = %self.name, "MCP server does not advertise the tools capability; it provides no tools");
                Ok((service, vec![]))
            }
            Ok(Ok(service)) => match tokio::time::timeout(timeout.saturating_sub(started.elapsed()), service.peer().list_all_tools()).await {
                Ok(Ok(tools)) => Ok((service, tools)),
                Ok(Err(e)) => Err(format!("tools/list failed: {e}")),
                Err(_) => Err(format!("tools/list timed out (startupTimeoutSecs={})", self.cfg.startup_timeout_secs)),
            },
            Ok(Err(e)) => Err(format!("initialize failed: {e}")),
            Err(_) => Err(format!("timed out after {}s (startupTimeoutSecs)", self.cfg.startup_timeout_secs)),
        };
        let (service, tools) = match result {
            Ok(ok) => ok,
            Err(e) => {
                // 进程已经退出时带上退出状态与 stderr
                let Some(child) = child else { return Err(e) };
                let mut exit = child.exit.clone();
                let status = tokio::time::timeout(Duration::from_millis(500), exit.wait_for(Option::is_some)).await.ok().and_then(|r| r.ok().and_then(|s| s.clone()));
                let e = match status {
                    Some(status) => format!("{e} (process exited: {status})"),
                    None => e,
                };
                return Err(with_stderr(e, &tail(&child.stderr, STDERR_IN_ERROR)));
            }
        };
        let connect_ms = started.elapsed().as_millis() as u64;
        let info = service.peer().peer_info().and_then(|i| i.server_info.clone()).map(|i| (i.name, i.version));
        let tools = self.tool_infos(tools.into_iter().map(|t| serde_json::to_value(t).unwrap_or_default()).collect());
        tracing::info!(
            server = %self.name,
            transport = self.cfg.transport(),
            connect_ms,
            server_name = info.as_ref().map(|i| i.0.as_str()).unwrap_or("-"),
            server_version = info.as_ref().map(|i| i.1.as_str()).unwrap_or("-"),
            tools = tools.len(),
            pid = ?child.as_ref().and_then(|c| c.pid),
            "MCP server connected"
        );
        tracing::debug!(server = %self.name, tools = ?tools.iter().map(|t| t.exposed.as_str()).collect::<Vec<_>>(), "MCP tools");
        {
            let mut st = self.state.lock().unwrap();
            st.status = Status::Ready;
            st.tools = Arc::new(tools);
            st.server_info = info;
            st.last_error = None;
            st.connect_ms = Some(connect_ms);
            st.connected_at = Some(unix_ms());
            st.failures = 0;
            st.live = generation;
        }
        Ok(Conn { generation, service, child })
    }

    /// tools/list 结果（JSON）→ allow-list 过滤、命名、schema 清洗。
    fn tool_infos(&self, tools: Vec<Value>) -> Vec<ToolInfo> {
        let mut kept = vec![];
        for t in tools {
            let Some(name) = t.get("name").and_then(Value::as_str).map(str::to_string) else { continue };
            if !self.cfg.tool_allowed(&name) {
                tracing::debug!(server = %self.name, tool = %name, "MCP tool filtered out by tools allow-list");
                continue;
            }
            let description = t.get("description").and_then(Value::as_str).or_else(|| t.get("title").and_then(Value::as_str)).unwrap_or_default();
            let description = if description.trim().is_empty() { format!("MCP tool '{name}' from server '{}'", self.name) } else { description.to_string() };
            kept.push((name, description, tool::sanitize_schema(t.get("inputSchema").unwrap_or(&Value::Null))));
        }
        let names = tool::assign_names(&self.name, &kept.iter().map(|k| k.0.clone()).collect::<Vec<_>>());
        kept.into_iter()
            .zip(names)
            .map(|((name, description, schema), exposed)| {
                if exposed != format!("mcp__{}__{name}", self.name) {
                    tracing::debug!(server = %self.name, tool = %name, exposed = %exposed, "MCP tool name mapped to provider-safe name");
                }
                ToolInfo { mcp_name: name, exposed, description, schema }
            })
            .collect()
    }

    /// `notifications/tools/list_changed`：重新拉取工具列表（之后创建的会话生效）。
    async fn refresh_tools(&self, generation: u64, peer: Peer<RoleClient>) {
        match peer.list_all_tools().await {
            Ok(tools) => {
                let tools = self.tool_infos(tools.into_iter().map(|t| serde_json::to_value(t).unwrap_or_default()).collect());
                let mut st = self.state.lock().unwrap();
                if st.live != generation {
                    return;
                }
                tracing::info!(server = %self.name, before = st.tools.len(), after = tools.len(), "MCP tool list changed; refreshed");
                st.tools = Arc::new(tools);
            }
            Err(e) => tracing::warn!(server = %self.name, error = %e, "MCP tool list refresh failed"),
        }
    }

    fn spawn(&self, generation: u64) -> Result<(ChildHandle, tokio::process::ChildStdout, tokio::process::ChildStdin), String> {
        let command = self.cfg.command.as_deref().unwrap_or_default();
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(&self.cfg.args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        // 继承环境但剔除 provider key 等内置密钥变量（与 bash 工具的默认策略相同）
        agent_tools::ShellEnv::default().apply(&mut cmd);
        for (k, v) in &self.cfg.env {
            let value = config::expand(v, |var| std::env::var(var).ok()).map_err(|e| format!("env.{k}: {e}"))?;
            self.add_secret(&value);
            cmd.env(k, value);
        }
        if let Some(cwd) = &self.cfg.cwd {
            cmd.current_dir(cwd);
        }
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| format!("cannot spawn '{command}': {e}"))?;
        let pid = child.id();
        let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else { return Err("child stdio unavailable".into()) };
        let lines = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = stderr {
            let (lines, name) = (lines.clone(), self.name.clone());
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut reader = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let line: String = line.chars().take(STDERR_LINE_MAX).collect();
                    tracing::debug!(server = %name, line = %line, "MCP server stderr");
                    let mut buf = lines.lock().unwrap();
                    if buf.len() >= STDERR_LINES {
                        buf.pop_front();
                    }
                    buf.push_back(line);
                }
            });
        }
        let (exit_tx, exit_rx) = watch::channel(None);
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let closing = Arc::new(AtomicBool::new(false));
        let (weak, closing2, stderr_lines) = (self.weak.clone(), closing.clone(), lines.clone());
        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s,
                _ = kill_rx => {
                    signal_group(pid, Signal::Term);
                    match tokio::time::timeout(KILL_GRACE, child.wait()).await {
                        Ok(s) => s,
                        Err(_) => {
                            signal_group(pid, Signal::Kill);
                            let _ = child.start_kill();
                            child.wait().await
                        }
                    }
                }
            };
            let status = match status {
                Ok(s) => s.to_string(),
                Err(e) => format!("wait failed: {e}"),
            };
            // 进程组里可能还有孙进程（如 npx 启动的 node）
            signal_group(pid, Signal::Kill);
            let _ = exit_tx.send(Some(status.clone()));
            if closing2.load(Ordering::SeqCst) {
                tracing::debug!(pid = ?pid, status = %status, "MCP server process stopped");
            } else if let Some(server) = weak.upgrade() {
                // 退出时可能还有 stderr 没读完
                tokio::time::sleep(Duration::from_millis(50)).await;
                server.crashed(generation, &format!("exited ({status})"), &tail(&stderr_lines, STDERR_IN_ERROR));
            }
        });
        Ok((ChildHandle { pid, exit: exit_rx, stderr: lines, closing, kill: Some(kill_tx) }, stdout, stdin))
    }

    fn http_transport(&self) -> Result<StreamableHttpClientTransport<reqwest::Client>, String> {
        use reqwest::header::{HeaderName, HeaderValue};
        let url = self.cfg.url.clone().unwrap_or_default();
        let mut headers = std::collections::HashMap::new();
        let mut put = |name: &str, value: String| -> Result<(), String> {
            self.add_secret(&value);
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| format!("invalid header name '{name}'"))?;
            // 不回显值
            let mut value = HeaderValue::from_str(&value).map_err(|_| format!("header '{name}' has an invalid value"))?;
            value.set_sensitive(true);
            headers.insert(name, value);
            Ok(())
        };
        for (name, value) in &self.cfg.headers {
            put(name, config::expand(value, |var| std::env::var(var).ok()).map_err(|e| format!("headers.{name}: {e}"))?)?;
        }
        for (name, var) in &self.cfg.headers_from_env {
            put(name, std::env::var(var).map_err(|_| format!("environment variable {var} (headersFromEnv.{name}) is not set"))?)?;
        }
        let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers);
        Ok(StreamableHttpClientTransport::with_client(http_client()?, config))
    }

    /// tools/call。超时 / 取消时丢弃请求 future，rmcp 会发送 `notifications/cancelled`。
    pub(crate) async fn call(&self, tool: &str, call_id: &str, args: Value, cancel: &CancellationToken) -> anyhow::Result<Value> {
        let lease = self.ensure(cancel).await.map_err(anyhow::Error::msg)?;
        let arguments = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => anyhow::bail!("MCP tool arguments must be a JSON object, got {other}"),
        };
        let params: CallToolRequestParams = serde_json::from_value(json!({"name": tool, "arguments": arguments.unwrap_or_default()}))?;
        let started = Instant::now();
        let timeout = Duration::from_secs(self.cfg.call_timeout_secs);
        let outcome = tokio::select! {
            r = tokio::time::timeout(timeout, lease.peer.call_tool(params)) => r,
            _ = cancel.cancelled() => {
                tracing::debug!(server = %self.name, tool, tool_call_id = call_id, "MCP tool call cancelled; notifying server");
                anyhow::bail!("aborted")
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Err(_) => {
                // 会话层把这次工具失败上报为 tool_timeout issue（hub.rs report_event），这里只记诊断日志
                tracing::warn!(issue = "typed", server = %self.name, tool, tool_call_id = call_id, timeout_secs = self.cfg.call_timeout_secs, "MCP tool call timed out");
                anyhow::bail!("MCP tool '{tool}' on server '{}' timed out after {}s (callTimeoutSecs)", self.name, self.cfg.call_timeout_secs)
            }
            Ok(Ok(result)) => {
                let value = serde_json::to_value(&result)?;
                let is_error = value.get("isError").and_then(Value::as_bool).unwrap_or(false);
                tracing::debug!(server = %self.name, tool, tool_call_id = call_id, elapsed_ms, is_error, "MCP tool call finished");
                Ok(value)
            }
            Ok(Err(ServiceError::McpError(e))) => {
                tracing::debug!(server = %self.name, tool, tool_call_id = call_id, elapsed_ms, code = e.code.0, "MCP tool call returned a JSON-RPC error");
                anyhow::bail!("MCP server '{}' rejected tool '{tool}': {} (code {})", self.name, e.message, e.code.0)
            }
            Ok(Err(e @ (ServiceError::TransportClosed | ServiceError::TransportSend(_)))) => {
                // 进程退出 / 连接失败：等一下退出状态，给出明确的错误
                let status = match lease.exit {
                    Some(mut exit) => tokio::time::timeout(Duration::from_secs(1), exit.wait_for(Option::is_some)).await.ok().and_then(|r| r.ok().and_then(|s| s.clone())),
                    None => None,
                };
                let reason = match (status, &e) {
                    (Some(status), _) => format!("exited ({status})"),
                    (None, ServiceError::TransportClosed) => "connection closed".to_string(),
                    (None, ServiceError::TransportSend(t)) => format!("connection failed ({})", t.error),
                    (None, e) => format!("connection failed ({e})"),
                };
                let stderr = lease.stderr.as_deref().map(|s| tail(s, STDERR_IN_ERROR)).unwrap_or_default();
                let message = self.crashed(lease.generation, &reason, &stderr);
                // 让下一次调用重连
                if let Ok(mut slot) = self.slot.try_lock() {
                    if slot.as_ref().is_some_and(|c| c.generation == lease.generation) {
                        *slot = None;
                    }
                }
                anyhow::bail!(message)
            }
            Ok(Err(e)) => anyhow::bail!("MCP tool '{tool}' on server '{}' failed: {e}", self.name),
        }
    }

    async fn shutdown(&self) {
        let conn = self.slot.lock().await.take();
        if let Some(mut conn) = conn {
            {
                let mut st = self.state.lock().unwrap();
                st.live = 0;
                if st.status == Status::Ready {
                    st.status = Status::Idle;
                }
            }
            let _ = conn.service.close_with_timeout(Duration::from_secs(1)).await;
            if let Some(mut child) = conn.child.take() {
                let mut exit = child.exit.clone();
                child.closing.store(true, Ordering::SeqCst);
                drop(child.kill.take());
                let _ = tokio::time::timeout(KILL_GRACE + Duration::from_secs(1), exit.wait_for(Option::is_some)).await;
            }
            tracing::debug!(server = %self.name, "MCP server stopped");
        }
    }

    fn status_json(&self, schemas: bool) -> Value {
        let st = self.state.lock().unwrap();
        let tools: Vec<Value> = st
            .tools
            .iter()
            .map(|t| {
                let mut v = json!({"name": t.exposed, "mcpName": t.mcp_name, "description": t.description});
                if schemas {
                    v["inputSchema"] = t.schema.clone();
                }
                v
            })
            .collect();
        json!({
            "name": self.name,
            "transport": self.cfg.transport(),
            "status": st.status,
            "required": self.cfg.required,
            "serverName": st.server_info.as_ref().map(|i| i.0.clone()),
            "serverVersion": st.server_info.as_ref().map(|i| i.1.clone()),
            "connectMs": st.connect_ms,
            "connectedAt": st.connected_at,
            "restarts": st.restarts,
            "lastError": st.last_error.as_deref().map(|e| self.issues.redact(e)),
            "toolCount": tools.len(),
            "tools": tools,
        })
    }
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// 向子进程所在的进程组发信号（子进程以 process_group(0) 启动，pgid = pid）。
fn signal_group(pid: Option<u32>, signal: Signal) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0) {
        let sig = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY: kill(2) 只接收整数参数；负 pid 表示进程组。进程组已不存在时返回 ESRCH，忽略。
        unsafe {
            libc::kill(-pid, sig);
        }
    }
    #[cfg(not(unix))]
    let _ = (pid, signal);
}

/// MCP 的 HTTP 客户端：共享构建器（rustls + ring + webpki-roots，与其它出站请求一致，不依赖系统 OpenSSL），不跟随重定向（避免请求头被带到别的主机）。
fn http_client() -> Result<reqwest::Client, String> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let client = agent_ai::http::client_builder().connect_timeout(Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build().map_err(|e| format!("HTTP client setup failed: {e}"))?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

/// rmcp 客户端回调：工具列表变化 ⇒ 刷新；服务器日志 ⇒ debug。
#[derive(Clone)]
struct Handler {
    server: Weak<Server>,
    generation: u64,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        // 走 initialize 握手（ServiceExt::serve），所以用支持 initialize 的最新协议版本
        ClientConfig::new(ClientCapabilities::default(), Implementation::new("sub-bot", env!("CARGO_PKG_VERSION"))).with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
    }

    fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) -> impl std::future::Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        let (server, generation) = (self.server.clone(), self.generation);
        async move {
            if let Some(server) = server.upgrade() {
                tracing::debug!(server = %server.name, "MCP notifications/tools/list_changed");
                // 不在通知处理里等待请求，避免阻塞服务循环
                tokio::spawn(async move { server.refresh_tools(generation, context.peer).await });
            }
        }
    }
}

/// 所有 MCP 服务器 + profile → 服务器的映射。
pub struct McpManager {
    servers: BTreeMap<String, Arc<Server>>,
    profiles: BTreeMap<String, Vec<ProfileMcpServer>>,
}

impl McpManager {
    /// 服务进程用：所有被某个 profile 引用的服务器（未被引用的全局定义不启动）。
    pub fn new(cfg: &crate::config::Config, issues: Arc<Reporter>) -> anyhow::Result<Self> {
        let plan = config::plan(&cfg.mcp_servers, &cfg.profiles)?;
        let used: HashSet<&str> = plan.profiles.values().flatten().map(ProfileMcpServer::name).collect();
        let servers = plan.servers.iter().filter(|(n, _)| used.contains(n.as_str())).map(|(n, s)| (n.clone(), Server::new(n.clone(), s.clone(), issues.clone()))).collect();
        Ok(Self { servers, profiles: plan.profiles })
    }

    /// `sub-bot mcp list`：指定 profile 时只有它引用的服务器，否则所有定义（全局 + 内联）。
    pub fn for_cli(cfg: &crate::config::Config, profile: Option<&str>, issues: Arc<Reporter>) -> anyhow::Result<Self> {
        let plan = config::plan(&cfg.mcp_servers, &cfg.profiles)?;
        let selected: Option<Vec<&str>> = match profile {
            Some(p) => {
                anyhow::ensure!(cfg.profiles.contains_key(p), "unknown profile '{p}'");
                Some(plan.profiles.get(p).map(|l| l.iter().map(ProfileMcpServer::name).collect()).unwrap_or_default())
            }
            None => None,
        };
        let servers =
            plan.servers.iter().filter(|(n, _)| selected.as_ref().is_none_or(|s| s.contains(&n.as_str()))).map(|(n, s)| (n.clone(), Server::new(n.clone(), s.clone(), issues.clone()))).collect();
        Ok(Self { servers, profiles: plan.profiles })
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// 并行连接所有启用的服务器。`required` 服务器失败 ⇒ Err（其它失败已记录 WARN + issue）。
    pub async fn start(&self) -> Result<(), McpStartupError> {
        if self.servers.is_empty() {
            return Ok(());
        }
        tracing::info!(servers = ?self.servers.keys().collect::<Vec<_>>(), "starting MCP servers");
        let results = futures::future::join_all(self.servers.values().map(|s| async move { (s, s.start().await) })).await;
        let mut fatal = None;
        for (server, result) in results {
            if let Err(reason) = result {
                if server.cfg.required && fatal.is_none() {
                    fatal = Some(McpStartupError { server: server.name.clone(), reason: server.issues.redact(&reason) });
                }
            }
        }
        match fatal {
            Some(e) => {
                tracing::error!(issue = "typed", server = %e.server, "required MCP server failed to start");
                self.shutdown().await;
                Err(e)
            }
            None => Ok(()),
        }
    }

    pub async fn shutdown(&self) {
        futures::future::join_all(self.servers.values().map(|s| s.shutdown())).await;
    }

    /// profile 引用的服务器名（按配置顺序）。
    pub fn profile_servers(&self, profile: &str) -> Vec<String> {
        self.profiles.get(profile).map(|l| l.iter().map(|e| e.name().to_string()).collect()).unwrap_or_default()
    }

    /// profile 当前可用的 MCP 工具（服务器 allow-list 与 profile 收窄之后）。
    fn profile_tools(&self, profile: &str) -> Vec<(Arc<Server>, ToolInfo)> {
        let mut out = vec![];
        for entry in self.profiles.get(profile).into_iter().flatten() {
            let Some(server) = self.servers.get(entry.name()) else { continue };
            match server.status() {
                Status::Ready => {}
                Status::Disabled => continue,
                _ => {
                    server.reconnect_in_background();
                    continue;
                }
            }
            for t in server.tools().iter() {
                if entry.narrow().is_none_or(|n| n.iter().any(|p| config::glob(p, &t.mcp_name))) {
                    out.push((server.clone(), t.clone()));
                }
            }
        }
        out
    }

    /// 暴露给模型的 MCP 工具名（GET /v1/profiles）。
    pub fn profile_tool_names(&self, profile: &str) -> Vec<String> {
        self.profile_tools(profile).into_iter().map(|(_, t)| t.exposed).collect()
    }

    /// 会话的 MCP 工具。`requested`：调用方的 `tools` 里以 `mcp__` 开头的名字（支持 `*`）；空 ⇒ profile 的全部 MCP 工具。
    /// `images`：会话 provider 的工具结果是否支持图片。
    pub fn session_tools(&self, profile: &str, requested: &[String], images: bool) -> anyhow::Result<Vec<Arc<dyn AgentTool>>> {
        let servers = self.profile_servers(profile);
        for r in requested {
            let known = r.contains('*') || servers.iter().any(|s| r.starts_with(&format!("mcp__{s}__")));
            anyhow::ensure!(known, "tool '{r}' is not enabled in profile '{profile}'");
        }
        Ok(self
            .profile_tools(profile)
            .into_iter()
            .filter(|(_, t)| requested.is_empty() || requested.iter().any(|r| config::glob(r, &t.exposed)))
            .map(|(server, info)| Arc::new(McpTool { server, info, images }) as Arc<dyn AgentTool>)
            .collect())
    }

    /// 状态（GET /v1/mcp、`sub-bot mcp list --json`）。`only`：只列这些服务器。
    pub fn status(&self, only: Option<&HashSet<String>>, schemas: bool) -> Vec<Value> {
        self.servers.values().filter(|s| only.is_none_or(|o| o.contains(&s.name))).map(|s| s.status_json(schemas)).collect()
    }
}

/// `GET /v1/mcp`：调用方可用的 profile 引用的 MCP 服务器（状态、工具名、最近错误；不含命令行、URL、请求头或环境变量）。
pub async fn http_status(axum::extract::State(hub): axum::extract::State<Arc<Hub>>, axum::extract::Extension(c): axum::extract::Extension<Arc<Client>>) -> axum::Json<Value> {
    let visible: HashSet<String> = c.profiles.iter().flat_map(|p| hub.mcp.profile_servers(p)).collect();
    let servers = hub.mcp.status(Some(&visible), false);
    let profiles: serde_json::Map<String, Value> = c.profiles.iter().filter(|p| !hub.mcp.profile_servers(p).is_empty()).map(|p| (p.clone(), json!(hub.mcp.profile_servers(p)))).collect();
    axum::Json(json!({"servers": servers, "profiles": profiles}))
}

/// `sub-bot mcp list [--profile p] [--json]`：按配置连接，打印服务器与工具后退出。任一服务器失败 ⇒ 非零退出码。
pub async fn list_cli(cfg: &crate::config::Config, profile: Option<&str>, json_out: bool) -> anyhow::Result<()> {
    let manager = McpManager::for_cli(cfg, profile, Reporter::noop())?;
    if manager.is_empty() {
        if json_out {
            println!("[]");
        } else {
            eprintln!("no MCP servers configured{}", profile.map(|p| format!(" for profile '{p}'")).unwrap_or_default());
        }
        return Ok(());
    }
    let _ = futures::future::join_all(manager.servers.values().map(|s| s.start())).await;
    let status = manager.status(None, json_out);
    manager.shutdown().await;
    let failed: Vec<String> = status.iter().filter(|s| s["status"] == "failed").map(|s| s["name"].as_str().unwrap_or_default().to_string()).collect();
    if json_out {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        for s in &status {
            let server = match (s["serverName"].as_str(), s["serverVersion"].as_str()) {
                (Some(n), Some(v)) => format!("{n} {v}"),
                _ => "-".into(),
            };
            let ms = s["connectMs"].as_u64().map(|m| format!("{m} ms")).unwrap_or_else(|| "-".into());
            println!(
                "{:<20} {:<6} {:<9} {:<32} {:>3} tools  {ms}",
                s["name"].as_str().unwrap_or_default(),
                s["transport"].as_str().unwrap_or_default(),
                s["status"].as_str().unwrap_or_default(),
                server,
                s["toolCount"]
            );
            if let Some(e) = s["lastError"].as_str() {
                for line in e.lines() {
                    println!("    ! {line}");
                }
            }
            for t in s["tools"].as_array().into_iter().flatten() {
                let desc: String = t["description"].as_str().unwrap_or_default().lines().next().unwrap_or_default().chars().take(80).collect();
                println!("    {:<48} {desc}", t["name"].as_str().unwrap_or_default());
            }
        }
    }
    anyhow::ensure!(failed.is_empty(), "MCP server(s) failed to connect: {}", failed.join(", "));
    Ok(())
}
