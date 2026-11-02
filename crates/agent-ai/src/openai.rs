//! OpenAI provider：Chat Completions（对应 pi `api/openai-completions.ts`）和 Responses API（`api/openai-responses.ts`，见 `responses.rs`）。
//! 按 `Request.api` 选择；`auto` 时由 `models::default_api` 按模型决定。
use crate::retry::{retry_after, CallError};
use crate::{models::default_api, provider::*, types::*};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

pub struct OpenAiProvider {
    api_key: crate::secrets::Secret,
    base_url: String,
    /// 账号级请求头（OpenAI-Organization / OpenAI-Project），所有请求都带上
    extra_headers: reqwest::header::HeaderMap,
    http: reqwest::Client,
    stream_idle_timeout: Duration,
    /// 运行时从 400 错误里学到的推理强度能力
    effort_cache: EffortCache,
    /// auto 模式下运行时发现必须走 Responses 的模型
    api_cache: Arc<Mutex<HashMap<String, Api>>>,
}

/// 缓存键：(模型, API, 是否带工具)。值：None = 不发送推理强度；Some(levels) = 只能用这些档位。
/// 带上 API 和“是否带工具”，避免“函数工具 + 推理强度”之类的限制误伤不带工具的会话或另一个 API。
type EffortKey = (String, Api, bool);
type EffortCache = Arc<Mutex<HashMap<EffortKey, Option<Vec<ThinkingLevel>>>>>;

/// 结合运行时缓存，算出这次真正要发送的推理强度。
fn effective_effort(cache: &EffortCache, key: &EffortKey, requested: Option<ThinkingLevel>) -> Option<ThinkingLevel> {
    let requested = requested?;
    match cache.lock().unwrap().get(key) {
        Some(None) => None,
        Some(Some(levels)) => runtime_effort(requested, levels),
        None => Some(requested),
    }
}

/// 运行时列表是排序依据：在列表中按位置取，内置名称取最近（平局取高）；无法定位的名称取最高档。
fn runtime_effort(requested: ThinkingLevel, levels: &[ThinkingLevel]) -> Option<ThinkingLevel> {
    requested.nearest_in(levels).or_else(|| levels.last().copied())
}

/// 提取 provider 给出的顺序（包括未来的新名称），供运行时缓存和探测报告共用。
pub fn supported_values(body: &str) -> Vec<ThinkingLevel> {
    let decoded = serde_json::from_str::<Value>(body).ok();
    let message = decoded.as_ref().and_then(|v| v["error"]["message"].as_str()).unwrap_or(body);
    let Some((_, tail)) = message.split_once("Supported values are:") else { return vec![] };
    let tail = tail.split('.').next().unwrap_or(tail);
    if let Some(quote) = tail.chars().find(|c| matches!(c, '\'' | '"')) {
        return tail.split(quote).skip(1).step_by(2).filter_map(|name| name.parse().ok()).collect();
    }
    tail.split([',', ' ', '[', ']', '\n', '\r']).filter(|name| !name.is_empty() && *name != "and").filter_map(|name| name.parse().ok()).collect()
}

/// OpenAI 关于推理强度的 400 错误分类。
#[derive(Debug, PartialEq)]
enum EffortError {
    /// 模型根本不认识这个参数
    Unsupported,
    /// 只支持这些档位
    Levels(Vec<ThinkingLevel>),
    /// Chat Completions 上不允许“函数工具 + reasoning_effort”（gpt-5.4+ / gpt-6.x），应改走 Responses
    ToolsConflict,
}

fn parse_effort_error(body: &str) -> Option<EffortError> {
    if !(body.contains("reasoning_effort") || body.contains("reasoning.effort")) {
        return None;
    }
    if body.contains("Function tools with reasoning_effort are not supported") {
        return Some(EffortError::ToolsConflict);
    }
    if body.contains("Unrecognized request argument") || body.contains("Unsupported parameter") {
        return Some(EffortError::Unsupported);
    }
    if !body.contains("Supported values are:") {
        return None;
    }
    let levels = supported_values(body);
    Some(if levels.is_empty() { EffortError::Unsupported } else { EffortError::Levels(levels) })
}

