//! 核心编码工具（对应 pi `coding-agent/src/core/tools/{read,write,ls,bash}.ts`）。仅支持 macOS 和 Linux。
//! 所有路径都限制在会话 cwd 之内。
use agent_core::{AgentTool, ExecutionMode, ToolResult, UpdateSink};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT: usize = 50_000;

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
        Ok(ToolResult::text(truncate(tokio::fs::read_to_string(&path).await?)))
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
        while let Some(e) = rd.next_entry().await? {
            let suffix = if e.file_type().await?.is_dir() { "/" } else { "" };
            names.push(format!("{}{}", e.file_name().to_string_lossy(), suffix));
        }
        names.sort();
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
        let secs = args.get("timeoutSecs").and_then(|v| v.as_u64()).unwrap_or(120);
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", cmd]);
        c.current_dir(&self.root).kill_on_drop(true);
        let out = tokio::select! {
            r = tokio::time::timeout(std::time::Duration::from_secs(secs), c.output()) => r.context("command timed out")??,
            _ = cancel.cancelled() => bail!("aborted"),
        };
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        let code = out.status.code().unwrap_or(-1);
        let mut r = ToolResult::text(truncate(format!("{text}\n[exit code {code}]")));
        r.is_error = !out.status.success();
        r.details = json!({"exitCode": code});
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
}
