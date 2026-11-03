//! `sub-bot keys set|list|delete|check|reauth`：管理 provider API key 的受管存储。
//!
//! - macOS：Keychain generic password，service `sub-bot`、account `<provider>/<account>`（v0.10.3 起经 `/usr/bin/security` 读写，
//!   brew upgrade 后无需重新授权；`reauth` 一次性迁移旧版本创建的条目）；
//! - 其它平台（或 `--file`）：0600 key 文件（受管目录 `~/.config/sub-bot/keys`，0700）。
//!
//! 约定：任何输出（stdout/stderr/JSON/错误）都只包含来源描述（路径、变量名、service/account 名），绝不包含 key。
//! 不构造 Hub；`list` 只解析配置里与 key 有关的只读部分，`check` 需要完整 `Config::load`。
use crate::config::{Config, KeychainRef};
use crate::hub::KeyFiles;
use agent_ai::keychain::{self, KeychainError, KeychainReader, SystemKeychain};
use agent_ai::secrets::{
    ensure_private_dir, load_account_key, managed_key_dir, managed_key_path_in, provider_key, valid_account_name, validate_key_text, wipe_bytes, write_key_file, KeyLoadError, KeySource, KeyTextError,
    ManagedStore, ProviderKey, Secret, DEFAULT_ACCOUNT, PROVIDER_KEYS,
};
use clap::Subcommand;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// provider 用全局 `--provider` 选择（set/delete 必填；list/check 作为筛选）。
// 会话账号的 `--account` 是 run/rpc/ask/models/probe-efforts 各自的参数（不是全局参数），因此这里的 `--account` 不冲突，
// 也不读取 SUB_BOT_ACCOUNT（避免环境变量悄悄改变 set/delete 的目标）。
#[derive(Subcommand, Debug)]
pub enum KeysCmd {
    /// 从 stdin 读取 key 并保存（macOS: Keychain；Linux 或 --file: 0600 文件）。存在则覆盖。
    ///
    /// stdin 是终端时无回显输入；否则读取到 EOF（例如 `pbpaste | sub-bot keys set --provider openai`）。
    Set {
        #[arg(long, default_value = DEFAULT_ACCOUNT)]
        account: String,
        /// 写到指定文件而不是 Keychain/默认目录
        #[arg(long)]
        file: Option<PathBuf>,
        /// Keychain service（默认 sub-bot）；非 macOS 上指定则报错
        #[arg(long)]
        service: Option<String>,
        /// 不打印配置片段
        #[arg(long)]
        quiet: bool,
    },
    /// 列出已知 key 来源及是否存在，绝不输出 key
    List {
        #[arg(long)]
        json: bool,
    },
    /// 删除受管存储里的 key（或 --file 指定的文件）
    Delete {
        #[arg(long, default_value = DEFAULT_ACCOUNT)]
        account: String,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        service: Option<String>,
    },
    /// 按当前配置尝试加载每个将被使用的 key（触发 Keychain 授权），只报告成功/失败与来源
    Check,
    /// 一次性迁移：把旧版本 sub-bot（<= 0.10.2，ACL 绑定旧二进制）创建的 Keychain 条目重建为 /usr/bin/security 拥有的条目，之后 brew upgrade 无需任何操作（仅 macOS）
    ///
    /// 目标：service sub-bot（或 --service）下的受管条目 + 配置里的 apiKeyKeychain 条目（--config），可用全局 --provider / --account 筛选。
    /// 每个条目读取一次（旧条目会弹一次授权对话框，点“始终允许”），删除后重建并读回校验。任一失败 ⇒ 非零退出码。
    Reauth {
        /// 只迁移该账号（需配合 --provider 更精确）
        #[arg(long)]
        account: Option<String>,
        /// Keychain service（默认 sub-bot）
        #[arg(long)]
        service: Option<String>,
        /// 不读旧条目，改从环境变量取 key（OPENAI_API_KEY / ANTHROPIC_API_KEY；命名账号用其 apiKeyEnv）：零弹窗
        #[arg(long)]
        from_env: bool,
        /// 只列出目标和将要做的事，不读也不改任何条目
        #[arg(long)]
        dry_run: bool,
        /// 输出 JSON 数组（每项 provider / account / service / item / status / source / error）
        #[arg(long)]
        json: bool,
    },
}

/// Keychain 读写（生产：系统 Keychain；测试：mock）。
pub trait KeychainStore: KeychainReader {
    fn set(&self, service: &str, account: &str, secret: &Secret) -> Result<(), KeychainError>;
    fn delete(&self, service: &str, account: &str) -> Result<(), KeychainError>;
    fn list(&self, service: &str) -> Result<Vec<String>, KeychainError>;
}

impl KeychainStore for SystemKeychain {
    fn set(&self, service: &str, account: &str, secret: &Secret) -> Result<(), KeychainError> {
        keychain::set_generic(service, account, secret)
    }
    fn delete(&self, service: &str, account: &str) -> Result<(), KeychainError> {
        keychain::delete_generic(service, account)
    }
    fn list(&self, service: &str) -> Result<Vec<String>, KeychainError> {
        keychain::list_accounts(service)
    }
}

/// 读取（`get` / `list`）限时的 Keychain 存储（见 [`keychain::with_timeout`]）：`keys check` / `keys list` 不会因为
/// 无人响应的授权对话框而挂起；超过 grace 仍未返回 ⇒ 调用 `on_wait`（CLI 里把提示打到 stderr）。
/// `set` / `delete` 直接透传（不限时：写入超时后结果不确定）。
pub struct TimedStore {
    inner: Arc<dyn KeychainStore>,
    timeout: Duration,
    grace: Duration,
    on_wait: keychain::WaitHook,
}

impl TimedStore {
    pub fn new(inner: Arc<dyn KeychainStore>, timeout: Duration, grace: Duration, on_wait: impl Fn(&str, &str) + Send + Sync + 'static) -> Self {
        Self { inner, timeout, grace, on_wait: Box::new(on_wait) }
    }
}

impl KeychainReader for TimedStore {
    fn get(&self, service: &str, account: &str) -> Result<Secret, KeychainError> {
        let (inner, s, a) = (self.inner.clone(), service.to_string(), account.to_string());
        keychain::with_timeout(service, account, self.timeout, self.grace, || (self.on_wait)(service, account), move || inner.get(&s, &a))
    }
}

impl KeychainStore for TimedStore {
    fn set(&self, service: &str, account: &str, secret: &Secret) -> Result<(), KeychainError> {
        self.inner.set(service, account, secret)
    }
    fn delete(&self, service: &str, account: &str) -> Result<(), KeychainError> {
        self.inner.delete(service, account)
    }
    fn list(&self, service: &str) -> Result<Vec<String>, KeychainError> {
        let (inner, s) = (self.inner.clone(), service.to_string());
        keychain::with_timeout(service, "*", self.timeout, self.grace, || (self.on_wait)(service, "*"), move || inner.list(&s))
    }
}

/// 存储环境（可注入，便于在 Linux 上测试 Keychain 分支）。
pub struct Stores<'a> {
    /// 受管存储是否为 Keychain（= 当前构建是否为 macOS）
    pub macos: bool,
    /// 非 Keychain 平台的受管 key 文件目录
    pub managed_dir: Option<PathBuf>,
    pub keychain: &'a dyn KeychainStore,
}

impl Stores<'_> {
    fn managed(&self) -> ManagedStore {
        if self.macos {
            ManagedStore::Keychain { service: keychain::DEFAULT_SERVICE.into() }
        } else {
            self.managed_dir.clone().map_or(ManagedStore::None, ManagedStore::Dir)
        }
    }
}

/// key 的保存位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Keychain {
        service: String,
        account: String,
    },
    /// `managed`：受管目录（写入前校验目录私有）
    File {
        path: PathBuf,
        managed: bool,
    },
}

/// 选择存储目标：`--file` > macOS Keychain > 受管 key 文件。
pub fn storage_target(provider: &str, account: &str, file: Option<&Path>, service: Option<&str>, stores: &Stores<'_>) -> anyhow::Result<Target> {
    anyhow::ensure!(provider_key(provider).is_some(), "unknown provider '{provider}' (expected one of: {})", PROVIDER_KEYS.iter().map(|k| k.provider).collect::<Vec<_>>().join(", "));
    anyhow::ensure!(valid_account_name(account), "invalid account name '{account}' (allowed: [A-Za-z0-9_.-]{{1,64}})");
    if let Some(service) = service {
        anyhow::ensure!(stores.macos, "--service is only supported on macOS");
        anyhow::ensure!(file.is_none(), "--service cannot be combined with --file");
        anyhow::ensure!(!service.trim().is_empty(), "--service must not be blank");
    }
    if let Some(path) = file {
        return Ok(Target::File { path: std::path::absolute(path)?, managed: false });
    }
    if stores.macos {
        return Ok(Target::Keychain { service: service.unwrap_or(keychain::DEFAULT_SERVICE).into(), account: keychain::managed_account(provider, account) });
    }
    let dir = stores.managed_dir.as_deref().ok_or_else(|| anyhow::anyhow!("cannot determine the config directory for managed key files; use --file"))?;
    let path = managed_key_path_in(dir, provider, account).expect("validated provider and account");
    Ok(Target::File { path, managed: true })
}

fn text_error(e: KeyTextError) -> anyhow::Error {
    match e {
        KeyTextError::Empty => anyhow::anyhow!("no key provided"),
        KeyTextError::Whitespace => anyhow::anyhow!("stdin must contain only the key"),
    }
}

/// 非 TTY：读到 EOF，trim 后必须恰好一段无空白内容。读入的缓冲区用完即清零；错误不含内容。
pub fn read_key_from_reader(mut reader: impl Read) -> anyhow::Result<Secret> {
    let mut buf = Vec::new();
    let read = reader.read_to_end(&mut buf);
    let result = match read {
        Ok(_) => std::str::from_utf8(&buf).map_err(|_| anyhow::anyhow!("stdin must contain only the key")).and_then(|text| validate_key_text(text).map_err(text_error)),
        Err(e) => Err(anyhow::anyhow!("cannot read stdin: {e}")),
    };
    wipe_bytes(buf);
    result
}

