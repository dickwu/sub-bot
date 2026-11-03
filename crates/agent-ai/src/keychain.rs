//! macOS Keychain（generic password）key 来源。
//!
//! - 读取通过可注入的 [`KeychainReader`]（生产用 [`SystemKeychain`]，测试用 mock）；
//! - 写入/删除/枚举（[`set_generic`] / [`delete_generic`] / [`list_accounts`]）只给 `sub-bot keys` CLI 用；
//! - 平台相关代码全部集中在私有 `sys` 模块：macOS 上读/写/删通过 Apple 的 `/usr/bin/security`（见 [`security_tool`]），
//!   枚举用 `security-framework` 的属性搜索；其它平台是返回 [`KeychainError::Unsupported`] 的 stub。
//!
//! - 为什么走 `/usr/bin/security`（v0.10.3 起）：sub-bot 自己用 SecItemAdd 创建的条目，ACL 只信任创建它的二进制的 cdhash
//!   （Homebrew 二进制是 ad-hoc 签名，partition list 也是 `cdhash:<hash>`），每次 `brew upgrade` 都换 cdhash ⇒ 新二进制读取时
//!   弹授权对话框（launchd 下无人点击则超时）。`security add-generic-password` 创建的条目 ACL 是
//!   `identifier "com.apple.security" and anchor apple`、partition `apple-tool:`，之后 `security find-generic-password -w`
//!   读取永不弹窗，与 gh / go-keyring 的做法相同。
//! - 旧版本创建的条目（ACL 绑定旧 cdhash）读取仍可能被授权对话框无限期阻塞，因此调用方用 [`TimedKeychain`] /
//!   [`with_timeout`] 在独立线程上读取并限时等待；`sub-bot keys reauth` 一次性把它们迁移成新形式。
//!
//! 约定：错误信息只含 service/account 名，绝不包含条目内容。
use crate::secrets::{validate_key_text, wipe_bytes, Secret};
use std::{sync::Arc, time::Duration};

/// 受管 Keychain 条目的默认 service；account 约定为 `<provider>/<account>`。
pub const DEFAULT_SERVICE: &str = "sub-bot";

/// errSecItemNotFound
pub const ERR_ITEM_NOT_FOUND: i32 = -25300;
/// errSecInteractionNotAllowed
pub const ERR_INTERACTION_NOT_ALLOWED: i32 = -25308;
/// errSecAuthFailed
pub const ERR_AUTH_FAILED: i32 = -25293;
/// errSecUserCanceled
pub const ERR_USER_CANCELED: i32 = -128;

/// Keychain 读取的默认超时（秒）；配置 `keychainTimeoutSecs` / `--keychain-timeout-secs` 覆盖。
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// 读取超过该时长仍未返回才提示“可能在等授权对话框”（正常读取远快于此，避免噪音）。
pub const WAIT_HINT_GRACE: Duration = Duration::from_secs(1);

/// 当前构建是否支持 Keychain。
pub const fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// 受管条目的 Keychain account 名：`<provider>/<account>`。
pub fn managed_account(provider: &str, account: &str) -> String {
    format!("{provider}/{account}")
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeychainError {
    #[error("keychain item not found (service={service}, account={account})")]
    NotFound { service: String, account: String },
    /// errSecInteractionNotAllowed (-25308)：login keychain 锁定且当前会话无法弹窗（ssh / launchd 无 GUI）
    #[error(
        "keychain interaction not allowed (service={service}, account={account}): the login keychain is locked and no UI is available; run `security unlock-keychain` in this session or start sub-bot from a GUI login session"
    )]
    InteractionNotAllowed { service: String, account: String },
    /// errSecAuthFailed (-25293) / errSecUserCanceled (-128)：用户在 ACL 提示里拒绝/取消，或条目由旧版本 sub-bot 创建（ACL 绑定旧二进制）且未授权
    #[error(
        "keychain access denied (service={service}, account={account}): approve sub-bot in the Keychain prompt, or run `sub-bot keys reauth` once to migrate items saved by sub-bot <= 0.10.2 (after that, upgrades need nothing)"
    )]
    Denied { service: String, account: String },
    #[error("keychain item is not valid UTF-8 or is empty (service={service}, account={account})")]
    Invalid { service: String, account: String },
    #[error("keychain is only supported on macOS")]
    Unsupported,
    /// 未映射的错误：`code` 是 Security.framework 的 OSStatus（属性搜索），或 `/usr/bin/security` 的退出码（读/写/删；= OSStatus & 0xff）
    #[error("keychain error {code} (service={service}, account={account})")]
    Other { code: i32, service: String, account: String },
    /// `/usr/bin/security` 无法运行或行为异常（`message` 只含退出码 / IO 错误 / 名称规则，不含条目内容）
    #[error("keychain tool failed (service={service}, account={account}): {message}")]
    Tool { message: String, service: String, account: String },
    /// 读取在 `secs` 秒内没有返回（多半是授权对话框在等人点击，或会话没有 GUI 无法显示对话框）
    #[error(
        "no answer from macOS Keychain within {secs}s (service={service}, account={account}): an authorization dialog is probably waiting (the item was saved by sub-bot <= 0.10.2 and the binary changed, e.g. after `brew upgrade`) or this session has no GUI to show it; click \"Always Allow\" in the dialog, then run `sub-bot keys reauth` once (or `sub-bot keys reauth --from-env` / `sub-bot keys set`) so later upgrades need nothing, or use apiKeyFile for headless use (ssh / LaunchDaemon)"
    )]
    Timeout { service: String, account: String, secs: u64 },
}

