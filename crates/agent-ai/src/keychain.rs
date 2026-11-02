//! macOS Keychain（generic password）key 来源。
//!
//! - 读取通过可注入的 [`KeychainReader`]（生产用 [`SystemKeychain`]，测试用 mock）；
//! - 写入/删除/枚举（[`set_generic`] / [`delete_generic`] / [`list_accounts`]）只给 `sub-bot keys` CLI 用；
//! - 平台相关代码全部集中在私有 `sys` 模块：macOS 用 `security-framework`（不 shell 调 `security`），
//!   其它平台是返回 [`KeychainError::Unsupported`] 的 stub。
//!
//! 约定：错误信息只含 service/account 名，绝不包含条目内容。
use crate::secrets::{validate_key_text, wipe_bytes, Secret};

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
    use std::{collections::BTreeMap, sync::Mutex};

    /// `(service, account)` → 条目内容或预设错误。
    #[derive(Default)]
    pub struct MockKeychain {
        pub items: Mutex<BTreeMap<(String, String), Result<String, KeychainError>>>,
    }

    impl MockKeychain {
        pub fn with(entries: impl IntoIterator<Item = ((&'static str, &'static str), Result<&'static str, KeychainError>)>) -> Self {
            let items = entries.into_iter().map(|((s, a), v)| ((s.to_string(), a.to_string()), v.map(str::to_string))).collect();
            Self { items: Mutex::new(items) }
        }
    }

    impl KeychainReader for MockKeychain {
        fn get(&self, service: &str, account: &str) -> Result<Secret, KeychainError> {
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
