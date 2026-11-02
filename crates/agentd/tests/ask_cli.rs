//! Exercise the CLI dispatch without initializing providers, secrets, logs, or a store.
#![cfg(unix)]

use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::{os::unix::fs::PermissionsExt, path::PathBuf};

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("sb-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn explicit_and_discovered_sockets_reject_unsafe_fallback() {
    let dir = TempDir::new();
    // Isolate the predictable fallback tree without changing this process's environment.
    // SAFETY: geteuid has no arguments or memory preconditions.
    let root = dir.0.join(format!("sub-bot-{}", unsafe { libc::geteuid() }));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
    let socket = root.join("sub-bot.sock");
    for explicit in [false, true] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sub-bot"));
        command.env_remove("SUB_BOT_CONFIG").env_remove("SUB_BOT_TOKEN").env("TMPDIR", &dir.0).arg("ask");
        if explicit {
            command.arg("--socket").arg(&socket);
        } else {
            command.arg("--data-dir").arg(&root);
        }
        let output = command.arg("hello").output().await.unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("group/world writable"));
        assert!(!socket.exists());
        assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode() & 0o777, 0o777);
    }
}

#[tokio::test]
async fn streaming_cli_preserves_output_and_client_only_initialization() {
    let dir = TempDir::new();
    let data = dir.0.join("unused-data");
    let logs = dir.0.join("unused-logs");
    let config = dir.0.join("config.json");
    // These server-only settings must never be loaded by ask.
    std::fs::write(&config, json!({"dataDir": data, "apiKeyFile": "missing-key", "alerts": {"webhookSecretFile": "missing-secret"}}).to_string()).unwrap();
    let events = vec![
        json!({"type":"message_update", "assistantMessageEvent":{"type":"text_delta", "delta":"你好"}}),
        json!({"type":"message_update", "assistantMessageEvent":{"type":"text_delta", "delta":" world"}}),
        json!({"type":"agent_settled", "aborted":false}),
    ];
    let body = events.iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
    let app = Router::new().route(
        "/v1/chat",
        post(move |Json(request): Json<Value>| {
            let body = body.clone();
            async move {
                assert_eq!(request["stream"], true);
                ([("content-type", "text/event-stream")], body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut servers = tokio::task::JoinSet::new();
    servers.spawn(async move { axum::serve(listener, app).await.unwrap() });
    for json_output in [false, true] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sub-bot"));
        command.env_remove("SUB_BOT_TOKEN").arg("--config").arg(&config).arg("--log-dir").arg(&logs).args(["--provider", "invalid-provider", "ask", "--url", &format!("http://{address}")]);
        if json_output {
            command.arg("--json");
        }
        let output = command.args(["--timeout", "5", "hello"]).output().await.unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        if json_output {
            let actual: Vec<Value> = stdout.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
            assert_eq!(actual, events);
        } else {
            assert_eq!(stdout, "你好 world\n");
        }
        assert!(!data.exists());
        assert!(!logs.exists());
    }
}
