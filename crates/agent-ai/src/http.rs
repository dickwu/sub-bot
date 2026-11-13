//! 共享的出站 HTTP 客户端构建器。
//!
//! 所有 reqwest 客户端（LLM provider、工具、webhook、MCP streamable HTTP、测试）都从 [`client_builder`] 开始：
//! TLS 固定为 rustls + ring + webpki-roots（发布为静态 musl 二进制，不依赖 aws-lc / OpenSSL / 系统证书校验器）。
//! reqwest 以 `rustls-no-provider` 构建，没有进程级默认 crypto provider，因此不能直接用
//! `reqwest::Client::new()` / `reqwest::Client::builder()`（构建时会 panic）。
use std::sync::{Arc, OnceLock};

/// 预配置的 rustls 客户端配置（只构建一次；`ClientConfig` 克隆很便宜，内部是 `Arc`）。
fn tls_config() -> rustls::ClientConfig {
    static CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                // ring provider 支持 TLS 1.2 / 1.3 的默认套件，这里不会失败
                .expect("ring crypto provider supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            // reqwest 未启用 http2：只协商 HTTP/1.1
            tls.alpn_protocols = vec![b"http/1.1".to_vec()];
            tls
        })
        .clone()
}

/// `reqwest::ClientBuilder`，已配置 rustls（ring + webpki-roots）。调用方按需追加超时、重定向策略等后 `build()`。
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().tls_backend_preconfigured(tls_config())
}

/// 没有额外选项的客户端（等价于以前的 `reqwest::Client::new()`）。
pub fn client() -> reqwest::Client {
    client_builder().build().expect("HTTP client setup failed")
}

/// LLM provider 客户端：在 [`client_builder`] 基础上加 30s 连接超时与 TCP keepalive（30s 后开始探测，每 10s 一次，
/// 最多 5 次），让长时间无输出的流（推理模型思考中）不被 NAT / 防火墙静默断开，也能更快发现死连接。
/// 只协商 HTTP/1.1，因此没有 HTTP/2 PING。
pub fn llm_client_builder() -> reqwest::ClientBuilder {
    client_builder().connect_timeout(std::time::Duration::from_secs(30)).tcp_keepalive(crate::LLM_TCP_KEEPALIVE).tcp_keepalive_interval(std::time::Duration::from_secs(10)).tcp_keepalive_retries(5u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_without_a_process_default_crypto_provider() {
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
        client_builder().connect_timeout(std::time::Duration::from_secs(1)).build().unwrap();
        llm_client_builder().build().unwrap();
        let _ = client();
        let config = tls_config();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[tokio::test]
    async fn plain_http_requests_work() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 4096];
            let _ = socket.read(&mut buf).await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").await.unwrap();
        });
        let body = client().get(format!("http://{address}/")).send().await.unwrap().text().await.unwrap();
        assert_eq!(body, "ok");
        server.await.unwrap();
    }
}
