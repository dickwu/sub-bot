//! 工具调用审批的服务侧接入：按 profile / 会话策略构造 [`ApprovalGate`]，HTTP 端点
//! `GET /v1/sessions/{id}/approvals`、`POST /v1/sessions/{id}/approvals/{callId}`，RPC `approve` 的共用逻辑，
//! 以及异步任务的 `task.approval_required` webhook 转发。
use crate::{
    config::Client,
    hub::{Hub, SessionHandle, WebhookQueue},
};
use agent_core::{AgentEvent, ApprovalGate, PendingApproval, ResolveError};
use agent_tools::{ApprovalStack, ToolApproval};
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

/// webhook 事件名（`webhookEvents: "approval_required"` 或 `"all"` 订阅）。
pub const WEBHOOK_EVENT: &str = "task.approval_required";

/// profile 策略 + 会话策略（只能收紧）⇒ 审批闸门；两者都不会 ask/deny 时返回 None（不审批，零开销）。
pub fn gate(profile: ToolApproval, session: Option<ToolApproval>) -> Option<Arc<ApprovalGate>> {
    let stack = ApprovalStack::new(profile, session);
    if !stack.is_active() {
        return None;
    }
    let (timeout, approve_on_timeout) = (stack.timeout_secs(), stack.approve_on_timeout());
    Some(ApprovalGate::new(Arc::new(stack), Duration::from_secs(timeout), approve_on_timeout))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Approve,
    Deny,
}

impl Decision {
    pub fn approve(self) -> bool {
        self == Self::Approve
    }
}

impl SessionHandle {
    /// 等待中的审批（会话视图 / get_state 的 `pendingApprovals`）。
    pub fn pending_approvals(&self) -> Vec<PendingApproval> {
        self.agent.config.approval.as_ref().map(|g| g.pending()).unwrap_or_default()
    }

    /// 调用方（HTTP / RPC / CLI）决定一次审批。会话没有启用审批时等同未知 callId。
    pub fn resolve_approval(&self, call_id: &str, decision: Decision, reason: Option<String>) -> Result<PendingApproval, ResolveError> {
        let gate = self.agent.config.approval.as_ref().ok_or(ResolveError::NotFound)?;
        let result = gate.resolve(call_id, decision.approve(), reason);
        match &result {
            Ok(p) => tracing::info!(session_id = %self.id, tool_call_id = %call_id, tool = %p.tool, decision = ?decision, "approval decision accepted"),
            Err(e) => tracing::debug!(session_id = %self.id, tool_call_id = %call_id, error = %e, "approval decision rejected"),
        }
        result
    }
}

fn error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(json!({"error": message.to_string()}))).into_response()
}

pub async fn list(State(hub): State<Arc<Hub>>, Extension(c): Extension<Arc<Client>>, Path(id): Path<String>) -> Response {
    match hub.get(&c, &id) {
        Some(h) => Json(json!({"approvals": h.pending_approvals()})).into_response(),
        None => error(StatusCode::NOT_FOUND, "session not found"),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionBody {
    pub decision: Decision,
    #[serde(default)]
    pub reason: Option<String>,
}

/// 200 `{resolved:true}`；会话不存在/非 owner、callId 未知 ⇒ 404；已决定（含超时/取消）⇒ 409。
pub async fn decide(
    State(hub): State<Arc<Hub>>,
    Extension(c): Extension<Arc<Client>>,
    Path((id, call_id)): Path<(String, String)>,
    crate::http::ApiJson(body): crate::http::ApiJson<DecisionBody>,
) -> Response {
    let Some(h) = hub.get(&c, &id) else { return error(StatusCode::NOT_FOUND, "session not found") };
    match h.resolve_approval(&call_id, body.decision, body.reason) {
        Ok(p) => Json(json!({"resolved": true, "callId": p.call_id, "tool": p.tool, "decision": if body.decision.approve() { "approved" } else { "denied" }})).into_response(),
        Err(e @ ResolveError::NotFound) => error(StatusCode::NOT_FOUND, e),
        Err(e @ ResolveError::AlreadyResolved) => error(StatusCode::CONFLICT, e),
    }
}

/// webhook / CLI 使用的 approval_required 载荷。
pub fn payload(task_id: Option<&str>, session_id: &str, event: &AgentEvent) -> Option<Value> {
    let AgentEvent::ApprovalRequired { call_id, tool, args, reason, requested_at, expires_at, timeout_secs } = event else { return None };
    let mut v = json!({
        "sessionId": session_id, "callId": call_id, "tool": tool, "args": args, "reason": reason,
        "requestedAt": requested_at, "expiresAt": expires_at, "timeoutSecs": timeout_secs,
        "approvalUrl": format!("/v1/sessions/{session_id}/approvals/{call_id}"),
    });
    if let Some(id) = task_id {
        v["taskId"] = json!(id);
    }
    Some(v)
}

/// 异步任务：把 approval_required 投递给订阅了 `task.approval_required` 的 webhook，直到会话 settled。
pub fn forward_to_webhooks(task_id: String, session: Arc<SessionHandle>, mut rx: tokio::sync::broadcast::Receiver<AgentEvent>, queues: Vec<WebhookQueue>) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(AgentEvent::AgentSettled { .. }) => break,
                Ok(event) => {
                    if let Some(data) = payload(Some(&task_id), &session.id, &event) {
                        for queue in &queues {
                            let _ = queue.send((WEBHOOK_EVENT, data.clone()));
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => session.report_lag(n),
                Err(_) => break,
            }
        }
    });
}

/// 交互式审批是否可用（CLI `ask` / `run`）：stderr 是终端且能打开 `/dev/tty`（stdin 被管道占用时也从 /dev/tty 读答复）。
pub fn tty_available() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal() && std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").is_ok()
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}… [truncated]", s.chars().take(max).collect::<String>())
    }
}

/// 终端里展示的一次审批摘要：工具、原因、参数（bash 显示命令原文；其它工具显示 JSON，截断到 2000 字符）。
pub fn summary(tool: &str, call_id: &str, args: &Value, reason: &str) -> String {
    let body = match args.get("command").and_then(Value::as_str) {
        Some(cmd) if tool == "bash" => format!("  command: {}", clip(cmd, 2000)),
        _ => format!("  args: {}", clip(&serde_json::to_string_pretty(args).unwrap_or_default(), 2000)),
    };
    format!("[approval] {tool} (callId {call_id}): {reason}\n{body}")
}

/// 在 /dev/tty 上打印摘要并询问 `Approve? [y/N]`；只有 y / yes 视为批准。阻塞调用，请放在 spawn_blocking 里。
pub fn prompt_tty(summary: &str) -> std::io::Result<bool> {
    use std::io::{BufRead, Write};
    let mut tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    write!(tty, "\n{summary}\nApprove? [y/N] ")?;
    tty.flush()?;
    let mut line = String::new();
    std::io::BufReader::new(tty).read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// URL 路径段编码（callId 来自 provider，通常是 [A-Za-z0-9_-]）。
pub fn path_segment(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}
