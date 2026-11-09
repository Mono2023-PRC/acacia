//! Registry, schema and argument-validation tests.
//!
//! These are the contract the model sees: every tool the design lists exists, every schema is an
//! object, and a bad argument answers `invalid_argument` with the argument named.

use acacia_agent::config::ToolsConfig;
use acacia_agent::perceive::Detail;
use acacia_agent::tools::{Call, ContainerOp, CraftMode, EquipSlot, OfferChoice, Registry};
use serde_json::json;

fn registry() -> Registry {
    Registry::new(&ToolsConfig::default())
}

#[test]
fn sleep_pickup_and_stop_have_schemas_and_defaults() {
    // sleep: no required arguments (nearest bed), radius/timeout bounded.
    match registry().resolve("sleep", None).expect("sleep with no arguments") {
        Call::Sleep { at, radius, timeout_s } => {
            assert_eq!(at, None);
            assert_eq!(radius, 8);
            assert_eq!(timeout_s, 90);
        }
        other => panic!("unexpected call {other:?}"),
    }
    // Numeric bounds clamp (like `observe`), they are not errors.
    match registry().resolve("sleep", Some(&json!({"radius": 99}))).expect("radius clamps") {
        Call::Sleep { radius, .. } => assert_eq!(radius, 32),
        other => panic!("unexpected call {other:?}"),
    }

    // pickup: item is optional, radius defaults to 6.
    match registry().resolve("pickup", None).expect("pickup with no arguments") {
        Call::Pickup { id, item, radius, timeout_s } => {
            assert_eq!(id, None);
            assert_eq!(item, None);
            assert_eq!(radius, 6);
            assert_eq!(timeout_s, 60);
        }
        other => panic!("unexpected call {other:?}"),
    }
    match registry().resolve("pickup", Some(&json!({"item": "minecraft:oak_log", "radius": 12}))).expect("pickup oak") {
        Call::Pickup { id, item, radius, .. } => {
            assert_eq!(id, None);
            assert_eq!(item.as_deref(), Some("minecraft:oak_log"));
            assert_eq!(radius, 12);
        }
        other => panic!("unexpected call {other:?}"),
    }

    // stop: no arguments at all, and it is a write so it takes the action slot.
    assert!(matches!(registry().resolve("stop", None).expect("stop"), Call::Stop));
    for name in ["sleep", "pickup", "stop"] {
        let schema = acacia_agent::tools::spec::schema(name);
        assert_eq!(schema["type"], "object", "{name} schema");
        assert_eq!(schema["additionalProperties"], false, "{name} schema");
    }
}

#[test]
fn the_catalogue_has_every_tool_the_task_requires() {
    let required = [
        "status", "observe", "find", "inventory", "block", "entities", "recipes", "capabilities", "move_to", "dig", "place",
        "attack", "container", "craft", "smelt", "eat", "equip", "look", "chat", "hold", "sleep", "pickup", "stop",
        "climb_out", "drop", "trade", "trades",
    ];
    // The static catalogue, not the filtered registry: `chat` exists but is denied by default.
    for name in required {
        assert!(acacia_agent::tools::spec(name).is_some(), "{name} is missing from the catalogue");
    }
}

#[test]
fn trade_needs_a_target_and_bounds_its_numbers() {
    let failure = registry().resolve("trade", Some(&json!({}))).expect_err("trade without a target must fail");
    assert_eq!(failure.reason, "invalid_argument");
    assert!(failure.message.contains("target"), "the message names the argument: {}", failure.message);
    match registry().resolve("trade", Some(&json!({"target": "nearest", "offer": 2, "times": 3}))).expect("trade nearest") {
        Call::Trade { target, offer, times, radius } => {
            assert_eq!(target, "nearest");
            assert_eq!(offer, OfferChoice::Index(2), "a number is still an index");
            assert_eq!(times, 3);
            assert_eq!(radius, 16.0);
        }
        other => panic!("unexpected call {other:?}"),
    }
    // t83: omitting `offer` means the first offer the inventory can pay for, not offer 0.
    match registry().resolve("trade", Some(&json!({"target": "nearest"}))).expect("trade nearest") {
        Call::Trade { offer, .. } => assert_eq!(offer, OfferChoice::FirstPayable),
        other => panic!("unexpected call {other:?}"),
    }
    // A negative `times` is an argument error the model can fix, not a silent 1.
    let failure = registry().resolve("trade", Some(&json!({"target": "7", "times": -3}))).expect_err("negative times");
    assert_eq!(failure.reason, "invalid_argument");
    assert!(failure.message.contains("times"), "the message names the argument: {}", failure.message);
    match registry().resolve("trades", Some(&json!({"target": "nearest"}))).expect("trades") {
        Call::Trades { radius, .. } => assert_eq!(radius, 16.0),
        other => panic!("unexpected call {other:?}"),
    }
}

