//! 回合预算（maxTurns）：只有调用方（主 agent）能调整，模型没有任何延长回合的工具。
//! - 上限放在 [`crate::LoopConfig::max_turns`]（`Shared<u64>`），每次模型调用前读取；运行期修改立即作用于正在进行的 run；
//! - 达到上限（模型调用前 turnsUsed >= maxTurns）时按 [`OnMaxTurns`]：stop 直接结束；summarize 再给一次不带工具的总结调用；
//!   ask 暂停在模型调用之前，发 `turn_budget_exhausted`，等待调用方提高上限 / stop、超时或 abort（[`TurnGate`]）。
//!
//! 闸门与 [`crate::ApprovalGate`] 同构：挂起项 + oneshot + 超时 + 取消；调整与挂起共用一把锁，避免“刚提高上限、循环才挂起”的竞争。
use crate::{agent_loop::Emit, event::AgentEvent, Shared};
use serde::{Deserialize, Serialize};
use std::{sync::Mutex, time::Duration};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// ask 模式默认等待时间（秒）
pub const DEFAULT_ASK_TIMEOUT_SECS: u64 = 600;

/// 达到 maxTurns 时的处理方式（profile / 会话 `onMaxTurns`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnMaxTurns {
    /// 立即结束（RunOutcome `max_turns`）
    #[default]
    Stop,
    /// 暂停等待调用方：提高上限继续、stop 停止；超时停止
    Ask,
    /// 再给模型一次不带工具的调用，总结进度与后续步骤，然后结束
    Summarize,
}

impl OnMaxTurns {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Ask => "ask",
            Self::Summarize => "summarize",
        }
    }
}

impl std::fmt::Display for OnMaxTurns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for OnMaxTurns {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "stop" => Ok(Self::Stop),
            "ask" => Ok(Self::Ask),
            "summarize" => Ok(Self::Summarize),
            other => Err(format!("invalid onMaxTurns '{other}' (expected stop | ask | summarize)")),
        }
    }
}

/// `turn_budget_resolved.decision`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnBudgetDecision {
    /// 调用方把上限提高到 turnsUsed 之上，run 继续
    Extended,
    /// 调用方 stop
    Stopped,
    /// 等待超时，run 停止
    Timeout,
    /// 等待期间 run 被 abort / 超时取消 / 会话删除
    Cancelled,
}

/// `turn_budget_changed.by`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnBudgetChangedBy {
    /// 调用方（HTTP / RPC / CLI）
    Caller,
    /// 预留：系统在超时时修改预算（当前超时只停止 run，不修改上限）
    Timeout,
}

/// 一次预算调整。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnChange {
    /// 绝对值（>= 1）
    Set(u64),
    /// 相对值，可为负；结果钳制到 >= 1
    Add(i64),
}

impl TurnChange {
    /// 按当前上限计算新上限（>= 1）。
    pub fn apply(self, current: u64) -> u64 {
        match self {
            Self::Set(n) => n.max(1),
            Self::Add(d) => (i128::from(current) + i128::from(d)).clamp(1, i128::from(u64::MAX)) as u64,
        }
    }
}

/// 暂停中的预算等待（会话视图 `turnBudget` 的一部分）。时间为 Unix 毫秒。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnPause {
    pub turns_used: u64,
    pub max_turns: u64,
    pub timeout_secs: u64,
    pub paused_at: u64,
    pub expires_at: u64,
}

/// 调整结果（`POST /v1/sessions/{id}/turns` 的响应主体）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Adjusted {
    pub max_turns: u64,
    pub previous: u64,
    /// 当前 run 已用回合；空闲时 0
    pub turns_used: u64,
    pub running: bool,
    /// 调整之后是否仍在暂停
    pub paused: bool,
}

/// 最近一次 run 的回合统计（最终结果的 turnsUsed / stopReason）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastRun {
    pub turns_used: u64,
    pub max_turns: u64,
    /// 因 maxTurns 结束（stop / summarize / ask 后 stop 或超时）
    pub limit_reached: bool,
}

/// 会话视图里的 `turnBudget`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnBudgetView {
    pub max_turns: u64,
    pub on_max_turns: OnMaxTurns,
    pub ask_timeout_secs: u64,
    pub running: bool,
    pub paused: bool,
    /// 当前 run 已用回合；空闲时 0
    pub turns_used: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause: Option<TurnPause>,
    /// 最近一次结束的 run
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<LastRun>,
}