/// TTY：关闭回显读取一行（自实现 termios，不引入 rpassword）。
///
/// 同时关闭 ICANON/ISIG：自己处理退格、Ctrl-U、Ctrl-C、Ctrl-D，保证任何退出路径（包括 Ctrl-C）都经过
/// `Restore::drop` 恢复终端设置；也不受 macOS 规范模式 1024 字节行长限制。
fn read_hidden_line() -> anyhow::Result<Secret> {
    struct Restore(libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: 恢复之前由 tcgetattr 取得的合法 termios
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.0) };
        }
    }
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr 成功时完整初始化 termios
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: 上面 tcgetattr 已成功
    let original = unsafe { original.assume_init() };
    let mut hidden = original;
    hidden.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
    hidden.c_cc[libc::VMIN] = 1;
    hidden.c_cc[libc::VTIME] = 0;
    let guard = Restore(original);
    // SAFETY: hidden 是由合法 termios 拷贝修改而来
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &hidden) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut byte = 0u8;
    let outcome = loop {
        // SAFETY: 读 1 字节到栈上变量；直接读 fd 避免 std 的 stdin 缓冲留下明文副本
        let n = unsafe { libc::read(libc::STDIN_FILENO, (&mut byte as *mut u8).cast(), 1) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break Err(anyhow::Error::from(e));
        }
        match (n, byte) {
            (0, _) | (_, b'\n' | b'\r') => break Ok(()),
            (_, 0x03) => break Err(anyhow::anyhow!("cancelled")),
            (_, 0x04) if buf.is_empty() => break Ok(()),
            (_, 0x7f | 0x08) => {
                if let Some(b) = buf.last_mut() {
                    *b = 0;
                }
                buf.pop();
            }
            (_, 0x15) => {
                buf.iter_mut().for_each(|b| *b = 0);
                buf.clear();
            }
            (_, b) => buf.push(b),
        }
    };
    drop(guard);
    eprintln!();
    let result = outcome.and_then(|()| read_key_from_reader(buf.as_slice()));
    wipe_bytes(buf);
    result
}

fn read_key(pk: &ProviderKey, account: &str) -> anyhow::Result<Secret> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        eprint!("Enter {} API key for account '{account}' (input hidden): ", pk.label);
        read_hidden_line()
    } else {
        read_key_from_reader(std::io::stdin().lock())
    }
}

fn quoted(s: &str) -> String {
    serde_json::Value::from(s).to_string()
}

/// 可粘贴到配置文件的 JSON 片段（不含 key）。
fn config_snippet(provider: &str, account: &str, target: &Target) -> String {
    let source = match target {
        Target::Keychain { service, .. } if service == keychain::DEFAULT_SERVICE => r#""apiKeyKeychain": {}"#.to_string(),
        Target::Keychain { service, .. } => format!(r#""apiKeyKeychain": {{"service": {}}}"#, quoted(service)),
        Target::File { path, .. } => format!(r#""apiKeyFile": {}"#, quoted(&path.to_string_lossy())),
    };
    if account == DEFAULT_ACCOUNT {
        format!(r#"  "providers": {{ "{provider}": {{ {source} }} }}"#)
    } else {
        format!(r#"  "providers": {{ "{provider}": {{ "accounts": {{ "{account}": {{ {source} }} }} }} }}"#)
    }
}

fn describe(target: &Target) -> String {
    match target {
        Target::Keychain { service, account } => format!("keychain (service={service}, account={account})"),
        Target::File { path, .. } => path.display().to_string(),
    }
}

/// 保存 key 到目标位置并打印结果。
pub fn set(stores: &Stores<'_>, pk: &ProviderKey, account: &str, target: &Target, key: &Secret, quiet: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    match target {
        Target::Keychain { service, account: item } => stores.keychain.set(service, item, key)?,
        Target::File { path, managed } => {
            if *managed {
                ensure_private_dir(path.parent().expect("managed key path has a parent")).map_err(anyhow::Error::msg)?;
            }
            write_key_file(path, key).map_err(anyhow::Error::msg)?;
        }
    }
    let mode = if matches!(target, Target::File { .. }) { " (mode 0600)" } else { "" };
    writeln!(out, "Saved {} key for account '{account}' to {}{mode}.", pk.label, describe(target))?;
    if !quiet {
        writeln!(out, "Add to your config:")?;
        writeln!(out, "{}", config_snippet(pk.provider, account, target))?;
        let managed = match target {
            Target::Keychain { service, .. } => service == keychain::DEFAULT_SERVICE,
            Target::File { managed, .. } => *managed,
        };
        if managed && account == DEFAULT_ACCOUNT {
            writeln!(out, "(Without this config entry the key is still found automatically as the last-resort source for account '{DEFAULT_ACCOUNT}'.)")?;
        }
    }
    Ok(())
}

/// 删除受管存储/--file 的 key。不存在 ⇒ 错误 `no key stored for <provider>/<account>`。
pub fn delete(stores: &Stores<'_>, pk: &ProviderKey, account: &str, target: &Target, out: &mut dyn Write) -> anyhow::Result<()> {
    let missing = || anyhow::anyhow!("no key stored for {}/{account}", pk.provider);
    match target {
        Target::Keychain { service, account: item } => match stores.keychain.delete(service, item) {
            Err(e) if e.is_not_found() => return Err(missing()),
            other => other?,
        },
        Target::File { path, .. } => match std::fs::remove_file(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing()),
            other => other.map_err(|e| anyhow::anyhow!("cannot delete key file {}: {e}", path.display()))?,
        },
    }
    writeln!(out, "Deleted {} key for account '{account}' from {}.", pk.label, describe(target))?;
    Ok(())
}

/// 配置里与 key 有关的只读部分（`list` 不做完整 `Config::load`：不读 webhook secret 等其它文件）。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyConfig {
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    #[serde(default)]
    pub keychain_timeout_secs: Option<u64>,
    #[serde(default)]
    pub providers: BTreeMap<String, KeyProviderConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyProviderConfig {
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    #[serde(default)]
    pub api_key_keychain: Option<KeychainRef>,
    #[serde(default)]
    pub accounts: BTreeMap<String, KeyAccountConfig>,
}

/// 账号块里与 key 有关的部分（其它字段忽略）。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyAccountConfig {
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    #[serde(default)]
    pub api_key_keychain: Option<KeychainRef>,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

/// 一个账号在配置里声明的来源（最多一个）。
#[derive(Debug, Default)]
struct Declared {
    file: Option<PathBuf>,
    keychain: Option<(String, String)>,
    env: Option<String>,
}

impl KeyConfig {
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|_| anyhow::anyhow!("cannot read configuration"))?;
        let mut cfg: Self = serde_json::from_str(&text).map_err(|_| anyhow::anyhow!("invalid configuration"))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let account_files = cfg.providers.values_mut().flat_map(|p| p.api_key_file.as_mut().into_iter().chain(p.accounts.values_mut().filter_map(|a| a.api_key_file.as_mut())));
        for file in account_files.chain(cfg.api_key_file.as_mut()) {
            if file.is_relative() {
                *file = dir.join(&*file);
            }
        }
        Ok(cfg)
    }

    /// 配置里出现的账号名：default 在前，其后为命名账号（排序）。
    fn account_names(&self, provider: &str) -> Vec<String> {
        let named = self.providers.get(provider).into_iter().flat_map(|p| p.accounts.keys()).filter(|n| *n != DEFAULT_ACCOUNT && valid_account_name(n)).cloned();
        std::iter::once(DEFAULT_ACCOUNT.to_string()).chain(named).collect()
    }

    fn declared(&self, provider: &str, account: &str) -> Declared {
        let settings = self.providers.get(provider);
        let block = settings.and_then(|p| p.accounts.get(account));
        let mut d = Declared {
            file: block.and_then(|a| a.api_key_file.clone()),
            keychain: block.and_then(|a| a.api_key_keychain.as_ref()).map(|k| k.resolve(provider, account)),
            env: block.and_then(|a| a.api_key_env.clone()),
        };
        if account == DEFAULT_ACCOUNT && d.file.is_none() && d.keychain.is_none() && d.env.is_none() {
            d.file = settings.and_then(|p| p.api_key_file.clone()).or_else(|| if provider == "openai" { self.api_key_file.clone() } else { None });
            d.keychain = settings.and_then(|p| p.api_key_keychain.as_ref()).map(|k| k.resolve(provider, account));
        }
        d
    }

    fn declares_account(&self, provider: &str, account: &str) -> bool {
        self.providers.get(provider).is_some_and(|p| p.accounts.contains_key(account))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyRow {
    pub provider: String,
    pub account: String,
    pub source: String,
    pub status: String,
    pub in_config: bool,
}

fn file_status(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                format!("present (mode {:04o})", m.permissions().mode() & 0o777)
            }
            #[cfg(not(unix))]
            {
                let _ = m;
                "present".into()
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing".into(),
        Err(_) => "unreadable".into(),
    }
}

/// 收集 key 来源（不读取任何 secret 数据：文件看 metadata，Keychain 做属性搜索，env 只看是否设置）。
pub fn list_rows(stores: &Stores<'_>, cfg: &KeyConfig, filter: Option<&str>, files: KeyFiles<'_>, getenv: &dyn Fn(&str) -> Option<String>) -> Vec<KeyRow> {
    // 每个 service 只做一次属性搜索
    let mut cache: BTreeMap<String, Result<Vec<String>, String>> = BTreeMap::new();
    let mut keychain_items = |service: &str| cache.entry(service.to_string()).or_insert_with(|| stores.keychain.list(service).map_err(|e| e.to_string())).clone();
    let set = |var: &str| getenv(var).filter(|v| !v.trim().is_empty());
    let mut rows = vec![];
    for pk in PROVIDER_KEYS.iter().filter(|pk| filter.is_none_or(|f| f == pk.provider)) {
        let id = pk.provider;
        let row = |account: &str, source: KeySource, status: String, in_config: bool| KeyRow { provider: id.into(), account: account.into(), source: source.to_string(), status, in_config };
        let cli_file = if id == "openai" { files.openai } else { files.anthropic };
        if let Some(p) = cli_file {
            rows.push(row(DEFAULT_ACCOUNT, KeySource::CliFile(p.into()), file_status(p), false));
        }
        if let Some(p) = set(pk.file_env) {
            let p = PathBuf::from(p.trim());
            rows.push(row(DEFAULT_ACCOUNT, KeySource::EnvFile(pk.file_env, p.clone()), file_status(&p), false));
        }
        // 配置声明的来源（每个账号最多一个）；受管扫描时跳过已列出的条目
        let mut listed_files: Vec<PathBuf> = vec![];
        let mut listed_items: Vec<(String, String)> = vec![];
        for name in cfg.account_names(id) {
            let declared = cfg.declared(id, &name);
            if let Some(p) = declared.file {
                rows.push(row(&name, KeySource::ConfigFile(p.clone()), file_status(&p), true));
                listed_files.push(p);
            }
            if let Some((service, account)) = declared.keychain {
                let status = match keychain_items(&service) {
                    _ if !stores.macos => "unsupported".into(),
                    Ok(items) if items.contains(&account) => "present".into(),
                    Ok(_) => "missing".into(),
                    Err(e) => format!("unknown ({e})"),
                };
                rows.push(row(&name, KeySource::Keychain { service: service.clone(), account: account.clone() }, status, true));
                listed_items.push((service, account));
            }
            if let Some(var) = declared.env {
                let status = if set(&var).is_some() { "set" } else { "missing" };
                rows.push(row(&name, KeySource::AccountEnv(var), status.into(), true));
            }
            if name == DEFAULT_ACCOUNT && set(pk.env).is_some() {
                rows.push(row(DEFAULT_ACCOUNT, KeySource::Env(pk.env), "set".into(), false));
            }
        }
        // 受管存储里发现的条目
        match stores.managed() {
            ManagedStore::Keychain { service } => {
                let prefix = format!("{id}/");
                for item in keychain_items(&service).unwrap_or_default() {
                    let Some(name) = item.strip_prefix(&prefix).filter(|n| valid_account_name(n)) else { continue };
                    if listed_items.iter().any(|(s, a)| *s == service && *a == item) {
                        continue;
                    }
                    rows.push(KeyRow {
                        provider: id.into(),
                        account: name.into(),
                        source: KeySource::Keychain { service: service.clone(), account: item.clone() }.to_string(),
                        status: "present".into(),
                        in_config: cfg.declares_account(id, name),
                    });
                }
            }
            ManagedStore::Dir(dir) => {
                let prefix = format!("{id}-");
                let mut names: Vec<String> = std::fs::read_dir(&dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".key")).and_then(|n| n.strip_prefix(&prefix)).filter(|n| valid_account_name(n)).map(String::from))
                    .collect();
                names.sort();
                for name in names {
                    let path = managed_key_path_in(&dir, id, &name).expect("validated name");
                    if listed_files.contains(&path) {
                        continue;
                    }
                    let in_config = cfg.declares_account(id, &name);
                    rows.push(KeyRow { provider: id.into(), account: name, source: KeySource::ManagedFile(path.clone()).to_string(), status: file_status(&path), in_config });
                }
            }
            ManagedStore::None => {}
        }
    }
    rows
}

