//! 回合预算的服务侧测试：`POST /v1/sessions/{id}/turns`（200/400/404/409）、PATCH maxTurns、会话选项、
//! ask 模式暂停后延长 / 停止、同步 chat 的最终结果字段、summarize、webhook `turn_budget` 与 RPC `set_max_turns`。
//! 都用 faux provider：`/tool <name> <json>` 先返回一次工具调用，再回显工具结果（共 2 次模型调用）。
use crate::{
    config::{Client, Config},
    hub::{Hub, SessionRequest},
};
use agent_core::{AgentEvent, OnMaxTurns, TurnBudgetDecision};
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::StatusCode,
    Router,
};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

const LS: &str = r#"/tool ls {}"#;

fn config() -> Config {
    let mut cfg = Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
    cfg.data_dir = Some(std::env::temp_dir().join(format!("sub-bot-turns-{}", uuid::Uuid::new_v4())));
    cfg.profiles.get_mut("default").unwrap().load_context_files = false;
    cfg.clients.push(Client { name: "other".into(), token_sha256: crate::config::sha256_hex("other-token"), profiles: vec!["default".into()], ..Default::default() });
    cfg
}

fn test_app() -> (Router, Arc<Hub>) {
    let hub = Hub::new(config(), "faux", None, None).unwrap();
    (crate::http::test_router(hub.clone()), hub)
}

