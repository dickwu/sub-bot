use crate::config::{Client, Config, Profile};
use agent_ai::{faux::FauxProvider, models::reasoning_support, openai::OpenAiProvider, Message, ModelInfo, Provider, ReasoningSupport, ThinkingLevel};
use agent_core::{Agent, AgentEvent, AgentTool, LoopConfig};
use agent_session::Session;
use agent_tools::{preset, CallbackTool, CallerToolSpec, ToolContext, ToolRegistry};
use anyhow::{bail, Result};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// 一个会话 = 一个 Agent + 可选 JSONL 持久化，归属于创建它的 client（会话隔离）。
pub struct SessionHandle {
    pub id: String,
    pub owner: String,
    pub profile: String,
    pub cwd: PathBuf,
    pub agent: Arc<Agent>,
    /// 调用方/配置请求的推理强度（解析前）
    pub requested_effort: Option<ThinkingLevel>,
    /// 最近一次成功调用实际使用的 (API, 推理强度)
    last_call: Mutex<Option<(agent_ai::Api, Option<ThinkingLevel>)>>,
    store: Mutex<Option<Session>>,
    last_activity: Mutex<Instant>,
    idle_ttl: Duration,
    ephemeral: bool,
}

pub use agent_core::Disposition;

impl SessionHandle {
    /// 运行中则排队（默认 follow_up），否则后台启动一次 run。
    pub fn submit(self: &Arc<Self>, text: String, mode: Option<&str>) -> Disposition {
        let mut activity = self.last_activity.lock().unwrap();
        *activity = Instant::now();
        let msg_len = text.chars().count();
        let msg = Message::user_text(text);
        let this = self.clone();
        // 会话 span 挂在当前请求 span 之下，因此运行期间的日志同时带 request_id / bot / session_id / profile
        let effort = self.agent.config.reasoning.map(|l| l.as_str()).unwrap_or("-");
        let span = tracing::info_span!("session_run", session_id = %self.id, bot = %self.owner, profile = %self.profile, model = %self.agent.config.model, reasoning_effort = effort);
        let _entered = span.enter();
        let started = std::time::Instant::now();
        let disposition = self.agent.submit_with(msg, mode, move |new| {
            let (mut input, mut output, mut reasoning_tokens, mut tools) = (0u64, 0u64, 0u64, 0usize);
            let mut stop = None;
            for m in &new {
                match m {
                    Message::Assistant(a) => {
                        input += a.usage.input;
                        output += a.usage.output;
                        reasoning_tokens += a.usage.reasoning;
                        if let Some(api) = a.api {
                            *this.last_call.lock().unwrap() = Some((api, a.reasoning_effort));
                        }
                        stop = Some(a.stop_reason);
                        if let Some(e) = &a.error_message {
                            tracing::warn!(error = %e, "assistant message ended with error");
                        }
                    }
                    Message::ToolResult(_) => tools += 1,
                    _ => {}
                }
            }
            tracing::info!(latency_ms = started.elapsed().as_millis() as u64, messages = new.len(), tool_calls = tools, input_tokens = input, output_tokens = output, reasoning_tokens, stop_reason = ?stop, "run finished");
            if let Some(store) = this.store.lock().unwrap().as_mut() {
                for m in new {
                    if let Err(e) = store.append_message(m) {
                        tracing::error!(error = %e, "persist failed");
                    }
                }
            }
            // 持久化完成后更新：长时间运行不会消耗下一次空闲窗口。
            *this.last_activity.lock().unwrap() = Instant::now();
        });
        drop(activity);
        tracing::info!(prompt_chars = msg_len, ?disposition, "message submitted");
        disposition
    }

    /// 实际生效的推理强度：有调用记录时取最近一次真正发送的值，否则取会话解析后的值。
    pub fn effective_effort(&self) -> Option<ThinkingLevel> {
        match *self.last_call.lock().unwrap() {
            Some((_, e)) => e,
            None => self.agent.config.reasoning,
        }
    }