fn print_rows(rows: &[KeyRow], json: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(rows)?)?;
        return Ok(());
    }
    if rows.is_empty() {
        eprintln!("no keys found (use `sub-bot keys set --provider <id>`)");
        return Ok(());
    }
    let width = |f: fn(&KeyRow) -> &str, title: &str| rows.iter().map(|r| f(r).len()).chain([title.len()]).max().unwrap_or(0);
    let (wp, wa, ws) = (width(|r| &r.provider, "PROVIDER"), width(|r| &r.account, "ACCOUNT"), width(|r| &r.source, "SOURCE"));
    writeln!(out, "{:<wp$}  {:<wa$}  {:<ws$}  STATUS", "PROVIDER", "ACCOUNT", "SOURCE")?;
    for r in rows {
        writeln!(out, "{:<wp$}  {:<wa$}  {:<ws$}  {}", r.provider, r.account, r.source, r.status)?;
    }
    Ok(())
}

/// 按配置尝试加载每个将被使用的 key（§4.5：所有配置的账号；default 账号只在 required 或声明了来源时检查）。任何失败 ⇒ Err。
pub fn check(stores: &Stores<'_>, cfg: &Config, default_provider: &str, filter: Option<&str>, files: KeyFiles<'_>, getenv: &dyn Fn(&str) -> Option<String>, out: &mut dyn Write) -> anyhow::Result<()> {
    if let Some(f) = filter {
        anyhow::ensure!(provider_key(f).is_some(), "keys check --provider must be one of: {}", PROVIDER_KEYS.iter().map(|k| k.provider).collect::<Vec<_>>().join(", "));
    }
    let required = crate::hub::required_accounts(cfg, default_provider);
    let managed = stores.managed();
    let mut failed = false;
    for pk in PROVIDER_KEYS.iter().filter(|pk| filter.is_none_or(|f| f == pk.provider)) {
        for (name, account) in cfg.provider_accounts(pk.provider) {
            let label = format!("{}/{name}", pk.provider);
            let spec = crate::hub::account_key_spec(pk, &name, &account, files, &managed);
            let is_required = required.contains(&crate::accounts::AccountKey::new(pk.provider, &name));
            if filter.is_none() && name == DEFAULT_ACCOUNT && !is_required && !spec.has_explicit_source(getenv) {
                writeln!(out, "{label}: skipped (not used by this configuration)")?;
                continue;
            }
            match load_account_key(&spec, getenv, stores.keychain) {
                Ok((_key, source)) => writeln!(out, "{label}: ok ({source})")?,
                Err(e) => {
                    failed = true;
                    let e = match e {
                        KeyLoadError::NotFound(_) if name == DEFAULT_ACCOUNT => format!("{} API key not found (run `sub-bot keys set --provider {}`)", pk.label, pk.provider),
                        KeyLoadError::NotFound(_) => format!("{} API key not found (run `sub-bot keys set --provider {} --account {name}`)", pk.label, pk.provider),
                        e => e.to_string(),
                    };
                    writeln!(out, "{label}: FAILED: {e}")?;
                }
            }
        }
    }
    anyhow::ensure!(!failed, "one or more API keys failed to load");
    Ok(())
}

/// `keys reauth` 的参数。
#[derive(Debug, Clone, Default)]
pub struct ReauthOpts<'a> {
    /// Keychain service（默认 `sub-bot`）
    pub service: Option<&'a str>,
    /// 全局 `--provider` 筛选
    pub provider: Option<&'a str>,
    /// `--account` 筛选
    pub account: Option<&'a str>,
    pub from_env: bool,
    pub dry_run: bool,
    pub json: bool,
}

/// `keys reauth` 每个条目的结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReauthRow {
    pub provider: String,
    pub account: String,
    pub service: String,
    pub item: String,
    /// `would migrate` | `migrated` | `not found` | `failed`
    pub status: String,
    /// key 从哪里来：`keychain` 或 `env <VAR>`
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 一个待迁移条目。
#[derive(Debug, Clone)]
struct ReauthTarget {
    provider: &'static ProviderKey,
    account: String,
    service: String,
    item: String,
    /// `--from-env` 时取 key 的变量名（命名账号没有 `apiKeyEnv` 时为 None）
    env: Option<String>,
}

/// 收集迁移目标：配置声明的 `apiKeyKeychain` 条目（按 `--service` 筛选）+ 受管 service 下 `<provider>/<account>` 形式的条目；去重。
fn reauth_targets(stores: &Stores<'_>, cfg: &KeyConfig, opts: &ReauthOpts<'_>) -> anyhow::Result<Vec<ReauthTarget>> {
    let service = opts.service.unwrap_or(keychain::DEFAULT_SERVICE);
    let managed = stores.keychain.list(service).map_err(|e| anyhow::anyhow!("cannot list keychain items (service={service}): {e}"))?;
    tracing::debug!(service, count = managed.len(), "reauth: keychain items found by attribute search");
    let mut targets: Vec<ReauthTarget> = vec![];
    let mut push = |t: ReauthTarget| {
        if !targets.iter().any(|x| x.service == t.service && x.item == t.item) {
            targets.push(t);
        }
    };
    for pk in PROVIDER_KEYS.iter().filter(|pk| opts.provider.is_none_or(|f| f == pk.provider)) {
        let wanted = |name: &str| opts.account.is_none_or(|a| a == name);
        for name in cfg.account_names(pk.provider) {
            let declared = cfg.declared(pk.provider, &name);
            let Some((svc, item)) = declared.keychain else { continue };
            if !wanted(&name) || (opts.service.is_some() && svc != service) {
                continue;
            }
            let env = if name == DEFAULT_ACCOUNT { Some(declared.env.unwrap_or_else(|| pk.env.to_string())) } else { declared.env };
            push(ReauthTarget { provider: pk, account: name.clone(), service: svc, item, env });
        }
        let prefix = format!("{}/", pk.provider);
        for item in &managed {
            let Some(name) = item.strip_prefix(&prefix).filter(|n| valid_account_name(n)) else { continue };
            if !wanted(name) {
                continue;
            }
            let env = if name == DEFAULT_ACCOUNT { Some(pk.env.to_string()) } else { cfg.declared(pk.provider, name).env };
            push(ReauthTarget { provider: pk, account: name.into(), service: service.into(), item: item.clone(), env });
        }
    }
    Ok(targets)
}

fn reauth_row(t: &ReauthTarget, from_env: bool, status: &str, error: Option<String>) -> ReauthRow {
    let source = match (from_env, &t.env) {
        (false, _) => "keychain".into(),
        (true, Some(var)) => format!("env {var}"),
        (true, None) => "env (none)".into(),
    };
    ReauthRow { provider: t.provider.provider.into(), account: t.account.clone(), service: t.service.clone(), item: t.item.clone(), status: status.into(), source, error }
}