impl KeychainError {
    /// 把 Security.framework 的 OSStatus 映射成错误（平台无关，便于在 Linux 上单测）。
    pub fn from_status(code: i32, service: &str, account: &str) -> Self {
        let (service, account) = (service.to_string(), account.to_string());
        match code {
            ERR_ITEM_NOT_FOUND => Self::NotFound { service, account },
            ERR_INTERACTION_NOT_ALLOWED => Self::InteractionNotAllowed { service, account },
            ERR_AUTH_FAILED | ERR_USER_CANCELED => Self::Denied { service, account },
            code => Self::Other { code, service, account },
        }
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }
}

/// 把条目 data 转成 `Secret`：UTF-8、trim 后非空且不含内部空白；原始字节用完即清零。
pub fn secret_from_bytes(bytes: Vec<u8>, service: &str, account: &str) -> Result<Secret, KeychainError> {
    let invalid = || KeychainError::Invalid { service: service.into(), account: account.into() };
    match String::from_utf8(bytes) {
        Ok(text) => {
            let result = validate_key_text(&text).map_err(|_| invalid());
            wipe_bytes(text.into_bytes());
            result
        }
        Err(e) => {
            wipe_bytes(e.into_bytes());
            Err(invalid())
        }
    }
}

/// 可注入的读取器（测试用 mock；生产用 [`SystemKeychain`]）。
pub trait KeychainReader: Send + Sync {
    fn get(&self, service: &str, account: &str) -> Result<Secret, KeychainError>;
}

/// 读取等待超过 [`WAIT_HINT_GRACE`] 时给用户的提示（只含 service/account 名）。
pub fn wait_hint(service: &str, account: &str) -> String {
    format!("waiting for macOS Keychain authorization for {service}/{account} — check for a dialog on the Mac's screen and choose \"Always Allow\"; then run `sub-bot keys reauth` once so later upgrades need no re-authorization")
}

/// 在独立线程上执行 Keychain 调用 `call`，最多等待 `timeout`：
/// - `grace`（< `timeout`）内未返回 ⇒ 调用一次 `on_wait`（提示用户去看授权对话框）；
/// - `timeout` 内未返回 ⇒ [`KeychainError::Timeout`]。
///
/// 超时后读取线程被放弃（不 join）：它可能一直阻塞在 Security.framework 里直到有人响应对话框；
/// 进程退出不等待它。迟到的结果（含 `Secret`）随 channel 一起丢弃，`Secret` 析构时清零。
/// 线程 panic 会在调用方线程重新抛出。
pub fn with_timeout<T: Send + 'static>(
    service: &str,
    account: &str,
    timeout: Duration,
    grace: Duration,
    on_wait: impl FnOnce(),
    call: impl FnOnce() -> Result<T, KeychainError> + Send + 'static,
) -> Result<T, KeychainError> {
    use std::sync::mpsc::{sync_channel, RecvTimeoutError};
    let started = std::time::Instant::now();
    // 容量 1：超时后读取线程的 send 不会阻塞（接收端已丢弃时直接失败）
    let (tx, rx) = sync_channel(1);
    let handle = std::thread::Builder::new()
        .name("keychain-read".into())
        .spawn(move || {
            let _ = tx.send(call());
        })
        .expect("spawn keychain reader thread");
    let rethrow = |handle: std::thread::JoinHandle<()>| -> ! {
        match handle.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => unreachable!("keychain reader exited without sending a result"),
        }
    };
    let grace = grace.min(timeout);
    match rx.recv_timeout(grace) {
        Ok(result) => return result,
        Err(RecvTimeoutError::Disconnected) => rethrow(handle),
        Err(RecvTimeoutError::Timeout) if grace < timeout => on_wait(),
        Err(RecvTimeoutError::Timeout) => {}
    }
    match rx.recv_timeout(timeout.saturating_sub(started.elapsed())) {
        Ok(result) => result,
        Err(RecvTimeoutError::Disconnected) => rethrow(handle),
        // 不 join：线程可能永远阻塞
        Err(RecvTimeoutError::Timeout) => Err(KeychainError::Timeout { service: service.into(), account: account.into(), secs: timeout.as_secs() }),
    }
}

/// 等待提示回调（参数为 service、account）。
pub type WaitHook = Box<dyn Fn(&str, &str) + Send + Sync>;

/// 限时读取的包装：每次 `get` 都在独立线程上调用内层读取器（见 [`with_timeout`]）。
pub struct TimedKeychain {
    inner: Arc<dyn KeychainReader>,
    timeout: Duration,
    grace: Duration,
    on_wait: WaitHook,
}

impl TimedKeychain {
    /// `grace` 默认 [`WAIT_HINT_GRACE`]；`on_wait` 默认什么都不做。
    pub fn new(inner: Arc<dyn KeychainReader>, timeout: Duration) -> Self {
        Self { inner, timeout, grace: WAIT_HINT_GRACE, on_wait: Box::new(|_, _| {}) }
    }

    pub fn grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// 读取超过 grace 仍未返回时调用（参数为 service、account；每次读取最多一次）。
    pub fn on_wait(mut self, hook: impl Fn(&str, &str) + Send + Sync + 'static) -> Self {
        self.on_wait = Box::new(hook);
        self
    }
}

impl KeychainReader for TimedKeychain {
    fn get(&self, service: &str, account: &str) -> Result<Secret, KeychainError> {
        let inner = self.inner.clone();
        let (s, a) = (service.to_string(), account.to_string());
        with_timeout(service, account, self.timeout, self.grace, || (self.on_wait)(service, account), move || inner.get(&s, &a))
    }
}

/// 系统 Keychain（非 macOS 上总是返回 `Unsupported`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemKeychain;

impl KeychainReader for SystemKeychain {
    fn get(&self, service: &str, account: &str) -> Result<Secret, KeychainError> {
        sys::get(service, account)
    }
}

/// 写入（存在则先删除再重建：条目的 ACL / partition 归属 `/usr/bin/security`，之后 brew upgrade 无需重新授权）。
pub fn set_generic(service: &str, account: &str, secret: &Secret) -> Result<(), KeychainError> {
    sys::set(service, account, secret)
}

