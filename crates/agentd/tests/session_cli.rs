//! `sub-bot run` 会话持久化与 `--resume`（faux provider，离线）。
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("sub-bot-session-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sub_bot_command(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sub-bot"));
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config-home"))
        .arg("--provider")
        .arg("faux")
        .arg("--workspace")
        .arg(dir)
        .arg("--data-dir")
        .arg(dir.join("data"))
        .args(["--log-dir", "-"]);
    cmd
}

fn sub_bot(dir: &Path, args: &[&str]) -> Output {
    sub_bot_command(dir).args(args).output().unwrap()
}

fn json(output: &Output) -> Value {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn run_cwd_relative_to_invocation_and_resume() {
    let dir = TempDir::new();
    let invoke = dir.0.join("invoke");
    let child = invoke.join("child");
    let base = invoke.join("profile-default");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir(&base).unwrap();
    fs::write(child.join("marker"), "process-local child").unwrap();
    let cfg = dir.0.join("config.json");
    fs::write(&cfg, json!({"pricing":{"source":"builtin"}, "profiles":{"default":{"workspaceRoots":["./profile-default"]}}}).to_string()).unwrap();
    let run = |from: &Path, args: &[&str]| sub_bot_command(&dir.0).current_dir(from).arg("--config").arg(&cfg).arg("run").arg("--json").args(args).output().unwrap();
    let child = fs::canonicalize(child).unwrap();
    let base = fs::canonicalize(base).unwrap();
    let expected = json!(child);
    let read = r#"/tool read {"path":"marker"}"#;
    let first = json(&run(&invoke, &["--cwd", "child", read]));
    assert_eq!((&first["cwd"], &first["sessionRoot"]), (&expected, &expected));
    assert_eq!(first["text"], "tool read returned: process-local child");
    let id = first["sessionId"].as_str().unwrap();
    let local = dir.0.join("data/sessions/local");
    let files: Vec<_> = fs::read_dir(&local).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files.len(), 1);
    let path = &files[0];
    for (from, args) in [(&dir.0, vec!["--resume", id, read]), (&base, vec!["--resume", id, "--cwd", "../child/.", read])] {
        let result = json(&run(from, &args));
        assert_eq!(result["sessionId"], id);
        assert_eq!(result["cwd"], expected);
        assert_eq!(result["sessionRoot"], expected);
        assert_eq!(result["text"], first["text"]);
    }
    let before = fs::read(path).unwrap();
    for (cwd, error) in [(".", "cwd is fixed for this session"), ("", "cwd must not be empty"), ("missing", "cwd does not exist")] {
        let rejected = run(&invoke, &["--resume", id, "--cwd", cwd, "not appended"]);
        assert!(!rejected.status.success());
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(error), "{}", String::from_utf8_lossy(&rejected.stderr));
        assert_eq!(fs::read(path).unwrap(), before);
    }
    let default = json(&run(&invoke, &["default"]));
    assert_eq!(default["cwd"], json!(base));
    assert_eq!(default["sessionRoot"], default["cwd"]);
}

#[test]
fn rpc_cwd_is_process_relative_for_flag_and_first_prompt() {
    use std::{io::Write, process::Stdio};
    let dir = TempDir::new();
    let invoke = dir.0.join("invoke");
    let child = invoke.join("child");
    fs::create_dir_all(&child).unwrap();
    let cwd = json!(fs::canonicalize(&child).unwrap());
    let cases = [
        (vec!["rpc", "--cwd", "child"], "{\"id\":2,\"type\":\"get_state\"}\n"),
        (vec!["rpc"], "{\"id\":1,\"type\":\"prompt\",\"cwd\":\"child\",\"message\":\"hi\"}\n{\"id\":2,\"type\":\"get_state\"}\n"),
    ];
    for (args, input) in cases {
        let mut proc = sub_bot_command(&dir.0).current_dir(&invoke).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        proc.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = proc.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let lines: Vec<Value> = std::str::from_utf8(&out.stdout).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let state = lines.iter().find(|line| line["command"] == "get_state").unwrap();
        assert_eq!(state["data"]["cwd"], cwd);
        assert_eq!(state["data"]["sessionRoot"], json!(fs::canonicalize(&dir.0).unwrap()));
    }
    let out = sub_bot_command(&dir.0).current_dir(&invoke).args(["rpc", "--cwd", ""]).output().unwrap();
    assert!(!out.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), json!({"type":"response", "command":"start", "success":false, "error":"cwd must not be empty"}));
}

