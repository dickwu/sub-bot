//! stdio JSONL RPC，协议形状参照 pi `coding-agent/src/modes/rpc/rpc-types.ts`：
//! stdin 每行一个命令 `{"id":"1","type":"prompt","message":"..."}`；
//! stdout 输出 `{"id":"1","type":"response","command":"prompt","success":true,"data":{...}}` 和事件行。
//! 严格按 LF 分帧；日志一律走 stderr。
use crate::hub::{Hub, SessionRequest};
use crate::config::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Command {
    Prompt { message: String },
    Steer { message: String },
    FollowUp { message: String },
    Abort,
    GetMessages,
    GetState,
}

/// stdio 的调用方就是父进程本身，视为可用全部 profile 的本地 client。
pub async fn run(hub: Arc<Hub>, profile: Option<String>) -> anyhow::Result<()> {
    let client = Client { name: "stdio".into(), token_sha256: String::new(), profiles: hub.config.profiles.keys().cloned().collect() };
    let session = hub.create_session(&client, SessionRequest { profile, ..Default::default() })?;
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();

    // 单一 writer，保证每条记录整行写出
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(v) = out_rx.recv().await {
            let mut line = serde_json::to_vec(&v).unwrap_or_default();
            line.push(b'\n');
            if stdout.write_all(&line).await.is_err() { break; }
            let _ = stdout.flush().await;
        }
    });

    // 事件转发
    let mut events = session.subscribe();
    let ev_tx = out_tx.clone();
    let forwarder = tokio::spawn(async move {
        while let Ok(ev) = events.recv().await {
            if let Ok(v) = serde_json::to_value(&ev) { let _ = ev_tx.send(v); }
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).split(b'\n');
    while let Some(raw) = lines.next_segment().await? {
        let text = String::from_utf8_lossy(&raw);
        let text = text.trim_end_matches('\r');
        if text.trim().is_empty() { continue; }
        let parsed: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => { let _ = out_tx.send(json!({"type":"response","command":"parse","success":false,"error":e.to_string()})); continue; }
        };
        let id = parsed.get("id").cloned();
        let cmd_name = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let resp = match serde_json::from_value::<Command>(parsed) {
            Err(e) => json!({"success": false, "error": e.to_string()}),
            Ok(Command::Prompt { message }) => json!({"success": true, "data": {"disposition": session.submit(message, None)}}),
            Ok(Command::Steer { message }) => json!({"success": true, "data": {"disposition": session.submit(message, Some("steer"))}}),
            Ok(Command::FollowUp { message }) => json!({"success": true, "data": {"disposition": session.submit(message, Some("follow_up"))}}),
            Ok(Command::Abort) => { session.agent.abort(); json!({"success": true}) }
            Ok(Command::GetMessages) => json!({"success": true, "data": {"messages": session.agent.messages().await}}),
            Ok(Command::GetState) => json!({"success": true, "data": {"sessionId": session.id, "isStreaming": session.agent.is_running(), "model": session.agent.config.model}}),
        };
        let mut r = json!({"type": "response", "command": cmd_name});
        if let Some(id) = id { r["id"] = id; }
        r.as_object_mut().unwrap().extend(resp.as_object().unwrap().clone());
        let _ = out_tx.send(r);
    }
    // stdin 关闭 = 有序退出
    session.agent.abort();
    forwarder.abort();
    let _ = forwarder.await;
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}
