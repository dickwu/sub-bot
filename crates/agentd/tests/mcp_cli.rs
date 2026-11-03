//! MCP 客户端端到端：`sub-bot` + 假 stdio MCP 服务器（`sub-bot-fake-mcp`，tests/support/fake_mcp.rs）+ faux provider，不访问网络。
//! 覆盖：`mcp list`（连接、分页、命名空间、长名映射、schema 清洗、allow-list）、run 调用、isError、崩溃后重启、
//! 调用超时与 notifications/cancelled、启动失败（非必需告警 / 必需致命）、profile 收窄、toolApproval 对 MCP 工具生效、环境变量。
#![cfg(unix)]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const FAKE: &str = env!("CARGO_BIN_EXE_sub-bot-fake-mcp");
const LONG_NAME: &str = "a_really_long_tool_name_that_goes_well_beyond_the_provider_limit_of_sixty_four_characters";

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("sb-mcp-{tag}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 配置：`fake`（第 2 页分页、allow-list 去掉 hidden / slow 之外的部分工具）、`bad`（启动即退出）。
fn write_config(dir: &Path, profile: Value, extra: Value) -> PathBuf {
    let mut cfg = json!({
        "dataDir": dir.join("data"),
        "persistSessions": false,
        "mcpServers": {
            "fake": {"command": FAKE, "args": ["--name", "fakesrv", "--page-size", "2"], "env": {"FOO": "foo-${FAKE_BAR}"}, "callTimeoutSecs": 2,
                     "tools": ["echo", "fail", "crash", "image", "slow", "untyped", "dotted.name", "a_really_*", "env"]},
            "bad": {"command": FAKE, "args": ["--fail-startup"]}
        },
        "profiles": {"default": {"workspaceRoots": [dir], "loadContextFiles": false, "tools": [], "mcpServers": ["fake"]}},
        "clients": []
    });
    cfg["profiles"]["default"].as_object_mut().unwrap().extend(profile.as_object().unwrap().clone());
    cfg.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    let path = dir.join("config.json");
    std::fs::write(&path, cfg.to_string()).unwrap();
    path
}

fn command(config: &Path, dir: &Path) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(env!("CARGO_BIN_EXE_sub-bot"));
    c.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("NO_COLOR", "1")
        .env("FAKE_BAR", "bar")
        .env("FAKE_MCP_LOG", dir.join("fake.log"))
        // 内置密钥变量不应传给 MCP 子进程
        .env("OPENAI_API_KEY", "sk-test-not-for-mcp-children")
        .arg("--config")
        .arg(config)
        .args(["--provider", "faux", "--log-dir", "-"])
        .kill_on_drop(true);
    c
}

