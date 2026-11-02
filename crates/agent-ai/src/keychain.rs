//! macOS Keychain（generic password）key 来源。
//!
//! - 读取通过可注入的 [`KeychainReader`]（生产用 [`SystemKeychain`]，测试用 mock）；
//! - 写入/删除/枚举（[`set_generic`] / [`delete_generic`] / [`list_accounts`]）只给 `sub-bot keys` CLI 用；
//! - 平台相关代码全部集中在私有 `sys` 模块：macOS 用 `security-framework`（不 shell 调 `security`），
//!   其它平台是返回 [`KeychainError::Unsupported`] 的 stub。
//!
//! - 读取可能被 macOS 的授权对话框无限期阻塞（例如 `brew upgrade` 换了二进制后 ACL 不再信任它），
//!   因此调用方用 [`TimedKeychain`] / [`with_timeout`] 在独立线程上读取并限时等待。
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
    /// errSecAuthFailed (-25293) / errSecUserCanceled (-128)：用户在 ACL 提示里拒绝/取消，或二进制签名变化后未授权
    #[error("keychain access denied (service={service}, account={account}): approve sub-bot in the Keychain prompt or re-run `sub-bot keys set` after upgrading the binary")]
    Denied { service: String, account: String },
    #[error("keychain item is not valid UTF-8 or is empty (service={service}, account={account})")]
    Invalid { service: String, account: String },
    #[error("keychain is only supported on macOS")]
    Unsupported,
    #[error("keychain error {code} (service={service}, account={account})")]
    Other { code: i32, service: String, account: String },
    /// 读取在 `secs` 秒内没有返回（多半是授权对话框在等人点击，或会话没有 GUI 无法显示对话框）
    #[error(
        "no answer from macOS Keychain within {secs}s (service={service}, account={account}): an authorization dialog is probably waiting (the sub-bot binary changed, e.g. after `brew upgrade`) or this session has no GUI to show it; click \"Always Allow\" in the dialog or re-run `sub-bot keys set`, or use apiKeyFile for headless use (ssh / LaunchDaemon)"
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
    format!("waiting for macOS Keychain authorization for {service}/{account} — check for a dialog on the Mac's screen; after brew upgrade choose \"Always Allow\"")
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

/// 写入（存在则更新）。
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

#[cfg(target_os = "macos")]
mod sys {
    use super::{secret_from_bytes, KeychainError, ERR_ITEM_NOT_FOUND};
    use crate::secrets::Secret;
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
    use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};

    pub fn get(service: &str, account: &str) -> Result<Secret, KeychainError> {
        match get_generic_password(service, account) {
            Ok(bytes) => secret_from_bytes(bytes, service, account),
            Err(e) => Err(KeychainError::from_status(e.code(), service, account)),
        }
    }

    pub fn set(service: &str, account: &str, secret: &Secret) -> Result<(), KeychainError> {
        set_generic_password(service, account, secret.expose().as_bytes()).map_err(|e| KeychainError::from_status(e.code(), service, account))
    }

    pub fn delete(service: &str, account: &str) -> Result<(), KeychainError> {
        delete_generic_password(service, account).map_err(|e| KeychainError::from_status(e.code(), service, account))
    }

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
            ["waiting for macOS Keychain authorization for sub-bot/openai/default — check for a dialog on the Mac's screen; after brew upgrade choose \"Always Allow\""]
        );
        let text = e.to_string();
        for needle in ["within 1s", "service=sub-bot", "account=openai/default", "brew upgrade", "Always Allow", "sub-bot keys set", "apiKeyFile"] {
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
