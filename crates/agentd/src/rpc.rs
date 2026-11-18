//! stdio JSONL RPC，协议形状参照 pi `coding-agent/src/modes/rpc/rpc-types.ts`：
//! stdin 每行一个命令 `{"id":"1","type":"prompt","message":"..."}`；
//! stdout 输出 `{"id":"1","type":"response","command":"prompt","success":true,"data":{...}}` 和事件行。
//! 严格按 LF 分帧；日志一律走 stderr。
use crate::config::Client;
use crate::hub::{Hub, SessionHandle, SessionRequest};
use crate::routing::Depth;
use crate::workdir::cli_cwd;
use agent_ai::ThinkingLevel;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
enum Command {
    /// reasoningEffort：粘性修改（从这条消息起）；depth：只在第一次 prompt 时有效
    Prompt {
        message: String,
        cwd: Option<PathBuf>,
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
    /// 回合预算：恰好一个 maxTurns（绝对值 >= 1）/ addTurns（相对值，可为负）/ stop:true（停止暂停在上限的 run）
    SetMaxTurns {
        max_turns: Option<u64>,
        add_turns: Option<i64>,
        stop: Option<bool>,
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
    let mut data = json!({"sessionId": session.id, "cwd": session.cwd, "sessionRoot": session.session_root, "persisted": session.hooks.path().is_some(), "isStreaming": session.agent.is_running(), "provider": session.provider_id(), "account": session.account, "model": session.model(), "api": session.effective_api().map(|a| a.as_str().to_string()).unwrap_or_else(|| hub.effective_api(&session.agent.config).to_string()), "profile": session.profile, "pendingApprovals": session.pending_approvals(), "maxTurns": session.max_turns(), "turnBudget": session.turn_budget()});
    data.as_object_mut().unwrap().extend(session.effort_fields());
    data
}

fn failure(error: impl std::fmt::Display) -> Value {
    json!({"success": false, "error": error.to_string()})
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
/// `resume` 恢复 `<sessionDir>/stdio/` 下的会话；`req.cwd` 若指定，只断言工作目录不变，推理强度作为粘性修改生效。
pub async fn run(hub: Arc<Hub>, req: SessionRequest, resume: Option<String>) -> anyhow::Result<()> {
    run_io(hub, req, resume, tokio::io::stdin(), tokio::io::stdout()).await
}

fn open_session(hub: &Hub, client: &Client, mut req: SessionRequest, resume: Option<&str>) -> anyhow::Result<Arc<SessionHandle>> {
    req.cwd = req.cwd.as_deref().map(cli_cwd).transpose()?;
    let Some(id) = resume else { return hub.create_session(client, req) };
    anyhow::ensure!(hub.client_session_dir(&client.name).is_some(), "session persistence is disabled (--no-persist-sessions / persistSessions: false); cannot resume");
    let session = hub.load_persisted(client, id)?.ok_or_else(|| anyhow::anyhow!("session {id} not found"))?;
    hub.check_session_cwd(&session, req.cwd.as_deref())?;
    if let Some(level) = req.reasoning_effort {
        session.set_reasoning_effort(Some(level));
    }
    Ok(session)
}

// Subscribe synchronously before the first prompt can emit an event.
fn forward_events(session: Arc<SessionHandle>, out: mpsc::UnboundedSender<Value>) -> tokio::task::JoinHandle<()> {
    let mut events = session.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => {
                    if let Ok(v) = serde_json::to_value(&ev) {
                        let _ = out.send(v);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => session.report_lag(n),
                Err(_) => break,
            }
        }
    })
}

pub(crate) async fn run_io(hub: Arc<Hub>, req: SessionRequest, resume: Option<String>, input: impl AsyncRead + Unpin, mut output: impl AsyncWrite + Unpin + Send + 'static) -> anyhow::Result<()> {
    let client = Client { name: "stdio".into(), token_sha256: String::new(), profiles: hub.config.profiles.keys().cloned().collect(), ..Default::default() };
    // A flag or resume pins the session immediately. Otherwise the first session
    // command can supply cwd, but invalid CLI options still fail before reading stdin.
    let start = if resume.is_some() || req.cwd.is_some() {
        open_session(&hub, &client, req, resume.as_deref()).map(|h| (None, Some(h)))
    } else {
        hub.preflight_session(&client, &req).map(|()| (Some(req), None))
    };
    let (mut pending, mut active) = match start {
        Ok(state) => state,
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

    let mut forwarder = active.as_ref().map(|h| forward_events(h.clone(), out_tx.clone()));
    let mut requests = tokio::task::JoinSet::new();
    let mut lines = BufReader::new(input).split(b'\n');
    let result = loop {
        let raw = match lines.next_segment().await {
            Ok(Some(raw)) => raw,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e.into()),
        };
        while requests.try_join_next().is_some() {}
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let parsed: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(e) => {
                let _ = out_tx.send(json!({"type":"response","command":"parse","success":false,"error":e.to_string()}));
                continue;
            }
        };
        let id = parsed.get("id").cloned();
        let cmd_name = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        if cmd_name != "prompt" && parsed.get("cwd").is_some() {
            send_response(&out_tx, id, cmd_name, failure("cwd is only accepted on prompt"));
            continue;
        }
        let command = match serde_json::from_value::<Command>(parsed) {
            Ok(command) => command,
            Err(e) => {
                send_response(&out_tx, id, cmd_name, failure(e));
                continue;
            }
        };
        if matches!(command, Command::GetAvailableModels) {
            let hub = hub.clone();
            let out = out_tx.clone();
            requests.spawn(async move {
                let resp = match hub.list_models(false).await {
                    Ok(models) => json!({"success": true, "data": {"models": models}}),
                    Err(e) => failure(e),
                };
                send_response(&out, id, cmd_name, resp);
            });
            continue;
        }
        let mut command = command;
        if let Some(mut req) = pending.take() {
            if let Command::Prompt { cwd, .. } = &mut command {
                req.cwd = cwd.take(); // cwd selects the new session; don't resolve it twice.
            }
            match open_session(&hub, &client, req, None) {
                Ok(h) => {
                    forwarder = Some(forward_events(h.clone(), out_tx.clone()));
                    active = Some(h);
                }
                Err(e) => {
                    send_response(&out_tx, id, cmd_name, failure(&e));
                    break Err(e);
                }
            }
        }
        let session = active.as_ref().expect("session is bound");
        let resp = match command {
            Command::Prompt { message, reasoning_effort, depth, cwd } => {
                let validated = (|| -> anyhow::Result<()> {
                    let cwd = cwd.as_deref().map(cli_cwd).transpose()?;
                    hub.check_session_cwd(session, cwd.as_deref())?;
                    if let Some(depth) = depth {
                        session.set_depth(depth)?;
                    }
                    Ok(())
                })();
                if let Err(e) = validated {
                    failure(e)
                } else {
                    if let Some(level) = reasoning_effort {
                        session.set_reasoning_effort(Some(level));
                    }
                    json!({"success": true, "data": {"disposition": session.submit(message, None)}})
                }
            }
            Command::SetReasoningEffort { level } => {
                json!({"success": true, "data": session.set_reasoning_effort(Some(level)).json()})
            }
            Command::Steer { message } => {
                json!({"success": true, "data": {"disposition": session.submit(message, Some("steer"))}})
            }
            Command::FollowUp { message } => {
                json!({"success": true, "data": {"disposition": session.submit(message, Some("follow_up"))}})
            }
            Command::Abort => {
                session.agent.abort();
                json!({"success": true})
            }
            Command::Approve { call_id, decision, reason } => match session.resolve_approval(&call_id, decision, reason) {
                Ok(p) => json!({"success": true, "data": {"resolved": true, "callId": p.call_id, "tool": p.tool}}),
                Err(e) => json!({"success": false, "error": e.to_string()}),
            },
            Command::SetMaxTurns { max_turns, add_turns, stop } => match (crate::turns::TurnsBody { max_turns, add_turns, stop }).op() {
                Err(e) => json!({"success": false, "error": e}),
                Ok(op) => match session.apply_turns(op) {
                    Ok(data) => json!({"success": true, "data": data}),
                    Err(e) => json!({"success": false, "error": e.to_string()}),
                },
            },
            Command::GetMessages => {
                let session = session.clone();
                let out = out_tx.clone();
                requests.spawn(async move {
                    let resp = json!({"success": true, "data": {"messages": session.agent.messages().await}});
                    send_response(&out, id, cmd_name, resp);
                });
                continue;
            }
            Command::GetState => json!({"success": true, "data": state(&hub, session)}),
            Command::Compact { instructions } => {
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
            Command::GetAvailableModels => unreachable!("handled before session binding"),
        };
        send_response(&out_tx, id, cmd_name, resp);
    };
    if let Some(session) = active {
        session.agent.abort();
    }
    requests.shutdown().await;
    if let Some(forwarder) = forwarder {
        forwarder.abort();
        let _ = forwarder.await;
    }
    drop(out_tx);
    let _ = writer.await;
    result
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
    async fn rpc_session(hub: Arc<Hub>, req: SessionRequest, resume: Option<String>, commands: Vec<(Value, bool)>) -> Vec<Value> {
        let (mut input, input_rx) = tokio::io::duplex(64 * 1024);
        let (output_tx, output) = tokio::io::duplex(1024 * 1024);
        let server = tokio::spawn(run_io(hub, req, resume, input_rx, output_tx));
        let mut lines = tokio::io::BufReader::new(output).lines();
        let mut responses = vec![];
        for (command, wait_settled) in commands {
            input.write_all(format!("{command}\n").as_bytes()).await.unwrap();
            let (mut response, mut settled) = (None, !wait_settled);
            while response.is_none() || !settled {
                let line = read_json(&mut lines).await;
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

    async fn read_json(lines: &mut tokio::io::Lines<BufReader<tokio::io::DuplexStream>>) -> Value {
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line()).await.expect("RPC response timed out").unwrap().expect("RPC closed");
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn rpc_first_prompt_cwd_binds_once() {
        use crate::sessions_tests::{hub, TempDir};
        use std::fs;
        let dir = TempDir::new();
        let workspace = dir.0.join("workspace");
        let cwd = dir.0.join("outside");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&cwd).unwrap();
        fs::write(cwd.join("marker"), "RPC external marker").unwrap();
        let cwd = fs::canonicalize(cwd).unwrap();
        let (hub, _) = hub(&dir, |cfg| cfg.profiles.get_mut("default").unwrap().workspace_roots = vec![workspace.clone()]);
        let result = rpc_session(
            hub.clone(),
            SessionRequest::default(),
            None,
            vec![
                (json!({"id":"1", "type":"prompt", "cwd":cwd, "message": r#"/tool read {"path":"marker"}"#}), true),
                (json!({"id":"2", "type":"get_state"}), false),
                (json!({"id":"3", "type":"prompt", "cwd":cwd.join("."), "message":"same cwd"}), true),
                (json!({"id":"4", "type":"prompt", "cwd":workspace, "message":"rejected", "reasoningEffort":"high"}), false),
                (json!({"id":"5", "type":"get_state"}), false),
                (json!({"id":"6", "type":"get_messages"}), false),
            ],
        )
        .await;
        assert_eq!(result[0]["success"], true);
        let state = &result[1]["data"];
        assert_eq!((state["cwd"].clone(), state["sessionRoot"].clone()), (json!(cwd), json!(cwd)));
        assert_eq!(result[2]["success"], true);
        assert_eq!(result[3]["success"], false);
        assert!(result[3]["error"].as_str().unwrap().starts_with("cwd is fixed"));
        assert_eq!(result[4]["data"]["sessionId"], state["sessionId"]);
        assert_eq!(result[4]["data"]["requestedReasoningEffort"], state["requestedReasoningEffort"]);
        let messages = result[5]["data"]["messages"].to_string();
        assert!(messages.contains("tool read returned: RPC external marker"));
        assert!(!messages.contains("rejected"));
        assert_eq!(agent_session::list_files(&dir.sessions().join("stdio")).len(), 1);
    }

    #[tokio::test]
    async fn rpc_cwd_flag_default_and_resume_assertions() {
        use crate::sessions_tests::{hub, TempDir};
        use std::fs;
        let dir = TempDir::new();
        let workspace = dir.0.join("workspace");
        let sub = workspace.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let workspace = fs::canonicalize(workspace).unwrap();
        let sub = fs::canonicalize(sub).unwrap();
        let edit = |cfg: &mut crate::config::Config| cfg.profiles.get_mut("default").unwrap().workspace_roots = vec![workspace.clone()];
        let (first, _) = hub(&dir, edit);
        let r =
            rpc_session(first.clone(), SessionRequest::default(), None, vec![(json!({"id":1, "type":"get_state"}), false), (json!({"id":2, "type":"prompt", "cwd":sub, "message":"too late"}), false)])
                .await;
        assert_eq!(r[0]["data"]["cwd"], json!(workspace));
        assert_eq!(r[1]["success"], false);
        let r = rpc_session(
            first.clone(),
            SessionRequest { cwd: Some(sub.clone()), ..Default::default() },
            None,
            vec![(json!({"id":1, "type":"prompt", "cwd":workspace, "message":"flag already bound"}), false), (json!({"id":2, "type":"get_state"}), false)],
        )
        .await;
        assert_eq!(r[0]["success"], false);
        let state = &r[1]["data"];
        assert_eq!((state["cwd"].clone(), state["sessionRoot"].clone()), (json!(sub), json!(workspace)));
        let id = state["sessionId"].as_str().unwrap();
        drop(first);

        let (reopened, _) = hub(&dir, edit);
        let req = SessionRequest { cwd: Some(workspace.clone()), ..Default::default() };
        let line = start_error(reopened.clone(), req, Some(id.into())).await;
        assert!(line["error"].as_str().unwrap().starts_with("cwd is fixed"));
        let r = rpc_session(
            reopened,
            SessionRequest { cwd: Some(sub.join(".")), ..Default::default() },
            Some(id.into()),
            vec![
                (json!({"id":1, "type":"prompt", "cwd":workspace, "message":"flag not ignored"}), false),
                (json!({"id":2, "type":"prompt", "cwd":sub, "message":"matching assertion"}), true),
                (json!({"id":3, "type":"get_state"}), false),
            ],
        )
        .await;
        assert_eq!(r[0]["success"], false);
        assert_eq!(r[1]["success"], true);
        assert_eq!(r[2]["data"]["sessionId"], id);
        assert_eq!(r[2]["data"]["sessionRoot"], state["sessionRoot"]);
        assert_eq!(agent_session::list_files(&dir.sessions().join("stdio")).len(), 2);
    }

    async fn start_error(hub: Arc<Hub>, req: SessionRequest, resume: Option<String>) -> Value {
        let (_input, input_rx) = tokio::io::duplex(1024); // deliberately leave stdin open
        let (output_tx, output) = tokio::io::duplex(1024);
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), run_io(hub, req, resume, input_rx, output_tx)).await.expect("eager validation").unwrap_err();
        let mut lines = BufReader::new(output).lines();
        let line = read_json(&mut lines).await;
        assert_eq!(line, json!({"type":"response", "command":"start", "success":false, "error":err.to_string()}));
        line
    }

    #[tokio::test]
    async fn rpc_invalid_first_cwd_returns_correlated_failure() {
        use crate::sessions_tests::{config, TempDir};
        let dir = TempDir::new();
        let mut cfg = config(&dir.0);
        // Without a usable default, any early attempt to bind is observable. An
        // absolute first prompt could still choose a directory under the any policy.
        cfg.profiles.get_mut("default").unwrap().workspace_roots = vec![dir.0.join("missing default")];
        let mut hub = Hub::new(cfg, "faux", Some(dir.sessions()), None).unwrap();
        // get_available_models enumerates all loaded providers; keep it offline even
        // if the environment configured an optional provider on this machine.
        Arc::get_mut(&mut hub).unwrap().providers.retain(|key, _| key.provider == "faux");
        for (req, expected) in [
            (SessionRequest { profile: Some("unknown".into()), ..Default::default() }, "profile 'unknown' is not allowed"),
            (SessionRequest { provider: Some("unknown".into()), ..Default::default() }, "unknown provider unknown"),
            (SessionRequest { max_turns: Some(0), ..Default::default() }, "maxTurns must be >= 1"),
            (SessionRequest { cwd: Some(PathBuf::new()), ..Default::default() }, "cwd must not be empty"),
        ] {
            assert!(start_error(hub.clone(), req, None).await["error"].as_str().unwrap().contains(expected));
        }
        let (mut input, input_rx) = tokio::io::duplex(4096);
        let (output_tx, output) = tokio::io::duplex(4096);
        let server = tokio::spawn(run_io(hub.clone(), SessionRequest::default(), None, input_rx, output_tx));
        let mut lines = BufReader::new(output).lines();
        input.write_all(b" \r\n{\"broken\":\n").await.unwrap();
        assert_eq!(read_json(&mut lines).await["command"], "parse");
        for command in
            [json!({"id":"ignored-cwd", "type":"get_state", "cwd":"."}), json!({"id":"bad-prompt", "type":"prompt", "message":"hi", "cwd":13}), json!({"id":"models", "type":"get_available_models"})]
        {
            input.write_all(format!("{command}\n").as_bytes()).await.unwrap();
            let line = read_json(&mut lines).await;
            assert_eq!(line["id"], command["id"]);
            match command["id"].as_str().unwrap() {
                "ignored-cwd" => assert_eq!(line["error"], "cwd is only accepted on prompt"),
                "bad-prompt" => assert_eq!(line["success"], false),
                "models" => assert_eq!(line["data"]["models"][0]["id"], "faux"),
                _ => unreachable!(),
            }
            assert!(hub.sessions.lock().unwrap().is_empty());
        }
        input.write_all(format!("{}\n", json!({"id":42, "type":"prompt", "cwd":dir.0.join("invalid"), "message":"hi"})).as_bytes()).await.unwrap();
        let line = read_json(&mut lines).await;
        assert_eq!(line, json!({"id":42, "type":"response", "command":"prompt", "success":false, "error":"cwd does not exist"}));
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), server).await.expect("first failure exits").unwrap().unwrap_err();
        assert_eq!(err.to_string(), "cwd does not exist");
        assert!(lines.next_line().await.unwrap().is_none());
        assert!(agent_session::list_files(&dir.sessions().join("stdio")).is_empty());
        assert!(hub.sessions.lock().unwrap().is_empty());

        // EOF before any session command is a clean exit, still without a session.
        let (input, input_rx) = tokio::io::duplex(1024);
        let (output_tx, _output) = tokio::io::duplex(1024);
        drop(input);
        tokio::time::timeout(std::time::Duration::from_secs(5), run_io(hub.clone(), SessionRequest::default(), None, input_rx, output_tx)).await.unwrap().unwrap();
        assert!(agent_session::list_files(&dir.sessions().join("stdio")).is_empty());
        assert!(hub.sessions.lock().unwrap().is_empty());
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
            SessionRequest::default(),
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
        let r =
            rpc_session(hub2.clone(), SessionRequest::default(), Some(id.clone()), vec![(json!({"id": "1", "type": "get_state"}), false), (json!({"id": "2", "type": "get_messages"}), false)]).await;
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
