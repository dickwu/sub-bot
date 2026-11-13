//! Anthropic Messages API, including prompt caching and signed thinking replay.
#[cfg(test)]
mod tests;
use crate::{
    openai::{drain_sse_lines, idle_timeout_error, redact},
    provider::*,
    retry::{retry_after, run_with_retries, CallError},
    types::*,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct AnthropicProvider {
    api_key: crate::secrets::Secret,
    base_url: String,
    http: Result<reqwest::Client, String>,
    stream_idle_timeout: Duration,
}

impl AnthropicProvider {
    pub fn new(api_key: crate::secrets::Secret, base_url: Option<String>) -> Self {
        Self::with_stream_idle_timeout(api_key, base_url, Duration::from_secs(300))
    }

    pub fn with_stream_idle_timeout(api_key: crate::secrets::Secret, base_url: Option<String>, stream_idle_timeout: Duration) -> Self {
        let base = base_url.unwrap_or_else(|| "https://api.anthropic.com".into());
        let base = base.trim_end_matches('/');
        Self {
            api_key,
            base_url: format!("{}/v1", base.strip_suffix("/v1").unwrap_or(base)),
            http: crate::http::client_builder().connect_timeout(Duration::from_secs(30)).build().map_err(|_| "cannot initialize Anthropic HTTP client".into()),
            stream_idle_timeout,
        }
    }

    fn safe(&self, text: &str) -> String {
        let text = if self.api_key.expose().is_empty() { text.to_string() } else { text.replace(self.api_key.expose(), "[redacted]") };
        redact(&text)
    }

    fn request(&self, method: reqwest::Method, endpoint: &str) -> Result<reqwest::RequestBuilder, String> {
        let http = self.http.as_ref().map_err(Clone::clone)?;
        let mut key = reqwest::header::HeaderValue::from_str(self.api_key.expose()).map_err(|_| "invalid Anthropic API key header".to_string())?;
        key.set_sensitive(true);
        Ok(http.request(method, format!("{}/{endpoint}", self.base_url)).header("x-api-key", key).header("anthropic-version", "2023-06-01").header("content-type", "application/json"))
    }

    fn log_request_id(&self, response: &reqwest::Response) {
        if let Some(id) = response.headers().get("request-id").and_then(|v| v.to_str().ok()) {
            tracing::debug!(request_id = %self.safe(id), status = response.status().as_u16(), "Anthropic response received");
        }
    }

    async fn run(&self, req: &mut Request, tx: mpsc::Sender<AssistantEvent>) -> Result<AssistantMessage, CallError> {
        let result = self.run_inner(req, tx).await;
        result.map_err(|mut error| {
            error.reason = self.safe(&error.reason);
            error
        })
    }

    async fn run_inner(&self, req: &mut Request, tx: mpsc::Sender<AssistantEvent>) -> Result<AssistantMessage, CallError> {
        let response = loop {
            let body = build_body(req);
            let response = tokio::time::timeout(Duration::from_secs(120), self.request(reqwest::Method::POST, "messages")?.json(&body).send())
                .await
                .map_err(|_| "response headers timeout after 120s".to_string())?
                .map_err(CallError::transport)?;
            self.log_request_id(&response);
            if response.status().is_success() {
                break response;
            }
            let status = response.status();
            let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(retry_after);
            let text = tokio::time::timeout(self.stream_idle_timeout, response.text()).await.ok().and_then(Result::ok).unwrap_or_default();
            let lower = text.to_ascii_lowercase();
            if status == reqwest::StatusCode::BAD_REQUEST
                && !req.raw_effort
                && req.reasoning.is_some()
                && lower.contains("thinking")
                && (lower.contains("not supported") || lower.contains("does not support") || lower.contains("unsupported"))
            {
                tracing::warn!(issue = "typed", "model does not support thinking; retrying without it");
                req.reasoning = None; // At most one adjustment, also retained across transient retries.
                continue;
            }
            return Err(CallError { reason: format!("Anthropic HTTP {status}: {text}"), transient: matches!(status.as_u16(), 408 | 429 | 500..=599), retry_after });
        };
        let mut message = parse_stream(response, &req.model, &tx, self).await?;
        message.reasoning_effort = req.reasoning;
        Ok(message)
    }
}

impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn stream(&self, mut req: Request, cancel: CancellationToken) -> EventStream {
        if !req.raw_effort {
            req.reasoning = crate::models::resolve_effort_quiet("anthropic", &req.model, req.reasoning).effective;
        }
        let (tx, rx) = mpsc::channel(64);
        let provider = self.clone();
        let span = tracing::info_span!("llm", provider = "anthropic", model = %req.model);
        tokio::spawn(tracing::Instrument::instrument(
            async move {
                let started = std::time::Instant::now();
                let model = req.model.clone();
                let max_retries = if req.raw_effort { 0 } else { req.llm_max_retries.unwrap_or(3) };
                tracing::debug!(messages = req.messages.len(), tools = req.tools.len(), "request sent");
                let mut state = (provider, req);
                let result = tokio::select! {
                    result = run_with_retries(&mut state, max_retries, &tx, |state, tx| Box::pin(state.0.run(&mut state.1, tx))) => Some(result),
                    _ = cancel.cancelled() => None,
                    _ = tx.closed() => return,
                };
                let event = match result {
                    Some(Ok(message)) => AssistantEvent::Done { message },
                    Some(Err(error)) => {
                        let mut message = AssistantMessage::failed(&model, StopReason::Error, error);
                        message.reasoning_effort = state.1.reasoning;
                        AssistantEvent::Error { message }
                    }
                    None => AssistantEvent::Error { message: AssistantMessage::failed(&model, StopReason::Aborted, "aborted") },
                };
                let latency_ms = started.elapsed().as_millis() as u64;
                match &event {
                    AssistantEvent::Done { message } => tracing::info!(latency_ms, stop_reason = ?message.stop_reason, input_tokens = message.usage.input, output_tokens = message.usage.output,
                    cached_tokens = message.usage.cache_read, cache_write_tokens = message.usage.cache_write, tool_calls = message.tool_calls().len(), "llm call finished"),
                    AssistantEvent::Error { message } if message.stop_reason == StopReason::Aborted => tracing::info!(latency_ms, "llm call aborted"),
                    AssistantEvent::Error { message } => tracing::warn!(issue = "typed", latency_ms, error = message.error_message.as_deref().unwrap_or(""), "llm call failed"),
                    _ => {}
                }
                let _ = tx.send(event).await;
            },
            span,
        ));
        Box::pin(ReceiverStream::new(rx))
    }

    fn list_models(&self) -> futures::future::BoxFuture<'_, Result<Vec<crate::models::ModelInfo>, String>> {
        Box::pin(tracing::Instrument::instrument(
            async move {
                let started = std::time::Instant::now();
                let mut out = vec![];
                let mut after: Option<String> = None;
                let mut seen = HashSet::new();
                loop {
                    let mut request = self.request(reqwest::Method::GET, "models")?.query(&[("limit", "1000")]);
                    if let Some(after) = &after {
                        request = request.query(&[("after_id", after)]);
                    }
                    let response = request.timeout(Duration::from_secs(30)).send().await.map_err(|e| self.safe(&e.to_string()))?;
                    self.log_request_id(&response);
                    let status = response.status();
                    let text = response.text().await.map_err(|e| self.safe(&e.to_string()))?;
                    if !status.is_success() {
                        return Err(self.safe(&format!("Anthropic HTTP {status}: {text}")));
                    }
                    let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                    let data = value["data"].as_array().ok_or("Anthropic models response missing data")?;
                    for model in data {
                        if let Some(id) = model["id"].as_str() {
                            let created = model["created_at"].as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.timestamp());
                            out.push(crate::models::info_for(id.to_string(), Some("anthropic".into()), created));
                        }
                    }
                    if value["has_more"] != true {
                        break;
                    }
                    let last = value["last_id"].as_str().filter(|id| !id.is_empty()).ok_or("Anthropic models response missing last_id")?;
                    if !seen.insert(last.to_string()) {
                        return Err("Anthropic models pagination did not advance".into());
                    }
                    after = Some(last.to_string());
                }
                out.sort_by(|a, b| a.id.cmp(&b.id));
                out.dedup_by(|a, b| a.id == b.id);
                tracing::info!(count = out.len(), latency_ms = started.elapsed().as_millis() as u64, "listed models");
                Ok(out)
            },
            tracing::info_span!("llm", provider = "anthropic", model = "catalogue"),
        ))
    }
}

