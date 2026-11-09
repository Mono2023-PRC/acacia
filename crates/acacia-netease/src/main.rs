//! `acacia-netease`: a Model Context Protocol (MCP) server that lets an LLM drive a Minecraft:
//! Bedrock bot through [`acacia_bot`].
//!
//! The bot joins a local Go proxy (Shuttler) that translates the session to a NetEase Bedrock
//! rental server; the proxy listens on `127.0.0.1:19132` with authentication disabled, so the bot
//! connects offline. The transport is MCP's stdio framing: one JSON-RPC object per line on stdin
//! and stdout, with logs on stderr — nothing else may reach stdout.
//!
//! One owner task holds the [`acacia_bot::Bot`] (it owns its state and must not be shared), driven
//! by commands on an mpsc channel. A shared snapshot keeps `netease_status` answerable while a long
//! action is running, and each request is handled on its own task so it cannot queue behind one.

mod bot;
mod tools;

use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::{mpsc, oneshot};

use bot::Shared;

/// Protocol version answered when the client does not name one.
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let local = tokio::task::LocalSet::new();
    local.run_until(serve()).await;
    eprintln!("[acacia-netease] stdin closed, exiting");
}

async fn serve() {
    // One writer owns stdout: responses from concurrent request tasks never interleave.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::task::spawn_local(writer_loop(out_rx));

    let snapshot: Shared = bot::shared();
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let owner = tokio::task::spawn_local(bot::run(cmd_rx, snapshot.clone()));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                // Requests run concurrently, so `netease_status` still answers while a move runs.
                tokio::task::spawn_local(handle_line(line, cmd_tx.clone(), snapshot.clone(), out_tx.clone()));
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("[acacia-netease] stdin error: {e}");
                break;
            }
        }
    }

    // Ask the owner to disconnect, then let the writer flush whatever is still queued.
    let (reply, _) = oneshot::channel();
    let _ = cmd_tx.send(bot::Command::Shutdown { reply }).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), owner).await;
    drop(cmd_tx);
    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
}

/// Writes each queued message as one JSON line and flushes it.
async fn writer_loop(mut rx: mpsc::UnboundedReceiver<Value>) {
    let mut out = BufWriter::new(tokio::io::stdout());
    while let Some(message) = rx.recv().await {
        if out.write_all(message.to_string().as_bytes()).await.is_err()
            || out.write_all(b"\n").await.is_err()
            || out.flush().await.is_err()
        {
            break;
        }
    }
}

/// Handles one stdin line: parses the JSON-RPC envelope and writes at most one response.
async fn handle_line(line: String, cmd_tx: mpsc::Sender<bot::Command>, snapshot: Shared, out: mpsc::UnboundedSender<Value>) {
    let request: Value = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(e) => return send(&out, rpc_error(Value::Null, -32700, format!("parse error: {e}"))),
    };
    let Some(object) = request.as_object() else {
        return send(&out, rpc_error(Value::Null, -32600, "invalid request: not a JSON object".to_owned()));
    };
    let Some(method) = object.get("method").and_then(Value::as_str).map(str::to_owned) else {
        // A response to a server request; this server sends none.
        return;
    };
    // Notifications (no `id`) never get a reply.
    let Some(id) = object.get("id").cloned() else {
        return;
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);

    let outcome: Result<Value, (i64, String)> = match method.as_str() {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools::list()),
        "tools/call" => tools::call(&params, &cmd_tx, &snapshot).await,
        other => Err((-32601, format!("method not found: {other}"))),
    };
    match outcome {
        Ok(result) => send(&out, json!({"jsonrpc": "2.0", "id": id, "result": result})),
        Err((code, message)) => send(&out, rpc_error(id, code, message)),
    }
}

/// `initialize`: echo the client's protocol version and advertise the tools capability.
fn initialize_result(params: &Value) -> Value {
    let protocol_version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "acacia-netease", "version": env!("CARGO_PKG_VERSION")},
        "instructions": "Drives a Minecraft: Bedrock bot through the local Shuttler proxy (offline authentication). Call netease_connect first, then netease_move_to / netease_dig / netease_chat / netease_look; netease_status returns the current snapshot at any time.",
    })
}

fn send(out: &mpsc::UnboundedSender<Value>, message: Value) {
    let _ = out.send(message);
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
