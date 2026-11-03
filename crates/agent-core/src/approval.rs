//! 工具调用审批（tool approval）：每次执行工具之前按策略判定 allow / ask / deny。
//! - allow：直接执行；
//! - deny：不执行，给模型返回错误工具结果 `tool call denied by policy: <reason>`，并发 `approval_resolved{by:"policy"}`；
//! - ask：暂停这次工具调用，发 `approval_required`，等待调用方 [`ApprovalGate::resolve`]、超时或 abort。
//!
//! 策略本身（配置形状、glob、bash 规则）在 `agent_tools::approval`；这里只负责运行时：挂起表、等待与事件。
//! 并行工具调用各自独立等待，调用方可以按任意顺序决定。
use crate::{agent_loop::Emit, event::AgentEvent};
use agent_ai::ToolCall;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// 策略对一次工具调用的判定。`reason` 说明命中了哪条规则（进入事件、日志和给模型的错误文本）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow { reason: String },
    Ask { reason: String },
    Deny { reason: String },
}

impl Verdict {
    /// deny > ask > allow
    pub fn rank(&self) -> u8 {
        match self {
            Self::Allow { .. } => 0,
            Self::Ask { .. } => 1,
            Self::Deny { .. } => 2,
        }
    }
    pub fn reason(&self) -> &str {
        match self {
            Self::Allow { reason } | Self::Ask { reason } | Self::Deny { reason } => reason,
        }
    }
}

/// 审批策略：纯函数，按工具名与完整参数判定。
pub trait ApprovalPolicy: Send + Sync {
    fn evaluate(&self, tool: &str, args: &Value) -> Verdict;
}

impl<F: Fn(&str, &Value) -> Verdict + Send + Sync> ApprovalPolicy for F {
    fn evaluate(&self, tool: &str, args: &Value) -> Verdict {
        self(tool, args)
    }
}

/// `approval_resolved.decision`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
    Denied,
    /// 超时；是否执行见 `approved`（onTimeout）
    Timeout,
    /// 等待期间 run 被 abort / 超时取消 / 会话删除
    Cancelled,
}

/// `approval_resolved.by`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovedBy {
    Caller,
    Policy,
    Timeout,
    Abort,
}

/// 等待中的审批（`pendingApprovals` / `GET /v1/sessions/{id}/approvals` 的元素）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApproval {
    pub call_id: String,
    pub tool: String,
    pub args: Value,
    pub reason: String,
    /// Unix 毫秒
    pub requested_at: u64,
    /// Unix 毫秒
    pub expires_at: u64,
    pub timeout_secs: u64,
}

/// [`ApprovalGate::resolve`] 的失败原因（HTTP 404 / 409）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    NotFound,
    AlreadyResolved,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "approval not found",
            Self::AlreadyResolved => "approval already resolved",
        })
    }
}

impl std::error::Error for ResolveError {}

struct CallerDecision {
    approve: bool,
    reason: Option<String>,
}

struct Entry {
    info: PendingApproval,
    tx: oneshot::Sender<CallerDecision>,
}

#[derive(Default)]
struct GateState {
    pending: BTreeMap<String, Entry>,
    /// 最近已决定的 callId（区分 409 与 404），有界
    resolved: VecDeque<String>,
}

const RESOLVED_MEMORY: usize = 1024;
/// 调用方 reason 的最大字符数
pub const MAX_REASON_CHARS: usize = 500;
const LOG_ARGS_CHARS: usize = 500;

/// 会话级审批闸门：策略 + 挂起表。放在 [`crate::LoopConfig::approval`]，调用方通过同一个 Arc 决定。
pub struct ApprovalGate {
    policy: Arc<dyn ApprovalPolicy>,
    timeout: Duration,
    approve_on_timeout: bool,
    state: Mutex<GateState>,
}

impl std::fmt::Debug for ApprovalGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalGate").field("timeout", &self.timeout).field("approve_on_timeout", &self.approve_on_timeout).finish_non_exhaustive()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

