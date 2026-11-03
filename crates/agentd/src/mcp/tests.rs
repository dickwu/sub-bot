//! MCP 单元测试：配置校验、命名、schema 清洗、结果映射；streamable HTTP 传输用进程内的 rmcp 服务器（dev-dependency）。
//! stdio 子进程（假服务器 `sub-bot-fake-mcp`）的端到端测试见 tests/mcp_cli.rs。
use super::{config::*, tool::*, *};
use agent_ai::Content;
use rmcp::{
    model::{CallToolResult, ErrorData, ListToolsResult, PaginatedRequestParams, ServerConfig},
    service::{MaybeSendFuture, RequestContext},
    transport::streamable_http_server::{session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService},
    RoleServer, ServerHandler,
};
use std::future::Future;

fn cfg_from(value: Value) -> anyhow::Result<crate::config::Config> {
    let cfg: crate::config::Config = serde_json::from_value(value)?;
    cfg.validate()?;
    Ok(cfg)
}

fn profile(extra: Value) -> Value {
    let mut p = json!({"workspaceRoots": ["."]});
    p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    p
}

#[test]
fn config_parses_refs_inline_and_validates() {
    let cfg = cfg_from(json!({
        "mcpServers": {
            "fs": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/srv"], "env": {"TOKEN": "${HOME}"}, "tools": ["read_*"]},
            "github": {"url": "https://api.example.com/mcp/", "headersFromEnv": {"Authorization": "GITHUB_MCP_AUTH"}, "headers": {"X-Client": "sub-bot ${USER}"}, "callTimeoutSecs": 60, "required": true},
            "off": {"command": "x", "enabled": false}
        },
        "profiles": {
            "a": profile(json!({"mcpServers": ["fs", {"name": "github", "tools": ["get_issue"]}, {"name": "local", "command": "my-mcp", "startupTimeoutSecs": 5}]})),
            "b": profile(json!({}))
        }
    }))
    .unwrap();
    let plan = plan(&cfg.mcp_servers, &cfg.profiles).unwrap();
    assert_eq!(plan.servers.keys().collect::<Vec<_>>(), ["fs", "github", "local", "off"]);
    assert_eq!(plan.servers["local"].startup_timeout_secs, 5);
    assert_eq!(plan.servers["github"].call_timeout_secs, 60);
    assert_eq!((plan.servers["fs"].startup_timeout_secs, plan.servers["fs"].call_timeout_secs, plan.servers["fs"].required), (30, 120, false));
    assert_eq!(plan.servers["github"].transport(), "http");
    assert!(plan.servers["fs"].tool_allowed("read_file") && !plan.servers["fs"].tool_allowed("write_file"));
    assert_eq!(cfg.profiles["a"].mcp_servers[1], ProfileMcpServer::Ref { name: "github".into(), tools: Some(vec!["get_issue".into()]) });
    assert!(cfg.profiles["b"].mcp_servers.is_empty() && !plan.profiles.contains_key("b"));
    // 只启动被 profile 引用的服务器
    let manager = McpManager::new(&cfg, Reporter::noop()).unwrap();
    assert_eq!(manager.servers.keys().collect::<Vec<_>>(), ["fs", "github", "local"]);
    assert_eq!(manager.profile_servers("a"), ["fs", "github", "local"]);
    assert!(manager.profile_servers("b").is_empty());
    // Debug 不含 env / header 值
    let debug = format!("{:?}", plan.servers["github"]);
    assert!(debug.contains("Authorization") && !debug.contains("GITHUB_MCP_AUTH") && !debug.contains("${USER}"), "{debug}");
    assert!(!format!("{:?}", plan.servers["fs"]).contains("${HOME}"));
}

