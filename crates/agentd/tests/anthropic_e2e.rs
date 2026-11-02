//! Real CLI -> hub -> Anthropic SSE -> bash -> tool_result -> streamed reply.
#![cfg(unix)]

use axum::{body::Body, extract::State, http::HeaderMap, response::Response, routing::post, Json, Router};
use futures::StreamExt;
use serde_json::{json, Value};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{Mutex, Notify},
};

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Default)]
struct Mock {
    requests: Arc<Mutex<Vec<Value>>>,
    finish: Arc<Notify>,
}

fn sse(events: Vec<Value>) -> String {
    events.into_iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect()
}

async fn messages(State(mock): State<Mock>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    // Never include credential values in assertion diagnostics.
    assert!(headers.get("x-api-key").is_some_and(|v| v == "mock-anthropic-key"));
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["stream"], true);
    let mut requests = mock.requests.lock().await;
    let tool_turn = requests.len() % 2 == 0;
    requests.push(body);
    drop(requests);
    let start = json!({"type":"message_start", "message":{"id":"msg-test", "usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":30}}});
    let events = if tool_turn {
        vec![
            start,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool-bash","name":"bash","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"echo hi\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ]
    } else {
        vec![
            start,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Final reply: hi"}}),
        ]
    };
    let first = futures::stream::once(async move { Ok::<_, std::io::Error>(sse(events)) });
    let tail = futures::stream::once(async move {
        if tool_turn {
            return Ok(String::new());
        }
        // The test releases completion only after observing the CLI's text delta.
        mock.finish.notified().await;
        Ok(sse(vec![
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":12,"output_tokens":7,"cache_creation_input_tokens":32}}),
            json!({"type":"message_stop"}),
        ]))
    });
    Response::builder().header("content-type", "text/event-stream").body(Body::from_stream(first.chain(tail))).unwrap()
}

fn command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_sub-bot"));
    // Isolate child configuration/credentials without inspecting the host environment.
    cmd.env_clear().env("PATH", "/usr/bin:/bin").env("NO_PROXY", "*").kill_on_drop(true);
    cmd
}

#[tokio::test]
async fn anthropic_cli_tool_round_trip_streams_before_completion() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = TempDir(std::env::temp_dir().join(format!("sb-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])));
        std::fs::create_dir(&dir.0).unwrap();
        let key = dir.0.join("anthropic.key");
        std::fs::write(&key, "mock-anthropic-key\n").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mock = Mock::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1/", listener.local_addr().unwrap());
        let app = Router::new().route("/v1/messages", post(messages)).with_state(mock.clone());
        let mut servers = tokio::task::JoinSet::new();
        servers.spawn(async move { axum::serve(listener, app).await.unwrap() });

        // Exercise env-over-config precedence, then the configured base URL alone.
        for env_base in [true, false] {
            mock.requests.lock().await.clear();
            let config = dir.0.join("config.json");
            let socket = dir.0.join("s");
            std::fs::write(
                &config,
                json!({
                    "defaultProvider":"faux", "defaultReasoningEffort":"minimal",
                    "dataDir":dir.0.join(if env_base { "env-data" } else { "config-data" }),
                    "providers":{"anthropic":{"baseUrl":if env_base { "http://127.0.0.1:1/wrong" } else { &base }}},
                    "profiles":{"claude":{"model":"claude-test","workspaceRoots":[dir.0],"tools":["bash"],"loadContextFiles":false}},
                    "clients":[{"name":"test","tokenSha256":"0".repeat(64),"profiles":["claude"]}],
                    "localClients":[{"any":true,"client":"test"}]
                })
                .to_string(),
            )
            .unwrap();
            let mut cmd = command();
            cmd.arg("--config").arg(&config).arg("--anthropic-key-file").arg(&key).args(["--log-dir", "-", "serve", "--bind", "none", "--unix-socket"]).arg(&socket);
            if env_base {
                cmd.env("ANTHROPIC_BASE_URL", &base);
            }
            let mut daemon = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            while !socket.exists() {
                assert!(daemon.try_wait().unwrap().is_none(), "daemon exited before listening");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let mut ask = command().args(["ask", "--json", "--profile", "claude", "--socket"]).arg(&socket).arg("Run echo hi").stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
            let mut lines = BufReader::new(ask.stdout.take().unwrap()).lines();
            let mut streamed = false;
            let mut settled = false;
            while let Some(line) = lines.next_line().await.unwrap() {
                let event: Value = serde_json::from_str(&line).unwrap();
                if event["assistantMessageEvent"]["type"] == "text_delta" {
                    assert_eq!(event["assistantMessageEvent"]["delta"], "Final reply: hi");
                    streamed = true;
                    mock.finish.notify_one();
                }
                if event["type"] == "agent_settled" {
                    settled = true;
                }
            }
            assert!(ask.wait().await.unwrap().success());
            assert!(streamed && settled);
            {
                let requests = mock.requests.lock().await;
                assert_eq!(requests.len(), 2);
                assert!(requests[0]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "bash"));
                let messages = requests[1]["messages"].as_array().unwrap();
                let last = messages.last().unwrap();
                assert_eq!(last["role"], "user");
                let result = &last["content"][0];
                assert_eq!(result["type"], "tool_result");
                assert_eq!(result["tool_use_id"], "tool-bash");
                assert_eq!(result["is_error"], false);
                assert!(result["content"][0]["text"].as_str().unwrap().contains("hi"));
            }
            daemon.kill().await.unwrap();
            // SIGKILL cannot clean up the socket; remove it before the next iteration.
            std::fs::remove_file(&socket).unwrap();

            // Standalone run uses the same real provider; check aggregate cache writes and actual thinking metadata.
            mock.finish.notify_one();
            let output = command()
                .arg("--config")
                .arg(&config)
                .arg("--anthropic-key-file")
                .arg(&key)
                .env("ANTHROPIC_BASE_URL", &base)
                .args(["--log-dir", "-", "run", "--profile", "claude", "--json", "Run echo hi"])
                .output()
                .await
                .unwrap();
            assert!(output.status.success());
            let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(reply["text"], "Final reply: hi");
            assert_eq!(reply["totalUsage"]["cacheWrite"], 62);
            assert_eq!(reply["usage"]["cacheWrite"], 32);
            assert_eq!(reply["reasoningEffort"], Value::Null);
            assert_eq!(reply["requestedReasoningEffort"], "minimal");
        }
    })
    .await
    .expect("Anthropic e2e timed out");
}
