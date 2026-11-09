//! Table-driven tests for the recursive `get` planner: given a source table and what the inventory
//! holds, the plan must be the *shortest sensible* chain — no wasted digs, no silent 2×2 squeeze, and a
//! named reason when it cannot be done. The planner is pure, so none of this needs a server.

use std::collections::BTreeMap;

use acacia_bot::workstation::{have_from, plan_get, PlanError, Source, Sources, Step, TABLE_BLOCK};

/// The step kinds a case expects, in order.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Mine(String),
    Craft { item: String, count: u32, table: bool },
    Smelt { item: String, count: u32 },
    PlaceTable,
}

fn kind(step: &Step) -> Kind {
    match step {
        Step::Mine { block, count, .. } => Kind::Mine(format!("{count} {block}")),
        Step::Craft { item, count, table } => Kind::Craft { item: item.clone(), count: *count, table: *table },
        Step::Smelt { item, count, .. } => Kind::Smelt { item: item.clone(), count: *count },
        Step::PlaceTable { .. } => Kind::PlaceTable,
    }
}

fn craft(_item: &str, inputs: &[(&str, u32)], output: u32) -> Source {
    Source::Craft {
        inputs: inputs.iter().map(|(name, count)| ((*name).to_owned(), *count)).collect(),
        output,
        table: false,
    }
}

fn craft3(_item: &str, inputs: &[(&str, u32)], output: u32) -> Source {
    Source::Craft {
        inputs: inputs.iter().map(|(name, count)| ((*name).to_owned(), *count)).collect(),
        output,
        table: true,
    }
}

fn mine(block: &str) -> Source {
    Source::Mine { block: block.to_owned(), tool: None }
}

fn smelt(input: &str, fuel: Option<&str>) -> Source {
    Source::Smelt { input: input.to_owned(), fuel: fuel.map(str::to_owned) }
}

/// A tiny item graph shared by the cases: logs → planks → sticks, and the 3×3 pickaxe on top.
fn wood_sources() -> Sources {
    let mut sources = Sources::default();
    sources.add("minecraft:oak_log", mine("minecraft:oak_log"));
    sources.add("minecraft:oak_planks", craft("minecraft:oak_planks", &[("minecraft:oak_log", 1)], 4));
    sources.add("minecraft:stick", craft("minecraft:stick", &[("minecraft:oak_planks", 2)], 4));
    sources.add(TABLE_BLOCK, craft(TABLE_BLOCK, &[("minecraft:oak_planks", 4)], 1));
    sources.add(
        "minecraft:wooden_pickaxe",
        craft3("minecraft:wooden_pickaxe", &[("minecraft:oak_planks", 3), ("minecraft:stick", 2)], 1),
    );
    sources
}

#[test]
fn plans_are_the_shortest_sensible_chain() {
    // name, sources, what is held, what is wanted, expected steps (or an error).
    let cases: Vec<(&str, Sources, Vec<(&str, u32)>, &str, u32, Result<Vec<Kind>, PlanError>)> = vec![
        (
            "already enough does nothing at all",
            wood_sources(),
            vec![("minecraft:stick", 4)],
            "minecraft:stick",
            4,
            Ok(vec![]),
        ),
        (
            "a zero count is an empty plan, not a missing source",
            wood_sources(),
            vec![],
            "minecraft:not_a_real_item",
            0,
            Ok(vec![]),
        ),
        (
            "eight sticks from two logs: planks first, and no extra digs",
            wood_sources(),
            vec![("minecraft:oak_log", 2)],
            "minecraft:stick",
            8,
            Ok(vec![
                Kind::Craft { item: "minecraft:oak_planks".into(), count: 4, table: false },
                Kind::Craft { item: "minecraft:stick".into(), count: 8, table: false },
            ]),
        ),
        (
            "a held stock is spent before anything is dug",
            wood_sources(),
            vec![("minecraft:oak_planks", 4)],
            "minecraft:stick",
            8,
            Ok(vec![Kind::Craft { item: "minecraft:stick".into(), count: 8, table: false }]),
        ),
        ("a plain dig when nothing else makes it", wood_sources(), vec![], "minecraft:oak_log", 3, Ok(vec![
            Kind::Mine("3 minecraft:oak_log".into()),
        ])),
        (
            "smelting plans the input and the fuel first",
            {
                let mut sources = Sources::default();
                sources.add("minecraft:iron_ingot", smelt("minecraft:raw_iron", Some("minecraft:coal")));
                sources.add("minecraft:raw_iron", mine("minecraft:iron_ore"));
                sources.add("minecraft:coal", mine("minecraft:coal_ore"));
                sources
            },
            vec![],
            "minecraft:iron_ingot",
            2,
            Ok(vec![
                Kind::Mine("2 minecraft:iron_ore".into()),
                Kind::Mine("1 minecraft:coal_ore".into()),
                Kind::Smelt { item: "minecraft:iron_ingot".into(), count: 2 },
            ]),
        ),
        (
            "a 3x3 recipe gets a table made and placed before it",
            wood_sources(),
            vec![("minecraft:oak_log", 4)],
            "minecraft:wooden_pickaxe",
            1,
            // The three planks shortages (table 4, pickaxe 3, sticks 2) are coalesced into one craft of
            // 12 planks at the front: same three logs, three fewer steps.
            Ok(vec![
                Kind::Craft { item: "minecraft:oak_planks".into(), count: 12, table: false },
                Kind::Craft { item: TABLE_BLOCK.into(), count: 1, table: false },
                Kind::PlaceTable,
                Kind::Craft { item: "minecraft:stick".into(), count: 4, table: false },
                Kind::Craft { item: "minecraft:wooden_pickaxe".into(), count: 1, table: true },
            ]),
        ),
        (
            "an item nothing makes and nothing holds says so",
            wood_sources(),
            vec![],
            "minecraft:netherite_ingot",
            1,
            Err(PlanError::NoSource { item: "minecraft:netherite_ingot".into() }),
        ),
        (
            "a recipe cycle is reported instead of looping",
            {
                let mut sources = Sources::default();
                sources.add("minecraft:a", craft("minecraft:a", &[("minecraft:b", 1)], 1));
                sources.add("minecraft:b", craft("minecraft:b", &[("minecraft:a", 1)], 1));
                sources
            },
            vec![],
            "minecraft:a",
            1,
            Err(PlanError::Cycle { item: "minecraft:a".into() }),
        ),
    ];
    assert!(cases.len() >= 8, "the table is the test");
    for (name, sources, held, item, count, expected) in cases {
        let have: BTreeMap<String, u32> = have_from(held);
        let got = plan_get(&sources, &have, item, count);
        match expected {
            Ok(kinds) => {
                let plan = got.unwrap_or_else(|e| panic!("{name}: unexpected {e}"));
                let rendered: Vec<Kind> = plan.steps.iter().map(kind).collect();
                assert_eq!(rendered, kinds, "{name}: {}", plan.summary());
            }
            Err(want) => assert_eq!(got.err(), Some(want), "{name}"),
        }
    }
}

