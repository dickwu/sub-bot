//! MCP 工具 → `AgentTool`：命名（`mcp__<server>__<tool>`，provider 限制 `^[a-zA-Z0-9_-]{1,64}$`）、
//! inputSchema 清洗（OpenAI / Anthropic 都要求顶层 `type: object`）、tools/call 结果映射。
use super::Server;
use agent_ai::Content;
use agent_core::{AgentTool, ToolResult, UpdateSink};
use serde_json::{json, Map, Value};
use std::{collections::BTreeSet, sync::Arc};
use tokio_util::sync::CancellationToken;

/// provider 工具名上限（OpenAI / Anthropic 都是 64）
pub const MAX_TOOL_NAME_LEN: usize = 64;
/// 单次结果给模型的文本上限（与 read / bash 等内置工具一致）
pub const MAX_RESULT_BYTES: usize = 50_000;
const HASH_LEN: usize = 8;

/// 一个已发现的 MCP 工具（allow-list 过滤、命名、schema 清洗之后）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    /// 服务器上的原始名字（tools/call 用）
    pub mcp_name: String,
    /// 暴露给模型的名字
    pub exposed: String,
    pub description: String,
    pub schema: Value,
}

pub fn is_mcp_name(name: &str) -> bool {
    name.starts_with("mcp__")
}

fn short_hash(s: &str) -> String {
    crate::config::sha256_hex(s)[..HASH_LEN].to_string()
}

fn with_hash(base: &str, server: &str, tool: &str) -> String {
    let keep = MAX_TOOL_NAME_LEN - 1 - HASH_LEN;
    // base 只含 ASCII，按字节截断安全
    format!("{}_{}", &base[..base.len().min(keep)], short_hash(&format!("{server}\0{tool}")))
}

/// `mcp__<server>__<tool>`：非 `[A-Za-z0-9_-]` 字符替换为 `_`；超过 64 字符 ⇒ 截断 + `_` + 8 位哈希（原名的 sha256）。
pub fn exposed_name(server: &str, tool: &str) -> String {
    let sanitized: String = tool.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    let full = format!("mcp__{server}__{sanitized}");
    if full.len() <= MAX_TOOL_NAME_LEN {
        full
    } else {
        with_hash(&full, server, tool)
    }
}

/// 为一个服务器的全部工具分配名字；清洗后撞名的（如 `a.b` 与 `a_b`）后来者加哈希后缀。
pub fn assign_names(server: &str, tools: &[String]) -> Vec<String> {
    let mut used = BTreeSet::new();
    tools
        .iter()
        .map(|tool| {
            let mut name = exposed_name(server, tool);
            if used.contains(&name) {
                name = with_hash(&name, server, tool);
            }
            used.insert(name.clone());
            name
        })
        .collect()
}

/// 清洗 inputSchema，使 OpenAI（function.parameters）与 Anthropic（input_schema）都接受：
/// 顶层必须是 `type: "object"` 且有 `properties`；去掉顶层 `$schema` 与两家都拒绝的顶层组合子
/// （oneOf / anyOf / allOf / not / enum / const）；`required` 只保留字符串。非对象 schema ⇒ 宽松的空对象 schema。
pub fn sanitize_schema(schema: &Value) -> Value {
    let permissive = || json!({"type": "object", "properties": {}, "additionalProperties": true});
    let Value::Object(input) = schema else { return permissive() };
    let mut obj: Map<String, Value> = input.clone();
    match obj.get("type") {
        None => {
            obj.insert("type".into(), json!("object"));
        }
        Some(Value::String(t)) if t == "object" => {}
        Some(Value::Array(types)) if types.iter().any(|t| t == "object") => {
            obj.insert("type".into(), json!("object"));
        }
        Some(_) => return permissive(),
    }
    for key in ["$schema", "oneOf", "anyOf", "allOf", "not", "enum", "const"] {
        obj.remove(key);
    }
    if !obj.get("properties").is_some_and(Value::is_object) {
        obj.insert("properties".into(), json!({}));
    }
    match obj.get("required") {
        Some(Value::Array(req)) => {
            let names: Vec<Value> = req.iter().filter(|v| v.is_string()).cloned().collect();
            obj.insert("required".into(), Value::Array(names));
        }
        Some(_) => {
            obj.remove("required");
        }
        None => {}
    }
    Value::Object(obj)
}

/// base64 解码后的字节数（不解码）
fn b64_len(data: &str) -> usize {
    let data = data.trim_end_matches('=');
    data.len() * 3 / 4
}

