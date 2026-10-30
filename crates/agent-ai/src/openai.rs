//! OpenAI Chat Completions 流式实现（对应 pi `packages/ai/src/api/openai-completions.ts`）。
//! 后续可加 Responses API（对应 `api/openai-responses.ts`）以支持 reasoning 摘要等。
use crate::{provider::*, types::*};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

pub struct OpenAiProvider {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl OpenAiProvider {
    /// key 只在进程内存中，从不通过对外接口返回。
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self { api_key, base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()), http: reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(20)).build().expect("http client") }
    }
}

fn content_to_openai(content: &[Content]) -> Value {
    let parts: Vec<Value> = content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(json!({"type": "text", "text": text})),
            Content::Image { data, mime_type } => Some(json!({"type": "image_url", "image_url": {"url": format!("data:{mime_type};base64,{data}")}})),
            _ => None,
        })
        .collect();
    Value::Array(parts)
}

pub fn build_body(req: &Request) -> Value {
    let mut messages = vec![];
    if let Some(sp) = &req.system_prompt {
        messages.push(json!({"role": "system", "content": sp}));
    }
    for m in &req.messages {
        match m {
            Message::User { content, .. } => messages.push(json!({"role": "user", "content": content_to_openai(content)})),
            Message::Assistant(a) => {
                let calls: Vec<Value> = a.tool_calls().iter().map(|t| json!({"id": t.id, "type": "function", "function": {"name": t.name, "arguments": t.arguments.to_string()}})).collect();
                let mut v = json!({"role": "assistant", "content": a.text()});
                if !calls.is_empty() {
                    v["tool_calls"] = Value::Array(calls);
                }
                messages.push(v);
            }
            Message::ToolResult(r) => {
                let text: String = r.content.iter().filter_map(|c| if let Content::Text { text } = c { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\n");
                messages.push(json!({"role": "tool", "tool_call_id": r.tool_call_id, "content": text}));
            }
        }
    }
    let mut body = json!({"model": req.model, "messages": messages, "stream": true, "stream_options": {"include_usage": true}});
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}})).collect());
    }
    if let Some(n) = req.max_tokens {
        body["max_completion_tokens"] = json!(n);
    }
    if let Some(level) = req.reasoning {
        if level != ThinkingLevel::Off {
            body["reasoning_effort"] = serde_json::to_value(level).unwrap();
        }
    }
    body
}

impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    fn stream(&self, req: Request, cancel: CancellationToken) -> EventStream {
        let (tx, rx) = mpsc::channel(64);
        let http = self.http.clone();
        let url = format!("{}/chat/completions", self.base_url);
        let key = self.api_key.clone();
        let span = tracing::info_span!("llm", provider = "openai", model = %req.model);
        tokio::spawn(tracing::Instrument::instrument(async move {
            let model = req.model.clone();
            let started = std::time::Instant::now();
            tracing::debug!(messages = req.messages.len(), tools = req.tools.len(), "request sent");
            let fut = run(http, url, key, req, tx.clone());
            let final_ev = tokio::select! {
                r = fut => match r {
                    Ok(msg) => AssistantEvent::Done { message: msg },
                    Err(e) => AssistantEvent::Error { message: AssistantMessage::failed(&model, StopReason::Error, e) },
                },
                _ = cancel.cancelled() => AssistantEvent::Error { message: AssistantMessage::failed(&model, StopReason::Aborted, "aborted") },
            };
            let latency_ms = started.elapsed().as_millis() as u64;
            match &final_ev {
                AssistantEvent::Done { message: m } => tracing::info!(latency_ms, stop_reason = ?m.stop_reason, input_tokens = m.usage.input, output_tokens = m.usage.output, cached_tokens = m.usage.cache_read, tool_calls = m.tool_calls().len(), "llm call finished"),
                AssistantEvent::Error { message: m } if m.stop_reason == StopReason::Aborted => tracing::info!(latency_ms, "llm call aborted"),
                AssistantEvent::Error { message: m } => tracing::error!(latency_ms, error = m.error_message.as_deref().unwrap_or(""), "llm call failed"),
                _ => {}
            }
            let _ = tx.send(final_ev).await;
        }, span));
        Box::pin(ReceiverStream::new(rx))
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

async fn run(http: reqwest::Client, url: String, key: String, req: Request, tx: mpsc::Sender<AssistantEvent>) -> Result<AssistantMessage, String> {
    let resp = http.post(&url).bearer_auth(&key).json(&build_body(&req)).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(redact(&format!("OpenAI HTTP {status}: {body}")));
    }
    let _ = tx.send(AssistantEvent::Start).await;
    let mut msg = AssistantMessage::empty(&req.model);
    let mut text = String::new();
    let mut calls: BTreeMap<u64, PartialCall> = BTreeMap::new();
    let mut finish: Option<String> = None;
    let mut buf: Vec<u8> = Vec::new();
    let mut bytes = resp.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        buf.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
        // 按字节切分 LF，避免多字节 UTF-8 字符跨 chunk 被截断
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line = String::from_utf8_lossy(&buf[..pos]).trim_end_matches('\r').to_string();
            buf.drain(..=pos);
            let Some(data) = line.strip_prefix("data: ") else { continue };
            if data == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
                return Err(redact(&format!("OpenAI stream error: {e}")));
            }
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                msg.usage.cache_read = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                msg.usage.input = u["prompt_tokens"].as_u64().unwrap_or(0);
                msg.usage.output = u["completion_tokens"].as_u64().unwrap_or(0);
                msg.usage.total_tokens = u["total_tokens"].as_u64().unwrap_or(0);
            }
            let Some(choice) = v["choices"].get(0) else { continue };
            if let Some(f) = choice["finish_reason"].as_str() {
                finish = Some(f.to_string());
            }
            let delta = &choice["delta"];
            if let Some(t) = delta["content"].as_str() {
                text.push_str(t);
                let _ = tx.send(AssistantEvent::TextDelta { content_index: 0, delta: t.to_string() }).await;
            }
            if let Some(tcs) = delta["tool_calls"].as_array() {
                for tc in tcs {
                    let idx = tc["index"].as_u64().unwrap_or(0);
                    let entry = calls.entry(idx).or_default();
                    let is_new = entry.id.is_empty() && entry.name.is_empty();
                    if let Some(id) = tc["id"].as_str() {
                        entry.id = id.to_string();
                    }
                    if let Some(n) = tc["function"]["name"].as_str() {
                        entry.name.push_str(n);
                    }
                    // 每个工具调用只发一次 toolcall_start（名称可能分片到达）
                    if is_new {
                        let _ = tx.send(AssistantEvent::ToolcallStart { content_index: idx as usize + 1, id: entry.id.clone(), tool_name: entry.name.clone() }).await;
                    }
                    if let Some(a) = tc["function"]["arguments"].as_str() {
                        entry.args.push_str(a);
                        let _ = tx.send(AssistantEvent::ToolcallDelta { content_index: idx as usize + 1, delta: a.to_string() }).await;
                    }
                }
            }
        }
    }
    if finish.is_none() {
        return Err("OpenAI stream ended without finish_reason (connection dropped?)".into());
    }
    if !text.is_empty() {
        msg.content.push(Content::Text { text });
    }
    for (idx, c) in calls {
        // 参数不是合法 JSON 时保留原文，让工具校验报错并反馈给模型，而不是静默变成 {}
        let arguments = if c.args.trim().is_empty() { json!({}) } else { serde_json::from_str(&c.args).unwrap_or_else(|_| json!({ "_invalidJson": c.args })) };
        let tool_call = ToolCall { id: c.id, name: c.name, arguments };
        let _ = tx.send(AssistantEvent::ToolcallEnd { content_index: idx as usize + 1, tool_call: tool_call.clone() }).await;
        msg.content.push(Content::ToolCall(tool_call));
    }
    msg.stop_reason = match finish.as_deref() {
        Some("length") => StopReason::Length,
        Some("tool_calls") => StopReason::ToolUse,
        _ if !msg.tool_calls().is_empty() => StopReason::ToolUse,
        _ => StopReason::Stop,
    };
    Ok(msg)
}

/// 去掉错误信息里任何形似 API key 的片段（OpenAI 401 会回显部分 key）。
fn redact(s: &str) -> String {
    s.split_inclusive(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
        .map(|w| if w.trim_start_matches(|c: char| !c.is_alphanumeric()).starts_with("sk-") { "[redacted]".to_string() + &w[w.trim_end_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',').len()..] } else { w.to_string() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redacts_keys() {
        let r = redact(r#"{"message":"Incorrect API key provided: sk-proj-abc***xyz. See"}"#);
        assert!(!r.contains("sk-"), "{r}");
        assert!(r.contains("[redacted]"));
    }
}
