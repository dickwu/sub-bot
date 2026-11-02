//! 密钥处理：
//! - `Secret`：内存中的密钥，`Debug`/`Display` 一律打码，析构时清零；
//! - `PROVIDER_KEYS`：各 provider 的 key 环境变量与 `*_FILE` 变量（新 provider 在这里登记）；
//! - `secret_env_vars()`：启动子进程（bash 工具）时必须剔除的环境变量；
//! - `load_account_key`：按 CLI 文件 > `*_FILE` 环境变量 > 配置声明（`apiKeyFile` | `apiKeyKeychain`）> 环境变量 > 受管存储 的顺序加载 key；
//! - 受管存储：`sub-bot keys set` 写入的位置（macOS：Keychain `sub-bot` / `<provider>/<account>`；其它：0600 key 文件）。
//!
//! 约定：任何错误信息、日志都只包含路径和来源，绝不包含 key 或文件内容。
use crate::keychain::{managed_account, KeychainError, KeychainReader};
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

/// 未指定账号时的账号名（向后兼容：现有配置/CLI/环境变量都作用于它）。
pub const DEFAULT_ACCOUNT: &str = "default";

/// 账号名规则：`^[A-Za-z0-9_.-]{1,64}$`（不能含 `/`：Keychain account 字段用 `/` 分隔；也保证可安全用作文件名）。
pub fn valid_account_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// 尽力清零一段明文字节（与 `Secret::drop` 同样做法）。
pub fn wipe_bytes(mut bytes: Vec<u8>) {
    for b in bytes.iter_mut() {
        // SAFETY: 写入的是本 Vec 自己的已初始化字节
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// key 文本校验失败的原因（不含内容）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTextError {
    Empty,
    /// trim 后内部仍有空白（多段内容）
    Whitespace,
}

/// 去掉首尾空白；必须非空且不含内部空白。文件、Keychain、stdin 共用。
pub fn validate_key_text(text: &str) -> Result<Secret, KeyTextError> {
    let key = text.trim();
    if key.is_empty() {
        return Err(KeyTextError::Empty);
    }
    if key.chars().any(char::is_whitespace) {
        return Err(KeyTextError::Whitespace);
    }
    Ok(Secret::new(key))
}

/// key 从哪里来的（只含路径、变量名和 Keychain 条目名，可以写日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    CliFile(PathBuf),
    EnvFile(&'static str, PathBuf),
    ConfigFile(PathBuf),
    /// 配置的 apiKeyEnv（账号级别），变量名运行时登记到 secret_env_vars
    AccountEnv(String),
    Env(&'static str),
    /// macOS Keychain generic password
    Keychain {
        service: String,
        account: String,
    },
    /// `sub-bot keys set` 写入的 0600 文件（Linux/非 Keychain 路径）
    ManagedFile(PathBuf),
}

impl fmt::Display for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeySource::CliFile(p) => write!(f, "CLI key file {}", p.display()),
            KeySource::EnvFile(var, p) => write!(f, "{var}={}", p.display()),
            KeySource::ConfigFile(p) => write!(f, "config apiKeyFile {}", p.display()),
            KeySource::AccountEnv(var) => write!(f, "env {var}"),
            KeySource::Env(var) => write!(f, "env {var}"),
            KeySource::Keychain { service, account } => write!(f, "keychain service={service} account={account}"),
            KeySource::ManagedFile(p) => write!(f, "managed key file {}", p.display()),
        }
    }
}

/// 读取 key 文件：去掉首尾空白；文件不可读、为空或包含多段内容时报错（错误信息不含文件内容）。
pub fn read_key_file(label: &str, path: &Path) -> Result<Secret, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {label} API key file {}: {e}", path.display()))?;
    let result = validate_key_text(&text);
    wipe_bytes(text.into_bytes());
    let key = result.map_err(|e| match e {
        KeyTextError::Empty => format!("{label} API key file {} is empty", path.display()),
        KeyTextError::Whitespace => format!("{label} API key file {} must contain only the key (found whitespace inside)", path.display()),
    })?;
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
    Ok(key)
}

