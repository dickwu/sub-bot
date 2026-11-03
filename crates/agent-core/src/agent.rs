use crate::{agent_loop::*, event::AgentEvent};
use agent_ai::Message;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Started,
    QueuedSteer,
    QueuedFollowUp,
}

#[derive(Default)]
struct State {
    cancel: Option<CancellationToken>,
}

/// 有状态 Agent。同一时刻只跑一个 run；提交与结束交接共用 state 锁。
pub struct Agent {
    pub config: LoopConfig,
    messages: Mutex<Vec<Message>>,
    events: broadcast::Sender<AgentEvent>,
    state: Mutex<State>,
}

impl Agent {
    pub fn new(config: LoopConfig, history: Vec<Message>) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Self { config, messages: Mutex::new(history), events, state: Mutex::new(State::default()) })
    }

    /// 请在提交之前订阅，避免漏掉快速完成的事件。
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap().cancel.is_some()
    }

    pub fn submit(self: &Arc<Self>, message: Message, mode: Option<&str>) -> Disposition {
        self.submit_with(message, mode, |_| {})
    }

    /// 原子地启动或排队。on_messages 在每段循环结束后调用，可用于持久化；排队时使用当前 run 的回调。
    pub fn submit_with(self: &Arc<Self>, message: Message, mode: Option<&str>, on_messages: impl Fn(Vec<Message>) + Send + 'static) -> Disposition {
        let mut state = self.state.lock().unwrap();
        if state.cancel.is_some() {
            return if mode == Some("steer") {
                self.config.steering.lock().unwrap().push_back(message);
                Disposition::QueuedSteer
            } else {
                self.config.follow_up.lock().unwrap().push_back(message);
                Disposition::QueuedFollowUp
            };
        }
        let token = CancellationToken::new();
        state.cancel = Some(token.clone());
        let settled = CancellationToken::new();
        let settled_guard = settled.clone().drop_guard();
        if let Some(timeout) = self.config.run_timeout {
            let token = token.clone();
            let settled = settled.clone();
            let this = self.clone();
            tokio::spawn(
                async move {
                    tokio::select! {
                        biased;
                        _ = settled.cancelled() => {}
                        _ = token.cancelled() => {}
                        _ = tokio::time::sleep(timeout) => {
                            // 与 settle 共用 state 锁，避免结束后才报超时。
                            let _state = this.state.lock().unwrap();
                            if !settled.is_cancelled() && !token.is_cancelled() {
                                let _ = this.events.send(AgentEvent::RunOutcome { kind: "run_timeout".into() });
                                tracing::warn!(issue = "typed", "run timed out");
                                token.cancel();
                            }
                        }
                    }
                }
                .in_current_span(),
            );
        }
        let this = self.clone();
        tokio::spawn(
            async move {
                let _settled_guard = settled_guard;
                let tx = this.events.clone();
                let emit: Emit = Arc::new(move |e| {
                    let _ = tx.send(e);
                });
                let outcome = {
                    let this = &this;
                    let token = &token;
                    let settled = &settled;
                    AssertUnwindSafe(async move {
                        let mut pending = vec![message];
                        if let Some(hook) = this.config.before_run.clone() {
                            // 取消时不再等待 hook；run_loop 会立即以 aborted 结束。
                            tokio::select! {
                                biased;
                                _ = token.cancelled() => {}
                                _ = hook(pending.clone(), emit.clone()) => {}
                            }
                        }
                        loop {
                            let out = run_loop(&this.messages, pending, &this.config, token.clone(), emit.clone()).await;
                            on_messages(out);
                            // run_loop 最后一次 drain 与清除 running 之间的提交也必须被接走。
                            // 锁顺序固定为 state -> queues；没有 std 锁跨越 await。
                            let mut state = this.state.lock().unwrap();
                            if token.is_cancelled() {
                                let mut steering = this.config.steering.lock().unwrap();
                                let mut follow_up = this.config.follow_up.lock().unwrap();
                                let discarded = steering.len() + follow_up.len();
                                steering.clear();
                                follow_up.clear();
                                tracing::warn!(issue = "typed", discarded, "discarded queued messages after abort");
                                state.cancel = None;
                                settled.cancel();
                                let _ = this.events.send(AgentEvent::AgentSettled { aborted: true });
                                break;
                            }
                            pending = this.config.steering.lock().unwrap().drain(..).collect();
                            pending.extend(this.config.follow_up.lock().unwrap().drain(..));
                            if pending.is_empty() {
                                state.cancel = None;
                                settled.cancel();
                                let _ = this.events.send(AgentEvent::AgentSettled { aborted: token.is_cancelled() });
                                break;
                            }
                        }
                    })
                    .catch_unwind()
                    .await
                };
                if outcome.is_err() {
                    // Never retain the panic payload: it may contain provider/tool secrets.
                    token.cancel();
                    this.config.steering.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    this.config.follow_up.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    this.state.lock().unwrap_or_else(|e| e.into_inner()).cancel = None;
                    settled.cancel();
                    let _ = this.events.send(AgentEvent::RunOutcome { kind: "task_panic".into() });
                    let _ = this.events.send(AgentEvent::AgentSettled { aborted: false });
                }
            }
            .in_current_span(),
        );
        Disposition::Started
    }

    /// 兼容等待式调用：Started 时等待整个 run 并返回新增消息；None 表示已排队，而非启动失败。
    pub async fn prompt(self: &Arc<Self>, message: Message) -> Option<Vec<Message>> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        if self.submit_with(message, None, move |messages| {
            let _ = tx.send(messages);
        }) != Disposition::Started
        {
            return None;
        }
        let mut out = vec![];
        while let Some(messages) = rx.recv().await {
            out.extend(messages);
        }
        Some(out)
    }

    pub fn steer(self: &Arc<Self>, m: Message) {
        self.submit(m, Some("steer"));
    }
    pub fn follow_up(self: &Arc<Self>, m: Message) {
        self.submit(m, Some("follow_up"));
    }
    pub fn abort(&self) {
        if let Some(t) = self.state.lock().unwrap().cancel.as_ref() {
            t.cancel();
        }
    }
    /// 在会话事件流上广播一个事件（例如运行期配置变化）。
    pub fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }
    pub async fn messages(&self) -> Vec<Message> {
        self.messages.lock().unwrap().clone()
    }

    /// 替换整个上下文（例如手动压缩）。只应在持有 [`ExclusiveGuard`] 时调用，不能与 run 并发。
    pub fn replace_messages(&self, messages: Vec<Message>) {
        *self.messages.lock().unwrap() = messages;
    }

    /// 在没有 run 时独占 Agent（例如手动压缩）：期间 `is_running()` 为 true、提交会排队、`abort()` 取消 guard 的 token。
    /// 正在运行时返回 None。
    pub fn try_exclusive(self: &Arc<Self>) -> Option<ExclusiveGuard> {
        let mut state = self.state.lock().unwrap();
        if state.cancel.is_some() {
            return None;
        }
        let token = CancellationToken::new();
        state.cancel = Some(token.clone());
        Some(ExclusiveGuard { agent: self.clone(), token, released: false })
    }

    fn release_exclusive(&self) -> Vec<(Message, &'static str)> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cancel = None;
        let mut queued: Vec<(Message, &'static str)> = self.config.steering.lock().unwrap_or_else(|e| e.into_inner()).drain(..).map(|m| (m, "steer")).collect();
        queued.extend(self.config.follow_up.lock().unwrap_or_else(|e| e.into_inner()).drain(..).map(|m| (m, "follow_up")));
        queued
    }
}

/// 见 [`Agent::try_exclusive`]。
pub struct ExclusiveGuard {
    agent: Arc<Agent>,
    pub token: CancellationToken,
    released: bool,
}

impl ExclusiveGuard {
    /// 结束独占；返回期间排队的消息（steer 在前、按提交顺序）及其模式，由调用方重新提交。
    pub fn release(mut self) -> Vec<(Message, &'static str)> {
        self.released = true;
        self.agent.release_exclusive()
    }
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        if !self.released {
            let discarded = self.agent.release_exclusive().len();
            if discarded > 0 {
                tracing::warn!(issue = "typed", discarded, "discarded queued messages after an interrupted exclusive operation");
            }
        }
    }
}