#[test]
fn profiles_lists_workspace_policies_offline() {
    let dir = TempDir::new();
    let cfg = dir.0.join("config.json");
    fs::write(
        &cfg,
        json!({
            "profiles": {
                "z-list": {"workspaceRoots":["relative"], "allowedWorkspaceRoots":["/not mounted"]},
                "a-any": {"workspaceRoots":["./missing"]},
                "b-roots-only": {"workspaceRoots":["/unavailable"], "allowedWorkspaceRoots":[]}
            },
            "clients": [{"name":"test", "webhookSecretFile":"nonexistent"}],
            "apiKeyFile":"nonexistent",
            "alerts": {"webhookSecretFile":"nonexistent"}
        })
        .to_string(),
    )
    .unwrap();
    let path = cfg.to_str().unwrap();
    let expected = json!({"profiles":[
        {"name":"a-any", "workspaceRoots":["./missing"], "allowedWorkspaceRoots":["*"]},
        {"name":"b-roots-only", "workspaceRoots":["/unavailable"], "allowedWorkspaceRoots":[]},
        {"name":"z-list", "workspaceRoots":["relative"], "allowedWorkspaceRoots":["/not mounted"]}
    ]});
    assert_eq!(json(&sub_bot(&dir.0, &["--config", path, "profiles", "--json"])), expected);
    let text = sub_bot(&dir.0, &["--config", path, "profiles"]);
    assert!(text.status.success());
    assert_eq!(
        String::from_utf8(text.stdout).unwrap(),
        concat!(
            "a-any: workspaceRoots=[\"./missing\"] allowedWorkspaceRoots=[\"*\"]\n",
            "b-roots-only: workspaceRoots=[\"/unavailable\"] allowedWorkspaceRoots=[]\n",
            "z-list: workspaceRoots=[\"relative\"] allowedWorkspaceRoots=[\"/not mounted\"]\n",
        )
    );
    let fallback = json(&sub_bot(&dir.0, &["profiles", "--json"]));
    assert_eq!(fallback, json!({"profiles":[{"name":"default", "workspaceRoots":[dir.0], "allowedWorkspaceRoots":["*"]}]}));
    assert!(!dir.0.join("data").exists(), "listing is read-only");

    for (config, error) in [
        (r#"{"profiles":{"p":{"workspaceRoots":["."],"allowedWorkspaceRoots":["relative"]}}}"#, "allowedWorkspaceRoots"),
        (r#"{"profiles":{"p":{"workspaceRoots":["."],"allowedWorkspaceRoots":"unlogged-invalid-policy"}}}"#, "invalid configuration for profiles"),
    ] {
        fs::write(&cfg, config).unwrap();
        let out = sub_bot(&dir.0, &["--config", path, "profiles"]);
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        let log = String::from_utf8_lossy(&out.stderr);
        assert!(log.contains(error) && !log.contains("unlogged-invalid-policy"), "{log}");
    }
}

#[test]
fn run_persists_by_default_and_resumes() {
    let dir = TempDir::new();
    let first = json(&sub_bot(&dir.0, &["run", "--json", "hello"]));
    let id = first["sessionId"].as_str().unwrap().to_string();
    assert_eq!((first["persisted"].clone(), first["resumed"].clone(), first["text"].clone()), (Value::Bool(true), Value::Bool(false), Value::from("echo: hello")));
    let local = dir.0.join("data/sessions/local");
    let files: Vec<_> = std::fs::read_dir(&local).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files.len(), 1);
    assert!(files[0].file_name().unwrap().to_str().unwrap().ends_with(&format!("_{id}.jsonl")));
    assert_eq!(std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&local).unwrap().permissions().mode() & 0o777, 0o700);

    // 文本模式：会话 id 打到 stderr，stdout 只有回复
    let output = sub_bot(&dir.0, &["run", "--resume", &id, "again"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "echo: again\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("[session] {id}")));
    let resumed = json(&sub_bot(&dir.0, &["run", "--resume", &id, "--json", "third"]));
    assert_eq!((resumed["sessionId"].as_str(), resumed["resumed"].clone()), (Some(id.as_str()), Value::Bool(true)));
    // 同一个文件里有三轮对话
    let text = std::fs::read_to_string(&files[0]).unwrap();
    let messages = text.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).filter(|e| e["type"] == "message").count();
    assert_eq!(messages, 6);
    assert_eq!(std::fs::read_dir(&local).unwrap().count(), 1);

    // 不存在 / 非法 id、关闭持久化时 --resume 失败
    for (args, expected) in [
        (vec!["run", "--resume", "00000000-0000-4000-8000-000000000000", "x"], "not found"),
        (vec!["run", "--resume", "../../etc/passwd", "x"], "invalid session id"),
        (vec!["--no-persist-sessions", "run", "--resume", id.as_str(), "x"], "persistence is disabled"),
    ] {
        let output = sub_bot(&dir.0, &args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }
    let ephemeral = json(&sub_bot(&dir.0, &["--no-persist-sessions", "run", "--json", "x"]));
    assert_eq!(ephemeral["persisted"], false);
    assert_eq!(std::fs::read_dir(&local).unwrap().count(), 1);
}
