//! 测试用的假 MCP 服务器（stdio，手写 JSON-RPC 2.0，只依赖 serde_json）。只供 tests/mcp_cli.rs 使用。
//!
//! 参数：`--name <serverInfo.name>`、`--page-size <n>`（tools/list 分页，默认 3）、`--fail-startup`（写 stderr 后 exit 2）。
//! 环境变量 `FAKE_MCP_LOG=<file>`：把收到的 `notifications/cancelled` 追加写入该文件。
//!
//! 工具：echo、fail（isError）、crash（stderr 后 exit 3）、image、slow（后台线程 sleep，可被取消）、untyped（不规范 schema）、
//! dotted.name、超长名字、hidden（用于 allow-list）、change_tools（发送 tools/list_changed 并新增 added_later）、env（读环境变量）。
use serde_json::{json, Value};
use std::{
    io::{BufRead, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

const LONG_NAME: &str = "a_really_long_tool_name_that_goes_well_beyond_the_provider_limit_of_sixty_four_characters";

fn send(out: &Mutex<std::io::Stdout>, msg: &Value) {
    let mut out = out.lock().unwrap();
    let _ = writeln!(out, "{msg}");
    let _ = out.flush();
}

fn tools(changed: bool) -> Vec<Value> {
    let text_schema = json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]});
    let mut list = vec![
        json!({"name": "echo", "description": "Echo the text back", "inputSchema": text_schema}),
        json!({"name": "fail", "description": "Always fails", "inputSchema": {"type": "object", "properties": {}}}),
        json!({"name": "crash", "description": "Exit the server process mid-call", "inputSchema": {"type": "object"}}),
        json!({"name": "image", "description": "Return an image", "inputSchema": {"type": "object"}}),
        json!({"name": "slow", "description": "Sleep for secs seconds", "inputSchema": {"type": "object", "properties": {"secs": {"type": "number"}}}}),
        json!({"name": "untyped", "description": "Schema without a top-level type", "inputSchema": {"$schema": "https://json-schema.org/draft/2020-12/schema", "properties": {"x": {"type": "number"}}, "anyOf": [{"required": ["x"]}], "required": ["x", 5]}}),
        json!({"name": "dotted.name", "inputSchema": {"type": "object"}}),
        json!({"name": LONG_NAME, "description": "Long name", "inputSchema": {"type": "object"}}),
        json!({"name": "hidden", "description": "Filtered by allow-lists", "inputSchema": {"type": "object"}}),
        json!({"name": "change_tools", "description": "Add a tool and notify", "inputSchema": {"type": "object"}}),
        json!({"name": "env", "description": "Read an environment variable", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}}}),
    ];
    if changed {
        list.push(json!({"name": "added_later", "description": "Added after list_changed", "inputSchema": {"type": "object"}}));
    }
    list
}

fn text(t: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": t.into()}]})
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    if args.iter().any(|a| a == "--fail-startup") {
        eprintln!("fake-mcp: simulated startup failure");
        std::process::exit(2);
    }
    let name = arg("--name").unwrap_or_else(|| "fake-mcp".into());
    let page: usize = arg("--page-size").and_then(|p| p.parse().ok()).unwrap_or(3);
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let changed = Arc::new(AtomicBool::new(false));
    eprintln!("fake-mcp: started (pid {})", std::process::id());
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            eprintln!("fake-mcp: bad line: {line}");
            continue;
        };
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let Some(id) = msg.get("id").cloned() else {
            if method == "notifications/cancelled" {
                eprintln!("fake-mcp: cancelled {}", msg["params"]["requestId"]);
                if let Ok(path) = std::env::var("FAKE_MCP_LOG") {
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                        let _ = writeln!(f, "cancelled {}", msg["params"]["requestId"]);
                    }
                }
            }
            continue;
        };
        let params = &msg["params"];
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let response = match method.as_str() {
            "initialize" => reply(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": name, "version": "1.2.3"},
            })),
            "ping" => reply(json!({})),
            "tools/list" => {
                let all = tools(changed.load(Ordering::SeqCst));
                let start: usize = params["cursor"].as_str().and_then(|c| c.parse().ok()).unwrap_or(0);
                let end = (start + page).min(all.len());
                let mut result = json!({"tools": all[start..end]});
                if end < all.len() {
                    result["nextCursor"] = json!(end.to_string());
                }
                reply(result)
            }
            "tools/call" => {
                let a = &params["arguments"];
                match params["name"].as_str().unwrap_or_default() {
                    "echo" => reply(text(a["text"].as_str().unwrap_or_default())),
                    "fail" => reply(json!({"content": [{"type": "text", "text": "fake failure: something went wrong"}], "isError": true})),
                    "crash" => {
                        eprintln!("fake-mcp: crashing on purpose");
                        std::process::exit(3);
                    }
                    "image" => reply(json!({"content": [{"type": "text", "text": "here is an image"}, {"type": "image", "data": "aGVsbG8gd29ybGQ=", "mimeType": "image/png"}]})),
                    "slow" => {
                        let secs = a["secs"].as_f64().unwrap_or(30.0);
                        let (out, id) = (out.clone(), id.clone());
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs_f64(secs));
                            send(&out, &json!({"jsonrpc": "2.0", "id": id, "result": text("slow done")}));
                        });
                        continue;
                    }
                    "change_tools" => {
                        changed.store(true, Ordering::SeqCst);
                        send(&out, &json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
                        reply(text("tools changed"))
                    }
                    "env" => reply(text(std::env::var(a["name"].as_str().unwrap_or_default()).unwrap_or_else(|_| "<unset>".into()))),
                    other => reply(text(format!("called {other}"))),
                }
            }
            other => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {other}")}}),
        };
        send(&out, &response);
    }
}
