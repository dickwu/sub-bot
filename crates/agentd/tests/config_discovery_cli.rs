//! Regression coverage for CLI config selection and the config-consuming command boundaries.
#![cfg(unix)]

#[path = "support/cli.rs"]
mod cli;

use axum::{http::HeaderMap, routing::post, Json, Router};
use cli::{command, output, TempDir};
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Output, Stdio},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};

const CONFIG_NAME: &str = "sub-bot/sub-bot.json";
const SESSION: &str = "00000000-0000-4000-8000-000000000000";

fn write_config(path: &Path, value: Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value.to_string()).unwrap();
}

fn stdout_json(out: &Output) -> Value {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn failure(out: &Output, expected: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && stderr.contains(expected), "expected error {expected:?}, got {}: {stderr}", out.status);
    assert!(out.stdout.is_empty());
}

async fn with_stdin(cmd: &mut Command, input: &[u8]) -> Output {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        child.stdin.take().unwrap().write_all(input).await.unwrap();
        child.wait_with_output().await.unwrap()
    })
    .await
    .expect("sub-bot with stdin did not exit")
}

#[tokio::test]
async fn config_sources_and_opt_out() {
    let dir = TempDir::new();
    let xdg_config = dir.0.join("xdg").join(CONFIG_NAME);
    let home_config = dir.0.join(".config").join(CONFIG_NAME);
    let env_config = dir.0.join("env.json");
    let explicit_config = dir.0.join("explicit.json");
    for (profile, path) in [("xdg", &xdg_config), ("home", &home_config), ("env", &env_config), ("explicit", &explicit_config)] {
        write_config(path, json!({"profiles":{profile:{"workspaceRoots":[format!("./{profile}-root")]}}}));
    }
    let list = || {
        let mut cmd = command(&dir.0);
        cmd.args(["--log-dir", "-", "--workspace", "builtin-root", "profiles", "--json"]);
        cmd
    };
    let check = |out: Output, profile: &str, root: &str, source: &str, path: Option<&Path>, reason: Option<&str>| {
        assert_eq!(stdout_json(&out), json!({"profiles":[{"name":profile, "workspaceRoots":[root], "allowedWorkspaceRoots":["*"]}]}));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(stderr.matches("configuration selected").count(), 1, "{stderr}");
        assert!(stderr.contains("INFO") && stderr.contains(&format!("config_source={source:?}")), "{stderr}");
        if let Some(path) = path {
            assert!(stderr.contains(&format!("config_path={path:?}")), "{stderr}");
        }
        if let Some(reason) = reason {
            assert!(stderr.contains(&format!("reason={reason:?}")), "{stderr}");
        }
    };
    check(output(&mut list()).await, "xdg", "./xdg-root", "default-path", Some(&xdg_config), None);
    for xdg in [None, Some(""), Some("relative")] {
        let mut cmd = list();
        if let Some(xdg) = xdg {
            cmd.env("XDG_CONFIG_HOME", xdg);
        } else {
            cmd.env_remove("XDG_CONFIG_HOME");
        }
        check(output(&mut cmd).await, "home", "./home-root", "default-path", Some(&home_config), None);
    }
    check(output(list().env("SUB_BOT_CONFIG", &env_config)).await, "env", "./env-root", "env", Some(&env_config), None);
    check(output(list().env("SUB_BOT_CONFIG", &env_config).arg("--config").arg(&explicit_config)).await, "explicit", "./explicit-root", "explicit", Some(&explicit_config), None);
    // The source must come from argument parsing, not a comparison to the env path.
    check(output(list().env("SUB_BOT_CONFIG", &env_config).arg("--config").arg(&env_config)).await, "env", "./env-root", "explicit", Some(&env_config), None);
    for path in [env_config.as_os_str(), std::ffi::OsStr::new("")] {
        check(output(list().env("SUB_BOT_CONFIG", path).arg("--no-config")).await, "default", "builtin-root", "built-in", None, Some("no-config"));
    }

    let explicit = explicit_config.to_str().unwrap();
    for args in [vec!["--no-config", "profiles", "--config", explicit], vec!["--config", explicit, "profiles", "--no-config"]] {
        let out = output(command(&dir.0).env("SUB_BOT_CONFIG", &explicit_config).args(args)).await;
        failure(&out, "--no-config cannot be used with an explicit --config <PATH>");
        assert_eq!(out.status.code(), Some(2));
    }
    let missing = dir.0.join("missing.json");
    let out = output(list().env("SUB_BOT_CONFIG", &missing)).await;
    failure(&out, &format!("cannot read configuration {missing:?}: NotFound"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("built-in"));
    let out = output(list().env("SUB_BOT_CONFIG", &env_config).arg("--config").arg(&missing)).await;
    failure(&out, &format!("cannot read configuration {missing:?}: NotFound"));
    fs::remove_file(&xdg_config).unwrap();
    // XDG miss does not trigger another search in HOME, or in the working directory.
    fs::copy(&home_config, dir.0.join("sub-bot.json")).unwrap();
    check(output(&mut list()).await, "default", "builtin-root", "built-in", None, Some("not-found"));
}

#[tokio::test]
async fn broken_config_errors_without_fallback() {
    const MARKER: &str = "unlogged-config-value";
    let dir = TempDir::new();
    let path = dir.0.join("xdg").join(CONFIG_NAME);
    let data = dir.0.join("override");
    let missing_socket = dir.0.join("missing.sock");
    let socket_arg = missing_socket.to_str().unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("{{\"profiles\":{{}}, \"secret\":\"{MARKER}\", \"bad\":[}}")).unwrap();
    let cli = || {
        let mut cmd = command(&dir.0);
        cmd.args(["--provider", "faux", "--log-dir", "-", "--data-dir"]).arg(&data);
        cmd
    };
    for args in [
        &["serve", "--bind", "none", "--unix-socket"][..],
        &["run", "hi"],
        &["rpc"],
        &["models"],
        &["probe-efforts", "--model", "faux"],
        &["mcp", "list"],
        &["issues", "test"],
        &["issues", "tail"],
        &["ask", "--socket", socket_arg, "hi"],
        &["turns", "--socket", socket_arg, SESSION, "--set", "4"],
        &["profiles", "--json"],
        &["usage", "--json"],
        &["pricing", "--json"],
        &["keys", "list"],
        &["keys", "check"],
        &["keys", "reauth", "--dry-run"],
    ] {
        let out = output(cli().args(args)).await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        failure(&out, "invalid configuration");
        assert!(stderr.contains(&format!("{path:?} at line 1 column")), "{args:?}: {stderr}");
        assert!(!stderr.contains(MARKER) && !stderr.contains("built-in"), "{args:?}: {stderr}");
    }
    assert!(!data.exists());

    for (value, args, expected) in [
        (json!({"profiles":{}, "keychainTimeoutSecs":0}), &["run", "hi"][..], "keychainTimeoutSecs must be >= 1"),
        (json!({"profiles":{}, "alerts":{"webhookSecretFile":MARKER}}), &["serve", "--bind", "none"], "cannot read webhookSecretFile"),
        (json!({"profiles":{}, "defaultProvider":"nope"}), &["keys", "check"], "unknown provider nope"),
        (json!({"profiles":{"p":{"workspaceRoots":["."],"maxTurns":0}}}), &["run", "hi"], "profile 'p': maxTurns must be >= 1"),
        (json!({"dataDir": {"secret":MARKER}}), &["ask", "--socket", socket_arg, "hi"], "at line 1 column"),
        (json!({"profiles":{(MARKER):{"workspaceRoots":[]}}}), &["profiles"], "workspaceRoots must not be empty"),
        (json!({"pricing":{"source":MARKER}}), &["pricing"], "at line 1 column"),
        (json!({"pricing":{"source":"builtin", "refreshHours":0}}), &["usage"], "pricing.refreshHours must be in 1..=8760"),
        (json!({"keychainTimeoutSecs":0}), &["keys", "list"], "keychainTimeoutSecs must be >= 1"),
        (json!({"providers":{"openai":{"accounts":MARKER}}}), &["keys", "reauth", "--dry-run"], "at line 1 column"),
    ] {
        write_config(&path, value);
        let out = output(cli().args(args)).await;
        failure(&out, expected);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(&format!("{path:?}")) && !stderr.contains(MARKER), "{args:?}: {stderr}");
    }
    assert!(!data.exists());

    fs::write(&path, "{").unwrap();
    let out = output(cli().args(["--no-config", "profiles", "--json"])).await;
    assert_eq!(stdout_json(&out)["profiles"][0]["name"], "default");
    for args in [["hash-token", "test-fixture"].as_slice(), &["--help"], &["--version"]] {
        let out = output(cli().args(args)).await;
        assert!(out.status.success() && out.stderr.is_empty());
        assert!(!out.stdout.is_empty());
    }
    let key_file = dir.0.join("bootstrap.key");
    let out = with_stdin(command(&dir.0).args(["keys", "set", "--provider", "openai", "--file"]).arg(&key_file), MARKER.as_bytes()).await;
    assert!(out.status.success() && key_file.is_file());
    assert!(!String::from_utf8_lossy(&out.stdout).contains(MARKER) && !String::from_utf8_lossy(&out.stderr).contains("configuration selected"));
    let out = output(command(&dir.0).args(["keys", "delete", "--provider", "openai", "--file"]).arg(&key_file)).await;
    assert!(out.status.success() && !key_file.exists());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("configuration selected"));
}