#[test]
fn drop_needs_a_slot_or_an_item() {
    let failure = registry().resolve("drop", Some(&json!({}))).expect_err("drop without a target must fail");
    assert_eq!(failure.reason, "invalid_argument");
    match registry().resolve("drop", Some(&json!({"item": "minecraft:wooden_pickaxe", "count": 1}))).expect("drop by item") {
        Call::Drop { slot, item, count } => {
            assert_eq!(slot, None);
            assert_eq!(item.as_deref(), Some("minecraft:wooden_pickaxe"));
            assert_eq!(count, 1);
        }
        other => panic!("unexpected call {other:?}"),
    }
    match registry().resolve("drop", Some(&json!({"slot": 3}))).expect("drop by slot") {
        Call::Drop { slot, .. } => assert_eq!(slot, Some(3)),
        other => panic!("unexpected call {other:?}"),
    }
    // Out-of-range numbers are clamped like every other numeric argument (the house style), so a
    // slot past the inventory lands on the last one instead of failing the call.
    match registry().resolve("drop", Some(&json!({"slot": 40}))).expect("slot 40 clamps") {
        Call::Drop { slot, .. } => assert_eq!(slot, Some(35)),
        other => panic!("unexpected call {other:?}"),
    }
}

#[test]
fn place_needs_at_or_auto() {
    let failure = registry().resolve("place", Some(&json!({}))).expect_err("place without at or auto must fail");
    assert_eq!(failure.reason, "invalid_argument");
    assert!(failure.message.contains("auto"), "the message names the way out: {}", failure.message);
    match registry()
        .resolve("place", Some(&json!({"auto": true, "item": "minecraft:crafting_table"})))
        .expect("place auto")
    {
        Call::Place { at, auto, radius, .. } => {
            assert_eq!(at, None);
            assert!(auto);
            assert_eq!(radius, 4.0);
        }
        other => panic!("unexpected call {other:?}"),
    }
}

#[test]
fn climb_out_takes_an_optional_surface_y() {
    match registry().resolve("climb_out", None).expect("climb_out") {
        Call::ClimbOut { surface_y } => assert_eq!(surface_y, None, "omitted: use the recorded surface"),
        other => panic!("unexpected call {other:?}"),
    }
    match registry().resolve("climb_out", Some(&json!({"surface_y": 64.0}))).expect("climb_out with a height") {
        Call::ClimbOut { surface_y } => assert_eq!(surface_y, Some(64)),
        other => panic!("unexpected call {other:?}"),
    }
    let schema = acacia_agent::tools::spec::schema("climb_out");
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["surface_y"].is_object(), "surface_y is documented: {schema}");
}

#[test]
fn chat_is_denied_by_default_but_the_rest_is_listed() {
    let registry = registry();
    assert!(registry.specs().iter().all(|spec| spec.name != "chat"));
    // 23 tools total, 1 (chat) denied by default.
    assert_eq!(registry.specs().len(), 26);
}