/// OpenAI provider 实例的连接参数（按账号）。
#[derive(Default, Clone, Debug)]
pub struct OpenAiOptions {
    pub base_url: Option<String>,
    /// → `OpenAI-Organization` 请求头
    pub organization: Option<String>,
    /// → `OpenAI-Project` 请求头
    pub project: Option<String>,
    /// 默认 300 秒
    pub stream_idle_timeout: Option<Duration>,
}

/// organization / project 请求头；不是合法 header 值的条目被忽略（配置校验会提前拒绝）。
fn account_headers(opts: &OpenAiOptions) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in [("openai-organization", &opts.organization), ("openai-project", &opts.project)] {
        match value.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(reqwest::header::HeaderValue::from_str) {
            Some(Ok(v)) => {
                headers.insert(name, v);
            }
            Some(Err(_)) => tracing::warn!(header = name, "ignoring invalid OpenAI account header value"),
            None => {}
        }
    }
    headers
}

impl OpenAiProvider {
    /// key 只在进程内存中，从不通过对外接口返回。
    pub fn new(api_key: crate::secrets::Secret, base_url: Option<String>) -> Self {
        Self::with_options(api_key, OpenAiOptions { base_url, ..Default::default() })
    }

    pub fn with_stream_idle_timeout(api_key: crate::secrets::Secret, base_url: Option<String>, stream_idle_timeout: Duration) -> Self {
        Self::with_options(api_key, OpenAiOptions { base_url, stream_idle_timeout: Some(stream_idle_timeout), ..Default::default() })
    }

    pub fn with_options(api_key: crate::secrets::Secret, opts: OpenAiOptions) -> Self {
        Self {
            api_key,
            extra_headers: account_headers(&opts),
            base_url: opts.base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
            http: reqwest::Client::builder().connect_timeout(Duration::from_secs(30)).build().expect("http client"),
            stream_idle_timeout: opts.stream_idle_timeout.unwrap_or(Duration::from_secs(300)),
            effort_cache: Default::default(),
            api_cache: Default::default(),
        }
    }

    fn resolve_api(&self, req: &Request) -> Api {
        match req.api {
            Api::Auto if req.raw_effort => default_api(&req.model),
            Api::Auto => self.api_cache.lock().unwrap().get(&req.model).copied().unwrap_or_else(|| default_api(&req.model)),
            a => a,
        }
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
        body["reasoning_effort"] = json!(level.as_str());
    }
    body
}

impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    fn list_models(&self) -> futures::future::BoxFuture<'_, Result<Vec<crate::models::ModelInfo>, String>> {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let resp = self
                .http
                .get(format!("{}/models", self.base_url))
                .headers(self.extra_headers.clone())
                .bearer_auth(self.api_key.expose())
                .timeout(std::time::Duration::from_secs(30))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                tracing::error!(status = status.as_u16(), "list models failed");
                return Err(redact(&format!("OpenAI HTTP {status}: {body}")));
            }
            let v: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            let mut out: Vec<_> = v["data"]
                .as_array()
                .map(|a| a.iter().filter_map(|m| Some(crate::models::info_for(m["id"].as_str()?.to_string(), m["owned_by"].as_str().map(String::from), m["created"].as_i64()))).collect())
                .unwrap_or_default();
            out.sort_by(|a: &crate::models::ModelInfo, b| a.id.cmp(&b.id));
            tracing::info!(count = out.len(), latency_ms = started.elapsed().as_millis() as u64, "listed models");
            Ok(out)
        })
    }

    fn stream(&self, req: Request, cancel: CancellationToken) -> EventStream {
        let (tx, rx) = mpsc::channel(64);
        let ctx = CallCtx {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            headers: self.extra_headers.clone(),
            key: self.api_key.clone(),
            effort_cache: self.effort_cache.clone(),
            api_cache: self.api_cache.clone(),
            stream_idle_timeout: self.stream_idle_timeout,
        };
        let api = self.resolve_api(&req);
        let span = tracing::info_span!("llm", provider = "openai", model = %req.model, api = tracing::field::Empty, reasoning_effort = tracing::field::Empty);
        tokio::spawn(tracing::Instrument::instrument(
            async move {
                let model = req.model.clone();
                let started = std::time::Instant::now();
                tracing::debug!(messages = req.messages.len(), tools = req.tools.len(), api = %api, "request sent");
                let mut attempt = (api, req.reasoning);
                let fut = run_with_retries(&ctx, req, api, &tx, &mut attempt);
                let r = tokio::select! {
                    r = fut => Some(r),
                    _ = cancel.cancelled() => None,
                };
                let final_ev = match r {
                    Some(Ok(msg)) => AssistantEvent::Done { message: msg },
                    Some(Err(e)) => {
                        // 失败的消息也带上最后一次尝试的 API / 推理强度，便于上层如实报告
                        let mut m = AssistantMessage::failed(&model, StopReason::Error, e);
                        m.api = Some(attempt.0);
                        m.reasoning_effort = attempt.1;
                        AssistantEvent::Error { message: m }
                    }
                    None => AssistantEvent::Error { message: AssistantMessage::failed(&model, StopReason::Aborted, "aborted") },
                };
                let latency_ms = started.elapsed().as_millis() as u64;
                match &final_ev {
                    AssistantEvent::Done { message: m } if m.stop_reason == StopReason::Error => {
                        tracing::warn!(issue = "typed", latency_ms, error = m.error_message.as_deref().unwrap_or(""), "llm call failed")
                    }
                    AssistantEvent::Done { message: m } => {
                        tracing::info!(latency_ms, stop_reason = ?m.stop_reason, input_tokens = m.usage.input, output_tokens = m.usage.output, reasoning_tokens = m.usage.reasoning, cached_tokens = m.usage.cache_read, tool_calls = m.tool_calls().len(), "llm call finished")
                    }
                    AssistantEvent::Error { message: m } if m.stop_reason == StopReason::Aborted => {
                        tracing::info!(latency_ms, "llm call aborted")
                    }
                    AssistantEvent::Error { message: m } => tracing::warn!(issue = "typed", latency_ms, error = m.error_message.as_deref().unwrap_or(""), "llm call failed"),
                    _ => {}
                }
                let _ = tx.send(final_ev).await;
            },
            span,
        ));
        Box::pin(ReceiverStream::new(rx))
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

struct CallCtx {
    stream_idle_timeout: Duration,
    http: reqwest::Client,
    base_url: String,
    headers: reqwest::header::HeaderMap,
    key: crate::secrets::Secret,
    effort_cache: EffortCache,
    api_cache: Arc<Mutex<HashMap<String, Api>>>,
}

async fn run_with_retries(ctx: &CallCtx, req: Request, api: Api, tx: &mpsc::Sender<AssistantEvent>, attempt: &mut (Api, Option<ThinkingLevel>)) -> Result<AssistantMessage, String> {
    let max_retries = if req.raw_effort { 0 } else { req.llm_max_retries.unwrap_or(3) };
    let mut state = (ctx, req, api, attempt);
    crate::retry::run_with_retries(&mut state, max_retries, tx, |state, tx| Box::pin(run(state.0, state.1.clone(), state.2, tx, state.3)))
        .await
        .map_err(|error| redact(&if ctx.key.expose().is_empty() { error } else { error.replace(ctx.key.expose(), "[redacted]") }))
}

/// 发请求；遇到推理强度相关的 400 时按错误类型调整（最多重试 2 次，且同样的请求不会重发），然后解析流。
async fn run(ctx: &CallCtx, mut req: Request, mut api: Api, tx: mpsc::Sender<AssistantEvent>, attempt: &mut (Api, Option<ThinkingLevel>)) -> Result<AssistantMessage, CallError> {
    let requested = if req.raw_effort { req.reasoning } else { crate::models::resolve_effort_quiet("openai", &req.model, req.reasoning).effective };
    let auto = req.api == Api::Auto;
    let mut retries = 0;
    let mut tried: Vec<(Api, Option<ThinkingLevel>)> = vec![];
    let resp = loop {
        let key = (req.model.clone(), api, !req.tools.is_empty());
        req.reasoning = if req.raw_effort { requested } else { effective_effort(&ctx.effort_cache, &key, requested) };
        tried.push((api, req.reasoning));
        *attempt = (api, req.reasoning);
        let (url, body) = match api {
            Api::Responses => (format!("{}/responses", ctx.base_url), crate::responses::build_body(&req)),
            _ => (format!("{}/chat/completions", ctx.base_url), build_body(&req)),
        };
        let record = |api: Api, effort: Option<ThinkingLevel>| {
            let span = tracing::Span::current();
            span.record("api", api.as_str());
            span.record("reasoning_effort", effort.map(|l| l.as_str()).unwrap_or("-"));
        };
        let resp = tokio::time::timeout(Duration::from_secs(120), ctx.http.post(&url).headers(ctx.headers.clone()).bearer_auth(ctx.key.expose()).json(&body).send())
            .await
            .map_err(|_| "response headers timeout after 120s".to_string())?
            .map_err(CallError::transport)?;
        if resp.status().is_success() {
            record(api, req.reasoning);
            break resp;
        }
        let status = resp.status();
        let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(retry_after);
        // Preserve HTTP classification even if reading the error body fails/times out.
        let text = tokio::time::timeout(ctx.stream_idle_timeout, resp.text()).await.ok().and_then(Result::ok).unwrap_or_default();
        let safe_text = if ctx.key.expose().is_empty() { text.clone() } else { text.replace(ctx.key.expose(), "[redacted]") };
        let err_msg = redact(&format!("OpenAI HTTP {status} ({} API): {safe_text}", api.as_str()));
        if req.raw_effort || status != reqwest::StatusCode::BAD_REQUEST || retries >= 2 {
            record(api, req.reasoning);
            return Err(CallError { reason: err_msg, transient: matches!(status.as_u16(), 408 | 429 | 500..=599), retry_after });
        }
        let Some(kind) = parse_effort_error(&text) else {
            record(api, req.reasoning);
            return Err(err_msg.into());
        };
        retries += 1;
        match kind {
            // auto：直接换 Responses（不再在 Chat 上试 none / 省略，避免白白浪费请求）
            EffortError::ToolsConflict if auto && api == Api::Chat => {
                ctx.api_cache.lock().unwrap().insert(req.model.clone(), Api::Responses);
                tracing::warn!(issue = "typed", model = %req.model, "chat completions rejects tools + reasoning_effort for this model; switching to responses API");
                api = Api::Responses;
            }
            // 已经没发推理强度了还报这个错（gpt-6.x）：再在 Chat 上重试没有意义
            EffortError::ToolsConflict if req.reasoning.is_none() => {
                record(api, req.reasoning);
                return Err(format!("{err_msg} (hint: this model needs api=responses for function tools)").into());
            }
            EffortError::ToolsConflict => {
                ctx.effort_cache.lock().unwrap().insert(key, None);
                tracing::warn!(issue = "typed", model = %req.model, requested = %requested.map(|l| l.as_str()).unwrap_or("-"), "chat completions rejects tools + reasoning_effort; retrying without it (use api=responses to keep reasoning effort)");
            }
            EffortError::Unsupported => {
                ctx.effort_cache.lock().unwrap().insert(key, None);
                tracing::warn!(issue = "typed", model = %req.model, api = %api, requested = %requested.map(|l| l.as_str()).unwrap_or("-"), "model does not accept reasoning effort; retrying without it");
            }
            EffortError::Levels(levels) => {
                let next = requested.and_then(|r| runtime_effort(r, &levels));
                tracing::warn!(issue = "typed", model = %req.model, api = %api, requested = %requested.map(|l| l.as_str()).unwrap_or("-"), using = next.map(|l| l.as_str()).unwrap_or("-"), supported = ?levels, "reasoning effort not supported by model; using nearest");
                ctx.effort_cache.lock().unwrap().insert(key, Some(levels));
            }
        }
        // 下一次请求和已经失败过的一样：不重发，直接报错
        let next_key = (req.model.clone(), api, !req.tools.is_empty());
        if tried.contains(&(api, effective_effort(&ctx.effort_cache, &next_key, requested))) {
            record(api, req.reasoning);
            return Err(err_msg.into());
        }
    };
    let mut msg = match api {
        Api::Responses => crate::responses::parse_stream(resp, &req.model, &tx, ctx.stream_idle_timeout).await?,
        _ => parse_chat(resp, &req.model, &tx, ctx.stream_idle_timeout).await?,
    };
    msg.api = Some(api);
    if let Some(error) = &mut msg.error_message {
        *error = redact(&if ctx.key.expose().is_empty() { error.clone() } else { error.replace(ctx.key.expose(), "[redacted]") });
    }
    msg.reasoning_effort = req.reasoning;
    Ok(msg)
}

/// 按行切 SSE，只取 `data: ` 行。按字节切分 LF，避免多字节 UTF-8 字符跨 chunk 被截断。
pub(crate) fn drain_sse_lines(buf: &mut Vec<u8>) -> Vec<String> {
    let mut out = vec![];
    while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
        let line = String::from_utf8_lossy(&buf[..pos]).trim_end_matches('\r').to_string();
        buf.drain(..=pos);
        if let Some(d) = line.strip_prefix("data:") {
            out.push(d.trim_start().to_string());
        }
    }
    out
}