fn thinking_budget(level: Option<ThinkingLevel>) -> Option<u32> {
    match level {
        Some(ThinkingLevel::Low) => Some(8192),
        Some(ThinkingLevel::Medium) => Some(16384),
        Some(ThinkingLevel::High) => Some(32000),
        _ => None,
    }
}

fn content_blocks(content: &[Content], assistant: bool) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(json!({"type": "text", "text": text})),
            Content::Image { data, mime_type } if !assistant => Some(json!({"type":"image", "source":{"type":"base64", "media_type":mime_type, "data":data}})),
            Content::Thinking { thinking, signature: Some(signature) } if assistant => Some(json!({"type":"thinking", "thinking":thinking, "signature":signature})),
            Content::RedactedThinking { data } if assistant => Some(json!({"type":"redacted_thinking", "data":data})),
            Content::ToolCall(call) if assistant => Some(json!({"type":"tool_use", "id":call.id, "name":call.name, "input":call.arguments})),
            _ => None,
        })
        .collect()
}

pub fn build_body(req: &Request) -> Value {
    let mut messages: Vec<Value> = vec![];
    for message in &req.messages {
        let (role, content) = match message {
            Message::User { content, .. } => ("user", content_blocks(content, false)),
            Message::Assistant(message) => ("assistant", content_blocks(&message.content, true)),
            Message::ToolResult(result) => {
                ("user", vec![json!({"type":"tool_result", "tool_use_id":result.tool_call_id, "content":content_blocks(&result.content, false), "is_error":result.is_error})])
            }
        };
        if content.is_empty() {
            continue;
        }
        if let Some(last) = messages.last_mut().filter(|last| last["role"] == role) {
            if let Some(blocks) = last["content"].as_array_mut() {
                blocks.extend(content);
            }
        } else {
            messages.push(json!({"role":role, "content":content}));
        }
    }
    // Only three breakpoints: system, last tool, last user block (never signed thinking).
    if let Some(last) = messages.iter_mut().rev().find(|m| m["role"] == "user").and_then(|m| m["content"].as_array_mut()).and_then(|blocks| blocks.last_mut()) {
        last["cache_control"] = json!({"type":"ephemeral"});
    }
    use crate::models::{effort_caps, resolve_effort_quiet, EffortMechanism::*};
    let caps = effort_caps("anthropic", &req.model);
    let level = if req.raw_effort { req.reasoning } else { resolve_effort_quiet("anthropic", &req.model, req.reasoning).effective };
    let budget = if matches!(caps.mechanism, AnthropicEffortWithBudget | AnthropicBudgetOnly) && !req.raw_effort { thinking_budget(level) } else { None };
    let default_tokens = match level {
        Some(ThinkingLevel::High) => 32000,
        Some(ThinkingLevel::Xhigh) => 64000,
        Some(ThinkingLevel::Max) => caps.max_output.unwrap_or(64000),
        Some(_) => 16000,
        // 未设 effort：沿用 v0.7 默认，避免老模型（claude-3.x 输出上限 8192）被拒。
        None if req.model.split('-').find_map(|part| part.parse::<u32>().ok()).is_some_and(|n| (4..100).contains(&n)) => 16000,
        None => 8192,
    };
    let mut max_tokens = req.max_tokens.unwrap_or(default_tokens).max(1);
    if let Some(budget) = budget {
        max_tokens = max_tokens.max(budget + 4096);
    }
    max_tokens = max_tokens.min(caps.max_output.unwrap_or(64000));
    let mut body = json!({"model":req.model, "messages":messages, "stream":true, "max_tokens":max_tokens});
    if let Some(system) = &req.system_prompt {
        body["system"] = json!([{"type":"text", "text":system, "cache_control":{"type":"ephemeral"}}]);
    }
    if !req.tools.is_empty() {
        let mut tools: Vec<Value> = req.tools.iter().map(|t| json!({"name":t.name, "description":t.description, "input_schema":t.parameters})).collect();
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = json!({"type":"ephemeral"});
        }
        body["tools"] = json!(tools);
    }
    if let Some(level) = level {
        if req.raw_effort {
            body["output_config"] = json!({"effort":level});
            match req.thinking.unwrap_or(ProbeThinking::Adaptive) {
                ProbeThinking::Adaptive => body["thinking"] = json!({"type":"adaptive"}),
                ProbeThinking::Enabled => body["thinking"] = json!({"type":"enabled", "budget_tokens":1024}),
                ProbeThinking::Omit => {}
            }
        } else {
            if let Some(budget) = budget {
                body["thinking"] = json!({"type":"enabled", "budget_tokens":budget});
            } else {
                body["thinking"] = json!({"type":"adaptive"});
            }
            if caps.mechanism != AnthropicBudgetOnly {
                body["output_config"] = json!({"effort":level});
            }
        }
    }
    body
}