/// 受管 key 文件目录：`<config_dir>/sub-bot/keys`（Linux 通常是 `~/.config/sub-bot/keys`，遵循 `$XDG_CONFIG_HOME`）。
/// 选 config 目录而非 data 目录：data 目录可能落到 /tmp 回退路径，key 不能放那里。
pub fn managed_key_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.config_dir().join("sub-bot").join("keys"))
}

/// `<root>/<provider>-<account>.key`；provider/账号名不合法时返回 None（防路径穿越）。
pub fn managed_key_path_in(root: &Path, provider: &str, account: &str) -> Option<PathBuf> {
    (provider_key(provider).is_some() && valid_account_name(account)).then(|| root.join(format!("{provider}-{account}.key")))
}

/// 受管 key 文件路径（见 [`managed_key_dir`]）。macOS 的受管存储是 Keychain，只有显式 `--file` 才写文件。
pub fn managed_key_path(provider: &str, account: &str) -> Option<PathBuf> {
    managed_key_path_in(&managed_key_dir()?, provider, account)
}

/// 受管存储（加载顺序的最后一级；`sub-bot keys set` 的默认写入位置）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedStore {
    /// 不探测（测试，或无法确定目录）
    None,
    /// macOS：Keychain `(service, "<provider>/<account>")`
    Keychain { service: String },
    /// 其它平台：`<dir>/<provider>-<account>.key`
    Dir(PathBuf),
}

impl ManagedStore {
    /// 当前平台的受管存储：macOS → Keychain `sub-bot`；其它 → [`managed_key_dir`]。
    pub fn system() -> Self {
        if crate::keychain::supported() {
            Self::Keychain { service: crate::keychain::DEFAULT_SERVICE.into() }
        } else {
            managed_key_dir().map_or(Self::None, Self::Dir)
        }
    }
}

/// 创建（递归，0700）并校验私有目录：必须是目录、不是符号链接、属于当前 euid、group/others 无权限。
/// 已存在的目录不改权限，不安全时直接报错。
pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).map_err(|e| format!("cannot create key directory {}: {e}", dir.display()))?;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("cannot inspect key directory {}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(format!("unsafe key directory {}: must be a directory, not a symlink", dir.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or memory preconditions.
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            return Err(format!("unsafe key directory {}: not owned by current euid {euid}", dir.display()));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(format!("unsafe key directory {}: accessible by group/others (run chmod 700)", dir.display()));
        }
    }
    Ok(())
}

/// 原子写 key 文件：父目录不存在时以 0700 创建；拒绝覆盖符号链接；先写同目录下 0600 临时文件再 rename。
/// 错误信息只含路径和系统错误，不含 key。
pub fn write_key_file(path: &Path, key: &Secret) -> Result<(), String> {
    use std::io::Write;
    let err = |e: &dyn fmt::Display| format!("cannot write key file {}: {e}", path.display());
    let name = path.file_name().ok_or_else(|| err(&"not a file path"))?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if std::fs::symlink_metadata(parent).is_err() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent).map_err(|e| err(&e))?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => return Err(err(&"refusing to overwrite a symlink")),
        Ok(m) if !m.is_file() => return Err(err(&"not a regular file")),
        _ => {}
    }
    let tmp = parent.join(format!(".{}.tmp-{}-{:08x}", name.to_string_lossy(), std::process::id(), rand::random::<u32>()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&tmp).and_then(|mut f| {
        f.write_all(key.expose().as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err(&e));
    }
    Ok(())
}

/// 一个账号的 key 来源声明。
#[derive(Debug, Clone)]
pub struct AccountKeySpec<'a> {
    pub pk: &'a ProviderKey,
    pub account: &'a str,
    /// 仅 default 账号：CLI --<provider>-key-file
    pub cli_file: Option<&'a Path>,
    /// 配置声明的来源（最多一个）：apiKeyFile
    pub config_file: Option<&'a Path>,
    /// 配置声明的来源（最多一个）：已 resolve 的 Keychain (service, account)
    pub keychain: Option<(String, String)>,
    /// 配置声明的来源（最多一个）：账号级环境变量名
    pub account_env: Option<&'a str>,
    /// 最后一级：受管存储
    pub managed: ManagedStore,
}

impl<'a> AccountKeySpec<'a> {
    pub fn new(pk: &'a ProviderKey, account: &'a str, managed: ManagedStore) -> Self {
        Self { pk, account, cli_file: None, config_file: None, keychain: None, account_env: None, managed }
    }