#[tokio::test]
async fn startup_alerts_use_the_discovered_config_directory() {
    let dir = TempDir::new();
    let path = dir.0.join("xdg").join(CONFIG_NAME);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/alert", listener.local_addr().unwrap());
    let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/alert",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let send = send.clone();
            async move {
                send.send((headers.contains_key("x-subbot-signature"), body)).unwrap();
                axum::http::StatusCode::NO_CONTENT
            }
        }),
    );
    let mut servers = tokio::task::JoinSet::new();
    servers.spawn(async move { axum::serve(listener, app).await.unwrap() });
    write_config(
        &path,
        json!({
            "profiles":{"p":{"workspaceRoots":[]}},
            "alerts":{"webhookUrl":url, "webhookSecretFile":"relative-alert-key"}
        }),
    );
    fs::write(path.with_file_name("relative-alert-key"), "fixture-alert-secret").unwrap();
    let out = output(command(&dir.0).args(["--log-dir", "-", "serve", "--bind", "none"])).await;
    failure(&out, "workspaceRoots must not be empty");
    let (signed, alert) = received.try_recv().expect("startup failure must reach the configured alert target");
    assert!(signed, "relative alert secret must resolve beside the selected config");
    assert_eq!(alert["data"]["kind"], "startup_failed");
    let message = alert["data"]["message"].as_str().unwrap();
    assert!(message.contains(&format!("{path:?}")) && message.contains("workspaceRoots must not be empty"));
    assert!(!message.contains(url.as_str()) && !message.contains("fixture-alert-secret"));
}

