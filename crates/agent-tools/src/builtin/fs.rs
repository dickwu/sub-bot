//! 核心编码工具（对应 pi `coding-agent/src/core/tools/{read,write,ls,bash}.ts`）。仅支持 macOS 和 Linux。
//! 所有路径都限制在会话 cwd 之内。
use agent_core::{AgentTool, ExecutionMode, ToolResult, UpdateSink};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT: usize = 50_000;
const MAX_CAPTURE_BYTES: usize = 512 * 1024;
const MAX_READ_BYTES: usize = 2 * 1024 * 1024;
const MAX_LS_ENTRIES: usize = 1000;
const MAX_TIMEOUT_SECS: u64 = 1800;

/// 即使 execute future 被直接丢弃，也清理整个进程组；Child 的 kill_on_drop 负责回收直接子进程。
struct ProcessGroup(u32);

impl ProcessGroup {
    fn kill(&self) {
        // SAFETY: spawn 返回的 PID 是正数，且 process_group(0) 已将它设为 PGID。
        unsafe { libc::killpg(self.0 as libc::pid_t, libc::SIGKILL) };
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

#[derive(Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    dropped: u64,
}

impl CapturedOutput {
    async fn drain(&mut self, mut pipe: impl AsyncRead + Unpin) -> std::io::Result<()> {
        let mut buf = [0; 8192];
        loop {
            let n = pipe.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }
            // 每个流保留一半预算，即使另一个流持续输出也不会饿死。
            let keep = n.min(MAX_CAPTURE_BYTES / 2 - self.bytes.len());
            self.bytes.extend_from_slice(&buf[..keep]);
            self.dropped += (n - keep) as u64;
        }
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_OUTPUT {
        let mut cut = MAX_OUTPUT;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n[output truncated]");
    }
    s
}

/// 把相对路径解析到 root 下，拒绝 `..` 越界与绝对路径。
pub fn resolve_in_root(root: &Path, p: &str) -> Result<PathBuf> {
    let rel = Path::new(p);
    if rel.is_absolute() || rel.components().any(|c| matches!(c, Component::ParentDir | Component::Prefix(_))) {
        bail!("path must be relative to the workspace root and must not contain '..'");
    }
    Ok(root.join(rel))
}

fn arg_str<'a>(args: &'a Value, k: &str) -> Result<&'a str> {
    args.get(k).and_then(|v| v.as_str()).with_context(|| format!("missing string argument '{k}'"))
}

pub struct ReadTool {
    pub root: PathBuf,
}
pub struct WriteTool {
    pub root: PathBuf,
}
pub struct LsTool {
    pub root: PathBuf,
}
pub struct BashTool {
    pub root: PathBuf,
    /// 子进程环境变量策略（默认剔除密钥变量）
    pub env: crate::ShellEnv,
}

#[async_trait::async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a UTF-8 text file relative to the workspace root."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
    }
    async fn execute(&self, _id: &str, args: Value, _c: CancellationToken, _u: UpdateSink) -> Result<ToolResult> {
        let path = resolve_in_root(&self.root, arg_str(&args, "path")?)?;
        let file = tokio::fs::File::open(&path).await?;
        let size = file.metadata().await?.len();
        let mut bytes = Vec::new();
        // 多读一个字节判断截断，也能应对 metadata 不准确或读取期间增长的文件。
        file.take((MAX_READ_BYTES + 1) as u64).read_to_end(&mut bytes).await?;
        let truncated = size > MAX_READ_BYTES as u64 || bytes.len() > MAX_READ_BYTES;
        bytes.truncate(MAX_READ_BYTES);
        let mut text = truncate(String::from_utf8_lossy(&bytes).into_owned());
        if truncated {
            text.push_str(&format!("\n[truncated: read at most {MAX_READ_BYTES} bytes]"));
        }
        Ok(ToolResult::text(text))
    }
}