/// 迁移一个条目：取 key（旧条目或环境变量）→ 删除 → 重建 → 读回比对。返回 (status, error)。
fn reauth_one(stores: &Stores<'_>, t: &ReauthTarget, from_env: bool, getenv: &dyn Fn(&str) -> Option<String>) -> (&'static str, Option<String>) {
    let (service, item) = (t.service.as_str(), t.item.as_str());
    let label = format!("{}/{}", t.provider.provider, t.account);
    let key = if from_env {
        let Some(var) = t.env.as_deref() else {
            return (
                "failed",
                Some(format!(
                    "no environment variable for account '{}' (named accounts do not use {}); use `sub-bot keys set --provider {} --account {}`",
                    t.account, t.provider.env, t.provider.provider, t.account
                )),
            );
        };
        tracing::info!(account = %label, service, item, var, "reauth: taking key from environment");
        match getenv(var) {
            Some(text) => {
                let result = validate_key_text(&text).map_err(|e| match e {
                    KeyTextError::Empty => format!("env {var} is empty"),
                    KeyTextError::Whitespace => format!("env {var} must contain only the key"),
                });
                wipe_bytes(text.into_bytes());
                match result {
                    Ok(key) => key,
                    Err(e) => return ("failed", Some(e)),
                }
            }
            None => return ("failed", Some(format!("env {var} is not set"))),
        }
    } else {
        tracing::info!(account = %label, service, item, "reauth: reading existing keychain item (a legacy item shows one authorization dialog)");
        match stores.keychain.get(service, item) {
            Ok(key) => key,
            Err(e) if e.is_not_found() => {
                tracing::info!(account = %label, service, item, "reauth: item not found");
                return ("not found", None);
            }
            Err(e) => {
                tracing::warn!(account = %label, service, item, error = %e, "reauth: cannot read existing item");
                return ("failed", Some(e.to_string()));
            }
        }
    };
    tracing::info!(account = %label, service, item, "reauth: deleting old item");
    match stores.keychain.delete(service, item) {
        Ok(()) => {}
        Err(e) if e.is_not_found() => tracing::debug!(account = %label, service, item, "reauth: nothing to delete"),
        Err(e) => {
            tracing::warn!(account = %label, service, item, error = %e, "reauth: delete failed");
            return ("failed", Some(format!("delete failed: {e}")));
        }
    }
    tracing::info!(account = %label, service, item, "reauth: re-adding item via /usr/bin/security");
    if let Err(e) = stores.keychain.set(service, item, &key) {
        tracing::warn!(account = %label, service, item, error = %e, "reauth: re-add failed");
        return ("failed", Some(format!("re-add failed: {e}")));
    }
    tracing::debug!(account = %label, service, item, "reauth: verifying read-back");
    match stores.keychain.get(service, item) {
        Ok(back) if back.expose() == key.expose() => {
            tracing::info!(account = %label, service, item, "reauth: migrated");
            ("migrated", None)
        }
        Ok(_) => ("failed", Some("verification failed: the re-added item does not read back as the value written".into())),
        Err(e) => ("failed", Some(format!("verification read failed: {e}"))),
    }
}

/// `keys reauth`：见 [`KeysCmd::Reauth`]。非 macOS 只打印说明并成功返回。任一条目失败 ⇒ Err。
pub fn reauth(stores: &Stores<'_>, cfg: &KeyConfig, opts: &ReauthOpts<'_>, getenv: &dyn Fn(&str) -> Option<String>, out: &mut dyn Write) -> anyhow::Result<()> {
    if let Some(f) = opts.provider {
        anyhow::ensure!(provider_key(f).is_some(), "unknown provider '{f}' (expected one of: {})", PROVIDER_KEYS.iter().map(|k| k.provider).collect::<Vec<_>>().join(", "));
    }
    if let Some(a) = opts.account {
        anyhow::ensure!(valid_account_name(a), "invalid account name '{a}' (allowed: [A-Za-z0-9_.-]{{1,64}})");
    }
    anyhow::ensure!(opts.service.is_none_or(|s| !s.trim().is_empty()), "--service must not be blank");
    if !stores.macos {
        let msg = "keychain is only supported on macOS; nothing to do here (keys are stored as 0600 files on this platform and need no re-authorization)";
        tracing::info!("reauth: {msg}");
        writeln!(out, "{}", if opts.json { "[]" } else { msg })?;
        return Ok(());
    }
    let targets = reauth_targets(stores, cfg, opts)?;
    tracing::info!(count = targets.len(), dry_run = opts.dry_run, from_env = opts.from_env, "reauth: targets collected");
    let mut rows: Vec<ReauthRow> = vec![];
    for t in &targets {
        let (status, error) = if opts.dry_run {
            match (opts.from_env, &t.env) {
                (true, None) => ("would fail", Some(format!("no environment variable for account '{}'", t.account))),
                (true, Some(var)) if getenv(var).is_none_or(|v| v.trim().is_empty()) => ("would fail", Some(format!("env {var} is not set"))),
                _ => ("would migrate", None),
            }
        } else {
            reauth_one(stores, t, opts.from_env, getenv)
        };
        rows.push(reauth_row(t, opts.from_env, status, error));
    }
    if opts.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
    } else if rows.is_empty() {
        writeln!(
            out,
            "no keychain items to migrate (service={}{})",
            opts.service.unwrap_or(keychain::DEFAULT_SERVICE),
            if cfg.providers.is_empty() { "; pass --config to include apiKeyKeychain entries" } else { "" }
        )?;
    } else {
        for r in &rows {
            let source = if opts.from_env || opts.dry_run { format!(" from {}", r.source) } else { String::new() };
            let where_ = format!("keychain service={} account={}", r.service, r.item);
            match r.status.as_str() {
                "failed" => writeln!(out, "{}/{}: FAILED: {} ({where_})", r.provider, r.account, r.error.as_deref().unwrap_or("unknown error"))?,
                "would fail" => writeln!(out, "{}/{}: would fail: {} ({where_})", r.provider, r.account, r.error.as_deref().unwrap_or("unknown error"))?,
                status => writeln!(out, "{}/{}: {status}{source} ({where_})", r.provider, r.account)?,
            }
        }
        if opts.dry_run {
            writeln!(out, "(dry run: nothing was read or changed)")?;
        }
    }
    let failed = rows.iter().filter(|r| r.status == "failed").count();
    anyhow::ensure!(failed == 0, "{failed} keychain item(s) could not be migrated");
    Ok(())
}

fn require_provider(cli: &crate::Cli) -> anyhow::Result<&'static ProviderKey> {
    let id = cli.provider.as_deref().ok_or_else(|| anyhow::anyhow!("--provider is required (one of: {})", PROVIDER_KEYS.iter().map(|k| k.provider).collect::<Vec<_>>().join(", ")))?;
    provider_key(id).ok_or_else(|| anyhow::anyhow!("unknown provider '{id}' (expected one of: {})", PROVIDER_KEYS.iter().map(|k| k.provider).collect::<Vec<_>>().join(", ")))
}

/// `--keychain-timeout-secs` > 配置 `keychainTimeoutSecs` > 默认 30 秒。
fn keychain_timeout(cli: &crate::Cli, configured: Option<u64>) -> anyhow::Result<Duration> {
    let secs = cli.keychain_timeout_secs.or(configured).unwrap_or_else(crate::config::default_keychain_timeout_secs);
    anyhow::ensure!(secs >= 1, "keychainTimeoutSecs must be >= 1");
    Ok(Duration::from_secs(secs))
}

/// 系统 Keychain + 限时读取；等待超过 grace 时把提示打到 stderr（stdout 只放结果）。
fn timed_system_store(timeout: Duration) -> TimedStore {
    TimedStore::new(Arc::new(SystemKeychain), timeout, keychain::WAIT_HINT_GRACE, |service, account| eprintln!("{}", keychain::wait_hint(service, account)))
}

