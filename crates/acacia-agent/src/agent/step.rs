//! The step machine: LLM tool-calling → tool execution → observation, until the model answers or a
//! budget runs out.
//!
//! The rules implemented here are the ones `docs/agent-design.md` ("The loop") states:
//! - one step = one model call; several tool calls in one step run sequentially (there is one bot);
//! - a step budget, a wall clock, a token budget and cancellation all end the run cleanly;
//! - nudges are **user** messages, never assistant ones, so the history a provider sees stays valid;
//! - a tool failure is data for the model, not the end of the run — except `disconnected`, which the
//!   model cannot fix.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::compact::{estimate_messages, safe_cut};
use super::guard::Guard;
use super::memory::TaskMemory;
use super::trace::Trace;
use crate::config::{ChatConfig, SafetyConfig};
use crate::error::AgentError;
use crate::llm::{ChatReply, ChatRequest, Llm, LlmError, Message, Role, Usage};
use crate::llm::prompt::{AGENT_PROMPT, CHAT_RULES};
use crate::tools::{Registry, ToolBox};

/// Shared with the RPC layer and `Ctrl-C`: set to `true` to stop after the current tool.
pub type CancelFlag = Arc<AtomicBool>;

/// Per-run budgets.
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_steps: u32,
    pub wall_clock: Duration,
    pub max_total_tokens: u64,
    /// Compact when the estimate reaches this many tokens (`0.8 × context_window` by default).
    pub compact_at: u64,
    pub keep_recent: usize,
    /// A further narrowing of the tool whitelist for this run.
    pub allow: Option<Vec<String>>,
    /// Automatic reconnect attempts after the connection drops (0 disables them).
    pub max_reconnects: u32,
    /// Measurement/debug switch: append every snapshot instead of replacing the previous one and keep
    /// older snapshot tool results. Off in real runs; `tests/loop_mock.rs` flips it to price the
    /// difference.
    pub legacy_history: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 80,
            wall_clock: Duration::from_secs(900),
            max_total_tokens: 1_500_000,
            compact_at: 102_400,
            keep_recent: 6,
            allow: None,
            max_reconnects: 1,
            legacy_history: false,
        }
    }
}

/// Why the run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The model answered.
    Answer,
    StepBudget,
    WallClock,
    TokenBudget,
    Cancelled,
    /// The connection dropped and the reconnect attempt failed.
    Disconnected,
}

impl Stop {
    pub fn as_str(self) -> &'static str {
        match self {
            Stop::Answer => "answer",
            Stop::StepBudget => "step_budget",
            Stop::WallClock => "wall_clock",
            Stop::TokenBudget => "token_budget",
            Stop::Cancelled => "cancelled",
            Stop::Disconnected => "disconnected",
        }
    }
}

/// What a finished run produced.
#[derive(Debug, Clone)]
pub struct Report {
    pub answer: String,
    pub steps: u32,
    pub tool_calls: u32,
    pub compactions: u32,
    pub usage: Usage,
    pub stop: Stop,
    pub trace: PathBuf,
    /// Wall clock from the first request to the last reply (t76: t69's cost was counted by hand off the trace).
    pub elapsed: Duration,
}

impl Report {
    /// The one machine-readable cost line every run prints: `steps / tool_calls / tokens / compactions /
    /// elapsed / max_steps`. It exists because t69's "55 calls, 480k tokens, how long?" had to be counted
    /// by hand from the trace; a verifier should never do that arithmetic again.
    pub fn summary_line(&self, max_steps: u32) -> String {
        format!(
            "steps={}/{max_steps} tool_calls={} tokens={} (prompt {}, completion {}) compactions={} elapsed={:.1}s",
            self.steps,
            self.tool_calls,
            self.usage.total(),
            self.usage.prompt_tokens,
            self.usage.completion_tokens,
            self.compactions,
            self.elapsed.as_secs_f64(),
        )
    }
}