/// [`TurnGate::stop`] 的失败：没有暂停中的等待（HTTP 409）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NothingPending;

impl std::fmt::Display for NothingPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no run is paused at the turn limit")
    }
}

impl std::error::Error for NothingPending {}

/// ask 等待的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolution {
    Extended(u64),
    Stopped,
    Timeout,
    Cancelled,
}

enum Wake {
    Extended(u64),
    Stop,
}

struct Pending {
    info: TurnPause,
    tx: oneshot::Sender<Wake>,
}

#[derive(Default)]
struct GateState {
    running: bool,
    used: u64,
    pending: Option<Pending>,
    last: Option<LastRun>,
    /// 每次调整 +1：循环据此重新武装 `turn_budget_low`
    changes: u64,
}

/// 会话级回合预算闸门：处理方式、ask 超时与运行期状态。放在 [`crate::LoopConfig::turn_gate`]，调用方通过同一个 Arc 调整 / 停止。
pub struct TurnGate {
    on_max: OnMaxTurns,
    timeout: Duration,
    state: Mutex<GateState>,
}

impl Default for TurnGate {
    fn default() -> Self {
        Self::new(OnMaxTurns::Stop, Duration::from_secs(DEFAULT_ASK_TIMEOUT_SECS))
    }
}

impl std::fmt::Debug for TurnGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnGate").field("on_max", &self.on_max).field("timeout", &self.timeout).finish_non_exhaustive()
    }
}

/// 低预算阈值：ceil(0.8 · maxTurns)
pub fn low_threshold(max_turns: u64) -> u64 {
    max_turns - max_turns / 5
}

impl TurnGate {
    pub fn new(on_max: OnMaxTurns, timeout: Duration) -> Self {
        Self { on_max, timeout, state: Mutex::new(GateState::default()) }
    }

    pub fn on_max(&self) -> OnMaxTurns {
        self.on_max
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// 调整上限（与挂起共用锁）。暂停中且新上限 > turnsUsed ⇒ 唤醒 run 继续。
    pub fn adjust(&self, max_turns: &Shared<u64>, change: TurnChange) -> Adjusted {
        self.adjust_with(max_turns, change, |_| {})
    }

    /// 同 [`Self::adjust`]；`notify` 在唤醒暂停的 run 之前（仍持锁）调用，
    /// 保证 `turn_budget_changed` 事件先于 `turn_budget_resolved` 发出。`notify` 不能再调用本闸门。
    pub fn adjust_with(&self, max_turns: &Shared<u64>, change: TurnChange, notify: impl FnOnce(&Adjusted)) -> Adjusted {
        let mut state = self.state.lock().unwrap();
        let previous = max_turns.get();
        let new = change.apply(previous);
        max_turns.set(new);
        state.changes += 1;
        let wake = state.pending.as_ref().is_some_and(|p| new > p.info.turns_used);
        let adjusted = Adjusted { max_turns: new, previous, turns_used: if state.running { state.used } else { 0 }, running: state.running, paused: state.pending.is_some() && !wake };
        notify(&adjusted);
        if wake {
            let pending = state.pending.take().unwrap();
            // 等待方已因取消/超时离开时发送失败；挂起项已移除，结果以等待方为准。
            let _ = pending.tx.send(Wake::Extended(new));
        }
        adjusted
    }

    /// 停止暂停中的 run（它以 max_turns 结束）；没有暂停 ⇒ [`NothingPending`]。
    pub fn stop(&self) -> Result<TurnPause, NothingPending> {
        let pending = self.state.lock().unwrap().pending.take().ok_or(NothingPending)?;
        let info = pending.info.clone();
        let _ = pending.tx.send(Wake::Stop);
        Ok(info)
    }

    /// 是否暂停在回合上限。
    pub fn paused(&self) -> Option<TurnPause> {
        self.state.lock().unwrap().pending.as_ref().map(|p| p.info.clone())
    }

    /// 最近一次结束的 run（AgentEnd 之前记录）。
    pub fn last_run(&self) -> Option<LastRun> {
        self.state.lock().unwrap().last
    }

    pub fn view(&self, max_turns: &Shared<u64>) -> TurnBudgetView {
        let state = self.state.lock().unwrap();
        TurnBudgetView {
            max_turns: max_turns.get(),
            on_max_turns: self.on_max,
            ask_timeout_secs: self.timeout.as_secs(),
            running: state.running,
            paused: state.pending.is_some(),
            turns_used: if state.running { state.used } else { 0 },
            pause: state.pending.as_ref().map(|p| p.info.clone()),
            last_run: state.last,
        }
    }

    pub(crate) fn begin(&self) -> RunGuard<'_> {
        let mut state = self.state.lock().unwrap();
        state.running = true;
        state.used = 0;
        RunGuard { gate: self }
    }