/// 删除；不存在 ⇒ `NotFound`。
pub fn delete_generic(service: &str, account: &str) -> Result<(), KeychainError> {
    sys::delete(service, account)
}

/// 按 service 枚举 account 名（只读属性、不读 data ⇒ 不触发 ACL 提示）。非 macOS 返回空列表。
pub fn list_accounts(service: &str) -> Result<Vec<String>, KeychainError> {
    sys::list(service)
}

/// 通过 Apple 的 `/usr/bin/security` 读/写/删 generic password（平台无关：命令构造、退出码映射、hex 编码、名称校验都在这里，
/// 在 Linux 上单测；进程执行抽象为 [`security_tool::Runner`]，macOS 生产用 [`security_tool::ProcessRunner`]）。
///
/// - 读取：`security find-generic-password -s S -a A -w`（绝对路径、不经 shell、stdin 为 /dev/null），stdout 即条目内容；
/// - 写入：绝不把 secret 放进 argv（`ps` 可见）——spawn `security -i`，向其 stdin 写一行
///   `add-generic-password -U -s S -a A -l sub-bot -X <hex>` 后关闭。先删除已有条目（旧版本创建的条目 `-U` 更新会保留旧 ACL），
///   写完再读回比对（`security -i` 失败时退出码也可能是 0）；
/// - 删除：`security delete-generic-password -s S -a A`；
/// - 退出码 = OSStatus & 0xff：44 ⇒ -25300、36 ⇒ -25308、51 ⇒ -25293、128 ⇒ -128，其它 ⇒ [`KeychainError::Other`]。
///
/// 约定：stdout 绝不进入错误信息或日志；stderr 只在 debug 级别记录清洗后的第一行（find/delete 失败时 stderr 不含条目内容；
/// add 的 stderr 不记录）。
pub mod security_tool {
    use super::{secret_from_bytes, KeychainError};
    use crate::secrets::{wipe_bytes, Secret};
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// 只用绝对路径，不依赖 PATH。
    pub const SECURITY_BIN: &str = "/usr/bin/security";
    /// 新建条目的显示名（`-l`）。
    pub const ITEM_LABEL: &str = "sub-bot";
    /// 退出码 ⇒ OSStatus（`security` 以 `OSStatus & 0xff` 退出）。
    pub const EXIT_ITEM_NOT_FOUND: i32 = 44;
    pub const EXIT_INTERACTION_NOT_ALLOWED: i32 = 36;
    pub const EXIT_AUTH_FAILED: i32 = 51;
    pub const EXIT_USER_CANCELED: i32 = 128;

    /// 一次 `security` 调用的结果。
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct ToolOutput {
        /// 退出码；被信号终止时为 `None`
        pub code: Option<i32>,
        pub stdout: Vec<u8>,
        pub stderr: Vec<u8>,
    }

    /// 进程执行器：运行 `/usr/bin/security <args>`，`stdin` 为 `Some` 时写入后关闭（否则 /dev/null），捕获 stdout/stderr。
    pub trait Runner {
        fn run(&self, args: &[String], stdin: Option<&[u8]>) -> std::io::Result<ToolOutput>;
    }

    /// 真实执行（`std::process::Command`，不经 shell）。
    #[derive(Debug, Clone, Copy, Default)]
    pub struct ProcessRunner;

    impl Runner for ProcessRunner {
        fn run(&self, args: &[String], stdin: Option<&[u8]>) -> std::io::Result<ToolOutput> {
            let mut cmd = Command::new(SECURITY_BIN);
            cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
            let mut child = cmd.spawn()?;
            if let Some(input) = stdin {
                if let Some(mut pipe) = child.stdin.take() {
                    // 写失败（例如 security 已退出）不致命：按退出码 / 读回校验处理
                    let _ = pipe.write_all(input).and_then(|()| pipe.flush());
                    drop(pipe);
                }
            }
            let out = child.wait_with_output()?;
            Ok(ToolOutput { code: out.status.code(), stdout: out.stdout, stderr: out.stderr })
        }
    }

