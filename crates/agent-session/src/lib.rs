//! JSONL 会话树，对应 pi `coding-agent/src/core/session-manager.ts` 与 `docs/session-format.md`。
//! 第一行是 header，之后每行一个 entry，通过 id/parentId 组成树；当前叶子决定活动分支。
//!
//! 兼容性：v1 文件（header 没有 `meta`、没有 `session_info` / 压缩字段）照常加载；新字段全部可选。
//! `session_info` 是不进入树的元数据条目（后写覆盖先写），用于在重启后重建会话（模型、推理强度、路由、cwd…）。
pub mod compaction;

use agent_ai::{Content, Message, ThinkingLevel, ToolResultMessage};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// v2：header 可带 `meta`，新增 `session_info` 条目与压缩 token 统计。读取时接受任何版本。
pub const SESSION_VERSION: u32 = 2;

/// 重建会话所需的元数据。所有字段可选：旧文件没有，新文件只写已知的值。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 调用方是否显式指定了模型（显式模型不参与 depth 路由）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit_model: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_reasoning_effort: Option<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ThinkingLevel>,
    /// 路由决定（agentd `Routing` 的 JSON）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// 调用方覆盖的系统提示（只有请求里给了才记录；profile 自身的提示按 profile 名重建）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    /// 创建时完整系统提示的 SHA-256（重建后比较，用于发现 AGENTS.md / profile 变化）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_sha256: Option<String>,
    /// 调用方回调工具的名字（回调地址与 token 不落盘，恢复后不可用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_tools: Option<Vec<String>>,
    /// /v1/chat 自动创建的会话（恢复后沿用 chat TTL）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    /// 会话级工具审批策略（请求的 `toolApproval`，只能收紧 profile 策略）；恢复时与当前 profile 策略叠加
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_approval: Option<serde_json::Value>,
    /// 会话级回合上限（请求的 `maxTurns`；运行期调整只保留在内存里）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u64>,
    /// 会话级 onMaxTurns（stop | ask | summarize）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_max_turns: Option<String>,
    /// 会话级 maxTurnsAskTimeoutSecs
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns_ask_timeout_secs: Option<u64>,
    /// 会话级 streamIdleTimeoutSecs（请求覆盖；恢复后沿用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_idle_timeout_secs: Option<u64>,
    /// 会话级 llmMaxRetries（请求覆盖；恢复后沿用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_max_retries: Option<u32>,
    /// 第一条用户消息的预览
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl SessionMeta {
    /// 用 `other` 中已设置的字段覆盖自身。
    pub fn merge(&mut self, other: &SessionMeta) {
        macro_rules! overlay {
            ($($f:ident),*) => { $( if other.$f.is_some() { self.$f = other.$f.clone(); } )* };
        }
        overlay!(
            client,
            provider,
            account,
            model,
            explicit_model,
            api,
            cwd,
            requested_reasoning_effort,
            reasoning_effort,
            routing,
            tools,
            system_prompt,
            append_system_prompt,
            system_prompt_sha256,
            caller_tools,
            ephemeral,
            tool_approval,
            max_turns,
            on_max_turns,
            max_turns_ask_timeout_secs,
            stream_idle_timeout_secs,
            llm_max_retries,
            title
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Entry {
    Session {
        version: u32,
        id: String,
        profile: String,
        timestamp: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<SessionMeta>,
    },
    Message {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        message: Message,
    },
    ModelChange {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        provider: String,
        model_id: String,
    },
    /// 压缩摘要：替换 first_kept_entry_id 之前的上下文（对应 compaction entry）
    Compaction {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        summary: String,
        first_kept_entry_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_before: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_after: Option<u64>,
    },
    /// 元数据更新（不进入树，不改变叶子）；按文件顺序合并到 header.meta 上。
    SessionInfo {
        id: String,
        timestamp: String,
        meta: SessionMeta,
    },
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Entry::Session { id, .. } | Entry::Message { id, .. } | Entry::ModelChange { id, .. } | Entry::Compaction { id, .. } | Entry::SessionInfo { id, .. } => id,
        }
    }
    pub fn parent(&self) -> Option<&str> {
        match self {
            Entry::Session { .. } | Entry::SessionInfo { .. } => None,
            Entry::Message { parent_id, .. } | Entry::ModelChange { parent_id, .. } | Entry::Compaction { parent_id, .. } => parent_id.as_deref(),
        }
    }
    fn in_tree(&self) -> bool {
        !matches!(self, Entry::Session { .. } | Entry::SessionInfo { .. })
    }
    pub fn timestamp(&self) -> &str {
        match self {
            Entry::Session { timestamp, .. } | Entry::Message { timestamp, .. } | Entry::ModelChange { timestamp, .. } | Entry::Compaction { timestamp, .. } | Entry::SessionInfo { timestamp, .. } => {
                timestamp
            }
        }
    }
}

pub struct Session {
    pub id: String,
    pub path: PathBuf,
    entries: Vec<Entry>,
    leaf: Option<String>,
    meta: SessionMeta,
    /// 打开时发现并截掉了崩溃留下的不完整末行
    pub torn_tail: bool,
}

fn ts() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// RFC 3339 时间戳 → Unix 毫秒。
pub fn timestamp_ms(ts: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(ts).ok().map(|d| d.timestamp_millis().max(0) as u64)
}

/// 会话 id 必须是规范的小写带连字符 UUID（拒绝路径分隔符、`..`、大小写变体等）。
pub fn valid_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|u| u.hyphenated().to_string() == id)
}