/// How a reply was cut short (t121).
///
/// `length` is the provider saying "I stopped at `max_tokens`". Any other non-final reason that arrives
/// **with** a tool call is the same situation with a different name (`content_filter`, a provider-specific
/// reason): the call is not something the model finished, so it must not run. A non-final reason on a
/// content-only reply is left to the answer path — there is no call to protect there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutShort {
    Length,
    Other(String),
}

impl CutShort {
    pub fn as_str(&self) -> &str {
        match self {
            CutShort::Length => "length",
            CutShort::Other(reason) => reason,
        }
    }
}

/// `Some` when this reply must not be executed or read as an answer (t121).
pub fn cut_short(reply: &ChatReply) -> Option<CutShort> {
    match reply.finish_reason.as_deref() {
        Some("length") => Some(CutShort::Length),
        Some("stop") | Some("tool_calls") | None => None,
        Some(other) if !reply.tool_calls.is_empty() => Some(CutShort::Other(other.to_owned())),
        Some(_) => None,
    }
}

/// The user message the model gets after a cut reply (t121): what happened, the budget that did it, and
/// the knob to turn. When the client cannot report its budget it says so instead of guessing a number.
pub fn truncation_note(cut: &CutShort, budget: Option<&(u32, String)>) -> String {
    let cause = match cut {
        CutShort::Length => "被 max_tokens 截断".to_owned(),
        CutShort::Other(reason) => format!("没有正常结束（finish_reason={reason}），内容可能被截断"),
    };
    let budget_text = match budget {
        Some((tokens, origin)) => format!("本次 max_tokens={tokens}，来自 {origin}"),
        None => "本次 max_tokens 未知（这个客户端没有报告预算）".to_owned(),
    };
    format!(
        "你的上一条回复{cause}（{budget_text}）。那次回复没有执行，也不算一次工具调用失败：把内容缩短，一次只调一个工具；如果确实不够用，把 ACACIA_LLM_MAX_TOKENS 调大再试。"
    )
}

/// One agent, one bot, one run at a time.
pub struct Agent<L: Llm, T: ToolBox> {
    pub llm: L,
    pub tools: T,
    pub registry: Registry,
    pub limits: Limits,
    pub system: String,
    pub chat: ChatConfig,
    pub safety: SafetyConfig,
    pub cancel: CancelFlag,
}

/// The default system prompt: identity, tool rules, and the chat rules when chat is enabled.
pub fn system_prompt(chat: &ChatConfig) -> String {
    let mut system = AGENT_PROMPT.to_owned();
    if chat.enabled {
        system.push('\n');
        system.push_str(CHAT_RULES);
    }
    system
}

