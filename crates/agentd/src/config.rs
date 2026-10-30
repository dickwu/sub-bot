//! 配置文件（JSON）：profiles（预设）+ clients（每个 bot 一个 token，映射到允许的 profile）。
//! 示例见仓库根目录 `sub-bot.example.json`。
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
    /// 允许调用方选择的模型（精确名或 `*` 通配，如 `gpt-5*`）；不填则不限制
    #[serde(default)]
    pub allowed_models: Option<Vec<String>>,
    /// 默认推理强度：none | minimal | low | medium | high | xhigh；不填则不发送
    #[serde(default)]
    pub reasoning_effort: Option<agent_ai::ThinkingLevel>,
    /// OpenAI API：auto | chat | responses；不填用全局 defaultApi
    #[serde(default)]
    pub api: Option<agent_ai::Api>,
    /// 单次模型调用的输出 token 上限（含推理 token）
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// 单次运行（一个 prompt）的总 token 预算；达到后让模型不带工具给出最终答复
    #[serde(default)]
    pub token_budget: Option<u64>,
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
    /// bash 子进程的环境变量策略：`{mode: inherit|allowlist, deny, allow}`；默认 inherit 并剔除内置密钥变量
    #[serde(default)]
    pub shell_env: agent_tools::ShellEnv,
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
    /// token 的 SHA-256（hex）。用 `sub-bot hash-token <token>` 生成，配置文件里不存明文。
    pub token_sha256: String,
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_model")]
    pub default_model: String,
    /// 所有 profile 的默认推理强度（profile 自己的 reasoningEffort 优先）
    #[serde(default)]
    pub default_reasoning_effort: Option<agent_ai::ThinkingLevel>,
    /// 默认 OpenAI API：auto（默认，gpt-5.4+/gpt-6.x/o 系列走 Responses）| chat | responses
    #[serde(default)]
    pub default_api: agent_ai::Api,
    /// OpenAI API key 文件（相对路径相对于配置文件所在目录）。优先级低于 --openai-key-file 和 OPENAI_API_KEY_FILE，高于 OPENAI_API_KEY
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
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
        let mut cfg: Self = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        if let (Some(f), Some(dir)) = (&cfg.api_key_file, path.parent()) {
            if f.is_relative() {
                cfg.api_key_file = Some(dir.join(f));
            }
        }
        Ok(cfg)
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
                allowed_models: None,
                reasoning_effort: None,
                api: None,
                max_output_tokens: None,
                token_budget: None,
                tools: default_tools(),
                workspace_roots: vec![workspace],
                load_context_files: true,
                http_allow_hosts: vec![],
                shell_env: Default::default(),
                allow_system_prompt_override: true,
                allow_caller_tools: true,
                max_turns: 30,
            },
        );
        Config {
            default_model: model,
            default_reasoning_effort: None,
            default_api: Default::default(),
            api_key_file: None,
            profiles,
            clients: vec![Client { name: "default".into(), token_sha256: sha256_hex(token), profiles: vec!["default".into()] }],
        }
    }

    /// profile 是否允许使用该模型。
    pub fn model_allowed(p: &Profile, model: &str) -> bool {
        p.allowed_models.as_ref().map_or(true, |list| list.iter().any(|pat| agent_ai::models::model_matches(pat, model)))
    }

    /// 常量时间比较哈希，找出 token 对应的客户端。
    pub fn client_for_token(&self, token: &str) -> Option<&Client> {
        let h = sha256_hex(token);
        self.clients.iter().find(|c| c.token_sha256.len() == h.len() && c.token_sha256.bytes().zip(h.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_file_is_relative_to_config_and_shell_env_parses() {
        let dir = std::env::temp_dir().join(format!("sub-bot-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sub-bot.json");
        std::fs::write(&path, r#"{"apiKeyFile":"keys/openai","profiles":{"p":{"workspaceRoots":["."],"shellEnv":{"mode":"allowlist","allow":["CARGO_*"]}}}}"#).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.api_key_file.as_deref(), Some(dir.join("keys/openai").as_path()));
        let se = &cfg.profiles["p"].shell_env;
        assert_eq!(se.mode, agent_tools::EnvMode::Allowlist);
        assert_eq!(se.allow, vec!["CARGO_*".to_string()]);
        std::fs::remove_dir_all(dir).ok();
    }
}
