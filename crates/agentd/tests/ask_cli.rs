//! Exercise the CLI dispatch without initializing providers, secrets, logs, or a store.
#![cfg(unix)]

#[path = "support/cli.rs"]
mod cli;

use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use cli::{command, output, TempDir};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

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
        let mut command = command(&dir.0);
        command.args(["--log-dir", "-", "ask", "--timeout", "5"]);
        if explicit {
            command.arg("--socket").arg(&socket);
        } else {
            command.arg("--data-dir").arg(&root);
        }
        let output = output(command.arg("hello")).await;
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
                ([("content-type", "text/event-stream"), ("x-session-id", "cli-session")], body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let socket = dir.0.join("ask.sock");
    let unix_listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let unix_app = app.clone();
    let mut servers = tokio::task::JoinSet::new();
    servers.spawn(async move { axum::serve(unix_listener, unix_app).await.unwrap() });
    servers.spawn(async move { axum::serve(listener, app).await.unwrap() });
    for (unix, json_output) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut command = command(&dir.0);
        command.arg("--config").arg(&config).arg("--log-dir").arg(&logs).args(["--provider", "invalid-provider", "ask"]);
        if unix {
            command.arg("--socket").arg(&socket);
        } else {
            command.args(["--url", &format!("http://{address}")]);
        }
        if json_output {
            command.arg("--json");
        }
        let output = output(command.args(["--timeout", "5", "hello"])).await;
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        if json_output {
            let actual: Vec<Value> = stdout.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
            assert_eq!(actual[0], json!({"type":"session", "sessionId":"cli-session"}));
            assert_eq!(&actual[1..], events.as_slice());
            assert_eq!(actual.last(), events.last());
        } else {
            assert_eq!(stdout, "你好 world\n");
        }
        assert!(!data.exists());
    }
}

#[tokio::test]
async fn ask_model_cli_env_and_resume() {
    const SESSION: &str = "00000000-0000-4000-8000-000000000000";
    const MODEL_ERROR: &str = "model 'blocked' is not allowed in profile 'p' (allowedModels)";
    let dir = TempDir::new();
    let config_dir = dir.0.join("xdg/sub-bot");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("sub-bot.json"), r#"{"defaultModel":"local-default"}"#).unwrap();

    let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new()
        .route(
            "/v1/chat",
            post(move |Json(request): Json<Value>| {
                let send = send.clone();
                async move {
                    let blocked = request["session"]["model"] == "blocked";
                    let asynchronous = request["async"] == true;
                    send.send(request).unwrap();
                    if blocked {
                        (StatusCode::BAD_REQUEST, Json(json!({"error": MODEL_ERROR}))).into_response()
                    } else if asynchronous {
                        Json(json!({"taskId": uuid::Uuid::new_v4()})).into_response()
                    } else {
                        ([("content-type", "text/event-stream")], "data: {\"type\":\"agent_settled\",\"aborted\":false}\n\n").into_response()
                    }
                }
            }),
        )
        .route("/v1/tasks/{id}", get(|| async { Json(json!({"status":"succeeded", "result":{"text":"done"}})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut servers = tokio::task::JoinSet::new();
    servers.spawn(async move { axum::serve(listener, app).await.unwrap() });

    for (args, env_model, expected_model, session) in [
        (vec!["--model", "before", "ask"], Some("env-model"), Some("before"), None),
        (vec!["ask", "--model", "after"], Some("env-model"), Some("after"), None),
        (vec!["ask", "--async"], Some("env-model"), Some("env-model"), None),
        (vec!["ask"], None, None, None),
        (vec!["ask", "--resume", SESSION, "--effort", "high"], Some("env-model"), None, Some(SESSION)),
        (vec!["ask", "--model", "blocked"], None, Some("blocked"), None),
        (vec!["ask", "--async", "--model", "blocked"], None, Some("blocked"), None),
    ] {
        let mut cmd = command(&dir.0);
        if let Some(model) = env_model {
            cmd.env("SUB_BOT_MODEL", model);
        }
        let out = output(cmd.args(["--log-dir", "-"]).args(&args).args(["--url", &url, "--timeout", "5", "--profile", "p", "hello"])).await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        let blocked = expected_model == Some("blocked");
        assert_eq!(out.status.success(), !blocked, "{args:?}: {stderr}");
        if blocked {
            assert!(stderr.contains(&format!("server returned HTTP 400 Bad Request: {MODEL_ERROR}")), "{stderr}");
        }
        assert_eq!(stderr.contains("--model applies only to new sessions; continuing with the session's model"), session.is_some(), "{stderr}");
        let body = received.try_recv().unwrap_or_else(|e| panic!("{args:?}: {e}: {stderr}"));
        assert_eq!(body["sessionId"].as_str(), session);
        assert!(body.get("model").is_none());
        assert_eq!(body["session"].get("model"), expected_model.map(Value::from).as_ref());
        assert_eq!(body[if args.contains(&"--async") { "async" } else { "stream" }], true);
        if session.is_some() {
            assert_eq!(body["reasoningEffort"], "high");
        }
    }
}
