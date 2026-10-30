//! 密钥处理：
//! - `Secret`：内存中的密钥，`Debug`/`Display` 一律打码，析构时清零；
//! - `PROVIDER_KEYS`：各 provider 的 key 环境变量与 `*_FILE` 变量（新 provider 在这里登记）；
//! - `secret_env_vars()`：启动子进程（bash 工具）时必须剔除的环境变量；
//! - `load_provider_key`：按 CLI 文件 > `*_FILE` 环境变量 > 配置 `apiKeyFile` > 环境变量 的顺序加载 key。
//!
//! 约定：任何错误信息、日志都只包含路径和来源，绝不包含 key 或文件内容。
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::RwLock,
};

/// 内存中的密钥。不实现 `Serialize`，`Debug`/`Display` 输出 `[redacted]`。
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 取出明文，仅在真正发请求时使用（如 `bearer_auth`）。
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // 写 0 仍是合法 UTF-8；volatile 防止被优化掉（尽力而为，不保证没有其它副本）
        // SAFETY: 只把字节改成 0，String 仍保持合法 UTF-8
        unsafe {
            for b in self.0.as_mut_vec().iter_mut() {
                std::ptr::write_volatile(b, 0);
            }
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// 一个 provider 的 key 来源。
#[derive(Debug, Clone, Copy)]
pub struct ProviderKey {
    pub provider: &'static str,
    /// 人类可读名称，用于错误信息
    pub label: &'static str,
    /// 直接存放 key 的环境变量
    pub env: &'static str,
    /// 存放 key 文件路径的环境变量
    pub file_env: &'static str,
}

/// 已知 provider 的 key 变量。新增 provider 时在这里登记，子进程会自动剔除这些变量。
pub const PROVIDER_KEYS: &[ProviderKey] = &[
    ProviderKey { provider: "openai", label: "OpenAI", env: "OPENAI_API_KEY", file_env: "OPENAI_API_KEY_FILE" },
    ProviderKey { provider: "anthropic", label: "Anthropic", env: "ANTHROPIC_API_KEY", file_env: "ANTHROPIC_API_KEY_FILE" },
];

/// sub-bot 自身的其它密钥变量。
pub const SERVICE_SECRET_VARS: &[&str] = &["SUB_BOT_TOKEN", "SUB_BOT_TOKEN_FILE"];

static EXTRA_SECRET_VARS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// 运行时登记额外的密钥变量（例如插件 provider），之后启动的子进程都会剔除它。
pub fn register_secret_var(name: &str) {
    let mut v = EXTRA_SECRET_VARS.write().unwrap_or_else(|e| e.into_inner());
    if !v.iter().any(|n| n == name) {
        v.push(name.to_string());
    }
}

/// 子进程必须剔除的全部环境变量名。
pub fn secret_env_vars() -> Vec<String> {
    let mut v: Vec<String> = PROVIDER_KEYS.iter().flat_map(|k| [k.env.to_string(), k.file_env.to_string()]).collect();
    v.extend(SERVICE_SECRET_VARS.iter().map(|s| s.to_string()));
    v.extend(EXTRA_SECRET_VARS.read().unwrap_or_else(|e| e.into_inner()).iter().cloned());
    v
}

pub fn provider_key(provider: &str) -> Option<&'static ProviderKey> {
    PROVIDER_KEYS.iter().find(|k| k.provider == provider)
}

/// key 从哪里来的（只含路径和变量名，可以写日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    CliFile(PathBuf),
    EnvFile(&'static str, PathBuf),
    ConfigFile(PathBuf),
    Env(&'static str),
}

impl fmt::Display for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeySource::CliFile(p) => write!(f, "CLI key file {}", p.display()),
            KeySource::EnvFile(var, p) => write!(f, "{var}={}", p.display()),
            KeySource::ConfigFile(p) => write!(f, "config apiKeyFile {}", p.display()),
            KeySource::Env(var) => write!(f, "env {var}"),
        }
    }
}

/// 读取 key 文件：去掉首尾空白；文件不可读、为空或包含多段内容时报错（错误信息不含文件内容）。
pub fn read_key_file(label: &str, path: &Path) -> Result<Secret, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {label} API key file {}: {e}", path.display()))?;
    let key = text.trim();
    if key.is_empty() {
        return Err(format!("{label} API key file {} is empty", path.display()));
    }
    if key.chars().any(char::is_whitespace) {
        return Err(format!("{label} API key file {} must contain only the key (found whitespace inside)", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(path) {
            let mode = m.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                tracing::warn!(path = %path.display(), mode = format!("{mode:o}"), "API key file is readable by group/others; consider chmod 600");
            }
        }
    }
    Ok(Secret::new(key))
}