    /// 最近一次调用实际使用的 API（还没调用过时为 None）。
    pub fn effective_api(&self) -> Option<agent_ai::Api> {
        self.last_call.lock().unwrap().map(|(a, _)| a)
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentEvent> {
        self.agent.subscribe()
    }
}

/// 调用方在创建会话时可以提交的覆盖项（都受 profile 权限约束）。
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRequest {
    pub profile: Option<String>,
    /// 会话工作目录：绝对路径或相对 profile 第一个 workspaceRoot；必须落在某个 workspaceRoot 内
    pub cwd: Option<PathBuf>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    /// 覆盖 profile 的默认模型；受 profile.allowedModels 限制
    pub model: Option<String>,
    /// 推理强度 none|minimal|low|medium|high|xhigh；模型不支持时忽略并记 warn 日志
    pub reasoning_effort: Option<ThinkingLevel>,
    /// OpenAI API：auto | chat | responses（覆盖 profile.api）
    pub api: Option<agent_ai::Api>,
    /// 只能从 profile 已启用的工具里再收窄
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub caller_tools: Vec<CallerToolSpec>,
}

#[derive(Debug)]
pub struct TooManySessions;

impl std::fmt::Display for TooManySessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("too many sessions for this client; delete a session or wait for idle expiry")
    }
}

impl std::error::Error for TooManySessions {}

pub struct Hub {
    pub config: Config,
    provider: Arc<dyn Provider>,
    registry: ToolRegistry,
    session_dir: Option<PathBuf>,
    sessions: Mutex<HashMap<String, Arc<SessionHandle>>>,
    shutting_down: AtomicBool,
    http: reqwest::Client,
    models_cache: tokio::sync::Mutex<Option<(std::time::Instant, Vec<ModelInfo>)>>,
}

/// 按模型能力处理推理强度：不支持则忽略（warn），取值不支持则换成最接近的（warn），未知模型原样发送。
pub fn resolve_effort(model: &str, requested: Option<ThinkingLevel>) -> Option<ThinkingLevel> {
    let level = requested?;
    match reasoning_support(model) {
        ReasoningSupport::Unsupported => {
            tracing::warn!(model, requested = %level, "model does not support reasoning_effort; ignoring");
            None
        }
        ReasoningSupport::Levels(ls) if !ls.contains(&level) => {
            let n = level.nearest_in(ls);
            tracing::warn!(model, requested = %level, using = %n.map(|l| l.as_str()).unwrap_or("-"), "reasoning_effort not supported by model; using nearest");
            n
        }
        _ => Some(level),
    }
}

fn expand_tools(names: &[String]) -> Result<Vec<String>> {
    let mut out = vec![];
    for n in names {
        match n.strip_prefix("preset:") {
            Some(p) => out.extend(preset(p).ok_or_else(|| anyhow::anyhow!("unknown preset '{p}'"))?),
            None => out.push(n.clone()),
        }
    }
    out.dedup();
    Ok(out)
}

/// 解析并校验会话 cwd：canonicalize 后必须位于 profile 的某个 workspaceRoot 之内（防止越权访问其它目录）。
fn resolve_cwd(p: &Profile, requested: Option<&std::path::Path>) -> Result<(PathBuf, PathBuf)> {
    let roots: Vec<PathBuf> = p.workspace_roots.iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
    let first = roots.first().ok_or_else(|| anyhow::anyhow!("profile has no existing workspaceRoots"))?;
    let cwd = match requested {
        None => first.clone(),
        Some(c) if c.is_absolute() => std::fs::canonicalize(c)?,
        Some(c) => std::fs::canonicalize(first.join(c))?,
    };
    if !cwd.is_dir() {
        bail!("cwd is not a directory");
    }
    let root = roots.iter().find(|r| cwd.starts_with(r)).ok_or_else(|| anyhow::anyhow!("cwd is outside the profile's workspaceRoots"))?;
    Ok((cwd, root.clone()))
}

impl Hub {
    /// `openai_key_file`：命令行 `--openai-key-file`。key 的加载顺序见 `agent_ai::secrets::load_provider_key`。
    pub fn new(config: Config, provider_name: &str, session_dir: Option<PathBuf>, openai_key_file: Option<&std::path::Path>) -> Result<Arc<Self>> {
        config.validate()?;
        let provider: Arc<dyn Provider> = match provider_name {
            "faux" => Arc::new(FauxProvider),
            "openai" => {
                let pk = agent_ai::secrets::provider_key("openai").expect("openai key spec");
                let (key, source) = agent_ai::secrets::load_provider_key(pk, openai_key_file, config.api_key_file.as_deref(), |k| std::env::var(k).ok())
                    .map_err(|e| anyhow::anyhow!("{e} (or use --provider faux)"))?;
                // 只记录来源（变量名/路径），不记录 key
                tracing::info!(provider = "openai", key_source = %source, "api key loaded");
                Arc::new(OpenAiProvider::with_stream_idle_timeout(key, std::env::var("OPENAI_BASE_URL").ok(), std::time::Duration::from_secs(config.stream_idle_timeout_secs)))
            }
            other => bail!("unknown provider {other}"),
        };
        let registry = ToolRegistry::with_builtins();
        Ok(Arc::new(Self {
            config,
            provider,
            registry,
            session_dir,
            sessions: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            http: reqwest::Client::new(),
            models_cache: Default::default(),
        }))
    }