    fn is_default(&self) -> bool {
        self.account == DEFAULT_ACCOUNT
    }

    /// 除受管存储外是否声明/提供了来源（不访问文件或 Keychain）。
    pub fn has_explicit_source(&self, getenv: impl Fn(&str) -> Option<String>) -> bool {
        let env_set = self.is_default() && [self.pk.env, self.pk.file_env].iter().any(|var| getenv(var).is_some_and(|s| !s.trim().is_empty()));
        (self.is_default() && self.cli_file.is_some()) || self.config_file.is_some() || self.keychain.is_some() || self.account_env.is_some() || env_set
    }
}

/// key 加载失败（文案只含来源描述，不含 key）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyLoadError {
    /// 所有来源都未命中
    #[error("{0}")]
    NotFound(String),
    /// 来源存在但读取/校验失败
    #[error("{0}")]
    Source(String),
    #[error(transparent)]
    Keychain(#[from] KeychainError),
}

/// 加载一个账号的 key。取第一个命中的来源（“命中”= 来源存在且非空；来源存在但读取失败 ⇒ 直接 Err，不继续向下）：
///
/// default 账号：
///   1. cli_file → CliFile
///   2. `<P>_API_KEY_FILE` → EnvFile
///   3. 配置声明（config_file | keychain | account_env）→ ConfigFile | Keychain | AccountEnv
///   4. `<P>_API_KEY` → Env
///   5. 受管存储 → Keychain | ManagedFile
///
/// 其它账号只看 3 和 5（CLI 文件和 `<P>_API_KEY*` 属于 default）。
/// 受管存储只在前面都未命中时探测；Keychain `NotFound` 视为未命中，其它 Keychain 错误 ⇒ Err。
pub fn load_account_key(spec: &AccountKeySpec<'_>, getenv: impl Fn(&str) -> Option<String>, keychain: &dyn KeychainReader) -> Result<(Secret, KeySource), KeyLoadError> {
    let pk = spec.pk;
    let get = |k: &str| getenv(k).filter(|v| !v.trim().is_empty());
    let file = |p: &Path| read_key_file(pk.label, p).map_err(KeyLoadError::Source);
    if spec.is_default() {
        if let Some(p) = spec.cli_file {
            return Ok((file(p)?, KeySource::CliFile(p.to_path_buf())));
        }
        if let Some(p) = get(pk.file_env) {
            let p = PathBuf::from(p.trim());
            return Ok((file(&p)?, KeySource::EnvFile(pk.file_env, p)));
        }
    }
    if let Some(p) = spec.config_file {
        return Ok((file(p)?, KeySource::ConfigFile(p.to_path_buf())));
    }
    if let Some((service, account)) = &spec.keychain {
        let key = keychain.get(service, account)?;
        return Ok((key, KeySource::Keychain { service: service.clone(), account: account.clone() }));
    }
    if let Some(var) = spec.account_env {
        register_secret_var(var);
        let value = get(var).ok_or_else(|| KeyLoadError::Source(format!("{} API key env {var} is not set", pk.label)))?;
        let key = validate_key_text(&value).map_err(|_| KeyLoadError::Source(format!("{} API key env {var} must contain only the key", pk.label)))?;
        return Ok((key, KeySource::AccountEnv(var.to_string())));
    }
    if spec.is_default() {
        if let Some(v) = get(pk.env) {
            return Ok((Secret::new(v.trim()), KeySource::Env(pk.env)));
        }
    }
    match &spec.managed {
        ManagedStore::Keychain { service } => {
            let account = managed_account(pk.provider, spec.account);
            match keychain.get(service, &account) {
                Ok(key) => return Ok((key, KeySource::Keychain { service: service.clone(), account })),
                Err(e) if e.is_not_found() || e == KeychainError::Unsupported => {}
                Err(e) => return Err(e.into()),
            }
        }
        ManagedStore::Dir(dir) => {
            if let Some(p) = managed_key_path_in(dir, pk.provider, spec.account).filter(|p| std::fs::symlink_metadata(p).is_ok()) {
                return Ok((file(&p)?, KeySource::ManagedFile(p)));
            }
        }
        ManagedStore::None => {}
    }
    let (label, id, account) = (pk.label, pk.provider, spec.account);
    Err(KeyLoadError::NotFound(if spec.is_default() {
        format!(
            "{label} API key not found for account '{account}': use --{id}-key-file, {}, providers.{id}.apiKeyFile / apiKeyKeychain, {}, or `sub-bot keys set --provider {id}`",
            pk.file_env, pk.env
        )
    } else {
        format!("{label} API key not found for account '{account}': set providers.{id}.accounts.{account}.apiKeyFile / apiKeyKeychain / apiKeyEnv, or run `sub-bot keys set --provider {id} --account {account}`")
    }))
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

    fn spec<'a>(account: &'a str) -> AccountKeySpec<'a> {
        AccountKeySpec::new(provider_key("openai").unwrap(), account, ManagedStore::None)
    }

    fn no_keychain() -> crate::keychain::mock::MockKeychain {
        Default::default()
    }

    #[test]
    fn load_account_key_precedence_default_account() {
        use crate::keychain::mock::MockKeychain;
        let f1 = tmp("cli.key", "  sk-from-cli-file \n");
        let f2 = tmp("env.key", "sk-from-env-file\n");
        let f3 = tmp("cfg.key", "sk-from-config-file");
        let managed_root = std::env::temp_dir().join(format!("sub-bot-managed-{}", std::process::id()));
        std::fs::create_dir_all(&managed_root).unwrap();
        std::fs::write(managed_root.join("openai-default.key"), "sk-from-managed-file\n").unwrap();
        let kc = MockKeychain::with([(("sub-bot", "openai/default"), Ok("sk-from-keychain")), (("my", "item"), Ok(" sk-from-config-keychain\n"))]);
        let env = |k: &str| match k {
            "OPENAI_API_KEY_FILE" => Some(f2.display().to_string()),
            "OPENAI_API_KEY" => Some("sk-from-env".into()),
            _ => None,
        };
        let only_env = |k: &str| (k == "OPENAI_API_KEY").then(|| "sk-from-env".to_string());
        let mut s = spec("default");
        s.cli_file = Some(&f1);
        s.config_file = Some(&f3);
        s.managed = ManagedStore::Keychain { service: "sub-bot".into() };
        let (k, src) = load_account_key(&s, env, &kc).unwrap();
        assert_eq!((k.expose(), src), ("sk-from-cli-file", KeySource::CliFile(f1.clone())));
        s.cli_file = None;
        assert_eq!(load_account_key(&s, env, &kc).unwrap().0.expose(), "sk-from-env-file");
        assert_eq!(load_account_key(&s, only_env, &kc).unwrap().0.expose(), "sk-from-config-file");
        // 配置声明的 keychain 与 apiKeyFile 同级（validate 保证最多一个）
        s.config_file = None;
        s.keychain = Some(("my".into(), "item".into()));
        let (k, src) = load_account_key(&s, only_env, &kc).unwrap();
        assert_eq!((k.expose(), src), ("sk-from-config-keychain", KeySource::Keychain { service: "my".into(), account: "item".into() }));
        s.keychain = None;
        let (k, src) = load_account_key(&s, only_env, &kc).unwrap();
        assert_eq!((k.expose(), src), ("sk-from-env", KeySource::Env("OPENAI_API_KEY")));
        // 第 5 级：受管 Keychain / 受管文件
        let (k, src) = load_account_key(&s, |_| None, &kc).unwrap();
        assert_eq!((k.expose(), src), ("sk-from-keychain", KeySource::Keychain { service: "sub-bot".into(), account: "openai/default".into() }));
        s.managed = ManagedStore::Dir(managed_root.clone());
        let (k, src) = load_account_key(&s, |_| None, &kc).unwrap();
        assert_eq!((k.expose(), src), ("sk-from-managed-file", KeySource::ManagedFile(managed_root.join("openai-default.key"))));
        // 空的 *_FILE 变量视为未设置
        let empty_file_env = |k: &str| match k {
            "OPENAI_API_KEY_FILE" => Some("  ".into()),
            "OPENAI_API_KEY" => Some("sk-from-env".into()),
            _ => None,
        };
        assert_eq!(load_account_key(&spec("default"), empty_file_env, &kc).unwrap().0.expose(), "sk-from-env");
        let e = load_account_key(&spec("default"), |_| None, &kc).unwrap_err();
        assert!(matches!(e, KeyLoadError::NotFound(_)), "{e}");
        assert!(e.to_string().contains("OpenAI API key not found for account 'default'") && e.to_string().contains("sub-bot keys set --provider openai"), "{e}");
        let _ = std::fs::remove_dir_all(&managed_root);
    }

    #[test]
    fn load_account_key_named_account_ignores_default_sources() {
        let f1 = tmp("named-cli.key", "sk-from-cli-file");
        let env = |k: &str| match k {
            "OPENAI_API_KEY" => Some("sk-from-env".into()),
            _ => None,
        };
        let mut s = spec("work");
        s.cli_file = Some(&f1);
        let e = load_account_key(&s, env, &no_keychain()).unwrap_err();
        assert_eq!(
            e.to_string(),
            "OpenAI API key not found for account 'work': set providers.openai.accounts.work.apiKeyFile / apiKeyKeychain / apiKeyEnv, or run `sub-bot keys set --provider openai --account work`"
        );
        assert!(!s.has_explicit_source(env));
        s.keychain = Some(("sub-bot".into(), "openai/work".into()));
        assert!(s.has_explicit_source(env));
    }

    #[test]
    fn keychain_errors_never_include_contents() {
        use crate::keychain::{mock::MockKeychain, KeychainError};
        let kc = MockKeychain::with([
            (("sub-bot", "openai/denied"), Err(KeychainError::Denied { service: "sub-bot".into(), account: "openai/denied".into() })),
            (("sub-bot", "openai/locked"), Err(KeychainError::InteractionNotAllowed { service: "sub-bot".into(), account: "openai/locked".into() })),
            (("sub-bot", "openai/other"), Err(KeychainError::Other { code: -1, service: "sub-bot".into(), account: "openai/other".into() })),
            (("sub-bot", "openai/multi"), Ok("sk-secret-one sk-secret-two")),
        ]);
        for (account, needle) in
            [("openai/denied", "access denied"), ("openai/locked", "interaction not allowed"), ("openai/other", "keychain error -1"), ("openai/multi", "not valid UTF-8 or is empty")]
        {
            let mut s = spec("default");
            s.keychain = Some(("sub-bot".into(), account.into()));
            let e = load_account_key(&s, |k| (k == "OPENAI_API_KEY").then(|| "sk-secret-env".into()), &kc).unwrap_err();
            assert!(matches!(e, KeyLoadError::Keychain(_)));
            let e = e.to_string();
            assert!(e.contains(needle) && e.contains(account) && e.contains("service=sub-bot") && !e.contains("sk-secret"), "{e}");
        }
        // 第 3 级（配置声明）NotFound ⇒ Err，不回落到环境变量
        let mut s = spec("default");
        s.keychain = Some(("sub-bot".into(), "openai/missing".into()));
        let e = load_account_key(&s, |k| (k == "OPENAI_API_KEY").then(|| "sk-x".into()), &kc).unwrap_err();
        assert!(e.to_string().contains("keychain item not found (service=sub-bot, account=openai/missing)"), "{e}");
        // 第 5 级 NotFound ⇒ 继续（最终 not found），其它错误 ⇒ Err
        let mut s = spec("missing");
        s.managed = ManagedStore::Keychain { service: "sub-bot".into() };
        assert!(matches!(load_account_key(&s, |_| None, &kc), Err(KeyLoadError::NotFound(_))));
        let s = AccountKeySpec { account: "locked", ..s };
        assert!(matches!(load_account_key(&s, |_| None, &kc), Err(KeyLoadError::Keychain(KeychainError::InteractionNotAllowed { .. }))));
    }

    #[test]
    fn key_source_display_has_no_secret() {
        let cases = [
            (KeySource::Keychain { service: "sub-bot".into(), account: "openai/work".into() }, "keychain service=sub-bot account=openai/work"),
            (KeySource::ManagedFile("/k/openai-default.key".into()), "managed key file /k/openai-default.key"),
            (KeySource::AccountEnv("OPENAI_API_KEY_PROXY".into()), "env OPENAI_API_KEY_PROXY"),
            (KeySource::Env("OPENAI_API_KEY"), "env OPENAI_API_KEY"),
            (KeySource::ConfigFile("/etc/k".into()), "config apiKeyFile /etc/k"),
        ];
        for (src, text) in cases {
            assert_eq!(src.to_string(), text);
        }
    }

    #[test]
    fn account_env_is_registered_for_subprocess_stripping() {
        let mut s = spec("proxy");
        s.account_env = Some("OPENAI_API_KEY_PROXY_TEST");
        let (k, src) = load_account_key(&s, |k| (k == "OPENAI_API_KEY_PROXY_TEST").then(|| " sk-proxy ".into()), &no_keychain()).unwrap();
        assert_eq!((k.expose(), src), ("sk-proxy", KeySource::AccountEnv("OPENAI_API_KEY_PROXY_TEST".into())));
        assert!(secret_env_vars().iter().any(|v| v == "OPENAI_API_KEY_PROXY_TEST"));
        assert!(load_account_key(&s, |_| None, &no_keychain()).unwrap_err().to_string().contains("OPENAI_API_KEY_PROXY_TEST is not set"));
    }

    #[test]
    fn account_names_and_managed_paths() {
        for ok in ["default", "work", "a.b-c_d", &"x".repeat(64)] {
            assert!(valid_account_name(ok), "{ok}");
        }
        for bad in ["", "a/b", "a b", "../x", &"x".repeat(65), "é"] {
            assert!(!valid_account_name(bad), "{bad}");
        }
        let root = Path::new("/r");
        assert_eq!(managed_key_path_in(root, "openai", "work"), Some(PathBuf::from("/r/openai-work.key")));
        assert_eq!(managed_key_path_in(root, "openai", "../x"), None);
        assert_eq!(managed_key_path_in(root, "faux", "default"), None);
        assert_eq!(validate_key_text(" k \n").unwrap().expose(), "k");
        assert_eq!(validate_key_text(" \n").unwrap_err(), KeyTextError::Empty);
        assert_eq!(validate_key_text("a b").unwrap_err(), KeyTextError::Whitespace);
    }

    #[cfg(unix)]
    #[test]
    fn write_key_file_sets_0600_and_private_dir() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("sub-bot-write-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("cfg/sub-bot/keys");
        ensure_private_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let path = dir.join("openai-default.key");
        write_key_file(&path, &Secret::new("sk-first")).unwrap();
        write_key_file(&path, &Secret::new("sk-second")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sk-second\n");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temp files left behind");
        // 新建的父目录是 0700
        let nested = base.join("new/dir/k.key");
        write_key_file(&nested, &Secret::new("sk-x")).unwrap();
        assert_eq!(std::fs::metadata(base.join("new/dir")).unwrap().permissions().mode() & 0o777, 0o700);
        // 拒绝 symlink 目录 / 宽松权限目录 / 覆盖 symlink 文件
        let link = base.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(ensure_private_dir(&link).unwrap_err().contains("not a symlink"));
        let loose = base.join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_dir(&loose).unwrap_err().contains("group/others"));
        let file_link = base.join("file-link.key");
        std::os::unix::fs::symlink(&path, &file_link).unwrap();
        let e = write_key_file(&file_link, &Secret::new("sk-should-not-appear")).unwrap_err();
        assert!(e.contains("symlink") && !e.contains("sk-should-not-appear"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sk-second\n");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn key_file_errors_never_include_contents() {
        let missing = std::env::temp_dir().join("sub-bot-definitely-missing.key");
        let mut s = spec("default");
        s.cli_file = Some(&missing);
        let e = load_account_key(&s, |_| None, &no_keychain()).unwrap_err().to_string();
        assert!(e.contains("cannot read OpenAI API key file") && e.contains("sub-bot-definitely-missing.key"), "{e}");
        let empty = tmp("empty.key", " \n\t");
        s.cli_file = Some(&empty);
        assert!(load_account_key(&s, |_| None, &no_keychain()).unwrap_err().to_string().contains("is empty"));
        let multi = tmp("multi.key", "sk-secret-part-one sk-secret-part-two\n");
        s.cli_file = Some(&multi);
        let e = load_account_key(&s, |_| None, &no_keychain()).unwrap_err().to_string();
        assert!(e.contains("only the key") && !e.contains("sk-secret"), "{e}");
    }
}