/// 创建 0700 目录（已存在时收紧为 0700）。
pub fn private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder.create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    builder.create(dir)?;
    Ok(())
}

fn open_append(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// 会话目录里的一个文件（只看文件名，不解析内容）。
#[derive(Debug, Clone)]
pub struct SessionFile {
    pub id: String,
    pub path: PathBuf,
    pub modified: std::time::SystemTime,
}

/// 文件名 `<UTC 时间>_<uuid>.jsonl` → id。
fn file_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_suffix(".jsonl")?.split_once('_')?.1;
    valid_id(id).then(|| id.to_string())
}

/// 列出目录下的会话文件（目录不存在 ⇒ 空）。
pub fn list_files(dir: &Path) -> Vec<SessionFile> {
    let Ok(read) = fs::read_dir(dir) else { return vec![] };
    read.filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let id = file_id(&path)?;
            let meta = fs::symlink_metadata(&path).ok().filter(|m| m.is_file())?;
            Some(SessionFile { id, modified: meta.modified().unwrap_or(std::time::UNIX_EPOCH), path })
        })
        .collect()
}

/// 最后一个条目的时间戳（Unix 毫秒），与 `Session::read(path).updated_at()` 一致（崩溃残行被忽略），
/// 但只从文件末尾往回读到最后一个完整条目为止，不解析整个文件。读不到 / 末条目损坏 ⇒ None。
pub fn last_timestamp_ms(path: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    #[derive(Deserialize)]
    struct Stamp {
        timestamp: String,
    }
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut window = 8 * 1024u64;
    loop {
        let start = len.saturating_sub(window);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut tail = Vec::with_capacity((len - start) as usize);
        Read::by_ref(&mut file).take(len - start).read_to_end(&mut tail).ok()?;
        let mut lines: Vec<&[u8]> = tail.split_inclusive(|b| *b == b'\n').collect();
        if start > 0 && !lines.is_empty() {
            // 窗口的第一段可能只是某一行的后半截
            lines.remove(0);
        }
        for (i, line) in lines.iter().enumerate().rev() {
            let complete = line.ends_with(b"\n");
            let text = std::str::from_utf8(line).unwrap_or("");
            if text.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Stamp>(text) {
                Ok(s) => return timestamp_ms(&s.timestamp),
                // 末尾的不完整残行（崩溃）：`Session::read` 会忽略它，这里也跳过
                Err(_) if !complete && i + 1 == lines.len() => continue,
                Err(_) => return None,
            }
        }
        if start == 0 {
            return None;
        }
        window = window.saturating_mul(2);
    }
}

/// 按 id 查找会话文件；id 不合法时直接返回 None（不访问文件系统）。
pub fn find(dir: &Path, id: &str) -> Option<PathBuf> {
    if !valid_id(id) {
        return None;
    }
    list_files(dir).into_iter().find(|f| f.id == id).map(|f| f.path)
}

