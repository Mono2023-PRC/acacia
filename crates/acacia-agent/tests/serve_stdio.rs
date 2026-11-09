//! `acacia-agent serve` as a *subprocess*: the contract a driver such as `run_caps.py --backend
//! agent` relies on.
//!
//! The regression this pins: the driver appends its own flags before the subcommand
//! (`acacia-agent --allow perceive,act,chat serve --address ... --name ...`), and a CLI that only
//! looked at `argv[0]` answered "unknown command \"--allow\"" and exited before `initialize` — which
//! the driver reports as "MCP process exited".

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Serve {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Serve {
    /// Spawns the binary exactly the way the capability runner does, against a port with no server.
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_acacia-agent"))
            .args(["--allow", "perceive,act,chat", "serve", "--address", "127.0.0.1:1", "--name", "stdio"]
                .iter()
                .copied()
                .collect::<Vec<_>>())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn acacia-agent serve");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Serve { child, stdin, lines }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").expect("write request");
        self.stdin.flush().expect("flush request");
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.response(id)
    }

    /// Reads until the response with `id` arrives; every line on stdout must be a JSON-RPC frame.
    fn response(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no answer for id {id} (stdout closed or silent)"));
            let value: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("stdout must carry only JSON-RPC frames, got {line:?}: {error}"));
            if value.get("id") == Some(&json!(id)) {
                return value;
            }
        }
    }

    fn shutdown(mut self) -> i32 {
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match self.child.try_wait().expect("wait") {
                Some(status) => return status.code().unwrap_or(-1),
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                None => {
                    let _ = self.child.kill();
                    panic!("serve did not exit after stdin EOF");
                }
            }
        }
    }
}

#[test]
fn serve_answers_initialize_before_the_game_connection_exists() {
    let mut serve = Serve::spawn();

    let init = serve.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "run_caps", "version": "1"},
        }),
    );
    let result = init.get("result").unwrap_or_else(|| panic!("initialize failed: {init}"));
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "acacia-agent");

    // A driver that gives the notification an id still gets an answer instead of a 5 s stall.
    let ack = serve.request(2, "notifications/initialized", json!({}));
    assert!(ack.get("result").is_some(), "initialized answered: {ack}");

    let list = serve.request(3, "tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().expect("tools").clone();
    // 23 tools, `chat` enabled by the `--allow` the driver passes.
    assert_eq!(tools.len(), 27, "tools/list count");
    let names: Vec<&str> = tools.iter().filter_map(|tool| tool["name"].as_str()).collect();
    for expected in ["status", "observe", "move_to", "dig", "sleep", "pickup", "stop"] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
    assert!(names.iter().all(|name| !name.starts_with("netease_")), "names are unprefixed: {names:?}");
    for tool in &tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{} schema", tool["name"]);
    }

    let status = serve.request(4, "tools/call", json!({"name": "status", "arguments": {}}));
    let result = &status["result"];
    assert_eq!(result["isError"], false, "status is not an error: {status}");
    let payload = &result["structuredContent"];
    assert_eq!(payload["ok"], true, "success results carry ok:true: {status}");
    // No server on 127.0.0.1:1: the process keeps serving and says so.
    assert_eq!(payload["connected"], false, "status: {payload}");
    assert!(payload.get("last_error").is_some(), "status carries last_error: {payload}");

    // Still alive after all of that.
    assert!(serve.child.try_wait().expect("wait").is_none(), "serve exited mid-session");

    // An action tool answers `disconnected` instead of dying.
    let observe = serve.request(5, "tools/call", json!({"name": "observe", "arguments": {"radius": 4}}));
    assert_eq!(observe["result"]["isError"], true, "observe while disconnected: {observe}");
    assert_eq!(observe["result"]["structuredContent"]["reason"], "disconnected");

    assert_eq!(serve.shutdown(), 0, "stdin EOF is a clean shutdown");
}

#[test]
fn a_bad_address_does_not_stop_the_server() {
    let mut serve = Serve::spawn();
    // `serve` connects in the background; a name-only call must still work while it retries.
    let list = serve.request(1, "tools/list", json!({}));
    assert_eq!(list["result"]["tools"].as_array().expect("tools").len(), 27);
    assert_eq!(serve.shutdown(), 0);
}
