#![cfg(unix)]
use crate::{
    ask,
    config::{Client, Config, LocalClient, TrustLoopback},
    hub::Hub,
    local::{socket, Peer},
};
use axum::http::{Method, StatusCode};
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, path::PathBuf};

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        // Short path also fits macOS's 104-byte sockaddr_un.sun_path.
        let path = std::env::temp_dir().join(format!("sb-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn config() -> Config {
    let mut cfg = Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
    cfg.data_dir = Some(std::env::temp_dir().join(format!("sub-bot-local-{}", uuid::Uuid::new_v4())));
    cfg.profiles.get_mut("default").unwrap().load_context_files = false;
    cfg.clients.push(Client { name: "other".into(), token_sha256: crate::config::sha256_hex("other-token"), profiles: vec!["default".into()], ..Default::default() });
    cfg
}
fn mapping(uid: u32) -> LocalClient {
    LocalClient { uid: Some(uid), username: None, any: false, client: "default".into() }
}

#[tokio::test]
async fn unix_auth_and_ask_round_trip() {
    for mapped in [false, true] {
        let dir = TempDir::new();
        let path = dir.0.join("s");
        let mut cfg = config();
        if !mapped {
            cfg.local_clients.clear();
        } // Otherwise exercise the no-config effective-uid mapping unchanged.
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        let (listener, guard) = socket::bind(&path, 0o600).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let stop = tokio_util::sync::CancellationToken::new();
        let shutdown = stop.clone();
        let app = crate::http::app(hub.clone());
        let mut servers = tokio::task::JoinSet::new();
        servers.spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<Peer>()).with_graceful_shutdown(shutdown.cancelled_owned()).await.unwrap();
        });
        let anonymous = ask::Client::new(Some(path.clone()), None, None).unwrap();
        let (status, body) = anonymous.request(Method::GET, "/v1/profiles", None).await.unwrap();
        assert_eq!(status, if mapped { StatusCode::OK } else { StatusCode::UNAUTHORIZED });
        if mapped {
            assert_eq!(body["client"], "default");
        }
        // Valid Bearer overrides the uid identity. Invalid Bearer must never fall back.
        for (token, expected) in [("other-token", StatusCode::OK), ("wrong", StatusCode::UNAUTHORIZED)] {
            let client = ask::Client::new(Some(path.clone()), None, Some(token)).unwrap();
            let (status, body) = client.request(Method::GET, "/v1/profiles", None).await.unwrap();
            assert_eq!(status, expected);
            if status.is_success() {
                assert_eq!(body["client"], "other");
            }
        }
        if mapped {
            let mut text = String::new();
            anonymous
                .ask_stream("hello local", Some("default"), None, 10, |_, event| {
                    if let Some(delta) = event["assistantMessageEvent"]["delta"].as_str() {
                        text.push_str(delta);
                    }
                    Ok(())
                })
                .await
                .unwrap();
            assert!(text.contains("hello local"));
            let response = anonymous.ask_async("hello async", None, None, 10).await.unwrap();
            assert_eq!(response["status"], "succeeded");
            assert!(response["result"]["text"].as_str().unwrap().contains("hello async"));
        }
        stop.cancel();
        hub.abort_all().await;
        servers.join_next().await.unwrap().unwrap();
        drop(guard);
        assert!(!path.exists());
    }
}

#[tokio::test]
async fn tcp_loopback_trust_is_opt_in_and_header_cannot_bypass_auth() {
    for trusted in [false, true] {
        let mut cfg = config();
        if trusted {
            cfg.trust_loopback = Some(TrustLoopback { client: "default".into() });
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        let app = crate::http::app(hub.clone());
        let mut servers = tokio::task::JoinSet::new();
        servers.spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<Peer>()).await.unwrap();
        });
        for (token, expected) in [(None, if trusted { StatusCode::OK } else { StatusCode::UNAUTHORIZED }), (Some("other-token"), StatusCode::OK), (Some("wrong"), StatusCode::UNAUTHORIZED)] {
            let client = ask::Client::new(None, Some(format!("http://{address}")), token).unwrap();
            let (status, body) = client.request(Method::GET, "/v1/profiles", None).await.unwrap();
            assert_eq!(status, expected);
            if status.is_success() {
                assert_eq!(body["client"], if token.is_some() { "other" } else { "default" });
            }
        }
        let response = reqwest::Client::new().get(format!("http://{address}/v1/profiles")).header("authorization", "Basic invalid").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        hub.abort_all().await;
    }
}