#[test]
fn every_listed_tool_has_an_object_schema() {
    let list = registry().list();
    let tools = list["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 26);
    for tool in tools {
        assert!(tool["name"].is_string());
        assert!(tool["description"].is_string());
        assert_eq!(tool["inputSchema"]["type"], "object", "{} has no object schema", tool["name"]);
    }
}

#[test]
fn a_missing_required_argument_is_invalid_argument() {
    let failure = registry().resolve("dig", Some(&json!({}))).expect_err("dig without at must fail");
    assert_eq!(failure.reason, "invalid_argument");
    assert!(failure.message.contains("at"), "message names the argument: {}", failure.message);
}

#[test]
fn a_wrong_type_names_the_argument_and_the_expected_type() {
    let failure = registry().resolve("observe", Some(&json!({"radius": "eight"}))).expect_err("radius must be an integer");
    assert_eq!(failure.reason, "invalid_argument");
    assert!(failure.message.contains("radius"));
    assert!(failure.message.contains("integer"));
}

#[test]
fn unknown_tools_and_whitelist_denials_look_identical() {
    let failure = registry().resolve("teleport", None).expect_err("unknown tool");
    assert_eq!(failure.reason, "unknown_tool");

    let perception_only = Registry::new(&ToolsConfig { allow: vec!["perceive".to_owned()], deny: Vec::new() });
    assert!(perception_only.resolve("status", None).is_ok());
    let failure = perception_only.resolve("dig", Some(&json!({"at": [0, 64, 0]}))).expect_err("dig is not allowed");
    assert_eq!(failure.reason, "unknown_tool");
}

#[test]
fn a_run_allow_list_narrows_the_catalogue_further() {
    let registry = registry().with_run_allow(Some(vec!["status".to_owned(), "observe".to_owned()]));
    assert_eq!(registry.specs().len(), 2);
    assert!(registry.resolve("find", Some(&json!({"block": "minecraft:oak_log"}))).is_err());
}

#[test]
fn attack_defaults_to_fighting_not_a_single_swing() {
    // The default is "walk into reach, then fight"; `once` is the explicit single-swing escape.
    match registry().resolve("attack", Some(&json!({"target": "nearest"}))).expect("attack") {
        Call::Attack { target, radius, times, once, allow_dig, timeout_s } => {
            assert_eq!(target, "nearest");
            assert_eq!(radius, 16.0);
            assert_eq!(times, 1);
            assert!(!once, "approaching is the default");
            assert!(!allow_dig);
            assert_eq!(timeout_s, 45);
        }
        other => panic!("unexpected call {other:?}"),
    }
    match registry().resolve("attack", Some(&json!({"target": "7", "once": true, "allow_dig": true}))).expect("attack once") {
        Call::Attack { once, allow_dig, .. } => {
            assert!(once);
            assert!(allow_dig);
        }
        other => panic!("unexpected call {other:?}"),
    }
    let schema = acacia_agent::tools::spec::schema("attack");
    assert_eq!(schema["properties"]["once"]["default"], false);
    assert_eq!(schema["properties"]["radius"]["default"], 16);
}

#[test]
fn observe_defaults_match_the_design() {
    // Full body by default: a delta is an optimisation, and the first call must hand over the world.
    match registry().resolve("observe", None).expect("observe with no arguments") {
        Call::Observe { radius, detail, delta } => {
            assert_eq!(radius, 8);
            assert_eq!(detail, Detail::Full);
            assert!(!delta, "delta is opt-in");
        }
        other => panic!("unexpected call {other:?}"),
    }
    match registry().resolve("observe", Some(&json!({"delta": true}))).expect("observe delta") {
        Call::Observe { delta, .. } => assert!(delta),
        other => panic!("unexpected call {other:?}"),
    }
    let schema = acacia_agent::tools::spec::schema("observe");
    assert_eq!(schema["properties"]["delta"]["default"], false);
}

#[test]
fn observe_clamps_the_radius() {
    match registry().resolve("observe", Some(&json!({"radius": 200}))).expect("observe") {
        Call::Observe { radius, .. } => assert_eq!(radius, 24),
        other => panic!("unexpected call {other:?}"),
    }
}

#[test]
fn craft_defaults_to_the_recipe_book() {
    // The NetEase server accepts AutoCraft (relay/t1, 2026-11-08), so book mode is the default and
    // grid is the fallback for recipes the book lacks.
    match registry().resolve("craft", Some(&json!({"item": "minecraft:stick"}))).expect("craft") {
        Call::Craft { mode, count, .. } => {
            assert_eq!(mode, CraftMode::Book);
            assert_eq!(count, 1);
        }
        other => panic!("unexpected call {other:?}"),
    }
    match registry().resolve("craft", Some(&json!({"item": "minecraft:stick", "mode": "grid"}))).expect("craft grid") {
        Call::Craft { mode, .. } => assert_eq!(mode, CraftMode::Grid),
        other => panic!("unexpected call {other:?}"),
    }
}

#[test]
fn move_to_accepts_at_or_xz_but_not_neither() {
    assert!(matches!(
        registry().resolve("move_to", Some(&json!({"at": [10, 64, -3]}))).expect("at"),
        Call::MoveTo { at: Some([10, 64, -3]), .. }
    ));
    assert!(matches!(
        registry().resolve("move_to", Some(&json!({"xz": [10, -3]}))).expect("xz"),
        Call::MoveTo { xz: Some([10, -3]), .. }
    ));
    let failure = registry().resolve("move_to", Some(&json!({}))).expect_err("no target");
    assert_eq!(failure.reason, "invalid_argument");
}

#[test]
fn container_needs_an_operation_and_reads_its_slot() {
    let failure = registry().resolve("container", Some(&json!({"at": [1, 2, 3]}))).expect_err("op is required");
    assert_eq!(failure.reason, "invalid_argument");
    match registry()
        .resolve("container", Some(&json!({"at": [1, 2, 3], "op": "take", "item": "minecraft:bread", "count": 3, "slot": 12})))
        .expect("container take")
    {
        Call::Container { op, count, slot, keep_open, .. } => {
            assert_eq!(op, ContainerOp::Take);
            assert_eq!(count, 3);
            assert_eq!(slot, Some(12));
            assert!(!keep_open);
        }
        other => panic!("unexpected call {other:?}"),
    }
}

#[test]
fn equip_defaults_to_the_hand_and_names_its_slots() {
    match registry().resolve("equip", Some(&json!({"item": "minecraft:shield"}))).expect("equip") {
        Call::Equip { slot, .. } => assert_eq!(slot, EquipSlot::Hand),
        other => panic!("unexpected call {other:?}"),
    }
    match registry().resolve("equip", Some(&json!({"item": "minecraft:iron_helmet", "slot": "head"}))).expect("equip head") {
        Call::Equip { slot, .. } => assert_eq!(slot, EquipSlot::Head),
        other => panic!("unexpected call {other:?}"),
    }
}

#[test]
fn a_block_position_can_be_an_array_or_an_object() {
    assert!(matches!(
        registry().resolve("block", Some(&json!({"at": [1, 2, 3]}))).expect("array at"),
        Call::Block { at: Some([1, 2, 3]), .. }
    ));
    assert!(matches!(
        registry().resolve("block", Some(&json!({"at": {"x": 1, "y": 2, "z": 3}}))).expect("object at"),
        Call::Block { at: Some([1, 2, 3]), .. }
    ));
}

#[test]
fn write_tools_are_marked_as_writes() {
    let registry = registry();
    let mut writes = Vec::new();
    for spec in registry.specs() {
        let is_write = matches!(
            registry.resolve(spec.name, Some(&sample_arguments(spec.name))).map(|call| call.is_write()),
            Ok(true)
        );
        if is_write {
            writes.push(spec.name);
        }
    }
    for expected in ["move_to", "dig", "place", "attack", "container", "craft", "smelt", "eat", "equip", "sleep", "pickup", "stop"] {
        assert!(writes.contains(&expected), "{expected} should take the action slot");
    }
    assert!(!writes.contains(&"status"));
    assert!(!writes.contains(&"hold"));
    assert!(!writes.contains(&"look"));
}

fn sample_arguments(name: &str) -> serde_json::Value {
    match name {
        "dig" | "place" => json!({"at": [0, 64, 0]}),
        "move_to" => json!({"at": [0, 64, 0]}),
        "craft" => json!({"item": "minecraft:stick"}),
        "smelt" => json!({"at": [0, 64, 0], "input": "minecraft:raw_iron"}),
        "attack" => json!({"target": "nearest"}),
        "container" => json!({"op": "list"}),
        "equip" => json!({"item": "minecraft:bread"}),
        "hold" => json!({"item": "minecraft:bread"}),
        "find" => json!({"block": "minecraft:oak_log"}),
        "chat" => json!({"text": "hello"}),
        "sleep" => json!({}),
        "pickup" => json!({}),
        "stop" => json!({}),
        "look" => json!({"yaw": 0.0}),
        _ => json!({}),
    }
}
