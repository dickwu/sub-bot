//! `sub-bot run` 的工具审批：非交互时 ask ⇒ deny + 警告；`--auto-approve` ⇒ 执行。用 faux provider，不访问网络。
#![cfg(unix)]

use serde_json::json;
use std::path::PathBuf;

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("sb-approval-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn run(dir: &TempDir, extra: &[&str]) -> (bool, String, String) {
    let config = dir.0.join("config.json");
    std::fs::write(
        &config,
        json!({
            "dataDir": dir.0.join("data"),
            "profiles": {"default": {"workspaceRoots": [dir.0], "loadContextFiles": false, "toolApproval": {"tools": {"ls": "ask", "write": "deny"}}}},
            "clients": []
        })
        .to_string(),
    )
    .unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sub-bot"));
    for var in ["SUB_BOT_CONFIG", "SUB_BOT_MODEL", "SUB_BOT_PROVIDER", "SUB_BOT_TOKEN", "SUB_BOT_ACCOUNT", "SUB_BOT_LOG_DIR"] {
        command.env_remove(var);
    }
    command.arg("--config").arg(&config).args(["--provider", "faux", "--log-dir", "-", "run"]).args(extra).arg("/tool ls {}");
    // output(): stdin 关闭、stderr 被捕获 ⇒ 非交互
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), command.output()).await.expect("run hung").unwrap();
    (output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned())
}

#[tokio::test]
async fn run_denies_ask_non_interactively_and_auto_approve_executes() {
    let dir = TempDir::new();
    std::fs::write(dir.0.join("marker.txt"), "x").unwrap();
    let (ok, stdout, stderr) = run(&dir, &[]).await;
    assert!(ok, "{stderr}");
    assert_eq!(stdout.trim(), "tool ls returned: tool call denied by user: non-interactive run (use --auto-approve)");
    assert!(stderr.contains("warning: tool ls requires approval (tool 'ls' requires approval (tools rule 'ls')); denied because run is non-interactive"), "{stderr}");

    let (ok, stdout, stderr) = run(&dir, &["--auto-approve"]).await;
    assert!(ok, "{stderr}");
    assert!(stdout.contains("tool ls returned:") && stdout.contains("marker.txt"), "{stdout}");
    assert!(!stdout.contains("denied"));
}