#[test]
fn config_rejects_invalid_definitions() {
    let bad = |servers: Value, profiles: Value| cfg_from(json!({"mcpServers": servers, "profiles": profiles})).unwrap_err().to_string();
    let p = |list: Value| json!({"p": profile(json!({"mcpServers": list}))});
    for name in ["a__b", "", "has space", "dot.ted", &"x".repeat(33)] {
        assert!(bad(json!({name: {"command": "x"}}), json!({})).contains("invalid server name"), "{name}");
    }
    assert!(bad(json!({"s": {"command": "x", "url": "https://h"}}), json!({})).contains("only one of command"));
    assert!(bad(json!({"s": {}}), json!({})).contains("command (stdio) or url"));
    assert!(bad(json!({"s": {"command": " "}}), json!({})).contains("must not be empty"));
    assert!(bad(json!({"s": {"url": "ftp://h"}}), json!({})).contains("http/https"));
    assert!(bad(json!({"s": {"url": "https://u:p@h"}}), json!({})).contains("credentials"));
    assert!(bad(json!({"s": {"url": "https://h", "headers": {"Accept": "x"}}}), json!({})).contains("managed by the MCP transport"));
    assert!(bad(json!({"s": {"url": "https://h", "headers": {"bad header": "x"}}}), json!({})).contains("invalid header name"));
    assert!(bad(json!({"s": {"url": "https://h", "headers": {"A": "${UNCLOSED"}}}), json!({})).contains("unterminated"));
    assert!(bad(json!({"s": {"url": "https://h", "headersFromEnv": {"A": "not-a-var"}}}), json!({})).contains("environment variable name"));
    assert!(bad(json!({"s": {"url": "https://h", "headers": {"a": "x"}, "headersFromEnv": {"A": "V"}}}), json!({})).contains("both headers and headersFromEnv"));
    assert!(bad(json!({"s": {"command": "x", "headers": {"A": "x"}}}), json!({})).contains("only valid for url servers"));
    assert!(bad(json!({"s": {"url": "https://h", "args": ["x"]}}), json!({})).contains("only valid for command servers"));
    assert!(bad(json!({"s": {"command": "x", "env": {"1BAD": "x"}}}), json!({})).contains("env name"));
    assert!(bad(json!({"s": {"command": "x", "startupTimeoutSecs": 0}}), json!({})).contains("startupTimeoutSecs"));
    assert!(bad(json!({"s": {"command": "x", "callTimeoutSecs": 3601}}), json!({})).contains("callTimeoutSecs"));
    assert!(bad(json!({"s": {"command": "x", "tools": [""]}}), json!({})).contains("invalid tools pattern"));
    assert!(serde_json::from_value::<crate::config::Config>(json!({"mcpServers": {"s": {"command": "x", "typo": 1}}, "profiles": {}})).is_err());
    // profile 引用
    assert!(bad(json!({}), p(json!(["missing"]))).contains("unknown server 'missing'"));
    assert!(bad(json!({"s": {"command": "x"}}), p(json!(["s", "s"]))).contains("duplicate server"));
    assert!(bad(json!({"s": {"command": "x"}}), p(json!([{"name": "s", "command": "y"}]))).contains("collides with the global"));
    let two = json!({"p": profile(json!({"mcpServers": [{"name": "i", "command": "y"}]})), "q": profile(json!({"mcpServers": [{"name": "i", "command": "y"}]}))});
    assert!(bad(json!({}), two).contains("already defined by profile 'p'"));
    assert!(serde_json::from_value::<crate::config::Config>(json!({"profiles": p(json!([{"name": "s", "args": ["x"]}]))})).unwrap_err().to_string().contains("only valid in an inline definition"));
    assert!(serde_json::from_value::<crate::config::Config>(json!({"profiles": p(json!([42]))})).is_err());
}

