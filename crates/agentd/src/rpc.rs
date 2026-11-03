//! stdio JSONL RPC，协议形状参照 pi `coding-agent/src/modes/rpc/rpc-types.ts`：
//! stdin 每行一个命令 `{"id":"1","type":"prompt","message":"..."}`；
//! stdout 输出 `{"id":"1","type":"response","command":"prompt","success":true,"data":{...}}` 和事件行。
//! 严格按 LF 分帧；日志一律走 stderr。
use crate::config::Client;
use crate::hub::{Hub, SessionHandle, SessionRequest};
use crate::routing::Depth;
use agent_ai::ThinkingLevel;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
enum Command {
    /// reasoningEffort：粘性修改（从这条消息起）；depth：只在第一次 prompt 时有效
    Prompt {
        message: String,
        reasoning_effort: Option<ThinkingLevel>,
        depth: Option<Depth>,
    },
    /// 修改会话推理强度（从下一次模型调用生效，运行中也一样）
    SetReasoningEffort {
        level: ThinkingLevel,
    },
    Steer {
        message: String,
    },
    FollowUp {
        message: String,
    },
    Abort,
    /// 决定一次工具调用审批（事件 `approval_required` 的 callId）
    Approve {
        call_id: String,
        decision: crate::approvals::Decision,
        reason: Option<String>,
    },
    GetMessages,
    GetState,
    GetAvailableModels,
    /// 立即压缩上下文（运行中失败）；可选给摘要模型的附加说明
    Compact {
        #[serde(default)]
        instructions: Option<String>,
    },
}

fn state(hub: &Hub, session: &SessionHandle) -> Value {
    let mut data = json!({"sessionId": session.id, "persisted": session.hooks.path().is_some(), "isStreaming": session.agent.is_running(), "provider": session.provider_id(), "account": session.account, "model": session.model(), "api": session.effective_api().map(|a| a.as_str().to_string()).unwrap_or_else(|| hub.effective_api(&session.agent.config).to_string()), "profile": session.profile, "pendingApprovals": session.pending_approvals()});
    data.as_object_mut().unwrap().extend(session.effort_fields());
    data
}

fn send_response(out: &mpsc::UnboundedSender<Value>, id: Option<Value>, command: String, resp: Value) {
    let mut r = json!({"type": "response", "command": command});
    if let Some(id) = id {
        r["id"] = id;
    }
    r.as_object_mut().unwrap().extend(resp.as_object().unwrap().clone());
    let _ = out.send(r);
}

/// stdio 的调用方就是父进程本身，视为可用全部 profile 的本地 client。
/// `resume`：恢复 `<sessionDir>/stdio/` 下已持久化的会话（会话参数来自记录，`req` 里只有推理强度作为粘性修改生效）。
pub async fn run(hub: Arc<Hub>, req: SessionRequest, resume: Option<String>) -> anyhow::Result<()> {
    run_io(hub, req, resume, tokio::io::stdin(), tokio::io::stdout()).await
}

fn open_session(hub: &Hub, client: &Client, req: SessionRequest, resume: Option<&str>) -> anyhow::Result<Arc<SessionHandle>> {
    let Some(id) = resume else { return hub.create_session(client, req) };
    anyhow::ensure!(hub.client_session_dir(&client.name).is_some(), "session persistence is disabled (--no-persist-sessions / persistSessions: false); cannot resume");
    let session = hub.load_persisted(client, id)?.ok_or_else(|| anyhow::anyhow!("session {id} not found"))?;
    if let Some(level) = req.reasoning_effort {
        session.set_reasoning_effort(Some(level));
    }
    Ok(session)
}

