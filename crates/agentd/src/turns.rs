//! 回合预算（maxTurns）的服务侧接入：会话级调整 / 停止、HTTP `POST /v1/sessions/{id}/turns`、
//! 最终结果字段（turnsUsed / maxTurns / stopReason / canContinue）、异步任务的 `task.turn_budget_*` webhook 转发，
//! 以及 CLI `sub-bot turns`。只有调用方（主 agent）能调整预算；模型没有任何延长回合的工具。
use crate::{
    config::Client,
    hub::{Hub, SessionHandle, WebhookQueue},
};
use agent_core::{Adjusted, AgentEvent, NothingPending, TurnBudgetChangedBy, TurnBudgetView, TurnChange, TurnPause};
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// webhook 事件名（`webhookEvents: "turn_budget"` 或 `"all"` 订阅）。
pub const WEBHOOK_LOW: &str = "task.turn_budget_low";
pub const WEBHOOK_EXHAUSTED: &str = "task.turn_budget_exhausted";
pub const WEBHOOK_EVENTS: [&str; 2] = [WEBHOOK_LOW, WEBHOOK_EXHAUSTED];

/// 一次调整请求：`{"maxTurns":N}` | `{"addTurns":N}` | `{"stop":true}`（恰好一个）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnsOp {
    Change(TurnChange),
    Stop,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnsBody {
    /// 绝对值（>= 1）
    pub max_turns: Option<u64>,
    /// 相对值（可为负；结果钳制到 >= 1）
    pub add_turns: Option<i64>,
    /// 停止暂停在回合上限的 run（没有暂停 ⇒ 409）
    pub stop: Option<bool>,
}

impl TurnsBody {
    pub fn op(&self) -> Result<TurnsOp, String> {
        match (self.max_turns, self.add_turns, self.stop) {
            (Some(0), None, None) => Err("maxTurns must be >= 1".into()),
            (Some(n), None, None) => Ok(TurnsOp::Change(TurnChange::Set(n))),
            (None, Some(n), None) => Ok(TurnsOp::Change(TurnChange::Add(n))),
            (None, None, Some(true)) => Ok(TurnsOp::Stop),
            (None, None, Some(false)) => Err("stop must be true".into()),
            _ => Err("body must contain exactly one of maxTurns, addTurns, stop".into()),
        }
    }
}

impl SessionHandle {
    /// 当前回合上限（会话视图 `maxTurns`）。
    pub fn max_turns(&self) -> u64 {
        self.agent.config.max_turns.get()
    }

    /// 会话视图 `turnBudget`：上限、处理方式、运行状态与暂停信息。
    pub fn turn_budget(&self) -> TurnBudgetView {
        self.agent.config.turn_gate.view(&self.agent.config.max_turns)
    }

    /// 调用方修改回合上限：立即作用于正在进行的 run（下一次模型调用前读取），并保留给会话之后的 run（只在内存里）。
    /// 发 `turn_budget_changed`；run 暂停在上限且新上限 > turnsUsed 时继续。
    pub fn adjust_turns(&self, change: TurnChange) -> Adjusted {
        let config = &self.agent.config;
        config.turn_gate.adjust_with(&config.max_turns, change, |a| {
            tracing::info!(session_id = %self.id, max_turns = a.max_turns, previous = a.previous, turns_used = a.turns_used, running = a.running, change = ?change, by = "caller", "turn budget changed");
            self.agent.emit(AgentEvent::TurnBudgetChanged { max_turns: a.max_turns, previous: a.previous, turns_used: a.turns_used, by: TurnBudgetChangedBy::Caller });
        })
    }

    /// 停止暂停在回合上限的 run（它以 max_turns 结束）；没有暂停 ⇒ [`NothingPending`]。
    pub fn stop_turns(&self) -> Result<TurnPause, NothingPending> {
        let result = self.agent.config.turn_gate.stop();
        match &result {
            Ok(p) => tracing::info!(session_id = %self.id, turns_used = p.turns_used, max_turns = p.max_turns, by = "caller", "turn budget wait stopped by caller"),
            Err(e) => tracing::debug!(session_id = %self.id, error = %e, "turn budget stop rejected"),
        }
        result
    }

    /// 执行一次调整 / 停止，返回 HTTP / RPC / CLI 共用的响应主体。
    pub fn apply_turns(&self, op: TurnsOp) -> Result<Value, NothingPending> {
        let a = match op {
            TurnsOp::Change(change) => self.adjust_turns(change),
            TurnsOp::Stop => {
                let pause = self.stop_turns()?;
                let max = self.max_turns();
                Adjusted { max_turns: max, previous: max, turns_used: pause.turns_used, running: true, paused: false }
            }
        };
        Ok(json!({"sessionId": self.id, "maxTurns": a.max_turns, "previous": a.previous, "turnsUsed": a.turns_used, "running": a.running, "paused": a.paused}))
    }

