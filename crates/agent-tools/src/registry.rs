use crate::builtin::{
    clock::ClockTool,
    fs::*,
    http::HttpFetchTool,
    search::{FindTool, GrepTool},
};
use agent_core::AgentTool;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

/// 风险等级，用于权限审查与文档（profile 显式列出才会启用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Safe,
    ReadFs,
    WriteFs,
    Exec,
    Network,
}

/// 构造工具时所需的会话级上下文。`cwd` 是该会话的工作目录（所有文件路径都限制在其中）。
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub http_allow_hosts: Vec<String>,
    /// bash 子进程的环境变量策略
    pub shell_env: crate::ShellEnv,
}

/// 默认启用的编码工具（pi 的默认是 read/bash/edit/write，见 agent-session.ts 中的 defaultTools；
/// 这里额外默认启用只读的 ls / grep / find，模型不必依赖宿主机装有 rg / find）。
pub const DEFAULT_TOOLS: &[&str] = &["read", "ls", "grep", "find", "edit", "write", "bash"];

type Factory = Arc<dyn Fn(&ToolContext) -> Option<Arc<dyn AgentTool>> + Send + Sync>;

/// 按名字注册的工具工厂。agent 核心不知道任何具体工具。
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, (Risk, Factory)>,
}

impl ToolRegistry {
    pub fn register(&mut self, name: &str, risk: Risk, f: impl Fn(&ToolContext) -> Option<Arc<dyn AgentTool>> + Send + Sync + 'static) {
        self.tools.insert(name.into(), (risk, Arc::new(f)));
    }

    /// 核心编码工具 + 可选辅助工具。具体启用哪些由 profile 的白名单决定。
    pub fn with_builtins() -> Self {
        let mut r = Self::default();
        // 核心：pi coding-agent 内置工具
        r.register("read", Risk::ReadFs, |c| Some(Arc::new(ReadTool { root: c.cwd.clone() })));
        r.register("ls", Risk::ReadFs, |c| Some(Arc::new(LsTool { root: c.cwd.clone() })));
        r.register("grep", Risk::ReadFs, |c| Some(Arc::new(GrepTool { root: c.cwd.clone() })));
        r.register("find", Risk::ReadFs, |c| Some(Arc::new(FindTool { root: c.cwd.clone() })));
        r.register("edit", Risk::WriteFs, |c| Some(Arc::new(EditTool { root: c.cwd.clone() })));
        r.register("write", Risk::WriteFs, |c| Some(Arc::new(WriteTool { root: c.cwd.clone() })));
        r.register("bash", Risk::Exec, |c| Some(Arc::new(BashTool { root: c.cwd.clone(), env: c.shell_env.clone() })));
        // 可选辅助
        r.register("clock", Risk::Safe, |_| Some(Arc::new(ClockTool)));
        r.register("http_fetch", Risk::Network, |c| {
            Some(Arc::new(HttpFetchTool {
                allow_hosts: c.http_allow_hosts.clone(),
                http: reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(15)).timeout(std::time::Duration::from_secs(60)).build().expect("http client"),
            }))
        });
        r
    }

    pub fn risk(&self, name: &str) -> Option<Risk> {
        self.tools.get(name).map(|(r, _)| *r)
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// 实例化一组工具；未知名字或缺少上下文的会报错，避免静默少工具。
    pub fn build(&self, names: &[String], ctx: &ToolContext) -> anyhow::Result<Vec<Arc<dyn AgentTool>>> {
        names
            .iter()
            .map(|n| {
                let (_, f) = self.tools.get(n).ok_or_else(|| anyhow::anyhow!("unknown tool '{n}'"))?;
                f(ctx).ok_or_else(|| anyhow::anyhow!("tool '{n}' cannot be built in this context"))
            })
            .collect()
    }
}

/// 工具白名单预设，方便 profile 引用（`preset:<name>`）。
pub fn preset(name: &str) -> Option<Vec<String>> {
    let v: &[&str] = match name {
        "default" => DEFAULT_TOOLS,
        "readonly" => &["read", "ls", "grep", "find"],
        "no-shell" => &["read", "ls", "grep", "find", "edit", "write"],
        _ => return None,
    };
    Some(v.iter().map(|s| s.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_include_read_only_search_tools() {
        let r = ToolRegistry::with_builtins();
        for name in ["default", "readonly", "no-shell"] {
            let tools = preset(name).unwrap();
            assert!(tools.contains(&"grep".to_string()) && tools.contains(&"find".to_string()), "{name}: {tools:?}");
            // 预设里的每个工具都已注册
            assert!(tools.iter().all(|t| r.risk(t).is_some()), "{name}: {tools:?}");
        }
        assert!(!preset("readonly").unwrap().iter().any(|t| r.risk(t) != Some(Risk::ReadFs)));
        assert_eq!((r.risk("grep"), r.risk("find")), (Some(Risk::ReadFs), Some(Risk::ReadFs)));
        let built = r.build(&["grep".into(), "find".into()], &ToolContext::default()).unwrap();
        assert_eq!(built.iter().map(|t| t.name()).collect::<Vec<_>>(), ["grep", "find"]);
    }
}