/// 被中断的工具调用（结果缺失）补上的合成错误结果文本。
pub const INTERRUPTED_TOOL_TEXT: &str = "interrupted: sub-bot restarted before the tool finished";

impl Session {
    pub fn create(dir: &Path, profile: &str) -> Result<Self> {
        Self::create_with_meta(dir, profile, SessionMeta::default())
    }

    /// 新建会话文件（目录 0700，文件 0600），header 带元数据。
    pub fn create_with_meta(dir: &Path, profile: &str, meta: SessionMeta) -> Result<Self> {
        Self::create_with_id(dir, &uuid::Uuid::new_v4().to_string(), profile, meta)
    }

    /// 用给定 id（规范 UUID）新建会话文件。
    pub fn create_with_id(dir: &Path, id: &str, profile: &str, meta: SessionMeta) -> Result<Self> {
        anyhow::ensure!(valid_id(id), "invalid session id");
        private_dir(dir)?;
        let id = id.to_string();
        let path = dir.join(format!("{}_{}.jsonl", chrono::Utc::now().format("%Y%m%dT%H%M%S"), id));
        let mut s = Self { id: id.clone(), path, entries: vec![], leaf: None, meta: meta.clone(), torn_tail: false };
        let meta = (meta != SessionMeta::default()).then_some(meta);
        s.append_raw(Entry::Session { version: SESSION_VERSION, id, profile: profile.into(), timestamp: ts(), meta })?;
        Ok(s)
    }

    /// 读取会话用于继续写入。崩溃留下的不完整末行（没有换行且无法解析）会被截掉并标记 `torn_tail`；其它损坏行报错。
    pub fn open(path: &Path) -> Result<Self> {
        Self::load(path, true)
    }

    /// 只读加载（列表等）：忽略不完整末行，不修改文件（文件可能正被另一个进程追加）。
    pub fn read(path: &Path) -> Result<Self> {
        Self::load(path, false)
    }