    /// 可以不加引号放进 `security -i` 命令行的名称：`[A-Za-z0-9._/@:-]`（非空；`_` 是账号名允许的字符）。
    pub fn safe_name(name: &str) -> bool {
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'@' | b':' | b'-'))
    }

    fn ensure_safe_names(service: &str, account: &str) -> Result<(), KeychainError> {
        for (what, value) in [("service", service), ("account", account)] {
            if !safe_name(value) {
                return Err(KeychainError::Tool {
                    message: format!("{what} name must be non-empty and contain only [A-Za-z0-9._/@:-] to be written with /usr/bin/security"),
                    service: service.into(),
                    account: account.into(),
                });
            }
        }
        Ok(())
    }

    /// 大写 hex（`-X` 接受的形式）。
    pub fn hex_encode(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0x0f) as usize] as char);
        }
        s
    }

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    /// `find-generic-password -s S -a A -w`
    pub fn find_args(service: &str, account: &str) -> Vec<String> {
        strings(&["find-generic-password", "-s", service, "-a", account, "-w"])
    }

    /// `delete-generic-password -s S -a A`
    pub fn delete_args(service: &str, account: &str) -> Vec<String> {
        strings(&["delete-generic-password", "-s", service, "-a", account])
    }

    /// 交互模式的参数（命令从 stdin 读取）。
    pub fn interactive_args() -> Vec<String> {
        strings(&["-i"])
    }

    /// 写给 `security -i` 的一行：`add-generic-password -U -s S -a A -l sub-bot -X <hex>\n`（调用方负责名称已校验、用完清零）。
    pub fn add_command_line(service: &str, account: &str, hex: &str) -> String {
        format!("add-generic-password -U -s {service} -a {account} -l {ITEM_LABEL} -X {hex}\n")
    }

    /// 退出码 ⇒ 错误（`Some(0)` 不应传进来）。
    pub fn error_from_exit(code: Option<i32>, service: &str, account: &str) -> KeychainError {
        let (s, a) = (service.to_string(), account.to_string());
        match code {
            Some(EXIT_ITEM_NOT_FOUND) => KeychainError::NotFound { service: s, account: a },
            Some(EXIT_INTERACTION_NOT_ALLOWED) => KeychainError::InteractionNotAllowed { service: s, account: a },
            Some(EXIT_AUTH_FAILED) | Some(EXIT_USER_CANCELED) => KeychainError::Denied { service: s, account: a },
            Some(code) => KeychainError::Other { code, service: s, account: a },
            None => KeychainError::Tool { message: format!("{SECURITY_BIN} was terminated by a signal"), service: s, account: a },
        }
    }

    fn spawn_error(e: std::io::Error, service: &str, account: &str) -> KeychainError {
        KeychainError::Tool { message: format!("cannot run {SECURITY_BIN}: {e}"), service: service.into(), account: account.into() }
    }

    /// stderr 的第一行，去掉不可打印字符、截断到 160 字符（只用于 debug 日志）。
    pub fn stderr_summary(stderr: &[u8]) -> String {
        let text = String::from_utf8_lossy(stderr);
        let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        line.chars().filter(|c| !c.is_control()).take(160).collect()
    }

    /// `find-generic-password -w` 对含不可打印字节的数据输出 `0x<hex>  "<escaped>"` 形式；这种内容不可能是合法 key。
    pub fn looks_like_hex_dump(stdout: &[u8]) -> bool {
        let Some(rest) = stdout.strip_prefix(b"0x") else { return false };
        let digits = rest.iter().take_while(|b| b.is_ascii_hexdigit()).count();
        digits > 0 && digits % 2 == 0 && matches!(rest.get(digits), None | Some(b' ' | b'\n' | b'\r'))
    }

    /// 读取条目内容（stdout 为内容；退出码映射见模块文档）。名称不做限制：配置可以指向任意已有条目。
    pub fn get_with(runner: &dyn Runner, service: &str, account: &str) -> Result<Secret, KeychainError> {
        let out = runner.run(&find_args(service, account), None).map_err(|e| spawn_error(e, service, account))?;
        if out.code != Some(0) {
            tracing::debug!(service, account, code = ?out.code, stderr = %stderr_summary(&out.stderr), "security find-generic-password failed");
            wipe_bytes(out.stdout);
            return Err(error_from_exit(out.code, service, account));
        }
        if looks_like_hex_dump(&out.stdout) {
            wipe_bytes(out.stdout);
            return Err(KeychainError::Invalid { service: service.into(), account: account.into() });
        }
        secret_from_bytes(out.stdout, service, account)
    }

    /// 删除条目；不存在 ⇒ `NotFound`。
    pub fn delete_with(runner: &dyn Runner, service: &str, account: &str) -> Result<(), KeychainError> {
        let out = runner.run(&delete_args(service, account), None).map_err(|e| spawn_error(e, service, account))?;
        // delete-generic-password 的 stdout 只有属性（无内容），仍不记录
        drop(out.stdout);
        if out.code != Some(0) {
            tracing::debug!(service, account, code = ?out.code, stderr = %stderr_summary(&out.stderr), "security delete-generic-password failed");
            return Err(error_from_exit(out.code, service, account));
        }
        Ok(())
    }

    /// 写入：删除旧条目（不存在则忽略）→ `security -i` + `add-generic-password -X <hex>` → 读回比对。
    pub fn set_with(runner: &dyn Runner, service: &str, account: &str, secret: &Secret) -> Result<(), KeychainError> {
        ensure_safe_names(service, account)?;
        match delete_with(runner, service, account) {
            Ok(()) => tracing::debug!(service, account, "removed existing keychain item before re-adding it via /usr/bin/security"),
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e),
        }
        let hex = hex_encode(secret.expose().as_bytes());
        let line = add_command_line(service, account, &hex);
        wipe_bytes(hex.into_bytes());
        let result = runner.run(&interactive_args(), Some(line.as_bytes()));
        wipe_bytes(line.into_bytes());
        let out = result.map_err(|e| spawn_error(e, service, account))?;
        wipe_bytes(out.stdout);
        if out.code != Some(0) {
            // 不记录 stderr：交互模式的错误输出理论上可能回显命令行
            tracing::debug!(service, account, code = ?out.code, "security -i add-generic-password failed");
            return Err(error_from_exit(out.code, service, account));
        }
        // security -i 失败时退出码也可能是 0 ⇒ 读回校验
        let back = get_with(runner, service, account).map_err(|e| match e {
            KeychainError::NotFound { service, account } => {
                KeychainError::Tool { message: format!("{SECURITY_BIN} add-generic-password reported success but the item cannot be found"), service, account }
            }
            e => e,
        })?;
        if back.expose() != secret.expose() {
            return Err(KeychainError::Tool { message: format!("item written by {SECURITY_BIN} does not read back as the value given"), service: service.into(), account: account.into() });
        }
        tracing::debug!(service, account, "keychain item written via /usr/bin/security and verified");
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use super::security_tool::{self, ProcessRunner};
    use super::{KeychainError, ERR_ITEM_NOT_FOUND};
    use crate::secrets::Secret;
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};

    pub fn get(service: &str, account: &str) -> Result<Secret, KeychainError> {
        security_tool::get_with(&ProcessRunner, service, account)
    }

    pub fn set(service: &str, account: &str, secret: &Secret) -> Result<(), KeychainError> {
        security_tool::set_with(&ProcessRunner, service, account, secret)
    }

    pub fn delete(service: &str, account: &str) -> Result<(), KeychainError> {
        security_tool::delete_with(&ProcessRunner, service, account)
    }

    /// 属性搜索（不读 data ⇒ 不触发 ACL 提示），仍用 Security.framework。
    pub fn list(service: &str) -> Result<Vec<String>, KeychainError> {
        let results = match ItemSearchOptions::new().class(ItemClass::generic_password()).service(service).load_attributes(true).load_data(false).limit(Limit::Max(1000)).search() {
            Ok(results) => results,
            Err(e) if e.code() == ERR_ITEM_NOT_FOUND => return Ok(vec![]),
            Err(e) => return Err(KeychainError::from_status(e.code(), service, "*")),
        };
        // kSecAttrAccount 的键名是 "acct"
        let mut accounts: Vec<String> = results.iter().filter_map(|r| r.simplify_dict()).filter_map(|mut d| d.remove("acct")).collect();
        accounts.sort();
        accounts.dedup();
        Ok(accounts)
    }
}

