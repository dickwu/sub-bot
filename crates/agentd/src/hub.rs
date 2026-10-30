use crate::config::{Client, Config, Profile};
use agent_ai::{faux::FauxProvider, openai::OpenAiProvider, Message, Provider};
use agent_core::{Agent, AgentEvent, AgentTool, LoopConfig};
use agent_session::Session;
use agent_tools::{preset, CallbackTool, CallerToolSpec, ToolContext, ToolRegistry};
use anyhow::{bail, Result};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// 一个会话 = 一个 Agent + 可选 JSONL 持久化，归属于创建它的 client（会话隔离）。
pub struct SessionHandle {
    pub id: String,
    pub owner: String,
    pub profile: String,
    pub cwd: PathBuf,
    pub agent: Arc<Agent>,
    store: Mutex<Option<Session>>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Started,
    QueuedSteer,
    QueuedFollowUp,
}

trait PipeSpawn: std::future::Future<Output = ()> + Send + Sized + 'static {
    fn pipe_spawn(self) {
        tokio::spawn(self);
    }
}
impl<F: std::future::Future<Output = ()> + Send + 'static> PipeSpawn for F {}

impl SessionHandle {
    /// 运行中则排队（默认 follow_up），否则后台启动一次 run。
    pub fn submit(self: &Arc<Self>, text: String, mode: Option<&str>) -> Disposition {
        let msg_len = text.chars().count();
        let msg = Message::user_text(text);
        if self.agent.is_running() {
            tracing::info!(session_id = %self.id, mode = mode.unwrap_or("follow_up"), "message queued while running");
            return if mode == Some("steer") {
                self.agent.steer(msg);
                Disposition::QueuedSteer
            } else {
                self.agent.follow_up(msg);
                Disposition::QueuedFollowUp
            };
        }
        let this = self.clone();
        // 会话 span 挂在当前请求 span 之下，因此运行期间的日志同时带 request_id / bot / session_id / profile
        let span = tracing::info_span!("session_run", session_id = %self.id, bot = %self.owner, profile = %self.profile);
        tracing::Instrument::instrument(async move {
            let started = std::time::Instant::now();
            tracing::info!(prompt_chars = msg_len, "run started");
            if let Some(new) = this.agent.prompt(msg).await {
                let (mut input, mut output, mut tools) = (0u64, 0u64, 0usize);
                let mut stop = None;
                for m in &new {
                    match m {
                        Message::Assistant(a) => {
                            input += a.usage.input;
                            output += a.usage.output;
                            stop = Some(a.stop_reason);
                            if let Some(e) = &a.error_message {
                                tracing::warn!(error = %e, "assistant message ended with error");
                            }
                        }
                        Message::ToolResult(_) => tools += 1,
                        _ => {}
                    }
                }
                tracing::info!(latency_ms = started.elapsed().as_millis() as u64, messages = new.len(), tool_calls = tools, input_tokens = input, output_tokens = output, stop_reason = ?stop, "run finished");
                if let Some(store) = this.store.lock().unwrap().as_mut() {
                    for m in new {
                        if let Err(e) = store.append_message(m) {
                            tracing::error!(error = %e, "persist failed");
                        }
                    }
                }
            }
        }, span)
        .pipe_spawn();
        Disposition::Started
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
    pub model: Option<String>,
    /// 只能从 profile 已启用的工具里再收窄
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub caller_tools: Vec<CallerToolSpec>,
}

pub struct Hub {
    pub config: Config,
    provider: Arc<dyn Provider>,
    registry: ToolRegistry,
    session_dir: Option<PathBuf>,
    sessions: Mutex<HashMap<String, Arc<SessionHandle>>>,
    http: reqwest::Client,
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
    pub fn new(config: Config, provider_name: &str, session_dir: Option<PathBuf>) -> Result<Arc<Self>> {
        let provider: Arc<dyn Provider> = match provider_name {
            "faux" => Arc::new(FauxProvider),
            "openai" => {
                let Ok(key) = std::env::var("OPENAI_API_KEY") else { bail!("OPENAI_API_KEY is not set (or use --provider faux)") };
                Arc::new(OpenAiProvider::new(key, std::env::var("OPENAI_BASE_URL").ok()))
            }
            other => bail!("unknown provider {other}"),
        };
        let registry = ToolRegistry::with_builtins();
        // 启动时校验所有 profile 的工具名，配置错误尽早暴露
        for (name, p) in &config.profiles {
            for t in expand_tools(&p.tools)? {
                if registry.risk(&t).is_none() {
                    bail!("profile '{name}': unknown tool '{t}'");
                }
            }
        }
        Ok(Arc::new(Self { config, provider, registry, session_dir, sessions: Mutex::new(HashMap::new()), http: reqwest::Client::new() }))
    }

    pub fn create_session(&self, client: &Client, req: SessionRequest) -> Result<Arc<SessionHandle>> {
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
        let ctx = ToolContext { cwd: cwd.clone(), http_allow_hosts: p.http_allow_hosts.clone() };
        let mut tools: Vec<Arc<dyn AgentTool>> = self.registry.build(&selected, &ctx)?;
        for spec in req.caller_tools {
            if tools.iter().any(|t| t.name() == spec.name) {
                bail!("caller tool '{}' collides with a built-in tool", spec.name);
            }
            tools.push(Arc::new(CallbackTool { spec, session_id: id.clone(), http: self.http.clone() }));
        }

        let config = LoopConfig {
            provider: self.provider.clone(),
            model: req.model.or_else(|| p.model.clone()).unwrap_or_else(|| self.config.default_model.clone()),
            system_prompt: Some(crate::prompt::build(crate::prompt::PromptInput {
                custom: req.system_prompt.as_deref().or(p.system_prompt.as_deref()),
                append: &[p.append_system_prompt.as_deref().unwrap_or(""), req.append_system_prompt.as_deref().unwrap_or("")],
                tools: &tools,
                cwd: &cwd,
                root: &root,
                load_context_files: p.load_context_files,
            })),
            tools,
            reasoning: None,
            steering: Default::default(),
            follow_up: Default::default(),
            max_turns: p.max_turns,
        };
        tracing::info!(session_id = %id, bot = %client.name, profile = %profile_name, cwd = %cwd.display(), model = %config.model, tools = ?config.tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>(), "session created");
        let h = Arc::new(SessionHandle { id: id.clone(), owner: client.name.clone(), profile: profile_name, cwd, agent: Agent::new(config, vec![]), store: Mutex::new(store) });
        self.sessions.lock().unwrap().insert(id, h.clone());
        Ok(h)
    }

    /// 只返回属于该 client 的会话；别的 bot 的会话一律当作不存在。
    pub fn get(&self, client: &Client, id: &str) -> Option<Arc<SessionHandle>> {
        self.sessions.lock().unwrap().get(id).filter(|h| h.owner == client.name).cloned()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.registry.names()
    }
}
