//! OpenAI Responses API（`POST /v1/responses`，流式 SSE），对应 pi `api/openai-responses.ts`。
//! 用在 gpt-5.4+ / gpt-6.x / o 系列：这些模型在 Chat Completions 上不能同时使用函数工具和推理强度。
//!
//! - 历史消息转成 `input` 条目：用户/助手消息、`function_call`、`function_call_output`；
//! - 系统提示放 `instructions`；`reasoning.effort`；`store: false`（不在 OpenAI 侧保存会话）；
//! - 流事件：`response.output_text.delta`、`response.output_item.added/done`、
//!   `response.function_call_arguments.delta`、`response.completed/incomplete/failed`、`error`。
use crate::{provider::Request, types::*};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

fn user_content(content: &[Content]) -> Value {
    let parts: Vec<Value> = content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(json!({"type": "input_text", "text": text})),
            Content::Image { data, mime_type } => Some(json!({"type": "input_image", "image_url": format!("data:{mime_type};base64,{data}")})),
            _ => None,
        })
        .collect();
    Value::Array(parts)
}

fn tool_output(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            Content::Image { .. } => Some("[image omitted]"),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn build_body(req: &Request) -> Value {
    let mut input = vec![];
    for m in &req.messages {
        match m {
            Message::User { content, .. } => input.push(json!({"role": "user", "content": user_content(content)})),
            Message::Assistant(a) => {
                let text = a.text();
                if !text.is_empty() {
                    input.push(json!({"role": "assistant", "content": text}));
                }
                for t in a.tool_calls() {
                    input.push(json!({"type": "function_call", "call_id": t.id, "name": t.name, "arguments": t.arguments.to_string()}));
                }
            }
            Message::ToolResult(r) => input.push(json!({"type": "function_call_output", "call_id": r.tool_call_id, "output": tool_output(&r.content)})),
        }
    }
    let mut body = json!({"model": req.model, "input": input, "stream": true, "store": false});
    if let Some(sp) = &req.system_prompt {
        body["instructions"] = json!(sp);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters})).collect());
    }
    if let Some(level) = req.reasoning {
        body["reasoning"] = json!({"effort": level.as_str()});
    }
    if let Some(mt) = req.max_tokens {
        body["max_output_tokens"] = json!(mt);
    }
    body
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    args: String,
    content_index: usize,
    done: Option<ToolCall>,
}

/// 流状态机：喂入每个 SSE `data` JSON，产出 `AssistantEvent`，最后 `finish` 得到完整消息。纯函数，便于离线测试。
pub(crate) struct StreamState {
    msg: AssistantMessage,
    text: String,
    thinking: String,
    calls: BTreeMap<u64, Call>,
    status: Option<String>,
    incomplete_reason: Option<String>,
    completed: bool,
}

impl StreamState {
    pub(crate) fn new(model: &str) -> Self {
        Self { msg: AssistantMessage::empty(model), text: String::new(), thinking: String::new(), calls: BTreeMap::new(), status: None, incomplete_reason: None, completed: false }
    }

    fn next_index(&self) -> usize {
        1 + self.calls.len()
    }

