use crate::{provider::*, types::*};
use tokio_util::sync::CancellationToken;

/// 离线假 provider（对应 pi `providers/faux.ts`），不访问网络，用于测试与演示：
/// - 用户消息形如 `/tool <name> <json-args>` 时，返回一次该工具调用；
/// - 最后一条是工具结果时，回显工具结果；
/// - 否则回显用户消息。
pub struct FauxProvider;

fn first_text(content: &[Content]) -> String {
    content.iter().find_map(|c| if let Content::Text { text } = c { Some(text.clone()) } else { None }).unwrap_or_default()
}

impl Provider for FauxProvider {
    fn id(&self) -> &str {
        "faux"
    }
    fn list_models(&self) -> futures::future::BoxFuture<'_, Result<Vec<crate::models::ModelInfo>, String>> {
        Box::pin(async { Ok(vec![crate::models::info_for("faux".into(), Some("sub-bot".into()), None)]) })
    }
    fn stream(&self, req: Request, _cancel: CancellationToken) -> EventStream {
        let mut msg = AssistantMessage::empty(&req.model);
        let mut events = vec![AssistantEvent::Start];
        match req.messages.last() {
            Some(Message::ToolResult(r)) => {
                let text = format!("tool {} returned: {}", r.tool_name, first_text(&r.content));
                events.push(AssistantEvent::TextDelta { content_index: 0, delta: text.clone() });
                msg.content.push(Content::Text { text });
            }
            Some(Message::User { content, .. }) if first_text(content).starts_with("/tool ") => {
                let t = first_text(content);
                let mut parts = t["/tool ".len()..].splitn(2, ' ');
                let name = parts.next().unwrap_or_default().to_string();
                let arguments = parts.next().and_then(|a| serde_json::from_str(a).ok()).unwrap_or_else(|| serde_json::json!({}));
                let tool_call = ToolCall { id: format!("faux-{}", now_ms()), name, arguments };
                events.push(AssistantEvent::ToolcallEnd { content_index: 0, tool_call: tool_call.clone() });
                msg.content.push(Content::ToolCall(tool_call));
                msg.stop_reason = StopReason::ToolUse;
            }
            other => {
                let last = match other { Some(Message::User { content, .. }) => first_text(content), _ => String::new() };
                let text = format!("echo: {last}");
                events.push(AssistantEvent::TextDelta { content_index: 0, delta: text.clone() });
                msg.content.push(Content::Text { text });
            }
        }
        events.push(AssistantEvent::Done { message: msg });
        Box::pin(futures::stream::iter(events))
    }
}
