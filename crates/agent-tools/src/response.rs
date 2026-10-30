/// 有界读取响应体；额外探测一个 chunk，区分恰好达到上限和真正截断。
pub(crate) async fn read_body(mut response: reqwest::Response, cap: usize) -> reqwest::Result<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let keep = chunk.len().min(cap - body.len());
        body.extend_from_slice(&chunk[..keep]);
        if keep < chunk.len() {
            return Ok((body, true));
        }
    }
    Ok((body, false))
}

#[cfg(test)]
pub(crate) mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 写完响应头后通知测试，便于验证 body 阶段的超时/取消。
    pub(crate) async fn server(body: Option<Vec<u8>>) -> (String, tokio::sync::oneshot::Receiver<()>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
            }
            let size = body.as_ref().map_or(100, Vec::len);
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            let _ = tx.send(());
            if let Some(body) = body {
                // 有界客户端可以提前关闭连接。
                let _ = stream.write_all(&body).await;
            } else {
                std::future::pending::<()>().await;
            }
        });
        (url, rx, task)
    }

    #[tokio::test]
    async fn body_cap_handles_exact_size_and_overflow() {
        for size in [1024, 1025] {
            let (url, _, server) = server(Some(vec![b'x'; size])).await;
            let response = reqwest::get(url).await.unwrap();
            let (body, truncated) = super::read_body(response, 1024).await.unwrap();
            assert_eq!(body.len(), 1024);
            assert_eq!(truncated, size > 1024);
            server.await.unwrap();
        }
    }
}