    fn load(path: &Path, repair: bool) -> Result<Self> {
        let bytes = fs::read(path)?;
        let mut entries = vec![];
        let mut offset = 0usize;
        let mut torn_tail = false;
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            let complete = line.ends_with(b"\n");
            let text = std::str::from_utf8(line).unwrap_or("");
            if text.trim().is_empty() && complete {
                offset += line.len();
                continue;
            }
            match serde_json::from_str::<Entry>(text) {
                Ok(e) => entries.push(e),
                Err(_) if !complete => {
                    torn_tail = true;
                    break;
                }
                Err(e) => anyhow::bail!("corrupt session entry at byte {offset}: {e}"),
            }
            offset += line.len();
        }
        if repair && (torn_tail || (!bytes.is_empty() && !bytes.ends_with(b"\n") && offset == bytes.len())) {
            // 截掉残行 / 补上换行，后续追加不会和残行粘在一起
            let f = OpenOptions::new().write(true).open(path)?;
            if torn_tail {
                f.set_len(offset as u64)?;
            } else {
                drop(f);
                open_append(path)?.write_all(b"\n")?;
            }
        }
        let (id, mut meta) = match entries.first() {
            Some(Entry::Session { id, meta, .. }) => (id.clone(), meta.clone().unwrap_or_default()),
            _ => anyhow::bail!("missing session header"),
        };
        for e in &entries {
            if let Entry::SessionInfo { meta: m, .. } = e {
                meta.merge(m);
            }
        }
        let leaf = entries.iter().rev().find(|e| e.in_tree()).map(|e| e.id().to_string());
        Ok(Self { id, path: path.into(), entries, leaf, meta, torn_tail })
    }

    fn append_raw(&mut self, e: Entry) -> Result<()> {
        let mut f = open_append(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(&e)?)?;
        if e.in_tree() {
            self.leaf = Some(e.id().to_string());
        }
        self.entries.push(e);
        Ok(())
    }

    pub fn append_message(&mut self, message: Message) -> Result<String> {
        let id = short_id();
        self.append_raw(Entry::Message { id: id.clone(), parent_id: self.leaf.clone(), timestamp: ts(), message })?;
        Ok(id)
    }

    /// 写入压缩条目；之后活动上下文 = 摘要 + first_kept_entry_id 起保留的消息 + 之后的新消息。
    pub fn append_compaction(&mut self, summary: String, first_kept_entry_id: String, tokens_before: Option<u64>, tokens_after: Option<u64>) -> Result<String> {
        let id = short_id();
        self.append_raw(Entry::Compaction { id: id.clone(), parent_id: self.leaf.clone(), timestamp: ts(), summary, first_kept_entry_id, tokens_before, tokens_after })?;
        Ok(id)
    }

    /// 合并元数据；只有合并后发生变化时才追加一条 `session_info`。返回是否写入。
    pub fn update_meta(&mut self, patch: &SessionMeta) -> Result<bool> {
        let mut merged = self.meta.clone();
        merged.merge(patch);
        if merged == self.meta {
            return Ok(false);
        }
        self.append_raw(Entry::SessionInfo { id: short_id(), timestamp: ts(), meta: patch.clone() })?;
        self.meta = merged;
        Ok(true)
    }

    /// 合并后的元数据（header + 所有 session_info）。
    pub fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    pub fn profile(&self) -> &str {
        match self.entries.first() {
            Some(Entry::Session { profile, .. }) => profile,
            _ => "",
        }
    }

    /// header 时间戳（RFC 3339）
    pub fn created_at(&self) -> &str {
        self.entries.first().map(Entry::timestamp).unwrap_or("")
    }

    /// 最后一个条目的时间戳（RFC 3339）
    pub fn updated_at(&self) -> &str {
        self.entries.last().map(Entry::timestamp).unwrap_or("")
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 从某个历史 entry 继续：之后的 append 会形成新分支（对应 branch / fork）。
    pub fn set_leaf(&mut self, entry_id: &str) {
        self.leaf = Some(entry_id.to_string());
    }

    fn chain(&self) -> Vec<&Entry> {
        let by_id: HashMap<&str, &Entry> = self.entries.iter().filter(|e| e.in_tree()).map(|e| (e.id(), e)).collect();
        let mut chain = vec![];
        let mut seen = HashSet::new();
        let mut cur = self.leaf.as_deref();
        while let Some(id) = cur {
            let Some(e) = by_id.get(id) else { break };
            if !seen.insert(id) {
                break;
            }
            chain.push(*e);
            cur = e.parent();
        }
        chain.reverse();
        chain
    }

    /// 活动上下文：(entry id, 消息)。最近一次压缩之后为 [摘要（id None）] + 保留消息 + 新消息（对应 buildSessionContext）。
    /// 保留消息（压缩之前写入的）去掉 thinking 块（见 [`compaction::strip_thinking`]），与在线压缩得到的上下文一致；
    /// 文件里的消息本身不变。压缩之后追加的消息原样返回。
    pub fn active_context(&self) -> Vec<(Option<String>, Message)> {
        let chain = self.chain();
        let last_compaction = chain.iter().rposition(|e| matches!(e, Entry::Compaction { .. }));
        let mut out = vec![];
        let start = match last_compaction {
            None => 0,
            Some(c) => {
                let Entry::Compaction { summary, first_kept_entry_id, .. } = chain[c] else { unreachable!() };
                out.push((None, compaction::summary_message(summary)));
                if let Some(k) = chain[..c].iter().position(|e| e.id() == first_kept_entry_id) {
                    for e in &chain[k..c] {
                        if let Entry::Message { id, message, .. } = e {
                            if let Some(message) = compaction::strip_thinking(message.clone()) {
                                out.push((Some(id.clone()), message));
                            }
                        }
                    }
                }
                c + 1
            }
        };
        for e in &chain[start..] {
            if let Entry::Message { id, message, .. } = e {
                out.push((Some(id.clone()), message.clone()));
            }
        }
        out
    }

    /// 活动分支上的消息（考虑压缩）。
    pub fn active_messages(&self) -> Vec<Message> {
        self.active_context().into_iter().map(|(_, m)| m).collect()
    }

    /// 活动分支上是否有压缩条目。
    pub fn is_compacted(&self) -> bool {
        self.chain().iter().any(|e| matches!(e, Entry::Compaction { .. }))
    }

    /// 修复崩溃：活动上下文末尾的助手工具调用没有对应结果 ⇒ 追加合成的错误结果（保持 tool_use / tool_result 配对）。
    /// 返回补上的工具调用 id。
    pub fn repair_interrupted_tools(&mut self) -> Result<Vec<String>> {
        let missing = missing_tool_results(&self.active_messages());
        for r in &missing {
            self.append_message(Message::ToolResult(r.clone()))?;
        }
        Ok(missing.into_iter().map(|r| r.tool_call_id).collect())
    }
}

