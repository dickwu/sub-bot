//! 配置文件（JSON）：profiles（预设）+ clients（每个 bot 一个 token，映射到允许的 profile）。
//! 示例见仓库根目录 `agentd.example.json`。
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    /// 替换默认编码 agent 前言（对应 pi `customPrompt` / `--system-prompt`）
    pub system_prompt: Option<String>,
    /// 追加到系统提示末尾（对应 pi `appendSystemPrompt`）
    pub append_system_prompt: Option<String>,
    pub model: Option<String>,
    /// 工具白名单：工具名或 `preset:<name>`；默认是 pi 的编码工具集
    #[serde(default = "default_tools")]
    pub tools: Vec<String>,
    /// 允许作为会话 cwd 的根目录；会话 cwd 必须位于其中之一（第一个是默认 cwd）
    pub workspace_roots: Vec<PathBuf>,
    /// 是否从 cwd 向上读取 AGENTS.md / CLAUDE.md 注入系统提示（对应 pi resource-loader.ts）
    #[serde(default = "yes")]
    pub load_context_files: bool,
    #[serde(default)]
    pub http_allow_hosts: Vec<String>,
    /// 是否允许调用方在会话里覆盖 system prompt
    #[serde(default)]
    pub allow_system_prompt_override: bool,
    /// 是否允许调用方注册回调工具
    #[serde(default)]
    pub allow_caller_tools: bool,
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
}

fn default_tools() -> Vec<String> {
    agent_tools::DEFAULT_TOOLS.iter().map(|s| s.to_string()).collect()
}
fn yes() -> bool {
    true
}
fn default_max_turns() -> usize {
    50
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub name: String,
    /// token 的 SHA-256（hex）。用 `pi-agentd hash-token <token>` 生成，配置文件里不存明文。
    pub token_sha256: String,
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_model")]
    pub default_model: String,
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub clients: Vec<Client>,
}

fn default_model() -> String {
    "gpt-4.1-mini".into()
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    /// 无配置文件时的默认值：一个以 `workspace` 为根、启用默认编码工具的 `default` profile + 单一 client。
    pub fn fallback(model: String, token: &str, workspace: PathBuf) -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "default".into(),
            Profile {
                system_prompt: None,
                append_system_prompt: None,
                model: None,
                tools: default_tools(),
                workspace_roots: vec![workspace],
                load_context_files: true,
                http_allow_hosts: vec![],
                allow_system_prompt_override: true,
                allow_caller_tools: true,
                max_turns: 30,
            },
        );
        Config { default_model: model, profiles, clients: vec![Client { name: "default".into(), token_sha256: sha256_hex(token), profiles: vec!["default".into()] }] }
    }

    /// 常量时间比较哈希，找出 token 对应的客户端。
    pub fn client_for_token(&self, token: &str) -> Option<&Client> {
        let h = sha256_hex(token);
        self.clients.iter().find(|c| c.token_sha256.len() == h.len() && c.token_sha256.bytes().zip(h.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0)
    }
}
