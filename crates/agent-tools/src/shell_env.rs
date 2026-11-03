//! 子进程（bash 工具）的环境变量策略。
//!
//! - `inherit`（默认）：继承 sub-bot 的环境，但剔除 `agent_ai::secrets::secret_env_vars()`（各 provider 的 key、
//!   `*_FILE`、`SUB_BOT_TOKEN` 等）以及 `deny` 匹配的变量；`allow` 可以豁免 `deny`，但豁免不了内置密钥变量。
//! - `allowlist`：清空环境，只传 `BASE_ALLOWLIST` 和 `allow` 匹配的变量（同样去掉内置密钥与 `deny`）。
//!
//! 名字支持 `*` 通配（如 `AWS_*`）。注意：这只能防止密钥出现在子进程环境里；同一用户的子进程仍可能读到
//! `/proc/<ppid>/environ`（Linux）或 key 文件，见 README 的“API key”一节。
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvMode {
    #[default]
    Inherit,
    Allowlist,
}

/// allowlist 模式下默认保留的变量。
pub const BASE_ALLOWLIST: &[&str] = &["PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_*", "TERM", "TZ", "TMPDIR", "TMP", "TEMP"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShellEnv {
    #[serde(default)]
    pub mode: EnvMode,
    /// 额外剔除的变量（支持 `*`）
    #[serde(default)]
    pub deny: Vec<String>,
    /// inherit：豁免 `deny`；allowlist：在 `BASE_ALLOWLIST` 之外额外保留的变量（支持 `*`）
    #[serde(default)]
    pub allow: Vec<String>,
}

/// 对 `Command` 的具体操作，便于测试。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EnvPlan {
    pub clear: bool,
    pub remove: Vec<OsString>,
    pub set: Vec<(OsString, OsString)>,
}

pub(crate) fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

fn matches_any<S: AsRef<str>>(patterns: &[S], name: &OsStr) -> bool {
    name.to_str().is_some_and(|n| patterns.iter().any(|p| glob(p.as_ref(), n)))
}

impl ShellEnv {
    pub fn plan(&self, vars: impl IntoIterator<Item = (OsString, OsString)>) -> EnvPlan {
        let secrets = agent_ai::secrets::secret_env_vars();
        let is_secret = |n: &OsStr| secrets.iter().any(|s| OsStr::new(s) == n);
        match self.mode {
            EnvMode::Inherit => {
                // 内置密钥变量无条件 env_remove（即使当前进程里没有，也无害）
                let mut remove: Vec<OsString> = secrets.iter().map(OsString::from).collect();
                for (k, _) in vars {
                    if !is_secret(&k) && matches_any(&self.deny, &k) && !matches_any(&self.allow, &k) {
                        remove.push(k);
                    }
                }
                EnvPlan { clear: false, remove, set: vec![] }
            }
            EnvMode::Allowlist => {
                let set = vars.into_iter().filter(|(k, _)| !is_secret(k) && !matches_any(&self.deny, k) && (matches_any(BASE_ALLOWLIST, k) || matches_any(&self.allow, k))).collect();
                EnvPlan { clear: true, remove: vec![], set }
            }
        }
    }

    /// 应用到即将启动的子进程上。
    pub fn apply(&self, cmd: &mut tokio::process::Command) {
        let plan = self.plan(std::env::vars_os());
        if plan.clear {
            cmd.env_clear();
        }
        for k in &plan.remove {
            cmd.env_remove(k);
        }
        cmd.envs(plan.set);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(names: &[&str]) -> Vec<(OsString, OsString)> {
        names.iter().map(|n| (OsString::from(n), OsString::from("v"))).collect()
    }

    #[test]
    fn glob_matches() {
        assert!(glob("AWS_*", "AWS_SECRET_ACCESS_KEY"));
        assert!(glob("*_TOKEN", "GITHUB_TOKEN"));
        assert!(glob("A*B*C", "AxxBxxC"));
        assert!(!glob("AWS_*", "XAWS_A"));
        assert!(!glob("PATH", "PATHX"));
    }

    #[test]
    fn inherit_removes_secrets_and_deny() {
        let e = ShellEnv { deny: vec!["AWS_*".into(), "GITHUB_TOKEN".into()], allow: vec!["AWS_REGION".into(), "OPENAI_API_KEY".into()], ..Default::default() };
        let p = e.plan(vars(&["PATH", "AWS_SECRET", "AWS_REGION", "GITHUB_TOKEN", "OPENAI_API_KEY"]));
        assert!(!p.clear);
        let removed: Vec<&str> = p.remove.iter().filter_map(|s| s.to_str()).collect();
        for n in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "SUB_BOT_TOKEN", "OPENAI_API_KEY_FILE", "AWS_SECRET", "GITHUB_TOKEN"] {
            assert!(removed.contains(&n), "{n} should be removed");
        }
        assert!(!removed.contains(&"AWS_REGION") && !removed.contains(&"PATH"));
    }

    #[test]
    fn allowlist_keeps_only_allowed() {
        let e = ShellEnv { mode: EnvMode::Allowlist, allow: vec!["CARGO_*".into(), "OPENAI_API_KEY".into()], deny: vec!["CARGO_REGISTRY_TOKEN".into()] };
        let p = e.plan(vars(&["PATH", "HOME", "LC_ALL", "CARGO_HOME", "CARGO_REGISTRY_TOKEN", "OPENAI_API_KEY", "RANDOM_VAR"]));
        assert!(p.clear);
        let kept: Vec<&str> = p.set.iter().filter_map(|(k, _)| k.to_str()).collect();
        assert_eq!(kept, ["PATH", "HOME", "LC_ALL", "CARGO_HOME"]);
    }

    #[test]
    fn anthropic_secrets_cannot_be_allowed() {
        let names = ["ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY_FILE"];
        for mode in [EnvMode::Inherit, EnvMode::Allowlist] {
            let policy = ShellEnv { mode, allow: vec!["ANTHROPIC_*".into()], deny: vec![] };
            let plan = policy.plan(vars(&names));
            assert!(plan.set.is_empty());
            if mode == EnvMode::Inherit {
                for name in names {
                    assert!(plan.remove.contains(&OsString::from(name)));
                }
            } else {
                assert!(plan.clear);
            }
        }
    }

    #[test]
    fn parses_config() {
        let e: ShellEnv = serde_json::from_str(r#"{"mode":"allowlist","allow":["CARGO_*"]}"#).unwrap();
        assert_eq!(e.mode, EnvMode::Allowlist);
        assert_eq!(serde_json::from_str::<ShellEnv>("{}").unwrap(), ShellEnv::default());
        assert!(serde_json::from_str::<ShellEnv>(r#"{"mod":"inherit"}"#).is_err());
    }
}