#[cfg(not(target_os = "macos"))]
mod sys {
    use super::KeychainError;
    use crate::secrets::Secret;

    pub fn get(_service: &str, _account: &str) -> Result<Secret, KeychainError> {
        Err(KeychainError::Unsupported)
    }

    pub fn set(_service: &str, _account: &str, _secret: &Secret) -> Result<(), KeychainError> {
        Err(KeychainError::Unsupported)
    }

    pub fn delete(_service: &str, _account: &str) -> Result<(), KeychainError> {
        Err(KeychainError::Unsupported)
    }

    pub fn list(_service: &str) -> Result<Vec<String>, KeychainError> {
        Ok(vec![])
    }
}

/// 测试用的内存 Keychain（单测/agentd 测试共用）。
pub mod mock {
    use super::{secret_from_bytes, KeychainError, KeychainReader};
    use crate::secrets::Secret;
    use std::{collections::BTreeMap, sync::Mutex, time::Duration};

    /// `(service, account)` → 条目内容或预设错误。
    #[derive(Default)]
    pub struct MockKeychain {
        pub items: Mutex<BTreeMap<(String, String), Result<String, KeychainError>>>,
        /// 每次 `get` 先阻塞这么久（模拟等待授权对话框）
        pub delay: Option<Duration>,
    }

    impl MockKeychain {
        pub fn with(entries: impl IntoIterator<Item = ((&'static str, &'static str), Result<&'static str, KeychainError>)>) -> Self {
            let items = entries.into_iter().map(|((s, a), v)| ((s.to_string(), a.to_string()), v.map(str::to_string))).collect();
            Self { items: Mutex::new(items), delay: None }
        }

        /// 每次 `get` 前阻塞 `delay`。
        pub fn delayed(mut self, delay: Duration) -> Self {
            self.delay = Some(delay);
            self
        }
    }