#[tokio::test]
async fn default_config_partial_readers_stay_lightweight() {
    let dir = TempDir::new();
    let data = dir.0.join("data");
    let override_dir = dir.0.join("override");
    let logs = dir.0.join("must-not-create-logs");
    let path = dir.0.join("xdg").join(CONFIG_NAME);
    write_config(
        &path,
        json!({
            "profiles":{"p":{"workspaceRoots":[dir.0]}},
            "dataDir":data, "apiKeyFile":"missing-provider-key",
            "clients":[{"name":"bot", "tokenSha256":"0".repeat(64), "profiles":["p"], "webhookSecretFile":"missing-client-secret"}],
            "alerts":{"webhookSecretFile":"missing-alert-secret"},
            "pricing":{"source":"builtin", "models":{"faux/faux":{"input":9,"output":21}}}
        }),
    );

    let profile = output(command(&dir.0).arg("--log-dir").arg(&logs).args(["profiles", "--json"])).await;
    assert_eq!(stdout_json(&profile)["profiles"][0]["name"], "p");
    let usage = output(command(&dir.0).arg("--log-dir").arg(&logs).args(["usage", "--json"])).await;
    assert_eq!(stdout_json(&usage)["total"]["requests"], 0);
    let prices = output(command(&dir.0).arg("--log-dir").arg(&logs).args(["pricing", "faux/faux", "--json"])).await;
    let prices = stdout_json(&prices);
    assert_eq!(prices["models"][0]["priceSource"], "override");
    assert_eq!(prices["models"][0]["base"]["input"], 9.0);
    assert!(!data.exists() && !override_dir.exists());

    let mut servers = tokio::task::JoinSet::new();
    for (socket_dir, label) in [(&data, "configured"), (&override_dir, "override")] {
        fs::create_dir(socket_dir).unwrap();
        let listener = tokio::net::UnixListener::bind(socket_dir.join("sub-bot.sock")).unwrap();
        let app = Router::new()
            .route(
                "/v1/chat",
                post(move |headers: HeaderMap| async move {
                    assert!(!headers.contains_key("authorization"));
                    (
                        [("content-type", "text/event-stream")],
                        format!("data: {}\n\ndata: {{\"type\":\"agent_settled\"}}\n\n", json!({"type":"message_update", "assistantMessageEvent":{"type":"text_delta", "delta":label}})),
                    )
                }),
            )
            .route(
                "/v1/sessions/{id}/turns",
                post(move |headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert!(!headers.contains_key("authorization"));
                    assert_eq!(body, json!({"maxTurns":7}));
                    Json(json!({"endpoint":label}))
                }),
            );
        servers.spawn(async move { axum::serve(listener, app).await.unwrap() });
    }
    for label in ["configured", "override"] {
        let cli = || {
            let mut cmd = command(&dir.0);
            cmd.arg("--log-dir").arg(&logs);
            if label == "override" {
                cmd.arg("--data-dir").arg(&override_dir);
            }
            cmd
        };
        let ask = output(cli().args(["ask", "--timeout", "5", "hi"])).await;
        assert!(ask.status.success(), "{}", String::from_utf8_lossy(&ask.stderr));
        assert_eq!(String::from_utf8(ask.stdout).unwrap(), format!("{label}\n"));
        let turns = output(cli().args(["turns", SESSION, "--set", "7"])).await;
        assert_eq!(stdout_json(&turns)["endpoint"], label);
    }
    assert!(!logs.exists());
    for socket_dir in [data, override_dir] {
        assert_eq!(fs::read_dir(socket_dir).unwrap().count(), 1, "only the test socket should exist");
    }
}