pub fn run(cli: &crate::Cli, cmd: &KeysCmd) -> anyhow::Result<()> {
    let stores = Stores { macos: keychain::supported(), managed_dir: managed_key_dir(), keychain: &SystemKeychain };
    let getenv = |k: &str| std::env::var(k).ok();
    let files = KeyFiles { openai: cli.openai_key_file.as_deref(), anthropic: cli.anthropic_key_file.as_deref() };
    let mut out = std::io::stdout().lock();
    match cmd {
        KeysCmd::Set { account, file, service, quiet } => {
            let pk = require_provider(cli)?;
            let target = storage_target(pk.provider, account, file.as_deref(), service.as_deref(), &stores)?;
            let key = read_key(pk, account)?;
            set(&stores, pk, account, &target, &key, *quiet, &mut out)
        }
        KeysCmd::Delete { account, file, service } => {
            let pk = require_provider(cli)?;
            let target = storage_target(pk.provider, account, file.as_deref(), service.as_deref(), &stores)?;
            delete(&stores, pk, account, &target, &mut out)
        }
        KeysCmd::List { json } => {
            let cfg = cli.config.as_deref().map(KeyConfig::read).transpose()?.unwrap_or_default();
            if let Some(f) = cli.provider.as_deref() {
                anyhow::ensure!(provider_key(f).is_some(), "unknown provider '{f}'");
            }
            let timed = timed_system_store(keychain_timeout(cli, cfg.keychain_timeout_secs)?);
            let stores = Stores { keychain: &timed, ..stores };
            print_rows(&list_rows(&stores, &cfg, cli.provider.as_deref(), files, &getenv), *json, &mut out)
        }
        KeysCmd::Reauth { account, service, from_env, dry_run, json } => {
            let cfg = cli.config.as_deref().map(KeyConfig::read).transpose()?.unwrap_or_default();
            let timed = timed_system_store(keychain_timeout(cli, cfg.keychain_timeout_secs)?);
            let stores = Stores { keychain: &timed, ..stores };
            let opts = ReauthOpts { service: service.as_deref(), provider: cli.provider.as_deref(), account: account.as_deref(), from_env: *from_env, dry_run: *dry_run, json: *json };
            reauth(&stores, &cfg, &opts, &getenv, &mut out)
        }
        KeysCmd::Check => {
            let mut cfg = match &cli.config {
                Some(p) => Config::load(p).map_err(|e| anyhow::anyhow!("configuration load failed: {e}"))?,
                None => Config::fallback_for(cli.provider.as_deref(), cli.model.clone(), "", cli.workspace.clone()),
            };
            cfg.keychain_timeout_secs = keychain_timeout(cli, Some(cfg.keychain_timeout_secs))?.as_secs();
            let timed = timed_system_store(Duration::from_secs(cfg.keychain_timeout_secs));
            let stores = Stores { keychain: &timed, ..stores };
            let default = cfg.default_provider.clone().unwrap_or_else(|| "openai".into());
            check(&stores, &cfg, &default, cli.provider.as_deref(), files, &getenv, &mut out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ai::keychain::mock::MockKeychain;

    impl KeychainStore for MockKeychain {
        fn set(&self, service: &str, account: &str, secret: &Secret) -> Result<(), KeychainError> {
            self.items.lock().unwrap().insert((service.into(), account.into()), Ok(secret.expose().into()));
            Ok(())
        }
        fn delete(&self, service: &str, account: &str) -> Result<(), KeychainError> {
            self.items.lock().unwrap().remove(&(service.into(), account.into())).map(|_| ()).ok_or_else(|| KeychainError::NotFound { service: service.into(), account: account.into() })
        }
        fn list(&self, service: &str) -> Result<Vec<String>, KeychainError> {
            if let Some(delay) = self.delay {
                std::thread::sleep(delay);
            }
            Ok(self.items.lock().unwrap().keys().filter(|(s, _)| s == service).map(|(_, a)| a.clone()).collect())
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sub-bot-keys-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pk(id: &str) -> &'static ProviderKey {
        provider_key(id).unwrap()
    }

    fn output(f: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>) -> (anyhow::Result<()>, String) {
        let mut out = Vec::new();
        let r = f(&mut out);
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn read_key_from_reader_trims_and_rejects_multi_or_empty() {
        assert_eq!(read_key_from_reader(&b"  sk-test-value \n"[..]).unwrap().expose(), "sk-test-value");
        assert_eq!(read_key_from_reader(&b" \n\t"[..]).unwrap_err().to_string(), "no key provided");
        assert_eq!(read_key_from_reader(&b""[..]).unwrap_err().to_string(), "no key provided");
        let e = read_key_from_reader(&b"sk-secret-one\nsk-secret-two\n"[..]).unwrap_err().to_string();
        assert_eq!(e, "stdin must contain only the key");
        assert_eq!(read_key_from_reader(&[0xff, 0xfe][..]).unwrap_err().to_string(), "stdin must contain only the key");
    }

    #[test]
    fn storage_target_selection() {
        let kc = MockKeychain::default();
        let dir = PathBuf::from("/cfg/sub-bot/keys");
        let lin = Stores { macos: false, managed_dir: Some(dir.clone()), keychain: &kc };
        let mac = Stores { macos: true, managed_dir: Some(dir.clone()), keychain: &kc };
        assert_eq!(storage_target("openai", "default", None, None, &lin).unwrap(), Target::File { path: dir.join("openai-default.key"), managed: true });
        assert_eq!(storage_target("openai", "work", None, None, &mac).unwrap(), Target::Keychain { service: "sub-bot".into(), account: "openai/work".into() });
        assert_eq!(storage_target("anthropic", "default", None, Some("my-keys"), &mac).unwrap(), Target::Keychain { service: "my-keys".into(), account: "anthropic/default".into() });
        assert_eq!(storage_target("openai", "default", Some(Path::new("/k/o.key")), None, &mac).unwrap(), Target::File { path: "/k/o.key".into(), managed: false });
        let rel = storage_target("openai", "default", Some(Path::new("o.key")), None, &lin).unwrap();
        assert!(matches!(rel, Target::File { ref path, managed: false } if path.is_absolute() && path.ends_with("o.key")));
        assert_eq!(storage_target("openai", "default", None, Some("x"), &lin).unwrap_err().to_string(), "--service is only supported on macOS");
        assert_eq!(storage_target("openai", "default", Some(Path::new("/k")), Some("x"), &mac).unwrap_err().to_string(), "--service cannot be combined with --file");
        assert!(storage_target("openai", "a/b", None, None, &lin).unwrap_err().to_string().contains("invalid account name 'a/b'"));
        assert!(storage_target("faux", "default", None, None, &lin).unwrap_err().to_string().contains("unknown provider 'faux'"));
        let nowhere = Stores { macos: false, managed_dir: None, keychain: &kc };
        assert!(storage_target("openai", "default", None, None, &nowhere).unwrap_err().to_string().contains("use --file"));
    }

    #[cfg(unix)]
    #[test]
    fn set_with_file_writes_0600_and_prints_snippet_without_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("file");
        let kc = MockKeychain::default();
        let stores = Stores { macos: false, managed_dir: Some(dir.join("managed")), keychain: &kc };
        let path = dir.join("nested/openai.key");
        let target = storage_target("openai", "default", Some(&path), None, &stores).unwrap();
        let key = Secret::new("sk-test-file-secret");
        let (r, out) = output(|o| set(&stores, pk("openai"), "default", &target, &key, false, o));
        r.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sk-test-file-secret\n");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!out.contains("sk-test-file-secret"));
        assert_eq!(out, format!("Saved OpenAI key for account 'default' to {0} (mode 0600).\nAdd to your config:\n  \"providers\": {{ \"openai\": {{ \"apiKeyFile\": \"{0}\" }} }}\n", path.display()));
        // --quiet 只打印第一行；delete 删除后再次删除报错
        let (r, out) = output(|o| set(&stores, pk("openai"), "default", &target, &key, true, o));
        r.unwrap();
        assert_eq!(out.lines().count(), 1);
        let (r, out) = output(|o| delete(&stores, pk("openai"), "default", &target, o));
        r.unwrap();
        assert!(out.starts_with("Deleted OpenAI key for account 'default' from "));
        assert!(!path.exists());
        assert_eq!(delete(&stores, pk("openai"), "default", &target, &mut Vec::new()).unwrap_err().to_string(), "no key stored for openai/default");
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn set_managed_file_creates_private_dir_and_rejects_unsafe_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("managed");
        let kc = MockKeychain::default();
        let stores = Stores { macos: false, managed_dir: Some(dir.join("cfg/sub-bot/keys")), keychain: &kc };
        let target = storage_target("anthropic", "default", None, None, &stores).unwrap();
        let (r, out) = output(|o| set(&stores, pk("anthropic"), "default", &target, &Secret::new("sk-ant-managed"), false, o));
        r.unwrap();
        let keys = dir.join("cfg/sub-bot/keys");
        assert_eq!(std::fs::metadata(&keys).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(keys.join("anthropic-default.key")).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(out.contains("found automatically") && !out.contains("sk-ant-managed"), "{out}");
        std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = set(&stores, pk("anthropic"), "default", &target, &Secret::new("sk-ant-managed"), true, &mut Vec::new()).unwrap_err().to_string();
        assert!(e.contains("group/others"), "{e}");
        // symlink 目录被拒绝
        std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link_root = dir.join("link");
        std::os::unix::fs::symlink(&keys, &link_root).unwrap();
        let linked = Stores { macos: false, managed_dir: Some(link_root), keychain: &kc };
        let target = storage_target("anthropic", "default", None, None, &linked).unwrap();
        assert!(set(&linked, pk("anthropic"), "default", &target, &Secret::new("x"), true, &mut Vec::new()).unwrap_err().to_string().contains("not a symlink"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn set_list_delete_with_mock_keychain() {
        let kc = MockKeychain::default();
        let stores = Stores { macos: true, managed_dir: None, keychain: &kc };
        let target = storage_target("openai", "work", None, None, &stores).unwrap();
        let (r, out) = output(|o| set(&stores, pk("openai"), "work", &target, &Secret::new("sk-mock-work"), false, o));
        r.unwrap();
        assert_eq!(
            out,
            "Saved OpenAI key for account 'work' to keychain (service=sub-bot, account=openai/work).\nAdd to your config:\n  \"providers\": { \"openai\": { \"accounts\": { \"work\": { \"apiKeyKeychain\": {} } } } }\n"
        );
        assert_eq!(kc.get("sub-bot", "openai/work").unwrap().expose(), "sk-mock-work");
        // 已存在则更新
        set(&stores, pk("openai"), "work", &target, &Secret::new("sk-mock-work-2"), true, &mut Vec::new()).unwrap();
        assert_eq!(kc.get("sub-bot", "openai/work").unwrap().expose(), "sk-mock-work-2");
        let default = storage_target("openai", "default", None, None, &stores).unwrap();
        let (r, out) = output(|o| set(&stores, pk("openai"), "default", &default, &Secret::new("sk-mock-default"), false, o));
        r.unwrap();
        assert!(out.contains(r#""providers": { "openai": { "apiKeyKeychain": {} } }"#) && out.contains("found automatically"), "{out}");
        let custom = storage_target("anthropic", "default", None, Some("my-keys"), &stores).unwrap();
        let (r, out) = output(|o| set(&stores, pk("anthropic"), "default", &custom, &Secret::new("sk-mock-custom"), false, o));
        r.unwrap();
        assert!(out.contains(r#""apiKeyKeychain": {"service": "my-keys"}"#) && !out.contains("found automatically"), "{out}");

        let cfg: KeyConfig = serde_json::from_value(serde_json::json!({"providers": {"anthropic": {"apiKeyKeychain": {"service": "my-keys"}}, "openai": {"apiKeyKeychain": {}}}})).unwrap();
        let rows = list_rows(&stores, &cfg, None, KeyFiles::default(), &|_| None);
        let summary: Vec<_> = rows.iter().map(|r| (r.provider.as_str(), r.account.as_str(), r.source.as_str(), r.status.as_str(), r.in_config)).collect();
        assert_eq!(
            summary,
            [
                ("openai", "default", "keychain service=sub-bot account=openai/default", "present", true),
                ("openai", "work", "keychain service=sub-bot account=openai/work", "present", false),
                ("anthropic", "default", "keychain service=my-keys account=anthropic/default", "present", true),
            ]
        );
        let (r, out) = output(|o| print_rows(&rows, true, o));
        r.unwrap();
        assert!(!out.contains("sk-mock"));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()[1]["inConfig"], false);

        let (r, out) = output(|o| delete(&stores, pk("openai"), "work", &target, o));
        r.unwrap();
        assert_eq!(out, "Deleted OpenAI key for account 'work' from keychain (service=sub-bot, account=openai/work).\n");
        assert_eq!(delete(&stores, pk("openai"), "work", &target, &mut Vec::new()).unwrap_err().to_string(), "no key stored for openai/work");
    }

    #[test]
    fn list_scans_managed_dir_config_and_env_without_reading_keys() {
        let dir = temp_dir("list");
        let kc = MockKeychain::default();
        let stores = Stores { macos: false, managed_dir: Some(dir.join("keys")), keychain: &kc };
        ensure_private_dir(&dir.join("keys")).unwrap();
        for (name, content) in
            [("openai-default.key", "sk-list-secret-1"), ("openai-work.key", "sk-list-secret-2"), ("anthropic-ci.key", "sk-list-secret-3"), ("junk.txt", "x"), ("openai-a b.key", "x")]
        {
            std::fs::write(dir.join("keys").join(name), content).unwrap();
        }
        let config_path = dir.join("sub-bot.json");
        std::fs::write(&config_path, r#"{"providers": {"openai": {"apiKeyFile": "keys/openai-default.key"}, "anthropic": {"apiKeyFile": "missing.key"}}, "profiles": {}}"#).unwrap();
        let cfg = KeyConfig::read(&config_path).unwrap();
        let env = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk-list-env-secret".to_string());
        let rows = list_rows(&stores, &cfg, None, KeyFiles::default(), &env);
        let summary: Vec<_> = rows.iter().map(|r| (r.provider.as_str(), r.account.as_str(), r.source.clone(), r.status.split(' ').next().unwrap().to_string(), r.in_config)).collect();
        let keys = dir.join("keys");
        assert_eq!(
            summary,
            [
                ("openai", "default", format!("config apiKeyFile {}", keys.join("openai-default.key").display()), "present".into(), true),
                ("openai", "work", format!("managed key file {}", keys.join("openai-work.key").display()), "present".into(), false),
                ("anthropic", "default", format!("config apiKeyFile {}", dir.join("missing.key").display()), "missing".into(), true),
                ("anthropic", "default", "env ANTHROPIC_API_KEY".into(), "set".into(), false),
                ("anthropic", "ci", format!("managed key file {}", keys.join("anthropic-ci.key").display()), "present".into(), false),
            ]
        );
        assert_eq!(list_rows(&stores, &cfg, Some("anthropic"), KeyFiles::default(), &env).len(), 3);
        for json in [false, true] {
            let (r, out) = output(|o| print_rows(&rows, json, o));
            r.unwrap();
            assert!(!out.contains("sk-list"), "{out}");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn check_reports_sources_and_failures_without_keys() {
        let dir = temp_dir("check");
        let key_file = dir.join("openai.key");
        std::fs::write(&key_file, "sk-check-secret\n").unwrap();
        let mut cfg = Config::fallback("gpt-6.1-sol".into(), "t", dir.clone());
        cfg.api_key_file = Some(key_file.clone());
        let kc = MockKeychain::default();
        let stores = Stores { macos: false, managed_dir: Some(dir.join("keys")), keychain: &kc };
        let (r, out) = output(|o| check(&stores, &cfg, "openai", None, KeyFiles::default(), &|_| None, o));
        r.unwrap();
        assert_eq!(out, format!("openai/default: ok (config apiKeyFile {})\nanthropic/default: skipped (not used by this configuration)\n", key_file.display()));
        // 显式 --provider anthropic：未找到 ⇒ FAILED + 非零退出
        let (r, out) = output(|o| check(&stores, &cfg, "openai", Some("anthropic"), KeyFiles::default(), &|_| None, o));
        assert_eq!(r.unwrap_err().to_string(), "one or more API keys failed to load");
        assert_eq!(out, "anthropic/default: FAILED: Anthropic API key not found (run `sub-bot keys set --provider anthropic`)\n");
        // Keychain 错误原样报告（只含 service/account）
        cfg.providers.insert("anthropic".into(), crate::config::ProviderConfig { api_key_keychain: Some(Default::default()), ..Default::default() });
        let kc = MockKeychain::with([(("sub-bot", "anthropic/default"), Err(KeychainError::Denied { service: "sub-bot".into(), account: "anthropic/default".into() }))]);
        let stores = Stores { macos: true, managed_dir: None, keychain: &kc };
        let (r, out) = output(|o| check(&stores, &cfg, "openai", None, KeyFiles::default(), &|_| None, o));
        assert!(r.is_err());
        assert!(out.contains("openai/default: ok (config apiKeyFile") && out.contains("anthropic/default: FAILED: keychain access denied (service=sub-bot, account=anthropic/default)"), "{out}");
        assert!(!out.contains("sk-check-secret"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn list_and_check_cover_named_accounts_without_keys() {
        let dir = temp_dir("accounts");
        let kc = MockKeychain::default();
        let stores = Stores { macos: false, managed_dir: Some(dir.join("keys")), keychain: &kc };
        ensure_private_dir(&dir.join("keys")).unwrap();
        std::fs::write(dir.join("keys/openai-managed.key"), "sk-acct-secret-managed\n").unwrap();
        std::fs::write(dir.join("work.key"), "sk-acct-secret-work\n").unwrap();
        let config_path = dir.join("sub-bot.json");
        let value = serde_json::json!({
            "providers": {"openai": {"accounts": {
                "work": {"apiKeyFile": "work.key", "organization": "org_x", "fallbackAccounts": ["default"]},
                "proxy": {"apiKeyEnv": "TEST_KEYS_PROXY", "baseUrl": "http://localhost:9/v1"},
                "managed": {}
            }}},
            "profiles": {"p": {"account": "work", "workspaceRoots": ["."]}}
        });
        std::fs::write(&config_path, value.to_string()).unwrap();
        let env = |k: &str| (k == "TEST_KEYS_PROXY").then(|| "sk-acct-secret-proxy".to_string());
        let rows = list_rows(&stores, &KeyConfig::read(&config_path).unwrap(), Some("openai"), KeyFiles::default(), &env);
        let summary: Vec<_> = rows.iter().map(|r| (r.account.as_str(), r.source.clone(), r.status.split(' ').next().unwrap().to_string(), r.in_config)).collect();
        assert_eq!(
            summary,
            [
                ("proxy", "env TEST_KEYS_PROXY".to_string(), "set".to_string(), true),
                ("work", format!("config apiKeyFile {}", dir.join("work.key").display()), "present".into(), true),
                ("managed", format!("managed key file {}", dir.join("keys/openai-managed.key").display()), "present".into(), true),
            ]
        );
        let (r, out) = output(|o| print_rows(&rows, true, o));
        r.unwrap();
        assert!(!out.contains("sk-acct-secret"), "{out}");
        // check：所有配置的账号都尝试加载；default 未使用则跳过
        let mut cfg = Config::load(&config_path).unwrap();
        let (r, out) = output(|o| check(&stores, &cfg, "faux", None, KeyFiles::default(), &env, o));
        r.unwrap();
        assert_eq!(
            out,
            format!(
                "openai/default: skipped (not used by this configuration)\nopenai/managed: ok (managed key file {})\nopenai/proxy: ok (env TEST_KEYS_PROXY)\nopenai/work: ok (config apiKeyFile {})\nanthropic/default: skipped (not used by this configuration)\n",
                dir.join("keys/openai-managed.key").display(),
                dir.join("work.key").display()
            )
        );
        cfg.providers.get_mut("openai").unwrap().accounts.get_mut("managed").unwrap().api_key_env = Some("TEST_KEYS_MISSING".into());
        let (r, out) = output(|o| check(&stores, &cfg, "faux", None, KeyFiles::default(), &env, o));
        assert!(r.is_err());
        assert!(out.contains("openai/managed: FAILED: OpenAI API key env TEST_KEYS_MISSING is not set"), "{out}");
        assert!(!out.contains("sk-acct-secret"), "{out}");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 回归：anthropic 只被 profile 以命名账号 ci 使用 ⇒ anthropic/default 跳过，而不是 FAILED（与 serve 启动规则一致）。
    #[test]
    fn check_skips_default_account_of_provider_used_only_via_named_profile_account() {
        let dir = temp_dir("named-only");
        std::fs::write(dir.join("ci.key"), "sk-named-only-ci\n").unwrap();
        std::fs::write(dir.join("openai.key"), "sk-named-only-openai\n").unwrap();
        let config_path = dir.join("sub-bot.json");
        let value = serde_json::json!({
            "defaultProvider": "openai",
            "providers": {"openai": {"apiKeyFile": "openai.key"}, "anthropic": {"accounts": {"ci": {"apiKeyFile": "ci.key"}}}},
            "profiles": {"claude": {"provider": "anthropic", "account": "ci", "workspaceRoots": ["."]}}
        });
        std::fs::write(&config_path, value.to_string()).unwrap();
        let cfg = Config::load(&config_path).unwrap();
        let kc = MockKeychain::default();
        let stores = Stores { macos: false, managed_dir: Some(dir.join("keys")), keychain: &kc };
        let (r, out) = output(|o| check(&stores, &cfg, "openai", None, KeyFiles::default(), &|_| None, o));
        r.unwrap();
        assert_eq!(
            out,
            format!(
                "openai/default: ok (config apiKeyFile {})\nanthropic/ci: ok (config apiKeyFile {})\nanthropic/default: skipped (not used by this configuration)\n",
                dir.join("openai.key").display(),
                dir.join("ci.key").display()
            )
        );
        assert!(!out.contains("sk-named-only"), "{out}");
        std::fs::remove_dir_all(dir).ok();
    }

    type Hints = Arc<std::sync::Mutex<Vec<String>>>;

    fn hint_recorder() -> (Hints, impl Fn(&str, &str) + Send + Sync + 'static) {
        let hints = Arc::new(std::sync::Mutex::new(vec![]));
        let seen = hints.clone();
        (hints, move |s: &str, a: &str| seen.lock().unwrap().push(keychain::wait_hint(s, a)))
    }

    /// 授权对话框无人响应：`keys check` 先打印提示（grace 后），超时后报告该账号 FAILED 而不是挂起。
    #[test]
    fn check_times_out_on_blocked_keychain_and_prints_hint() {
        let dir = temp_dir("check-timeout");
        let mut cfg = Config::fallback("gpt-6.1-sol".into(), "t", dir.clone());
        std::fs::write(dir.join("openai.key"), "sk-timeout-openai\n").unwrap();
        cfg.api_key_file = Some(dir.join("openai.key"));
        cfg.providers.insert("anthropic".into(), crate::config::ProviderConfig { api_key_keychain: Some(Default::default()), ..Default::default() });
        let slow = MockKeychain::with([(("sub-bot", "anthropic/default"), Ok("sk-timeout-anthropic"))]).delayed(Duration::from_secs(30));
        let (hints, hook) = hint_recorder();
        let timed = TimedStore::new(Arc::new(slow), Duration::from_secs(1), Duration::from_millis(100), hook);
        let stores = Stores { macos: true, managed_dir: None, keychain: &timed };
        let started = std::time::Instant::now();
        let (r, out) = output(|o| check(&stores, &cfg, "openai", None, KeyFiles::default(), &|_| None, o));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(r.unwrap_err().to_string(), "one or more API keys failed to load");
        let timeout = KeychainError::Timeout { service: "sub-bot".into(), account: "anthropic/default".into(), secs: 1 };
        assert!(out.contains("openai/default: ok (config apiKeyFile"), "{out}");
        assert!(out.contains(&format!("anthropic/default: FAILED: {timeout}")), "{out}");
        assert!(!out.contains("sk-timeout"), "{out}");
        assert_eq!(*hints.lock().unwrap(), [keychain::wait_hint("sub-bot", "anthropic/default")]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn list_times_out_on_blocked_keychain_search() {
        let mut cfg = KeyConfig::default();
        cfg.providers.insert("openai".into(), KeyProviderConfig { api_key_keychain: Some(Default::default()), ..Default::default() });
        let slow = MockKeychain::with([(("sub-bot", "openai/default"), Ok("sk-list-secret"))]).delayed(Duration::from_secs(30));
        let (hints, hook) = hint_recorder();
        let timed = TimedStore::new(Arc::new(slow), Duration::from_secs(1), Duration::from_millis(100), hook);
        let stores = Stores { macos: true, managed_dir: None, keychain: &timed };
        let rows = list_rows(&stores, &cfg, Some("openai"), KeyFiles::default(), &|_| None);
        let row = rows.iter().find(|r| r.account == "default").unwrap();
        assert!(row.status.starts_with("unknown (no answer from macOS Keychain within 1s"), "{row:?}");
        // 每个 service 只搜索一次 ⇒ 只等待/提示一次
        assert_eq!(hints.lock().unwrap().len(), 1);
        // 快速路径：不提示
        let fast = MockKeychain::with([(("sub-bot", "openai/default"), Ok("sk-list-secret"))]);
        let (hints, hook) = hint_recorder();
        let timed = TimedStore::new(Arc::new(fast), Duration::from_secs(1), Duration::from_millis(100), hook);
        let stores = Stores { macos: true, managed_dir: None, keychain: &timed };
        let rows = list_rows(&stores, &cfg, Some("openai"), KeyFiles::default(), &|_| None);
        assert_eq!(rows.iter().find(|r| r.account == "default").unwrap().status, "present");
        assert!(hints.lock().unwrap().is_empty());
    }

    /// `--from-env` 不读旧条目；默认账号用 provider 环境变量，命名账号用 apiKeyEnv。
    fn reauth_env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    fn reauth_out(stores: &Stores<'_>, cfg: &KeyConfig, opts: &ReauthOpts<'_>, getenv: &dyn Fn(&str) -> Option<String>) -> (anyhow::Result<()>, String) {
        output(|o| reauth(stores, cfg, opts, getenv, o))
    }

    #[test]
    fn reauth_is_a_no_op_off_macos() {
        let kc = MockKeychain::with([(("sub-bot", "openai/default"), Ok("sk-linux-secret"))]);
        let stores = Stores { macos: false, managed_dir: Some(PathBuf::from("/tmp/x")), keychain: &kc };
        let (r, out) = reauth_out(&stores, &KeyConfig::default(), &ReauthOpts::default(), &|_| None);
        r.unwrap();
        assert_eq!(out, "keychain is only supported on macOS; nothing to do here (keys are stored as 0600 files on this platform and need no re-authorization)\n");
        let (r, out) = reauth_out(&stores, &KeyConfig::default(), &ReauthOpts { json: true, ..Default::default() }, &|_| None);
        r.unwrap();
        assert_eq!(out, "[]\n");
        // 没有读也没有改
        assert_eq!(kc.get("sub-bot", "openai/default").unwrap().expose(), "sk-linux-secret");
    }

    #[test]
    fn reauth_dry_run_lists_targets_without_reading_or_changing() {
        let kc = MockKeychain::with([
            (("sub-bot", "openai/default"), Ok("sk-dry-openai")),
            (("sub-bot", "openai/work"), Ok("sk-dry-work")),
            (("sub-bot", "anthropic/default"), Err(KeychainError::Denied { service: "sub-bot".into(), account: "anthropic/default".into() })),
            (("sub-bot", "junk"), Ok("sk-dry-junk")),
            (("my-keys", "claude-prod"), Ok("sk-dry-prod")),
        ]);
        let stores = Stores { macos: true, managed_dir: None, keychain: &kc };
        let cfg: KeyConfig = serde_json::from_value(serde_json::json!({"providers": {
            "anthropic": {"apiKeyKeychain": {"service": "my-keys", "account": "claude-prod"}, "accounts": {"ci": {"apiKeyKeychain": {}}}},
            "openai": {"apiKeyKeychain": {}}
        }}))
        .unwrap();
        let (r, out) = reauth_out(&stores, &cfg, &ReauthOpts { dry_run: true, ..Default::default() }, &|_| None);
        r.unwrap();
        assert_eq!(
            out,
            "openai/default: would migrate from keychain (keychain service=sub-bot account=openai/default)\n\
             openai/work: would migrate from keychain (keychain service=sub-bot account=openai/work)\n\
             anthropic/default: would migrate from keychain (keychain service=my-keys account=claude-prod)\n\
             anthropic/ci: would migrate from keychain (keychain service=sub-bot account=anthropic/ci)\n\
             anthropic/default: would migrate from keychain (keychain service=sub-bot account=anthropic/default)\n\
             (dry run: nothing was read or changed)\n"
        );
        assert!(!out.contains("sk-dry"), "{out}");
        // 条目原样（Denied 的仍是 Denied，没有被读取/重建）
        let items = kc.items.lock().unwrap().clone();
        assert_eq!(items.len(), 5);
        assert!(matches!(items[&("sub-bot".to_string(), "anthropic/default".to_string())], Err(KeychainError::Denied { .. })));
        // --from-env 的 dry-run 只看变量是否设置
        let env = reauth_env(&[("OPENAI_API_KEY", "sk-dry-env")]);
        let (r, out) = reauth_out(&stores, &cfg, &ReauthOpts { dry_run: true, from_env: true, json: true, ..Default::default() }, &env);
        r.unwrap();
        let rows: Vec<ReauthRow> = serde_json::from_str(&out).unwrap();
        let summary: Vec<_> = rows.iter().map(|r| (r.account.as_str(), r.status.as_str(), r.source.as_str(), r.error.as_deref())).collect();
        assert_eq!(
            summary,
            [
                ("default", "would migrate", "env OPENAI_API_KEY", None),
                ("work", "would fail", "env (none)", Some("no environment variable for account 'work'")),
                ("default", "would fail", "env ANTHROPIC_API_KEY", Some("env ANTHROPIC_API_KEY is not set")),
                ("ci", "would fail", "env (none)", Some("no environment variable for account 'ci'")),
                ("default", "would fail", "env ANTHROPIC_API_KEY", Some("env ANTHROPIC_API_KEY is not set")),
            ]
        );
        assert!(!out.contains("sk-dry"), "{out}");
        assert_eq!(rows[2].item, "claude-prod");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap()[0]["inConfig"], serde_json::Value::Null);
    }

    #[test]
    fn reauth_migrates_items_and_reports_not_found_and_failures() {
        let kc = MockKeychain::with([
            (("sub-bot", "openai/default"), Ok("sk-mig-openai")),
            (("sub-bot", "openai/work"), Ok("sk-mig-work\n")),
            (("sub-bot", "anthropic/default"), Err(KeychainError::Denied { service: "sub-bot".into(), account: "anthropic/default".into() })),
        ]);
        let stores = Stores { macos: true, managed_dir: None, keychain: &kc };
        let cfg: KeyConfig = serde_json::from_value(serde_json::json!({"providers": {"anthropic": {"accounts": {"ci": {"apiKeyKeychain": {}}}}}})).unwrap();
        let (r, out) = reauth_out(&stores, &cfg, &ReauthOpts::default(), &|_| None);
        assert_eq!(r.unwrap_err().to_string(), "1 keychain item(s) could not be migrated");
        assert_eq!(
            out,
            "openai/default: migrated (keychain service=sub-bot account=openai/default)\n\
             openai/work: migrated (keychain service=sub-bot account=openai/work)\n\
             anthropic/ci: not found (keychain service=sub-bot account=anthropic/ci)\n\
             anthropic/default: FAILED: keychain access denied (service=sub-bot, account=anthropic/default): approve sub-bot in the Keychain prompt, or run `sub-bot keys reauth` once to migrate items saved by sub-bot <= 0.10.2 (after that, upgrades need nothing) (keychain service=sub-bot account=anthropic/default)\n"
        );
        assert!(!out.contains("sk-mig"), "{out}");
        // 重建后的内容与原来一致（trim 后）；失败的条目未被动过
        assert_eq!(kc.get("sub-bot", "openai/default").unwrap().expose(), "sk-mig-openai");
        assert_eq!(kc.items.lock().unwrap()[&("sub-bot".to_string(), "openai/work".to_string())], Ok("sk-mig-work".to_string()));
        assert!(matches!(kc.get("sub-bot", "anthropic/default"), Err(KeychainError::Denied { .. })));
        assert!(kc.get("sub-bot", "anthropic/ci").unwrap_err().is_not_found());
        // 全部成功 ⇒ Ok；--json
        kc.items.lock().unwrap().remove(&("sub-bot".into(), "anthropic/default".into()));
        let (r, out) = reauth_out(&stores, &KeyConfig::default(), &ReauthOpts { json: true, ..Default::default() }, &|_| None);
        r.unwrap();
        let rows: Vec<ReauthRow> = serde_json::from_str(&out).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.status == "migrated" && r.source == "keychain" && r.error.is_none()), "{rows:?}");
        assert!(!out.contains("sk-mig"), "{out}");
        // 其它 Keychain 错误（例如 /usr/bin/security 异常）同样是 FAILED，消息只含 service/account
        let kc = MockKeychain::with([(
            ("sub-bot", "openai/default"),
            Err(KeychainError::Tool { message: "cannot run /usr/bin/security: boom".into(), service: "sub-bot".into(), account: "openai/default".into() }),
        )]);
        let stores = Stores { macos: true, managed_dir: None, keychain: &kc };
        let (r, out) = reauth_out(&stores, &KeyConfig::default(), &ReauthOpts::default(), &|_| None);
        assert!(r.is_err());
        assert_eq!(
            out,
            "openai/default: FAILED: keychain tool failed (service=sub-bot, account=openai/default): cannot run /usr/bin/security: boom (keychain service=sub-bot account=openai/default)\n"
        );
    }

    #[test]
    fn reauth_from_env_never_reads_old_items() {
        let kc = MockKeychain::with([
            (("sub-bot", "openai/default"), Err(KeychainError::Denied { service: "sub-bot".into(), account: "openai/default".into() })),
            (("sub-bot", "openai/work"), Err(KeychainError::Denied { service: "sub-bot".into(), account: "openai/work".into() })),
            (("sub-bot", "openai/proxy"), Err(KeychainError::Denied { service: "sub-bot".into(), account: "openai/proxy".into() })),
            (("sub-bot", "anthropic/default"), Err(KeychainError::Denied { service: "sub-bot".into(), account: "anthropic/default".into() })),
        ]);
        let stores = Stores { macos: true, managed_dir: None, keychain: &kc };
        let cfg: KeyConfig = serde_json::from_value(serde_json::json!({"providers": {"openai": {"accounts": {"proxy": {"apiKeyEnv": "TEST_REAUTH_PROXY"}}}}})).unwrap();
        let env = reauth_env(&[("OPENAI_API_KEY", " sk-env-openai\n"), ("TEST_REAUTH_PROXY", "sk-env-proxy"), ("ANTHROPIC_API_KEY", "sk-env a b")]);
        let (r, out) = reauth_out(&stores, &cfg, &ReauthOpts { from_env: true, ..Default::default() }, &env);
        assert_eq!(r.unwrap_err().to_string(), "2 keychain item(s) could not be migrated");
        assert_eq!(
            out,
            "openai/default: migrated from env OPENAI_API_KEY (keychain service=sub-bot account=openai/default)\n\
             openai/proxy: migrated from env TEST_REAUTH_PROXY (keychain service=sub-bot account=openai/proxy)\n\
             openai/work: FAILED: no environment variable for account 'work' (named accounts do not use OPENAI_API_KEY); use `sub-bot keys set --provider openai --account work` (keychain service=sub-bot account=openai/work)\n\
             anthropic/default: FAILED: env ANTHROPIC_API_KEY must contain only the key (keychain service=sub-bot account=anthropic/default)\n"
        );
        assert!(!out.contains("sk-env"), "{out}");
        assert_eq!(kc.get("sub-bot", "openai/default").unwrap().expose(), "sk-env-openai");
        assert_eq!(kc.get("sub-bot", "openai/proxy").unwrap().expose(), "sk-env-proxy");
        assert!(matches!(kc.get("sub-bot", "openai/work"), Err(KeychainError::Denied { .. })));
        assert!(matches!(kc.get("sub-bot", "anthropic/default"), Err(KeychainError::Denied { .. })));
        // 变量未设置 ⇒ FAILED
        let (r, out) = reauth_out(&stores, &cfg, &ReauthOpts { from_env: true, provider: Some("anthropic"), ..Default::default() }, &|_| None);
        assert!(r.is_err());
        assert_eq!(out, "anthropic/default: FAILED: env ANTHROPIC_API_KEY is not set (keychain service=sub-bot account=anthropic/default)\n");
    }

    #[test]
    fn reauth_filters_by_provider_account_and_service() {
        let kc = MockKeychain::with([
            (("sub-bot", "openai/default"), Ok("sk-f-openai")),
            (("sub-bot", "openai/work"), Ok("sk-f-work")),
            (("sub-bot", "anthropic/default"), Ok("sk-f-anthropic")),
            (("my-keys", "openai/default"), Ok("sk-f-custom")),
            (("my-keys", "claude-prod"), Ok("sk-f-prod")),
        ]);
        let stores = Stores { macos: true, managed_dir: None, keychain: &kc };
        let cfg: KeyConfig = serde_json::from_value(serde_json::json!({"providers": {"anthropic": {"apiKeyKeychain": {"service": "my-keys", "account": "claude-prod"}}}})).unwrap();
        let names = |opts: ReauthOpts<'_>| -> Vec<String> {
            let (r, out) = reauth_out(&stores, &cfg, &ReauthOpts { dry_run: true, ..opts }, &|_| None);
            r.unwrap();
            out.lines().filter(|l| l.contains("would migrate")).map(|l| l.split(" (").nth(1).unwrap().trim_end_matches(')').to_string()).collect()
        };
        assert_eq!(names(ReauthOpts { provider: Some("openai"), ..Default::default() }), ["keychain service=sub-bot account=openai/default", "keychain service=sub-bot account=openai/work"]);
        assert_eq!(names(ReauthOpts { account: Some("work"), ..Default::default() }), ["keychain service=sub-bot account=openai/work"]);
        assert_eq!(
            names(ReauthOpts { provider: Some("anthropic"), account: Some("default"), ..Default::default() }),
            ["keychain service=my-keys account=claude-prod", "keychain service=sub-bot account=anthropic/default"]
        );
        // --service：只看该 service（配置里指向其它 service 的条目不包括）
        assert_eq!(names(ReauthOpts { service: Some("my-keys"), ..Default::default() }), ["keychain service=my-keys account=openai/default", "keychain service=my-keys account=claude-prod"]);
        assert_eq!(names(ReauthOpts { service: Some("my-keys"), provider: Some("anthropic"), ..Default::default() }), ["keychain service=my-keys account=claude-prod"]);
        // 没有目标
        let (r, out) = reauth_out(&stores, &cfg, &ReauthOpts { provider: Some("openai"), account: Some("nope"), ..Default::default() }, &|_| None);
        r.unwrap();
        assert_eq!(out, "no keychain items to migrate (service=sub-bot)\n");
        let (r, out) = reauth_out(&stores, &KeyConfig::default(), &ReauthOpts { service: Some("empty"), ..Default::default() }, &|_| None);
        r.unwrap();
        assert_eq!(out, "no keychain items to migrate (service=empty; pass --config to include apiKeyKeychain entries)\n");
        // 参数校验
        assert!(reauth(&stores, &cfg, &ReauthOpts { provider: Some("faux"), ..Default::default() }, &|_| None, &mut Vec::new()).unwrap_err().to_string().contains("unknown provider 'faux'"));
        assert!(reauth(&stores, &cfg, &ReauthOpts { account: Some("a/b"), ..Default::default() }, &|_| None, &mut Vec::new()).unwrap_err().to_string().contains("invalid account name 'a/b'"));
        assert_eq!(reauth(&stores, &cfg, &ReauthOpts { service: Some(" "), ..Default::default() }, &|_| None, &mut Vec::new()).unwrap_err().to_string(), "--service must not be blank");
        // 实际迁移只动筛选到的条目
        let (r, _) = reauth_out(&stores, &cfg, &ReauthOpts { provider: Some("openai"), account: Some("work"), ..Default::default() }, &|_| None);
        r.unwrap();
        assert_eq!(kc.items.lock().unwrap().len(), 5);
    }

    /// 只有读取（`get`）阻塞的 mock：属性搜索照常返回（真实 Keychain 的 list 不触发 ACL 对话框）。
    struct SlowGet(MockKeychain, Duration);

    impl KeychainReader for SlowGet {
        fn get(&self, service: &str, account: &str) -> Result<Secret, KeychainError> {
            std::thread::sleep(self.1);
            self.0.get(service, account)
        }
    }

    impl KeychainStore for SlowGet {
        fn set(&self, service: &str, account: &str, secret: &Secret) -> Result<(), KeychainError> {
            self.0.set(service, account, secret)
        }
        fn delete(&self, service: &str, account: &str) -> Result<(), KeychainError> {
            self.0.delete(service, account)
        }
        fn list(&self, service: &str) -> Result<Vec<String>, KeychainError> {
            self.0.list(service)
        }
    }

    /// 旧条目的授权对话框无人响应：读取限时失败（打印等待提示），该条目 FAILED 而不是挂起。
    #[test]
    fn reauth_times_out_on_blocked_item_and_prints_hint() {
        let slow = SlowGet(MockKeychain::with([(("sub-bot", "openai/default"), Ok("sk-blocked"))]), Duration::from_secs(30));
        let (hints, hook) = hint_recorder();
        let timed = TimedStore::new(Arc::new(slow), Duration::from_secs(1), Duration::from_millis(100), hook);
        let stores = Stores { macos: true, managed_dir: None, keychain: &timed };
        let started = std::time::Instant::now();
        let (r, out) = reauth_out(&stores, &KeyConfig::default(), &ReauthOpts::default(), &|_| None);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(r.unwrap_err().to_string(), "1 keychain item(s) could not be migrated");
        assert!(out.starts_with("openai/default: FAILED: no answer from macOS Keychain within 1s (service=sub-bot, account=openai/default)"), "{out}");
        assert!(out.contains("sub-bot keys reauth") && !out.contains("sk-blocked"), "{out}");
        assert_eq!(*hints.lock().unwrap(), [keychain::wait_hint("sub-bot", "openai/default")]);
        // 列举阶段超时 ⇒ 整体报错（没有逐条输出）
        let slow = MockKeychain::with([(("sub-bot", "openai/default"), Ok("sk-blocked"))]).delayed(Duration::from_secs(30));
        let (_, hook) = hint_recorder();
        let timed = TimedStore::new(Arc::new(slow), Duration::from_secs(1), Duration::from_millis(100), hook);
        let stores = Stores { macos: true, managed_dir: None, keychain: &timed };
        let (r, out) = reauth_out(&stores, &KeyConfig::default(), &ReauthOpts::default(), &|_| None);
        assert!(r.unwrap_err().to_string().starts_with("cannot list keychain items (service=sub-bot): no answer from macOS Keychain within 1s"));
        assert!(out.is_empty());
    }
}