    pub fn create_session(&self, client: &Client, req: SessionRequest) -> Result<Arc<SessionHandle>> {
        self.create_session_with_ttl(client, req, false)
    }

    pub fn create_chat_session(&self, client: &Client, req: SessionRequest) -> Result<Arc<SessionHandle>> {
        self.create_session_with_ttl(client, req, true)
    }

    fn create_session_with_ttl(&self, client: &Client, req: SessionRequest, ephemeral: bool) -> Result<Arc<SessionHandle>> {
        // 上限检查与插入共用 map 锁，避免并发创建突破上限。
        let mut sessions = self.sessions.lock().unwrap();
        anyhow::ensure!(!self.shutting_down.load(Ordering::Relaxed), "server is shutting down");
        let cap = self.config.max_sessions_per_client;
        if cap != 0 && sessions.values().filter(|h| h.owner == client.name).count() >= cap {
            Self::evict_idle_from(&mut sessions, Instant::now(), Some(&client.name));
            if sessions.values().filter(|h| h.owner == client.name).count() >= cap {
                return Err(TooManySessions.into());
            }
        }
        let profile_name = req.profile.clone().or_else(|| client.profiles.first().cloned()).ok_or_else(|| anyhow::anyhow!("client has no profiles"))?;
        if !client.profiles.contains(&profile_name) {
            bail!("profile '{profile_name}' is not allowed for client '{}'", client.name);
        }
        let p: &Profile = self.config.profiles.get(&profile_name).ok_or_else(|| anyhow::anyhow!("unknown profile '{profile_name}'"))?;

        let allowed = expand_tools(&p.tools)?;
        let selected = match req.tools {
            Some(t) => {
                if let Some(bad) = t.iter().find(|n| !allowed.contains(n)) {
                    bail!("tool '{bad}' is not enabled in profile '{profile_name}'");
                }
                t
            }
            None => allowed,
        };
        if (req.system_prompt.is_some() || req.append_system_prompt.is_some()) && !p.allow_system_prompt_override {
            bail!("profile '{profile_name}' does not allow system prompt override");
        }
        if !req.caller_tools.is_empty() && !p.allow_caller_tools {
            bail!("profile '{profile_name}' does not allow caller tools");
        }

        let store = match &self.session_dir {
            Some(dir) => Some(Session::create(&dir.join(&client.name), &profile_name)?),
            None => None,
        };
        let id = store.as_ref().map(|s| s.id.clone()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let (cwd, root) = resolve_cwd(p, req.cwd.as_deref())?;
        let ctx = ToolContext { cwd: cwd.clone(), http_allow_hosts: p.http_allow_hosts.clone(), shell_env: p.shell_env.clone() };
        let mut tools: Vec<Arc<dyn AgentTool>> = self.registry.build(&selected, &ctx)?;
        for spec in req.caller_tools {
            if tools.iter().any(|t| t.name() == spec.name) {
                bail!("caller tool '{}' collides with a built-in tool", spec.name);
            }
            tools.push(Arc::new(CallbackTool { spec, session_id: id.clone(), http: self.http.clone() }));
        }

        let model = match req.model {
            Some(m) if !Config::model_allowed(p, &m) => {
                bail!("model '{m}' is not allowed in profile '{profile_name}' (allowedModels)")
            }
            Some(m) => m,
            None => p.model.clone().unwrap_or_else(|| self.config.default_model.clone()),
        };
        if !Config::model_allowed(p, &model) {
            bail!("default model '{model}' is not in profile '{profile_name}' allowedModels");
        }
        let requested_effort = req.reasoning_effort.or(p.reasoning_effort).or(self.config.default_reasoning_effort);
        let reasoning = resolve_effort(&model, requested_effort);
        let config = LoopConfig {
            provider: self.provider.clone(),
            model,
            system_prompt: Some(crate::prompt::build(crate::prompt::PromptInput {
                custom: req.system_prompt.as_deref().or(p.system_prompt.as_deref()),
                append: &[p.append_system_prompt.as_deref().unwrap_or(""), req.append_system_prompt.as_deref().unwrap_or("")],
                tools: &tools,
                cwd: &cwd,
                root: &root,
                load_context_files: p.load_context_files,
            })),
            tools,
            reasoning,
            api: req.api.or(p.api).unwrap_or(self.config.default_api),
            steering: Default::default(),
            follow_up: Default::default(),
            max_turns: p.max_turns,
            max_tokens: p.max_output_tokens,
            token_budget: p.token_budget,
            run_timeout: p.run_timeout_secs.map(std::time::Duration::from_secs),
        };
        tracing::info!(session_id = %id, bot = %client.name, profile = %profile_name, cwd = %cwd.display(), model = %config.model, reasoning_effort = config.reasoning.map(|l| l.as_str()).unwrap_or("-"), api = %self.effective_api(&config), tools = ?config.tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>(), "session created");
        let h = Arc::new(SessionHandle {
            id: id.clone(),
            owner: client.name.clone(),
            profile: profile_name,
            cwd,
            agent: Agent::new(config, vec![]),
            requested_effort,
            last_call: Mutex::new(None),
            store: Mutex::new(store),
            last_activity: Mutex::new(Instant::now()),
            idle_ttl: Duration::from_secs(if ephemeral { self.config.chat_session_ttl_secs } else { self.config.session_idle_ttl_secs }),
            ephemeral,
        });
        sessions.insert(id, h.clone());
        Ok(h)
    }

    /// 只返回属于该 client 的会话；别的 bot 的会话一律当作不存在。
    pub fn get(&self, client: &Client, id: &str) -> Option<Arc<SessionHandle>> {
        self.sessions.lock().unwrap().get(id).filter(|h| h.owner == client.name).cloned()
    }

    /// HTTP 提交与删除/清理共用 map 锁：已取出的旧 handle 不能在删除后重新启动。
    pub fn submit(&self, client: &Client, id: &str, text: String, mode: Option<&str>) -> Option<Disposition> {
        let sessions = self.sessions.lock().unwrap();
        let h = sessions.get(id).filter(|h| h.owner == client.name)?;
        Some(h.submit(text, mode))
    }

    pub fn remove(&self, client: &Client, id: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        if !sessions.get(id).is_some_and(|h| h.owner == client.name) {
            return false;
        }
        let h = sessions.remove(id).unwrap();
        h.agent.abort();
        tracing::info!(session_id = %h.id, bot = %h.owner, "session deleted; persisted JSONL files retained");
        true
    }

    /// 关机时禁止新会话/提交，取消全部运行并等待持久化完成；释放 handle 后 SSE 也会结束。
    pub async fn abort_all(&self) {
        let sessions: Vec<_> = {
            let mut sessions = self.sessions.lock().unwrap();
            self.shutting_down.store(true, Ordering::Relaxed);
            sessions.drain().map(|(_, h)| h).collect()
        };
        for h in &sessions {
            h.agent.abort();
        }
        // Agent 在持久化回调之后才清除 running；不持有 map 锁跨 await。
        while sessions.iter().any(|h| h.agent.is_running()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub fn evict_idle(&self, now: Instant) -> usize {
        Self::evict_idle_from(&mut self.sessions.lock().unwrap(), now, None)
    }

    fn evict_idle_from(sessions: &mut HashMap<String, Arc<SessionHandle>>, now: Instant, owner: Option<&str>) -> usize {
        let before = sessions.len();
        sessions.retain(|_, h| {
            if owner.is_some_and(|o| o != h.owner) || h.idle_ttl.is_zero() {
                return true;
            }
            let activity = h.last_activity.lock().unwrap();
            if h.agent.is_running() || now.saturating_duration_since(*activity) <= h.idle_ttl {
                return true;
            }
            tracing::info!(session_id = %h.id, bot = %h.owner, ephemeral = h.ephemeral, "idle session evicted; persisted JSONL files retained");
            false
        });
        before - sessions.len()
    }

    /// 两类 TTL 独立；普通会话关闭过期时仍然清理 chat 会话。
    pub fn sweep_interval(&self) -> Option<Duration> {
        [self.config.session_idle_ttl_secs, self.config.chat_session_ttl_secs].into_iter().filter(|ttl| *ttl != 0).min().map(|ttl| Duration::from_secs(ttl.min(60)))
    }

    /// 实时模型列表（缓存 10 分钟）。`all=false` 时只保留可用于对话的模型并去掉日期快照。
    pub async fn list_models(&self, all: bool) -> Result<Vec<ModelInfo>> {
        let mut cache = self.models_cache.lock().await;
        let fresh = cache.as_ref().filter(|(t, _)| t.elapsed() < std::time::Duration::from_secs(600)).map(|(_, m)| m.clone());
        let models = match fresh {
            Some(m) => m,
            None => {
                let m = self.provider.list_models().await.map_err(|e| anyhow::anyhow!(e))?;
                *cache = Some((std::time::Instant::now(), m.clone()));
                m
            }
        };
        Ok(if all || self.provider.id() != "openai" { models } else { models.into_iter().filter(|m| agent_ai::models::is_chat_model(&m.id) && !agent_ai::models::is_snapshot(&m.id)).collect() })
    }

    /// 会话预计使用的 API（auto 按模型解析；非 OpenAI provider 返回 "n/a"）。运行时若被迫切换，以消息里的 `api` 为准。
    pub fn effective_api(&self, cfg: &LoopConfig) -> &'static str {
        if self.provider.id() != "openai" {
            return "n/a";
        }
        match cfg.api {
            agent_ai::Api::Auto => agent_ai::default_api(&cfg.model).as_str(),
            a => a.as_str(),
        }
    }

    pub fn provider_id(&self) -> &str {
        self.provider.id()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.registry.names()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let mut cfg = Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
        cfg.profiles.get_mut("default").unwrap().load_context_files = false;
        cfg
    }

    #[test]
    fn cap_is_typed_per_client_and_zero_is_unlimited() {
        let mut cfg = config();
        cfg.max_sessions_per_client = 1;
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let h = hub.create_session(client, SessionRequest::default()).unwrap();
        let err = hub.create_session(client, SessionRequest::default()).err().unwrap();
        assert!(err.is::<TooManySessions>());
        let other = Client { name: "other".into(), ..client.clone() };
        hub.create_session(&other, SessionRequest::default()).unwrap();
        assert!(hub.remove(client, &h.id));
        hub.create_session(client, SessionRequest::default()).unwrap();

        let mut cfg = config();
        cfg.max_sessions_per_client = 0;
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        for _ in 0..65 {
            hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
        }
    }

    #[test]
    fn cap_reclaims_only_owners_expired_sessions() {
        let mut cfg = config();
        cfg.max_sessions_per_client = 1;
        cfg.session_idle_ttl_secs = 10;
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let other = Client { name: "other".into(), ..client.clone() };
        let h = hub.create_session(client, SessionRequest::default()).unwrap();
        let unrelated = hub.create_session(&other, SessionRequest::default()).unwrap();
        let past = Instant::now() - Duration::from_secs(11);
        *h.last_activity.lock().unwrap() = past;
        *unrelated.last_activity.lock().unwrap() = past;
        hub.create_session(client, SessionRequest::default()).unwrap();
        assert!(hub.get(client, &h.id).is_none());
        assert!(hub.get(&other, &unrelated.id).is_some());
    }

    #[test]
    fn idle_eviction_uses_fake_time_and_per_session_ttl() {
        let hub = Hub::new(config(), "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let regular = hub.create_session(client, SessionRequest::default()).unwrap();
        let chat = hub.create_chat_session(client, SessionRequest::default()).unwrap();
        let now = Instant::now();
        *regular.last_activity.lock().unwrap() = now;
        *chat.last_activity.lock().unwrap() = now;
        assert_eq!(hub.evict_idle(now + Duration::from_secs(600)), 0);
        assert_eq!(hub.evict_idle(now + Duration::from_secs(601)), 1);
        assert!(hub.get(client, &chat.id).is_none());
        assert!(hub.get(client, &regular.id).is_some());
        assert_eq!(hub.evict_idle(now + Duration::from_secs(3601)), 1);
        assert_eq!(hub.evict_idle(now + Duration::from_secs(7200)), 0);
    }

    #[test]
    fn disabled_ttl_is_independent_of_chat_ttl() {
        let mut cfg = config();
        cfg.session_idle_ttl_secs = 0;
        cfg.chat_session_ttl_secs = 2;
        let hub = Hub::new(cfg.clone(), "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let regular = hub.create_session(client, SessionRequest::default()).unwrap();
        hub.create_chat_session(client, SessionRequest::default()).unwrap();
        assert_eq!(hub.sweep_interval(), Some(Duration::from_secs(2)));
        assert_eq!(hub.evict_idle(Instant::now() + Duration::from_secs(7200)), 1);
        assert!(hub.get(client, &regular.id).is_some());
        cfg.chat_session_ttl_secs = 0;
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        assert_eq!(hub.sweep_interval(), None);
        hub.create_chat_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
        assert_eq!(hub.evict_idle(Instant::now() + Duration::from_secs(7200)), 0);
    }

    async fn settled(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let AgentEvent::AgentSettled { aborted } = rx.recv().await.unwrap() {
                    return aborted;
                }
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn abort_all_settles_persists_and_prevents_new_runs() {
        let dir = std::env::temp_dir().join(format!("sub-bot-shutdown-{}", uuid::Uuid::new_v4()));
        let hub = Hub::new(config(), "faux", Some(dir.clone()), None).unwrap();
        let client = &hub.config.clients[0];
        let mut handles = vec![];
        for _ in 0..2 {
            let h = hub.create_session(client, SessionRequest::default()).unwrap();
            let rx = h.subscribe();
            h.submit("hello".into(), None);
            h.submit("discard this queued message".into(), None);
            handles.push((h, rx));
        }
        tokio::time::timeout(Duration::from_secs(5), hub.abort_all()).await.unwrap();
        for (h, mut rx) in handles {
            assert!(settled(&mut rx).await);
            assert!(!h.agent.is_running());
            let path = h.store.lock().unwrap().as_ref().unwrap().path.clone();
            let persisted = Session::open(&path).unwrap().active_messages();
            assert!(!persisted.is_empty());
            assert_eq!(persisted, h.agent.messages().await);
            assert!(!persisted.contains(&Message::user_text("discard this queued message")));
            assert!(hub.submit(client, &h.id, "late".into(), None).is_none());
        }
        assert!(hub.create_session(client, SessionRequest::default()).err().unwrap().to_string().contains("shutting down"));
        hub.abort_all().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn remove_checks_owner_and_aborts_running_session() {
        let hub = Hub::new(config(), "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let h = hub.create_session(client, SessionRequest::default()).unwrap();
        let other = Client { name: "other".into(), ..client.clone() };
        let mut rx = h.subscribe();
        h.submit("hello".into(), None);
        assert!(h.agent.is_running());
        assert!(!hub.remove(&other, &h.id));
        assert!(hub.get(&other, &h.id).is_none());
        assert!(hub.remove(client, &h.id));
        assert!(hub.get(client, &h.id).is_none());
        assert!(!hub.remove(client, &h.id));
        assert!(hub.submit(client, &h.id, "late".into(), None).is_none());
        assert!(settled(&mut rx).await);
        assert!(!h.agent.is_running());
    }

    #[tokio::test]
    async fn running_sessions_survive_and_activity_refreshes_on_submit_and_settle() {
        let mut cfg = config();
        cfg.session_idle_ttl_secs = 10;
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        let client = &hub.config.clients[0];
        let h = hub.create_session(client, SessionRequest::default()).unwrap();
        let old = Instant::now() - Duration::from_secs(20);
        *h.last_activity.lock().unwrap() = old;
        let mut rx = h.subscribe();
        h.submit("hello".into(), None);
        assert!(*h.last_activity.lock().unwrap() > old);
        assert_eq!(hub.evict_idle(Instant::now() + Duration::from_secs(100)), 0);
        // 模拟长运行，settle 应重新开启完整的空闲窗口。
        *h.last_activity.lock().unwrap() = old;
        assert!(!settled(&mut rx).await);
        let finished = *h.last_activity.lock().unwrap();
        assert!(finished > old);
        assert_eq!(hub.evict_idle(finished + Duration::from_secs(10)), 0);
        assert_eq!(hub.evict_idle(finished + Duration::from_secs(11)), 1);
    }
}
