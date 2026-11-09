//! Tool results and the failure taxonomy the model reasons about.
//!
//! A tool never fails the run: it returns `{"ok":false,"reason":…,"message":…}` and the model picks
//! another approach. The `reason` vocabulary is the one `acacia-netease` prints (`fail_reason` in
//! `crates/acacia-netease/src/bot.rs`), copied here so this crate stands alone; `docs/agent-design.md`
//! ("Result envelope") explains why it is duplicated instead of hoisted into `acacia-bot`.

use acacia_bot::ActionError;
use serde_json::{Value, json};

/// Every reason the registry can emit, in the order the design document lists them.
///
/// This is the **closed** vocabulary: `Failure::new` debug-asserts membership, so a tool that invents a
/// reason fails its tests instead of shipping a name nobody can look up. `no_such_entity` and `refused`
/// were emitted for a while without being listed here (t69/t76) — that is exactly the drift
/// `tests/reason_table.rs` now pins in both directions.
pub const REASONS: [&str; 32] = [
    "no_block",
    "no_item",
    "no_entity",
    "no_such_entity",
    "entity_gone",
    "no_path",
    "no_villager",
    "no_offers",
    "missing_offer_cost",
    "trade_failed",
    "not_visible",
    "out_of_reach",
    "unreachable_target",
    "blocked_by_terrain",
    "position_desync",
    "client_stale",
    "no_effect_on_target",
    "missing_ingredient",
    "no_recipe",
    "needs_table",
    "rejected",
    "timeout",
    "no_space",
    "hotbar_full",
    "no_physics",
    "game_mode",
    "disconnected",
    "cancelled",
    "busy",
    "refused",
    "invalid_argument",
    "unknown_tool",
];

/// A failed tool call.
#[derive(Debug, Clone)]
pub struct Failure {
    pub reason: &'static str,
    pub message: String,
    /// One line, imperative, no paths or stack traces. Owned so a tool can say *which* block hid the
    /// target or *which* cell to try instead — a static table cannot.
    pub hint: Option<String>,
    /// Extra machine-readable fields for this one failure (`settled_ticks` and the two positions for a stale body):
    /// the model reads `message`, a verifier reads these.
    pub details: Option<Value>,
}

impl Failure {
    pub fn new(reason: &'static str, message: impl Into<String>) -> Self {
        debug_assert!(
            REASONS.contains(&reason),
            "reason {reason:?} is not in REASONS: add it (and its hint, and its row in docs/agent-design.md) or reuse an existing one"
        );
        let message: String = message.into();
        let message = if message.trim().is_empty() { describe(reason) } else { message };
        Self { reason, message, hint: hint_for(reason), details: None }
    }

    /// A schema or argument error: the model can fix these by itself.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self {
            reason: "invalid_argument",
            message: message.into(),
            hint: Some("fix the arguments and call again".to_owned()),
            details: None,
        }
    }

    pub fn unknown_tool(name: &str) -> Self {
        Self {
            reason: "unknown_tool",
            message: format!("no tool named {name} is available"),
            hint: Some("call tools/list or status to see what is allowed in this run".to_owned()),
            details: None,
        }
    }

    pub fn busy(tool: &str) -> Self {
        Self {
            reason: "busy",
            message: format!("another action is still running; {tool} took the action slot"),
            hint: Some("run status, wait, or cancel the running action".to_owned()),
            details: None,
        }
    }

    pub fn disconnected(message: impl Into<String>) -> Self {
        Self { reason: "disconnected", message: message.into(), hint: None, details: None }
    }

    /// Replaces the generic hint with one built from what the tool just learned.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn to_json(&self) -> Value {
        // Belt and braces for failures built as struct literals: an empty message is the shape that made t69
        // unreadable, and it must not reach the model from any path.
        let message: String = if self.message.trim().is_empty() {
            describe(self.reason)
        } else {
            self.message.clone()
        };
        let mut out = json!({"ok": false, "reason": self.reason, "message": message});
        if let Some(details) = &self.details {
            out["details"] = details.clone();
        }
        // Same reason: a literal with `hint: None` used to reach the model as a reason and nothing else,
        // which is the t69 shape (`{"reason":"no_block","message":"","data":null}` × 11).
        match &self.hint {
            Some(hint) => out["hint"] = json!(hint),
            None => {
                if let Some(hint) = hint_for(self.reason) {
                    out["hint"] = json!(hint);
                }
            }
        }
        out
    }
}

/// Wraps a successful payload.
pub fn ok(data: Value) -> Value {
    json!({"ok": true, "data": data})
}