    pub(crate) fn on_event(&mut self, v: &Value, out: &mut Vec<AssistantEvent>) -> Result<(), String> {
        let ty = v["type"].as_str().unwrap_or("");
        let oi = v["output_index"].as_u64().unwrap_or(0);
        match ty {
            "response.output_text.delta" => {
                let d = v["delta"].as_str().unwrap_or("");
                if !d.is_empty() {
                    self.text.push_str(d);
                    out.push(AssistantEvent::TextDelta { content_index: 0, delta: d.to_string() });
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let d = v["delta"].as_str().unwrap_or("");
                self.thinking.push_str(d);
                out.push(AssistantEvent::ThinkingDelta { content_index: 0, delta: d.to_string() });
            }
            "response.output_item.added" if v["item"]["type"] == "function_call" => {
                let item = &v["item"];
                let idx = self.next_index();
                let call = Call {
                    id: item["call_id"].as_str().unwrap_or_default().to_string(),
                    name: item["name"].as_str().unwrap_or_default().to_string(),
                    args: item["arguments"].as_str().unwrap_or_default().to_string(),
                    content_index: idx,
                    done: None,
                };
                out.push(AssistantEvent::ToolcallStart { content_index: idx, id: call.id.clone(), tool_name: call.name.clone() });
                self.calls.insert(oi, call);
            }
            "response.function_call_arguments.delta" => {
                if let Some(c) = self.calls.get_mut(&oi) {
                    let d = v["delta"].as_str().unwrap_or("");
                    c.args.push_str(d);
                    out.push(AssistantEvent::ToolcallDelta { content_index: c.content_index, delta: d.to_string() });
                }
            }
            "response.function_call_arguments.done" => {
                if let (Some(c), Some(a)) = (self.calls.get_mut(&oi), v["arguments"].as_str()) {
                    c.args = a.to_string();
                }
            }
            "response.output_item.done" if v["item"]["type"] == "function_call" => {
                let item = &v["item"];
                let idx = self.next_index();
                let c = self.calls.entry(oi).or_insert_with(|| Call { content_index: idx, ..Default::default() });
                if let Some(id) = item["call_id"].as_str() {
                    c.id = id.to_string();
                }
                if let Some(n) = item["name"].as_str() {
                    c.name = n.to_string();
                }
                if let Some(a) = item["arguments"].as_str() {
                    c.args = a.to_string();
                }
                let tc = ToolCall { id: c.id.clone(), name: c.name.clone(), arguments: parse_args(&c.args) };
                out.push(AssistantEvent::ToolcallEnd { content_index: c.content_index, tool_call: tc.clone() });
                c.done = Some(tc);
            }
            "response.completed" | "response.incomplete" => {
                self.completed = true;
                let r = &v["response"];
                self.status = r["status"].as_str().map(str::to_string);
                self.incomplete_reason = r["incomplete_details"]["reason"].as_str().map(str::to_string);
                let u = &r["usage"];
                if u.is_object() {
                    self.msg.usage.input = u["input_tokens"].as_u64().unwrap_or(0);
                    self.msg.usage.output = u["output_tokens"].as_u64().unwrap_or(0);
                    self.msg.usage.cache_read = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                    self.msg.usage.reasoning = u["output_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0);
                    self.msg.usage.total_tokens = u["total_tokens"].as_u64().unwrap_or(self.msg.usage.input + self.msg.usage.output);
                }
            }
            "response.failed" => {
                let e = &v["response"]["error"];
                return Err(format!("OpenAI responses failed: {}", e["message"].as_str().map(str::to_string).unwrap_or_else(|| e.to_string())));
            }
            "error" => {
                let m = v["message"].as_str().or(v["error"]["message"].as_str()).map(str::to_string).unwrap_or_else(|| v.to_string());
                return Err(format!("OpenAI responses stream error: {m}"));
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<AssistantMessage, String> {
        if !self.completed {
            return Err("OpenAI responses stream ended without response.completed".into());
        }
        if !self.thinking.is_empty() {
            self.msg.content.push(Content::Thinking { thinking: std::mem::take(&mut self.thinking) });
        }
        if !self.text.is_empty() {
            self.msg.content.push(Content::Text { text: std::mem::take(&mut self.text) });
        }
        for (_, c) in std::mem::take(&mut self.calls) {
            let tc = c.done.unwrap_or_else(|| ToolCall { id: c.id, name: c.name, arguments: parse_args(&c.args) });
            self.msg.content.push(Content::ToolCall(tc));
        }
        self.msg.stop_reason = if self.status.as_deref() == Some("incomplete") {
            if self.incomplete_reason.as_deref() == Some("max_output_tokens") {
                StopReason::Length
            } else {
                StopReason::Error
            }
        } else if !self.msg.tool_calls().is_empty() {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
        if self.msg.stop_reason == StopReason::Error {
            self.msg.error_message = Some(format!("response incomplete: {}", self.incomplete_reason.as_deref().unwrap_or("unknown")));
        }
        Ok(self.msg)
    }
}

fn parse_args(s: &str) -> Value {
    if s.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
}

pub(crate) async fn parse_stream(resp: reqwest::Response, model: &str, tx: &mpsc::Sender<AssistantEvent>, idle_timeout: std::time::Duration) -> Result<AssistantMessage, String> {
    let _ = tx.send(AssistantEvent::Start).await;
    let mut st = StreamState::new(model);
    let mut buf: Vec<u8> = Vec::new();
    let mut bytes = resp.bytes_stream();
    let mut out = vec![];
    while let Some(chunk) = tokio::time::timeout(idle_timeout, bytes.next()).await.map_err(|_| crate::openai::idle_timeout_error(idle_timeout))? {
        buf.extend_from_slice(&chunk.map_err(|_| "stream body read error".to_string())?);
        for data in crate::openai::drain_sse_lines(&mut buf) {
            if data == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            st.on_event(&v, &mut out).map_err(|e| crate::openai::redact(&e))?;
            for ev in out.drain(..) {
                let _ = tx.send(ev).await;
            }
            if st.completed {
                return st.finish();
            }
        }
    }
    st.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Request;

    fn req() -> Request {
        let call = ToolCall { id: "call_1".into(), name: "read".into(), arguments: json!({"path": "a.rs"}) };
        let mut a = AssistantMessage::empty("gpt-6-astra");
        a.content.push(Content::Text { text: "looking".into() });
        a.content.push(Content::ToolCall(call));
        Request {
            model: "gpt-6-astra".into(),
            system_prompt: Some("be brief".into()),
            messages: vec![
                Message::user_text("hi"),
                Message::Assistant(a),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_1".into(),
                    tool_name: "read".into(),
                    content: vec![Content::Text { text: "fn main(){}".into() }],
                    is_error: false,
                    timestamp: 0,
                }),
            ],
            tools: vec![ToolSpec { name: "read".into(), description: "read a file".into(), parameters: json!({"type": "object"}) }],
            reasoning: Some(ThinkingLevel::High),
            api: Api::Responses,
            max_tokens: Some(1000),
            llm_max_retries: None,
        }
    }

    #[test]
    fn builds_body() {
        let b = build_body(&req());
        assert_eq!(b["instructions"], "be brief");
        assert_eq!(b["reasoning"]["effort"], "high");
        assert_eq!(b["max_output_tokens"], 1000);
        assert_eq!(b["store"], false);
        assert_eq!(b["stream"], true);
        assert_eq!(b["tools"][0], json!({"type": "function", "name": "read", "description": "read a file", "parameters": {"type": "object"}}));
        let input = b["input"].as_array().unwrap();
        assert_eq!(input[0], json!({"role": "user", "content": [{"type": "input_text", "text": "hi"}]}));
        assert_eq!(input[1], json!({"role": "assistant", "content": "looking"}));
        assert_eq!(input[2], json!({"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a.rs\"}"}));
        assert_eq!(input[3], json!({"type": "function_call_output", "call_id": "call_1", "output": "fn main(){}"}));
        let mut r = req();
        r.reasoning = None;
        r.tools.clear();
        r.max_tokens = None;
        let b = build_body(&r);
        assert!(b.get("reasoning").is_none() && b.get("tools").is_none() && b.get("max_output_tokens").is_none());
    }

    fn feed(events: &[Value]) -> (Vec<AssistantEvent>, Result<AssistantMessage, String>) {
        let mut st = StreamState::new("gpt-6-astra");
        let mut out = vec![];
        for e in events {
            if let Err(e) = st.on_event(e, &mut out) {
                return (out, Err(e));
            }
        }
        (out, st.finish())
    }

    #[test]
    fn parses_text_and_tool_calls() {
        let ev = [
            json!({"type": "response.created", "response": {"status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1"}}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "id": "msg_1"}}),
            json!({"type": "response.output_text.delta", "output_index": 1, "delta": "Let me "}),
            json!({"type": "response.output_text.delta", "output_index": 1, "delta": "check."}),
            json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_A", "name": "read", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"path\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "\"x\"}"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 2, "arguments": "{\"path\":\"x\"}"}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": {"type": "function_call", "call_id": "call_A", "name": "read", "arguments": "{\"path\":\"x\"}"}}),
            json!({"type": "response.output_item.added", "output_index": 3, "item": {"type": "function_call", "call_id": "call_B", "name": "ls", "arguments": ""}}),
            json!({"type": "response.output_item.done", "output_index": 3, "item": {"type": "function_call", "call_id": "call_B", "name": "ls", "arguments": ""}}),
            json!({"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 100, "input_tokens_details": {"cached_tokens": 40}, "output_tokens": 60, "output_tokens_details": {"reasoning_tokens": 32}, "total_tokens": 160}}}),
        ];
        let (out, msg) = feed(&ev);
        let msg = msg.unwrap();
        assert_eq!(msg.text(), "Let me check.");
        assert_eq!(msg.stop_reason, StopReason::ToolUse);
        let calls = msg.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[0].id.as_str(), calls[0].name.as_str(), &calls[0].arguments), ("call_A", "read", &json!({"path": "x"})));
        assert_eq!((calls[1].id.as_str(), &calls[1].arguments), ("call_B", &json!({})));
        assert_eq!((msg.usage.input, msg.usage.output, msg.usage.cache_read, msg.usage.reasoning, msg.usage.total_tokens), (100, 60, 40, 32, 160));
        let starts: Vec<usize> = out.iter().filter_map(|e| if let AssistantEvent::ToolcallStart { content_index, .. } = e { Some(*content_index) } else { None }).collect();
        assert_eq!(starts, vec![1, 2]);
        assert_eq!(out.iter().filter(|e| matches!(e, AssistantEvent::TextDelta { .. })).count(), 2);
    }

    #[test]
    fn handles_incomplete_and_errors() {
        let (_, msg) = feed(&[
            json!({"type": "response.output_text.delta", "delta": "partial"}),
            json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 5, "output_tokens": 7}}}),
        ]);
        let msg = msg.unwrap();
        assert_eq!(msg.stop_reason, StopReason::Length);
        assert_eq!(msg.usage.total_tokens, 12);
        let (_, r) = feed(&[json!({"type": "response.failed", "response": {"status": "failed", "error": {"code": "server_error", "message": "boom"}}})]);
        assert!(r.unwrap_err().contains("boom"));
        let (_, r) = feed(&[json!({"type": "error", "code": "invalid", "message": "bad thing"})]);
        assert!(r.unwrap_err().contains("bad thing"));
        let (_, r) = feed(&[json!({"type": "response.output_text.delta", "delta": "cut"})]);
        assert!(r.unwrap_err().contains("without response.completed"));
    }

    #[test]
    fn sse_lines_handle_split_utf8() {
        let mut buf = "event: response.output_text.delta\ndata: {\"delta\":\"你".as_bytes().to_vec();
        assert!(crate::openai::drain_sse_lines(&mut buf).is_empty());
        buf.extend_from_slice("好\"}\r\n\n".as_bytes());
        assert_eq!(crate::openai::drain_sse_lines(&mut buf), vec!["{\"delta\":\"你好\"}".to_string()]);
    }
}
