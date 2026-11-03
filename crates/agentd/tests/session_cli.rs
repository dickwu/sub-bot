//! `sub-bot run` 会话持久化与 `--resume`（faux provider，离线）。
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sub_bot(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_sub-bot"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .arg("--provider")
        .arg("faux")
        .arg("--workspace")
        .arg(dir)
        .arg("--data-dir")
        .arg(dir.join("data"))
        .args(["--log-dir", "-"])
        .args(args)
        .output()
        .unwrap()
}

fn json(output: &std::process::Output) -> Value {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn run_persists_by_default_and_resumes() {
    let dir = TempDir(std::env::temp_dir().join(format!("sub-bot-run-resume-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
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