/// Every hint, as a table rather than a `match`: a test can then walk it in both directions
/// (`REASONS` ⊆ keys, keys ⊆ `REASONS`), which a `match` with a `_ => None` arm can never prove.
pub const HINTS: [(&str, &str); 32] = [
    ("no_block", "nothing there to break; look, or dig a different block"),
    ("no_item", "equip or hold the item first, or craft/smelt it"),
    ("no_entity", "nothing to attack there: call entities to see what is around, then pick a target"),
    (
        "no_such_entity",
        "that runtime id is not tracked: it may already have been auto-picked-up (walking near a drop collects it) — check the count in inventory, then run entities and use the id it lists",
    ),
    (
        "entity_gone",
        "the drop vanished between planning and the walk: call entities and use a fresh id (a position is not one)",
    ),
    (
        "no_path",
        "no walking route to that drop: move_to open ground near it first, then pickup by its id",
    ),
    (
        "no_villager",
        "no villager within reach: call entities (villager / wandering_trader) and move_to one first",
    ),
    (
        "no_offers",
        "the trader opened no offers: it may need a workstation or a level; call trades again after it restocks",
    ),
    (
        "missing_offer_cost",
        "that offer's price is not in the bag: call trade with offer:\"first_payable\" (or pick one of trades' payable_offers) instead of gathering first — the message names what is short",
    ),
    (
        "trade_failed",
        "the server refused that offer (sold out, or the trader's level is too low): pick another offer from trades",
    ),
    (
        "not_visible",
        "something hides the target and the message names it: `block [x, y, z] is behind [a, b, c]` — dig [a, b, c] first,          then look and dig the target; a second not_visible on the same cell means pick another cell or move, not retry",
    ),
    ("out_of_reach", "move_to within 4.5 blocks of the target"),
    ("unreachable_target", "pick another target: this one cannot be pathed to, and digging a route failed"),
    ("blocked_by_terrain", "a route exists but the body cannot walk it: take another direction, or allow_dig:true"),
    (
        "position_desync",
        "the clicks would be dropped: read status.position_desync, move or wait for the server reference, then retry",
    ),
    ("client_stale", "the snapshot is old: call observe/status again before acting on it"),
    ("no_effect_on_target", "the clicks are not landing; stop swinging and check status.position_desync"),
    ("missing_ingredient", "gather the missing item listed in the message"),
    ("no_recipe", "check recipes; there is no recipe for that item in the book"),
    ("needs_table", "this recipe is 3x3: place or find a crafting_table, then retry with table:[x,y,z]"),
    (
        "rejected",
        "the server refused it (anticheat, claim or game rule): re-read the cell or the inventory before retrying, and do not repeat the same call",
    ),
    (
        "timeout",
        "the time budget ran out, not the server refusing: if the bot stood still it was blocked by terrain (allow_dig / another route), otherwise it was fighting or waiting for an echo",
    ),
    ("no_space", "clear room or pick another spot"),
    ("hotbar_full", "the hotbar is full: drop something, or let the bot swap a slot"),
    ("no_physics", "this bot has no physics simulation; not supported here"),
    ("game_mode", "creative-only action; this bot is in survival"),
    ("disconnected", "the connection is gone; reconnect, then resume or stop"),
    ("cancelled", "the run was cancelled"),
    ("busy", "wait for the running action to finish"),
    ("refused", "the bot declined the action and said why in the message; read it before retrying"),
    ("invalid_argument", "fix the arguments and call again"),
    ("unknown_tool", "pick a tool that is allowed in this run"),
];

/// A one-line hint that travels with a reason, so the model does not need the design document.
pub fn hint_for(reason: &str) -> Option<String> {
    HINTS.iter().find(|(name, _)| *name == reason).map(|(_, hint)| (*hint).to_owned())
}

/// What the model is told when a failure arrives with no message at all.
///
/// t69 shipped `{"ok":false,"reason":"no_block","message":""}` eleven times: the model could see *that*
/// the pickup failed and nothing about why, so it retried blind. A failure always says something now.
fn describe(reason: &str) -> String {
    format!("{reason}: the tool reported no detail about what went wrong")
}

/// `ActionError` → reason, verbatim from the bridge.
pub fn reason(error: &ActionError) -> &'static str {
    match error {
        ActionError::Timeout => "timeout",
        ActionError::Disconnected => "disconnected",
        ActionError::Rejected(message) => classify_rejected(message),
        ActionError::NotVisible(_) => "not_visible",
        ActionError::Cancelled => "cancelled",
        ActionError::NotPossible(message) => classify_not_possible(message),
        _ => "refused",
    }
}

