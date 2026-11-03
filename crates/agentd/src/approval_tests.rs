//! 工具调用审批的服务侧测试：HTTP 端点（200/404/409/owner 隔离）、会话视图、会话级收紧、SSE、同步/异步 chat、
//! webhook、approval_timeout issue 与 RPC `approve`。都用 faux provider（`/tool <name> <json>`）。
use crate::{
    config::{Client, Config},
    hub::{Hub, SessionRequest},
    issues::Reporter,
};
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::StatusCode,
    Router,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

fn config(policy: Option<Value>) -> Config {
    let mut cfg = Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
    cfg.data_dir = Some(std::env::temp_dir().join(format!("sub-bot-approval-{}", uuid::Uuid::new_v4())));
    let p = cfg.profiles.get_mut("default").unwrap();
    p.load_context_files = false;
    p.tool_approval = policy.map(|v| serde_json::from_value(v).unwrap());
    cfg.clients.push(Client { name: "other".into(), token_sha256: crate::config::sha256_hex("other-token"), profiles: vec!["default".into()], ..Default::default() });
    cfg
}

fn test_app(policy: Option<Value>) -> (Router, Arc<Hub>) {
    let hub = Hub::new(config(policy), "faux", None, None).unwrap();
    (crate::http::test_router(hub.clone()), hub)
}

