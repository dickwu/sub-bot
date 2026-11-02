//! Local transport identity comes only from the accepted socket, never HTTP headers.
use crate::config::{Client, Config};
use axum::{extract::connect_info::Connected, serve::IncomingStream};
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Debug)]
pub enum Peer {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix {
        uid: u32,
        gid: u32,
        pid: Option<i32>,
    },
    #[cfg(unix)]
    Unknown,
}

impl Connected<IncomingStream<'_, tokio::net::TcpListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        Self::Tcp(*stream.remote_addr())
    }
}

#[cfg(unix)]
impl Connected<IncomingStream<'_, tokio::net::UnixListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, tokio::net::UnixListener>) -> Self {
        match stream.io().peer_cred() {
            Ok(cred) => Self::Unix { uid: cred.uid(), gid: cred.gid(), pid: cred.pid() },
            Err(_) => Self::Unknown, // Fail closed: Bearer authentication remains available.
        }
    }
}

pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback()),
    }
}

impl Peer {
    pub fn client<'a>(&self, cfg: &'a Config) -> Option<&'a Client> {
        let name = match self {
            Self::Tcp(addr) if is_loopback(addr.ip()) => &cfg.trust_loopback.as_ref()?.client,
            #[cfg(unix)]
            Self::Unix { uid, gid, pid } => {
                // Keep all kernel credentials available for future audit policies.
                let _ = (gid, pid);
                let username = if cfg.local_clients.iter().any(|m| m.username.is_some()) { username(*uid) } else { None };
                &cfg.local_clients.iter().find(|m| m.any || m.uid == Some(*uid) || m.username.as_ref().is_some_and(|name| Some(name) == username.as_ref()))?.client
            }
            _ => return None,
        };
        cfg.clients.iter().find(|c| &c.name == name)
    }
}

#[cfg(unix)]
fn username(uid: u32) -> Option<String> {
    // getpwuid_r is reentrant on both Linux and macOS. Bound NSS buffer growth.
    let mut size = 1024;
    while size <= 1024 * 1024 {
        let mut buffer = vec![0u8; size];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: entry and buffer are writable and live through the call and name copy.
        let rc = unsafe { libc::getpwuid_r(uid, entry.as_mut_ptr(), buffer.as_mut_ptr().cast(), buffer.len(), &mut result) };
        if rc == libc::ERANGE {
            size *= 2;
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        // SAFETY: successful getpwuid_r initialized result; pw_name is a NUL-terminated string.
        return unsafe { std::ffi::CStr::from_ptr((*result).pw_name) }.to_str().ok().map(str::to_owned);
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn username_mapping_uses_kernel_uid() {
        // SAFETY: geteuid has no memory preconditions.
        let uid = unsafe { libc::geteuid() };
        let name = username(uid);
        let has_passwd_entry = name.is_some();
        let mut cfg = Config::fallback("faux".into(), "token", ".".into());
        cfg.local_clients = vec![crate::config::LocalClient { uid: None, username: Some(name.unwrap_or_else(|| "sub-bot-nonexistent-user".into())), any: false, client: "default".into() }];
        cfg.validate().unwrap();
        assert_eq!(Peer::Unix { uid, gid: 0, pid: None }.client(&cfg).is_some(), has_passwd_entry);
        cfg.local_clients[0].username = Some("sub-bot-nonexistent-user".into());
        assert!(Peer::Unix { uid, gid: 0, pid: None }.client(&cfg).is_none());
    }
}

#[cfg(unix)]
pub mod socket {
    use std::{
        io,
        os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
    };

    /// Removes only the socket we created, not a replacement file or symlink.
    pub struct SocketGuard {
        path: PathBuf,
        dev: u64,
        ino: u64,
    }
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
                if meta.file_type().is_socket() && meta.dev() == self.dev && meta.ino() == self.ino {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }
    }

    pub fn bind(path: &Path, mode: u32) -> anyhow::Result<(tokio::net::UnixListener, SocketGuard)> {
        crate::config::prepare_temp_data_dir(path, true)?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                anyhow::ensure!(meta.file_type().is_socket(), "unix socket path exists and is not a socket");
                match std::os::unix::net::UnixStream::connect(path) {
                    Ok(_) => anyhow::bail!("unix socket is already in use"),
                    Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => std::fs::remove_file(path)?,
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        // Bind inside a private directory, then publish only after chmod: no permissive window.
        let staging = parent.join(format!(".s{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
        let temporary = staging.join("s");
        let result = (|| {
            let listener = tokio::net::UnixListener::bind(&temporary)?;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode))?;
            // hard_link is atomic and refuses to replace a concurrently created path.
            let meta = std::fs::symlink_metadata(&temporary)?;
            std::fs::hard_link(&temporary, path)?;
            Ok((listener, SocketGuard { path: path.to_owned(), dev: meta.dev(), ino: meta.ino() }))
        })();
        let _ = std::fs::remove_file(&temporary);
        let _ = std::fs::remove_dir(&staging);
        result
    }
}