async fn request(app: &Router, method: &str, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(path).header("content-type", "application/json").header("authorization", format!("Bearer {token}"));
    let response = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn create(app: &Router, body: Value) -> String {
    let (status, view) = request(app, "POST", "/v1/sessions", "test-token", body).await;
    assert_eq!(status, StatusCode::CREATED, "{view}");
    view["sessionId"].as_str().unwrap().to_string()
}

async fn view(app: &Router, sid: &str) -> Value {
    request(app, "GET", &format!("/v1/sessions/{sid}"), "test-token", json!({})).await.1
}

async fn wait_paused(app: &Router, sid: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v = view(app, sid).await;
            if v["turnBudget"]["paused"] == true {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("run did not pause at the turn limit")
}

async fn wait_idle(app: &Router, sid: &str) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if view(app, sid).await["isStreaming"] == false {
                let (_, messages) = request(app, "GET", &format!("/v1/sessions/{sid}/messages"), "test-token", json!({})).await;
                return messages["messages"].as_array().unwrap().clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session did not finish")
}

fn assistant_texts(messages: &[Value]) -> Vec<String> {
    messages.iter().filter(|m| m["role"] == "assistant").map(|m| m["content"].as_array().unwrap().iter().filter_map(|c| c["text"].as_str()).collect::<String>()).collect()
}

#[tokio::test]
async fn post_turns_validates_owner_and_conflict() {
    let (app, _hub) = test_app();
    let sid = create(&app, json!({})).await;
    let v = view(&app, &sid).await;
    assert_eq!(v["maxTurns"], 150);
    assert_eq!(v["turnBudget"], json!({"maxTurns": 150, "onMaxTurns": "stop", "askTimeoutSecs": 600, "running": false, "paused": false, "turnsUsed": 0}));
    let path = format!("/v1/sessions/{sid}/turns");
    for bad in
        [json!({}), json!({"maxTurns": 0}), json!({"maxTurns": 5, "addTurns": 1}), json!({"stop": false}), json!({"addTurns": 1, "stop": true}), json!({"maxTurns": -1}), json!({"extendTurns": 1})]
    {
        let (status, body) = request(&app, "POST", &path, "test-token", bad.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    assert_eq!(request(&app, "POST", &path, "other-token", json!({"maxTurns": 5})).await.0, StatusCode::NOT_FOUND);
    assert_eq!(request(&app, "POST", &format!("/v1/sessions/{}/turns", uuid::Uuid::new_v4()), "test-token", json!({"maxTurns": 5})).await.0, StatusCode::NOT_FOUND);
    let (status, body) = request(&app, "POST", &path, "test-token", json!({"stop": true})).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::CONFLICT, Some("no run is paused at the turn limit")));
    let (status, body) = request(&app, "POST", &path, "test-token", json!({"maxTurns": 5})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"sessionId": sid, "maxTurns": 5, "previous": 150, "turnsUsed": 0, "running": false, "paused": false}));
    // addTurns 可为负，结果钳制到 >= 1
    let (_, body) = request(&app, "POST", &path, "test-token", json!({"addTurns": -100})).await;
    assert_eq!((body["maxTurns"].clone(), body["previous"].clone()), (json!(1), json!(5)));
    let (_, body) = request(&app, "POST", &path, "test-token", json!({"addTurns": 9})).await;
    assert_eq!(body["maxTurns"], 10);
    assert_eq!(view(&app, &sid).await["maxTurns"], 10);
    // PATCH 也接受 maxTurns
    let (status, _) = request(&app, "PATCH", &format!("/v1/sessions/{sid}"), "test-token", json!({"maxTurns": 0})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(&app, "PATCH", &format!("/v1/sessions/{sid}"), "test-token", json!({"maxTurns": 42})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view(&app, &sid).await["turnBudget"]["maxTurns"], 42);
}

#[tokio::test]
async fn session_options_and_invalid_values() {
    let (app, hub) = test_app();
    let sid = create(&app, json!({"maxTurns": 3, "onMaxTurns": "ask", "maxTurnsAskTimeoutSecs": 9})).await;
    let budget = &view(&app, &sid).await["turnBudget"];
    assert_eq!((budget["maxTurns"].clone(), budget["onMaxTurns"].clone(), budget["askTimeoutSecs"].clone()), (json!(3), json!("ask"), json!(9)));
    for bad in [json!({"maxTurns": 0}), json!({"maxTurnsAskTimeoutSecs": 0}), json!({"onMaxTurns": "extend"}), json!({"maxTurns": -2})] {
        let (status, body) = request(&app, "POST", "/v1/sessions", "test-token", bad.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    // /v1/chat 顶层不接受（会话选项放在 session 里）
    let (status, body) = request(&app, "POST", "/v1/chat", "test-token", json!({"message": "hi", "maxTurns": 3})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains(r#"session options go under "session""#), "{body}");
    let (status, body) = request(&app, "POST", "/v1/chat", "test-token", json!({"message": "hi", "session": {"maxTurns": 0}})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let h = hub.create_session(&hub.config.clients[0], SessionRequest { max_turns: Some(77), on_max_turns: Some(OnMaxTurns::Summarize), ..Default::default() }).unwrap();
    assert_eq!((h.max_turns(), h.turn_budget().on_max_turns), (77, OnMaxTurns::Summarize));
}

#[tokio::test]
async fn sync_chat_stop_reports_max_turns_and_can_continue() {
    let (app, _hub) = test_app();
    let (status, body) = request(&app, "POST", "/v1/chat", "test-token", json!({"message": LS, "session": {"maxTurns": 1}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sid = body["sessionId"].as_str().unwrap().to_string();
    assert_eq!((body["stopReason"].clone(), body["canContinue"].clone(), body["turnsUsed"].clone(), body["maxTurns"].clone()), (json!("max_turns"), json!(true), json!(1), json!(1)));
    assert_eq!(body["error"], format!("max_turns reached (1/1 turns used); continue by sending another prompt with sessionId {sid}"));
    // 提高上限后继续
    request(&app, "POST", &format!("/v1/sessions/{sid}/turns"), "test-token", json!({"addTurns": 4})).await;
    let (status, body) = request(&app, "POST", "/v1/chat", "test-token", json!({"message": "continue", "sessionId": sid})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], "echo: continue");
    assert_eq!((body["turnsUsed"].clone(), body["maxTurns"].clone(), body["sessionId"].clone()), (json!(1), json!(5), json!(sid)));
    assert!(body.get("canContinue").is_none() && body["stopReason"] != "max_turns", "{body}");
    assert!(body["error"].is_null());
}

#[tokio::test]
async fn ask_mode_pauses_and_extends_via_http() {
    let (app, hub) = test_app();
    let sid = create(&app, json!({"maxTurns": 1, "onMaxTurns": "ask"})).await;
    let h = hub.get(&hub.config.clients[0], &sid).unwrap();
    let mut rx = h.subscribe();
    assert_eq!(request(&app, "POST", &format!("/v1/sessions/{sid}/prompt"), "test-token", json!({"message": LS})).await.0, StatusCode::ACCEPTED);
    let v = wait_paused(&app, &sid).await;
    assert_eq!(v["isStreaming"], true);
    let pause = &v["turnBudget"]["pause"];
    assert_eq!((pause["turnsUsed"].clone(), pause["maxTurns"].clone(), pause["timeoutSecs"].clone()), (json!(1), json!(1), json!(600)));
    assert_eq!(v["turnBudget"]["turnsUsed"], 1);
    let (status, body) = request(&app, "POST", &format!("/v1/sessions/{sid}/turns"), "test-token", json!({"addTurns": 2})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"sessionId": sid, "maxTurns": 3, "previous": 1, "turnsUsed": 1, "running": true, "paused": false}));
    let messages = wait_idle(&app, &sid).await;
    assert!(assistant_texts(&messages).last().unwrap().starts_with("tool ls returned: "), "{messages:?}");
    let mut seen = vec![];
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::TurnBudgetExhausted { turns_used, max_turns, action, timeout_secs, .. } => seen.push(format!("exhausted {turns_used}/{max_turns} {action} {timeout_secs:?}")),
            AgentEvent::TurnBudgetChanged { max_turns, previous, turns_used, .. } => seen.push(format!("changed {previous}->{max_turns} used {turns_used}")),
            AgentEvent::TurnBudgetResolved { decision, max_turns, .. } => seen.push(format!("resolved {decision:?} {max_turns}")),
            AgentEvent::RunOutcome { kind } => seen.push(kind),
            _ => {}
        }
    }
    assert_eq!(seen, ["exhausted 1/1 ask Some(600)", "changed 1->3 used 1", "resolved Extended 3"]);
    assert_eq!(h.turn_budget().last_run.unwrap().turns_used, 2);
    assert_eq!(view(&app, &sid).await["turnBudget"]["paused"], false);
}

#[tokio::test]
async fn ask_mode_stop_ends_sync_chat_with_max_turns() {
    let (app, hub) = test_app();
    let sid = create(&app, json!({"maxTurns": 1, "onMaxTurns": "ask"})).await;
    let h = hub.get(&hub.config.clients[0], &sid).unwrap();
    let mut rx = h.subscribe();
    let sync = {
        let (app, sid) = (app.clone(), sid.clone());
        tokio::spawn(async move { request(&app, "POST", "/v1/chat", "test-token", json!({"message": LS, "sessionId": sid})).await })
    };
    wait_paused(&app, &sid).await;
    // 别的 bot 不能停止
    assert_eq!(request(&app, "POST", &format!("/v1/sessions/{sid}/turns"), "other-token", json!({"stop": true})).await.0, StatusCode::NOT_FOUND);
    let (status, body) = request(&app, "POST", &format!("/v1/sessions/{sid}/turns"), "test-token", json!({"stop": true})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!((body["maxTurns"].clone(), body["turnsUsed"].clone(), body["paused"].clone()), (json!(1), json!(1), json!(false)));
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), sync).await.unwrap().unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!((body["stopReason"].clone(), body["canContinue"].clone(), body["turnsUsed"].clone()), (json!("max_turns"), json!(true), json!(1)));
    assert_eq!(request(&app, "POST", &format!("/v1/sessions/{sid}/turns"), "test-token", json!({"stop": true})).await.0, StatusCode::CONFLICT);
    let mut decisions = vec![];
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::TurnBudgetResolved { decision, .. } = event {
            decisions.push(decision);
        }
    }
    assert_eq!(decisions, [TurnBudgetDecision::Stopped]);
}

#[tokio::test]
async fn abort_while_paused_does_not_hang() {
    let (app, hub) = test_app();
    let sid = create(&app, json!({"maxTurns": 1, "onMaxTurns": "ask"})).await;
    let h = hub.get(&hub.config.clients[0], &sid).unwrap();
    request(&app, "POST", &format!("/v1/sessions/{sid}/prompt"), "test-token", json!({"message": LS})).await;
    wait_paused(&app, &sid).await;
    assert_eq!(request(&app, "POST", &format!("/v1/sessions/{sid}/abort"), "test-token", json!({})).await.0, StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(3), async {
        while h.agent.is_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("abort while paused hung");
    assert!(h.turn_budget().pause.is_none());
    assert!(!h.turn_budget().last_run.unwrap().limit_reached);
}

#[tokio::test]
async fn summarize_makes_a_final_toolless_call() {
    let (app, _hub) = test_app();
    let (status, body) = request(&app, "POST", "/v1/chat", "test-token", json!({"message": LS, "session": {"maxTurns": 1, "onMaxTurns": "summarize"}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["text"], "echo: [sub-bot] Turn limit reached. Do not call tools; summarize progress, what remains, and next steps.");
    assert_eq!((body["stopReason"].clone(), body["canContinue"].clone(), body["turnsUsed"].clone()), (json!("max_turns"), json!(true), json!(1)));
}

#[tokio::test]
async fn async_task_webhook_gets_turn_budget_events() {
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
    let mut cfg = config();
    cfg.clients[0].webhook_allowed_hosts = vec![address.to_string()];
    let hub = Hub::new(cfg, "faux", None, None).unwrap();
    let app = crate::http::test_router(hub.clone());
    let body = json!({"message": LS, "async": true, "session": {"maxTurns": 1, "onMaxTurns": "ask"}, "callbackUrl": format!("http://{address}/hook"), "webhookEvents": ["turn_budget", "final"]});
    let (status, started) = request(&app, "POST", "/v1/chat", "test-token", body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    let hook = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    assert_eq!(hook["event"], "task.turn_budget_exhausted");
    let data = &hook["data"];
    assert_eq!((data["taskId"].clone(), data["sessionId"].clone()), (started["taskId"].clone(), started["sessionId"].clone()));
    assert_eq!((data["turnsUsed"].clone(), data["maxTurns"].clone(), data["action"].clone(), data["timeoutSecs"].clone()), (json!(1), json!(1), json!("ask"), json!(600)));
    assert!(data["expiresAt"].as_u64().is_some());
    let url = data["turnsUrl"].as_str().unwrap();
    assert_eq!(url, format!("/v1/sessions/{}/turns", started["sessionId"].as_str().unwrap()));
    assert_eq!(request(&app, "POST", url, "test-token", json!({"maxTurns": 2})).await.0, StatusCode::OK);
    let done = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    assert_eq!(done["event"], "task.completed");
    let result = &done["data"]["result"];
    assert_eq!((result["turnsUsed"].clone(), result["maxTurns"].clone(), result["sessionId"].clone()), (json!(2), json!(2), started["sessionId"].clone()));
    assert!(result.get("canContinue").is_none(), "{result}");

    // faux 的 /tool 链只有 2 次模型调用，到不了 turn_budget_low 的阈值（载荷见 webhook_payload_for_low_and_exhausted）；
    // 这里只确认 "all" 被接受且正常完成。
    let (status, _) = request(&app, "POST", "/v1/chat", "test-token", json!({"message": "hi", "async": true, "callbackUrl": format!("http://{address}/hook"), "webhookEvents": "all"})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let mut events = vec![];
    while let Ok(Some(hook)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        let name = hook["event"].as_str().unwrap().to_string();
        events.push(name.clone());
        if name == "task.completed" {
            break;
        }
    }
    assert_eq!(events.last().map(String::as_str), Some("task.completed"));
    server.abort();
}

#[test]
fn webhook_payload_for_low_and_exhausted() {
    let low = AgentEvent::TurnBudgetLow { turns_used: 8, max_turns: 10, remaining: 2 };
    let (name, data) = crate::turns::payload(Some("t1"), "s1", &low).unwrap();
    assert_eq!(name, "task.turn_budget_low");
    assert_eq!(data, json!({"turnsUsed": 8, "maxTurns": 10, "remaining": 2, "sessionId": "s1", "turnsUrl": "/v1/sessions/s1/turns", "taskId": "t1"}));
    let stop = AgentEvent::TurnBudgetExhausted { turns_used: 10, max_turns: 10, action: OnMaxTurns::Stop, timeout_secs: None, expires_at: None };
    let (name, data) = crate::turns::payload(None, "s1", &stop).unwrap();
    assert_eq!(name, "task.turn_budget_exhausted");
    assert_eq!(data, json!({"turnsUsed": 10, "maxTurns": 10, "action": "stop", "sessionId": "s1", "turnsUrl": "/v1/sessions/s1/turns"}));
    assert!(crate::turns::payload(None, "s1", &AgentEvent::TurnStart).is_none());
}

#[tokio::test]
async fn rpc_set_max_turns_and_state() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let hub = Hub::new(config(), "faux", None, None).unwrap();
    let (mut input, input_rx) = tokio::io::duplex(64 * 1024);
    let (output_tx, output) = tokio::io::duplex(1024 * 1024);
    let req = SessionRequest { max_turns: Some(1), on_max_turns: Some(OnMaxTurns::Ask), ..Default::default() };
    let server = tokio::spawn(crate::rpc::run_io(hub, req, None, input_rx, output_tx));
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
    send(json!({"id": "1", "type": "set_max_turns", "maxTurns": 0})).await;
    assert_eq!(next(&|l| l["id"] == "1").await, json!({"id": "1", "type": "response", "command": "set_max_turns", "success": false, "error": "maxTurns must be >= 1"}));
    send(json!({"id": "2", "type": "set_max_turns", "stop": true})).await;
    assert_eq!(next(&|l| l["id"] == "2").await["error"], "no run is paused at the turn limit");
    send(json!({"id": "3", "type": "prompt", "message": LS})).await;
    let event = next(&|l| l["type"] == "turn_budget_exhausted").await;
    assert_eq!((event["turnsUsed"].clone(), event["action"].clone()), (json!(1), json!("ask")));
    send(json!({"id": "4", "type": "get_state"})).await;
    let state = next(&|l| l["id"] == "4").await;
    assert_eq!((state["data"]["maxTurns"].clone(), state["data"]["turnBudget"]["paused"].clone()), (json!(1), json!(true)));
    send(json!({"id": "5", "type": "set_max_turns", "addTurns": 1})).await;
    let response = next(&|l| l["id"] == "5").await;
    assert_eq!(response["data"]["maxTurns"], 2);
    assert_eq!(response["data"]["previous"], 1);
    let resolved = next(&|l| l["type"] == "turn_budget_resolved").await;
    assert_eq!((resolved["decision"].clone(), resolved["maxTurns"].clone()), (json!("extended"), json!(2)));
    next(&|l| l["type"] == "agent_settled").await;
    drop(send);
    server.abort();
}
