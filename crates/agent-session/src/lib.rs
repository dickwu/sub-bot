//! JSONL 会话树，对应 pi `coding-agent/src/core/session-manager.ts` 与 `docs/session-format.md`。
//! 第一行是 header，之后每行一个 entry，通过 id/parentId 组成树；当前叶子决定活动分支。
use agent_ai::Message;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

pub const SESSION_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Entry {
    Session { version: u32, id: String, profile: String, timestamp: String },
    Message { id: String, parent_id: Option<String>, timestamp: String, message: Message },
    ModelChange { id: String, parent_id: Option<String>, timestamp: String, provider: String, model_id: String },
    /// 压缩摘要：替换 first_kept_entry_id 之前的上下文（对应 compaction entry）
    Compaction { id: String, parent_id: Option<String>, timestamp: String, summary: String, first_kept_entry_id: String },
}

impl Entry {
    fn id(&self) -> &str {
        match self {
            Entry::Session { id, .. } | Entry::Message { id, .. } | Entry::ModelChange { id, .. } | Entry::Compaction { id, .. } => id,
        }
    }
    fn parent(&self) -> Option<&str> {
        match self {
            Entry::Session { .. } => None,
            Entry::Message { parent_id, .. } | Entry::ModelChange { parent_id, .. } | Entry::Compaction { parent_id, .. } => parent_id.as_deref(),
        }
    }
}

pub struct Session {
    pub id: String,
    pub path: PathBuf,
    entries: Vec<Entry>,
    leaf: Option<String>,
}

fn ts() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

impl Session {
    pub fn create(dir: &Path, profile: &str) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let id = uuid::Uuid::new_v4().to_string();
        let path = dir.join(format!("{}_{}.jsonl", chrono::Utc::now().format("%Y%m%dT%H%M%S"), id));
        let mut s = Self { id: id.clone(), path, entries: vec![], leaf: None };
        s.append_raw(Entry::Session { version: SESSION_VERSION, id, profile: profile.into(), timestamp: ts() })?;
        Ok(s)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let mut entries = vec![];
        for line in BufReader::new(fs::File::open(path)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            entries.push(serde_json::from_str::<Entry>(&line)?);
        }
        let id = match entries.first() {
            Some(Entry::Session { id, .. }) => id.clone(),
            _ => anyhow::bail!("missing session header"),
        };
        let leaf = entries.iter().rev().find(|e| !matches!(e, Entry::Session { .. })).map(|e| e.id().to_string());
        Ok(Self { id, path: path.into(), entries, leaf })
    }

    fn append_raw(&mut self, e: Entry) -> Result<()> {
        let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(&e)?)?;
        if !matches!(e, Entry::Session { .. }) {
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

    /// 从某个历史 entry 继续：之后的 append 会形成新分支（对应 branch / fork）。
    pub fn set_leaf(&mut self, entry_id: &str) {
        self.leaf = Some(entry_id.to_string());
    }

    /// 从叶子回溯到根，得到活动分支上的消息（对应 buildSessionContext）。
    pub fn active_messages(&self) -> Vec<Message> {
        let by_id: HashMap<&str, &Entry> = self.entries.iter().map(|e| (e.id(), e)).collect();
        let mut chain = vec![];
        let mut cur = self.leaf.as_deref();
        while let Some(id) = cur {
            let Some(e) = by_id.get(id) else { break };
            chain.push(*e);
            cur = e.parent();
        }
        chain.reverse();
        chain.into_iter().filter_map(|e| if let Entry::Message { message, .. } = e { Some(message.clone()) } else { None }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_branch() {
        let dir = std::env::temp_dir().join(format!("sess-{}", short_id()));
        let mut s = Session::create(&dir, "/tmp").unwrap();
        let a = s.append_message(Message::user_text("a")).unwrap();
        s.append_message(Message::user_text("b")).unwrap();
        s.set_leaf(&a);
        s.append_message(Message::user_text("c")).unwrap();
        assert_eq!(s.active_messages().len(), 2);
        let re = Session::open(&s.path).unwrap();
        assert_eq!(re.active_messages(), s.active_messages());
    }
}
