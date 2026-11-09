//! The two failure shapes of t69, reproduced without a server (t76).
//!
//! Live t69 was: eleven `pickup` calls answered `no_block` with no message and no hint, and five `trades`
//! calls answered `position_desync` while the villager stood 2.7 blocks away. Neither can be re-run on
//! demand (the bed and the villager were one-off world state), so both shapes are pinned here: a fake
//! toolbox for the failure envelope, and the pure `may_interact` predicate for the drift gate.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use acacia_agent::agent::{system_prompt, Agent, Limits, Stop, Trace};
use acacia_agent::config::{ChatConfig, LlmConfig, SafetyConfig, ToolsConfig};
use acacia_agent::llm::mock::{answer_reply, tool_reply, wire_body, MockServer};
use acacia_agent::llm::openai::OpenAiClient;
use acacia_agent::tools::result::{classify_not_possible, hint_for};
use acacia_agent::tools::{Call, Failure, Registry, ToolBox};
use acacia_bot::desync::{DESYNC_LIMIT, may_interact};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------------------------------
// Scenario A: what a failed `pickup` says now
// ---------------------------------------------------------------------------------------------------

/// A toolbox whose `pickup` answers the t69 shape: the old code produced a reason from the *block*
/// branch and an empty message, so the model could not tell an unreachable drop from a gone one.
#[derive(Default, Clone)]
struct PickupTools {
    /// When set, `pickup` fails this way; otherwise it succeeds with a full result body.
    fail_with: Arc<Mutex<Option<Failure>>>,
    calls: Arc<Mutex<Vec<String>>>,
    ids: Arc<Mutex<Vec<Option<u64>>>>,
}

impl PickupTools {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn ids(&self) -> Vec<Option<u64>> {
        self.ids.lock().expect("ids").clone()
    }
}

impl ToolBox for PickupTools {
    async fn call(&mut self, call: Call) -> Result<Value, Failure> {
        self.calls.lock().expect("calls").push(call.name().to_owned());
        match call {
            Call::Pickup { id, .. } => {
                self.ids.lock().expect("ids").push(id);
                if let Some(failure) = self.fail_with.lock().expect("fail").clone() {
                    return Err(failure);
                }
                // The shape `shell::pickup` returns on the `{id}` path (t73).
                Ok(json!({
                    "collected": [{"item": "minecraft:emerald", "count": 2}],
                    "position": [150033, 69, 149855],
                    "id": id,
                }))
            }
            Call::Stop => Ok(json!({"stopped": true})),
            _ => Ok(json!({})),
        }
    }

    async fn state(&mut self) -> Value {
        json!({"tick": 1, "connected": true, "delta": {}})
    }
}

struct Harness {
    agent: Agent<OpenAiClient, PickupTools>,
    tools: PickupTools,
    server: MockServer,
    runs_dir: std::path::PathBuf,
}