    /// 最终结果的回合字段：总是带 sessionId / turnsUsed / maxTurns；因上限结束时另加 stopReason:"max_turns" 与 canContinue:true，
    /// 并在错误文本后附上如何继续。`limit_reached` 由调用方根据本次收集到的 `run_outcome: max_turns` 决定。
    pub fn turn_result_fields(&self, limit_reached: bool, error: Option<&str>) -> Map<String, Value> {
        let view = self.turn_budget();
        let turns_used = if view.running { view.turns_used } else { view.last_run.map(|r| r.turns_used).unwrap_or(0) };
        let mut out = Map::new();
        out.insert("sessionId".into(), json!(self.id));
        out.insert("turnsUsed".into(), json!(turns_used));
        out.insert("maxTurns".into(), json!(view.max_turns));
        if limit_reached {
            out.insert("stopReason".into(), json!("max_turns"));
            out.insert("canContinue".into(), json!(true));
            if let Some(e) = error {
                out.insert("error".into(), json!(continue_hint(e, &self.id)));
            }
        }
        out
    }
}

/// agent-core 的通用错误文本 + 继续方式。
pub fn continue_hint(error: &str, session_id: &str) -> String {
    format!("{error}; continue by sending another prompt with sessionId {session_id}")
}

fn error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(json!({"error": message.to_string()}))).into_response()
}

/// `POST /v1/sessions/{id}/turns`：200 响应 `{sessionId, maxTurns, previous, turnsUsed, running, paused}`；
/// 非法请求体 ⇒ 400；会话不存在 / 非 owner ⇒ 404；stop 但没有暂停 ⇒ 409。
pub async fn post(State(hub): State<Arc<Hub>>, Extension(c): Extension<Arc<Client>>, Path(id): Path<String>, crate::http::ApiJson(body): crate::http::ApiJson<TurnsBody>) -> Response {
    let op = match body.op() {
        Ok(op) => op,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let h = match crate::http::load_session(&hub, &c, &id).await {
        Ok(h) => h,
        Err(e) => return e.into_response(),
    };
    match h.apply_turns(op) {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}

/// webhook / CLI 使用的 turn_budget_low / turn_budget_exhausted 载荷。
pub fn payload(task_id: Option<&str>, session_id: &str, event: &AgentEvent) -> Option<(&'static str, Value)> {
    let (name, mut v) = match event {
        AgentEvent::TurnBudgetLow { turns_used, max_turns, remaining } => (WEBHOOK_LOW, json!({"turnsUsed": turns_used, "maxTurns": max_turns, "remaining": remaining})),
        AgentEvent::TurnBudgetExhausted { turns_used, max_turns, action, timeout_secs, expires_at } => {
            let mut v = json!({"turnsUsed": turns_used, "maxTurns": max_turns, "action": action});
            if let Some(t) = timeout_secs {
                v["timeoutSecs"] = json!(t);
            }
            if let Some(t) = expires_at {
                v["expiresAt"] = json!(t);
            }
            (WEBHOOK_EXHAUSTED, v)
        }
        _ => return None,
    };
    v["sessionId"] = json!(session_id);
    v["turnsUrl"] = json!(format!("/v1/sessions/{session_id}/turns"));
    if let Some(id) = task_id {
        v["taskId"] = json!(id);
    }
    Some((name, v))
}

/// 异步任务：把 turn_budget_low / turn_budget_exhausted 投递给订阅了对应事件的 webhook，直到会话 settled。
pub fn forward_to_webhooks(task_id: String, session: Arc<SessionHandle>, mut rx: tokio::sync::broadcast::Receiver<AgentEvent>, queues: Vec<(Vec<&'static str>, WebhookQueue)>) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(AgentEvent::AgentSettled { .. }) => break,
                Ok(event) => {
                    if let Some((name, data)) = payload(Some(&task_id), &session.id, &event) {
                        for (wanted, queue) in &queues {
                            if wanted.contains(&name) {
                                let _ = queue.send((name, data.clone()));
                            }
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => session.report_lag(n),
                Err(_) => break,
            }
        }
    });
}

/// `sub-bot turns <sessionId> (--set N | --add N | --stop)`：连接参数同 `ask`。
#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("op").required(true).args(["set", "add", "stop"])))]
pub struct Turns {
    #[command(flatten)]
    pub conn: crate::ask::Connection,
    /// 会话 id（UUID）
    pub session_id: String,
    /// 把回合上限设为 N（>= 1）
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub set: Option<u64>,
    /// 回合上限加 N（可为负，结果至少 1）
    #[arg(long, allow_hyphen_values = true)]
    pub add: Option<i64>,
    /// 停止暂停在回合上限的 run
    #[arg(long)]
    pub stop: bool,
}

impl Turns {
    pub fn body(&self) -> Value {
        match (self.set, self.add) {
            (Some(n), _) => json!({"maxTurns": n}),
            (_, Some(n)) => json!({"addTurns": n}),
            _ => json!({"stop": true}),
        }
    }
}

pub async fn run(args: &Turns, data_dir: std::path::PathBuf) -> anyhow::Result<()> {
    let sid = uuid::Uuid::parse_str(&args.session_id).map_err(|_| anyhow::anyhow!("invalid session id '{}' (expected a UUID)", args.session_id))?;
    let client = args.conn.client(data_dir)?;
    let response = client.post_turns(&sid.to_string(), &args.body()).await?;
    println!("{response}");
    Ok(())
}