/// 最后一条助手消息里没有结果的工具调用 ⇒ 合成错误结果（按调用顺序）。
pub fn missing_tool_results(messages: &[Message]) -> Vec<ToolResultMessage> {
    let Some(pos) = messages.iter().rposition(|m| matches!(m, Message::Assistant(_))) else { return vec![] };
    let Message::Assistant(a) = &messages[pos] else { unreachable!() };
    let answered: HashSet<&str> = messages[pos + 1..].iter().filter_map(|m| if let Message::ToolResult(r) = m { Some(r.tool_call_id.as_str()) } else { None }).collect();
    a.tool_calls()
        .into_iter()
        .filter(|c| !answered.contains(c.id.as_str()))
        .map(|c| ToolResultMessage {
            tool_call_id: c.id.clone(),
            tool_name: c.name.clone(),
            content: vec![Content::Text { text: INTERRUPTED_TOOL_TEXT.into() }],
            is_error: true,
            timestamp: agent_ai::now_ms(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ai::{AssistantMessage, StopReason, ToolCall};

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("sess-{tag}-{}", short_id()))
    }

    #[test]
    fn roundtrip_and_branch() {
        let dir = temp_dir("rt");
        let mut s = Session::create(&dir, "/tmp").unwrap();
        let a = s.append_message(Message::user_text("a")).unwrap();
        s.append_message(Message::user_text("b")).unwrap();
        s.set_leaf(&a);
        s.append_message(Message::user_text("c")).unwrap();
        assert_eq!(s.active_messages().len(), 2);
        let re = Session::open(&s.path).unwrap();
        assert_eq!(re.active_messages(), s.active_messages());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn v1_files_without_new_fields_still_load() {
        let dir = temp_dir("v1");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("20250101T000000_0b0e8a64-6f0e-4a59-9e41-5d0c1d9e2f11.jsonl");
        let lines = [
            r#"{"type":"session","version":1,"id":"0b0e8a64-6f0e-4a59-9e41-5d0c1d9e2f11","profile":"coder","timestamp":"2025-01-01T00:00:00Z"}"#,
            r#"{"type":"message","id":"aaaa","parentId":null,"timestamp":"2025-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1}}"#,
            r#"{"type":"compaction","id":"cccc","parentId":"aaaa","timestamp":"2025-01-01T00:00:02Z","summary":"old","firstKeptEntryId":"aaaa"}"#,
        ];
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        let s = Session::open(&path).unwrap();
        assert_eq!(s.id, "0b0e8a64-6f0e-4a59-9e41-5d0c1d9e2f11");
        assert_eq!(s.profile(), "coder");
        assert_eq!(s.meta(), &SessionMeta::default());
        assert_eq!(s.active_messages(), vec![compaction::summary_message("old"), Message::User { content: vec![Content::Text { text: "hi".into() }], timestamp: 1 }]);
        assert_eq!(find(&dir, &s.id), Some(path.clone()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn meta_merges_and_only_writes_changes() {
        let dir = temp_dir("meta");
        let meta = SessionMeta { model: Some("m1".into()), cwd: Some("/w".into()), ..Default::default() };
        let mut s = Session::create_with_meta(&dir, "p", meta).unwrap();
        assert!(!s.update_meta(&SessionMeta { model: Some("m1".into()), ..Default::default() }).unwrap());
        assert!(s.update_meta(&SessionMeta { model: Some("m2".into()), title: Some("t".into()), ..Default::default() }).unwrap());
        let re = Session::open(&s.path).unwrap();
        assert_eq!(re.meta().model.as_deref(), Some("m2"));
        assert_eq!(re.meta().cwd.as_deref(), Some(Path::new("/w")));
        assert_eq!(re.meta().title.as_deref(), Some("t"));
        // session_info 不进入树
        assert!(re.active_messages().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn last_timestamp_reads_tail_and_matches_full_parse() {
        let dir = temp_dir("tail");
        let mut s = Session::create(&dir, "p").unwrap();
        // 跨越多个读取窗口的长末行
        s.append_message(Message::user_text("x".repeat(40_000))).unwrap();
        let full = || timestamp_ms(Session::read(&s.path).unwrap().updated_at());
        assert_eq!(last_timestamp_ms(&s.path), full());
        // 崩溃残行被忽略（与 Session::read 一致）
        open_append(&s.path).unwrap().write_all(br#"{"type":"message","id":"zz","timestamp":"2999-01-01T00:00:00Z","mess"#).unwrap();
        assert_eq!(last_timestamp_ms(&s.path), full());
        assert!(last_timestamp_ms(&s.path).is_some());
        assert_eq!(last_timestamp_ms(&dir.join("missing.jsonl")), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perm");
        let s = Session::create(&dir.join("client"), "p").unwrap();
        assert_eq!(fs::metadata(dir.join("client")).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&s.path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn compaction_entry_rebuilds_context_and_survives_reload() {
        let dir = temp_dir("cmp");
        let mut s = Session::create(&dir, "p").unwrap();
        s.append_message(Message::user_text("one")).unwrap();
        s.append_message(Message::Assistant(AssistantMessage::empty("m"))).unwrap();
        let kept = s.append_message(Message::user_text("two")).unwrap();
        s.append_message(Message::Assistant(AssistantMessage::empty("m"))).unwrap();
        s.append_compaction("SUM".into(), kept.clone(), Some(100), Some(10)).unwrap();
        s.append_message(Message::user_text("three")).unwrap();
        let ctx = s.active_context();
        assert_eq!(ctx.len(), 4);
        assert_eq!(ctx[0], (None, compaction::summary_message("SUM")));
        assert_eq!(ctx[1].0.as_deref(), Some(kept.as_str()));
        let re = Session::open(&s.path).unwrap();
        assert_eq!(re.active_context(), ctx);
        assert!(re.is_compacted());
        fs::remove_dir_all(dir).unwrap();
    }

    fn has_thinking(m: &Message) -> bool {
        matches!(m, Message::Assistant(a) if a.content.iter().any(|c| matches!(c, Content::Thinking { .. } | Content::RedactedThinking { .. })))
    }

    fn thinking_assistant(n: u32, rest: Vec<Content>) -> Message {
        let mut a = AssistantMessage::empty("m");
        a.content = vec![Content::Thinking { thinking: format!("t{n}"), signature: Some(format!("SIG-{n}")) }, Content::RedactedThinking { data: format!("R-{n}") }];
        a.content.extend(rest);
        if a.content.iter().any(|c| matches!(c, Content::ToolCall(_))) {
            a.stop_reason = StopReason::ToolUse;
        }
        Message::Assistant(a)
    }

    #[test]
    fn compaction_rebuild_strips_thinking_from_kept_messages_only() {
        let dir = temp_dir("think");
        let mut s = Session::create(&dir, "p").unwrap();
        let text = |t: &str| Content::Text { text: t.into() };
        let call = Content::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), arguments: serde_json::json!({}) });
        let tool_result = Message::ToolResult(ToolResultMessage { tool_call_id: "c1".into(), tool_name: "read".into(), content: vec![text("ok")], is_error: false, timestamp: 0 });
        s.append_message(Message::user_text("one")).unwrap();
        s.append_message(thinking_assistant(1, vec![text("r1")])).unwrap();
        let kept = s.append_message(Message::user_text("two")).unwrap();
        s.append_message(thinking_assistant(2, vec![call])).unwrap();
        s.append_message(tool_result.clone()).unwrap();
        // 只有 thinking 的助手消息（中止）：去掉后为空 ⇒ 从上下文中丢弃
        s.append_message(thinking_assistant(3, vec![])).unwrap();
        s.append_message(thinking_assistant(4, vec![text("r4")])).unwrap();
        let before = s.active_messages();
        assert_eq!(before.len(), 7);
        // 在线压缩得到的上下文：摘要 + 去掉 thinking 的保留消息
        let live: Vec<Message> = std::iter::once(compaction::summary_message("SUM")).chain(compaction::strip_kept_thinking(&before[2..])).collect();
        s.append_compaction("SUM".into(), kept.clone(), None, None).unwrap();
        assert_eq!(s.active_messages(), live);
        assert_eq!(live.len(), 5);
        assert!(!live.iter().any(has_thinking));
        let Message::Assistant(a2) = &live[2] else { panic!("assistant") };
        assert_eq!((a2.tool_calls()[0].id.as_str(), a2.content.len()), ("c1", 1));
        assert_eq!(live[3], tool_result);
        // 压缩之后的新消息保留 thinking
        s.append_message(Message::user_text("three")).unwrap();
        let after = thinking_assistant(5, vec![text("r5")]);
        let after_id = s.append_message(after.clone()).unwrap();
        let ctx = s.active_context();
        assert_eq!(ctx.last().unwrap().1, after);
        // 重建（重新打开）与内存结果相同；JSONL 里的原消息不变
        let re = Session::open(&s.path).unwrap();
        assert_eq!(re.active_context(), ctx);
        let stored: Vec<&Message> = re.entries().iter().filter_map(|e| if let Entry::Message { message, .. } = e { Some(message) } else { None }).collect();
        assert_eq!(stored.iter().filter(|m| has_thinking(m)).count(), 5);
        // 第二次压缩保留 after ⇒ 它的 thinking 也被去掉
        let mut re = re;
        re.append_compaction("SUM2".into(), after_id, None, None).unwrap();
        let Message::Assistant(mut expected) = after else { unreachable!() };
        expected.content.retain(|c| matches!(c, Content::Text { .. }));
        assert_eq!(re.active_messages(), vec![compaction::summary_message("SUM2"), Message::Assistant(expected)]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn torn_tail_is_truncated_and_later_appends_are_clean() {
        let dir = temp_dir("torn");
        let mut s = Session::create(&dir, "p").unwrap();
        s.append_message(Message::user_text("ok")).unwrap();
        let mut f = open_append(&s.path).unwrap();
        f.write_all(br#"{"type":"message","id":"x","par"#).unwrap();
        drop(f);
        let mut re = Session::open(&s.path).unwrap();
        assert!(re.torn_tail);
        assert_eq!(re.active_messages().len(), 1);
        re.append_message(Message::user_text("after")).unwrap();
        assert_eq!(Session::open(&s.path).unwrap().active_messages().len(), 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interrupted_tool_call_is_repaired() {
        let dir = temp_dir("repair");
        let mut s = Session::create(&dir, "p").unwrap();
        s.append_message(Message::user_text("run two tools")).unwrap();
        let mut a = AssistantMessage::empty("m");
        a.stop_reason = StopReason::ToolUse;
        for id in ["t1", "t2"] {
            a.content.push(Content::ToolCall(ToolCall { id: id.into(), name: "bash".into(), arguments: serde_json::json!({}) }));
        }
        s.append_message(Message::Assistant(a)).unwrap();
        s.append_message(Message::ToolResult(ToolResultMessage { tool_call_id: "t1".into(), tool_name: "bash".into(), content: vec![], is_error: false, timestamp: 0 })).unwrap();
        let mut re = Session::open(&s.path).unwrap();
        assert_eq!(re.repair_interrupted_tools().unwrap(), ["t2"]);
        let messages = Session::open(&s.path).unwrap().active_messages();
        let Some(Message::ToolResult(r)) = messages.last() else { panic!("tool result") };
        assert!(r.is_error && r.tool_call_id == "t2");
        assert_eq!(r.content, vec![Content::Text { text: INTERRUPTED_TOOL_TEXT.into() }]);
        assert!(Session::open(&s.path).unwrap().repair_interrupted_tools().unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ids_are_validated_strictly() {
        let dir = temp_dir("ids");
        let s = Session::create(&dir, "p").unwrap();
        assert_eq!(find(&dir, &s.id), Some(s.path.clone()));
        for bad in ["", "..", "../x", "a/b", &s.id.to_uppercase(), &format!("{}/../x", s.id), &s.id.replace('-', ""), "*"] {
            assert!(!valid_id(bad), "{bad}");
            assert_eq!(find(&dir, bad), None);
        }
        assert_eq!(list_files(&dir).len(), 1);
        assert!(list_files(&dir.join("missing")).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
}
