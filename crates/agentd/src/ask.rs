//! Small HTTP client; no provider, Hub, or server-side secrets are initialized by `ask`.
use clap::Args;
use serde_json::{json, Value};
use std::path::PathBuf;

/// 连接参数（`ask` 与 `turns` 共用）。
#[derive(Args)]
pub struct Connection {
    #[arg(long, conflicts_with = "url")]
    pub socket: Option<PathBuf>,
    #[arg(long)]
    pub url: Option<String>,
    #[arg(long)]
    pub token_file: Option<PathBuf>,
}

impl Connection {
    /// token：--token-file > SUB_BOT_TOKEN；默认优先使用 `<dataDir>/sub-bot.sock`（存在时），否则 http://127.0.0.1:7878。
    pub fn client(&self, data_dir: PathBuf) -> anyhow::Result<Client> {
        let token = match &self.token_file {
            Some(path) => Some(std::fs::read_to_string(path).map_err(|_| anyhow::anyhow!("cannot read token file"))?),
            None => std::env::var("SUB_BOT_TOKEN").ok(),
        };
        let default_socket = data_dir.join("sub-bot.sock");
        if self.socket.is_none() && self.url.is_none() {
            crate::config::prepare_temp_data_dir(&default_socket, false)?;
        }
        let socket = self.socket.clone().or_else(|| (self.url.is_none() && default_socket.exists()).then_some(default_socket));
        Client::new(socket, self.url.clone(), token.as_deref())
    }
}

#[derive(Args)]
pub struct Ask {
    #[command(flatten)]
    pub conn: Connection,
    #[arg(long)]
    pub profile: Option<String>,
    /// Continue a server session (also after a server restart: the server reloads it from its session directory).
    #[arg(long, visible_alias = "resume")]
    pub session: Option<String>,
    /// Submit an asynchronous task and poll until its final result.
    #[arg(long = "async")]
    pub asynchronous: bool,
    /// Maximum seconds to wait for the request and final result (including async polling).
    #[arg(long, default_value_t = 3600)]
    pub timeout: u64,
    /// Emit session metadata then NDJSON events in sync mode (ending with a `session_state` line carrying
    /// reasoningEffort/requestedReasoningEffort/effortAdjusted/allowedEfforts/routing); with --async, print the final task JSON.
    #[arg(long)]
    pub json: bool,
    /// Use the profile's deep model (same as --depth deep).
    #[arg(long, conflicts_with = "depth")]
    pub deep: bool,
    /// Model routing for a new session (or one without prompts yet): standard | deep | auto.
    #[arg(long)]
    pub depth: Option<crate::routing::Depth>,
    /// Account for a new session (session.account); must be allowed by the profile's allowedAccounts (HTTP 403 otherwise).
    #[command(flatten)]
    pub account: crate::AccountArg,
    /// Approve every tool call that the server's toolApproval policy asks about (deny rules still apply).
    #[arg(long, conflicts_with = "auto_deny")]
    pub auto_approve: bool,
    /// Deny every tool call that the server's toolApproval policy asks about.
    #[arg(long)]
    pub auto_deny: bool,
    /// Turn budget (model calls per run, >= 1, no cap): session.maxTurns for a new session; with --session it is applied
    /// to the existing session first (POST /v1/sessions/{id}/turns).
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub max_turns: Option<u64>,
    /// What to do when the turn budget runs out (new sessions only): stop | ask | summarize.
    #[arg(long)]
    pub on_max_turns: Option<agent_core::OnMaxTurns>,
    /// When the run pauses at the turn limit (onMaxTurns=ask), add N turns automatically (POST addTurns) and continue.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub auto_extend_turns: Option<u64>,
    /// Message, or - to read stdin.
    pub message: String,
}

/// 收到 `approval_required` 时 ask 怎么做。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ApprovalMode {
    /// 不处理（--json：事件原样输出，由调用方决定）
    #[default]
    Ignore,
    /// 非交互：在 stderr 提示如何批准，保持等待（超时按服务端 onTimeout）
    Notify,
    /// --auto-approve（true）/ --auto-deny（false）
    Auto(bool),
    /// 交互终端：在 /dev/tty 上询问 `Approve? [y/N]`
    Prompt,
}

pub struct Client {
    socket: Option<PathBuf>,
    url: String,
    token: Option<axum::http::HeaderValue>,
    http: reqwest::Client,
    provider: Option<String>,
    /// 粘性推理强度（/v1/chat 顶层 reasoningEffort）
    effort: Option<agent_ai::ThinkingLevel>,
    depth: Option<crate::routing::Depth>,
    /// 新会话的账号（session.account）
    account: Option<String>,
    approval: ApprovalMode,
    /// 新会话的 session.maxTurns / session.onMaxTurns
    max_turns: Option<u64>,
    on_max_turns: Option<agent_core::OnMaxTurns>,
    /// --auto-extend-turns：暂停在回合上限时自动 addTurns
    auto_extend_turns: Option<u64>,
    /// 暂停在回合上限时在 stderr 提示如何延长（--json 时不提示）
    turn_hints: bool,
}

