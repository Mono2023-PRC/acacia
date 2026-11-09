//! The acceptance loop: a **real** OpenAI-compatible HTTP client against a **fake** provider, driving
//! a fake toolbox. This is the "perceive → decide → call a tool → read the result" path the task
//! requires, proven without a key and without a Minecraft server.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use acacia_agent::agent::{system_prompt, Agent, Limits, Stop, Trace};
use acacia_agent::config::{ChatConfig, LlmConfig, SafetyConfig, ToolsConfig};
use acacia_agent::llm::mock::{answer_reply, tool_reply, truncated_reply, wire_body, MockServer};
use acacia_agent::llm::openai::OpenAiClient;
use acacia_agent::tools::{Call, Failure, Registry, ToolBox};
use serde_json::{json, Value};

#[derive(Default, Clone)]
struct ScriptedTools {
    calls: Arc<Mutex<Vec<String>>>,
    states: Arc<Mutex<u32>>,
    /// Fail call number `n` (1-based) with `disconnected`; `None` never does.
    disconnect_at: Arc<Mutex<Option<u32>>>,
    reconnects: Arc<Mutex<u32>>,
}

/// A body shaped like the real one: this is what used to stay in the history forever.
fn observed_body() -> Value {
    json!({
        "tick": 12,
        "me": {"pos": [0.0, 64.0, 0.0], "block": [0, 64, 0], "health": 20.0, "food": 18.0},
        "held": {"item": "minecraft:iron_axe", "count": 1},
        "inventory": (0..10).map(|index| json!({"item": format!("minecraft:item{index}"), "count": index + 1})).collect::<Vec<_>>(),
        "armor": [null, null, null, null],
        "entities": (0..8).map(|index| json!({"id": index, "kind": "minecraft:zombie", "dist": 6.0, "hostile": true})).collect::<Vec<_>>(),
        "resources": (0..8).map(|index| json!({"block": "minecraft:oak_log", "nearest": [index, 64, 2], "dist": 3.0, "count": 4})).collect::<Vec<_>>(),
        "window": null,
        "scan": {"radius": 8, "y": [60, 68], "solid_cells": 120, "available": true},
    })
}

impl ScriptedTools {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl ToolBox for ScriptedTools {
    async fn call(&mut self, call: Call) -> Result<Value, Failure> {
        let mut calls = self.calls.lock().expect("calls lock");
        calls.push(call.name().to_owned());
        let count = calls.len() as u32;
        drop(calls);
        if self.disconnect_at.lock().expect("disconnect lock").is_some_and(|at| at == count) {
            return Err(Failure::disconnected("the connection dropped: ServerClosed"));
        }
        match call {
            Call::Observe { .. } => Ok(observed_body()),
            Call::MoveTo { .. } => Ok(json!({"arrived": true, "block_position": [10, 64, 20]})),
            Call::Dig { .. } => Err(Failure::new("no_item", "not possible: nothing held")),
            Call::Sleep { .. } => Err(Failure::new("no_block", "no bed within 8 blocks")),
            Call::Pickup { .. } => Err(Failure::new("no_block", "no dropped item within 6 blocks")),
            Call::Stop => Ok(json!({"stopped": true, "activity": "idle"})),
            _ => Ok(json!({"queued": false})),
        }
    }

    async fn state(&mut self) -> Value {
        *self.states.lock().expect("states lock") += 1;
        json!({"tick": 7, "connected": true, "delta": {"moved": [1.0, 0.0, 0.0], "items_gained": [{"item": "minecraft:oak_log", "count": 1}]}})
    }