#[async_trait::async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write (overwrite) a text file relative to the workspace root."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})
    }
    // 写文件串行执行，避免并发写同一文件（pi 用 file-mutation-queue.ts 做得更细）
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }
    async fn execute(&self, _id: &str, args: Value, _c: CancellationToken, _u: UpdateSink) -> Result<ToolResult> {
        let path = resolve_in_root(&self.root, arg_str(&args, "path")?)?;
        let content = arg_str(&args, "content")?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, content).await?;
        Ok(ToolResult::text(format!("wrote {} bytes", content.len())))
    }
}

#[async_trait::async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List a directory relative to the workspace root."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string","default":"."}}})
    }
    async fn execute(&self, _id: &str, args: Value, _c: CancellationToken, _u: UpdateSink) -> Result<ToolResult> {
        let p = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let dir = resolve_in_root(&self.root, p)?;
        let mut rd = tokio::fs::read_dir(&dir).await?;
        let mut names = vec![];
        let mut omitted = 0u64;
        while let Some(e) = rd.next_entry().await? {
            if names.len() == MAX_LS_ENTRIES {
                omitted += 1;
                continue;
            }
            let suffix = if e.file_type().await?.is_dir() { "/" } else { "" };
            names.push(format!("{}{}", e.file_name().to_string_lossy(), suffix));
        }
        names.sort();
        if omitted > 0 {
            names.push(format!("[truncated: {omitted} more entries]"));
        }
        Ok(ToolResult::text(names.join("\n")))
    }
}

#[async_trait::async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command (sh -c) in the workspace root."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"command":{"type":"string"},"timeoutSecs":{"type":"integer"}},"required":["command"]})
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }
    async fn execute(&self, _id: &str, args: Value, cancel: CancellationToken, _u: UpdateSink) -> Result<ToolResult> {
        let cmd = arg_str(&args, "command")?;
        let secs = args.get("timeoutSecs").and_then(|v| v.as_u64()).unwrap_or(120).min(MAX_TIMEOUT_SECS);
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", cmd]);
        c.current_dir(&self.root).kill_on_drop(true).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        c.as_std_mut().process_group(0);
        // 不把 provider key / 服务 token 等密钥传给子进程
        self.env.apply(&mut c);
        let mut child = c.spawn()?;
        let group = ProcessGroup(child.id().context("missing child pid")?);
        let stdout = child.stdout.take().context("missing stdout")?;
        let stderr = child.stderr.take().context("missing stderr")?;
        let mut out = CapturedOutput::default();
        let mut err = CapturedOutput::default();
        // 等待管道 EOF 也受 deadline 约束（shell 退出后后台进程可能仍持有管道）。
        let completion = {
            let collect = async { tokio::try_join!(child.wait(), out.drain(stdout), err.drain(stderr)) };
            tokio::select! {
                r = collect => Some(r),
                _ = tokio::time::sleep(Duration::from_secs(secs)) => None,
                _ = cancel.cancelled() => None,
            }
        };
        let (status, stopped) = match completion {
            Some(Ok((status, (), ()))) => (status, None),
            other => {
                group.kill();
                let status = child.wait().await?;
                if let Some(Err(e)) = other {
                    return Err(e.into());
                }
                let reason = if cancel.is_cancelled() { "[aborted]".to_string() } else { format!("[timed out after {secs}s]") };
                (status, Some(reason))
            }
        };
        let mut text = String::from_utf8_lossy(&out.bytes).into_owned();
        text.push_str(&String::from_utf8_lossy(&err.bytes));
        let code = status.code().unwrap_or(-1);
        let mut text = truncate(format!("{text}\n[exit code {code}]"));
        let dropped = out.dropped + err.dropped;
        if dropped > 0 {
            text.push_str(&format!("\n[dropped {dropped} bytes of output]"));
        }
        if let Some(reason) = &stopped {
            text.push('\n');
            text.push_str(reason);
        }
        let mut r = ToolResult::text(text);
        r.is_error = stopped.is_some() || !status.success();
        r.details = json!({"exitCode": code, "droppedBytes": dropped});
        Ok(r)
    }
}

pub struct EditTool {
    pub root: PathBuf,
}