/// tools/call 结果（`CallToolResult` 的 JSON）→ 工具结果。
/// text ⇒ 文本；image ⇒ provider 支持图片（`images`）时为图片块，否则占位文本 `[image: mime, N bytes]`；
/// audio ⇒ 占位文本；resource ⇒ 文本（`[resource: uri]` + 内容，二进制只给占位）；resource_link ⇒ `[resource link: name <uri>]`；
/// 没有 content 但有 structuredContent ⇒ JSON 文本；`isError: true` ⇒ 工具错误结果。
pub fn map_result(server: &str, tool: &str, result: &Value, images: bool) -> ToolResult {
    let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let mut content = vec![];
    let mut text_bytes = 0usize;
    let mut truncated = false;
    let mut push_text = |content: &mut Vec<Content>, text: String| {
        if truncated {
            return;
        }
        let room = MAX_RESULT_BYTES.saturating_sub(text_bytes);
        let text = if text.len() > room {
            truncated = true;
            let mut cut = room;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}\n[truncated: MCP result exceeds {MAX_RESULT_BYTES} bytes]", &text[..cut])
        } else {
            text
        };
        text_bytes += text.len();
        content.push(Content::Text { text });
    };
    for block in result.get("content").and_then(Value::as_array).into_iter().flatten() {
        let s = |k: &str| block.get(k).and_then(Value::as_str);
        match s("type").unwrap_or_default() {
            "text" => push_text(&mut content, s("text").unwrap_or_default().to_string()),
            "image" => {
                let (data, mime) = (s("data").unwrap_or_default(), s("mimeType").unwrap_or("application/octet-stream"));
                if images && !data.is_empty() && mime.starts_with("image/") {
                    content.push(Content::Image { data: data.to_string(), mime_type: mime.to_string() });
                } else {
                    push_text(&mut content, format!("[image: {mime}, {} bytes]", b64_len(data)));
                }
            }
            "audio" => push_text(&mut content, format!("[audio: {}, {} bytes]", s("mimeType").unwrap_or("application/octet-stream"), b64_len(s("data").unwrap_or_default()))),
            "resource" => {
                let r = block.get("resource").cloned().unwrap_or_default();
                let rs = |k: &str| r.get(k).and_then(Value::as_str);
                let uri = rs("uri").unwrap_or_default();
                match (rs("text"), rs("blob")) {
                    (Some(text), _) => push_text(&mut content, format!("[resource: {uri}]\n{text}")),
                    (None, Some(blob)) => push_text(&mut content, format!("[resource: {uri}, {}, {} bytes]", rs("mimeType").unwrap_or("application/octet-stream"), b64_len(blob))),
                    (None, None) => push_text(&mut content, format!("[resource: {uri}]")),
                }
            }
            "resource_link" => push_text(&mut content, format!("[resource link: {} <{}>]", s("name").or(s("title")).unwrap_or_default(), s("uri").unwrap_or_default())),
            other => push_text(&mut content, format!("[unsupported MCP content type '{other}']")),
        }
    }
    let structured = result.get("structuredContent").filter(|v| !v.is_null()).cloned();
    if content.is_empty() {
        match &structured {
            Some(v) => push_text(&mut content, serde_json::to_string(v).unwrap_or_default()),
            None if is_error => push_text(&mut content, format!("MCP tool '{tool}' on server '{server}' returned an error without details")),
            None => push_text(&mut content, String::new()),
        }
    }
    let mut details = json!({"mcp": {"server": server, "tool": tool}});
    if let Some(v) = structured {
        details["structuredContent"] = v;
    }
    if truncated {
        details["truncated"] = json!(true);
    }
    ToolResult { content, details, is_error, terminate: false }
}

/// 会话里的一个 MCP 工具。图片是否原样交给模型取决于会话 provider（只有 Anthropic 的工具结果支持图片）。
pub struct McpTool {
    pub(super) server: Arc<Server>,
    pub(super) info: ToolInfo,
    pub(super) images: bool,
}

#[async_trait::async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.info.exposed
    }
    fn description(&self) -> &str {
        &self.info.description
    }
    fn parameters(&self) -> Value {
        self.info.schema.clone()
    }
    async fn execute(&self, call_id: &str, args: Value, cancel: CancellationToken, _on_update: UpdateSink) -> anyhow::Result<ToolResult> {
        let result = self.server.call(&self.info.mcp_name, call_id, args, &cancel).await?;
        Ok(map_result(&self.server.name, &self.info.mcp_name, &result, self.images))
    }
}
