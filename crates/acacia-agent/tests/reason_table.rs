//! Pins the failure taxonomy. The strings are the contract with the model *and* with the bridge
//! (`acacia-netease`'s `fail_reason`), so they must not drift until `acacia-bot` grows
//! `ActionError::reason()` (planned for t9).

use acacia_agent::tools::result::{REASONS, classify_not_possible, classify_rejected, reason};
use acacia_bot::ActionError;

#[test]
fn not_possible_messages_classify_to_the_documented_reasons() {
    let cases = [
        ("nothing to break here", "no_block"),
        ("nothing held to place", "no_item"),
        ("no aim at the block", "not_visible"),
        ("target is 12 blocks away", "out_of_reach"),
        ("unreachable target: entity 42 at [1, 2, 3] is 11 blocks below the bot", "unreachable_target"),
        (
            "position_desync: the client's body is 8.31 blocks from the position the server reports",
            "position_desync",
        ),
        ("missing ingredient: minecraft:oak_planks", "missing_ingredient"),
        ("no recipe for minecraft:stick", "no_recipe"),
        ("no room to place the block", "no_space"),
        ("this is a physics bot with no controls", "no_physics"),
        ("not allowed in this game mode", "game_mode"),
        // Local budgets, not refusals: the fight stopped swinging and the pilot stopped walking.
        ("the fight with entity 1444 did not end in 60s (9 swings, 0 retreats)", "timeout"),
        ("did not end in 60s", "timeout"),
        ("cannot reach [10, 64, 20]: no closer for 60 ticks", "timeout"),
    ];
    for (message, expected) in cases {
        assert_eq!(classify_not_possible(message), expected, "message {message:?}");
    }
}

/// `refused` is "the server refused"; `timeout` is "the bot ran out of time". A fight that ran out
/// of clock must never answer a hint about anticheat or claims — that told the model to give up on a
/// fight it may have won (live t20: 9 swings, target gone, `rejected` + "server refused it").
#[test]
fn a_fight_that_ran_out_of_clock_is_a_timeout_with_a_honest_hint() {
    let error = ActionError::NotPossible("the fight with entity 1444 did not end in 60s (9 swings, 0 retreats)".to_owned());
    assert_eq!(reason(&error), "timeout");
    assert_ne!(reason(&error), "rejected");
    let failure = acacia_agent::tools::result::from_action_error(&error);
    let hint = failure.hint.clone().unwrap_or_default().to_lowercase();
    assert!(!hint.contains("server refused"), "hint must not blame the server: {hint}");
    assert!(!hint.contains("anticheat"), "hint must not blame anticheat: {hint}");
    assert!(hint.contains("time"), "hint says time ran out: {hint}");
}

#[test]
fn rejected_messages_classify_to_the_documented_reasons() {
    assert_eq!(classify_rejected("no_effect_on_target: 12 swings at entity 42, none hurt it"), "no_effect_on_target");
    assert_eq!(classify_rejected("breaking stone at [1, 2, 3]: server said no"), "rejected");
}

#[test]
fn action_error_variants_map_to_the_documented_reasons() {
    assert_eq!(reason(&ActionError::Timeout), "timeout");
    assert_eq!(reason(&ActionError::Disconnected), "disconnected");
    assert_eq!(reason(&ActionError::Cancelled), "cancelled");
    assert_eq!(reason(&ActionError::Rejected("claim".to_owned())), "rejected");
    assert_eq!(reason(&ActionError::NotVisible("behind a wall".to_owned())), "not_visible");
    assert_eq!(reason(&ActionError::NotPossible("nothing to break".to_owned())), "no_block");
}

#[test]
fn the_whole_vocabulary_is_listed() {
    for expected in [
        "no_block",
        "no_item",
        "not_visible",
        "out_of_reach",
        "missing_ingredient",
        "no_recipe",
        "rejected",
        "timeout",
        "no_space",
        "no_physics",
        "game_mode",
        "disconnected",
        "cancelled",
    ] {
        assert!(REASONS.contains(&expected), "{expected} missing from REASONS");
    }
    for expected in ["no_entity", "needs_table", "busy", "invalid_argument", "unknown_tool"] {
        assert!(REASONS.contains(&expected), "{expected} missing from REASONS");
    }
}