/// 精确文本替换（对应 pi `tools/edit.ts`：`edits[]` 里每个 `oldText` 必须在文件中唯一出现）。
#[async_trait::async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Edit a file by exact text replacement. Each oldText must match exactly once in the current file."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string"},
            "edits":{"type":"array","items":{"type":"object","properties":{"oldText":{"type":"string"},"newText":{"type":"string"}},"required":["oldText","newText"]}}
        },"required":["path","edits"]})
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }
    async fn execute(&self, _id: &str, args: Value, _c: CancellationToken, _u: UpdateSink) -> Result<ToolResult> {
        let path = resolve_in_root(&self.root, arg_str(&args, "path")?)?;
        let mut text = tokio::fs::read_to_string(&path).await?;
        let edits = args.get("edits").and_then(|v| v.as_array()).context("missing 'edits' array")?;
        for (i, e) in edits.iter().enumerate() {
            let old = arg_str(e, "oldText")?;
            let new = arg_str(e, "newText")?;
            match text.matches(old).count() {
                1 => text = text.replacen(old, new, 1),
                0 => bail!("edit {i}: oldText not found"),
                n => bail!("edit {i}: oldText matches {n} times; add more context to make it unique"),
            }
        }
        tokio::fs::write(&path, &text).await?;
        Ok(ToolResult::text(format!("applied {} edit(s) to {}", edits.len(), arg_str(&args, "path")?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn edit_requires_unique_match_and_stays_in_root() {
        let root = std::env::temp_dir().join(format!("agent-tools-edit-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "x = 1\ny = 1\n").unwrap();
        let t = EditTool { root: root.clone() };
        let sink: UpdateSink = Arc::new(|_| {});
        let ok = t.execute("1", json!({"path":"a.txt","edits":[{"oldText":"x = 1","newText":"x = 2"}]}), CancellationToken::new(), sink.clone()).await;
        assert!(ok.is_ok());
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "x = 2\ny = 1\n");
        let dup = t.execute("2", json!({"path":"a.txt","edits":[{"oldText":"= ","newText":"="}]}), CancellationToken::new(), sink.clone()).await;
        assert!(dup.is_err());
        let esc = t.execute("3", json!({"path":"../a.txt","edits":[]}), CancellationToken::new(), sink).await;
        assert!(esc.is_err());
        std::fs::remove_dir_all(root).ok();
    }

    fn out_text(r: &ToolResult) -> String {
        r.content.iter().filter_map(|c| if let agent_ai::Content::Text { text } = c { Some(text.as_str()) } else { None }).collect()
    }

    async fn assert_dead(pid: i32) {
        for _ in 0..50 {
            // SAFETY: signal 0 only probes this test's child PID.
            if unsafe { libc::kill(pid, 0) } == -1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("grandchild {pid} survived process-group cleanup");
    }

    async fn bash_interrupted(abort: bool) {
        let root = tempfile::tempdir().unwrap();
        let tool = BashTool { root: root.path().to_owned(), env: Default::default() };
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let pidfile = root.path().join("pid");
        let cancel_task = tokio::spawn(async move {
            if abort {
                while !pidfile.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                stop.cancel();
            }
        });
        let result =
            tokio::time::timeout(Duration::from_secs(5), tool.execute("1", json!({"command": "sleep 30 & echo $! > pid; echo started; sleep 30", "timeoutSecs": 1}), cancel, Arc::new(|_| {})))
                .await
                .expect("bash did not return promptly")
                .unwrap();
        cancel_task.await.unwrap();
        assert!(result.is_error);
        let text = out_text(&result);
        assert!(text.contains("started"));
        assert!(text.contains(if abort { "[aborted]" } else { "[timed out after 1s]" }));
        let pid = std::fs::read_to_string(root.path().join("pid")).unwrap().trim().parse().unwrap();
        assert_dead(pid).await;
    }

    #[tokio::test]
    async fn bash_timeout_kills_grandchild() {
        bash_interrupted(false).await;
    }

    #[tokio::test]
    async fn bash_abort_kills_grandchild() {
        bash_interrupted(true).await;
    }

    #[tokio::test]
    async fn bash_dropped_future_kills_group() {
        let root = tempfile::tempdir().unwrap();
        let tool = BashTool { root: root.path().to_owned(), env: Default::default() };
        let task = tokio::spawn(async move { tool.execute("1", json!({"command": "sleep 30 & echo $! > pid; wait"}), CancellationToken::new(), Arc::new(|_| {})).await });
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(root.path().join("pid")).await {
                    if let Ok(pid) = text.trim().parse::<i32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_dead(pid).await;
    }

    #[tokio::test]
    async fn bash_output_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        let tool = BashTool { root: root.path().to_owned(), env: Default::default() };
        let result = tool.execute("1", json!({"command": "yes | head -c 5000000; yes | head -c 5000000 >&2"}), CancellationToken::new(), Arc::new(|_| {})).await.unwrap();
        let text = out_text(&result);
        assert!(!result.is_error);
        assert!(text.len() < MAX_CAPTURE_BYTES);
        assert!(text.contains("dropped"));
        assert_eq!(result.details["droppedBytes"], 10_000_000 - MAX_CAPTURE_BYTES);
    }

    #[tokio::test]
    async fn read_and_ls_are_bounded() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("big"), vec![b'x'; 3 * 1024 * 1024]).unwrap();
        let read = ReadTool { root: root.path().to_owned() };
        let result = read.execute("1", json!({"path": "big"}), CancellationToken::new(), Arc::new(|_| {})).await.unwrap();
        let text = out_text(&result);
        assert!(text.len() < MAX_READ_BYTES);
        assert!(text.contains("truncated: read at most"));
        for i in 0..1005 {
            std::fs::write(root.path().join(format!("entry-{i}")), "").unwrap();
        }
        let ls = LsTool { root: root.path().to_owned() };
        let result = ls.execute("1", json!({}), CancellationToken::new(), Arc::new(|_| {})).await.unwrap();
        let text = out_text(&result);
        assert_eq!(text.lines().count(), MAX_LS_ENTRIES + 1);
        assert!(text.contains("[truncated: 6 more entries]"));
    }

    /// bash 子进程看不到 provider key 等密钥变量。断言信息里不打印 env 输出，避免泄露测试机上的其它变量。
    #[tokio::test]
    async fn bash_does_not_inherit_secret_env() {
        std::env::set_var("OPENAI_API_KEY", "sk-fake-test-key-not-real");
        std::env::set_var("OPENAI_API_KEY_FILE", "/tmp/fake-key-file");
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-fake-test");
        std::env::set_var("SUBBOT_TEST_VISIBLE", "yes");
        std::env::set_var("SUBBOT_TEST_DENIED", "secret");
        let root = std::env::temp_dir();
        let sink: UpdateSink = Arc::new(|_| {});
        let run = |env: crate::ShellEnv, cmd: &'static str| {
            let t = BashTool { root: root.clone(), env };
            let sink = sink.clone();
            async move { out_text(&t.execute("1", json!({"command": cmd}), CancellationToken::new(), sink).await.unwrap()) }
        };
        // 默认 inherit：密钥被剔除，其它变量保留
        let out = run(Default::default(), "env | grep -c -e '^OPENAI_API_KEY' -e '^ANTHROPIC_API_KEY' -e 'sk-fake-test-key-not-real' -e 'sk-ant-fake-test'; echo visible=$SUBBOT_TEST_VISIBLE").await;
        assert!(out.starts_with("0\n"), "secret env vars leaked into bash (output withheld)");
        assert!(out.contains("visible=yes"), "non-secret variable should be inherited");
        // deny 额外剔除
        let deny = crate::ShellEnv { deny: vec!["SUBBOT_TEST_DEN*".into()], ..Default::default() };
        assert!(run(deny, "echo d=${SUBBOT_TEST_DENIED:-unset}").await.contains("d=unset"));
        // allowlist：只保留基础变量和 allow
        let allow = crate::ShellEnv { mode: crate::EnvMode::Allowlist, allow: vec!["OPENAI_API_KEY".into()], ..Default::default() };
        let out = run(allow, "echo v=${SUBBOT_TEST_VISIBLE:-unset} p=${PATH:+set} k=${OPENAI_API_KEY:-unset}").await;
        assert!(out.contains("v=unset p=set k=unset"), "allowlist mode misbehaved");
    }
}
