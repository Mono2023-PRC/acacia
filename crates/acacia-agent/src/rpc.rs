//! The stdio JSON-RPC server (`acacia-agent serve`).
//!
//! Framing follows `crates/acacia-netease/src/main.rs`: one JSON object per line, one writer task
//! owns stdout, requests run on their own local tasks (so `ping` answers during a long `agent/run`),
//! notifications get no reply, and protocol errors use `-32700/-32600/-32601/-32602`.
//!
//! Methods: `initialize`, `ping`, `tools/list`, `tools/call`, `connect`, `agent/run`, `agent/status`,
//! `agent/cancel`, `agent/trace`. `agent/run` is asynchronous and reports progress through
//! `agent/event` notifications.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::mpsc;

use crate::agent::CancelFlag;
use crate::config::Config;
use crate::runner::{self, RunOptions};
use crate::shell::ShellHandle;
use crate::tools::Registry;

const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// What `agent/status` answers from.
#[derive(Debug, Clone)]
pub struct RunStatus {
    pub id: u64,
    pub state: &'static str,
    pub task: String,
    pub step: u32,
    pub last_tool: Option<String>,
    pub answer: Option<String>,
    pub trace: Option<String>,
    pub error: Option<String>,
    pub tokens: u64,
}

impl Default for RunStatus {
    fn default() -> Self {
        Self {
            id: 0,
            state: "idle",
            task: String::new(),
            step: 0,
            last_tool: None,
            answer: None,
            trace: None,
            error: None,
            tokens: 0,
        }
    }
}

type SharedStatus = Arc<Mutex<RunStatus>>;

pub async fn serve(shell: ShellHandle, config: Arc<Config>) -> crate::Result<()> {
    // One writer owns stdout: responses from concurrent request tasks never interleave.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::task::spawn_local(writer_loop(out_rx));

    let status: SharedStatus = Arc::new(Mutex::new(RunStatus::default()));
    let cancel: Arc<Mutex<Option<CancelFlag>>> = Arc::new(Mutex::new(None));
    let runs = Arc::new(AtomicU64::new(0));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                tokio::task::spawn_local(handle_line(
                    line,
                    shell.clone(),
                    config.clone(),
                    status.clone(),
                    cancel.clone(),
                    runs.clone(),
                    out_tx.clone(),
                ));
            }
            Ok(None) => break,
            Err(error) => {
                eprintln!("[acacia-agent] stdin error: {error}");
                break;
            }
        }
    }

    // Cancel any run in flight, close the connection, let the writer flush.
    if let Some(flag) = cancel.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref() {
        flag.store(true, Ordering::Release);
    }
    shell.shutdown().await;
    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    Ok(())
}

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

#[allow(clippy::too_many_arguments)]
async fn handle_line(
    line: String,
    shell: ShellHandle,
    config: Arc<Config>,
    status: SharedStatus,
    cancel: Arc<Mutex<Option<CancelFlag>>>,
    runs: Arc<AtomicU64>,
    out: mpsc::UnboundedSender<Value>,
) {
    let request: Value = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(error) => return send(&out, rpc_error(Value::Null, -32700, format!("parse error: {error}"))),
    };
    let Some(object) = request.as_object() else {
        return send(&out, rpc_error(Value::Null, -32600, "invalid request: not a JSON object".to_owned()));
    };
    let Some(method) = object.get("method").and_then(Value::as_str).map(str::to_owned) else {
        // A response to a server request; this server sends none.
        return;
    };
    let Some(id) = object.get("id").cloned() else {
        // Notifications never get a reply.
        return;
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);

    let outcome: Result<Value, (i64, String)> = match method.as_str() {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        // MCP sends this as a notification; a driver that gives it an id still gets an answer, so
        // it does not sit on a timeout before its first tool call.
        "notifications/initialized" | "initialized" => Ok(json!({})),
        "tools/list" => Ok(registry(&config).list()),
        "tools/call" => tools_call(&params, &shell, &config).await,
        "connect" => connect(&params, &shell, &config).await,
        "agent/run" => agent_run(&params, config.clone(), shell.clone(), status.clone(), cancel.clone(), runs.clone(), out.clone()),
        "agent/status" => Ok(status_snapshot(&status)),
        "agent/cancel" => {
            let flag = cancel.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
            match flag {
                Some(flag) => {
                    flag.store(true, Ordering::Release);
                    Ok(json!({"cancelled": true}))
                }
                None => Ok(json!({"cancelled": false})),
            }
        }
        "agent/trace" => {
            let status = status.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            Ok(json!({"path": status.trace}))
        }
        other => Err((-32601, format!("method not found: {other}"))),
    };
    match outcome {
        Ok(result) => send(&out, json!({"jsonrpc": "2.0", "id": id, "result": result})),
        Err((code, message)) => send(&out, rpc_error(id, code, message)),
    }
}

fn registry(config: &Config) -> Registry {
    Registry::new(&config.tools)
}

fn initialize_result(params: &Value) -> Value {
    let protocol_version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "acacia-agent", "version": env!("CARGO_PKG_VERSION")},
    })
}