async fn request(app: &Router, method: &str, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(path).header("content-type", "application/json").header("authorization", format!("Bearer {token}"));
    let response = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn wait_pending(app: &Router, session: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (status, body) = request(app, "GET", &format!("/v1/sessions/{session}/approvals"), "test-token", json!({})).await;
            assert_eq!(status, StatusCode::OK);
            if let Some(first) = body["approvals"].as_array().and_then(|a| a.first()) {
                return first.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no pending approval")
}

async fn wait_idle(app: &Router, session: &str) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (_, view) = request(app, "GET", &format!("/v1/sessions/{session}"), "test-token", json!({})).await;
            if view["isStreaming"] == false {
                let (_, messages) = request(app, "GET", &format!("/v1/sessions/{session}/messages"), "test-token", json!({})).await;
                return messages["messages"].as_array().unwrap().clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session did not finish")
}

fn texts(messages: &[Value], role: &str) -> Vec<String> {
    messages.iter().filter(|m| m["role"] == role).map(|m| m["content"].as_array().unwrap().iter().filter_map(|c| c["text"].as_str()).collect::<String>()).collect()
}

const BASH: &str = r#"/tool bash {"command":"printf approved-output"}"#;

#[tokio::test]
async fn http_approve_200_404_409_and_owner_isolation() {
    let (app, _hub) = test_app(Some(json!({"tools": {"bash": "ask"}})));
    let (status, view) = request(&app, "POST", "/v1/sessions", "test-token", json!({})).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(view["pendingApprovals"], json!([]));
    let sid = view["sessionId"].as_str().unwrap().to_string();
    let (status, profiles) = request(&app, "GET", "/v1/profiles", "test-token", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(profiles["profiles"][0]["toolApproval"]["tools"], json!({"bash": "ask"}));
    assert_eq!(request(&app, "POST", &format!("/v1/sessions/{sid}/prompt"), "test-token", json!({"message": BASH})).await.0, StatusCode::ACCEPTED);
    let pending = wait_pending(&app, &sid).await;
    assert_eq!(pending["tool"], "bash");
    assert_eq!(pending["args"], json!({"command": "printf approved-output"}));
    assert_eq!(pending["reason"], "tool 'bash' requires approval (tools rule 'bash')");
    assert_eq!(pending["timeoutSecs"], 300);
    let call = pending["callId"].as_str().unwrap().to_string();
    let (_, view) = request(&app, "GET", &format!("/v1/sessions/{sid}"), "test-token", json!({})).await;
    assert_eq!(view["pendingApprovals"][0]["callId"], call.as_str());
    assert_eq!(view["isStreaming"], true);

    let path = format!("/v1/sessions/{sid}/approvals/{call}");
    // owner 隔离：别的 bot 看不到也决定不了
    assert_eq!(request(&app, "GET", &format!("/v1/sessions/{sid}/approvals"), "other-token", json!({})).await.0, StatusCode::NOT_FOUND);
    assert_eq!(request(&app, "POST", &path, "other-token", json!({"decision": "approve"})).await.0, StatusCode::NOT_FOUND);
    let (status, body) = request(&app, "POST", &format!("/v1/sessions/{sid}/approvals/unknown"), "test-token", json!({"decision": "approve"})).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::NOT_FOUND, Some("approval not found")));
    for bad in [json!({"decision": "maybe"}), json!({}), json!({"decision": "approve", "extra": 1})] {
        assert_eq!(request(&app, "POST", &path, "test-token", bad).await.0, StatusCode::BAD_REQUEST);
    }
    let (status, body) = request(&app, "POST", &path, "test-token", json!({"decision": "approve", "reason": "ok"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"resolved": true, "callId": call, "tool": "bash", "decision": "approved"}));
    let (status, body) = request(&app, "POST", &path, "test-token", json!({"decision": "deny"})).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::CONFLICT, Some("approval already resolved")));
    let messages = wait_idle(&app, &sid).await;
    assert!(texts(&messages, "toolResult")[0].starts_with("approved-output"), "{messages:?}");
    assert!(texts(&messages, "assistant").last().unwrap().contains("approved-output"));
    // 不存在的会话
    assert_eq!(request(&app, "GET", "/v1/sessions/nope/approvals", "test-token", json!({})).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_deny_returns_error_to_model_and_policy_deny_skips_asking() {
    let (app, _hub) = test_app(Some(json!({"tools": {"bash": "ask", "write": "deny"}})));
    let (_, view) = request(&app, "POST", "/v1/sessions", "test-token", json!({})).await;
    let sid = view["sessionId"].as_str().unwrap().to_string();
    request(&app, "POST", &format!("/v1/sessions/{sid}/prompt"), "test-token", json!({"message": BASH})).await;
    let call = wait_pending(&app, &sid).await["callId"].as_str().unwrap().to_string();
    let (status, _) = request(&app, "POST", &format!("/v1/sessions/{sid}/approvals/{call}"), "test-token", json!({"decision": "deny", "reason": "nope"})).await;
    assert_eq!(status, StatusCode::OK);
    let messages = wait_idle(&app, &sid).await;
    assert_eq!(texts(&messages, "toolResult"), ["tool call denied by user: nope"]);
    assert_eq!(texts(&messages, "assistant").last().unwrap(), "tool bash returned: tool call denied by user: nope");
    // 策略 deny：不等待
    request(&app, "POST", &format!("/v1/sessions/{sid}/prompt"), "test-token", json!({"message": r#"/tool write {"path":"x.txt","content":"x"}"#})).await;
    let messages = wait_idle(&app, &sid).await;
    assert_eq!(texts(&messages, "toolResult").last().unwrap(), "tool call denied by policy: tool 'write' is denied (tools rule 'write')");
    assert!(!std::path::Path::new("x.txt").exists());
}

#[tokio::test]
async fn session_override_can_only_narrow() {
    // profile never（默认）+ 会话 always ⇒ 审批
    let (app, hub) = test_app(None);
    let (status, view) = request(&app, "POST", "/v1/sessions", "test-token", json!({"toolApproval": "always"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let sid = view["sessionId"].as_str().unwrap().to_string();
    request(&app, "POST", &format!("/v1/sessions/{sid}/prompt"), "test-token", json!({"message": r#"/tool ls {}"#})).await;
    assert_eq!(wait_pending(&app, &sid).await["reason"], "session: tool 'ls' requires approval (toolApproval: always)");
    hub.remove(&hub.config.clients[0], &sid);
    // 不传 ⇒ 不审批（向后兼容）
    let h = hub.create_session(&hub.config.clients[0], SessionRequest::default()).unwrap();
    assert!(h.agent.config.approval.is_none());
    // 非法会话策略 ⇒ 400；放在 chat 顶层 ⇒ 提示放进 session
    let (status, body) = request(&app, "POST", "/v1/sessions", "test-token", json!({"toolApproval": {"timeoutSecs": 0}})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("toolApproval.timeoutSecs must be between 1 and 86400"), "{body}");
    let (status, body) = request(&app, "POST", "/v1/chat", "test-token", json!({"message": "hi", "toolApproval": "always"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains(r#"session options go under "session""#));

    // profile always + 会话 never ⇒ 仍然审批
    let (app, _hub) = test_app(Some(json!("always")));
    let (_, view) = request(&app, "POST", "/v1/sessions", "test-token", json!({"toolApproval": "never"})).await;
    let sid = view["sessionId"].as_str().unwrap().to_string();
    request(&app, "POST", &format!("/v1/sessions/{sid}/prompt"), "test-token", json!({"message": r#"/tool ls {}"#})).await;
    assert_eq!(wait_pending(&app, &sid).await["reason"], "tool 'ls' requires approval (toolApproval: always)");
}

#[tokio::test]
async fn sse_stream_carries_approval_events_and_sync_chat_is_approved_by_parallel_request() {
    let (app, _hub) = test_app(Some(json!({"tools": {"bash": "ask"}})));
    // 流式 chat：approval_required / approval_resolved 出现在 SSE 上
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test-token")
        .body(Body::from(json!({"message": BASH, "stream": true}).to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let sid = response.headers()["x-session-id"].to_str().unwrap().to_string();
    let mut body = response.into_body().into_data_stream();
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !text.contains("event: approval_required") {
            text.push_str(std::str::from_utf8(&body.next().await.unwrap().unwrap()).unwrap());
        }
    })
    .await
    .expect("no approval_required on SSE");
    let call = wait_pending(&app, &sid).await["callId"].as_str().unwrap().to_string();
    request(&app, "POST", &format!("/v1/sessions/{sid}/approvals/{call}"), "test-token", json!({"decision": "approve"})).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(chunk) = body.next().await {
            text.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
    })
    .await
    .unwrap();
    assert!(text.contains("event: approval_resolved") && text.contains(r#""decision":"approved""#) && text.contains("event: agent_settled"), "{text}");

    // 同步 chat：另一个请求批准后返回 200
    let sync = {
        let app = app.clone();
        let sid = sid.clone();
        tokio::spawn(async move { request(&app, "POST", "/v1/chat", "test-token", json!({"message": BASH, "sessionId": sid})).await })
    };
    let call = wait_pending(&app, &sid).await["callId"].as_str().unwrap().to_string();
    assert_eq!(request(&app, "POST", &format!("/v1/sessions/{sid}/approvals/{call}"), "test-token", json!({"decision": "approve"})).await.0, StatusCode::OK);
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), sync).await.unwrap().unwrap();
    assert_eq!(status, StatusCode::OK);
    assert!(body["text"].as_str().unwrap().contains("approved-output"), "{body}");
}

#[tokio::test]
async fn async_task_webhook_gets_approval_required() {
    use axum::{routing::post, Json};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let receiver = Router::new().route(
        "/hook",
        post(move |Json(body): Json<Value>| {
            let tx = tx.clone();
            async move {
                tx.send(body).unwrap();
                StatusCode::NO_CONTENT
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, receiver).await.unwrap() });
    let mut cfg = config(Some(json!({"tools": {"bash": "ask"}})));
    cfg.clients[0].webhook_allowed_hosts = vec![address.to_string()];
    let hub = Hub::new(cfg, "faux", None, None).unwrap();
    let app = crate::http::test_router(hub.clone());
    let (status, started) =
        request(&app, "POST", "/v1/chat", "test-token", json!({"message": BASH, "async": true, "callbackUrl": format!("http://{address}/hook"), "webhookEvents": ["approval_required", "final"]}))
            .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let hook = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    assert_eq!(hook["event"], "task.approval_required");
    let data = &hook["data"];
    assert_eq!((data["taskId"].clone(), data["sessionId"].clone(), data["tool"].clone()), (started["taskId"].clone(), started["sessionId"].clone(), json!("bash")));
    assert_eq!(data["args"]["command"], "printf approved-output");
    let url = data["approvalUrl"].as_str().unwrap();
    assert_eq!(url, format!("/v1/sessions/{}/approvals/{}", data["sessionId"].as_str().unwrap(), data["callId"].as_str().unwrap()));
    assert_eq!(request(&app, "POST", url, "test-token", json!({"decision": "approve"})).await.0, StatusCode::OK);
    let done = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    assert_eq!(done["event"], "task.completed");
    assert_eq!(done["data"]["status"], "succeeded");
    assert!(done["data"]["result"]["text"].as_str().unwrap().contains("approved-output"));
    server.abort();
}

#[tokio::test]
async fn timeout_denies_and_reports_issue_once() {
    let mut hub = Hub::new(config(Some(json!({"tools": {"bash": "ask"}, "timeoutSecs": 1}))), "faux", None, None).unwrap();
    Arc::get_mut(&mut hub).unwrap().issues = Reporter::new(&hub.config.alerts, None, vec![]);
    let app = crate::http::test_router(hub.clone());
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), request(&app, "POST", "/v1/chat", "test-token", json!({"message": BASH}))).await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], "tool bash returned: tool call denied: approval timed out after 1s");
    hub.issues.flush(Duration::from_secs(2)).await;
    let issues = hub.issues.recent(Some("default"), 0, 100);
    let timeouts: Vec<_> = issues.iter().filter(|e| e.kind == "approval_timeout").collect();
    assert_eq!(timeouts.len(), 1, "{issues:?}");
    assert_eq!(timeouts[0].tool.as_deref(), Some("bash"));
    assert!(!issues.iter().any(|e| e.kind == "log_warn" || e.kind == "tool_error"), "{issues:?}");
}

#[tokio::test]
async fn abort_and_delete_while_waiting_do_not_hang() {
    let (app, hub) = test_app(Some(json!("always")));
    for delete in [false, true] {
        let (_, view) = request(&app, "POST", "/v1/sessions", "test-token", json!({})).await;
        let sid = view["sessionId"].as_str().unwrap().to_string();
        let h = hub.get(&hub.config.clients[0], &sid).unwrap();
        request(&app, "POST", &format!("/v1/sessions/{sid}/prompt"), "test-token", json!({"message": BASH})).await;
        let call = wait_pending(&app, &sid).await["callId"].as_str().unwrap().to_string();
        if delete {
            assert_eq!(request(&app, "DELETE", &format!("/v1/sessions/{sid}"), "test-token", json!({})).await.0, StatusCode::OK);
        } else {
            assert_eq!(request(&app, "POST", &format!("/v1/sessions/{sid}/abort"), "test-token", json!({})).await.0, StatusCode::OK);
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while h.agent.is_running() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("abort while waiting hung");
        assert!(h.pending_approvals().is_empty());
        if !delete {
            assert_eq!(request(&app, "POST", &format!("/v1/sessions/{sid}/approvals/{call}"), "test-token", json!({"decision": "approve"})).await.0, StatusCode::CONFLICT);
        }
    }
}

#[tokio::test]
async fn rpc_approve_command_and_state() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let hub = Hub::new(config(Some(json!({"tools": {"bash": "ask"}}))), "faux", None, None).unwrap();
    let (mut input, input_rx) = tokio::io::duplex(64 * 1024);
    let (output_tx, output) = tokio::io::duplex(1024 * 1024);
    let server = tokio::spawn(crate::rpc::run_io(hub, SessionRequest::default(), None, input_rx, output_tx));
    let mut lines = tokio::io::BufReader::new(output).lines();
    let mut send = async |command: Value| input.write_all(format!("{command}\n").as_bytes()).await.unwrap();
    let mut next = async |pred: &dyn Fn(&Value) -> bool| -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let line: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                if pred(&line) {
                    return line;
                }
            }
        })
        .await
        .expect("rpc line not received")
    };
    send(json!({"id": "1", "type": "prompt", "message": BASH})).await;
    let event = next(&|l| l["type"] == "approval_required").await;
    let call = event["callId"].as_str().unwrap().to_string();
    assert_eq!(event["args"]["command"], "printf approved-output");
    send(json!({"id": "2", "type": "get_state"})).await;
    let state = next(&|l| l["id"] == "2").await;
    assert_eq!(state["data"]["pendingApprovals"][0]["callId"], call.as_str());
    send(json!({"id": "3", "type": "approve", "callId": "unknown", "decision": "approve"})).await;
    assert_eq!(next(&|l| l["id"] == "3").await, json!({"id": "3", "type": "response", "command": "approve", "success": false, "error": "approval not found"}));
    send(json!({"id": "4", "type": "approve", "callId": call, "decision": "maybe"})).await;
    assert_eq!(next(&|l| l["id"] == "4").await["success"], false);
    send(json!({"id": "5", "type": "approve", "callId": call, "decision": "approve", "reason": "fine"})).await;
    let response = next(&|l| l["id"] == "5").await;
    assert_eq!(response["data"], json!({"resolved": true, "callId": call, "tool": "bash"}));
    let resolved = next(&|l| l["type"] == "approval_resolved").await;
    assert_eq!((resolved["decision"].clone(), resolved["by"].clone(), resolved["reason"].clone()), (json!("approved"), json!("caller"), json!("fine")));
    let end = next(&|l| l["type"] == "tool_execution_end").await;
    assert_eq!(end["isError"], false);
    next(&|l| l["type"] == "agent_settled").await;
    send(json!({"id": "6", "type": "approve", "callId": call, "decision": "deny"})).await;
    assert_eq!(next(&|l| l["id"] == "6").await["error"], "approval already resolved");
    drop(send);
    drop(input);
    tokio::time::timeout(Duration::from_secs(3), server).await.unwrap().unwrap().unwrap();
}