#[test]
fn relative_cwd_resolves_against_config_dir() {
    let dir = std::env::temp_dir().join(format!("sub-bot-mcp-cfg-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        json!({"mcpServers": {"g": {"command": "x", "cwd": "work"}}, "profiles": {"p": {"workspaceRoots": ["."], "mcpServers": [{"name": "i", "command": "y", "cwd": "/abs"}, "g"]}}}).to_string(),
    )
    .unwrap();
    let cfg = crate::config::Config::load(&path).unwrap();
    assert_eq!(cfg.mcp_servers["g"].cwd.as_deref(), Some(dir.join("work").as_path()));
    let ProfileMcpServer::Inline { server, .. } = &cfg.profiles["p"].mcp_servers[0] else { panic!("inline") };
    assert_eq!(server.cwd.as_deref(), Some(std::path::Path::new("/abs")));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn env_reference_expansion() {
    let env = |v: &str| (v == "TOKEN").then(|| "s3cret".to_string());
    assert_eq!(expand("Bearer ${TOKEN}", env).unwrap(), "Bearer s3cret");
    assert_eq!(expand("a$b ${TOKEN}${TOKEN}", env).unwrap(), "a$b s3crets3cret");
    assert_eq!(expand("${MISSING}", env).unwrap_err(), "environment variable MISSING is not set");
    assert!(expand("${1X}", env).is_err() && expand("${TOKEN", env).is_err());
}

#[test]
fn tool_names_are_namespaced_sanitized_and_bounded() {
    let valid = |n: &str| !n.is_empty() && n.len() <= MAX_TOOL_NAME_LEN && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    assert_eq!(exposed_name("github", "get_issue"), "mcp__github__get_issue");
    assert_eq!(exposed_name("fs", "files.read/v2"), "mcp__fs__files_read_v2");
    let long = "t".repeat(100);
    let a = exposed_name("srv", &long);
    assert!(valid(&a) && a.len() == MAX_TOOL_NAME_LEN && a.starts_with("mcp__srv__ttt"), "{a}");
    // 截断后仍可区分：哈希来自完整原名
    let b = exposed_name("srv", &format!("{long}x"));
    assert!(valid(&b) && a != b);
    assert_eq!(exposed_name("srv", &long), a, "deterministic");
    // 清洗后撞名 ⇒ 后来者加哈希
    let names = assign_names("s", &["a.b".into(), "a_b".into(), "c".into()]);
    assert_eq!(names[0], "mcp__s__a_b");
    assert!(names[1].starts_with("mcp__s__a_b_") && names[1] != names[0] && valid(&names[1]));
    assert_eq!(names[2], "mcp__s__c");
    let max = format!("mcp__{}__{}", "s".repeat(MAX_SERVER_NAME_LEN), "x".repeat(40));
    assert!(valid(&exposed_name(&"s".repeat(MAX_SERVER_NAME_LEN), &"x".repeat(40))) && max.len() > MAX_TOOL_NAME_LEN);
}

#[test]
fn schemas_are_sanitized_for_openai_and_anthropic() {
    assert_eq!(
        sanitize_schema(&json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]})),
        json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]})
    );
    assert_eq!(sanitize_schema(&json!({})), json!({"type": "object", "properties": {}}));
    assert_eq!(sanitize_schema(&Value::Null), json!({"type": "object", "properties": {}, "additionalProperties": true}));
    assert_eq!(sanitize_schema(&json!({"type": "string"})), json!({"type": "object", "properties": {}, "additionalProperties": true}));
    let messy = json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": ["object", "null"], "properties": {"x": {"type": "number", "anyOf": [{"minimum": 0}]}}, "anyOf": [{"required": ["x"]}], "oneOf": [], "required": ["x", 5], "$defs": {"d": {"type": "string"}}});
    let clean = sanitize_schema(&messy);
    assert_eq!(
        clean,
        json!({"type": "object", "properties": {"x": {"type": "number", "anyOf": [{"minimum": 0}]}}, "required": ["x"], "$defs": {"d": {"type": "string"}}}),
        "nested combinators are kept"
    );
    assert_eq!(sanitize_schema(&json!({"type": "object", "properties": [], "required": "x"})), json!({"type": "object", "properties": {}}));
}