#[test]
fn the_table_steps_run_before_the_three_by_three_craft() {
    let have = have_from([("minecraft:oak_log", 4u32)]);
    let plan = plan_get(&wood_sources(), &have, "minecraft:wooden_pickaxe", 1).expect("plan");
    assert!(plan.needs_table());
    let place = plan.steps.iter().position(|s| matches!(s, Step::PlaceTable { .. })).expect("place table");
    let pick = plan
        .steps
        .iter()
        .position(|s| matches!(s, Step::Craft { item, table: true, .. } if item == "minecraft:wooden_pickaxe"))
        .expect("3x3 craft");
    assert!(place < pick, "{}", plan.summary());
    // Four logs are enough for the table, the planks and the sticks: nothing else is dug.
    assert_eq!(plan.steps.iter().filter(|s| matches!(s, Step::Mine { .. })).count(), 0, "{}", plan.summary());
}

/// Replay a plan against the source table: every craft must have its full input count in the pool at the
/// moment it runs. This is the property a "just emit the steps" planner gets wrong, and the reason the
/// planner keeps a simulated inventory.
fn assert_affordable(name: &str, plan: &acacia_bot::workstation::Plan, sources: &Sources, have: &BTreeMap<String, u32>) {
    let mut pool = have.clone();
    for step in &plan.steps {
        match step {
            Step::Mine { item, count, .. } | Step::Smelt { item, count, .. } => {
                *pool.entry(item.clone()).or_default() += count;
            }
            Step::Craft { item, count, table } => {
                let (inputs, output) = sources
                    .get(item)
                    .iter()
                    .find_map(|source| match source {
                        Source::Craft { inputs, output, table: t } if t == table => Some((inputs.clone(), *output)),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("no recipe for {item}"));
                let crafts = count.div_ceil(output.max(1));
                for (input, per) in &inputs {
                    let need = per * crafts;
                    let available = pool.get(input).copied().unwrap_or(0);
                    assert!(
                        available >= need,
                        "{name}: `{item}` needs {need} {input} but the pool held {available} ({})",
                        plan.summary()
                    );
                    pool.insert(input.clone(), available - need);
                }
                *pool.entry(item.clone()).or_default() += count;
            }
            Step::PlaceTable { .. } => {}
        }
    }
    assert!(pool.get("minecraft:stick").copied().unwrap_or(0) >= 8, "{name}: {}", plan.summary());
}

#[test]
fn a_plan_never_hands_the_caller_a_recipe_it_cannot_afford() {
    let sources = wood_sources();
    for held in [vec![("minecraft:oak_log", 1u32)], vec![("minecraft:oak_log", 4u32)], vec![]] {
        let have: BTreeMap<String, u32> = have_from(held.clone());
        let plan = plan_get(&sources, &have, "minecraft:stick", 8).expect("plan");
        assert_affordable(&format!("stick 8 from {held:?}"), &plan, &sources, &have);
        // And the same for a 3x3 target, which adds the table craft and its placement.
        let plan = plan_get(&sources, &have, "minecraft:wooden_pickaxe", 1).expect("plan");
        let mut pool = have.clone();
        for step in &plan.steps {
            match step {
                Step::Mine { item, count, .. } | Step::Smelt { item, count, .. } => {
                    *pool.entry(item.clone()).or_default() += count;
                }
                Step::Craft { item, count, table } => {
                    let (inputs, output) = sources
                        .get(item)
                        .iter()
                        .find_map(|source| match source {
                            Source::Craft { inputs, output, table: t } if t == table => Some((inputs.clone(), *output)),
                            _ => None,
                        })
                        .expect("recipe");
                    let crafts = count.div_ceil(output.max(1));
                    for (input, per) in &inputs {
                        let need = per * crafts;
                        let available = pool.get(input).copied().unwrap_or(0);
                        assert!(available >= need, "pickaxe from {held:?}: `{item}` needs {need} {input}, pool {available} ({})", plan.summary());
                        pool.insert(input.clone(), available - need);
                    }
                    *pool.entry(item.clone()).or_default() += count;
                }
                Step::PlaceTable { .. } => {}
            }
        }
    }
}