    impl KeychainReader for MockKeychain {
        fn get(&self, service: &str, account: &str) -> Result<Secret, KeychainError> {
            if let Some(delay) = self.delay {
                std::thread::sleep(delay);
            }
            match self.items.lock().unwrap().get(&(service.to_string(), account.to_string())) {
                Some(Ok(v)) => secret_from_bytes(v.clone().into_bytes(), service, account),
                Some(Err(e)) => Err(e.clone()),
                None => Err(KeychainError::NotFound { service: service.into(), account: account.into() }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockKeychain;
    use super::*;

    #[test]
    fn status_codes_map_to_errors() {
        assert!(KeychainError::from_status(-25300, "s", "a").is_not_found());
        assert_eq!(KeychainError::from_status(-25308, "s", "a"), KeychainError::InteractionNotAllowed { service: "s".into(), account: "a".into() });
        assert_eq!(KeychainError::from_status(-25293, "s", "a"), KeychainError::Denied { service: "s".into(), account: "a".into() });
        assert_eq!(KeychainError::from_status(-128, "s", "a"), KeychainError::Denied { service: "s".into(), account: "a".into() });
        assert_eq!(KeychainError::from_status(-1, "s", "a"), KeychainError::Other { code: -1, service: "s".into(), account: "a".into() });
        let e = KeychainError::from_status(-25308, "sub-bot", "openai/default").to_string();
        assert!(e.contains("service=sub-bot") && e.contains("account=openai/default") && e.contains("security unlock-keychain"), "{e}");
        let e = KeychainError::from_status(-25293, "sub-bot", "openai/default").to_string();
        assert!(e.contains("sub-bot keys reauth") && !e.contains("keys set"), "{e}");
    }

    /// `security` 退出码（OSStatus & 0xff）映射与 Security.framework 的 OSStatus 映射一致。
    #[test]
    fn security_tool_exit_codes_map_like_os_status() {
        use security_tool::error_from_exit;
        for (exit, status) in [(44, ERR_ITEM_NOT_FOUND), (36, ERR_INTERACTION_NOT_ALLOWED), (51, ERR_AUTH_FAILED), (128, ERR_USER_CANCELED)] {
            assert_eq!(status & 0xff, exit, "{status}");
            assert_eq!(error_from_exit(Some(exit), "s", "a"), KeychainError::from_status(status, "s", "a"));
        }
        assert_eq!(error_from_exit(Some(1), "s", "a"), KeychainError::Other { code: 1, service: "s".into(), account: "a".into() });
        let e = error_from_exit(None, "s", "a");
        assert!(matches!(&e, KeychainError::Tool { message, .. } if message.contains("signal")), "{e}");
        assert_eq!(e.to_string(), "keychain tool failed (service=s, account=a): /usr/bin/security was terminated by a signal");
    }

    #[test]
    fn security_tool_command_construction() {
        use security_tool::*;
        assert_eq!(find_args("sub-bot", "openai/default"), ["find-generic-password", "-s", "sub-bot", "-a", "openai/default", "-w"]);
        assert_eq!(delete_args("sub-bot", "openai/default"), ["delete-generic-password", "-s", "sub-bot", "-a", "openai/default"]);
        assert_eq!(interactive_args(), ["-i"]);
        assert_eq!(hex_encode(b""), "");
        assert_eq!(hex_encode(b"sk-\x00\xff"), "736B2D00FF");
        assert_eq!(add_command_line("sub-bot", "openai/work_1", "736B"), "add-generic-password -U -s sub-bot -a openai/work_1 -l sub-bot -X 736B\n");
        for ok in ["sub-bot", "openai/default", "a.b_c-d@e:f/g", "0"] {
            assert!(safe_name(ok), "{ok}");
        }
        for bad in ["", "a b", "a\"b", "a'b", "a\nb", "a;b", "a$b", "中文", "a\\b"] {
            assert!(!safe_name(bad), "{bad:?}");
        }
        assert!(looks_like_hex_dump(b"0xDEAD  \"\\336\\255\"\n"));
        assert!(looks_like_hex_dump(b"0x00\n"));
        assert!(!looks_like_hex_dump(b"0xdeadbeefy\n"));
        assert!(!looks_like_hex_dump(b"sk-abc\n"));
        assert!(!looks_like_hex_dump(b"0x\n"));
        assert_eq!(
            stderr_summary(b"\nsecurity: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.\n"),
            "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain."
        );
        assert_eq!(stderr_summary(b"a\x07b\tc"), "abc");
        assert_eq!(stderr_summary(&[b'x'; 300]).len(), 160);
        assert_eq!(stderr_summary(b""), "");
    }

    /// 一次调用的 (argv, stdin)。
    type Call = (Vec<String>, Option<Vec<u8>>);

    /// 假的 `security`：记录每次调用的 argv / stdin，模拟一个内存 keychain。
    #[derive(Default)]
    struct FakeTool {
        calls: std::sync::Mutex<Vec<Call>>,
        items: std::sync::Mutex<std::collections::BTreeMap<(String, String), Vec<u8>>>,
        /// 覆盖 find 的退出码（模拟 ACL 拒绝等）
        find_exit: Option<i32>,
        /// `-i` 不实际写入（模拟静默失败）
        add_silently_fails: bool,
        /// `-i` 写入这个值而不是命令行给的（模拟写坏）
        add_writes: Option<Vec<u8>>,
        /// spawn 失败
        spawn_error: bool,
    }

    impl security_tool::Runner for FakeTool {
        fn run(&self, args: &[String], stdin: Option<&[u8]>) -> std::io::Result<security_tool::ToolOutput> {
            self.calls.lock().unwrap().push((args.to_vec(), stdin.map(<[u8]>::to_vec)));
            if self.spawn_error {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory (os error 2)"));
            }
            let arg = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].clone()).unwrap();
            let out = |code: i32, stdout: Vec<u8>, stderr: &str| Ok(security_tool::ToolOutput { code: Some(code), stdout, stderr: stderr.as_bytes().to_vec() });
            let mut items = self.items.lock().unwrap();
            match args[0].as_str() {
                "find-generic-password" => {
                    if let Some(code) = self.find_exit {
                        return out(code, vec![], "security: SecKeychainSearchCopyNext: simulated failure");
                    }
                    match items.get(&(arg("-s"), arg("-a"))) {
                        Some(data) => out(0, [data.as_slice(), b"\n"].concat(), ""),
                        None => out(44, vec![], "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain."),
                    }
                }
                "delete-generic-password" => match items.remove(&(arg("-s"), arg("-a"))) {
                    Some(_) => out(0, b"keychain: \"/Users/x/Library/Keychains/login.keychain-db\"\n".to_vec(), ""),
                    None => out(44, vec![], "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain."),
                },
                "-i" => {
                    let line = String::from_utf8(stdin.unwrap().to_vec()).unwrap();
                    let words: Vec<&str> = line.trim_end_matches('\n').split(' ').collect();
                    assert!(!line.trim_end_matches('\n').contains('\n'), "one command per line");
                    assert_eq!(words[0], "add-generic-password");
                    let word = |flag: &str| words[words.iter().position(|w| *w == flag).unwrap() + 1].to_string();
                    let hex = word("-X");
                    let data: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
                    if !self.add_silently_fails {
                        items.insert((word("-s"), word("-a")), self.add_writes.clone().unwrap_or(data));
                    }
                    // 交互模式总是以 0 退出
                    out(0, b"security> security> ".to_vec(), "")
                }
                other => panic!("unexpected security subcommand {other}"),
            }
        }
    }

    #[test]
    fn security_tool_set_deletes_adds_via_stdin_and_verifies() {
        use security_tool::{delete_with, get_with, set_with};
        let tool = FakeTool::default();
        tool.items.lock().unwrap().insert(("sub-bot".into(), "openai/default".into()), b"sk-legacy-value".to_vec());
        assert_eq!(get_with(&tool, "sub-bot", "openai/default").unwrap().expose(), "sk-legacy-value");
        set_with(&tool, "sub-bot", "openai/default", &Secret::new("sk-new-value")).unwrap();
        assert_eq!(get_with(&tool, "sub-bot", "openai/default").unwrap().expose(), "sk-new-value");
        let calls = tool.calls.lock().unwrap().clone();
        let argv: Vec<String> = calls.iter().map(|(a, _)| a.join(" ")).collect();
        assert_eq!(
            argv,
            [
                "find-generic-password -s sub-bot -a openai/default -w",
                "delete-generic-password -s sub-bot -a openai/default",
                "-i",
                "find-generic-password -s sub-bot -a openai/default -w",
                "find-generic-password -s sub-bot -a openai/default -w",
            ]
        );
        // secret 只出现在 -i 的 stdin 里（hex 形式），从不在 argv 中
        assert!(argv.iter().all(|a| !a.contains("sk-") && !a.contains("736B")));
        let stdin = String::from_utf8(calls[2].1.clone().unwrap()).unwrap();
        assert_eq!(stdin, format!("add-generic-password -U -s sub-bot -a openai/default -l sub-bot -X {}\n", security_tool::hex_encode(b"sk-new-value")));
        assert!(calls.iter().enumerate().all(|(i, (_, input))| (i == 2) == input.is_some()));
        // 不存在的条目：set 跳过 delete 的 NotFound；delete 报 NotFound
        set_with(&tool, "sub-bot", "anthropic/ci", &Secret::new("sk-ci")).unwrap();
        delete_with(&tool, "sub-bot", "anthropic/ci").unwrap();
        assert!(delete_with(&tool, "sub-bot", "anthropic/ci").unwrap_err().is_not_found());
        assert!(get_with(&tool, "sub-bot", "anthropic/ci").unwrap_err().is_not_found());
    }

    #[test]
    fn security_tool_errors_never_contain_the_secret() {
        use security_tool::{get_with, set_with};
        // 名称校验（写入前，不 spawn）
        let tool = FakeTool::default();
        for (service, account) in [("sub-bot", "open ai/x"), ("my keys", "openai/default"), ("", "openai/default"), ("sub-bot", "a\"b")] {
            let e = set_with(&tool, service, account, &Secret::new("sk-unsafe-secret")).unwrap_err();
            assert!(matches!(&e, KeychainError::Tool { message, .. } if message.contains("[A-Za-z0-9._/@:-]")), "{e}");
            assert!(!e.to_string().contains("sk-unsafe"));
        }
        assert!(tool.calls.lock().unwrap().is_empty());
        // 读取不做名称限制（配置可指向任意条目）
        tool.items.lock().unwrap().insert(("my keys".into(), "claude prod".into()), b"sk-spaced".to_vec());
        assert_eq!(get_with(&tool, "my keys", "claude prod").unwrap().expose(), "sk-spaced");
        // ACL 拒绝 / 锁定 / 其它退出码
        for (exit, check) in [(51, KeychainError::is_not_found as fn(&KeychainError) -> bool), (36, KeychainError::is_not_found), (1, KeychainError::is_not_found)] {
            let tool = FakeTool { find_exit: Some(exit), ..Default::default() };
            let e = get_with(&tool, "sub-bot", "openai/default").unwrap_err();
            assert!(!check(&e));
            assert_eq!(e, security_tool::error_from_exit(Some(exit), "sub-bot", "openai/default"));
            assert!(!e.to_string().contains("simulated"), "{e}");
        }
        // -i 静默失败（退出码 0 但没写进去）⇒ 读回校验失败
        let tool = FakeTool { add_silently_fails: true, ..Default::default() };
        let e = set_with(&tool, "sub-bot", "openai/default", &Secret::new("sk-silent-secret")).unwrap_err();
        assert!(matches!(&e, KeychainError::Tool { message, .. } if message.contains("cannot be found")), "{e}");
        assert!(!e.to_string().contains("sk-silent") && !e.to_string().contains(&security_tool::hex_encode(b"sk-silent-secret")), "{e}");
        // 读回不一致
        let tool = FakeTool { add_writes: Some(b"sk-garbled".to_vec()), ..Default::default() };
        let e = set_with(&tool, "sub-bot", "openai/default", &Secret::new("sk-fresh")).unwrap_err();
        assert!(matches!(&e, KeychainError::Tool { message, .. } if message.contains("does not read back")), "{e}");
        assert!(!e.to_string().contains("sk-"), "{e}");
        // 读到非法内容 / hex dump 形式
        let tool = FakeTool::default();
        tool.items.lock().unwrap().insert(("sub-bot".into(), "openai/default".into()), b"0xDEAD  \"\\336\\255\"".to_vec());
        tool.items.lock().unwrap().insert(("sub-bot".into(), "openai/two".into()), b"sk-a sk-b".to_vec());
        for account in ["openai/default", "openai/two"] {
            assert_eq!(get_with(&tool, "sub-bot", account).unwrap_err(), KeychainError::Invalid { service: "sub-bot".into(), account: account.into() });
        }
        // spawn 失败
        let tool = FakeTool { spawn_error: true, ..Default::default() };
        let e = get_with(&tool, "sub-bot", "openai/default").unwrap_err();
        assert_eq!(e.to_string(), "keychain tool failed (service=sub-bot, account=openai/default): cannot run /usr/bin/security: No such file or directory (os error 2)");
        assert!(matches!(set_with(&tool, "sub-bot", "openai/default", &Secret::new("sk-x")).unwrap_err(), KeychainError::Tool { .. }));
    }

    /// 真实的 `ProcessRunner` 在没有 `/usr/bin/security` 的机器上报 `Tool`（不是 panic）。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn process_runner_reports_missing_tool() {
        if std::path::Path::new(security_tool::SECURITY_BIN).exists() {
            return;
        }
        let e = security_tool::get_with(&security_tool::ProcessRunner, "sub-bot", "openai/default").unwrap_err();
        assert!(matches!(&e, KeychainError::Tool { message, .. } if message.starts_with("cannot run /usr/bin/security")), "{e}");
    }