/// 因回合上限结束（stopReason max_turns）：stderr 已提示如何继续，退出码非零。
#[derive(Debug)]
pub struct MaxTurnsReached;

impl std::fmt::Display for MaxTurnsReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request stopped at max_turns")
    }
}

impl std::error::Error for MaxTurnsReached {}

/// 文本模式的继续提示（stderr）。
pub fn max_turns_line(turns_used: Option<u64>, max_turns: Option<u64>, session_id: &str, continue_cmd: &str) -> String {
    let n = turns_used.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
    let m = max_turns.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
    format!("max_turns reached ({n}/{m}). Continue: {continue_cmd} {session_id} \"<message>\" (add --max-turns N to raise the budget)")
}

impl Client {
    pub fn new(socket: Option<PathBuf>, url: Option<String>, token: Option<&str>) -> anyhow::Result<Self> {
        if let Some(path) = &socket {
            crate::config::prepare_temp_data_dir(path, false)?;
        }
        let token = token
            .map(|token| {
                let mut header = axum::http::HeaderValue::from_str(&format!("Bearer {}", token.trim())).map_err(|_| anyhow::anyhow!("invalid token"))?;
                header.set_sensitive(true);
                anyhow::Ok(header)
            })
            .transpose()?;
        let url = url.unwrap_or_else(|| "http://127.0.0.1:7878".into());
        let parsed = reqwest::Url::parse(&url).map_err(|_| anyhow::anyhow!("invalid server URL"))?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https")
                && parsed.host_str().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "invalid server URL"
        );
        // No redirects or proxies: do not leak local credentials to another endpoint.
        let http = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build()?;
        Ok(Self {
            socket,
            url,
            token,
            http,
            provider: None,
            effort: None,
            depth: None,
            account: None,
            approval: ApprovalMode::Ignore,
            max_turns: None,
            on_max_turns: None,
            auto_extend_turns: None,
            turn_hints: false,
        })
    }

    async fn response(
        &self,
        method: axum::http::Method,
        path: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<(axum::http::StatusCode, axum::http::HeaderMap, axum::body::Body, tokio::task::JoinSet<()>)> {
        #[cfg(unix)]
        if let Some(socket) = &self.socket {
            use http_body_util::Full;
            let stream = tokio::net::UnixStream::connect(socket).await.map_err(|_| anyhow::anyhow!("cannot connect to Unix socket"))?;
            let (mut sender, connection) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
            let mut connections = tokio::task::JoinSet::new();
            connections.spawn(async move {
                let _ = connection.await;
            });
            let mut request = hyper::Request::builder().method(method).uri(path).header("host", "localhost").header("content-type", "application/json");
            if let Some(token) = &self.token {
                request = request.header("authorization", token);
            }
            let body = axum::body::Bytes::from(body.map(Value::to_string).unwrap_or_default());
            let response = sender.send_request(request.body(Full::new(body))?).await?;
            let (parts, body) = response.into_parts();
            return Ok((parts.status, parts.headers, axum::body::Body::new(body), connections));
        }
        let mut request = self.http.request(method, format!("{}{path}", self.url.trim_end_matches('/')));
        if let Some(token) = &self.token {
            request = request.header("authorization", token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|_| anyhow::anyhow!("HTTP request failed"))?;
        let status = response.status();
        let headers = response.headers().clone();
        Ok((status, headers, axum::body::Body::from_stream(response.bytes_stream()), tokio::task::JoinSet::new()))
    }

    pub async fn request(&self, method: axum::http::Method, path: &str, body: Option<&Value>) -> anyhow::Result<(axum::http::StatusCode, Value)> {
        use http_body_util::BodyExt;
        let (status, _, body, _connections) = self.response(method, path, body).await?;
        let bytes = body.collect().await?.to_bytes();
        let value = serde_json::from_slice(&bytes).map_err(|_| if status.is_success() { anyhow::anyhow!("invalid server JSON") } else { http_error(status, None) })?;
        Ok((status, value))
    }

    fn chat_body(&self, message: &str, profile: Option<&str>, session: Option<&str>, extra: Value) -> Value {
        let mut body = json!({"message": message, "session": {"profile": profile, "provider": self.provider}, "sessionId": session});
        if let Some(account) = &self.account {
            body["session"]["account"] = json!(account);
        }
        if let Some(effort) = self.effort {
            body["reasoningEffort"] = json!(effort);
        }
        if let Some(depth) = self.depth {
            body["depth"] = json!(depth);
        }
        // 已有会话的 maxTurns 走 turns 端点（见 run）；onMaxTurns 只对新会话有效
        if session.is_none() {
            if let Some(n) = self.max_turns {
                body["session"]["maxTurns"] = json!(n);
            }
            if let Some(mode) = self.on_max_turns {
                body["session"]["onMaxTurns"] = json!(mode);
            }
        }
        body.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
        body
    }

    /// `POST /v1/sessions/{id}/turns`；非 2xx ⇒ 错误（含服务端错误文本）。
    pub async fn post_turns(&self, session_id: &str, body: &Value) -> anyhow::Result<Value> {
        let (status, v) = self.request(axum::http::Method::POST, &format!("/v1/sessions/{session_id}/turns"), Some(body)).await?;
        if !status.is_success() {
            return Err(http_error(status, Some(&v)));
        }
        Ok(v)
    }

    /// run 暂停在回合上限（turn_budget_exhausted action=ask，或会话视图 turnBudget.pause）：
    /// --auto-extend-turns N ⇒ POST addTurns N；否则在 stderr 提示如何延长 / 停止。失败只在 stderr 提示。
    async fn handle_turn_pause(&self, session_id: Option<&str>, event: &Value) {
        let Some(sid) = session_id.and_then(|id| uuid::Uuid::parse_str(id).ok()) else { return };
        let (used, max, timeout) = (&event["turnsUsed"], &event["maxTurns"], &event["timeoutSecs"]);
        match self.auto_extend_turns {
            Some(n) => match self.post_turns(&sid.to_string(), &json!({"addTurns": n})).await {
                Ok(v) => eprintln!("[turns] max_turns reached ({used}/{max}); added {n} turns by --auto-extend-turns (maxTurns now {})", v["maxTurns"]),
                Err(e) => eprintln!("[turns] cannot extend the turn budget: {e}"),
            },
            None if self.turn_hints => eprintln!(
                "[turns] run paused at max_turns ({used}/{max}); waiting up to {timeout}s. Extend: sub-bot turns {sid} --add N (or stop: sub-bot turns {sid} --stop), or rerun with --auto-extend-turns N"
            ),
            None => {}
        }
    }

    /// 按 [`ApprovalMode`] 处理一次 approval_required（SSE 事件或 GET approvals 列表项，字段相同）。失败只在 stderr 提示。
    async fn handle_approval(&self, session_id: Option<&str>, event: &Value) {
        let (Some(call_id), Some(tool)) = (event["callId"].as_str(), event["tool"].as_str()) else { return };
        let reason = event["reason"].as_str().unwrap_or_default();
        let sid = session_id.and_then(|id| uuid::Uuid::parse_str(id).ok());
        let (approve, why) = match self.approval {
            ApprovalMode::Ignore => return,
            ApprovalMode::Notify => {
                match sid {
                    Some(sid) => eprintln!(
                        "[approval] {tool} (callId {call_id}) is waiting for approval: {reason}\n  approve with POST /v1/sessions/{sid}/approvals/{} {{\"decision\":\"approve\"}} or rerun with --auto-approve",
                        crate::approvals::path_segment(call_id)
                    ),
                    None => eprintln!("[approval] {tool} (callId {call_id}) is waiting for approval: {reason}"),
                }
                return;
            }
            ApprovalMode::Auto(approve) => (approve, if approve { "approved by --auto-approve" } else { "denied by --auto-deny" }),
            ApprovalMode::Prompt => {
                let text = crate::approvals::summary(tool, call_id, &event["args"], reason);
                let approve = matches!(tokio::task::spawn_blocking(move || crate::approvals::prompt_tty(&text)).await, Ok(Ok(true)));
                (approve, if approve { "approved at terminal" } else { "denied at terminal" })
            }
        };
        let Some(sid) = sid else {
            eprintln!("[approval] cannot answer callId {call_id}: unknown session id");
            return;
        };
        let path = format!("/v1/sessions/{sid}/approvals/{}", crate::approvals::path_segment(call_id));
        let body = json!({"decision": if approve { "approve" } else { "deny" }, "reason": why});
        match self.request(axum::http::Method::POST, &path, Some(&body)).await {
            Ok((status, _)) if status.is_success() => {}
            Ok((axum::http::StatusCode::CONFLICT, _)) => eprintln!("[approval] callId {call_id} was already resolved (timed out or aborted)"),
            Ok((status, v)) => eprintln!("[approval] {}", http_error(status, Some(&v))),
            Err(e) => eprintln!("[approval] {e}"),
        }
    }

    pub async fn ask_stream(
        &self,
        message: &str,
        profile: Option<&str>,
        session: Option<&str>,
        timeout_secs: u64,
        mut on_event: impl FnMut(&str, &Value) -> anyhow::Result<()>,
    ) -> anyhow::Result<StreamOutcome> {
        use http_body_util::BodyExt;
        let request = async {
            let body = self.chat_body(message, profile, session, json!({"stream": true}));
            let (status, headers, mut body, _connections) = self.response(axum::http::Method::POST, "/v1/chat", Some(&body)).await?;
            if !status.is_success() {
                let bytes = axum::body::to_bytes(body, 64 * 1024).await.unwrap_or_default();
                return Err(http_error(status, serde_json::from_slice::<Value>(&bytes).ok().as_ref()));
            }
            let mut session_id = session.map(str::to_owned);
            if let Some(id) = headers.get("x-session-id").and_then(|value| value.to_str().ok()) {
                session_id = Some(id.to_owned());
                let event = json!({"type": "session", "sessionId": id});
                on_event(&event.to_string(), &event)?;
            }
            let mut parser = SseParser::default();
            let mut outcome = StreamOutcome::default();
            let mut approvals = vec![];
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|_| anyhow::anyhow!("cannot read server event stream"))?;
                if let Some(bytes) = frame.data_ref() {
                    parser.feed(bytes, &mut |data| {
                        let event: Value = serde_json::from_str(data).map_err(|_| anyhow::anyhow!("invalid server event JSON"))?;
                        anyhow::ensure!(event.is_object(), "invalid server event JSON");
                        on_event(data, &event)?;
                        outcome.event(&event);
                        if event["type"] == "approval_required" || (event["type"] == "turn_budget_exhausted" && event["action"] == "ask") {
                            approvals.push(event);
                        }
                        Ok(())
                    })?;
                    for event in approvals.drain(..) {
                        if event["type"] == "turn_budget_exhausted" {
                            self.handle_turn_pause(session_id.as_deref(), &event).await;
                        } else {
                            self.handle_approval(session_id.as_deref(), &event).await;
                        }
                    }
                    if outcome.settled {
                        return outcome.finish();
                    }
                }
            }
            outcome.finish()
        };
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), request)
            .await
            .map_err(|_| anyhow::anyhow!("ask timed out after {timeout_secs} seconds; server-side work may still be running"))?
    }

    pub async fn ask_async(&self, message: &str, profile: Option<&str>, session: Option<&str>, timeout_secs: u64) -> anyhow::Result<Value> {
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), self.ask_async_inner(message, profile, session))
            .await
            .map_err(|_| anyhow::anyhow!("ask timed out after {timeout_secs} seconds; server-side work may still be running"))?
    }

    async fn ask_async_inner(&self, message: &str, profile: Option<&str>, session: Option<&str>) -> anyhow::Result<Value> {
        let body = self.chat_body(message, profile, session, json!({"async": true}));
        let (status, response) = self.request(axum::http::Method::POST, "/v1/chat", Some(&body)).await?;
        if !status.is_success() {
            return Err(http_error(status, Some(&response)));
        }
        let id = response["taskId"].as_str().ok_or_else(|| anyhow::anyhow!("missing taskId"))?;
        let id = uuid::Uuid::parse_str(id).map_err(|_| anyhow::anyhow!("invalid taskId"))?;
        let path = format!("/v1/tasks/{id}");
        let session_id = response["sessionId"].as_str().and_then(|s| uuid::Uuid::parse_str(s).ok()).map(|s| s.to_string());
        let mut handled = std::collections::HashSet::new();
        let mut paused = None;
        loop {
            let (status, task) = self.request(axum::http::Method::GET, &path, None).await?;
            if !status.is_success() {
                return Err(http_error(status, Some(&task)));
            }
            if task["status"] != "running" {
                return Ok(task);
            }
            // 异步模式没有事件流：轮询会话的等待审批
            if let (false, Some(sid)) = (self.approval == ApprovalMode::Ignore, session_id.as_deref()) {
                if let Ok((status, list)) = self.request(axum::http::Method::GET, &format!("/v1/sessions/{sid}/approvals"), None).await {
                    for item in list["approvals"].as_array().filter(|_| status.is_success()).into_iter().flatten() {
                        if let Some(call_id) = item["callId"].as_str() {
                            if handled.insert(call_id.to_owned()) {
                                self.handle_approval(Some(sid), item).await;
                            }
                        }
                    }
                }
            }
            // 同样轮询回合预算暂停（每次暂停只处理一次）
            if let (true, Some(sid)) = (self.auto_extend_turns.is_some() || self.turn_hints, session_id.as_deref()) {
                if let Ok((status, view)) = self.request(axum::http::Method::GET, &format!("/v1/sessions/{sid}"), None).await {
                    let pause = &view["turnBudget"]["pause"];
                    if status.is_success() && pause.is_object() && paused.as_ref() != Some(pause) {
                        paused = Some(pause.clone());
                        self.handle_turn_pause(Some(sid), pause).await;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

fn http_error(status: axum::http::StatusCode, body: Option<&Value>) -> anyhow::Error {
    match body.and_then(|v| v["error"].as_str()) {
        Some(message) => anyhow::anyhow!("server returned HTTP {status}: {message}"),
        None => anyhow::anyhow!("server returned HTTP {status}"),
    }
}

// Decode lines as bytes so chunk boundaries may split UTF-8 and CRLF alike.
#[derive(Default)]
struct SseParser {
    line: Vec<u8>,
    data: Vec<String>,
    after_cr: bool,
}

impl SseParser {
    fn feed(&mut self, bytes: &[u8], on_event: &mut impl FnMut(&str) -> anyhow::Result<()>) -> anyhow::Result<()> {
        for &byte in bytes {
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if matches!(byte, b'\r' | b'\n') {
                let line = std::str::from_utf8(&self.line).map_err(|_| anyhow::anyhow!("invalid UTF-8 in server event stream"))?;
                if line.is_empty() {
                    if !self.data.is_empty() {
                        let data = self.data.join("\n");
                        self.data.clear();
                        on_event(&data)?;
                    }
                } else {
                    let (field, value) = line.split_once(':').unwrap_or((line, ""));
                    if field == "data" {
                        self.data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
                    }
                    // Ignore event/id/retry fields and keep-alive comments.
                }
                self.line.clear();
            } else {
                self.line.push(byte);
            }
        }
        Ok(())
    }
}

fn text_delta(event: &Value) -> Option<&str> {
    let update = &event["assistantMessageEvent"];
    (event["type"] == "message_update" && update["type"] == "text_delta").then(|| update["delta"].as_str()).flatten()
}

#[derive(Default, Debug)]
pub struct StreamOutcome {
    settled: bool,
    error: Option<String>,
    /// 本次 run 因回合上限结束（`run_outcome: max_turns`；每个 agent_start 重置）
    pub max_turns_reached: bool,
    /// 最近一次 turn_budget_exhausted 的 turnsUsed / maxTurns
    pub turns_used: Option<u64>,
    pub max_turns: Option<u64>,
}

impl StreamOutcome {
    fn message(&mut self, message: &Value) {
        if matches!(message["stopReason"].as_str(), Some("error" | "aborted")) || message["errorMessage"].is_string() {
            self.error = Some(message["errorMessage"].as_str().or(message["stopReason"].as_str()).unwrap_or("agent error").to_owned());
        }
    }

    fn event(&mut self, event: &Value) {
        match event["type"].as_str() {
            Some("message_update") => {
                let update = &event["assistantMessageEvent"];
                if update["type"] == "error" {
                    self.error = Some("agent error".into());
                }
                self.message(&update["message"]);
            }
            Some("message_end") => self.message(&event["message"]),
            Some("agent_end") => {
                if let Some(messages) = event["messages"].as_array() {
                    for message in messages {
                        self.message(message);
                    }
                }
            }
            Some("run_outcome") if matches!(event["kind"].as_str(), Some("task_panic" | "run_timeout")) => {
                self.error = Some(format!("agent failed: {}", event["kind"].as_str().unwrap()));
            }
            Some("agent_start") => self.max_turns_reached = false,
            Some("run_outcome") if event["kind"] == "max_turns" => self.max_turns_reached = true,
            Some("turn_budget_exhausted") => {
                self.turns_used = event["turnsUsed"].as_u64();
                self.max_turns = event["maxTurns"].as_u64();
            }
            Some("agent_settled") => {
                self.settled = true;
                if event["aborted"] == true {
                    self.error = Some("request aborted".into());
                }
            }
            // Tool errors are recoverable by the agent; they are not request failures.
            _ => {}
        }
    }

    /// 回合上限结束不在这里报错（调用方打印继续提示后以 [`MaxTurnsReached`] 退出）。
    fn finish(self) -> anyhow::Result<Self> {
        if let (Some(error), false) = (&self.error, self.max_turns_reached) {
            anyhow::bail!("{error}");
        }
        anyhow::ensure!(self.settled, "server event stream ended before agent_settled");
        Ok(self)
    }
}

pub async fn run(args: &Ask, data_dir: PathBuf, provider: Option<&str>, effort: Option<agent_ai::ThinkingLevel>) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut message = args.message.clone();
    if message == "-" {
        message.clear();
        tokio::io::stdin().read_to_string(&mut message).await?;
    }
    anyhow::ensure!(args.on_max_turns.is_none() || args.session.is_none(), "--on-max-turns only applies to new sessions (onMaxTurns is fixed when the session is created)");
    let mut client = args.conn.client(data_dir)?;
    client.max_turns = args.max_turns;
    client.on_max_turns = args.on_max_turns;
    client.auto_extend_turns = args.auto_extend_turns;
    client.turn_hints = !args.json;
    if let (Some(n), Some(id)) = (args.max_turns, args.session.as_deref()) {
        // 已有会话：先通过 turns 端点设置上限（同时作用于正在进行的 run）
        let sid = uuid::Uuid::parse_str(id).map_err(|_| anyhow::anyhow!("invalid session id '{id}' (expected a UUID)"))?;
        let v = client.post_turns(&sid.to_string(), &json!({"maxTurns": n})).await?;
        eprintln!("[turns] session {sid}: maxTurns {} -> {}", v["previous"], v["maxTurns"]);
    }
    client.provider = provider.map(str::to_owned);
    client.effort = effort;
    client.depth = if args.deep { Some(crate::routing::Depth::Deep) } else { args.depth };
    client.account = args.account.account.clone();
    client.approval = if args.auto_approve {
        ApprovalMode::Auto(true)
    } else if args.auto_deny {
        ApprovalMode::Auto(false)
    } else if args.json {
        ApprovalMode::Ignore
    } else if crate::approvals::tty_available() {
        ApprovalMode::Prompt
    } else {
        ApprovalMode::Notify
    };
    if !args.asynchronous {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        let mut session_id = None;
        let result = client
            .ask_stream(&message, args.profile.as_deref(), args.session.as_deref(), args.timeout, |data, event| {
                if event["type"] == "session" {
                    session_id = event["sessionId"].as_str().map(str::to_owned);
                }
                if args.json {
                    // Preserve the server JSON, removing only SSE's physical line breaks for NDJSON.
                    writeln!(stdout, "{}", data.replace(['\r', '\n'], ""))?;
                } else if let Some(delta) = text_delta(event) {
                    write!(stdout, "{delta}")?;
                } else {
                    return Ok(());
                }
                stdout.flush()?;
                Ok(())
            })
            .await;
        let limit = result.as_ref().ok().filter(|o| o.max_turns_reached);
        if !args.json {
            writeln!(stdout)?;
            stdout.flush()?;
        } else if let Some(id) = session_id.as_deref().and_then(|id| uuid::Uuid::parse_str(id).ok()) {
            // 结束时附上会话的推理强度、路由与回合信息（失败不影响退出码）。
            if let Ok((status, mut view)) = client.request(axum::http::Method::GET, &format!("/v1/sessions/{id}"), None).await {
                if status.is_success() && view.is_object() {
                    view["type"] = json!("session_state");
                    view["turnsUsed"] = view["turnBudget"]["lastRun"]["turnsUsed"].as_u64().map(Value::from).unwrap_or(json!(0));
                    if limit.is_some() {
                        view["stopReason"] = json!("max_turns");
                        view["canContinue"] = json!(true);
                    }
                    writeln!(stdout, "{view}")?;
                    stdout.flush()?;
                }
            }
        }
        let outcome = result?;
        if outcome.max_turns_reached {
            let id = session_id.as_deref().unwrap_or("<sessionId>");
            eprintln!("{}", max_turns_line(outcome.turns_used, outcome.max_turns, id, "sub-bot ask --session"));
            return Err(MaxTurnsReached.into());
        }
        return Ok(());
    }
    let response = client.ask_async(&message, args.profile.as_deref(), args.session.as_deref(), args.timeout).await?;
    let result = &response["result"];
    if args.json {
        println!("{response}");
    } else if let Some(text) = result["text"].as_str() {
        println!("{text}");
    }
    if result["stopReason"] == "max_turns" {
        if !args.json {
            let id = result["sessionId"].as_str().or(response["sessionId"].as_str()).unwrap_or("<sessionId>");
            eprintln!("{}", max_turns_line(result["turnsUsed"].as_u64(), result["maxTurns"].as_u64(), id, "sub-bot ask --session"));
        }
        return Err(MaxTurnsReached.into());
    }
    anyhow::ensure!(
        response["status"] == "succeeded"
            && result["error"].is_null()
            && result["aborted"] != true
            && !matches!(result["stopReason"].as_str(), Some("error" | "aborted"))
            && result["text"].is_string(),
        "request did not complete successfully"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        routing::{get, post},
        Json, Router,
    };
    use clap::Parser;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn timeout_cli_default_and_override() {
        for (args, expected) in [(vec!["sub-bot", "ask", "hello"], 3600), (vec!["sub-bot", "ask", "--async", "--timeout", "12", "hello"], 12)] {
            let crate::Cmd::Ask(ask) = crate::Cli::try_parse_from(args).unwrap().cmd else { panic!("expected ask") };
            assert_eq!(ask.timeout, expected);
        }
        assert!(crate::Cli::try_parse_from(["sub-bot", "ask", "--timeout", "-1", "hello"]).is_err());
    }

    #[test]
    fn chat_body_carries_sticky_effort_and_depth() {
        let mut client = Client::new(None, Some("http://127.0.0.1:1".into()), None).unwrap();
        let body = client.chat_body("hi", None, Some("s"), json!({"stream": true}));
        assert!(body.get("reasoningEffort").is_none() && body.get("depth").is_none());
        assert!(body["session"].get("account").is_none());
        client.effort = Some(agent_ai::ThinkingLevel::Max);
        client.depth = Some(crate::routing::Depth::Deep);
        client.account = Some("work".into());
        let body = client.chat_body("hi", Some("p"), None, json!({"async": true}));
        assert_eq!(body["session"]["account"], "work");
        assert_eq!((body["reasoningEffort"].clone(), body["depth"].clone(), body["async"].clone(), body["session"]["profile"].clone()), (json!("max"), json!("deep"), json!(true), json!("p")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sse_tcp_and_unix_incremental_success_and_error() {
        use axum::body::{Body, Bytes};
        use std::convert::Infallible;
        for unix in [false, true] {
            for fail in [false, true] {
                let received = Arc::new(tokio::sync::Notify::new());
                let gate = received.clone();
                let delta = json!({"type":"message_update", "assistantMessageEvent":{"type":"text_delta", "contentIndex":0, "delta":"你好"}}).to_string();
                // Multi-line data, comments, CRLF, and UTF-8 split across individual body chunks.
                let first = format!(": keep-alive\r\nevent: message_update\r\ndata: {}\r\ndata: {}\r\n\r\n", &delta[..1], &delta[1..]);
                let app = Router::new().route("/v1/chat", post(move |Json(body): Json<Value>| {
                    assert_eq!(body["stream"], true);
                    assert_ne!(body["async"], true);
                    let first = first.clone();
                    let gate = gate.clone();
                    async move {
                        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(1);
                        tokio::spawn(async move {
                            for byte in first.bytes() {
                                if tx.send(Ok(Bytes::from(vec![byte]))).await.is_err() {
                                    return;
                                }
                            }
                            // The rest of the response cannot arrive until the client exposes the first delta.
                            gate.notified().await;
                            let mut rest = String::from("data: {\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\" world\"}}\n\n");
                            rest.push_str("data: {\"type\":\"tool_execution_end\",\"toolCallId\":\"t\",\"toolName\":\"bash\",\"isError\":true,\"result\":{}}\n\n");
                            if fail {
                                rest.push_str("data: {\"type\":\"message_update\",\"assistantMessageEvent\":{\"type\":\"error\",\"message\":{\"stopReason\":\"error\",\"errorMessage\":\"fake provider failed\"}}}\n\n");
                            }
                            rest.push_str("data: {\"type\":\"agent_settled\",\"aborted\":false}\n\n");
                            let _ = tx.send(Ok(Bytes::from(rest))).await;
                        });
                        axum::http::Response::builder().header("content-type", "text/event-stream").header("x-session-id", "test-session").body(Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))).unwrap()
                    }
                }));
                let mut servers = tokio::task::JoinSet::new();
                let path = std::env::temp_dir().join(format!("sb-{}.sock", &uuid::Uuid::new_v4().simple().to_string()[..8]));
                let client = if unix {
                    let listener = tokio::net::UnixListener::bind(&path).unwrap();
                    servers.spawn(async move { axum::serve(listener, app).await.unwrap() });
                    Client::new(Some(path.clone()), None, None).unwrap()
                } else {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let address = listener.local_addr().unwrap();
                    servers.spawn(async move { axum::serve(listener, app).await.unwrap() });
                    Client::new(None, Some(format!("http://{address}")), None).unwrap()
                };
                let mut text = String::new();
                let mut events = Vec::new();
                let result = client
                    .ask_stream("hello", None, None, 5, |raw, event| {
                        events.push(raw.to_owned());
                        if let Some(delta) = text_delta(event) {
                            text.push_str(delta);
                            received.notify_one();
                        }
                        Ok(())
                    })
                    .await;
                assert_eq!(text, "你好 world");
                assert_eq!(serde_json::from_str::<Value>(&events[0]).unwrap(), json!({"type":"session", "sessionId":"test-session"}));
                assert_eq!(events[1], format!("{}\n{}", &delta[..1], &delta[1..]));
                assert_eq!(serde_json::from_str::<Value>(events.last().unwrap()).unwrap(), json!({"type":"agent_settled", "aborted":false}));
                if fail {
                    assert_eq!(result.unwrap_err().to_string(), "fake provider failed");
                } else {
                    result.unwrap();
                }
                if unix {
                    std::fs::remove_file(path).unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn server_json_error_is_included_for_sync_and_async() {
        let app = Router::new().route("/v1/chat", post(|| async { (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "unknown profile 'missing'"}))) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(None, Some(format!("http://{}", listener.local_addr().unwrap())), None).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let expected = "server returned HTTP 400 Bad Request: unknown profile 'missing'";
        assert_eq!(client.ask_stream("hi", Some("missing"), None, 5, |_, _| Ok(())).await.unwrap_err().to_string(), expected);
        assert_eq!(client.ask_async("hi", Some("missing"), None, 5).await.unwrap_err().to_string(), expected);
        server.abort();
    }

    #[tokio::test]
    async fn sync_timeout_and_http_error() {
        use axum::body::{Body, Bytes};
        for http_error in [false, true] {
            let app = Router::new().route(
                "/v1/chat",
                post(move || async move {
                    if http_error {
                        axum::http::Response::builder().status(403).body(Body::from("not JSON")).unwrap()
                    } else {
                        let initial = futures::stream::once(async { Ok::<_, std::convert::Infallible>(Bytes::from_static(b": keep-alive\n\n")) });
                        let stream = futures::StreamExt::chain(initial, futures::stream::pending());
                        axum::http::Response::new(Body::from_stream(stream))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let mut servers = tokio::task::JoinSet::new();
            servers.spawn(async move { axum::serve(listener, app).await.unwrap() });
            let client = Client::new(None, Some(format!("http://{address}")), None).unwrap();
            let error = client.ask_stream("hello", None, None, 1, |_, _| Ok(())).await.unwrap_err().to_string();
            if http_error {
                assert_eq!(error, "server returned HTTP 403 Forbidden");
            } else {
                assert_eq!(error, "ask timed out after 1 seconds; server-side work may still be running");
            }
        }
    }

    #[test]
    fn stream_failures_and_settled() {
        for event in [
            json!({"type":"agent_settled","aborted":true}),
            json!({"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"missing done"}}),
            json!({"type":"run_outcome","kind":"task_panic"}),
            json!({"type":"run_outcome","kind":"run_timeout"}),
        ] {
            let mut outcome = StreamOutcome::default();
            outcome.event(&event);
            outcome.event(&json!({"type":"agent_settled","aborted":false}));
            assert!(outcome.finish().is_err());
        }
        assert!(StreamOutcome::default().finish().unwrap_err().to_string().contains("before agent_settled"));
    }

    #[tokio::test]
    async fn timeout_bounds_polling_and_stalled_requests() {
        // Cover repeated running responses, a hung poll, and a hung initial POST.
        for (hang_post, hang_poll) in [(false, false), (false, true), (true, false)] {
            let polls = Arc::new(AtomicUsize::new(0));
            let count = polls.clone();
            let app = Router::new()
                .route(
                    "/v1/chat",
                    post(move || async move {
                        if hang_post {
                            std::future::pending::<()>().await;
                        }
                        Json(json!({"taskId": uuid::Uuid::new_v4(), "status": "running"}))
                    }),
                )
                .route(
                    "/v1/tasks/{id}",
                    get(move || {
                        let count = count.clone();
                        async move {
                            count.fetch_add(1, Ordering::SeqCst);
                            if hang_poll {
                                std::future::pending::<()>().await;
                            }
                            Json(json!({"status": "running"}))
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let mut servers = tokio::task::JoinSet::new();
            servers.spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let client = Client::new(None, Some(format!("http://{address}")), None).unwrap();
            let error = tokio::time::timeout(std::time::Duration::from_secs(5), client.ask_async("hello", None, None, 1)).await.expect("ask did not enforce its deadline").unwrap_err();
            assert_eq!(error.to_string(), "ask timed out after 1 seconds; server-side work may still be running");
            if !hang_post {
                assert!(polls.load(Ordering::SeqCst) > 0);
            }
        }
    }

    #[test]
    fn approval_flags_conflict() {
        let crate::Cmd::Ask(ask) = crate::Cli::try_parse_from(["sub-bot", "ask", "--auto-approve", "hi"]).unwrap().cmd else { panic!("expected ask") };
        assert!(ask.auto_approve && !ask.auto_deny);
        assert!(crate::Cli::try_parse_from(["sub-bot", "ask", "--auto-approve", "--auto-deny", "hi"]).is_err());
    }

    /// 流式：收到 approval_required 后按 --auto-approve POST 决定；异步：轮询 approvals 列表并按 --auto-deny 决定。
    #[tokio::test]
    async fn auto_decisions_are_posted_for_stream_and_async() {
        use axum::body::{Body, Bytes};
        use axum::extract::Path;
        use axum::response::IntoResponse;
        let sid = uuid::Uuid::new_v4().to_string();
        let decided = Arc::new(tokio::sync::Mutex::new(Vec::<(String, Value)>::new()));
        let notify = Arc::new(tokio::sync::Notify::new());
        let (d1, n1, d2, s1, seen) = (decided.clone(), notify.clone(), decided.clone(), sid.clone(), decided.clone());
        let app = Router::new()
            .route(
                "/v1/chat",
                post(move |Json(body): Json<Value>| {
                    let (notify, sid) = (n1.clone(), s1.clone());
                    async move {
                        if body["async"] == true {
                            return Json(json!({"taskId": uuid::Uuid::new_v4(), "sessionId": sid, "status": "running"})).into_response();
                        }
                        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(4);
                        tokio::spawn(async move {
                            let ev = json!({"type": "approval_required", "callId": "call/1", "tool": "bash", "args": {"command": "ls"}, "reason": "r", "timeoutSecs": 300});
                            tx.send(Ok(Bytes::from(format!("data: {ev}\n\n")))).await.unwrap();
                            notify.notified().await;
                            tx.send(Ok(Bytes::from_static(b"data: {\"type\":\"agent_settled\",\"aborted\":false}\n\n"))).await.unwrap();
                        });
                        axum::http::Response::builder().header("x-session-id", sid).body(Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))).unwrap()
                    }
                }),
            )
            .route(
                "/v1/sessions/{sid}/approvals/{call}",
                post(move |Path((sid, call)): Path<(String, String)>, Json(body): Json<Value>| {
                    let (decided, notify) = (d1.clone(), notify.clone());
                    async move {
                        decided.lock().await.push((format!("{sid}/{call}"), body));
                        notify.notify_one();
                        Json(json!({"resolved": true}))
                    }
                }),
            )
            .route(
                "/v1/sessions/{sid}/approvals",
                get(move || {
                    let decided = d2.clone();
                    async move {
                        let list = if decided.lock().await.len() < 2 { json!([{"callId": "c2", "tool": "write", "args": {}, "reason": "r"}]) } else { json!([]) };
                        Json(json!({"approvals": list}))
                    }
                }),
            )
            .route(
                "/v1/tasks/{id}",
                get(move || {
                    let decided = decided.clone();
                    async move { Json(json!({"status": if decided.lock().await.len() < 2 { "running" } else { "succeeded" }})) }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = Client::new(None, Some(format!("http://{}", listener.local_addr().unwrap())), None).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        client.approval = ApprovalMode::Auto(true);
        client.ask_stream("hi", None, None, 5, |_, _| Ok(())).await.unwrap();
        client.approval = ApprovalMode::Auto(false);
        let task = client.ask_async("hi", None, None, 5).await.unwrap();
        assert_eq!(task["status"], "succeeded");
        let seen = seen.lock().await.clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        // callId 按路径段编码（含 `/` 也能路由到同一端点，服务端解码回原值）
        assert_eq!(seen[0].0, format!("{sid}/call/1"));
        assert_eq!(seen[0].1, json!({"decision": "approve", "reason": "approved by --auto-approve"}));
        assert_eq!(seen[1].0, format!("{sid}/c2"));
        assert_eq!(seen[1].1, json!({"decision": "deny", "reason": "denied by --auto-deny"}));
        server.abort();
    }
}