impl<L: Llm, T: ToolBox> Agent<L, T> {
    pub async fn run(&mut self, task: &str, trace: &mut Trace) -> Result<Report, AgentError> {
        let mut messages = vec![Message::system(self.system.clone()), Message::user(task.to_owned())];
        let mut memory = TaskMemory::new(task);
        let mut guard = Guard::new(self.chat.clone(), self.safety.clone());
        let mut usage = Usage::default();
        let mut steps = 0u32;
        let mut tool_calls = 0u32;
        let mut compactions = 0u32;
        let mut signatures: VecDeque<String> = VecDeque::new();
        let started = Instant::now();
        // Bounded reconnect: the model is told about the drop and either continues after a successful
        // reconnect or gets a clean `Stop::Disconnected` (exit 6) instead of the process dying.
        let mut reconnects = 0u32;
        let mut announced: Option<String> = None;
        // Tool-call ids whose result was a snapshot body: the next one supersedes them.
        let mut snapshot_ids: Vec<String> = Vec::new();
        let mut budget_warned = false;

        loop {
            // --- budgets
            if self.cancel.load(Ordering::Acquire) {
                return Ok(self.finish("the run was cancelled", &mut messages, trace, steps, tool_calls, compactions, usage, Stop::Cancelled, started));
            }
            if steps >= self.limits.max_steps {
                return Ok(self.finish(
                    &format!("step budget exhausted after {} steps; stop and summarise what you have", steps),
                    &mut messages,
                    trace,
                    steps,
                    tool_calls,
                    compactions,
                    usage,
                    Stop::StepBudget,
                    started,
                ));
            }
            if started.elapsed() >= self.limits.wall_clock {
                return Ok(self.finish(
                    &format!("wall clock budget exhausted after {}s", started.elapsed().as_secs()),
                    &mut messages,
                    trace,
                    steps,
                    tool_calls,
                    compactions,
                    usage,
                    Stop::WallClock,
                    started,
                ));
            }
            if usage.total() >= self.limits.max_total_tokens {
                return Ok(self.finish(
                    &format!("token budget exhausted after {} tokens", usage.total()),
                    &mut messages,
                    trace,
                    steps,
                    tool_calls,
                    compactions,
                    usage,
                    Stop::TokenBudget,
                    started,
                ));
            }
            steps += 1;

            // --- near the end of the budget, say so while the model can still act on it (t69: a run
            // that walked, looked at offers and only then needed a bed ran out at 40 steps in silence)
            let remaining_steps = self.limits.max_steps.saturating_sub(steps);
            if remaining_steps == 10 && !budget_warned {
                budget_warned = true;
                let left = self.limits.wall_clock.saturating_sub(started.elapsed());
                let nudge = format!(
                    "系统提醒：步数预算只剩 {remaining_steps} 步（时间还剩 {} 秒）。优先完成当前目标，别再开新的支线；必要时先 stop 再总结。",
                    left.as_secs()
                );
                trace.record("nudge", json!({"step": steps, "kind": "budget", "text": nudge}));
                messages.push(Message::user(nudge));
            }

            // --- compaction before the call that would overflow
            if estimate_messages(&messages) >= self.limits.compact_at && messages.len() > 8 {
                let estimate_before = estimate_messages(&messages);
                match super::compact::compact(&mut messages, &self.llm, self.limits.keep_recent).await {
                    Ok((from, to, chars)) if to < from => {
                        compactions += 1;
                        memory.push_note("earlier steps were compacted into a summary".to_owned());
                        trace.record(
                            "compact",
                            json!({
                                "before": from,
                                "after": to,
                                "summary_chars": chars,
                                "est_tokens_before": estimate_before,
                                "est_tokens_after": estimate_messages(&messages),
                            }),
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        // A failed summariser just leaves the history as it is; retry next step.
                        eprintln!("[acacia-agent] compaction failed: {error}");
                        trace.record("compact_error", json!({"message": error.to_string()}));
                    }
                }
            }

            // --- state message: the observation the model acts on
            if steps > 1 {
                let state = self.tools.state().await;
                // A drop between steps reaches the model as an event instead of silently ending the
                // run; the reason is what the CLI would otherwise only print on exit.
                if state.get("connected") == Some(&Value::Bool(false)) {
                    if let Some(reason) = state.get("last_error").and_then(Value::as_str) {
                        if announced.as_deref() != Some(reason) {
                            announced = Some(reason.to_owned());
                            trace.record("disconnect", json!({"step": steps, "source": "state", "reason": reason}));
                            messages.push(Message::user(format!(
                                "系统事件：连接已中断（{reason}）。可以结束并总结，或继续（系统会尝试重连一次）。"
                            )));
                        }
                    }
                }
                let rendered = serde_json::to_string(&state).unwrap_or_else(|_| state.to_string());
                push_state(&mut messages, steps, &rendered, &memory.render(), self.limits.legacy_history);
            }

            // --- the model
            let request_messages = messages.clone();
            let estimate = estimate_messages(&request_messages);
            let tools = self.registry.tools();
            trace.record(
                "llm_request",
                json!({"step": steps, "messages": request_messages.len(), "est_tokens": estimate, "tools": tools.len()}),
            );
            let sent = Instant::now();
            let reply = match self.llm.chat(ChatRequest { messages: request_messages, tools }).await {
                Ok(reply) => reply,
                Err(error) => {
                    trace.record("llm_error", json!({"step": steps, "message": error.to_string()}));
                    self.finish(&format!("llm error: {error}"), &mut messages, trace, steps, tool_calls, compactions, usage, Stop::Cancelled, started);
                    return Err(AgentError::Llm(error.to_string()));
                }
            };
            usage.prompt_tokens += reply.usage.prompt_tokens;
            usage.completion_tokens += reply.usage.completion_tokens;
            trace.record(
                "llm_response",
                json!({
                    "step": steps,
                    "latency_ms": sent.elapsed().as_millis() as u64,
                    "content": reply.content,
                    "tool_calls": reply.tool_calls.iter().map(|call| json!({"id": call.id, "name": call.name, "arguments": call.arguments})).collect::<Vec<_>>(),
                    "usage": {"prompt_tokens": reply.usage.prompt_tokens, "completion_tokens": reply.usage.completion_tokens},
                    "finish_reason": reply.finish_reason,
                }),
            );

            if let Some(cut) = cut_short(&reply) {
                // t121: a reply the provider cut at `max_tokens` is not a failed tool call — nothing ran,
                // and saying "the tool call was wrong" sends the operator hunting for a model bug while the
                // real cause is a 256-token budget. The nudge names the budget and the knob that set it.
                // A half-written tool call must never be executed on a best-effort parse of its arguments.
                let budget = self.llm.max_tokens_budget();
                let note = truncation_note(&cut, budget.as_ref());
                messages.push(Message::user(note.clone()));
                trace.record(
                    "nudge",
                    json!({
                        "step": steps,
                        "kind": "truncated",
                        "finish_reason": reply.finish_reason,
                        "cut": cut.as_str(),
                        "tool_calls": reply.tool_calls.len(),
                        "max_tokens": budget.as_ref().map(|(tokens, _)| *tokens),
                        "max_tokens_origin": budget.as_ref().map(|(_, origin)| origin.clone()),
                        "text": note,
                    }),
                );
                continue;
            }

            if reply.tool_calls.is_empty() {
                let answer = reply.content.clone().unwrap_or_default();
                messages.push(Message::assistant(reply.content.clone(), Vec::new()));
                return Ok(self.finish(&answer, &mut messages, trace, steps, tool_calls, compactions, usage, Stop::Answer, started));
            }

            messages.push(Message::assistant(reply.content.clone(), reply.tool_calls.clone()));
            let mut nudges: Vec<String> = Vec::new();
            let mut reconnect_now = false;
            for call in &reply.tool_calls {
                tool_calls += 1;
                let parsed = match call.arguments_json() {
                    Ok(value) => Ok(value),
                    Err(error) => Err(crate::tools::Failure::invalid_argument(format!("arguments are not valid JSON: {error}"))),
                };
                let resolved = match &parsed {
                    Ok(value) => self.registry.resolve(&call.name, Some(value)),
                    Err(failure) => Err(failure.clone()),
                };
                trace.record("tool_call", json!({"step": steps, "name": call.name, "args": parsed.as_ref().ok()}));

                let (result, ok, noted) = match resolved {
                    Err(failure) => (failure.to_json(), false, None),
                    Ok(parsed_call) => match guard.check_call(&parsed_call) {
                        Err(failure) => (failure.to_json(), false, None),
                        Ok(()) => {
                            let mut outcome = self.tools.call(parsed_call.clone()).await;
                            // A read that timed out is idempotent, so it is retried once. A write
                            // never is: a `dig` that timed out may already have broken the block.
                            if outcome.as_ref().err().is_some_and(|failure| failure.reason == "timeout")
                                && crate::tools::spec(parsed_call.name())
                                    .is_some_and(|spec| spec.table == crate::tools::Table::Perceive)
                            {
                                outcome = self.tools.call(parsed_call.clone()).await;
                            }
                            match outcome {
                                Ok(data) => (crate::tools::result::ok(data), true, Some(parsed_call)),
                                Err(failure) => (failure.to_json(), false, Some(parsed_call)),
                            }
                        }
                    },
                };
                let reason = result.get("reason").and_then(Value::as_str).map(str::to_owned);
                trace.record(
                    "tool_result",
                    json!({"step": steps, "name": call.name, "ok": ok, "reason": reason, "data": result.get("data")}),
                );
                let text = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_owned());
                if !self.limits.legacy_history && SNAPSHOT_TOOLS.contains(&call.name.as_str()) {
                    // The world body the model keeps is the newest one; older copies become a stub.
                    supersede_snapshots(&mut messages, &snapshot_ids);
                    snapshot_ids.push(call.id.clone());
                }
                messages.push(Message::tool(call.id.clone(), text));
                if let Some(noted) = &noted {
                    memory.note(noted, &result, ok);
                }

                if !ok {
                    if reason.as_deref() == Some("disconnected") {
                        let lost = result.get("message").and_then(Value::as_str).unwrap_or("the connection dropped").to_owned();
                        trace.record("disconnect", json!({"step": steps, "source": "tool", "reason": lost}));
                        messages.push(Message::user(format!("系统事件：连接中断（{lost}）。")));
                        let back = reconnects < self.limits.max_reconnects && self.tools.reconnect().await;
                        trace.record("reconnect", json!({"step": steps, "attempt": reconnects + 1, "ok": back}));
                        if back {
                            reconnects += 1;
                            messages.push(Message::user("重连成功：继续任务，先 status 确认位置与背包。".to_owned()));
                            reconnect_now = true;
                            break;
                        }
                        return Ok(self.finish(
                            &format!("connection lost: {lost}"),
                            &mut messages,
                            trace,
                            steps,
                            tool_calls,
                            compactions,
                            usage,
                            Stop::Disconnected,
                            started,
                        ));
                    }
                    let message = result.get("message").and_then(Value::as_str).unwrap_or("");
                    nudges.push(format!("that failed: {} — {message}", reason.clone().unwrap_or_default()));
                }

                signatures.push_back(format!("{}:{}", call.name, call.arguments));
                if signatures.len() > 6 {
                    signatures.pop_front();
                }
            }

            if reconnect_now {
                // The drop was handled: the next step starts from a fresh connection, not from the
                // stale nudges of a world that is gone.
                continue;
            }

            for nudge in nudges {
                trace.record("nudge", json!({"step": steps, "kind": "tool_failure", "text": nudge}));
                messages.push(Message::user(format!("{nudge} Pick another approach.")));
            }

            // --- no progress: the same call three times in a row
            let n = signatures.len();
            if n >= 3 {
                let last = signatures[n - 1].clone();
                if signatures[n - 2] == last && signatures[n - 3] == last {
                    let nudge = "no progress for 3 steps: the same call keeps returning. Run status, then change strategy or stop and explain.".to_owned();
                    trace.record("nudge", json!({"step": steps, "kind": "no_progress", "text": nudge}));
                    messages.push(Message::user(nudge));
                    signatures.clear();
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        answer: &str,
        messages: &mut Vec<Message>,
        trace: &mut Trace,
        steps: u32,
        tool_calls: u32,
        compactions: u32,
        usage: Usage,
        stop: Stop,
        started: Instant,
    ) -> Report {
        let _ = safe_cut(messages, 1);
        trace.record(
            "run_end",
            json!({
                "stop": stop.as_str(),
                "steps": steps,
                "tool_calls": tool_calls,
                "compactions": compactions,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "tokens": {"prompt": usage.prompt_tokens, "completion": usage.completion_tokens},
                "answer": answer,
            }),
        );
        Report {
            answer: answer.to_owned(),
            steps,
            tool_calls,
            compactions,
            usage,
            stop,
            trace: trace.path().to_path_buf(),
            elapsed: started.elapsed(),
        }
    }
}

impl From<LlmError> for AgentError {
    fn from(error: LlmError) -> Self {
        AgentError::Llm(error.to_string())
    }
}

/// The marker every per-step snapshot starts with, so the previous one can be found and replaced.
const STATE_PREFIX: &str = "状态快照（第 ";

/// What an older snapshot tool result is replaced with once a newer one has been sent.
const SUPERSEDED: &str = r#"{"ok":true,"data":{"superseded":"a newer snapshot follows"}}"#;

/// Tools whose result *is* the world state: a second one supersedes the first.
const SNAPSHOT_TOOLS: [&str; 3] = ["observe", "status", "inventory"];

/// Pushes the step snapshot, replacing the previous one in place of stacking a new copy every step.
///
/// A snapshot is the current truth, not a log entry: keeping all of them made the prompt grow by
/// one full body per step (t20: 587K prompt tokens over 36 steps).
fn push_state(messages: &mut Vec<Message>, step: u32, rendered: &str, memory: &str, legacy: bool) {
    let text = format!("状态快照（第 {step} 步）：\n{rendered}\n\n任务记忆：\n{memory}");
    if !legacy {
        if let Some(index) = messages.iter().rposition(|message| {
            message.role == Role::User && message.content.as_deref().is_some_and(|text| text.starts_with(STATE_PREFIX))
        }) {
            messages.remove(index);
        }
    }
    messages.push(Message::user(text));
}

/// Blanks the earlier results of snapshot tools (by tool-call id) so only the newest body stays.
fn supersede_snapshots(messages: &mut [Message], ids: &[String]) {
    for message in messages.iter_mut() {
        if message.tool_call_id.as_ref().is_some_and(|id| ids.contains(id)) {
            message.content = Some(SUPERSEDED.to_owned());
        }
    }
}

#[cfg(test)]
mod truncation_tests {
    //! t121: a reply cut at `max_tokens` must be named as such, with the budget and the knob that set it.
    //!
    //! The failure this prevents is one of attribution, not of function: nothing crashed when t69's model
    //! "kept calling the wrong tool" — the run had a small reply budget and every cut reply looked like a
    //! model mistake. The note is the only place the operator can still tell those apart.
    use super::*;
    use crate::llm::ToolCall;

    fn reply(finish: Option<&str>, calls: usize) -> ChatReply {
        ChatReply {
            content: None,
            tool_calls: (0..calls).map(|index| ToolCall::new(format!("c{index}"), "observe", "{\"radius\":")).collect(),
            usage: Usage::default(),
            finish_reason: finish.map(str::to_owned),
        }
    }

    #[test]
    fn length_is_read_as_the_budget_and_never_as_the_model() {
        let cut = cut_short(&reply(Some("length"), 1)).expect("length is a cut");
        assert_eq!(cut, CutShort::Length);
        let budget = (256u32, "ACACIA_LLM_MAX_TOKENS=256".to_owned());
        let note = truncation_note(&cut, Some(&budget));
        assert!(note.contains("被 max_tokens 截断"), "it must say what happened: {note}");
        assert!(note.contains("max_tokens=256"), "and the value that did it: {note}");
        assert!(note.contains("ACACIA_LLM_MAX_TOKENS=256"), "and the knob that set it: {note}");
        assert!(note.contains("不算一次工具调用失败"), "and that the tool did not fail: {note}");
    }

    #[test]
    fn a_finished_reply_is_not_a_cut() {
        // `tool_calls` is the *final* reason for a complete call, `stop` for an answer: neither is truncation.
        assert_eq!(cut_short(&reply(Some("tool_calls"), 2)), None);
        assert_eq!(cut_short(&reply(Some("stop"), 0)), None);
        assert_eq!(cut_short(&reply(None, 1)), None, "a provider that sends no reason is not accused of cutting");
    }

    #[test]
    fn another_reason_with_a_call_is_a_cut_too() {
        // Same situation, different name: the call is not finished, so it must not run.
        assert_eq!(cut_short(&reply(Some("content_filter"), 1)), Some(CutShort::Other("content_filter".to_owned())));
        // Without a call there is nothing to protect: the answer path may still have usable content.
        assert_eq!(cut_short(&reply(Some("content_filter"), 0)), None);
    }

    #[test]
    fn a_cut_without_a_known_budget_says_so_instead_of_guessing() {
        let note = truncation_note(&CutShort::Length, None);
        assert!(note.contains("未知"), "an unknown budget is stated, not filled in: {note}");
        assert!(!note.contains("4096"), "inventing the default would blame a budget that may not be in use: {note}");
    }

    #[tokio::test]
    async fn a_cut_reply_never_reaches_the_tools_and_the_model_is_told_the_budget() {
        // The end-to-end shape of t121 inside the crate: a real `Agent` loop, a scripted model that gets
        // cut mid-call, and a toolbox that must stay untouched. The margin is that `tests/loop_mock.rs`
        // is out of this task's scope, so the loop half of the evidence lives here instead.
        use crate::config::ToolsConfig;
        use crate::llm::mock::{ScriptedLlm, answer_reply, truncated_reply};
        use crate::tools::{Call as ToolCall, Failure};
        use serde_json::json;

        #[derive(Default)]
        struct NoTools {
            seen: Vec<String>,
        }

        impl ToolBox for NoTools {
            async fn call(&mut self, call: ToolCall) -> Result<Value, Failure> {
                self.seen.push(format!("{call:?}"));
                Ok(json!({}))
            }
            async fn state(&mut self) -> Value {
                json!({})
            }
        }

        let llm = ScriptedLlm::new(vec![
            truncated_reply("c1", "observe", "{\"radius\":"),
            answer_reply("I could not tell, the reply was cut."),
        ])
        .with_budget(256, "ACACIA_LLM_MAX_TOKENS=256");
        let mut agent = Agent {
            llm,
            tools: NoTools::default(),
            registry: Registry::new(&ToolsConfig::default()),
            limits: Limits::default(),
            system: system_prompt(&ChatConfig::default()),
            chat: ChatConfig::default(),
            safety: SafetyConfig::default(),
            cancel: CancelFlag::default(),
        };
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-runs");
        let mut trace = Trace::open(&dir, "truncated reply", json!({"test": "truncation_tests"})).expect("trace");
        let report = agent.run("look around", &mut trace).await.expect("run");

        assert!(agent.tools.seen.is_empty(), "a half-written call must never be executed: {:?}", agent.tools.seen);
        assert_eq!(report.tool_calls, 0, "a cut reply is not a tool call");
        assert_eq!(report.stop, Stop::Answer);
        let requests = agent.llm.requests();
        // The nudge is not the last user message: the step also appends the rendered state, so every
        // user message of the retry is searched rather than just the tail.
        let said: Vec<String> = requests[1]
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .map(|message| message.content.clone().unwrap_or_default())
            .collect();
        let said = said.join("
---
");
        assert!(said.contains("被 max_tokens 截断"), "the model is told what happened: {said}");
        assert!(said.contains("max_tokens=256"), "and the budget that did it: {said}");
        assert!(said.contains("ACACIA_LLM_MAX_TOKENS=256"), "and the knob: {said}");
        let text = trace_body(&report.trace);
        assert!(text.contains("truncated"), "the trace keeps the record: {text}");
        assert!(text.contains("max_tokens_origin"), "with the origin, not just the number: {text}");
        assert!(!text.contains("invalid_argument"), "a cut reply is not the model's JSON mistake: {text}");
    }

    /// The JSONL a run wrote, for the assertions that need the record itself.
    fn trace_body(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).expect("trace file")
    }

    #[test]
    fn a_second_reason_keeps_its_own_name() {
        let cut = CutShort::Other("content_filter".to_owned());
        assert_eq!(cut.as_str(), "content_filter");
        let note = truncation_note(&cut, Some(&(512, "the default 4096".to_owned())));
        assert!(note.contains("finish_reason=content_filter"), "the provider's word is preserved: {note}");
        assert!(note.contains("max_tokens=512"), "and so is the real budget: {note}");
    }
}
