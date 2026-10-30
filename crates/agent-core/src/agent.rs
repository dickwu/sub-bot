use crate::{agent_loop::*, event::AgentEvent};
use agent_ai::Message;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, Mutex as AsyncMutex};
use tokio_util::sync::CancellationToken;

/// 有状态 Agent（对应 `packages/agent/src/agent.ts` 的 `Agent` 类）。
/// 同一时刻只跑一个 run；运行中再来的输入走 steer / follow_up 队列。
pub struct Agent {
    pub config: LoopConfig,
    messages: AsyncMutex<Vec<Message>>,
    events: broadcast::Sender<AgentEvent>,
    cancel: Mutex<Option<CancellationToken>>,
}

impl Agent {
    pub fn new(config: LoopConfig, history: Vec<Message>) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Self { config, messages: AsyncMutex::new(history), events, cancel: Mutex::new(None) })
    }

    /// 订阅事件（对应 `subscribe`）。请在 prompt 之前订阅，避免漏掉快速完成的事件。
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    pub fn is_running(&self) -> bool {
        self.cancel.lock().unwrap().is_some()
    }

    /// 运行一次完整 run，返回新增消息。若已有 run 在跑，返回 None（调用方应改用 steer/follow_up）。
    pub async fn prompt(self: &Arc<Self>, message: Message) -> Option<Vec<Message>> {
        let Ok(mut ctx) = self.messages.try_lock() else { return None };
        let token = CancellationToken::new();
        *self.cancel.lock().unwrap() = Some(token.clone());
        let tx = self.events.clone();
        let emit: Emit = Arc::new(move |e| {
            let _ = tx.send(e);
        });
        let out = run_loop(&mut ctx, vec![message], &self.config, token.clone(), emit).await;
        *self.cancel.lock().unwrap() = None;
        let _ = self.events.send(AgentEvent::AgentSettled { aborted: token.is_cancelled() });
        Some(out)
    }

    pub fn steer(&self, m: Message) {
        self.config.steering.lock().unwrap().push_back(m);
    }
    pub fn follow_up(&self, m: Message) {
        self.config.follow_up.lock().unwrap().push_back(m);
    }
    pub fn abort(&self) {
        if let Some(t) = self.cancel.lock().unwrap().as_ref() {
            t.cancel();
        }
    }
    pub async fn messages(&self) -> Vec<Message> {
        self.messages.lock().await.clone()
    }
}