    #[test]
    fn keychain_value_is_trimmed_and_validated() {
        assert_eq!(secret_from_bytes(b" sk-x \n".to_vec(), "s", "a").unwrap().expose(), "sk-x");
        for bad in [&b"sk-secret-a sk-secret-b"[..], b"", b" \n", &[0xff, 0xfe]] {
            let e = secret_from_bytes(bad.to_vec(), "s", "a").unwrap_err();
            assert_eq!(e, KeychainError::Invalid { service: "s".into(), account: "a".into() });
            assert!(!e.to_string().contains("sk-secret"));
        }
    }

    #[test]
    fn mock_reader_covers_success_and_errors() {
        let kc = MockKeychain::with([
            (("sub-bot", "openai/default"), Ok("sk-mock-value\n")),
            (("sub-bot", "openai/locked"), Err(KeychainError::from_status(ERR_INTERACTION_NOT_ALLOWED, "sub-bot", "openai/locked"))),
            (("sub-bot", "openai/mac"), Err(KeychainError::Unsupported)),
        ]);
        assert_eq!(kc.get("sub-bot", "openai/default").unwrap().expose(), "sk-mock-value");
        assert!(kc.get("sub-bot", "openai/missing").unwrap_err().is_not_found());
        assert!(matches!(kc.get("sub-bot", "openai/locked"), Err(KeychainError::InteractionNotAllowed { .. })));
        assert_eq!(kc.get("sub-bot", "openai/mac").unwrap_err(), KeychainError::Unsupported);
    }