async fn harness(replies: Vec<Value>, fail_with: Option<Failure>) -> Harness {
    let server = MockServer::start(replies).await.expect("mock server");
    let mut llm = LlmConfig::default();
    llm.base_url = server.base_url().to_owned();
    llm.model = "mock-model".to_owned();
    llm.timeout_s = 10;
    let client = OpenAiClient::new(&llm, "test-key".to_owned()).expect("client");
    let tools = PickupTools::default();
    *tools.fail_with.lock().expect("fail") = fail_with;
    let port = server.base_url().rsplit(':').next().unwrap_or("0").trim_end_matches("/v1");
    let runs_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/test-runs")
        .join(format!("shapes-{}-{port}", std::process::id()));
    let agent = Agent {
        llm: client,
        tools: tools.clone(),
        registry: Registry::new(&ToolsConfig::default()),
        limits: Limits { max_steps: 8, ..Limits::default() },
        system: system_prompt(&ChatConfig::default()),
        chat: ChatConfig::default(),
        safety: SafetyConfig::default(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    Harness { agent, tools, server, runs_dir }
}

/// The `tool` message the model reads for call `id`.
fn tool_message(requests: &[Value], id: &str) -> Value {
    requests
        .iter()
        .flat_map(|request| request["messages"].as_array().cloned().unwrap_or_default())
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
        .unwrap_or_else(|| panic!("no tool message for {id} in {} request(s)", requests.len()))
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_pickup_says_why_and_points_at_the_runtime_id() {
    // The old body: a reason borrowed from the block branch and nothing else.
    let old_shape = Failure { reason: "no_such_entity", message: String::new(), hint: None, details: None };
    let mut harness = harness(
        vec![
            wire_body(&tool_reply("p1", "pickup", json!({"radius": 6}))),
            wire_body(&answer_reply("no drop to pick up; I will list entities")),
        ],
        Some(old_shape),
    )
    .await;
    let mut trace = Trace::open(&harness.runs_dir, "pick up the emerald", json!({"test": true})).expect("trace");
    let report = harness.agent.run("pick up the emerald", &mut trace).await.expect("run");
    assert_eq!(report.stop, Stop::Answer);

    let requests = harness.server.requests();
    let body: Value = serde_json::from_str(tool_message(&requests, "p1")["content"].as_str().expect("content"))
        .expect("the tool result is JSON");
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["reason"], json!("no_such_entity"), "the reason survives: {body}");
    let message = body["message"].as_str().expect("a message");
    assert!(!message.trim().is_empty(), "t69 shape: the failure arrived with an empty message: {body}");
    assert!(message.contains("no_such_entity"), "the stand-in message names the reason: {body}");
    let hint = body["hint"].as_str().expect("t69 shape: the failure arrived with no hint at all");
    assert!(
        hint.contains("entities") || hint.contains("id"),
        "the hint must say the way out is a runtime id: {hint}"
    );
    // The run itself still ends normally: a failure is data, not an exit.
    assert_eq!(harness.tools.calls(), vec!["pickup".to_owned()]);
}

#[tokio::test(flavor = "current_thread")]
async fn a_pickup_by_runtime_id_reports_what_it_took() {
    let mut harness = harness(
        vec![
            wire_body(&tool_reply("p1", "pickup", json!({"id": 2347}))),
            wire_body(&answer_reply("picked up 2 emeralds")),
        ],
        None,
    )
    .await;
    let mut trace = Trace::open(&harness.runs_dir, "pick up 2347", json!({"test": true})).expect("trace");
    harness.agent.run("pick up 2347", &mut trace).await.expect("run");

    assert_eq!(harness.tools.ids(), vec![Some(2347)], "the id reaches the toolbox");
    let requests = harness.server.requests();
    let body: Value = serde_json::from_str(tool_message(&requests, "p1")["content"].as_str().expect("content"))
        .expect("the tool result is JSON");
    assert_eq!(body["ok"], json!(true), "{body}");
    let data = &body["data"];
    for key in ["collected", "position", "id"] {
        assert!(!data[key].is_null(), "a successful pickup by id must report `{key}`: {body}");
    }
    assert_eq!(data["id"], json!(2347), "the id it took, so a retry cannot pick the wrong drop: {body}");
    assert_eq!(data["collected"][0]["item"], json!("minecraft:emerald"));
}

// ---------------------------------------------------------------------------------------------------
// Scenario B: the drift gate that made five `trades` calls fail on a villager 2.7 blocks away
// ---------------------------------------------------------------------------------------------------

/// t69's live state: the server never confirmed a position, so every number the client had was the spawn
/// one and the "drift" was measured against it. A gate that refuses on that comparison refuses forever
/// (t70/t71), which is exactly what happened while the villager stood 2.7 blocks away.
#[test]
fn an_unconfirmed_reference_never_blocks_the_interaction() {
    assert!(may_interact(0, None, Some(2.7)), "no confirmation at all: the 2.7 is not evidence");
    assert!(may_interact(0, None, Some(11.39)), "worse nominal drift, still not evidence");
    assert!(may_interact(0, Some(500), Some(20.58)), "a very old reference proves nothing either way");
    // And the number the tool layer would have reported is above the limit, i.e. the old raw comparison
    // is what refused: this pins *why* the predicate exists.
    assert!(2.7 > DESYNC_LIMIT, "DESYNC_LIMIT is {DESYNC_LIMIT}; a raw comparison refuses the t69 villager");
}

/// The other half: a reference that *is* confirmed and fresh, with a real 11-block drift, still refuses.
/// Without this, "never block" would pass every test above and the guard would be gone.
#[test]
fn a_fresh_confirmed_reference_still_refuses_a_real_drift() {
    assert!(!may_interact(3, Some(2), Some(11.39)), "3 confirmations, 2 ticks ago, 11.39 blocks: refuse");
    assert!(may_interact(3, Some(2), Some(1.07)), "steady-state jitter on this server is ~1.07: allow");
    assert!(may_interact(3, Some(400), Some(11.39)), "400 ticks old: not evidence, allow");
    assert!(may_interact(3, Some(2), None), "no measured drift: allow");
}

/// Whichever side of the gate a run lands on, the reason it reports must be the drift one and the hint
/// must point at `status.position_desync` — not at anticheat, and not at the trader.
#[test]
fn the_drift_refusal_carries_the_right_reason_and_hint() {
    let message = "position_desync: the client's body is 2.70 blocks from the position the server reports \
                   (predicted [150033.0, 69.0, 149855.0], server [150032.0, 69.0, 149855.0], 0 ticks); \
                   move or wait for the position to sync before clicking";
    assert_eq!(classify_not_possible(message), "position_desync");
    let hint = hint_for("position_desync").expect("a hint");
    assert!(
        hint.contains("position_desync") || hint.contains("status"),
        "the hint must send the model to the drift field, not to the trader: {hint}"
    );
    assert!(!hint.contains("anticheat"), "a drift refusal is not a server refusal: {hint}");
}