async fn output(mut c: tokio::process::Command) -> (bool, String, String) {
    let out = tokio::time::timeout(Duration::from_secs(60), c.output()).await.expect("sub-bot hung").unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

async fn run(config: &Path, dir: &Path, prompt: &str) -> (bool, String, String) {
    let mut c = command(config, dir);
    c.args(["run", prompt]);
    output(c).await
}

#[tokio::test]
async fn mcp_list_shows_namespaced_paginated_sanitized_tools_and_failures() {
    let dir = TempDir::new("list");
    let config = write_config(&dir.0, json!({}), json!({}));
    let mut c = command(&config, &dir.0);
    c.args(["mcp", "list", "--json", "--profile", "default"]);
    let (ok, stdout, stderr) = output(c).await;
    assert!(ok, "{stderr}");
    let list: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(list.len(), 1, "only the profile's servers");
    let fake = &list[0];
    assert_eq!((fake["name"].as_str(), fake["transport"].as_str(), fake["status"].as_str()), (Some("fake"), Some("stdio"), Some("ready")));
    assert_eq!((fake["serverName"].as_str(), fake["serverVersion"].as_str()), (Some("fakesrv"), Some("1.2.3")));
    assert!(fake["connectMs"].is_u64());
    let tools = fake["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    // 11 个工具分 6 页；allow-list 去掉 hidden / change_tools
    assert_eq!(&names[..7], ["mcp__fake__echo", "mcp__fake__fail", "mcp__fake__crash", "mcp__fake__image", "mcp__fake__slow", "mcp__fake__untyped", "mcp__fake__dotted_name"]);
    assert_eq!(names[8], "mcp__fake__env");
    assert_eq!(names.len(), 9);
    let long = names[7];
    assert!(long.len() == 64 && long.starts_with("mcp__fake__a_really_long_tool_name") && long.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'), "{long}");
    assert_eq!(tools[7]["mcpName"], LONG_NAME);
    assert_eq!(tools[6]["mcpName"], "dotted.name");
    // 没有描述 ⇒ 生成的描述；schema 清洗
    assert_eq!(tools[6]["description"], "MCP tool 'dotted.name' from server 'fake'");
    assert_eq!(tools[5]["inputSchema"], json!({"type": "object", "properties": {"x": {"type": "number"}}, "required": ["x"]}));
    assert_eq!(tools[2]["inputSchema"], json!({"type": "object", "properties": {}}));
    assert!(stderr.contains("MCP server connected") && stderr.contains("server_name=\"fakesrv\"") && stderr.contains("tools=9"), "{stderr}");

    // 全部服务器：bad 失败 ⇒ 非零退出码，lastError 带退出状态与 stderr
    let mut c = command(&config, &dir.0);
    c.args(["mcp", "list", "--json"]);
    let (ok, stdout, stderr) = output(c).await;
    assert!(!ok);
    assert!(stderr.contains("MCP server(s) failed to connect: bad"), "{stderr}");
    let list: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let bad = list.iter().find(|s| s["name"] == "bad").unwrap();
    let error = bad["lastError"].as_str().unwrap();
    assert_eq!(bad["status"], "failed");
    assert!(error.contains("exit status: 2") && error.contains("fake-mcp: simulated startup failure"), "{error}");

    // 文本输出
    let mut c = command(&config, &dir.0);
    c.args(["mcp", "list", "--profile", "default"]);
    let (ok, stdout, _) = output(c).await;
    assert!(ok && stdout.contains("fake") && stdout.contains("fakesrv 1.2.3") && stdout.contains("mcp__fake__echo"), "{stdout}");
}

#[tokio::test]
async fn run_calls_mcp_tools_and_respects_profile_narrowing() {
    let dir = TempDir::new("run");
    let config = write_config(&dir.0, json!({}), json!({}));
    let (ok, stdout, stderr) = run(&config, &dir.0, r#"/tool mcp__fake__echo {"text":"hello from mcp"}"#).await;
    assert!(ok, "{stderr}");
    assert_eq!(stdout.trim(), "tool mcp__fake__echo returned: hello from mcp");
    assert!(stderr.contains("mcp__fake__echo") && stderr.contains("MCP server connected"), "{stderr}");
    // env：${VAR} 展开；provider key 不传给子进程
    let (_, stdout, _) = run(&config, &dir.0, r#"/tool mcp__fake__env {"name":"FOO"}"#).await;
    assert_eq!(stdout.trim(), "tool mcp__fake__env returned: foo-bar");
    let (_, stdout, _) = run(&config, &dir.0, r#"/tool mcp__fake__env {"name":"OPENAI_API_KEY"}"#).await;
    assert_eq!(stdout.trim(), "tool mcp__fake__env returned: <unset>");
    // 长名字按映射回原名调用
    let long = {
        let mut c = command(&config, &dir.0);
        c.args(["mcp", "list", "--json"]);
        let list: Vec<Value> = serde_json::from_str(&output(c).await.1).unwrap();
        list.iter().find(|s| s["name"] == "fake").unwrap()["tools"].as_array().unwrap().iter().find(|t| t["mcpName"] == LONG_NAME).unwrap()["name"].as_str().unwrap().to_string()
    };
    let (_, stdout, _) = run(&config, &dir.0, &format!("/tool {long} {{}}")).await;
    assert_eq!(stdout.trim(), format!("tool {long} returned: called {LONG_NAME}"));

    // profile 收窄到 echo：fail 不在会话工具里
    let config = write_config(&dir.0, json!({"mcpServers": [{"name": "fake", "tools": ["echo"]}]}), json!({}));
    let (ok, stdout, _) = run(&config, &dir.0, "/tool mcp__fake__fail {}").await;
    assert!(ok);
    assert_eq!(stdout.trim(), "tool mcp__fake__fail returned: Tool mcp__fake__fail not found");
    // 没有 mcpServers 的 profile 没有 MCP 工具（向后兼容），也不启动服务器
    let config = write_config(&dir.0, json!({"mcpServers": []}), json!({}));
    let (_, stdout, stderr) = run(&config, &dir.0, r#"/tool mcp__fake__echo {"text":"x"}"#).await;
    assert_eq!(stdout.trim(), "tool mcp__fake__echo returned: Tool mcp__fake__echo not found");
    assert!(!stderr.contains("starting MCP servers"), "{stderr}");
}

#[tokio::test]
async fn approval_policy_applies_to_mcp_tools() {
    let dir = TempDir::new("approval");
    let config = write_config(&dir.0, json!({"toolApproval": {"tools": {"mcp__fake__*": "deny", "mcp__fake__echo": "allow"}}}), json!({}));
    let (ok, stdout, _) = run(&config, &dir.0, "/tool mcp__fake__fail {}").await;
    assert!(ok);
    assert!(stdout.starts_with("tool mcp__fake__fail returned: tool call denied by policy"), "{stdout}");
    // 精确名优先于 glob
    let (_, stdout, _) = run(&config, &dir.0, r#"/tool mcp__fake__echo {"text":"allowed"}"#).await;
    assert_eq!(stdout.trim(), "tool mcp__fake__echo returned: allowed");
    // 全局 never（默认）⇒ 允许
    let config = write_config(&dir.0, json!({}), json!({"toolApproval": "never"}));
    let (_, stdout, _) = run(&config, &dir.0, "/tool mcp__fake__fail {}").await;
    assert_eq!(stdout.trim(), "tool mcp__fake__fail returned: fake failure: something went wrong");
}

#[tokio::test]
async fn startup_failure_non_required_warns_required_is_fatal() {
    let dir = TempDir::new("startup");
    let config = write_config(&dir.0, json!({"mcpServers": ["bad", "fake"]}), json!({}));
    let (ok, stdout, stderr) = run(&config, &dir.0, r#"/tool mcp__fake__echo {"text":"still works"}"#).await;
    assert!(ok, "{stderr}");
    assert_eq!(stdout.trim(), "tool mcp__fake__echo returned: still works");
    assert!(stderr.contains("MCP server startup failed; continuing without its tools") && stderr.contains("server=bad"), "{stderr}");

    let mut cfg: Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    cfg["mcpServers"]["bad"]["required"] = json!(true);
    std::fs::write(&config, cfg.to_string()).unwrap();
    let (ok, stdout, stderr) = run(&config, &dir.0, "hi").await;
    assert!(!ok && stdout.is_empty());
    assert!(stderr.contains("required MCP server 'bad' failed to start") && stderr.contains("exit status: 2"), "{stderr}");
}

/// rpc：一个进程里多次 prompt，验证 isError、崩溃后下一次调用重启、调用超时 + notifications/cancelled。
#[tokio::test]
async fn rpc_is_error_crash_restart_and_timeout() {
    let dir = TempDir::new("rpc");
    let config = write_config(&dir.0, json!({}), json!({}));
    let mut c = command(&config, &dir.0);
    c.arg("rpc").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let stderr = child.stderr.take().unwrap();
    let stderr_task = tokio::spawn(async move {
        let mut text = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            text.push_str(&l);
            text.push('\n');
        }
        text
    });
    let mut prompt = async |message: &str| -> (bool, String) {
        stdin.write_all(format!("{}\n", json!({"id": "1", "type": "prompt", "message": message})).as_bytes()).await.unwrap();
        let mut end = None;
        loop {
            let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line()).await.expect("rpc hung").unwrap().expect("rpc exited");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["type"] == "tool_execution_end" {
                let text = v["result"]["content"].as_array().unwrap().iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n");
                end = Some((v["isError"].as_bool().unwrap(), text));
            }
            if v["type"] == "agent_settled" {
                return end.expect("tool executed");
            }
        }
    };
    let (is_error, text) = prompt("/tool mcp__fake__fail {}").await;
    assert!(is_error);
    assert_eq!(text, "fake failure: something went wrong");

    let (is_error, text) = prompt(r#"/tool mcp__fake__image {}"#).await;
    // faux provider ⇒ 图片转占位文本
    assert_eq!((is_error, text.as_str()), (false, "here is an image\n[image: image/png, 11 bytes]"));

    let (is_error, text) = prompt("/tool mcp__fake__crash {}").await;
    assert!(is_error);
    assert!(text.starts_with("MCP server 'fake' exited (exit status: 3); restarting"), "{text}");
    assert!(text.contains("fake-mcp: crashing on purpose"), "{text}");

    let (is_error, text) = prompt(r#"/tool mcp__fake__echo {"text":"after restart"}"#).await;
    assert_eq!((is_error, text.as_str()), (false, "after restart"));

    let (is_error, text) = prompt(r#"/tool mcp__fake__slow {"secs": 20}"#).await;
    assert!(is_error);
    assert_eq!(text, "MCP tool 'slow' on server 'fake' timed out after 2s (callTimeoutSecs)");
    let mut cancelled = String::new();
    for _ in 0..50 {
        cancelled = std::fs::read_to_string(dir.0.join("fake.log")).unwrap_or_default();
        if cancelled.contains("cancelled") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(cancelled.contains("cancelled"), "server did not receive notifications/cancelled");

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait()).await.expect("rpc did not exit").unwrap();
    assert!(status.success());
    let stderr = stderr_task.await.unwrap();
    assert!(stderr.contains("MCP server crashed; will restart on next use") && stderr.contains("restarting MCP server"), "{stderr}");
    assert_eq!(stderr.matches("MCP server connected").count(), 2, "{stderr}");
}