#[tokio::test]
async fn discovered_run_and_rpc_coexist_with_serve() {
    let dir = TempDir::new();
    let data = dir.0.join("data");
    let sessions = dir.0.join("history");
    let path = dir.0.join("xdg").join(CONFIG_NAME);
    let workspace = dir.0.join("project");
    fs::create_dir(&workspace).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let token = "discovery-test-bearer";
    let hash = output(command(&dir.0).args(["hash-token", token])).await;
    assert!(hash.status.success());
    let hash = std::str::from_utf8(&hash.stdout).unwrap().trim();
    let token_file = dir.0.join("token");
    fs::write(&token_file, token).unwrap();
    let profile = json!({"model":"faux", "deepModel":"faux-deep", "workspaceRoots":[workspace], "allowedModels":["faux", "faux-*"], "loadContextFiles":false});
    write_config(
        &path,
        json!({
            "defaultProvider":"faux", "dataDir":data, "sessionDir":sessions,
            "pricing":{"source":"builtin"},
            "profiles":{"coder-openai":profile, "http":profile},
            "clients":[{"name":"remote", "tokenSha256":hash, "profiles":["http"]}]
        }),
    );
    let cli = || {
        let mut cmd = command(&dir.0);
        cmd.args(["--log-dir", "-"]);
        cmd
    };
    let daemon_log = dir.0.join("serve.stderr");
    let mut daemon = cli().args(["serve", "--bind", "none", "--unix-socket", "--token", "fallback-only"]).stdout(Stdio::null()).stderr(fs::File::create(&daemon_log).unwrap()).spawn().unwrap();
    let socket = data.join("sub-bot.sock");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !socket.exists() {
            if let Some(status) = daemon.try_wait().unwrap() {
                panic!("serve exited before listening ({status}): {}", fs::read_to_string(&daemon_log).unwrap());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("serve did not bind a socket");
    let lock = data.join("state/lock");
    assert!(lock.is_file());
    let second = output(cli().args(["serve", "--bind", "none", "--unix-socket"])).await;
    failure(&second, "is in use by another sub-bot server");

    let run = output(cli().args(["run", "--profile", "coder-openai", "--model", "faux-alt", "--json", "hi"])).await;
    let run = stdout_json(&run);
    assert_eq!(run["text"], "echo: hi");
    assert_eq!(run["model"], "faux-alt");
    assert_eq!(run["cwd"], json!(workspace));
    assert_eq!(run["persisted"], true);
    failure(&output(cli().args(["run", "--profile", "coder-openai", "--model", "blocked", "--json", "hi"])).await, "model 'blocked' is not allowed in profile 'coder-openai'");

    let rpc = with_stdin(cli().args(["rpc", "--profile", "coder-openai"]), b"{\"id\":1,\"type\":\"get_state\"}\n").await;
    let rpc = stdout_json(&rpc);
    assert_eq!(rpc["command"], "get_state");
    assert_eq!(rpc["success"], true, "{rpc}");
    assert_eq!(rpc["data"]["profile"], "coder-openai");
    assert_eq!(rpc["data"]["model"], "faux");
    assert_eq!(rpc["data"]["cwd"], json!(workspace));
    for (owner, state) in [("local", &run), ("stdio", &rpc["data"])] {
        let files = agent_session::list_files(&sessions.join(owner));
        assert_eq!(files.len(), 1);
        assert_eq!(state["sessionId"], files[0].id);
        let saved = agent_session::Session::read(&files[0].path).unwrap();
        assert_eq!(saved.profile(), "coder-openai");
        assert_eq!(saved.meta().client.as_deref(), Some(owner));
        assert_eq!(saved.meta().model.as_deref(), state["model"].as_str());
    }
    let models = output(cli().args(["models", "--profile", "coder-openai", "--json"])).await;
    let models = stdout_json(&models);
    assert_eq!(models.as_array().unwrap().len(), 1);
    assert_eq!(models[0]["id"], "faux");
    assert_eq!(models[0]["allowed"], true);

    let ask = || {
        let mut cmd = cli();
        cmd.args(["ask", "--json", "--timeout", "5"]);
        cmd
    };
    for unauthorized in [output(ask().arg("hi")).await, output(ask().env("SUB_BOT_TOKEN", "fallback-only").arg("hi")).await] {
        failure(&unauthorized, "HTTP 401 Unauthorized");
    }
    failure(&output(ask().env("SUB_BOT_TOKEN", token).args(["--profile", "coder-openai", "hi"])).await, "profile 'coder-openai' is not allowed for client 'remote'");
    let authenticated = || {
        let mut cmd = ask();
        cmd.env("SUB_BOT_TOKEN", "wrong-token").arg("--token-file").arg(&token_file);
        cmd
    };
    let session_state = |out: &Output| {
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let last: Value = serde_json::from_str(std::str::from_utf8(&out.stdout).unwrap().lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "session_state");
        last
    };
    let created = session_state(&output(authenticated().args(["--profile", "http", "--depth", "deep", "--model", "faux-alt", "hi"])).await);
    assert_eq!(created["model"], "faux-alt");
    assert_eq!(created["routing"]["reason"], "explicit model");
    assert_eq!(created["cwd"], json!(workspace));
    let id = created["sessionId"].as_str().unwrap();
    for asynchronous in [false, true] {
        let mut cmd = authenticated();
        if asynchronous {
            cmd.arg("--async");
        }
        failure(&output(cmd.args(["--profile", "http", "--model", "blocked", "hi"])).await, "HTTP 400 Bad Request: model 'blocked' is not allowed in profile 'http' (allowedModels)");
    }
    let resumed = output(authenticated().args(["--resume", id, "--model", "blocked", "--effort", "high", "again"])).await;
    assert!(String::from_utf8_lossy(&resumed.stderr).contains("continuing with the session's model"));
    let resumed = session_state(&resumed);
    assert_eq!(resumed["sessionId"], id);
    assert_eq!(resumed["model"], "faux-alt");
    assert_eq!(resumed["requestedReasoningEffort"], "high");
    let files = agent_session::list_files(&sessions.join("remote"));
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].id, id);
    assert!(!data.join("sessions").exists());

    // SAFETY: daemon is this test's still-running child.
    assert_eq!(unsafe { libc::kill(daemon.id().unwrap() as i32, libc::SIGTERM) }, 0);
    let exit = tokio::time::timeout(Duration::from_secs(5), daemon.wait()).await.unwrap().unwrap();
    let log = fs::read_to_string(&daemon_log).unwrap();
    assert!(exit.success() && log.contains("config_source=\"default-path\""), "{log}");
    assert!(!log.contains("single client"));
}