struct Block {
    content: Content,
    arguments: String,
    stopped: bool,
}

struct StreamState {
    message: AssistantMessage,
    blocks: BTreeMap<usize, Block>,
    completed: bool,
}

impl StreamState {
    fn new(model: &str) -> Self {
        Self { message: AssistantMessage::empty(model), blocks: BTreeMap::new(), completed: false }
    }

    fn usage(&mut self, usage: &Value) {
        let u = &mut self.message.usage;
        if let Some(n) = usage["input_tokens"].as_u64() {
            u.input = n;
        }
        if let Some(n) = usage["output_tokens"].as_u64() {
            u.output = n;
        }
        if let Some(n) = usage["cache_read_input_tokens"].as_u64() {
            u.cache_read = n;
        }
        if let Some(n) = usage["cache_creation_input_tokens"].as_u64() {
            u.cache_write = n;
        }
        // Anthropic's input_tokens excludes cache reads/writes, unlike OpenAI's prompt_tokens.
        u.total_tokens = u.input.saturating_add(u.output).saturating_add(u.cache_read).saturating_add(u.cache_write);
    }

    fn on_event(&mut self, value: &Value, out: &mut Vec<AssistantEvent>) -> Result<(), CallError> {
        let index = value["index"].as_u64().unwrap_or(0) as usize;
        match value["type"].as_str().unwrap_or("") {
            "message_start" => {
                self.usage(&value["message"]["usage"]);
            }
            "content_block_start" => {
                if self.blocks.contains_key(&index) {
                    return Err("Anthropic duplicate content block".to_string().into());
                }
                let block = &value["content_block"];
                let content = match block["type"].as_str().unwrap_or("") {
                    "text" => {
                        let text = block["text"].as_str().unwrap_or_default().to_string();
                        if !text.is_empty() {
                            out.push(AssistantEvent::TextDelta { content_index: index, delta: text.clone() });
                        }
                        Content::Text { text }
                    }
                    "thinking" => {
                        let thinking = block["thinking"].as_str().unwrap_or_default().to_string();
                        if !thinking.is_empty() {
                            out.push(AssistantEvent::ThinkingDelta { content_index: index, delta: thinking.clone() });
                        }
                        Content::Thinking { thinking, signature: block["signature"].as_str().map(str::to_string) }
                    }
                    "redacted_thinking" => Content::RedactedThinking { data: block["data"].as_str().unwrap_or_default().into() },
                    "tool_use" => {
                        let call = ToolCall {
                            id: block["id"].as_str().unwrap_or_default().into(),
                            name: block["name"].as_str().unwrap_or_default().into(),
                            arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                        };
                        out.push(AssistantEvent::ToolcallStart { content_index: index, id: call.id.clone(), tool_name: call.name.clone() });
                        Content::ToolCall(call)
                    }
                    _ => return Ok(()),
                };
                self.blocks.insert(index, Block { content, arguments: String::new(), stopped: false });
            }
            "content_block_delta" => {
                let Some(block) = self.blocks.get_mut(&index) else {
                    return Err("Anthropic delta without content block".to_string().into());
                };
                if block.stopped {
                    return Err("Anthropic delta after content_block_stop".to_string().into());
                }
                let delta = &value["delta"];
                match (&mut block.content, delta["type"].as_str().unwrap_or("")) {
                    (Content::Text { text }, "text_delta") => {
                        let d = delta["text"].as_str().unwrap_or_default();
                        text.push_str(d);
                        out.push(AssistantEvent::TextDelta { content_index: index, delta: d.into() });
                    }
                    (Content::Thinking { thinking, .. }, "thinking_delta") => {
                        let d = delta["thinking"].as_str().unwrap_or_default();
                        thinking.push_str(d);
                        out.push(AssistantEvent::ThinkingDelta { content_index: index, delta: d.into() });
                    }
                    (Content::Thinking { signature, .. }, "signature_delta") => signature.get_or_insert_with(String::new).push_str(delta["signature"].as_str().unwrap_or_default()),
                    (Content::ToolCall(_), "input_json_delta") => {
                        let d = delta["partial_json"].as_str().unwrap_or_default();
                        block.arguments.push_str(d);
                        out.push(AssistantEvent::ToolcallDelta { content_index: index, delta: d.into() });
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(block) = self.blocks.get_mut(&index) {
                    if block.stopped {
                        return Err("Anthropic duplicate content_block_stop".to_string().into());
                    }
                    block.stopped = true;
                    if let Content::ToolCall(call) = &mut block.content {
                        if !block.arguments.trim().is_empty() {
                            call.arguments = serde_json::from_str(&block.arguments).unwrap_or_else(|_| json!({"_invalidJson":block.arguments}));
                        }
                        out.push(AssistantEvent::ToolcallEnd { content_index: index, tool_call: call.clone() });
                    }
                }
            }
            "message_delta" => {
                self.usage(&value["usage"]);
                if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                    self.message.stop_reason = match reason {
                        "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
                        "tool_use" => StopReason::ToolUse,
                        // No refusal/pause variants exist. These are completed, non-tool turns.
                        "end_turn" | "stop_sequence" | "refusal" | "pause_turn" => StopReason::Stop,
                        other => return Err(format!("Anthropic unknown stop_reason: {other}").into()),
                    };
                }
            }
            "message_stop" => self.completed = true,
            "error" => {
                return Err(CallError {
                    reason: format!("Anthropic stream error: {}", value["error"]),
                    transient: matches!(value["error"]["type"].as_str(), Some("overloaded_error" | "api_error" | "rate_limit_error")),
                    retry_after: None,
                })
            }
            _ => {} // ping and future informational events
        }
        Ok(())
    }

    fn finish(mut self) -> Result<AssistantMessage, CallError> {
        if !self.completed {
            return Err("Anthropic stream ended without message_stop".to_string().into());
        }
        if self.blocks.values().any(|block| !block.stopped) {
            return Err("Anthropic message stopped with unfinished content blocks".to_string().into());
        }
        self.message.content = self.blocks.into_values().map(|block| block.content).collect();
        Ok(self.message)
    }
}

async fn parse_stream(response: reqwest::Response, model: &str, tx: &mpsc::Sender<AssistantEvent>, provider: &AnthropicProvider) -> Result<AssistantMessage, CallError> {
    let idle_timeout = provider.stream_idle_timeout;
    let _ = tx.send(AssistantEvent::Start).await;
    let mut state = StreamState::new(model);
    let mut bytes = response.bytes_stream();
    let mut buffer = vec![];
    while let Some(chunk) = tokio::time::timeout(idle_timeout, bytes.next()).await.map_err(|_| idle_timeout_error(idle_timeout))? {
        buffer.extend_from_slice(&chunk.map_err(|_| "stream body read error".to_string())?);
        for data in drain_sse_lines(&mut buffer) {
            let value: Value = serde_json::from_str(&data).map_err(|_| "Anthropic invalid SSE JSON".to_string())?;
            // AssistantMessage has no provider-specific ID field; trace it alongside request-id.
            if value["type"] == "message_start" {
                if let Some(id) = value["message"]["id"].as_str() {
                    tracing::debug!(message_id = %provider.safe(id), "Anthropic message started");
                }
            }
            let mut events = vec![];
            state.on_event(&value, &mut events)?;
            for event in events {
                tx.send(event).await.map_err(|_| "consumer closed".to_string())?;
            }
            if state.completed {
                return state.finish();
            }
        }
    }
    state.finish()
}