    pub(crate) fn set_used(&self, used: u64) {
        self.state.lock().unwrap().used = used;
    }

    pub(crate) fn changes(&self) -> u64 {
        self.state.lock().unwrap().changes
    }

    pub(crate) fn finish(&self, last: LastRun) {
        let mut state = self.state.lock().unwrap();
        state.last = Some(last);
        state.running = false;
        state.used = 0;
    }

    /// ask：在模型调用之前暂停，等待调用方 / 超时 / 取消。发 `turn_budget_exhausted` 与 `turn_budget_resolved`。
    pub(crate) async fn wait(&self, max_turns: &Shared<u64>, used: u64, cancel: &CancellationToken, emit: &Emit) -> Resolution {
        let (tx, mut rx) = oneshot::channel();
        let paused_at = agent_ai::now_ms().max(0) as u64;
        let timeout_secs = self.timeout.as_secs();
        let info = {
            let mut state = self.state.lock().unwrap();
            // 与 adjust 同锁：检查之后提高的上限一定能看到挂起项
            let max = max_turns.get();
            if max > used {
                return Resolution::Extended(max);
            }
            let info = TurnPause { turns_used: used, max_turns: max, timeout_secs, paused_at, expires_at: paused_at + self.timeout.as_millis() as u64 };
            state.pending = Some(Pending { info: info.clone(), tx });
            info
        };
        tracing::info!(turns_used = used, max_turns = info.max_turns, action = "ask", timeout_secs, "turn budget exhausted; waiting for the caller to extend or stop");
        emit(AgentEvent::TurnBudgetExhausted { turns_used: used, max_turns: info.max_turns, action: OnMaxTurns::Ask, timeout_secs: Some(timeout_secs), expires_at: Some(info.expires_at) });
        let started = std::time::Instant::now();
        let outcome = tokio::select! {
            biased;
            w = &mut rx => match w {
                Ok(Wake::Extended(n)) => Resolution::Extended(n),
                Ok(Wake::Stop) => Resolution::Stopped,
                Err(_) => Resolution::Cancelled,
            },
            _ = cancel.cancelled() => Resolution::Cancelled,
            _ = tokio::time::sleep(self.timeout) => Resolution::Timeout,
        };
        // 超时/取消与调用方竞争：挂起项已被调用方取走 ⇒ 以调用方为准。
        let outcome = match outcome {
            Resolution::Timeout | Resolution::Cancelled => {
                let removed = self.state.lock().unwrap().pending.take().is_some();
                if removed {
                    outcome
                } else {
                    match rx.await {
                        Ok(Wake::Extended(n)) => Resolution::Extended(n),
                        Ok(Wake::Stop) => Resolution::Stopped,
                        Err(_) => outcome,
                    }
                }
            }
            other => other,
        };
        // 调用方决定之后才发现取消：仍按取消处理（run 马上以 aborted 结束）
        let outcome = if cancel.is_cancelled() { Resolution::Cancelled } else { outcome };
        let wait_ms = started.elapsed().as_millis() as u64;
        let (decision, max) = match outcome {
            Resolution::Extended(n) => (TurnBudgetDecision::Extended, n),
            Resolution::Stopped => (TurnBudgetDecision::Stopped, max_turns.get()),
            Resolution::Timeout => (TurnBudgetDecision::Timeout, max_turns.get()),
            Resolution::Cancelled => (TurnBudgetDecision::Cancelled, max_turns.get()),
        };
        if decision == TurnBudgetDecision::Timeout {
            tracing::warn!(issue = "typed", turns_used = used, max_turns = max, timeout_secs, wait_ms, "turn budget wait timed out; stopping the run");
        } else {
            tracing::info!(decision = ?decision, turns_used = used, max_turns = max, wait_ms, "turn budget wait resolved");
        }
        emit(AgentEvent::TurnBudgetResolved { decision, max_turns: max, wait_ms });
        outcome
    }
}

/// run 结束（含 panic / future 被丢弃）时清除 running；正常结束由 [`TurnGate::finish`] 记录统计。
pub(crate) struct RunGuard<'a> {
    gate: &'a TurnGate,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        state.running = false;
        state.used = 0;
        state.pending = None;
    }
}