    async fn reconnect(&mut self) -> bool {
        *self.reconnects.lock().expect("reconnects lock") += 1;
        false
    }
}

struct Fixture {
    agent: Agent<OpenAiClient, ScriptedTools>,
    tools: ScriptedTools,
    server: MockServer,
    runs_dir: std::path::PathBuf,
}

async fn fixture(replies: Vec<Value>, limits: Limits, allow: Option<Vec<String>>) -> Fixture {
    fixture_with_budget(replies, limits, allow, None).await
}

/// The same fixture, with the reply budget the client will claim (t121).
async fn fixture_with_budget(
    replies: Vec<Value>,
    limits: Limits,
    allow: Option<Vec<String>>,
    budget: Option<(u32, &str)>,
) -> Fixture {
    let server = MockServer::start(replies).await.expect("mock server");
    let mut llm = LlmConfig::default();
    llm.base_url = server.base_url().to_owned();
    llm.model = "mock-model".to_owned();
    llm.timeout_s = 10;
    if let Some((tokens, variable)) = budget {
        llm.max_tokens = tokens;
        llm.max_tokens_source = Some(variable.to_owned());
    }
    let client = OpenAiClient::new(&llm, "test-key".to_owned()).expect("client");
    let mut registry = Registry::new(&ToolsConfig::default());
    if allow.is_some() {
        registry = registry.with_run_allow(allow);
    }
    let tools = ScriptedTools::default();
    // Under `target/`, so no test writes into the source tree and no temp-dir ACL gets in the way.
    let port = server.base_url().rsplit(':').next().unwrap_or("0").trim_end_matches("/v1");
    let runs_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/test-runs")
        .join(format!("{}-{port}", std::process::id()));
    let agent = Agent {
        llm: client,
        tools: tools.clone(),
        registry,
        limits,
        system: system_prompt(&ChatConfig::default()),
        chat: ChatConfig::default(),
        safety: SafetyConfig::default(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    Fixture { agent, tools, server, runs_dir }
}

#[tokio::test(flavor = "current_thread")]
async fn perceive_decide_tool_result_loop_runs_end_to_end() {
    let mut fixture = fixture(
        vec![
            wire_body(&tool_reply("c1", "observe", json!({"radius": 8}))),
            wire_body(&tool_reply("c2", "move_to", json!({"xz": [10, 20]}))),
            wire_body(&answer_reply("collected nothing yet; I moved to 10,20")),
        ],
        Limits { max_steps: 10, ..Limits::default() },
        None,
    )
    .await;

    let mut trace = Trace::open(&fixture.runs_dir, "collect 8 oak logs", json!({"test": true})).expect("trace");
    let report = fixture.agent.run("collect 8 oak logs", &mut trace).await.expect("run");

    assert_eq!(report.stop, Stop::Answer);
    assert_eq!(report.steps, 3, "observe, move_to, answer");
    assert_eq!(report.tool_calls, 2);
    assert!(report.answer.contains("10,20"));
    assert_eq!(fixture.tools.calls(), vec!["observe".to_owned(), "move_to".to_owned()]);
    // Step 2 and 3 each got a state message.
    assert_eq!(*fixture.tools.states.lock().expect("states"), 2);

    // The provider really was asked, with the full tool list and the tool result in the history.
    let requests = fixture.server.requests();
    assert_eq!(requests.len(), 3);
    let first = &requests[0];
    assert_eq!(first["model"], "mock-model");
    assert_eq!(first["tools"].as_array().expect("tools").len(), 26);
    assert_eq!(first["messages"][0]["role"], "system");
    assert_eq!(first["messages"][1]["role"], "user");

    let second = &requests[1];
    let messages = second["messages"].as_array().expect("messages");
    assert!(messages.iter().any(|message| message["role"] == "assistant" && message["tool_calls"].is_array()));
    let tool_message = messages.iter().find(|message| message["role"] == "tool").expect("a tool result");
    assert_eq!(tool_message["tool_call_id"], "c1");
    assert!(tool_message["content"].as_str().expect("content").contains("\"ok\":true"));

    // The trace has every record the design promises.
    let text = std::fs::read_to_string(&report.trace).expect("trace file");
    for record in ["run_start", "llm_request", "llm_response", "tool_call", "tool_result", "run_end"] {
        assert!(text.contains(&format!("\"rec\":\"{record}\"")), "trace is missing {record}:\n{text}");
    }
    assert!(text.contains("\"stop\":\"answer\""));
    assert_eq!(report.trace, fixture.runs_dir.join(report.trace.file_name().expect("file name")));
    assert!(!text.contains("test-key"), "the API key must never reach the trace");
}

#[tokio::test(flavor = "current_thread")]
async fn a_reply_cut_at_max_tokens_is_named_as_truncation_not_a_tool_failure() {
    // t121: with a 256-token budget the provider can stop mid-call. Nothing ran, so calling it a failed
    // tool call would send the operator looking for a model bug; the nudge has to name the budget and the
    // variable that set it, and the trace has to record both.
    let mut fixture = fixture_with_budget(
        vec![
            // Half a tool call, cut at the budget.
            wire_body(&truncated_reply("c1", "observe", "{\"radius\":")),
            wire_body(&answer_reply("short answer, as asked")),
        ],
        Limits { max_steps: 10, ..Limits::default() },
        None,
        Some((256, "ACACIA_LLM_MAX_TOKENS")),
    )
    .await;

    let mut trace = Trace::open(&fixture.runs_dir, "short budget", json!({"test": true})).expect("trace");
    let report = fixture.agent.run("look around", &mut trace).await.expect("run");

    // The half-written call never reached the toolbox, and it is not counted as a tool call.
    assert_eq!(fixture.tools.calls(), Vec::<String>::new(), "a truncated call must never be executed");
    assert_eq!(report.tool_calls, 0, "and must not be counted as one that failed");
    assert_eq!(report.stop, Stop::Answer);

    // The model was told what happened, with the value and the variable.
    let requests = fixture.server.requests();
    let second = requests[1]["messages"].as_array().expect("messages");
    let note = second
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .find(|text| text.contains("截断"))
        .expect("the model must be told the reply was cut short");
    assert!(note.contains("被 max_tokens 截断"), "the cause is named: {note}");
    assert!(note.contains("max_tokens=256"), "with the value that did it: {note}");
    assert!(note.contains("ACACIA_LLM_MAX_TOKENS=256"), "and the knob that set it: {note}");
    assert!(!note.contains("invalid_argument"), "a cut reply is not an argument error: {note}");

    // The trace keeps the same facts for a human reading the run afterwards.
    let text = std::fs::read_to_string(&report.trace).expect("trace");
    assert!(text.contains("\"kind\":\"truncated\""), "trace: {text}");
    assert!(text.contains("\"max_tokens\":256"), "trace: {text}");
    assert!(text.contains("ACACIA_LLM_MAX_TOKENS=256"), "trace: {text}");
    assert!(text.contains("\"cut\":\"length\""), "trace: {text}");
    assert!(!text.contains("\"rec\":\"tool_call\""), "no tool was called: {text}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_tool_failure_is_data_and_the_model_can_finish() {
    let mut fixture = fixture(
        vec![
            wire_body(&tool_reply("c1", "dig", json!({"at": [1, 64, 1]}))),
            wire_body(&answer_reply("I have no pickaxe; stopping")),
        ],
        Limits { max_steps: 10, ..Limits::default() },
        None,
    )
    .await;
    let mut trace = Trace::open(&fixture.runs_dir, "dig a block", json!({})).expect("trace");
    let report = fixture.agent.run("dig a block", &mut trace).await.expect("run");
    assert_eq!(report.stop, Stop::Answer);
    assert_eq!(report.steps, 2);
    let text = std::fs::read_to_string(&report.trace).expect("trace");
    assert!(text.contains("\"reason\":\"no_item\""), "the failure reason reaches the trace");
    // A failure produces a nudge (a user message), which the next request carries.
    let requests = fixture.server.requests();
    let second = requests[1]["messages"].as_array().expect("messages");
    assert!(second.iter().any(|message| {
        message["role"] == "user" && message["content"].as_str().is_some_and(|text| text.contains("no_item"))
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn the_step_budget_ends_the_run_cleanly() {
    let mut fixture = fixture(
        vec![wire_body(&tool_reply("c1", "observe", json!({})))],
        Limits { max_steps: 1, ..Limits::default() },
        None,
    )
    .await;
    let mut trace = Trace::open(&fixture.runs_dir, "look around", json!({})).expect("trace");
    let report = fixture.agent.run("look around", &mut trace).await.expect("run");
    assert_eq!(report.stop, Stop::StepBudget);
    assert_eq!(report.steps, 1);
    let text = std::fs::read_to_string(&report.trace).expect("trace");
    assert!(text.contains("\"stop\":\"step_budget\""));
}

#[tokio::test(flavor = "current_thread")]
async fn sleep_and_pickup_failures_carry_their_reason_codes() {
    // Two runs, not two calls in one run: a second world write within 250 ms is refused as `busy`
    // by the guard, which would hide what this test is about.
    let mut sleeping = fixture(
        vec![wire_body(&tool_reply("c1", "sleep", json!({}))), wire_body(&answer_reply("no bed here"))],
        Limits { max_steps: 10, ..Limits::default() },
        None,
    )
    .await;
    let mut trace = Trace::open(&sleeping.runs_dir, "sleep", json!({})).expect("trace");
    let report = sleeping.agent.run("sleep", &mut trace).await.expect("run");
    assert_eq!(report.stop, Stop::Answer);
    assert_eq!(sleeping.tools.calls(), vec!["sleep".to_owned()]);
    let text = std::fs::read_to_string(&report.trace).expect("trace");
    assert!(text.contains("\"reason\":\"no_block\""), "sleep answers no_block:\n{text}");
    let requests = sleeping.server.requests();
    let second = requests[1]["messages"].as_array().expect("messages");
    assert!(second.iter().any(|message| {
        message["role"] == "tool" && message["content"].as_str().is_some_and(|text| text.contains("no bed within"))
    }));

    let mut picking = fixture(
        vec![
            wire_body(&tool_reply("c1", "pickup", json!({"item": "minecraft:oak_log"}))),
            wire_body(&answer_reply("no logs to pick up")),
        ],
        Limits { max_steps: 10, ..Limits::default() },
        None,
    )
    .await;
    let mut trace = Trace::open(&picking.runs_dir, "pickup", json!({})).expect("trace");
    let report = picking.agent.run("pickup", &mut trace).await.expect("run");
    assert_eq!(report.stop, Stop::Answer);
    assert_eq!(picking.tools.calls(), vec!["pickup".to_owned()]);
    let text = std::fs::read_to_string(&report.trace).expect("trace");
    assert!(text.contains("\"reason\":\"no_block\""), "pickup answers no_block:\n{text}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_denied_tool_never_reaches_the_bot() {
    let mut fixture = fixture(
        vec![
            wire_body(&tool_reply("c1", "dig", json!({"at": [0, 64, 0]}))),
            wire_body(&answer_reply("that tool is not allowed here")),
        ],
        Limits { max_steps: 10, allow: Some(vec!["status".to_owned(), "observe".to_owned()]), ..Limits::default() },
        Some(vec!["status".to_owned(), "observe".to_owned()]),
    )
    .await;
    let mut trace = Trace::open(&fixture.runs_dir, "try a denied tool", json!({})).expect("trace");
    let report = fixture.agent.run("try a denied tool", &mut trace).await.expect("run");
    assert_eq!(report.stop, Stop::Answer);
    assert!(fixture.tools.calls().is_empty(), "a denied tool must not reach the shell");
    let requests = fixture.server.requests();
    let second = requests[1]["messages"].as_array().expect("messages");
    let tool_message = second.iter().find(|message| message["role"] == "tool").expect("a tool result");
    assert!(tool_message["content"].as_str().expect("content").contains("unknown_tool"));
}

/// Prompt size is the thing that decides whether a run survives: measure both policies on the same
/// script and require the documented 40% cut.
#[tokio::test(flavor = "current_thread")]
async fn replacing_snapshots_cuts_the_prompt_by_more_than_40_percent() {
    let steps = 10;
    let mut replies = Vec::new();
    for index in 0..steps {
        replies.push(wire_body(&tool_reply(&format!("c{index}"), "observe", json!({}))));
    }
    replies.push(wire_body(&answer_reply("done")));

    let mut legacy = fixture(replies.clone(), Limits { max_steps: 40, legacy_history: true, ..Limits::default() }, None).await;
    let mut trace = Trace::open(&legacy.runs_dir, "legacy history", json!({})).expect("trace");
    legacy.agent.run("measure", &mut trace).await.expect("run");
    let before: usize = legacy.server.requests().iter().map(|request| request["messages"].to_string().len()).sum();

    let mut current = fixture(replies, Limits { max_steps: 40, ..Limits::default() }, None).await;
    let mut trace = Trace::open(&current.runs_dir, "replacing history", json!({})).expect("trace");
    current.agent.run("measure", &mut trace).await.expect("run");
    let after: usize = current.server.requests().iter().map(|request| request["messages"].to_string().len()).sum();

    assert!(before > 0 && after > 0);
    let cut = 1.0 - after as f64 / before as f64;
    eprintln!("prompt chars over {steps} steps: legacy={before} replacing={after} (-{:.0}%)", cut * 100.0);
    assert!(cut >= 0.40, "expected at least 40% less prompt: legacy={before} replacing={after}");
}

#[tokio::test(flavor = "current_thread")]
async fn only_the_newest_state_and_snapshot_survive() {
    let replies = vec![
        wire_body(&tool_reply("c1", "observe", json!({}))),
        wire_body(&tool_reply("c2", "dig", json!({"at": [1, 64, 1]}))),
        wire_body(&tool_reply("c3", "observe", json!({}))),
        wire_body(&answer_reply("done")),
    ];
    let mut fixture = fixture(replies, Limits { max_steps: 20, ..Limits::default() }, None).await;
    let mut trace = Trace::open(&fixture.runs_dir, "state replacement", json!({})).expect("trace");
    fixture.agent.run("look around", &mut trace).await.expect("run");

    let last = fixture.server.requests().last().cloned().expect("a request");
    let messages = last["messages"].as_array().expect("messages");
    let states = messages
        .iter()
        .filter(|message| {
            message["role"] == json!("user")
                && message["content"].as_str().is_some_and(|text| text.starts_with("状态快照"))
        })
        .count();
    assert_eq!(states, 1, "one snapshot, replaced each step:\n{last}");
    let superseded = messages
        .iter()
        .filter(|message| message["role"] == json!("tool") && message["content"].as_str().is_some_and(|text| text.contains("superseded")))
        .count();
    assert_eq!(superseded, 1, "the first observe body became a stub:\n{last}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_disconnect_reaches_the_model_and_ends_as_disconnected() {
    let replies = vec![
        wire_body(&tool_reply("c1", "observe", json!({}))),
        wire_body(&answer_reply("should not be reached")),
    ];
    let mut fixture = fixture(replies, Limits { max_steps: 10, max_reconnects: 1, ..Limits::default() }, None).await;
    *fixture.tools.disconnect_at.lock().expect("disconnect lock") = Some(1);
    let mut trace = Trace::open(&fixture.runs_dir, "disconnect", json!({})).expect("trace");
    let report = fixture.agent.run("do something", &mut trace).await.expect("run");

    assert_eq!(report.stop, Stop::Disconnected, "a lost connection is its own stop reason");
    assert_eq!(*fixture.tools.reconnects.lock().expect("reconnects lock"), 1, "one bounded reconnect attempt");
    let text = std::fs::read_to_string(&report.trace).expect("trace");
    assert!(text.contains("\"rec\":\"disconnect\""), "the drop is recorded:\n{text}");
    assert!(text.contains("\"rec\":\"reconnect\""), "the attempt is recorded:\n{text}");
    assert!(text.contains("ServerClosed"), "the reason is carried:\n{text}");
}