pub(crate) fn idle_timeout_error(timeout: Duration) -> String {
    format!("stream idle timeout after {}s", timeout.as_secs_f64())
}

async fn parse_chat(resp: reqwest::Response, model: &str, tx: &mpsc::Sender<AssistantEvent>, idle_timeout: Duration) -> Result<AssistantMessage, String> {
    let _ = tx.send(AssistantEvent::Start).await;
    let mut msg = AssistantMessage::empty(model);
    let mut text = String::new();
    let mut calls: BTreeMap<u64, PartialCall> = BTreeMap::new();
    let mut finish: Option<String> = None;
    let mut buf: Vec<u8> = Vec::new();
    let mut bytes = resp.bytes_stream();
    'stream: while let Some(chunk) = tokio::time::timeout(idle_timeout, bytes.next()).await.map_err(|_| idle_timeout_error(idle_timeout))? {
        buf.extend_from_slice(&chunk.map_err(|_| "stream body read error".to_string())?);
        // 按字节切分 LF，避免多字节 UTF-8 字符跨 chunk 被截断
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line = String::from_utf8_lossy(&buf[..pos]).trim_end_matches('\r').to_string();
            buf.drain(..=pos);
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                break 'stream;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
                return Err(redact(&format!("OpenAI stream error: {e}")));
            }
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                msg.usage.cache_read = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                msg.usage.input = u["prompt_tokens"].as_u64().unwrap_or(0);
                msg.usage.output = u["completion_tokens"].as_u64().unwrap_or(0);
                msg.usage.total_tokens = u["total_tokens"].as_u64().unwrap_or(0);
                msg.usage.reasoning = u["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0);
            }
            let Some(choice) = v["choices"].get(0) else {
                continue;
            };
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
pub fn redact(s: &str) -> String {
    s.split_inclusive(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
        .map(|w| {
            if w.trim_start_matches(|c: char| !c.is_alphanumeric()).starts_with("sk-") {
                "[redacted]".to_string() + &w[w.trim_end_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',').len()..]
            } else {
                w.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn assert_idle_timeout(api: Api) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            let event = match api {
                Api::Responses => r#"{"type":"response.output_text.delta","delta":"partial"}"#,
                _ => r#"{"choices":[{"delta":{"content":"partial"}}]}"#,
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {event}\n\n").as_bytes()).await.unwrap();
            std::future::pending::<()>().await;
        });
        let provider = OpenAiProvider::with_stream_idle_timeout(crate::Secret::new("test"), Some(format!("http://{addr}")), Duration::from_millis(200));
        let req =
            Request { model: "test".into(), system_prompt: None, messages: vec![], tools: vec![], reasoning: None, api, max_tokens: None, raw_effort: false, thinking: None, llm_max_retries: Some(0) };
        let started = std::time::Instant::now();
        let events = tokio::time::timeout(Duration::from_secs(2), provider.stream(req, CancellationToken::new()).collect::<Vec<_>>()).await.expect("stream hung");
        server.abort();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(events.iter().any(|e| matches!(e, AssistantEvent::TextDelta { delta, .. } if delta == "partial")));
        assert!(
            matches!(events.last(), Some(AssistantEvent::Error { message }) if message.stop_reason == StopReason::Error && message.error_message.as_deref() == Some("stream idle timeout after 0.2s"))
        );
    }

    #[tokio::test]
    async fn chat_stream_idle_timeout() {
        assert_idle_timeout(Api::Chat).await;
    }

    #[tokio::test]
    async fn responses_stream_idle_timeout() {
        assert_idle_timeout(Api::Responses).await;
    }

    #[tokio::test]
    async fn effort_adjustment_does_not_emit_retry() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for api in [Api::Chat, Api::Responses] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                for index in 0..2 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") {
                        headers.push(socket.read_u8().await.unwrap());
                    }
                    let headers = String::from_utf8(headers).unwrap();
                    let length: usize = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").map(|s| s.parse().unwrap())).unwrap();
                    socket.read_exact(&mut vec![0; length]).await.unwrap();
                    let (status, body) = if index == 0 {
                        ("400 Bad Request", "Unsupported parameter: reasoning_effort")
                    } else if api == Api::Chat {
                        ("200 OK", "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n")
                    } else {
                        ("200 OK", "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n")
                    };
                    socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
            });
            let provider = OpenAiProvider::new(crate::Secret::new("test"), Some(format!("http://{addr}")));
            let req = Request {
                model: "test".into(),
                system_prompt: None,
                messages: vec![],
                tools: vec![],
                reasoning: Some(ThinkingLevel::High),
                api,
                max_tokens: None,
                raw_effort: false,
                thinking: None,
                llm_max_retries: Some(0),
            };
            let events = tokio::time::timeout(Duration::from_secs(2), provider.stream(req, CancellationToken::new()).collect::<Vec<_>>()).await.unwrap();
            server.await.unwrap();
            assert!(matches!(events.last(), Some(AssistantEvent::Done { .. })));
            assert!(!events.iter().any(|event| matches!(event, AssistantEvent::Retry { .. })));
        }
    }

    #[test]
    fn provider_order_includes_unknown_levels() {
        use ThinkingLevel::*;
        let expected = vec![Low, Max, Other("ultra"), Other("unknown"), Other("and")];
        for text in
            ["reasoning_effort: Supported values are: 'low', 'max', 'ultra', 'unknown', and 'and'.", "reasoning.effort: Supported values are: \"low\", \"max\", \"ultra\", \"unknown\", and \"and\"."]
        {
            assert_eq!(parse_effort_error(text), Some(EffortError::Levels(expected.clone())));
            assert_eq!(supported_values(&json!({"error":{"message":text}}).to_string()), expected);
        }
        assert_eq!(runtime_effort(High, &expected), Some(Max));
        assert_eq!(runtime_effort(Off, &[Minimal, Low, Medium, High]), Some(Minimal));
        assert_eq!(runtime_effort(Xhigh, &[Low, Medium, High]), Some(High));
        assert_eq!(runtime_effort(High, &[Other("ultra"), Other("mega")]), Some(Other("mega")));
        assert_eq!(runtime_effort(Other("ultra"), &expected), Some(Other("ultra")));
        assert_eq!(runtime_effort(Other("absent"), &expected), Some(Other("and")));
    }

    #[tokio::test]
    async fn runtime_order_cache_and_raw_bypass() {
        use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
        #[derive(Clone, Default)]
        struct Mock(Arc<Mutex<Vec<Value>>>);
        async fn handler(State(mock): State<Mock>, Json(body): Json<Value>) -> (StatusCode, String) {
            let mut requests = mock.0.lock().unwrap();
            let index = requests.len();
            requests.push(body);
            if matches!(index, 0 | 3) {
                return (StatusCode::BAD_REQUEST, "reasoning_effort: Supported values are: 'max', 'ultra', 'unknown'.".into());
            }
            if index == 4 {
                return (StatusCode::SERVICE_UNAVAILABLE, "busy".into());
            }
            (StatusCode::OK, "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n".into())
        }
        for api in [Api::Chat, Api::Responses] {
            let mock = Mock::default();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let provider = OpenAiProvider::new(crate::Secret::new("test"), Some(format!("http://{}", listener.local_addr().unwrap())));
            let app = Router::new().route("/chat/completions", post(handler)).route("/responses", post(handler)).with_state(mock.clone());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let mut req = Request {
                model: "gpt-5".into(),
                reasoning: Some(ThinkingLevel::Off),
                raw_effort: false,
                thinking: None,
                api,
                system_prompt: None,
                messages: vec![],
                tools: vec![],
                max_tokens: Some(1024),
                llm_max_retries: Some(3),
            };
            for _ in 0..2 {
                let events = provider.stream(req.clone(), CancellationToken::new()).collect::<Vec<_>>().await;
                assert!(matches!(events.last(), Some(AssistantEvent::Done { message }) if message.reasoning_effort == Some(ThinkingLevel::Max)));
            }
            req.raw_effort = true;
            for level in [ThinkingLevel::Other("ultra"), ThinkingLevel::Max] {
                req.reasoning = Some(level);
                let events = provider.stream(req.clone(), CancellationToken::new()).collect::<Vec<_>>().await;
                assert!(matches!(events.last(), Some(AssistantEvent::Error { message }) if message.reasoning_effort == Some(level)));
                assert!(!events.iter().any(|e| matches!(e, AssistantEvent::Retry { .. })));
            }
            req.reasoning = Some(ThinkingLevel::Off);
            provider.stream(req, CancellationToken::new()).collect::<Vec<_>>().await;
            let requests = mock.0.lock().unwrap();
            let field = if api == Api::Chat { "/reasoning_effort" } else { "/reasoning/effort" };
            let levels: Vec<_> = requests.iter().map(|v| v.pointer(field).unwrap().as_str().unwrap()).collect();
            assert_eq!(levels, ["low", "max", "max", "ultra", "max", "none"]);
            assert_eq!(provider.effort_cache.lock().unwrap().get(&("gpt-5".into(), api, false)), Some(&Some(vec![ThinkingLevel::Max, ThinkingLevel::Other("ultra"), ThinkingLevel::Other("unknown")])));
            server.abort();
        }
    }

    #[tokio::test]
    async fn organization_and_project_headers_are_sent() {
        use axum::{
            extract::State,
            http::HeaderMap,
            routing::{get, post},
            Router,
        };
        type Seen = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;
        fn record(seen: &Seen, path: &str, headers: &HeaderMap) {
            let get = |name: &str| headers.get(name).map(|v| v.to_str().unwrap().to_string());
            seen.lock().unwrap().push((path.into(), get("openai-organization"), get("openai-project")));
        }
        async fn chat(State(seen): State<Seen>, headers: HeaderMap) -> String {
            record(&seen, "chat", &headers);
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".into()
        }
        async fn responses(State(seen): State<Seen>, headers: HeaderMap) -> String {
            record(&seen, "responses", &headers);
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n".into()
        }
        async fn models(State(seen): State<Seen>, headers: HeaderMap) -> String {
            record(&seen, "models", &headers);
            r#"{"data":[{"id":"gpt-6.1-sol"}]}"#.into()
        }
        for (org, project) in [(Some("org_abc123"), Some("proj_xyz789")), (None, None)] {
            let seen = Seen::default();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new().route("/chat/completions", post(chat)).route("/responses", post(responses)).route("/models", get(models)).with_state(seen.clone());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let provider = OpenAiProvider::with_options(
                crate::Secret::new("test"),
                OpenAiOptions { base_url: Some(base), organization: org.map(String::from), project: project.map(String::from), ..Default::default() },
            );
            for api in [Api::Chat, Api::Responses] {
                let req = Request {
                    model: "m".into(),
                    system_prompt: None,
                    messages: vec![],
                    tools: vec![],
                    reasoning: None,
                    api,
                    max_tokens: None,
                    raw_effort: false,
                    thinking: None,
                    llm_max_retries: Some(0),
                };
                let events = provider.stream(req, CancellationToken::new()).collect::<Vec<_>>().await;
                assert!(matches!(events.last(), Some(AssistantEvent::Done { .. })), "{events:?}");
            }
            assert_eq!(provider.list_models().await.unwrap().len(), 1);
            let expected: Vec<_> = ["chat", "responses", "models"].iter().map(|p| (p.to_string(), org.map(String::from), project.map(String::from))).collect();
            assert_eq!(*seen.lock().unwrap(), expected);
            server.abort();
        }
        // 非法 header 值被忽略而不是 panic
        assert!(account_headers(&OpenAiOptions { organization: Some("bad\nvalue".into()), ..Default::default() }).is_empty());
    }

    #[test]
    fn retry_after_seconds_and_dates() {
        assert_eq!(retry_after(" 42 "), Some(Duration::from_secs(42)));
        assert_eq!(retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), Some(Duration::ZERO));
        let future = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(120));
        assert_eq!(retry_after(&future), Some(crate::MAX_RETRY_AFTER));
        assert_eq!(retry_after("86400"), Some(crate::MAX_RETRY_AFTER));
        assert_eq!(retry_after(&u64::MAX.to_string()), Some(crate::MAX_RETRY_AFTER));
        assert_eq!(retry_after("invalid"), None);
    }

    #[test]
    fn parses_effort_errors() {
        use EffortError::*;
        let e = r#"{"error":{"message":"Unsupported value: 'reasoning_effort' does not support 'minimal' with this model. Supported values are: 'none', 'low', 'medium', 'high', and 'xhigh'."}}"#;
        assert_eq!(parse_effort_error(e), Some(Levels(vec![ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::Medium, ThinkingLevel::High, ThinkingLevel::Xhigh])));
        let r = r#"{"error":{"message":"Unsupported value: 'reasoning.effort' does not support 'none' with this model. Supported values are: 'low', 'medium', 'high', and 'xhigh'.","param":"reasoning.effort"}}"#;
        assert_eq!(parse_effort_error(r), Some(Levels(vec![ThinkingLevel::Low, ThinkingLevel::Medium, ThinkingLevel::High, ThinkingLevel::Xhigh])));
        assert_eq!(parse_effort_error("Unrecognized request argument supplied: reasoning_effort"), Some(Unsupported));
        assert_eq!(parse_effort_error("Unsupported parameter: 'reasoning.effort' is not supported with this model."), Some(Unsupported));
        assert_eq!(
            parse_effort_error("Function tools with reasoning_effort are not supported for gpt-6-astra in /v1/chat/completions. Please use /v1/responses or set reasoning_effort to 'none'"),
            Some(ToolsConflict)
        );
        assert_eq!(parse_effort_error("model not found"), None);
    }

    #[test]
    fn effort_cache_is_keyed_by_api_and_tools() {
        let cache: EffortCache = Default::default();
        let with_tools = ("m".to_string(), Api::Chat, true);
        let no_tools = ("m".to_string(), Api::Chat, false);
        let responses = ("m".to_string(), Api::Responses, true);
        cache.lock().unwrap().insert(with_tools.clone(), None);
        assert_eq!(effective_effort(&cache, &with_tools, Some(ThinkingLevel::High)), None);
        // 学到的“带工具不能发推理强度”不能影响不带工具的会话和 Responses
        assert_eq!(effective_effort(&cache, &no_tools, Some(ThinkingLevel::High)), Some(ThinkingLevel::High));
        assert_eq!(effective_effort(&cache, &responses, Some(ThinkingLevel::High)), Some(ThinkingLevel::High));
        cache.lock().unwrap().insert(no_tools.clone(), Some(vec![ThinkingLevel::Off, ThinkingLevel::Low]));
        assert_eq!(effective_effort(&cache, &no_tools, Some(ThinkingLevel::Minimal)), Some(ThinkingLevel::Low));
        assert_eq!(effective_effort(&cache, &no_tools, None), None);
    }

    #[test]
    fn redacts_keys() {
        let r = redact(r#"{"message":"Incorrect API key provided: sk-proj-abc***xyz. See"}"#);
        assert!(!r.contains("sk-"), "{r}");
        assert!(r.contains("[redacted]"));
    }
}