#[test]
fn call_results_map_to_tool_results() {
    let r = map_result("s", "t", &json!({"content": [{"type": "text", "text": "hello"}, {"type": "text", "text": "world"}]}), false);
    assert!(!r.is_error);
    assert_eq!(r.content, vec![Content::Text { text: "hello".into() }, Content::Text { text: "world".into() }]);
    assert_eq!(r.details["mcp"], json!({"server": "s", "tool": "t"}));
    let r = map_result("s", "t", &json!({"content": [{"type": "text", "text": "bad input"}], "isError": true}), false);
    assert!(r.is_error && r.content == vec![Content::Text { text: "bad input".into() }]);
    let r = map_result("s", "t", &json!({"isError": true}), false);
    assert!(r.is_error && matches!(&r.content[0], Content::Text { text } if text.contains("returned an error")));
    let image = json!({"content": [{"type": "image", "data": "aGVsbG8gd29ybGQ=", "mimeType": "image/png"}]});
    assert_eq!(map_result("s", "t", &image, false).content, vec![Content::Text { text: "[image: image/png, 11 bytes]".into() }]);
    assert_eq!(map_result("s", "t", &image, true).content, vec![Content::Image { data: "aGVsbG8gd29ybGQ=".into(), mime_type: "image/png".into() }]);
    let r = map_result(
        "s",
        "t",
        &json!({"content": [
            {"type": "resource", "resource": {"uri": "file:///a.txt", "text": "contents"}},
            {"type": "resource", "resource": {"uri": "file:///b.bin", "mimeType": "application/zip", "blob": "AAAA"}},
            {"type": "resource_link", "uri": "https://x/y", "name": "y"},
            {"type": "audio", "data": "AAAA", "mimeType": "audio/wav"},
            {"type": "weird"}
        ]}),
        true,
    );
    let texts: Vec<String> = r.content.iter().map(|c| if let Content::Text { text } = c { text.clone() } else { String::new() }).collect();
    assert_eq!(
        texts,
        [
            "[resource: file:///a.txt]\ncontents",
            "[resource: file:///b.bin, application/zip, 3 bytes]",
            "[resource link: y <https://x/y>]",
            "[audio: audio/wav, 3 bytes]",
            "[unsupported MCP content type 'weird']"
        ]
    );
    let r = map_result("s", "t", &json!({"content": [], "structuredContent": {"n": 1}}), false);
    assert_eq!(r.content, vec![Content::Text { text: r#"{"n":1}"#.into() }]);
    assert_eq!(r.details["structuredContent"], json!({"n": 1}));
    let big = "é".repeat(MAX_RESULT_BYTES);
    let r = map_result("s", "t", &json!({"content": [{"type": "text", "text": big}, {"type": "text", "text": "dropped"}]}), false);
    assert_eq!(r.content.len(), 1);
    assert!(matches!(&r.content[0], Content::Text { text } if text.ends_with("bytes]") && text.len() < MAX_RESULT_BYTES + 100));
    assert_eq!(r.details["truncated"], json!(true));
}

#[tokio::test]
async fn restart_backoff_and_limit() {
    let server = Server::new("s".into(), McpServerConfig { command: Some("x".into()), ..Default::default() }, Reporter::noop());
    assert_eq!(server.restart_delay().unwrap(), Duration::ZERO);
    for (failures, secs) in [(1, 1), (2, 2), (3, 5), (7, 5)] {
        let mut st = server.state.lock().unwrap();
        st.failures = failures;
        st.last_failure = Some(Instant::now());
        drop(st);
        let d = server.restart_delay().unwrap();
        assert!(d <= Duration::from_secs(secs) && d > Duration::from_secs(secs) - Duration::from_millis(500), "{failures}: {d:?}");
    }
    server.state.lock().unwrap().attempts = std::iter::repeat_n(Instant::now(), MAX_RESTARTS).collect();
    assert!(server.restart_delay().unwrap_err().contains("restart limit reached (5 attempts in 5 min)"));
}

#[tokio::test]
async fn non_required_failures_warn_and_continue_required_fail_startup() {
    let reporter = Reporter::new(&crate::config::AlertsConfig::default(), None, vec![]);
    let missing = "/nonexistent/sub-bot-mcp-test-binary";
    let cfg = cfg_from(json!({"mcpServers": {"opt": {"command": missing}, "env": {"url": "http://127.0.0.1:9/mcp", "headersFromEnv": {"Authorization": "SUB_BOT_MCP_TEST_UNSET_VAR"}}},
        "profiles": {"p": profile(json!({"mcpServers": ["opt", "env"]}))}}))
    .unwrap();
    let manager = McpManager::new(&cfg, reporter.clone()).unwrap();
    manager.start().await.unwrap();
    let status = manager.status(None, false);
    assert!(status.iter().all(|s| s["status"] == "failed"), "{status:?}");
    assert!(status[0]["lastError"].as_str().unwrap().contains("environment variable SUB_BOT_MCP_TEST_UNSET_VAR (headersFromEnv.Authorization) is not set"), "{status:?}");
    assert!(status[1]["lastError"].as_str().unwrap().contains("cannot spawn"), "{status:?}");
    assert!(manager.session_tools("p", &[], false).unwrap().is_empty());
    reporter.flush(Duration::from_secs(1)).await;
    let issues = reporter.recent(None, 0, 10);
    assert_eq!(issues.iter().filter(|i| i.kind == "mcp_server_failed" && i.severity == Severity::Warn).count(), 2, "{issues:?}");

    let mut cfg = cfg;
    cfg.mcp_servers.get_mut("opt").unwrap().required = true;
    let err = McpManager::new(&cfg, Reporter::noop()).unwrap().start().await.unwrap_err();
    assert_eq!(err.server, "opt");
    assert!(err.to_string().starts_with("required MCP server 'opt' failed to start: cannot spawn"), "{err}");
}

// ---------- streamable HTTP：进程内 rmcp 服务器 ----------

#[derive(Clone, Default)]
struct HttpFake {
    changed: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

fn call_result(v: Value) -> Result<rmcp::model::CallToolResponse, ErrorData> {
    Ok(serde_json::from_value::<CallToolResult>(v).unwrap().into())
}

impl ServerHandler for HttpFake {
    fn get_info(&self) -> ServerConfig {
        serde_json::from_value(json!({"protocolVersion": "2025-11-25", "capabilities": {"tools": {"listChanged": true}}, "serverInfo": {"name": "http-fake", "version": "0.4.2"}})).unwrap()
    }

    fn list_tools(&self, _r: Option<PaginatedRequestParams>, _c: RequestContext<RoleServer>) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        let mut tools = vec![
            json!({"name": "echo", "description": "echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}),
            json!({"name": "fail", "inputSchema": {"type": "object"}}),
            json!({"name": "slow", "inputSchema": {"type": "object"}}),
            json!({"name": "change", "inputSchema": {"type": "object"}}),
            json!({"name": "secret_tool", "inputSchema": {"type": "object"}}),
        ];
        if self.changed.load(Ordering::SeqCst) {
            tools.push(json!({"name": "added_later", "inputSchema": {}}));
        }
        std::future::ready(Ok(serde_json::from_value(json!({"tools": tools})).unwrap()))
    }

    async fn call_tool(&self, req: CallToolRequestParams, ctx: RequestContext<RoleServer>) -> Result<rmcp::model::CallToolResponse, ErrorData> {
        let args = Value::Object(req.arguments.clone().unwrap_or_default());
        match req.name.as_ref() {
            "echo" => call_result(json!({"content": [{"type": "text", "text": args["text"]}]})),
            "fail" => call_result(json!({"content": [{"type": "text", "text": "http failure"}], "isError": true})),
            "slow" => {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                    _ = ctx.ct.cancelled() => self.cancelled.store(true, Ordering::SeqCst),
                }
                call_result(json!({"content": [{"type": "text", "text": "slow done"}]}))
            }
            "change" => {
                self.changed.store(true, Ordering::SeqCst);
                let _ = ctx.peer.notify_tool_list_changed().await;
                call_result(json!({"content": [{"type": "text", "text": "changed"}]}))
            }
            other => Err(ErrorData::invalid_params(format!("unknown tool {other}"), None)),
        }
    }
}

/// 要求 `x-api-key: k3y-from-env-value`（来自 headersFromEnv）与 `x-static: static-1`（headers）。
async fn require_headers(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::response::IntoResponse;
    let h = req.headers();
    if h.get("x-api-key").and_then(|v| v.to_str().ok()) != Some("k3y-from-env-value") || h.get("x-static").and_then(|v| v.to_str().ok()) != Some("static-1") {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}

struct HttpServer {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl HttpServer {
    async fn start(listener: tokio::net::TcpListener, fake: HttpFake) -> Self {
        let stop = CancellationToken::new();
        let mut config = StreamableHttpServerConfig::default();
        config.cancellation_token = stop.child_token();
        let service = StreamableHttpService::new(move || Ok(fake.clone()), Arc::new(LocalSessionManager::default()), config);
        let app = axum::Router::new().nest_service("/mcp", service).layer(axum::middleware::from_fn(require_headers));
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).with_graceful_shutdown(shutdown.cancelled_owned()).await;
        });
        Self { stop, task }
    }
    async fn stop(self) {
        self.stop.cancel();
        if tokio::time::timeout(Duration::from_secs(3), &mut { self.task }).await.is_err() {
            // 打开的 SSE 连接可能让优雅关闭一直等待；测试只需要端口不再响应
        }
    }
}

#[tokio::test]
async fn http_transport_lists_calls_refreshes_and_reconnects() {
    // 用唯一的变量名，避免与并行测试冲突
    let var = format!("SUB_BOT_MCP_TEST_KEY_{}", std::process::id());
    std::env::set_var(&var, "k3y-from-env-value");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fake = HttpFake::default();
    let server = HttpServer::start(listener, fake.clone()).await;
    let reporter = Reporter::new(&crate::config::AlertsConfig::default(), None, vec![]);
    let cfg = cfg_from(json!({
        "mcpServers": {"web": {"url": format!("http://{addr}/mcp"), "headers": {"X-Static": "static-1"}, "headersFromEnv": {"X-Api-Key": var}, "tools": ["echo", "fail", "slow", "change", "added_*"], "callTimeoutSecs": 2}},
        "profiles": {"p": profile(json!({"mcpServers": ["web"]})), "narrow": profile(json!({"mcpServers": [{"name": "web", "tools": ["echo"]}]}))}
    }))
    .unwrap();
    let manager = McpManager::new(&cfg, reporter.clone()).unwrap();
    manager.start().await.unwrap();
    let status = &manager.status(None, true)[0];
    assert_eq!(
        (status["status"].as_str(), status["transport"].as_str(), status["serverName"].as_str(), status["serverVersion"].as_str()),
        (Some("ready"), Some("http"), Some("http-fake"), Some("0.4.2")),
        "{status}"
    );
    // allow-list 过滤掉 secret_tool；命名空间
    assert_eq!(manager.profile_tool_names("p"), ["mcp__web__echo", "mcp__web__fail", "mcp__web__slow", "mcp__web__change"]);
    assert_eq!(manager.profile_tool_names("narrow"), ["mcp__web__echo"]);
    // 调用方的 tools 选择
    let names = |tools: Vec<Arc<dyn AgentTool>>| tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>();
    assert_eq!(names(manager.session_tools("p", &["mcp__web__f*".into()], false).unwrap()), ["mcp__web__fail"]);
    assert!(manager.session_tools("p", &["mcp__other__x".into()], false).err().unwrap().to_string().contains("not enabled"));
    let tools = manager.session_tools("p", &[], false).unwrap();
    let tool = |n: &str| tools.iter().find(|t| t.name() == n).unwrap().clone();
    let noop: agent_core::UpdateSink = Arc::new(|_| {});
    let echo = tool("mcp__web__echo");
    assert_eq!(echo.parameters(), json!({"type": "object", "properties": {"text": {"type": "string"}}}));
    let r = echo.execute("c1", json!({"text": "over http"}), CancellationToken::new(), noop.clone()).await.unwrap();
    assert_eq!((r.content, r.is_error), (vec![Content::Text { text: "over http".into() }], false));
    let r = tool("mcp__web__fail").execute("c2", json!({}), CancellationToken::new(), noop.clone()).await.unwrap();
    assert!(r.is_error && r.content == vec![Content::Text { text: "http failure".into() }]);
    // 超时 ⇒ 错误，并通知服务器取消
    let err = tool("mcp__web__slow").execute("c3", json!({}), CancellationToken::new(), noop.clone()).await.unwrap_err();
    assert!(err.to_string().contains("timed out after 2s"), "{err}");
    for _ in 0..50 {
        if fake.cancelled.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(fake.cancelled.load(Ordering::SeqCst), "notifications/cancelled was not delivered");
    // 会话取消
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c.cancel();
    });
    assert_eq!(tool("mcp__web__slow").execute("c4", json!({}), cancel, noop.clone()).await.unwrap_err().to_string(), "aborted");
    // tools/list_changed ⇒ 刷新
    tool("mcp__web__change").execute("c5", json!({}), CancellationToken::new(), noop.clone()).await.unwrap();
    for _ in 0..50 {
        if manager.profile_tool_names("p").contains(&"mcp__web__added_later".to_string()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(manager.profile_tool_names("p").contains(&"mcp__web__added_later".to_string()), "{:?}", manager.profile_tool_names("p"));

    // 连接失败：服务器停掉 ⇒ 明确错误 + mcp_server_crashed；重新起服务器后下一次调用重连
    server.stop().await;
    let err = echo.execute("c6", json!({"text": "x"}), CancellationToken::new(), noop.clone()).await.unwrap_err().to_string();
    assert!(err.starts_with("MCP server 'web' connection failed (Client error: error sending request") && err.ends_with("; restarting"), "{err}");
    reporter.flush(Duration::from_secs(1)).await;
    assert!(reporter.recent(None, 0, 20).iter().any(|i| i.kind == "mcp_server_crashed"));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let server = HttpServer::start(listener, HttpFake::default()).await;
    let r = echo.execute("c7", json!({"text": "back"}), CancellationToken::new(), noop.clone()).await.unwrap();
    assert_eq!(r.content, vec![Content::Text { text: "back".into() }]);
    let status = &manager.status(None, false)[0];
    assert_eq!((status["status"].as_str(), status["restarts"].as_u64()), (Some("ready"), Some(1)), "{status}");
    // 请求头的值从不出现在状态 / issue 里
    assert!(!status.to_string().contains("k3y-from-env-value"));
    reporter.flush(Duration::from_secs(1)).await;
    assert!(!format!("{:?}", reporter.recent(None, 0, 50)).contains("k3y-from-env-value"));
    manager.shutdown().await;
    server.stop().await;
    std::env::remove_var(&var);
}

async fn api(app: &axum::Router, method: &str, path: &str, token: &str, body: Value) -> (u16, Value) {
    use tower::ServiceExt as _;
    let req = axum::http::Request::builder().method(method).uri(path).header("content-type", "application/json").header("authorization", format!("Bearer {token}"));
    let response = app.clone().oneshot(req.body(axum::body::Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Hub 集成：会话工具列表、调用方 `tools` 选择 MCP 工具、GET /v1/mcp 与 GET /v1/profiles 的可见性。
#[tokio::test]
async fn hub_sessions_and_http_endpoints_expose_mcp_tools() {
    let var = format!("SUB_BOT_MCP_TEST_HUB_KEY_{}", std::process::id());
    std::env::set_var(&var, "k3y-from-env-value");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = HttpServer::start(listener, HttpFake::default()).await;
    let mut cfg = crate::config::Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
    cfg.data_dir = Some(std::env::temp_dir().join(format!("sub-bot-mcp-hub-{}", uuid::Uuid::new_v4())));
    cfg.mcp_servers.insert(
        "web".into(),
        serde_json::from_value(json!({"url": format!("http://{addr}/mcp"), "headers": {"X-Static": "static-1"}, "headersFromEnv": {"X-Api-Key": var}, "tools": ["echo", "fail"]})).unwrap(),
    );
    let mut plain = cfg.profiles["default"].clone();
    plain.load_context_files = false;
    let default = cfg.profiles.get_mut("default").unwrap();
    default.load_context_files = false;
    default.mcp_servers = vec![ProfileMcpServer::Ref { name: "web".into(), tools: None }];
    cfg.profiles.insert("plain".into(), plain);
    cfg.clients.push(crate::config::Client { name: "other".into(), token_sha256: crate::config::sha256_hex("other-token"), profiles: vec!["plain".into()], ..Default::default() });
    let hub = crate::hub::Hub::new(cfg, "faux", None, None).unwrap();
    hub.mcp.start().await.unwrap();
    let app = crate::http::app(hub.clone());

    let (status, body) = api(&app, "GET", "/v1/mcp", "test-token", json!({})).await;
    assert_eq!(status, 200);
    assert_eq!(body["profiles"], json!({"default": ["web"]}));
    let web = &body["servers"][0];
    assert_eq!((web["name"].as_str(), web["status"].as_str(), web["toolCount"].as_u64()), (Some("web"), Some("ready"), Some(2)), "{body}");
    assert_eq!(web["tools"][0], json!({"name": "mcp__web__echo", "mcpName": "echo", "description": "echo"}));
    assert!(!body.to_string().contains("k3y") && !body.to_string().contains(&addr.to_string()), "no URLs / header values: {body}");
    // 其它 client 看不到不属于它的 profile 的服务器
    let (_, body) = api(&app, "GET", "/v1/mcp", "other-token", json!({})).await;
    assert_eq!(body, json!({"servers": [], "profiles": {}}));

    let (_, body) = api(&app, "GET", "/v1/profiles", "test-token", json!({})).await;
    let tools = body["profiles"][0]["tools"].as_array().unwrap();
    assert!(tools.contains(&json!("read")) && tools.contains(&json!("mcp__web__echo")) && tools.contains(&json!("mcp__web__fail")), "{body}");
    assert_eq!(body["profiles"][0]["mcpServers"], json!(["web"]));

    let (status, body) = api(&app, "POST", "/v1/sessions", "test-token", json!({})).await;
    assert_eq!(status, 201, "{body}");
    let tools = body["tools"].as_array().unwrap();
    assert!(tools.contains(&json!("bash")) && tools.contains(&json!("mcp__web__echo")) && tools.contains(&json!("mcp__web__fail")), "{body}");
    let (status, body) = api(&app, "POST", "/v1/sessions", "test-token", json!({"tools": ["read", "mcp__web__echo"]})).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["tools"], json!(["read", "mcp__web__echo"]));
    let (status, body) = api(&app, "POST", "/v1/sessions", "test-token", json!({"tools": ["mcp__nope__x"]})).await;
    assert_eq!((status, body["error"].as_str()), (400, Some("tool 'mcp__nope__x' is not enabled in profile 'default'")));
    let (status, body) =
        api(&app, "POST", "/v1/sessions", "test-token", json!({"callerTools": [{"name": "mcp__web__echo", "description": "", "parameters": {}, "callbackUrl": "http://127.0.0.1:9/x"}]})).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("collides with an MCP tool"), "{body}");
    // 会话里调用 MCP 工具（faux provider：/tool …）
    let session = hub.create_session(&hub.config.clients[0], crate::hub::SessionRequest::default()).unwrap();
    let mut events = session.subscribe();
    session.submit(r#"/tool mcp__web__echo {"text":"via hub"}"#.into(), None);
    let text = loop {
        match tokio::time::timeout(Duration::from_secs(10), events.recv()).await.unwrap().unwrap() {
            agent_core::AgentEvent::ToolExecutionEnd { result, is_error, .. } => {
                assert!(!is_error);
                break result.content;
            }
            _ => continue,
        }
    };
    assert_eq!(text, vec![Content::Text { text: "via hub".into() }]);
    hub.mcp.shutdown().await;
    server.stop().await;
    std::env::remove_var(&var);
}

#[test]
fn readme_and_example_configs_validate() {
    let readme = include_str!("../../../../README.md");
    let section = &readme[readme.find("## MCP 服务器").expect("README section")..];
    let start = section.find("```json\n").unwrap() + "```json\n".len();
    let mut cfg: crate::config::Config = serde_json::from_str(&section[start..start + section[start..].find("```").unwrap()]).unwrap();
    for client in &mut cfg.clients {
        client.token_sha256 = crate::config::sha256_hex(&client.name);
    }
    cfg.validate().unwrap();
    let manager = McpManager::new(&cfg, Reporter::noop()).unwrap();
    assert_eq!(manager.profile_servers("coder"), ["fs", "internal", "github", "local"]);
    assert!(manager.profile_servers("reviewer").is_empty());
    let approval = cfg.tool_approval_of(&cfg.profiles["coder"]);
    let verdict = |tool: &str| approval.evaluate(tool, &json!({}));
    assert!(matches!(verdict("mcp__internal__anything"), agent_core::Verdict::Ask { .. }));
    assert!(matches!(verdict("mcp__fs__read_file"), agent_core::Verdict::Allow { .. }));
    assert!(matches!(verdict("mcp__github__get_issue"), agent_core::Verdict::Allow { .. }));

    let mut example: crate::config::Config = serde_json::from_str(include_str!("../../../../sub-bot.example.json")).unwrap();
    for client in &mut example.clients {
        client.token_sha256 = crate::config::sha256_hex(&client.name);
    }
    example.validate().unwrap();
    assert_eq!(McpManager::new(&example, Reporter::noop()).unwrap().profile_servers("coder"), ["fs", "github"]);
    assert_eq!(example.mcp_servers["github"].headers_from_env["Authorization"], "GITHUB_MCP_AUTHORIZATION");
}