async fn connect(params: &Value, shell: &ShellHandle, config: &Config) -> Result<Value, (i64, String)> {
    let address = params.get("address").and_then(Value::as_str).unwrap_or(&config.address).to_owned();
    let name = params.get("name").and_then(Value::as_str).unwrap_or(&config.name).to_owned();
    let timeout_s = params.get("timeout_s").and_then(Value::as_u64).unwrap_or(60);
    shell
        .connect(address, name, timeout_s)
        .await
        .map_err(|failure| (-32000, failure.message))
}

async fn tools_call(params: &Value, shell: &ShellHandle, config: &Config) -> Result<Value, (i64, String)> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err((-32602, "invalid params: missing tool name".to_owned()));
    };
    let arguments = params.get("arguments").cloned();
    let resolved = registry(config).resolve(name, arguments.as_ref());
    let call = match resolved {
        Ok(call) => call,
        Err(failure) => return Ok(tool_error(failure.to_json())),
    };
    match shell.call(call).await {
        Ok(data) => Ok(tool_ok(with_ok(data))),
        Err(failure) => Ok(tool_error(failure.to_json())),
    }
}

/// Successful tool results carry `ok: true` next to their own fields (failures carry
/// `{ok:false, reason, message, hint}`), so a driver can tell them apart without reading `isError`.
fn with_ok(data: Value) -> Value {
    match data {
        Value::Object(mut map) => {
            map.insert("ok".to_owned(), Value::Bool(true));
            Value::Object(map)
        }
        other => json!({"ok": true, "data": other}),
    }
}

#[allow(clippy::too_many_arguments)]
fn agent_run(
    params: &Value,
    config: Arc<Config>,
    shell: ShellHandle,
    status: SharedStatus,
    cancel: Arc<Mutex<Option<CancelFlag>>>,
    runs: Arc<AtomicU64>,
    out: mpsc::UnboundedSender<Value>,
) -> Result<Value, (i64, String)> {
    if status.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).state == "running" {
        return Err((-32001, "a run is already in progress; agent/cancel it first".to_owned()));
    }
    let Some(task) = params.get("task").and_then(Value::as_str).map(str::to_owned) else {
        return Err((-32602, "invalid params: missing task".to_owned()));
    };
    let mut limits = config.limits();
    if let Some(overrides) = params.get("limits").and_then(Value::as_object) {
        if let Some(steps) = overrides.get("max_steps").and_then(Value::as_u64) {
            limits.max_steps = (steps as u32).clamp(1, 200);
        }
        if let Some(secs) = overrides.get("wall_secs").and_then(Value::as_u64) {
            limits.wall_clock = Duration::from_secs(secs.max(1));
        }
        if let Some(allow) = overrides.get("allow").and_then(Value::as_array) {
            limits.allow = Some(allow.iter().filter_map(Value::as_str).map(str::to_owned).collect());
        }
    }
    let run_id = runs.fetch_add(1, Ordering::AcqRel) + 1;
    let flag: CancelFlag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut status = status.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *status = RunStatus { id: run_id, state: "running", task: task.clone(), ..RunStatus::default() };
    }
    *cancel.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(flag.clone());

    tokio::task::spawn_local(async move {
        let options = RunOptions { limits, cancel: flag, verbose: false };
        let outcome = runner::run_task(&shell, &config, &task, options).await;
        let mut status = status.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match outcome {
            Ok(report) => {
                status.state = match report.stop {
                    crate::agent::Stop::Answer => "done",
                    crate::agent::Stop::Cancelled => "cancelled",
                    _ => "stopped",
                };
                status.answer = Some(report.answer.clone());
                status.step = report.steps;
                status.tokens = report.usage.total();
                status.trace = Some(report.trace.display().to_string());
                let _ = out.send(json!({
                    "jsonrpc": "2.0",
                    "method": "agent/event",
                    "params": {"run_id": status.id, "kind": "answer", "text": report.answer, "step": status.step},
                }));
            }
            Err(error) => {
                status.state = "failed";
                status.error = Some(error.to_string());
                let _ = out.send(json!({
                    "jsonrpc": "2.0",
                    "method": "agent/event",
                    "params": {"run_id": status.id, "kind": "error", "text": error.to_string()},
                }));
            }
        }
    });

    Ok(json!({"run_id": run_id}))
}

fn status_snapshot(status: &SharedStatus) -> Value {
    let status = status.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    json!({
        "run_id": status.id,
        "state": status.state,
        "task": status.task,
        "step": status.step,
        "last_tool": status.last_tool,
        "answer": status.answer,
        "error": status.error,
        "trace": status.trace,
        "tokens": status.tokens,
    })
}

fn tool_ok(value: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": value.to_string()}],
        "structuredContent": value,
        "isError": false,
    })
}

fn tool_error(value: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": value.to_string()}],
        "structuredContent": value,
        "isError": true,
    })
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn send(out: &mpsc::UnboundedSender<Value>, message: Value) {
    let _ = out.send(message);
}