    fn counting_timed(kc: MockKeychain, timeout: Duration, grace: Duration) -> (TimedKeychain, Arc<std::sync::Mutex<Vec<String>>>) {
        let hints = Arc::new(std::sync::Mutex::new(vec![]));
        let seen = hints.clone();
        let timed = TimedKeychain::new(Arc::new(kc), timeout).grace(grace).on_wait(move |s, a| seen.lock().unwrap().push(wait_hint(s, a)));
        (timed, hints)
    }

    #[test]
    fn timed_reader_fast_path_has_no_hint() {
        let kc = MockKeychain::with([(("sub-bot", "openai/default"), Ok("sk-fast"))]);
        let (timed, hints) = counting_timed(kc, Duration::from_secs(1), Duration::from_millis(100));
        assert_eq!(timed.get("sub-bot", "openai/default").unwrap().expose(), "sk-fast");
        assert!(timed.get("sub-bot", "openai/missing").unwrap_err().is_not_found());
        assert!(hints.lock().unwrap().is_empty());
    }

    #[test]
    fn timed_reader_slow_read_hints_then_succeeds() {
        let kc = MockKeychain::with([(("sub-bot", "openai/default"), Ok("sk-slow"))]).delayed(Duration::from_millis(300));
        let (timed, hints) = counting_timed(kc, Duration::from_secs(2), Duration::from_millis(50));
        assert_eq!(timed.get("sub-bot", "openai/default").unwrap().expose(), "sk-slow");
        assert_eq!(hints.lock().unwrap().len(), 1);
    }

    #[test]
    fn timed_reader_times_out_with_hint() {
        // 读取线程阻塞 30s：调用方在 1s 后返回 Timeout，不等待（也不 join）该线程
        let kc = MockKeychain::with([(("sub-bot", "openai/default"), Ok("sk-never"))]).delayed(Duration::from_secs(30));
        let (timed, hints) = counting_timed(kc, Duration::from_secs(1), Duration::from_millis(100));
        let started = std::time::Instant::now();
        let e = timed.get("sub-bot", "openai/default").unwrap_err();
        let waited = started.elapsed();
        assert!(waited >= Duration::from_secs(1) && waited < Duration::from_secs(5), "{waited:?}");
        assert_eq!(e, KeychainError::Timeout { service: "sub-bot".into(), account: "openai/default".into(), secs: 1 });
        assert_eq!(
            *hints.lock().unwrap(),
            ["waiting for macOS Keychain authorization for sub-bot/openai/default — check for a dialog on the Mac's screen and choose \"Always Allow\"; then run `sub-bot keys reauth` once so later upgrades need no re-authorization"]
        );
        let text = e.to_string();
        for needle in ["within 1s", "service=sub-bot", "account=openai/default", "brew upgrade", "Always Allow", "sub-bot keys reauth", "sub-bot keys set", "apiKeyFile"] {
            assert!(text.contains(needle), "{needle}: {text}");
        }
        assert!(!text.contains("sk-never"));
    }

    #[test]
    fn with_timeout_skips_hint_when_grace_is_not_shorter_than_timeout() {
        let mut hinted = false;
        let e = with_timeout(
            "s",
            "a",
            Duration::from_millis(100),
            Duration::from_secs(1),
            || hinted = true,
            || {
                std::thread::sleep(Duration::from_secs(5));
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(e, KeychainError::Timeout { secs: 0, .. }));
        assert!(!hinted);
    }

    #[test]
    #[should_panic(expected = "reader boom")]
    fn with_timeout_propagates_reader_panic() {
        let _ = with_timeout::<()>("s", "a", Duration::from_secs(5), Duration::from_secs(1), || {}, || panic!("reader boom"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn system_keychain_is_unsupported_off_macos() {
        assert!(!supported());
        assert_eq!(SystemKeychain.get("sub-bot", "openai/default").unwrap_err(), KeychainError::Unsupported);
        assert_eq!(set_generic("sub-bot", "openai/default", &Secret::new("x")).unwrap_err(), KeychainError::Unsupported);
        assert_eq!(delete_generic("sub-bot", "openai/default").unwrap_err(), KeychainError::Unsupported);
        assert!(list_accounts("sub-bot").unwrap().is_empty());
    }

    /// 真实 Keychain 往返；CI 不跑。本地：`cargo test -p agent-ai -- --ignored keychain`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn real_keychain_roundtrip() {
        let (service, account) = ("sub-bot-test", format!("openai/t-{}", std::process::id()));
        set_generic(service, &account, &Secret::new("sk-roundtrip-test")).unwrap();
        set_generic(service, &account, &Secret::new("sk-roundtrip-test-2")).unwrap();
        assert_eq!(SystemKeychain.get(service, &account).unwrap().expose(), "sk-roundtrip-test-2");
        assert!(list_accounts(service).unwrap().contains(&account));
        delete_generic(service, &account).unwrap();
        assert!(SystemKeychain.get(service, &account).unwrap_err().is_not_found());
        assert!(delete_generic(service, &account).unwrap_err().is_not_found());
    }
}