/// The occluder a `not_visible` message names, when acacia-bot knew one: `block [x, y, z] is behind [a, b, c] —
/// dig that one first` yields `[a, b, c]`, the cell to dig first. The vague fallback ("behind other blocks")
/// yields `None`, so the tool never invents a cell to blame. See `acacia-bot/src/interact/mod.rs::behind_message`.
pub fn not_visible_occluder(message: &str) -> Option<[i32; 3]> {
    let behind = message.split_once(" is behind ")?.1;
    let cell = behind.split_once('[')?.1.split_once(']')?.0;
    let mut out = [0i32; 3];
    let mut parts = cell.split(',');
    for slot in out.iter_mut() {
        *slot = parts.next()?.trim().parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

/// The message-based classification an `ActionError::Rejected` needs: a refusal the bot made for a reason the
/// model can act on (a body out of sync, a target the server never counted a hit on) is its own reason.
pub fn classify_rejected(message: &str) -> &'static str {
    if message.starts_with("no_effect_on_target") {
        "no_effect_on_target"
    } else {
        "rejected"
    }
}

/// The message-based classification `ActionError::NotPossible` needs: acacia-bot builds these
/// messages itself, and until `ActionError::reason()` exists in `acacia-bot` (planned for t9), the
/// strings are the contract. `crates/acacia-agent/tests/reason_table.rs` pins every one of them.
///
/// The line that matters: **`rejected` means the server explicitly refused** (there is a reply, a
/// claim, an anticheat kick); **`timeout` means the bot's own budget ran out** with no answer from
/// the server. A fight that stops swinging after 60 s is the second kind.
pub fn classify_not_possible(message: &str) -> &'static str {
    if message.starts_with("no_offers:") {
        "no_offers"
    } else if message.starts_with("missing_offer_cost:") {
        "missing_offer_cost"
    } else if message.starts_with("trade_failed:") {
        "trade_failed"
    } else if message.starts_with("client_stale") {
        "client_stale"
    } else if message.starts_with("position_desync") {
        "position_desync"
    } else if message.starts_with("unreachable target") {
        "unreachable_target"
    } else if message.starts_with("the fight with entity") || message.starts_with("did not end in") {
        // `Bot::fight` ran out of its own clock: the swing loop stopped, the server said nothing.
        // That is a local budget, not a refusal — calling it `rejected` told the model the server
        // had refused an attack it may well have won.
        "timeout"
    } else if message.contains("no closer for") {
        // The pilot gave up after standing still: local budget again.
        "timeout"
    } else if message.starts_with("nothing to break") {
        "no_block"
    } else if message.contains("nothing held") {
        "no_item"
    } else if message.contains("blocks away") {
        "out_of_reach"
    } else if message.starts_with("no recipe for") {
        "no_recipe"
    } else if message.starts_with("missing ingredient") {
        "missing_ingredient"
    } else if message.contains("no room") {
        "no_space"
    } else if message.contains("physics bot") {
        "no_physics"
    } else if message.contains("game mode") {
        "game_mode"
    } else if message.starts_with("no aim") {
        "not_visible"
    } else {
        "refused"
    }
}

/// Turns an `ActionError` into a [`Failure`], keeping the bot's own message for the trace.
pub fn from_action_error(error: &ActionError) -> Failure {
    let reason = reason(error);
    let mut failure = Failure::new(reason, error.to_string());
    failure.hint = hint_for(reason);
    failure
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hint_names_the_two_steps_the_occluder_message_allows() {
        // t101: nine `not_visible` in one live run, five of them the same cell retried in place. The hint must
        // turn the occluder the bot now names into a next step, and forbid the in-place retry.
        let hint = hint_for("not_visible").expect("not_visible has a hint");
        assert!(hint.contains("dig [a, b, c] first"), "{hint}");
        assert!(hint.contains("look and dig the target"), "{hint}");
        assert!(hint.contains("not retry") || hint.contains("not retry it"), "{hint}");
        // The bot's real sentence (interact/mod.rs::behind_message) is what the model sees beside it.
        let message = "block [150026, 2, 149828] is behind [150026, 3, 149828] — dig that one first";
        assert_eq!(not_visible_occluder(message), Some([150026, 3, 149828]));
        assert_eq!(not_visible_occluder("block [150026, 2, 149828] is behind other blocks"), None);
        assert_eq!(not_visible_occluder("entity 42 is behind blocks"), None);
        assert_eq!(not_visible_occluder(""), None);
    }

    #[test]
    fn a_not_visible_error_with_an_occluder_still_maps_to_not_visible() {
        let error = ActionError::NotVisible(
            "block [150026, 2, 149828] is behind [150026, 3, 149828] — dig that one first".to_owned(),
        );
        assert_eq!(reason(&error), "not_visible");
        assert_eq!(not_visible_occluder(&error.to_string()), Some([150026, 3, 149828]));
    }
}