/// 加载 provider key。优先级：`--<provider>-key-file` > `<P>_API_KEY_FILE` > 配置 `apiKeyFile` > `<P>_API_KEY`。
/// 空字符串视为未设置。`getenv` 便于测试注入。
pub fn load_provider_key(pk: &ProviderKey, cli_file: Option<&Path>, config_file: Option<&Path>, getenv: impl Fn(&str) -> Option<String>) -> Result<(Secret, KeySource), String> {
    let get = |k: &str| getenv(k).filter(|v| !v.trim().is_empty());
    if let Some(p) = cli_file {
        return Ok((read_key_file(pk.label, p)?, KeySource::CliFile(p.to_path_buf())));
    }
    if let Some(p) = get(pk.file_env) {
        let p = PathBuf::from(p.trim());
        return Ok((read_key_file(pk.label, &p)?, KeySource::EnvFile(pk.file_env, p)));
    }
    if let Some(p) = config_file {
        return Ok((read_key_file(pk.label, p)?, KeySource::ConfigFile(p.to_path_buf())));
    }
    if let Some(v) = get(pk.env) {
        return Ok((Secret::new(v.trim()), KeySource::Env(pk.env)));
    }
    Err(format!("{} API key not found: use --{}-key-file, {}, config apiKeyFile, or {}", pk.label, pk.provider, pk.file_env, pk.env))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sub-bot-secrets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn secret_is_redacted() {
        let s = Secret::new("sk-test-not-real");
        assert_eq!(format!("{s:?}"), "Secret([redacted])");
        assert_eq!(format!("{s}"), "[redacted]");
        assert_eq!(s.expose(), "sk-test-not-real");
    }

    #[test]
    fn deny_list_covers_known_vars() {
        register_secret_var("EXTRA_TEST_SECRET");
        let v = secret_env_vars();
        for n in ["OPENAI_API_KEY", "OPENAI_API_KEY_FILE", "ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY_FILE", "SUB_BOT_TOKEN", "SUB_BOT_TOKEN_FILE", "EXTRA_TEST_SECRET"] {
            assert!(v.iter().any(|x| x == n), "{n}");
        }
    }

    #[test]
    fn loads_key_file_with_precedence() {
        let pk = provider_key("openai").unwrap();
        let f1 = tmp("cli.key", "  sk-from-cli-file \n");
        let f2 = tmp("env.key", "sk-from-env-file\n");
        let f3 = tmp("cfg.key", "sk-from-config-file");
        let env = |k: &str| match k {
            "OPENAI_API_KEY_FILE" => Some(f2.display().to_string()),
            "OPENAI_API_KEY" => Some("sk-from-env".into()),
            _ => None,
        };
        let (k, src) = load_provider_key(pk, Some(&f1), Some(&f3), env).unwrap();
        assert_eq!(k.expose(), "sk-from-cli-file");
        assert_eq!(src, KeySource::CliFile(f1.clone()));
        let (k, _) = load_provider_key(pk, None, Some(&f3), env).unwrap();
        assert_eq!(k.expose(), "sk-from-env-file");
        let only_env = |k: &str| (k == "OPENAI_API_KEY").then(|| "sk-from-env".to_string());
        let (k, _) = load_provider_key(pk, None, Some(&f3), only_env).unwrap();
        assert_eq!(k.expose(), "sk-from-config-file");
        let (k, src) = load_provider_key(pk, None, None, only_env).unwrap();
        assert_eq!((k.expose(), src), ("sk-from-env", KeySource::Env("OPENAI_API_KEY")));
        // 空的 *_FILE 变量视为未设置
        let empty_file_env = |k: &str| match k {
            "OPENAI_API_KEY_FILE" => Some("  ".into()),
            "OPENAI_API_KEY" => Some("sk-from-env".into()),
            _ => None,
        };
        assert_eq!(load_provider_key(pk, None, None, empty_file_env).unwrap().0.expose(), "sk-from-env");
        assert!(load_provider_key(pk, None, None, |_| None).unwrap_err().contains("not found"));
    }

    #[test]
    fn key_file_errors_never_include_contents() {
        let pk = provider_key("openai").unwrap();
        let missing = std::env::temp_dir().join("sub-bot-definitely-missing.key");
        let e = load_provider_key(pk, Some(&missing), None, |_| None).unwrap_err();
        assert!(e.contains("cannot read OpenAI API key file") && e.contains("sub-bot-definitely-missing.key"), "{e}");
        let empty = tmp("empty.key", " \n\t");
        assert!(load_provider_key(pk, Some(&empty), None, |_| None).unwrap_err().contains("is empty"));
        let multi = tmp("multi.key", "sk-secret-part-one sk-secret-part-two\n");
        let e = load_provider_key(pk, Some(&multi), None, |_| None).unwrap_err();
        assert!(e.contains("only the key") && !e.contains("sk-secret"), "{e}");
    }
}
