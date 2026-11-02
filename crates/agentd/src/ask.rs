//! Small HTTP client; no provider, Hub, or server-side secrets are initialized by `ask`.
use clap::Args;
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Args)]
pub struct Ask {
    #[arg(long, conflicts_with = "url")]
    pub socket: Option<PathBuf>,
    #[arg(long)]
    pub url: Option<String>,
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub session: Option<String>,
    /// Submit an asynchronous task and poll until its final result.
    #[arg(long = "async")]
    pub asynchronous: bool,
    /// Maximum seconds to wait for the request and final result (including async polling).
    #[arg(long, default_value_t = 3600)]
    pub timeout: u64,
    #[arg(long)]
    pub json: bool,
    /// Message, or - to read stdin.
    pub message: String,
}

pub struct Client {
    socket: Option<PathBuf>,
    url: String,
    token: Option<axum::http::HeaderValue>,
    http: reqwest::Client,
}

impl Client {
    pub fn new(socket: Option<PathBuf>, url: Option<String>, token: Option<&str>) -> anyhow::Result<Self> {
        let token = token
            .map(|token| {
                let mut header = axum::http::HeaderValue::from_str(&format!("Bearer {}", token.trim())).map_err(|_| anyhow::anyhow!("invalid token"))?;
                header.set_sensitive(true);
                anyhow::Ok(header)
            })
            .transpose()?;
        let url = url.unwrap_or_else(|| "http://127.0.0.1:7878".into());
        let parsed = reqwest::Url::parse(&url).map_err(|_| anyhow::anyhow!("invalid server URL"))?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https")
                && parsed.host_str().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "invalid server URL"
        );
        // No redirects or proxies: do not leak local credentials to another endpoint.
        let http = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build()?;
        Ok(Self { socket, url, token, http })
    }

    pub async fn request(&self, method: axum::http::Method, path: &str, body: Option<&Value>) -> anyhow::Result<(axum::http::StatusCode, Value)> {
        #[cfg(unix)]
        if let Some(socket) = &self.socket {
            use http_body_util::{BodyExt, Full};
            let stream = tokio::net::UnixStream::connect(socket).await.map_err(|_| anyhow::anyhow!("cannot connect to Unix socket"))?;
            let (mut sender, connection) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
            let mut connections = tokio::task::JoinSet::new();
            connections.spawn(async move {
                let _ = connection.await;
            });
            let mut request = hyper::Request::builder().method(method).uri(path).header("host", "localhost").header("content-type", "application/json");
            if let Some(token) = &self.token {
                request = request.header("authorization", token);
            }
            let body = axum::body::Bytes::from(body.map(Value::to_string).unwrap_or_default());
            let response = sender.send_request(request.body(Full::new(body))?).await?;
            let status = response.status();
            let bytes = response.into_body().collect().await?.to_bytes();
            return Ok((status, serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid server JSON"))?));
        }
        let mut request = self.http.request(method, format!("{}{path}", self.url.trim_end_matches('/')));
        if let Some(token) = &self.token {
            request = request.header("authorization", token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|_| anyhow::anyhow!("HTTP request failed"))?;
        let status = response.status();
        Ok((status, response.json().await.map_err(|_| anyhow::anyhow!("invalid server JSON"))?))
    }

    pub async fn ask(&self, message: &str, profile: Option<&str>, session: Option<&str>, asynchronous: bool, timeout_secs: u64) -> anyhow::Result<Value> {
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), self.ask_inner(message, profile, session, asynchronous))
            .await
            .map_err(|_| anyhow::anyhow!("ask timed out after {timeout_secs} seconds; server-side work may still be running"))?
    }

    async fn ask_inner(&self, message: &str, profile: Option<&str>, session: Option<&str>, asynchronous: bool) -> anyhow::Result<Value> {
        let body = json!({"message": message, "session": {"profile": profile}, "sessionId": session, "async": asynchronous});
        let (status, mut response) = self.request(axum::http::Method::POST, "/v1/chat", Some(&body)).await?;
        anyhow::ensure!(status.is_success(), "server returned HTTP {status}");
        if asynchronous {
            let id = response["taskId"].as_str().ok_or_else(|| anyhow::anyhow!("missing taskId"))?;
            let id = uuid::Uuid::parse_str(id).map_err(|_| anyhow::anyhow!("invalid taskId"))?;
            let path = format!("/v1/tasks/{id}");
            loop {
                let (status, task) = self.request(axum::http::Method::GET, &path, None).await?;
                anyhow::ensure!(status.is_success(), "server returned HTTP {status}");
                if task["status"] != "running" {
                    response = task;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
        Ok(response)
    }
}

pub async fn run(args: &Ask, data_dir: PathBuf) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut message = args.message.clone();
    if message == "-" {
        message.clear();
        tokio::io::stdin().read_to_string(&mut message).await?;
    }
    let token = match &args.token_file {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|_| anyhow::anyhow!("cannot read token file"))?),
        None => std::env::var("SUB_BOT_TOKEN").ok(),
    };
    let default_socket = data_dir.join("sub-bot.sock");
    if args.socket.is_none() && args.url.is_none() {
        crate::config::prepare_temp_data_dir(&default_socket, false)?;
    }
    let socket = args.socket.clone().or_else(|| (args.url.is_none() && default_socket.exists()).then_some(default_socket));
    let client = Client::new(socket, args.url.clone(), token.as_deref())?;
    let response = client.ask(&message, args.profile.as_deref(), args.session.as_deref(), args.asynchronous, args.timeout).await?;
    let result = if args.asynchronous { &response["result"] } else { &response };
    if args.json {
        println!("{response}");
    } else if let Some(text) = result["text"].as_str() {
        println!("{text}");
    }
    anyhow::ensure!(
        (!args.asynchronous || response["status"] == "succeeded")
            && result["error"].is_null()
            && result["aborted"] != true
            && !matches!(result["stopReason"].as_str(), Some("error" | "aborted"))
            && result["text"].is_string(),
        "request did not complete successfully"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        routing::{get, post},
        Json, Router,
    };
    use clap::Parser;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn timeout_cli_default_and_override() {
        for (args, expected) in [(vec!["sub-bot", "ask", "hello"], 3600), (vec!["sub-bot", "ask", "--async", "--timeout", "12", "hello"], 12)] {
            let crate::Cmd::Ask(ask) = crate::Cli::try_parse_from(args).unwrap().cmd else { panic!("expected ask") };
            assert_eq!(ask.timeout, expected);
        }
        assert!(crate::Cli::try_parse_from(["sub-bot", "ask", "--timeout", "-1", "hello"]).is_err());
    }

    #[tokio::test]
    async fn timeout_bounds_polling_and_stalled_requests() {
        // Cover repeated running responses, a hung poll, and a hung initial POST.
        for (hang_post, hang_poll) in [(false, false), (false, true), (true, false)] {
            let polls = Arc::new(AtomicUsize::new(0));
            let count = polls.clone();
            let app = Router::new()
                .route(
                    "/v1/chat",
                    post(move || async move {
                        if hang_post {
                            std::future::pending::<()>().await;
                        }
                        Json(json!({"taskId": uuid::Uuid::new_v4(), "status": "running"}))
                    }),
                )
                .route(
                    "/v1/tasks/{id}",
                    get(move || {
                        let count = count.clone();
                        async move {
                            count.fetch_add(1, Ordering::SeqCst);
                            if hang_poll {
                                std::future::pending::<()>().await;
                            }
                            Json(json!({"status": "running"}))
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let mut servers = tokio::task::JoinSet::new();
            servers.spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let client = Client::new(None, Some(format!("http://{address}")), None).unwrap();
            let error = tokio::time::timeout(std::time::Duration::from_secs(5), client.ask("hello", None, None, true, 1)).await.expect("ask did not enforce its deadline").unwrap_err();
            assert_eq!(error.to_string(), "ask timed out after 1 seconds; server-side work may still be running");
            if !hang_post {
                assert!(polls.load(Ordering::SeqCst) > 0);
            }
        }
    }
}
