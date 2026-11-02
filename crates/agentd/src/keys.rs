//! `sub-bot keys set|list|delete|check`：管理 provider API key 的受管存储。
//!
//! - macOS：Keychain generic password，service `sub-bot`、account `<provider>/<account>`；
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

fn require_provider(cli: &crate::Cli) -> anyhow::Result<&'static ProviderKey> {
    let id = cli.provider.as_deref().ok_or_else(|| anyhow::anyhow!("--provider is required (one of: {})", PROVIDER_KEYS.iter().map(|k| k.provider).collect::<Vec<_>>().join(", ")))?;
    provider_key(id).ok_or_else(|| anyhow::anyhow!("unknown provider '{id}' (expected one of: {})", PROVIDER_KEYS.iter().map(|k| k.provider).collect::<Vec<_>>().join(", ")))
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
            print_rows(&list_rows(&stores, &cfg, cli.provider.as_deref(), files, &getenv), *json, &mut out)
        }
        KeysCmd::Check => {
            let cfg = match &cli.config {
                Some(p) => Config::load(p).map_err(|e| anyhow::anyhow!("configuration load failed: {e}"))?,
                None => Config::fallback_for(cli.provider.as_deref(), cli.model.clone(), "", cli.workspace.clone()),
            };
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
}