/// A fight with nothing in range is "there is no entity", not "there is no block": the model was
/// told `no_block` and went looking for something to dig.
#[test]
fn an_empty_hunt_is_no_entity_not_no_block() {
    let failure = acacia_agent::tools::Failure::new("no_entity", "no attackable entity within 16 blocks");
    let json = failure.to_json();
    assert_eq!(json["reason"], "no_entity");
    assert_ne!(json["reason"], "no_block");
    let hint = acacia_agent::tools::result::hint_for("no_entity").expect("a hint").to_lowercase();
    assert!(hint.contains("entities"), "hint points at entities: {hint}");
}

/// The hotbar being full is not the world being full: `no_space` is about the target cell, and the
/// way out of `hotbar_full` is a swap (automatic now) or a `drop`.
#[test]
fn a_full_hotbar_is_not_the_same_as_no_room_in_the_world() {
    let hotbar = acacia_agent::tools::result::hint_for("hotbar_full").expect("a hint").to_lowercase();
    let space = acacia_agent::tools::result::hint_for("no_space").expect("a hint").to_lowercase();
    assert!(hotbar.contains("hotbar"), "hotbar_full hint names the hotbar: {hotbar}");
    assert!(hotbar.contains("drop"), "and the way out: {hotbar}");
    assert!(!space.contains("hotbar"), "no_space is about the target cell: {space}");
    assert_ne!(hotbar, space);
    assert!(acacia_agent::tools::result::REASONS.contains(&"hotbar_full"));
    assert!(acacia_agent::tools::result::REASONS.contains(&"no_space"));
}

/// `no_recipe` = nothing in the book; `missing_ingredient` = a recipe exists but items are short;
/// `needs_table` = a recipe exists (materials may well be enough) but it needs the 3x3 grid.
#[test]
fn needs_table_is_its_own_mistake_not_a_missing_recipe() {
    let table = acacia_agent::tools::result::hint_for("needs_table").expect("a hint").to_lowercase();
    let recipe = acacia_agent::tools::result::hint_for("no_recipe").expect("a hint").to_lowercase();
    let ingredient = acacia_agent::tools::result::hint_for("missing_ingredient").expect("a hint").to_lowercase();
    assert!(table.contains("3x3") && table.contains("table"), "needs_table says 3x3 + table: {table}");
    assert!(!recipe.contains("3x3"), "no_recipe must not talk about the grid size: {recipe}");
    assert_ne!(table, recipe);
    assert_ne!(table, ingredient);
    assert!(acacia_agent::tools::result::REASONS.contains(&"needs_table"));
}

/// The trade failures are the bot's own words, prefixed so they classify without a guess.
#[test]
fn trade_failures_keep_their_own_reasons() {
    use acacia_agent::tools::result::{classify_not_possible, hint_for};
    assert_eq!(classify_not_possible("no_offers: the trader has 0 offers"), "no_offers");
    assert_eq!(classify_not_possible("missing_offer_cost: not enough minecraft:emerald to pay"), "missing_offer_cost");
    assert_eq!(classify_not_possible("trade_failed: offer 2 is sold out"), "trade_failed");
    for reason in ["no_villager", "no_offers", "missing_offer_cost", "trade_failed"] {
        assert!(acacia_agent::tools::result::REASONS.contains(&reason), "{reason} missing from REASONS");
        let hint = hint_for(reason).unwrap_or_else(|| panic!("{reason} has a hint")).to_lowercase();
        assert!(!hint.is_empty());
    }
    // The actions differ: finding a villager, looking at offers, paying, or picking another offer.
    let villager = hint_for("no_villager").expect("hint").to_lowercase();
    let offers = hint_for("no_offers").expect("hint").to_lowercase();
    let cost = hint_for("missing_offer_cost").expect("hint").to_lowercase();
    assert!(villager.contains("entities"), "no_villager points at entities: {villager}");
    assert!(offers.contains("trades"), "no_offers points at trades: {offers}");
    assert!(cost.contains("trades") && cost.contains("gather"), "missing_offer_cost says look then gather: {cost}");
}

#[test]
fn unknown_not_possible_messages_fall_back_to_refused() {
    assert_eq!(classify_not_possible("something else entirely"), "refused");
}