#[tokio::test]
async fn peer_policy_ignores_forwarded_headers_and_handles_ipv6() {
    use tower::ServiceExt;
    let mut cfg = config();
    cfg.trust_loopback = Some(TrustLoopback { client: "default".into() });
    for ip in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
        assert!(Peer::Tcp(std::net::SocketAddr::new(ip.parse().unwrap(), 42)).client(&cfg).is_some());
    }
    for ip in ["192.0.2.1", "::ffff:192.0.2.1", "2001:db8::1"] {
        let peer = Peer::Tcp(std::net::SocketAddr::new(ip.parse().unwrap(), 42));
        assert!(peer.client(&cfg).is_none());
        let hub = Hub::new(cfg.clone(), "faux", None, None).unwrap();
        let request = axum::http::Request::builder().uri("/v1/profiles").header("x-forwarded-for", "127.0.0.1").extension(axum::extract::ConnectInfo(peer)).body(axum::body::Body::empty()).unwrap();
        assert_eq!(crate::http::app(hub).oneshot(request).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn socket_lifecycle_preserves_non_sockets_and_active_listeners() {
    let dir = TempDir::new();
    let path = dir.0.join("s");
    std::fs::write(&path, "keep").unwrap();
    assert!(socket::bind(&path, 0o600).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep");
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(dir.0.join("target"), &path).unwrap();
    assert!(socket::bind(&path, 0o600).is_err());
    assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
    std::fs::remove_file(&path).unwrap();
    // Closed listener leaves a stale socket.
    drop(tokio::net::UnixListener::bind(&path).unwrap());
    let (listener, guard) = socket::bind(&path, 0o660).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o660);
    assert!(socket::bind(&path, 0o600).is_err());
    drop(listener);
    drop(guard);
    assert!(!path.exists());
    let (_listener, guard) = socket::bind(&path, 0o600).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "replacement").unwrap();
    drop(guard);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
}

#[test]
fn local_config_validation_and_mapping() {
    let mut cfg = config();
    cfg.data_dir = Some(PathBuf::from("custom-data"));
    assert_eq!(cfg.data_dir(), PathBuf::from("custom-data"));
    cfg.local_clients = vec![mapping(1234)];
    cfg.validate().unwrap();
    let peer = Peer::Unix { uid: 1234, gid: 1, pid: None };
    assert_eq!(peer.client(&cfg).unwrap().name, "default");
    assert!(Peer::Unknown.client(&cfg).is_none());
    cfg.local_clients[0].uid = Some(5678);
    assert!(peer.client(&cfg).is_none());
    cfg.local_clients[0].uid = None;
    cfg.local_clients[0].any = true;
    assert!(peer.client(&cfg).is_some());
    cfg.validate().unwrap();
    cfg.local_clients[0].client = "missing".into();
    assert!(cfg.validate().is_err());
    cfg.local_clients.clear();
    cfg.trust_loopback = Some(TrustLoopback { client: "missing".into() });
    assert!(cfg.validate().is_err());
    cfg.trust_loopback = None;
    for invalid in ["888", "", "-1", "1000", "0o600"] {
        cfg.unix_socket_mode = Some(invalid.into());
        assert!(cfg.validate().is_err());
    }
    for entry in [json!({"uid":1,"any":true,"client":"default"}), json!({"client":"default"})] {
        cfg.unix_socket_mode = None;
        cfg.local_clients = vec![serde_json::from_value(entry).unwrap()];
        assert!(cfg.validate().is_err());
    }
    let cfg: Config = serde_json::from_value(json!({"profiles":{},"dataDir":"data","unixSocket":"socket","unixSocketMode":"0660","localClients":[],"trustLoopback":null})).unwrap();
    assert_eq!(cfg.socket_mode().unwrap(), 0o660);
    assert_eq!(cfg.data_dir(), PathBuf::from("data"));
}

#[test]
fn cli_local_options() {
    use clap::Parser;
    for args in [vec!["sub-bot", "serve", "--unix-socket", "--bind", "none"], vec!["sub-bot", "serve", "--unix-socket", "/tmp/s", "--data-dir", "/tmp/data"]] {
        assert!(crate::Cli::try_parse_from(args).is_ok());
    }
    assert!(crate::Cli::try_parse_from(["sub-bot", "ask", "--socket", "/tmp/s", "--profile", "default", "--session", "id", "--async", "--json", "-"]).is_ok());
    assert!(crate::Cli::try_parse_from(["sub-bot", "ask", "--socket", "/tmp/s", "--url", "http://localhost", "hello"]).is_err());
}
