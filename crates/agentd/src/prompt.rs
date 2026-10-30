//! 系统提示构建，对应 pi `coding-agent/src/core/system-prompt.ts::buildSystemPromptSections`：
//! preamble（或自定义）+ 工具列表 + 规则 + 追加段 + 项目上下文文件（AGENTS.md）+ cwd。
use agent_core::AgentTool;
use std::{path::Path, sync::Arc};

const PREAMBLE: &str = "You are an expert coding assistant operating inside a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.";

const CONTEXT_FILES: &[&str] = &["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"];

/// 从 cwd 向上直到 root 收集上下文文件（离 cwd 越近越靠后，优先级越高）。
fn context_files(cwd: &Path, root: &Path) -> Vec<(String, String)> {
    let mut found = vec![];
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if let Some(f) = CONTEXT_FILES.iter().map(|n| d.join(n)).find(|p| p.is_file()) {
            if let Ok(text) = std::fs::read_to_string(&f) {
                found.push((f.to_string_lossy().replace('\\', "/"), text));
            }
        }
        if d == root {
            break;
        }
        dir = d.parent();
    }
    found.reverse();
    found
}

pub struct PromptInput<'a> {
    pub custom: Option<&'a str>,
    pub append: &'a [&'a str],
    pub tools: &'a [Arc<dyn AgentTool>],
    pub cwd: &'a Path,
    pub root: &'a Path,
    pub load_context_files: bool,
}

pub fn build(i: PromptInput) -> String {
    let mut sections = vec![];
    match i.custom {
        Some(c) => sections.push(c.to_string()),
        None => {
            sections.push(PREAMBLE.into());
            let list: Vec<String> = i.tools.iter().map(|t| format!("- {}: {}", t.name(), t.description())).collect();
            sections.push(format!("Available tools:\n{}", if list.is_empty() { "(none)".into() } else { list.join("\n") }));
            sections.push(
                "Guidelines:\n- Read files before editing them; prefer `edit` for targeted changes and `write` only for new files or full rewrites.\n- Keep changes minimal and focused on the task.\n- All paths are relative to the working directory."
                    .into(),
            );
        }
    }
    sections.extend(i.append.iter().filter(|s| !s.is_empty()).map(|s| s.to_string()));
    if i.load_context_files {
        for (path, text) in context_files(i.cwd, i.root) {
            sections.push(format!("<project_context path=\"{path}\">\n{text}\n</project_context>"));
        }
    }
    sections.push(format!("Current working directory: {}", i.cwd.to_string_lossy().replace('\\', "/")));
    sections.join("\n\n")
}
