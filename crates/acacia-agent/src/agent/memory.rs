//! Short-term task memory: the goal, what is done, what was learned, and what failed.
//!
//! It is rebuilt from tool results every step and rendered into the state message, so a compaction
//! can throw the conversation away without losing the plan (`docs/agent-design.md`, "Memory").

use serde_json::Value;

use crate::tools::Call;

/// Caps that keep the rendered memory under ~250 tokens.
const MAX_DONE: usize = 8;
const MAX_FACTS: usize = 12;
const MAX_FAILURES: usize = 6;
const MAX_NOTES: usize = 6;

#[derive(Debug, Clone, Default)]
pub struct TaskMemory {
    pub goal: String,
    pub done: Vec<String>,
    pub facts: Vec<String>,
    pub failures: Vec<String>,
    pub notes: Vec<String>,
}

impl TaskMemory {
    pub fn new(goal: &str) -> Self {
        Self { goal: goal.to_owned(), ..Self::default() }
    }

    /// Records one tool outcome: a fact when it worked, a failure when it did not.
    pub fn note(&mut self, call: &Call, result: &Value, ok: bool) {
        if !ok {
            let reason = result.get("reason").and_then(Value::as_str).unwrap_or("failed");
            let message = result.get("message").and_then(Value::as_str).unwrap_or("");
            push_capped(&mut self.failures, format!("{}: {reason} — {message}", call.name()), MAX_FAILURES);
            return;
        }
        let data = result.get("data").unwrap_or(&Value::Null);
        match call.name() {
            "find" => {
                if let Some(first) = data.get("matches").and_then(Value::as_array).and_then(|matches| matches.first()) {
                    let block = first.get("block").and_then(Value::as_str).unwrap_or("?");
                    let pos = first.get("pos").cloned().unwrap_or(Value::Null);
                    let dist = first.get("dist").cloned().unwrap_or(Value::Null);
                    push_capped(&mut self.facts, format!("{block} at {pos} ({dist} blocks)"), MAX_FACTS);
                } else {
                    push_capped(&mut self.facts, format!("no {:?} nearby", call_block(call)), MAX_FACTS);
                }
            }
            "move_to" => {
                let pos = data.get("block_position").cloned().unwrap_or(Value::Null);
                push_capped(&mut self.done, format!("moved to {pos}"), MAX_DONE);
            }
            "dig" => {
                let block = data.get("block").and_then(Value::as_str).unwrap_or("block");
                let pos = data.get("pos").cloned().unwrap_or(Value::Null);
                push_capped(&mut self.done, format!("broke {block} at {pos}"), MAX_DONE);
            }
            "place" => {
                let target = data.get("target").cloned().unwrap_or(Value::Null);
                push_capped(&mut self.done, format!("placed a block at {target}"), MAX_DONE);
            }
            "craft" => {
                let made = data.get("crafted").cloned().unwrap_or(Value::Null);
                let item = data.get("item").and_then(Value::as_str).unwrap_or("item");
                push_capped(&mut self.done, format!("crafted {made} x {item}"), MAX_DONE);
            }
            "smelt" => {
                let made = data.get("smelted").cloned().unwrap_or(Value::Null);
                let input = data.get("input").and_then(Value::as_str).unwrap_or("item");
                push_capped(&mut self.done, format!("smelted {made} x {input}"), MAX_DONE);
            }
            "container" => {
                push_capped(&mut self.done, format!("container {}", data.get("op").and_then(Value::as_str).unwrap_or("op")), MAX_DONE);
            }
            "eat" => {
                push_capped(&mut self.done, "ate".to_owned(), MAX_DONE);
            }
            "equip" | "hold" => {
                let item = data.get("equipped").or_else(|| data.get("held")).and_then(Value::as_str).unwrap_or("item");
                push_capped(&mut self.done, format!("{item} in hand"), MAX_DONE);
            }
            "attack" => {
                let kind = data.get("kind").and_then(Value::as_str).unwrap_or("entity");
                push_capped(&mut self.done, format!("attacked {kind}"), MAX_DONE);
            }
            _ => {}
        }
    }

    /// A note the loop adds itself (a nudge, a compaction summary pointer).
    pub fn push_note(&mut self, note: String) {
        push_capped(&mut self.notes, note, MAX_NOTES);
    }

    /// The compact block rendered into every state message.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("goal: {}\n", self.goal));
        if !self.done.is_empty() {
            out.push_str(&format!("done: {}\n", self.done.join("; ")));
        }
        if !self.facts.is_empty() {
            out.push_str(&format!("facts: {}\n", self.facts.join("; ")));
        }
        if !self.failures.is_empty() {
            out.push_str(&format!("failures: {}\n", self.failures.join("; ")));
        }
        if !self.notes.is_empty() {
            out.push_str(&format!("notes: {}\n", self.notes.join("; ")));
        }
        out
    }
}

fn call_block(call: &Call) -> String {
    match call {
        Call::Find { block, .. } => block.clone(),
        _ => "block".to_owned(),
    }
}

fn push_capped(list: &mut Vec<String>, item: String, cap: usize) {
    if list.last().is_some_and(|last| *last == item) {
        return;
    }
    list.push(item);
    if list.len() > cap {
        list.remove(0);
    }
}