impl ApprovalGate {
    pub fn new(policy: Arc<dyn ApprovalPolicy>, timeout: Duration, approve_on_timeout: bool) -> Arc<Self> {
        Arc::new(Self { policy, timeout, approve_on_timeout, state: Mutex::new(GateState::default()) })
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// 只判定、不等待（用于测试与诊断）。
    pub fn evaluate(&self, tool: &str, args: &Value) -> Verdict {
        self.policy.evaluate(tool, args)
    }

    /// 等待中的审批，按请求时间排序。
    pub fn pending(&self) -> Vec<PendingApproval> {
        let mut list: Vec<_> = self.state.lock().unwrap().pending.values().map(|e| e.info.clone()).collect();
        list.sort_by(|a, b| a.requested_at.cmp(&b.requested_at).then_with(|| a.call_id.cmp(&b.call_id)));
        list
    }

    /// 调用方决定一次审批；未知 callId ⇒ NotFound，已决定（含超时/取消）⇒ AlreadyResolved。
    pub fn resolve(&self, call_id: &str, approve: bool, reason: Option<String>) -> Result<PendingApproval, ResolveError> {
        let reason = reason.map(|r| truncate(r.trim(), MAX_REASON_CHARS)).filter(|r| !r.is_empty());
        let entry = {
            let mut state = self.state.lock().unwrap();
            match state.pending.remove(call_id) {
                Some(entry) => {
                    remember(&mut state, call_id);
                    entry
                }
                None if state.resolved.iter().any(|id| id == call_id) => return Err(ResolveError::AlreadyResolved),
                None => return Err(ResolveError::NotFound),
            }
        };
        let info = entry.info.clone();
        // 等待方已经因取消/超时离开时发送失败；状态已在锁内移除，结果仍以等待方为准。
        let _ = entry.tx.send(CallerDecision { approve, reason });
        Ok(info)
    }

    /// 执行前检查。Ok ⇒ 执行工具；Err(text) ⇒ 不执行，text 作为错误工具结果给模型。
    pub async fn check(&self, call: &ToolCall, cancel: &CancellationToken, emit: &Emit) -> Result<(), String> {
        let tool = call.name.clone();
        let call_id = call.id.clone();
        let reason = match self.policy.evaluate(&call.name, &call.arguments) {
            Verdict::Allow { reason } => {
                tracing::debug!(reason = %reason, "tool call allowed by policy");
                return Ok(());
            }
            Verdict::Deny { reason } => {
                tracing::info!(decision = "denied", by = "policy", reason = %reason, "tool call denied by policy");
                emit(AgentEvent::ApprovalResolved { call_id, tool, decision: ApprovalDecision::Denied, approved: false, reason: reason.clone(), by: ApprovedBy::Policy, wait_ms: 0 });
                return Err(format!("tool call denied by policy: {reason}"));
            }
            Verdict::Ask { reason } => reason,
        };
        let timeout_secs = self.timeout.as_secs();
        let requested_at = agent_ai::now_ms().max(0) as u64;
        let info = PendingApproval {
            call_id: call_id.clone(),
            tool: tool.clone(),
            args: call.arguments.clone(),
            reason: reason.clone(),
            requested_at,
            expires_at: requested_at + self.timeout.as_millis() as u64,
            timeout_secs,
        };
        let (tx, mut rx) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap();
            if state.pending.contains_key(&call_id) {
                drop(state);
                let reason = "duplicate pending tool call id".to_string();
                tracing::warn!(issue = "typed", reason = %reason, "tool call denied");
                emit(AgentEvent::ApprovalResolved { call_id, tool, decision: ApprovalDecision::Denied, approved: false, reason: reason.clone(), by: ApprovedBy::Policy, wait_ms: 0 });
                return Err(format!("tool call denied by policy: {reason}"));
            }
            state.pending.insert(call_id.clone(), Entry { info: info.clone(), tx });
        }
        tracing::info!(reason = %reason, timeout_secs, "approval required");
        tracing::debug!(args = %truncate(&call.arguments.to_string(), LOG_ARGS_CHARS), "approval args");
        emit(AgentEvent::ApprovalRequired {
            call_id: call_id.clone(),
            tool: tool.clone(),
            args: call.arguments.clone(),
            reason: reason.clone(),
            requested_at,
            expires_at: info.expires_at,
            timeout_secs,
        });

        let started = std::time::Instant::now();
        enum Outcome {
            Caller(CallerDecision),
            Timeout,
            Cancelled,
        }
        let outcome = tokio::select! {
            biased;
            d = &mut rx => match d {
                Ok(d) => Outcome::Caller(d),
                Err(_) => Outcome::Cancelled,
            },
            _ = cancel.cancelled() => Outcome::Cancelled,
            _ = tokio::time::sleep(self.timeout) => Outcome::Timeout,
        };
        // 超时/取消与调用方决定竞争：已被调用方取走 ⇒ 以调用方决定为准。
        let outcome = match outcome {
            Outcome::Caller(d) => Outcome::Caller(d),
            other => {
                let removed = {
                    let mut state = self.state.lock().unwrap();
                    let removed = state.pending.remove(&call_id).is_some();
                    if removed {
                        remember(&mut state, &call_id);
                    }
                    removed
                };
                if removed {
                    other
                } else {
                    match rx.await {
                        Ok(d) => Outcome::Caller(d),
                        Err(_) => other,
                    }
                }
            }
        };
        let wait_ms = started.elapsed().as_millis() as u64;
        let (decision, approved, reason, by, result) = match outcome {
            Outcome::Caller(CallerDecision { approve: true, reason }) => (ApprovalDecision::Approved, true, reason.unwrap_or_default(), ApprovedBy::Caller, Ok(())),
            Outcome::Caller(CallerDecision { approve: false, reason }) => {
                let reason = reason.unwrap_or_else(|| "no reason given".into());
                let text = format!("tool call denied by user: {reason}");
                (ApprovalDecision::Denied, false, reason, ApprovedBy::Caller, Err(text))
            }
            Outcome::Timeout => {
                let reason = format!("approval timed out after {timeout_secs}s");
                tracing::warn!(issue = "typed", timeout_secs, approved = self.approve_on_timeout, "approval timed out");
                let result = if self.approve_on_timeout { Ok(()) } else { Err(format!("tool call denied: {reason}")) };
                (ApprovalDecision::Timeout, self.approve_on_timeout, reason, ApprovedBy::Timeout, result)
            }
            Outcome::Cancelled => (ApprovalDecision::Cancelled, false, "run aborted while waiting for approval".to_string(), ApprovedBy::Abort, Err("aborted while waiting for approval".to_string())),
        };
        tracing::info!(decision = ?decision, by = ?by, approved, wait_ms, "approval resolved");
        emit(AgentEvent::ApprovalResolved { call_id, tool, decision, approved, reason, by, wait_ms });
        result
    }
}

fn remember(state: &mut GateState, call_id: &str) {
    if state.resolved.len() >= RESOLVED_MEMORY {
        state.resolved.pop_front();
    }
    state.resolved.push_back(call_id.to_string());
}
