//! Real probe CLI with local HTTP providers; no credentials or external network required.
#![cfg(unix)]
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Clone, Default)]
struct Mock(Arc<Mutex<Vec<Value>>>);

async fn handler(State(mock): State<Mock>, headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, String) {
    assert!(body.get("tools").is_none());
    let anthropic = headers.contains_key("x-api-key");
    if anthropic {
        assert!(headers["x-api-key"] == "probe-test-key");
        assert_eq!(body["max_tokens"], 2048);
    } else {
        assert!(headers["authorization"] == "Bearer probe-test-key");
    }
    mock.0.lock().unwrap().push(body.clone());
    let level = if anthropic {
        &body["output_config"]["effort"]
    } else if body.get("input").is_some() {
        &body["reasoning"]["effort"]
    } else {
        &body["reasoning_effort"]
    };
    if anthropic && body["thinking"]["type"] == "adaptive" && level == "max" {
        return (StatusCode::BAD_REQUEST, json!({"error":{"message":"thinking: adaptive is not supported"}}).to_string());
    }
    if level == "ultra" {
        return (
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":format!("probe-test-key reasoning_effort rejected. Supported values are: 'low', 'max', 'unknown'. {}", "长".repeat(400))}}).to_string(),
        );
    }
    let event = if anthropic {
        json!({"type":"message_stop"})
    } else if body.get("input").is_some() {
        json!({"type":"response.completed","response":{"status":"completed"}})
    } else {
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
    };
    (StatusCode::OK, format!("data: {event}\n\n"))
}

fn command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_sub-bot"));
    cmd.env_clear().env("PATH", "/usr/bin:/bin").env("NO_PROXY", "*").kill_on_drop(true);
    cmd
}

#[tokio::test]
async fn probe_cli_raw_levels_keys_variants_and_output() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let dir = TempDir(std::env::temp_dir().join(format!("probe-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir(&dir.0).unwrap();
        let key = dir.0.join("key");
        std::fs::write(&key, "probe-test-key\n").unwrap();
        let config = dir.0.join("config.json");
        let mock = Mock::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new().route("/v1/messages", post(handler)).route("/v1/chat/completions", post(handler)).route("/v1/responses", post(handler)).with_state(mock.clone());
        let mut servers = tokio::task::JoinSet::new();
        servers.spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        std::fs::write(
            &config,
            json!({
                "defaultProvider":"openai", "defaultModel":"gpt-5", "llmMaxRetries":3,
                "providers":{"openai":{"baseUrl":base},"anthropic":{"baseUrl":base,"apiKeyFile":"key"}},
                "profiles":{"default":{"workspaceRoots":[dir.0]}}
            })
            .to_string(),
        )
        .unwrap();
        for api in ["chat", "responses"] {
            mock.0.lock().unwrap().clear();
            let output = command()
                .arg("--config")
                .arg(&config)
                .arg("--openai-key-file")
                .arg(&key)
                .args(["--log-dir", "-", "probe-efforts", "--model", "gpt-5", "--levels", "none,max,ultra", "--api", api, "--json"])
                .output()
                .await
                .unwrap();
            assert!(output.status.success());
            assert!(!String::from_utf8_lossy(&output.stdout).contains("probe-test-key"));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("probe-test-key"));
            let rows: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(rows.as_array().unwrap().len(), 3);
            assert_eq!(rows[0]["accepted"], true);
            assert_eq!(rows[1]["level"], "max");
            assert_eq!(rows[1]["status"], 200);
            assert_eq!(rows[2]["accepted"], false);
            assert_eq!(rows[2]["status"], 400);
            assert_eq!(rows[2]["supportedValues"], json!(["low", "max", "unknown"]));
            assert_eq!(rows[2]["error"].as_str().unwrap().chars().count(), 300);
            assert_eq!(rows[2]["variant"], api);
            let requests = mock.0.lock().unwrap();
            assert_eq!(requests.len(), 3); // No catalogue clamp or runtime downgrade, even with retries configured.
            for (index, level) in ["none", "max", "ultra"].iter().enumerate() {
                let body = &requests[index];
                if api == "chat" {
                    assert_eq!(body["reasoning_effort"], *level);
                    assert_eq!(body["max_completion_tokens"], if index == 0 { 256 } else { 1024 });
                } else {
                    assert_eq!(body["reasoning"]["effort"], *level);
                    assert_eq!(body["max_output_tokens"], if index == 0 { 256 } else { 1024 });
                }
            }
        }
        // Only Anthropic config key is loaded; the default OpenAI provider has no key here.
        for (thinking, count, variant) in [(None, 3, "omit"), (Some("adaptive"), 2, "adaptive"), (Some("omit"), 2, "omit"), (Some("enabled"), 2, "enabled")] {
            mock.0.lock().unwrap().clear();
            let mut cmd = command();
            cmd.arg("--config").arg(&config).args(["--log-dir", "-", "probe-efforts", "--model", "claude-sonnet-5-5", "--levels", "max,ultra", "--json"]);
            if let Some(thinking) = thinking {
                cmd.args(["--thinking", thinking]);
            }
            let output = cmd.output().await.unwrap();
            assert!(output.status.success());
            let rows: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(rows[0]["variant"], variant);
            assert_eq!(rows[0]["accepted"], thinking != Some("adaptive"));
            assert_eq!(rows[0]["provider"], "anthropic");
            assert_eq!(rows[1]["level"], "ultra");
            assert_eq!(rows[1]["accepted"], false);
            assert_eq!(mock.0.lock().unwrap().len(), count);
        }
        let output = command()
            .arg("--config")
            .arg(&config)
            .arg("--anthropic-key-file")
            .arg(&key)
            .args(["--log-dir", "-", "probe-efforts", "--model", "claude-sonnet-5-5", "--levels", "max,ultra", "--thinking", "omit"])
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("max: accepted status=200 variant=omit"));
        assert!(text.contains("ultra: rejected status=400 variant=omit"));
    })
    .await
    .expect("probe CLI timeout");
}