pub(crate) async fn run_io(hub: Arc<Hub>, req: SessionRequest, resume: Option<String>, input: impl AsyncRead + Unpin, output: impl AsyncWrite + Unpin + Send + 'static) -> anyhow::Result<()> {
    let client = Client { name: "stdio".into(), token_sha256: String::new(), profiles: hub.config.profiles.keys().cloned().collect(), ..Default::default() };
    let mut output = output;
    let session = match open_session(&hub, &client, req, resume.as_deref()) {
        Ok(session) => session,
        Err(e) => {
            // 会话创建失败（如 AccountNotAllowed）：先给 JSONL 调用方一条 success:false，再以错误退出
            let line = json!({"type": "response", "command": "start", "success": false, "error": e.to_string()});
            let _ = output.write_all(format!("{line}\n").as_bytes()).await;
            let _ = output.flush().await;
            return Err(e);
        }
    };
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();

    // 单一 writer，保证每条记录整行写出
    let writer = tokio::spawn(async move {
        let mut stdout = output;
        while let Some(v) = out_rx.recv().await {
            let mut line = serde_json::to_vec(&v).unwrap_or_default();
            line.push(b'\n');
            if stdout.write_all(&line).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    // 事件转发
    let mut events = session.subscribe();
    let ev_tx = out_tx.clone();
    // Hub uses the installed Reporter or noop in RPC mode; no receiver subscriptions.
    let issue_session = session.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => {
                    if let Ok(v) = serde_json::to_value(&ev) {
                        let _ = ev_tx.send(v);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => issue_session.report_lag(n),
                Err(_) => break,
            }
        }
    });

    let mut requests = tokio::task::JoinSet::new();
    let mut lines = BufReader::new(input).split(b'\n');
    while let Some(raw) = lines.next_segment().await? {
        while requests.try_join_next().is_some() {}
        let text = String::from_utf8_lossy(&raw);
        let text = text.trim_end_matches('\r');
        if text.trim().is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                let _ = out_tx.send(json!({"type":"response","command":"parse","success":false,"error":e.to_string()}));
                continue;
            }
        };
        let id = parsed.get("id").cloned();
        let cmd_name = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let resp = match serde_json::from_value::<Command>(parsed) {
            Err(e) => json!({"success": false, "error": e.to_string()}),
            Ok(Command::Prompt { message, reasoning_effort, depth }) => match depth.map(|d| session.set_depth(d)).transpose() {
                Err(e) => json!({"success": false, "error": e.to_string()}),
                Ok(_) => {
                    if let Some(level) = reasoning_effort {
                        session.set_reasoning_effort(Some(level));
                    }
                    json!({"success": true, "data": {"disposition": session.submit(message, None)}})
                }
            },
            Ok(Command::SetReasoningEffort { level }) => {
                json!({"success": true, "data": session.set_reasoning_effort(Some(level)).json()})
            }
            Ok(Command::Steer { message }) => {
                json!({"success": true, "data": {"disposition": session.submit(message, Some("steer"))}})
            }
            Ok(Command::FollowUp { message }) => {
                json!({"success": true, "data": {"disposition": session.submit(message, Some("follow_up"))}})
            }
            Ok(Command::Abort) => {
                session.agent.abort();
                json!({"success": true})
            }
            Ok(Command::Approve { call_id, decision, reason }) => match session.resolve_approval(&call_id, decision, reason) {
                Ok(p) => json!({"success": true, "data": {"resolved": true, "callId": p.call_id, "tool": p.tool}}),
                Err(e) => json!({"success": false, "error": e.to_string()}),
            },
            Ok(Command::GetMessages) => {
                let session = session.clone();
                let out = out_tx.clone();
                requests.spawn(async move {
                    let resp = json!({"success": true, "data": {"messages": session.agent.messages().await}});
                    send_response(&out, id, cmd_name, resp);
                });
                continue;
            }
            Ok(Command::GetState) => json!({"success": true, "data": state(&hub, &session)}),
            Ok(Command::Compact { instructions }) => {
                let session = session.clone();
                let out = out_tx.clone();
                requests.spawn(async move {
                    let resp = match session.compact(instructions).await {
                        Ok(report) => json!({"success": true, "data": report}),
                        Err(e) => json!({"success": false, "error": e.to_string()}),
                    };
                    send_response(&out, id, cmd_name, resp);
                });
                continue;
            }
            Ok(Command::GetAvailableModels) => {
                let hub = hub.clone();
                let out = out_tx.clone();
                requests.spawn(async move {
                    let resp = match hub.list_models(false).await {
                        Ok(models) => json!({"success": true, "data": {"models": models}}),
                        Err(e) => json!({"success": false, "error": e.to_string()}),
                    };
                    send_response(&out, id, cmd_name, resp);
                });
                continue;
            }
        };
        send_response(&out_tx, id, cmd_name, resp);
    }
    // stdin 关闭 = 有序退出
    session.agent.abort();
    requests.shutdown().await;
    forwarder.abort();
    let _ = forwarder.await;
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;

    #[tokio::test]
    async fn set_reasoning_effort_clamps_and_get_state_reports() {
        let mut cfg = crate::config::Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
        cfg.profiles.get_mut("default").unwrap().load_context_files = false;
        let mut hub = Hub::new(cfg, "faux", None, None).unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<agent_ai::Request>::new()));
        Arc::get_mut(&mut hub).unwrap().providers.insert("anthropic".into(), Arc::new(crate::hub::tests::RecordingProvider { id: "anthropic", seen: seen.clone(), models: vec![] }));
        let (mut input, input_rx) = tokio::io::duplex(64 * 1024);
        let (output_tx, output) = tokio::io::duplex(1024 * 1024);
        let req = SessionRequest { provider: Some("anthropic".into()), model: Some("claude-sonnet-4-6".into()), depth: Some(Depth::Deep), ..Default::default() };
        let server = tokio::spawn(run_io(hub, req, None, input_rx, output_tx));
        let mut lines = tokio::io::BufReader::new(output).lines();
        let mut call = async |command: Value| -> Value {
            input.write_all(format!("{command}\n").as_bytes()).await.unwrap();
            loop {
                let line: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                if line["type"] == "response" && line["id"] == command["id"] {
                    return line;
                }
            }
        };
        let response = call(json!({"id": "1", "type": "set_reasoning_effort", "level": "low"})).await;
        assert_eq!(response["success"], true);
        assert_eq!(response["data"], json!({"requested": "low", "effective": "medium", "adjusted": "clamped to allowed top-three levels", "allowedEfforts": ["medium", "high", "max"]}));
        assert_eq!(call(json!({"id": "2", "type": "set_reasoning_effort", "level": "NOT VALID"})).await["success"], false);
        let state = call(json!({"id": "3", "type": "get_state"})).await;
        assert_eq!((state["data"]["reasoningEffort"].clone(), state["data"]["requestedReasoningEffort"].clone()), (json!("medium"), json!("low")));
        assert_eq!(state["data"]["routing"]["reason"], "explicit model");
        assert_eq!(state["data"]["model"], "claude-sonnet-4-6");
        let response = call(json!({"id": "4", "type": "prompt", "message": "hi", "reasoningEffort": "max"})).await;
        assert_eq!(response["success"], true);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while seen.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(seen.lock().unwrap()[0].reasoning, Some(ThinkingLevel::Max));
        let response = call(json!({"id": "5", "type": "prompt", "message": "hi", "depth": "standard"})).await;
        assert_eq!(response["success"], false);
        assert!(response["error"].as_str().unwrap().contains("before the first prompt"));
        drop(call);
        drop(input);
        tokio::time::timeout(std::time::Duration::from_secs(3), server).await.unwrap().unwrap().unwrap();
    }

    #[tokio::test]
    async fn account_in_state_and_disallowed_account_is_success_false() {
        let mut cfg = crate::config::Config::fallback("faux".into(), "test-token", std::env::current_dir().unwrap());
        cfg.profiles.get_mut("default").unwrap().load_context_files = false;
        let hub = Hub::new(cfg, "faux", None, None).unwrap();
        let (_input, input_rx) = tokio::io::duplex(1024);
        let (output_tx, output) = tokio::io::duplex(64 * 1024);
        let req = SessionRequest { account: Some("work".into()), ..Default::default() };
        let err = run_io(hub.clone(), req, None, input_rx, output_tx).await.unwrap_err();
        assert!(err.is::<crate::hub::AccountNotAllowed>());
        let mut lines = tokio::io::BufReader::new(output).lines();
        let line: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(line, json!({"type": "response", "command": "start", "success": false, "error": "account 'work' is not allowed in profile 'default' (allowedAccounts)"}));
        let (mut input, input_rx) = tokio::io::duplex(1024);
        let (output_tx, output) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(run_io(hub, SessionRequest::default(), None, input_rx, output_tx));
        input.write_all(b"{\"id\":\"1\",\"type\":\"get_state\"}\n").await.unwrap();
        let mut lines = tokio::io::BufReader::new(output).lines();
        let state: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(state["data"]["account"], "default");
        drop(input);
        tokio::time::timeout(std::time::Duration::from_secs(3), server).await.unwrap().unwrap().unwrap();
    }

    /// 一次 RPC 会话：发送命令，读到对应 response；`wait_settled` 时额外等 agent_settled。
    async fn rpc_session(hub: Arc<Hub>, resume: Option<String>, commands: Vec<(Value, bool)>) -> Vec<Value> {
        let (mut input, input_rx) = tokio::io::duplex(64 * 1024);
        let (output_tx, output) = tokio::io::duplex(1024 * 1024);
        let server = tokio::spawn(run_io(hub, SessionRequest::default(), resume, input_rx, output_tx));
        let mut lines = tokio::io::BufReader::new(output).lines();
        let mut responses = vec![];
        for (command, wait_settled) in commands {
            input.write_all(format!("{command}\n").as_bytes()).await.unwrap();
            let (mut response, mut settled) = (None, !wait_settled);
            while response.is_none() || !settled {
                let line: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                if line["type"] == "response" && line["id"] == command["id"] {
                    response = Some(line);
                } else if line["type"] == "agent_settled" {
                    settled = true;
                }
            }
            responses.push(response.unwrap());
        }
        drop(input);
        tokio::time::timeout(std::time::Duration::from_secs(5), server).await.unwrap().unwrap().unwrap();
        responses
    }

    #[tokio::test]
    async fn compact_command_and_resume_flag() {
        let dir = crate::sessions_tests::TempDir::new();
        let (hub, _) = crate::sessions_tests::hub(&dir, |cfg| {
            cfg.profiles.get_mut("default").unwrap().compaction = Some(crate::compaction::CompactionConfig { enabled: Some(false), keep_recent_tokens: Some(0), ..Default::default() });
        });
        let long = "lorem ipsum ".repeat(80);
        let r = rpc_session(
            hub.clone(),
            None,
            vec![
                (json!({"id": "1", "type": "prompt", "message": long}), true),
                (json!({"id": "2", "type": "prompt", "message": long}), true),
                (json!({"id": "3", "type": "compact", "instructions": "be brief"}), false),
                (json!({"id": "4", "type": "get_state"}), false),
            ],
        )
        .await;
        assert_eq!(r[2]["success"], true, "{}", r[2]);
        assert_eq!(r[2]["data"]["compacted"], true);
        assert!(r[2]["data"]["firstKeptEntryId"].is_string());
        assert_eq!(r[3]["data"]["persisted"], true);
        let id = r[3]["data"]["sessionId"].as_str().unwrap().to_string();
        // 重启 RPC 进程：--resume 恢复压缩后的上下文
        let (hub2, _) = crate::sessions_tests::hub(&dir, |_| {});
        let r = rpc_session(hub2.clone(), Some(id.clone()), vec![(json!({"id": "1", "type": "get_state"}), false), (json!({"id": "2", "type": "get_messages"}), false)]).await;
        assert_eq!(r[0]["data"]["sessionId"], id.as_str());
        let messages = r[1]["data"]["messages"].as_array().unwrap();
        assert_eq!(messages[0], serde_json::to_value(agent_session::compaction::summary_message("SUMMARY-1")).unwrap());
        // 未知 id ⇒ start 失败
        let (_input, input_rx) = tokio::io::duplex(1024);
        let (output_tx, _output) = tokio::io::duplex(64 * 1024);
        let err = run_io(hub2, SessionRequest::default(), Some(uuid::Uuid::new_v4().to_string()), input_rx, output_tx).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
