//! 上下文压缩的纯函数部分（对应 pi `coding-agent/src/core/compaction/compaction.ts` 与 `utils.ts`）：
//! token 估计、切点选择、给摘要模型的对话序列化与提示词。LLM 调用与持久化在 agentd。
use agent_ai::{Content, Message, StopReason, Usage};
use std::collections::{BTreeSet, HashMap};

/// 摘要作为一条用户消息放在上下文开头（对应 pi `COMPACTION_SUMMARY_PREFIX`）。
pub const SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const SUMMARY_SUFFIX: &str = "\n</summary>";

/// 摘要模型的系统提示（不带工具）。
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI coding assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. Do NOT call tools. ONLY output the structured summary.";

const SUMMARY_FORMAT: &str = "Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Constraints, preferences or requirements mentioned by the user, or \"(none)\"]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Files Touched
- [path] — [read | modified | created]: [what and why]

## Open Questions
- [Unresolved questions, or \"(none)\"]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Data, error messages, commands or references needed to continue, or \"(none)\"]

Keep each section concise. Preserve exact file paths, function names, commands and error messages.";

const INITIAL_INSTRUCTIONS: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.";

const UPDATE_INSTRUCTIONS: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, files and context from the new messages
- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed
- UPDATE \"Next Steps\" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it";

/// 单条工具结果 / thinking 在序列化时的最大字符数。
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;
pub const THINKING_MAX_CHARS: usize = 1000;
/// 发给摘要模型的对话文本上限（字符）；超过时保留开头与结尾。
pub const CONVERSATION_MAX_CHARS: usize = 400_000;
const IMAGE_CHARS: usize = 4800;

pub fn summary_message(summary: &str) -> Message {
    // 固定时间戳：从 JSONL 重建与内存中的结果逐字节相同
    Message::User { content: vec![Content::Text { text: format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}") }], timestamp: 0 }
}

/// 若是压缩摘要消息，返回摘要正文。
pub fn summary_of(message: &Message) -> Option<&str> {
    match message {
        Message::User { content, .. } if content.len() == 1 => match &content[0] {
            Content::Text { text } => text.strip_prefix(SUMMARY_PREFIX)?.strip_suffix(SUMMARY_SUFFIX),
            _ => None,
        },
        _ => None,
    }
}

pub fn estimate_text_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

fn content_chars(content: &[Content]) -> usize {
    content
        .iter()
        .map(|c| match c {
            Content::Text { text } => text.chars().count(),
            Content::Thinking { thinking, .. } => thinking.chars().count(),
            Content::RedactedThinking { data } => data.len(),
            Content::Image { .. } => IMAGE_CHARS,
            Content::ToolCall(t) => t.name.len() + t.arguments.to_string().len(),
        })
        .sum()
}

/// chars/4 启发式（偏保守，对应 pi `estimateTokens`）。
pub fn estimate_tokens(message: &Message) -> u64 {
    let chars = match message {
        Message::User { content, .. } => content_chars(content),
        Message::Assistant(a) => content_chars(&a.content),
        Message::ToolResult(r) => content_chars(&r.content),
    };
    (chars as u64).div_ceil(4)
}

/// 一次调用的上下文大小：优先 total_tokens（OpenAI 的 input 已含缓存，Anthropic 的 total 已加上缓存读写），否则各项相加。
pub fn usage_tokens(u: &Usage) -> u64 {
    if u.total_tokens > 0 {
        u.total_tokens
    } else {
        u.input + u.output + u.cache_read + u.cache_write
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextEstimate {
    pub tokens: u64,
    /// 是否基于 provider 报告的用量（否则为 chars/4 启发式 + overhead）
    pub from_usage: bool,
}

/// 估计当前上下文 token 数：`messages[usage_floor..]` 中最后一条有效用量（非 error/aborted、非 0）的助手消息的用量
/// 加上之后消息的启发式；没有时为 `overhead`（系统提示 + 工具声明）加上全部消息的启发式。
/// `usage_floor` 之前的用量已因压缩失效（它们统计的是压缩前的上下文）。
pub fn estimate_context_tokens(messages: &[Message], usage_floor: usize, overhead: u64) -> ContextEstimate {
    let floor = usage_floor.min(messages.len());
    let last = messages[floor..].iter().enumerate().rev().find_map(|(i, m)| match m {
        Message::Assistant(a) if !matches!(a.stop_reason, StopReason::Error | StopReason::Aborted) && usage_tokens(&a.usage) > 0 => Some((floor + i, usage_tokens(&a.usage))),
        _ => None,
    });
    match last {
        Some((i, used)) => ContextEstimate { tokens: used + messages[i + 1..].iter().map(estimate_tokens).sum::<u64>(), from_usage: true },
        None => ContextEstimate { tokens: overhead + messages.iter().map(estimate_tokens).sum::<u64>(), from_usage: false },
    }
}

/// 切点选择结果：`messages[start..cut]` 被摘要，`messages[cut..]` 原样保留。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPoint {
    /// 被摘要部分的起点（上下文开头已有摘要消息时为 1）
    pub start: usize,
    pub cut: usize,
    /// 切在助手消息上（拆开了一个用户回合，回合开头的用户消息进入摘要）
    pub split_turn: bool,
}

/// 选择切点，使最近约 `keep_recent_tokens` 的消息原样保留：
/// - 只在用户消息或助手消息之前切，绝不切在工具结果上；切点之后的每个工具结果，其工具调用也必须在切点之后
///   （不拆开 tool_use / tool_result 配对）；
/// - 优先用户消息边界（保留完整回合）；预算点之后没有用户边界时才切在助手消息前；
/// - 整个上下文不超过 `keep_recent_tokens` 或没有可切的位置 ⇒ None。
pub fn find_cut_point(messages: &[Message], keep_recent_tokens: u64) -> Option<CutPoint> {
    let start = usize::from(messages.first().is_some_and(|m| summary_of(m).is_some()));
    let n = messages.len();
    if n <= start + 1 {
        return None;
    }
    // 每个工具调用 id → 所在助手消息下标；suffix_min[i] = messages[i..] 中工具结果对应调用的最小下标
    let mut call_at: HashMap<&str, usize> = HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        if let Message::Assistant(a) = m {
            for c in a.tool_calls() {
                call_at.insert(c.id.as_str(), i);
            }
        }
    }
    let mut suffix_min = vec![usize::MAX; n + 1];
    for i in (0..n).rev() {
        let own = match &messages[i] {
            Message::ToolResult(r) => call_at.get(r.tool_call_id.as_str()).copied().unwrap_or(usize::MAX),
            _ => usize::MAX,
        };
        suffix_min[i] = suffix_min[i + 1].min(own);
    }
    let valid = |i: usize| i > start && !matches!(messages[i], Message::ToolResult(_)) && summary_of(&messages[i]).is_none() && suffix_min[i] >= i;
    let is_user = |i: usize| matches!(messages[i], Message::User { .. });
    // 从最新往回累计，直到达到保留预算
    let mut acc = 0u64;
    let mut budget_at = None;
    for i in (start..n).rev() {
        acc += estimate_tokens(&messages[i]);
        if acc >= keep_recent_tokens {
            budget_at = Some(i);
            break;
        }
    }
    let budget_at = budget_at?;
    let cut = (budget_at..n).find(|&i| valid(i) && is_user(i)).or_else(|| (budget_at..n).find(|&i| valid(i))).or_else(|| (start..budget_at).rev().find(|&i| valid(i)))?;
    Some(CutPoint { start, cut, split_turn: !is_user(cut) })
}

fn truncate(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}\n[... {} more characters truncated]", count - max)
}

fn texts(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            Content::Image { .. } => Some("[image]".into()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 把要摘要的消息序列化为纯文本（对应 pi `serializeConversation`）。thinking 只作为截断的纯文本出现，
/// 签名与 redacted thinking 不会发给摘要模型；已有的摘要消息跳过（作为 previous summary 单独传入）。
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts = vec![];
    for m in messages {
        if summary_of(m).is_some() {
            continue;
        }
        match m {
            Message::User { content, .. } => {
                let text = texts(content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(a) => {
                for c in &a.content {
                    match c {
                        Content::Thinking { thinking, .. } if !thinking.trim().is_empty() => parts.push(format!("[Assistant thinking]: {}", truncate(thinking, THINKING_MAX_CHARS))),
                        Content::Text { text } if !text.trim().is_empty() => parts.push(format!("[Assistant]: {text}")),
                        Content::ToolCall(t) => parts.push(format!("[Assistant tool call]: {}({})", t.name, truncate(&t.arguments.to_string(), TOOL_RESULT_MAX_CHARS))),
                        _ => {}
                    }
                }
                if let Some(e) = &a.error_message {
                    parts.push(format!("[Assistant error]: {}", truncate(e, 500)));
                }
            }
            Message::ToolResult(r) => {
                let text = texts(&r.content);
                let tag = if r.is_error { format!("[Tool result {} (error)]", r.tool_name) } else { format!("[Tool result {}]", r.tool_name) };
                parts.push(format!("{tag}: {}", truncate(&text, TOOL_RESULT_MAX_CHARS)));
            }
        }
    }
    let text = parts.join("\n\n");
    let count = text.chars().count();
    if count <= CONVERSATION_MAX_CHARS {
        return text;
    }
    let head: String = text.chars().take(CONVERSATION_MAX_CHARS / 4).collect();
    let tail: String = text.chars().skip(count - CONVERSATION_MAX_CHARS * 3 / 4).collect();
    format!("{head}\n\n[... {} characters of conversation omitted ...]\n\n{tail}", count - CONVERSATION_MAX_CHARS)
}

/// 从工具调用里提取文件路径：(读过的, 修改过的)。
pub fn file_operations(messages: &[Message]) -> (BTreeSet<String>, BTreeSet<String>) {
    let (mut read, mut modified) = (BTreeSet::new(), BTreeSet::new());
    for m in messages {
        if let Message::Assistant(a) = m {
            for c in a.tool_calls() {
                let Some(path) = c.arguments.get("path").and_then(|p| p.as_str()) else { continue };
                match c.name.as_str() {
                    "read" | "ls" | "grep" | "find" => {
                        read.insert(path.to_string());
                    }
                    "write" | "edit" => {
                        modified.insert(path.to_string());
                    }
                    _ => {}
                }
            }
        }
    }
    (read, modified)
}

/// 摘要请求的用户消息：对话 + （可选）上一次摘要 + 格式要求 + （可选）调用方附加说明。
pub fn summary_prompt(messages: &[Message], previous_summary: Option<&str>, instructions: Option<&str>) -> String {
    let mut prompt = format!("<conversation>\n{}\n</conversation>\n\n", serialize_conversation(messages));
    let (read, modified) = file_operations(messages);
    if !read.is_empty() || !modified.is_empty() {
        let list = |s: &BTreeSet<String>| if s.is_empty() { "(none)".to_string() } else { s.iter().cloned().collect::<Vec<_>>().join(", ") };
        prompt.push_str(&format!("<file-operations>\nread: {}\nmodified: {}\n</file-operations>\n\n", list(&read), list(&modified)));
    }
    match previous_summary {
        Some(previous) => {
            prompt.push_str(&format!("<previous-summary>\n{previous}\n</previous-summary>\n\n"));
            prompt.push_str(UPDATE_INSTRUCTIONS);
        }
        None => prompt.push_str(INITIAL_INSTRUCTIONS),
    }
    prompt.push_str("\n\n");
    prompt.push_str(SUMMARY_FORMAT);
    if let Some(extra) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
        prompt.push_str(&format!("\n\nAdditional instructions from the user for this summary:\n{extra}"));
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ai::{AssistantMessage, ToolCall, ToolResultMessage};

    fn user(text: &str) -> Message {
        Message::user_text(text)
    }
    fn assistant(text: &str, calls: &[&str]) -> Message {
        let mut a = AssistantMessage::empty("m");
        if !text.is_empty() {
            a.content.push(Content::Text { text: text.into() });
        }
        for id in calls {
            a.content.push(Content::ToolCall(ToolCall { id: (*id).into(), name: "read".into(), arguments: serde_json::json!({"path": format!("src/{id}.rs")}) }));
        }
        if !calls.is_empty() {
            a.stop_reason = StopReason::ToolUse;
        }
        Message::Assistant(a)
    }
    fn result(id: &str, text: &str) -> Message {
        Message::ToolResult(ToolResultMessage { tool_call_id: id.into(), tool_name: "read".into(), content: vec![Content::Text { text: text.into() }], is_error: false, timestamp: 0 })
    }
    fn big(n: usize) -> String {
        "x".repeat(n * 4)
    }

    #[test]
    fn summary_message_roundtrip() {
        let m = summary_message("S");
        assert_eq!(summary_of(&m), Some("S"));
        assert_eq!(summary_of(&user("S")), None);
    }

    #[test]
    fn cut_prefers_user_boundary_and_keeps_recent() {
        // 每条 100 token
        let messages = vec![user(&big(100)), assistant(&big(100), &[]), user(&big(100)), assistant(&big(100), &[]), user(&big(100)), assistant(&big(100), &[])];
        let cut = find_cut_point(&messages, 150).unwrap();
        assert_eq!(cut, CutPoint { start: 0, cut: 4, split_turn: false });
        // 整个上下文小于预算 ⇒ 不压缩
        assert_eq!(find_cut_point(&messages, 10_000), None);
        // 预算 0（模型调用前上下文以新的用户消息结尾）：只保留这条新消息
        let mut next = messages.clone();
        next.push(user("next"));
        assert_eq!(find_cut_point(&next, 0).unwrap(), CutPoint { start: 0, cut: 6, split_turn: false });
    }

    #[test]
    fn cut_never_splits_tool_call_from_results() {
        // user, assistant(call a,b), result a, result b(big)：模型调用前的典型上下文
        let mut messages = vec![user("start"), assistant("", &["a", "b"]), result("a", "1"), result("b", &big(500))];
        // 预算点落在工具结果上，之后没有合法切点 ⇒ 回退到发出这些调用的助手消息（拆开回合，用户消息进入摘要）
        let cut = find_cut_point(&messages, 300).unwrap();
        assert_eq!(cut, CutPoint { start: 0, cut: 1, split_turn: true });
        messages.push(assistant("done", &["c"]));
        messages.push(result("c", "ok"));
        // 检查每个合法切点之后的工具结果都有调用
        for keep in [0, 1, 5, 200, 400, 600] {
            if let Some(c) = find_cut_point(&messages, keep) {
                let kept = &messages[c.cut..];
                for m in kept {
                    if let Message::ToolResult(r) = m {
                        assert!(kept.iter().any(|k| matches!(k, Message::Assistant(a) if a.tool_calls().iter().any(|t| t.id == r.tool_call_id))), "keep={keep} cut={c:?}");
                    }
                }
                assert!(!matches!(messages[c.cut], Message::ToolResult(_)));
            }
        }
    }

    #[test]
    fn cut_skips_existing_summary_and_needs_something_to_summarize() {
        let messages = vec![summary_message("old"), user(&big(100)), assistant(&big(100), &[]), user(&big(100))];
        let cut = find_cut_point(&messages, 50).unwrap();
        assert_eq!((cut.start, cut.cut), (1, 3));
        // 只有摘要 + 一条消息：没有可摘要的内容
        assert_eq!(find_cut_point(&messages[..2], 0), None);
        assert_eq!(find_cut_point(&[user("only")], 0), None);
    }

    #[test]
    fn estimate_uses_last_valid_usage_after_floor() {
        let mut a = AssistantMessage::empty("m");
        a.usage = Usage { input: 1000, output: 50, cache_read: 200, cache_write: 0, total_tokens: 1250, reasoning: 0 };
        let messages = vec![user("hi"), Message::Assistant(a), user(&big(10))];
        assert_eq!(estimate_context_tokens(&messages, 0, 999), ContextEstimate { tokens: 1260, from_usage: true });
        // floor 之后没有用量 ⇒ 启发式 + overhead
        let est = estimate_context_tokens(&messages, 2, 100);
        assert!(!est.from_usage);
        assert_eq!(est.tokens, 100 + 1 + 10);
        // error 消息的用量不可信
        let mut e = AssistantMessage::failed("m", StopReason::Error, "x");
        e.usage.total_tokens = 5;
        assert!(!estimate_context_tokens(&[Message::Assistant(e)], 0, 0).from_usage);
    }

    #[test]
    fn serialization_strips_signatures_and_truncates() {
        let mut a = AssistantMessage::empty("m");
        a.content = vec![
            Content::Thinking { thinking: "pondering".into(), signature: Some("SIGNATURE-SECRET".into()) },
            Content::RedactedThinking { data: "REDACTED-DATA".into() },
            Content::Text { text: "answer".into() },
        ];
        let text = serialize_conversation(&[summary_message("old"), user("q"), Message::Assistant(a), result("x", &"y".repeat(5000))]);
        assert!(text.contains("[Assistant thinking]: pondering"));
        assert!(!text.contains("SIGNATURE-SECRET") && !text.contains("REDACTED-DATA") && !text.contains("old"));
        assert!(text.contains("more characters truncated"));
        let prompt = summary_prompt(&[user("q"), assistant("", &["f"])], Some("PREVIOUS"), Some("focus on tests"));
        assert!(prompt.contains("<previous-summary>\nPREVIOUS\n</previous-summary>"));
        assert!(prompt.contains("read: src/f.rs"));
        assert!(prompt.contains("## Files Touched") && prompt.contains("## Next Steps") && prompt.contains("focus on tests"));
        assert!(!summary_prompt(&[user("q")], None, None).contains("previous-summary"));
    }
}
