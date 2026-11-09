//! Where an item can come from, and the recursive planner that turns "get me 8 sticks" into an ordered
//! list of digs, crafts and smelts.
//!
//! The planner is **pure**: it never touches the world. It works on a [`Sources`] table (crafts from
//! `CraftingData`, plus the built-in mine and smelt tables below) and a *simulated* inventory, and it
//! keeps that inventory up to date as it emits steps. That is what makes the difference between
//! "eight sticks" and "eight logs": a craft of planks adds four planks to the simulated pool, and the
//! stick step then finds them there instead of planning another log.
//!
//! Two lessons from the earlier `craft` work are encoded here:
//!
//! * t56 — a 3×3 recipe is **not** silently squeezed into the player's 2×2 grid. When a step needs a
//!   table, the plan says so (`Step::Craft { table: true }`) and, when the player has no table yet, it
//!   begins with "make one, then place it".
//! * t69 — a plan that cannot be satisfied says *why* (`NoSource`, `Cycle`, `TooDeep`) instead of
//!   quietly returning fewer steps, so the caller can report "no source for X" rather than "done".

use std::collections::BTreeMap;
use std::fmt;

use crate::state::{GameState, Ingredient};

/// How many items deep a plan may recurse (`stick → planks → log` is depth 3) before the planner gives
/// up. A genuine survival chain is short; anything longer is almost always a recipe cycle.
pub const DEPTH_LIMIT: usize = 8;

/// The recipe *stations* the planner knows. Only `crafting_table` recipes can be planned: furnace
/// smelting is planned from the table below, and the other stations (stonecutter, smithing) are out of
/// scope for `get`.
pub const TABLE_BLOCK: &str = "minecraft:crafting_table";

/// Blocks that yield an item when broken. `(item, block, tool)` — the tool is a hint for the caller
/// (the plan does not equip anything). Kept deliberately short: every entry is a block whose drop is
/// the item itself, which is the only case `Bot::get` can walk to and break.
pub const MINES: &[(&str, &str, Option<&str>)] = &[
    ("minecraft:oak_log", "minecraft:oak_log", Some("minecraft:wooden_axe")),
    ("minecraft:spruce_log", "minecraft:spruce_log", Some("minecraft:wooden_axe")),
    ("minecraft:birch_log", "minecraft:birch_log", Some("minecraft:wooden_axe")),
    ("minecraft:jungle_log", "minecraft:jungle_log", Some("minecraft:wooden_axe")),
    ("minecraft:acacia_log", "minecraft:acacia_log", Some("minecraft:wooden_axe")),
    ("minecraft:dark_oak_log", "minecraft:dark_oak_log", Some("minecraft:wooden_axe")),
    ("minecraft:mangrove_log", "minecraft:mangrove_log", Some("minecraft:wooden_axe")),
    ("minecraft:cherry_log", "minecraft:cherry_log", Some("minecraft:wooden_axe")),
    ("minecraft:cobblestone", "minecraft:stone", Some("minecraft:wooden_pickaxe")),
    ("minecraft:cobbled_deepslate", "minecraft:deepslate", Some("minecraft:wooden_pickaxe")),
    ("minecraft:coal", "minecraft:coal_ore", Some("minecraft:wooden_pickaxe")),
    ("minecraft:raw_iron", "minecraft:iron_ore", Some("minecraft:stone_pickaxe")),
    ("minecraft:raw_copper", "minecraft:copper_ore", Some("minecraft:stone_pickaxe")),
    ("minecraft:raw_gold", "minecraft:gold_ore", Some("minecraft:iron_pickaxe")),
    ("minecraft:diamond", "minecraft:diamond_ore", Some("minecraft:iron_pickaxe")),
    ("minecraft:redstone", "minecraft:redstone_ore", Some("minecraft:iron_pickaxe")),
    ("minecraft:lapis_lazuli", "minecraft:lapis_ore", Some("minecraft:stone_pickaxe")),
    ("minecraft:emerald", "minecraft:emerald_ore", Some("minecraft:iron_pickaxe")),
    ("minecraft:sand", "minecraft:sand", None),
    ("minecraft:gravel", "minecraft:gravel", None),
    ("minecraft:dirt", "minecraft:dirt", None),
    ("minecraft:clay_ball", "minecraft:clay", None),
    ("minecraft:snowball", "minecraft:snow_block", Some("minecraft:wooden_shovel")),
    ("minecraft:sugar_cane", "minecraft:sugar_cane", None),
    ("minecraft:cactus", "minecraft:cactus", None),
    ("minecraft:kelp", "minecraft:kelp", None),
    ("minecraft:obsidian", "minecraft:obsidian", Some("minecraft:diamond_pickaxe")),
];

/// `(product, input, fuel)` for a furnace. One fuel item smelts several inputs, but the planner asks
/// for a single fuel item per smelt step — a conservative over-ask is harmless, an under-ask stalls.
pub const SMELTS: &[(&str, &str, Option<&str>)] = &[
    ("minecraft:charcoal", "minecraft:oak_log", Some("minecraft:oak_planks")),
    ("minecraft:iron_ingot", "minecraft:raw_iron", Some("minecraft:coal")),
    ("minecraft:copper_ingot", "minecraft:raw_copper", Some("minecraft:coal")),
    ("minecraft:gold_ingot", "minecraft:raw_gold", Some("minecraft:coal")),
    ("minecraft:glass", "minecraft:sand", Some("minecraft:coal")),
    ("minecraft:stone", "minecraft:cobblestone", Some("minecraft:coal")),
    ("minecraft:cooked_porkchop", "minecraft:porkchop", Some("minecraft:coal")),
    ("minecraft:cooked_beef", "minecraft:beef", Some("minecraft:coal")),
    ("minecraft:cooked_chicken", "minecraft:chicken", Some("minecraft:coal")),
    ("minecraft:baked_potato", "minecraft:potato", Some("minecraft:coal")),
];

/// One way to obtain an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A crafting-table recipe. `table` is true for the recipes that do not fit the player's own 2×2
    /// grid (t56: those must open a table, never be squeezed into four slots).
    Craft { inputs: Vec<(String, u32)>, output: u32, table: bool },
    /// Breaking a block yields the item.
    Mine { block: String, tool: Option<String> },
    /// A furnace turns `input` into the item.
    Smelt { input: String, fuel: Option<String> },
    /// Not implemented yet: a mob drop (wool, meat) and a villager offer. They exist so the planner's
    /// decision tree and its reasons are already shaped for them.
    Harvest { entity: String },
    Trade { item: String },
}

/// Item → the ways to make it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sources {
    map: BTreeMap<String, Vec<Source>>,
}

impl Sources {
    /// The built-in mine/smelt tables and nothing else: no crafts, which is what the tests of the
    /// miner/smelter alone want.
    pub fn builtin() -> Self {
        let mut sources = Self::default();
        for (item, block, tool) in MINES {
            sources.add(item, Source::Mine { block: (*block).to_owned(), tool: tool.map(str::to_owned) });
        }
        for (item, input, fuel) in SMELTS {
            sources.add(item, Source::Smelt { input: (*input).to_owned(), fuel: fuel.map(str::to_owned) });
        }
        sources
    }

    /// The built-in tables plus every crafting-table recipe the server sent, with item names resolved
    /// through the registry.
    ///
    /// Recipes whose inputs are tags or MoLang expressions are skipped: the planner cannot know which
    /// concrete item a tag means without the tag table, and guessing "any planks" would produce a plan
    /// that fails at the crafting table.
    pub fn from_state(state: &GameState) -> Self {
        let mut sources = Self::builtin();
        for recipe in state.recipes.book().iter() {
            if recipe.block != "minecraft:crafting_table" {
                continue;
            }
            let table = !recipe.fits_2x2();
            let mut inputs = Vec::new();
            let mut plannable = true;
            for ingredient in &recipe.inputs {
                match ingredient {
                    Ingredient::Empty => {}
                    Ingredient::Item { name, count, .. } => {
                        if *count == 0 {
                            continue;
                        }
                        inputs.push((name.clone(), u32::from(*count)));
                    }
                    Ingredient::Tag { .. } | Ingredient::Molang { .. } => {
                        plannable = false;
                        break;
                    }
                }
            }
            if !plannable || inputs.is_empty() {
                continue;
            }
            for output in &recipe.outputs {
                let Some(name) = state.item_name(output) else { continue };
                let count = u32::from(output.count).max(1);
                sources.add(name, Source::Craft { inputs: inputs.clone(), output: count, table });
            }
        }
        sources
    }

    pub fn add(&mut self, item: &str, source: Source) {
        self.map.entry(item.to_owned()).or_default().push(source);
    }

    pub fn get(&self, item: &str) -> &[Source] {
        self.map.get(item).map_or(&[], Vec::as_slice)
    }

    pub fn contains(&self, item: &str) -> bool {
        self.map.contains_key(item)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// One ordered action of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Craft `count` of `item` (the recipe's own output per craft, so more than the plan needed).
    Craft { item: String, count: u32, table: bool },
    /// Break `block` `count` times to get `count` of `item`.
    Mine { item: String, block: String, count: u32, tool: Option<String> },
    /// Smelt `count` of `item` from `input`, optionally needing `fuel`.
    Smelt { item: String, input: String, count: u32, fuel: Option<String> },
    /// Put a crafting table down, because a later step needs a 3×3 grid.
    PlaceTable { item: String },
}

impl Step {
    /// The item this step is *about*, for messages.
    pub fn item(&self) -> &str {
        match self {
            Step::Craft { item, .. } | Step::Mine { item, .. } | Step::Smelt { item, .. } | Step::PlaceTable { item } => item,
        }
    }
}

/// A plan: steps in the order they must run (inputs before the craft that consumes them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// What the simulated inventory holds once every step ran.
    pub have_after: BTreeMap<String, u32>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub fn needs_table(&self) -> bool {
        self.steps.iter().any(|step| matches!(step, Step::Craft { table: true, .. } | Step::PlaceTable { .. }))
    }

    /// `3 steps: mine oak_log, craft oak_planks, craft stick` — for logs and messages.
    pub fn summary(&self) -> String {
        let parts: Vec<String> = self
            .steps
            .iter()
            .map(|step| match step {
                Step::Craft { item, count, table } => {
                    format!("craft {count} {item}{}", if *table { " (table)" } else { "" })
                }
                Step::Mine { block, count, .. } => format!("mine {count} {block}"),
                Step::Smelt { item, count, .. } => format!("smelt {count} {item}"),
                Step::PlaceTable { item } => format!("place {item}"),
            })
            .collect();
        format!("{} steps: {}", parts.len(), parts.join(", "))
    }
}

/// Why a plan could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// Nothing in [`Sources`] makes this item, and the inventory does not hold enough of it.
    NoSource { item: String },
    /// The recipe chain leads back to an item that is already being planned: `A ← B ← A`.
    Cycle { item: String },
    /// The chain is longer than [`DEPTH_LIMIT`].
    TooDeep { item: String },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::NoSource { item } => write!(f, "no source for {item}"),
            PlanError::Cycle { item } => write!(f, "{item} is part of a recipe cycle"),
            PlanError::TooDeep { item } => write!(f, "the chain to {item} is deeper than {DEPTH_LIMIT}"),
        }
    }
}

impl std::error::Error for PlanError {}

/// Count how many of `item` an inventory holds: `(identifier, count)` pairs as `inventory_summary`
/// reports them, so the planner and the bot count the same way.
pub fn have_from<I, S>(pairs: I) -> BTreeMap<String, u32>
where
    I: IntoIterator<Item = (S, u32)>,
    S: Into<String>,
{
    let mut have: BTreeMap<String, u32> = BTreeMap::new();
    for (name, count) in pairs {
        *have.entry(name.into()).or_default() += count;
    }
    have
}

/// Plan how to come into `count` of `item`, given what the inventory already holds.
///
/// The returned steps are in dependency order; running them in that order works because every step
/// leaves its product in the simulated inventory before the next step is planned.
pub fn plan_get(
    sources: &Sources,
    have: &BTreeMap<String, u32>,
    item: &str,
    count: u32,
) -> Result<Plan, PlanError> {
    if count == 0 {
        return Ok(Plan { steps: Vec::new(), have_after: have.clone() });
    }
    let mut planner = Planner { sources, pool: have.clone(), steps: Vec::new(), stack: Vec::new(), table_ready: false };
    // A 3×3 recipe anywhere in the chain means a table is needed *before* the first such craft. Detect
    // it with a throw-away planner so the table steps land at the front of the real plan.
    let probe = planner.clone();
    let needs_table = probe.build(item, count).map(|plan| plan.needs_table()).unwrap_or(false);
    if needs_table && !planner.has_table() {
        planner.ensure_table()?;
    }
    planner.build(item, count)
}

/// Whether every step of `steps` can be paid for in order (inputs available when the craft runs). The
/// coalescing pass below only accepts a merged plan that still passes this.
fn affordable(sources: &Sources, have: &BTreeMap<String, u32>, steps: &[Step]) -> bool {
    let mut pool = have.clone();
    for step in steps {
        match step {
            Step::Mine { item, count, .. } | Step::Smelt { item, count, .. } => {
                *pool.entry(item.clone()).or_default() += count;
            }
            Step::Craft { item, count, table } => {
                let Some((inputs, output)) = sources.get(item).iter().find_map(|source| match source {
                    Source::Craft { inputs, output, table: t } if t == table => Some((inputs, *output)),
                    _ => None,
                }) else {
                    return false;
                };
                let crafts = count.div_ceil(output.max(1));
                for (input, per) in inputs {
                    let need = per.saturating_mul(crafts);
                    let held = pool.entry(input.clone()).or_default();
                    if *held < need {
                        return false;
                    }
                    *held -= need;
                }
                *pool.entry(item.clone()).or_default() += count;
            }
            Step::PlaceTable { item } => {
                let held = pool.entry(item.clone()).or_default();
                if *held == 0 {
                    return false;
                }
                *held -= 1;
            }
        }
    }
    true
}

/// Merge repeated `Craft` steps for the same item (and repeated `Mine`/`Smelt` for the same item) into
/// one, placed at the earliest of them, when that keeps the plan affordable.
///
/// The greedy planner plans each shortage as it meets it, so "planks for the table, planks for the
/// pickaxe, planks for the sticks" comes out as three small crafts. One bigger craft at the front is
/// the same resources with fewer steps — but only when the inputs are available that early, which is
/// exactly what [`affordable`] checks before the merge is accepted.
fn coalesce(sources: &Sources, have: &BTreeMap<String, u32>, steps: Vec<Step>) -> Vec<Step> {
    let mut merged = steps.clone();
    loop {
        let mut merged_something = false;
        'outer: for i in 0..merged.len() {
            for j in (i + 1)..merged.len() {
                let combined = match (&merged[i], &merged[j]) {
                    (
                        Step::Craft { item, count, table },
                        Step::Craft { item: other, count: other_count, table: other_table },
                    ) if item == other && table == other_table => Step::Craft {
                        item: item.clone(),
                        count: count.saturating_add(*other_count),
                        table: *table,
                    },
                    (Step::Mine { item, block, count, tool }, Step::Mine { item: o, block: ob, count: oc, tool: ot })
                        if item == o && block == ob && tool == ot =>
                    {
                        Step::Mine { item: item.clone(), block: block.clone(), count: count + oc, tool: tool.clone() }
                    }
                    (
                        Step::Smelt { item, input, count, fuel },
                        Step::Smelt { item: o, input: oi, count: oc, fuel: of },
                    ) if item == o && input == oi && fuel == of => Step::Smelt {
                        item: item.clone(),
                        input: input.clone(),
                        count: count + oc,
                        fuel: fuel.clone(),
                    },
                    _ => continue,
                };
                let mut candidate = merged.clone();
                candidate[i] = combined;
                candidate.remove(j);
                if affordable(sources, have, &candidate) {
                    merged = candidate;
                    merged_something = true;
                    break 'outer;
                }
            }
        }
        if !merged_something {
            return merged;
        }
    }
}

/// Walk the steps against the source table and return what the inventory holds afterwards: start from
/// `have`, add every step's product, subtract every craft's inputs. This is also the planner's own
/// self-check — `Bot::get` recounts the real inventory anyway.
pub fn replay(sources: &Sources, have: &BTreeMap<String, u32>, steps: &[Step]) -> BTreeMap<String, u32> {
    let mut pool = have.clone();
    for step in steps {
        match step {
            Step::Mine { item, count, .. } | Step::Smelt { item, count, .. } => {
                *pool.entry(item.clone()).or_default() += count;
            }
            Step::Craft { item, count, table } => {
                let recipe = sources.get(item).iter().find_map(|source| match source {
                    Source::Craft { inputs, output, table: t } if t == table => Some((inputs, *output)),
                    _ => None,
                });
                if let Some((inputs, output)) = recipe {
                    let crafts = count.div_ceil(output.max(1));
                    for (input, per) in inputs {
                        let gone = per.saturating_mul(crafts);
                        let held = pool.entry(input.clone()).or_default();
                        *held = held.saturating_sub(gone);
                    }
                }
                *pool.entry(item.clone()).or_default() += count;
            }
            Step::PlaceTable { item } => {
                let held = pool.entry(item.clone()).or_default();
                *held = held.saturating_sub(1);
            }
        }
    }
    pool
}

struct Planner<'a> {
    sources: &'a Sources,
    pool: BTreeMap<String, u32>,
    steps: Vec<Step>,
    stack: Vec<String>,
    table_ready: bool,
}

impl Clone for Planner<'_> {
    fn clone(&self) -> Self {
        Self {
            sources: self.sources,
            pool: self.pool.clone(),
            steps: self.steps.clone(),
            stack: self.stack.clone(),
            table_ready: self.table_ready,
        }
    }
}

impl Planner<'_> {
    fn has_table(&self) -> bool {
        self.table_ready || self.available(TABLE_BLOCK) > 0
    }

    fn available(&self, item: &str) -> u32 {
        self.pool.get(item).copied().unwrap_or(0)
    }

    fn take(&mut self, item: &str, count: u32) -> u32 {
        let held = self.pool.entry(item.to_owned()).or_default();
        let taken = (*held).min(count);
        *held -= taken;
        taken
    }

    fn build(mut self, item: &str, count: u32) -> Result<Plan, PlanError> {
        let have = self.pool.clone();
        self.ensure(item, count)?;
        let steps = coalesce(self.sources, &have, self.steps);
        let have_after = replay(self.sources, &have, &steps);
        Ok(Plan { steps, have_after })
    }

    /// Plan for `need` more of `item`. On return the plan delivers them, and they have been taken out of
    /// the simulated pool: a caller that wants them for a craft has its inputs committed, so a sibling
    /// step cannot spend them again. Anything a recipe made *beyond* the need stays in the pool as real
    /// surplus (one log makes four planks; three spare planks can serve the next recipe).
    fn ensure(&mut self, item: &str, need: u32) -> Result<(), PlanError> {
        if need == 0 {
            return Ok(());
        }
        let missing = need - self.take(item, need);
        if missing == 0 {
            return Ok(());
        }
        if self.stack.iter().any(|name| name == item) {
            return Err(PlanError::Cycle { item: item.to_owned() });
        }
        if self.stack.len() >= DEPTH_LIMIT {
            return Err(PlanError::TooDeep { item: item.to_owned() });
        }
        let sources = self.sources.get(item);
        // A craft is preferred over a mine/smelt: it is what makes "eight sticks" cost two planks and
        // one log instead of eight logs. Among crafts the smallest input list wins.
        let craft = sources
            .iter()
            .filter_map(|source| match source {
                Source::Craft { inputs, output, table } => Some((inputs, *output, *table)),
                _ => None,
            })
            .min_by_key(|(inputs, output, _)| (inputs.len(), std::cmp::Reverse(*output)));
        if let Some((inputs, output, table)) = craft {
            let crafts = missing.div_ceil(output.max(1));
            self.stack.push(item.to_owned());
            for (input, per_craft) in inputs {
                self.ensure(input, per_craft.saturating_mul(crafts))?;
            }
            self.stack.pop();
            let produced = crafts.saturating_mul(output.max(1));
            self.steps.push(Step::Craft { item: item.to_owned(), count: produced, table });
            // What was needed went to the caller; only the true surplus returns to the pool.
            if produced > missing {
                *self.pool.entry(item.to_owned()).or_default() += produced - missing;
            }
            return Ok(());
        }
        if let Some(Source::Smelt { input, fuel }) = sources.iter().find(|s| matches!(s, Source::Smelt { .. })) {
            let input = input.clone();
            let fuel = fuel.clone();
            self.stack.push(item.to_owned());
            self.ensure(&input, missing)?;
            if let Some(fuel) = fuel.clone() {
                // One fuel item per smelt step; the furnace burns it for several inputs anyway.
                self.ensure(&fuel, 1)?;
            }
            self.stack.pop();
            self.steps.push(Step::Smelt { item: item.to_owned(), input, count: missing, fuel });
            return Ok(());
        }
        if let Some(Source::Mine { block, tool }) = sources.iter().find(|s| matches!(s, Source::Mine { .. })) {
            self.steps.push(Step::Mine {
                item: item.to_owned(),
                block: block.clone(),
                count: missing,
                tool: tool.clone(),
            });
            return Ok(());
        }
        Err(PlanError::NoSource { item: item.to_owned() })
    }

    /// Make a table and put it down. Runs only when a 3×3 step is in the plan and no table is held.
    fn ensure_table(&mut self) -> Result<(), PlanError> {
        self.ensure(TABLE_BLOCK, 1)?;
        self.steps.push(Step::PlaceTable { item: TABLE_BLOCK.to_owned() });
        self.table_ready = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_does_not_leave_the_simulated_inventory_dirty() {
        let mut sources = Sources::default();
        sources.add("minecraft:stick", Source::Craft { inputs: vec![("minecraft:oak_planks".into(), 2)], output: 4, table: false });
        sources.add("minecraft:oak_planks", Source::Craft { inputs: vec![("minecraft:oak_log".into(), 1)], output: 4, table: false });
        let have = have_from([("minecraft:oak_log", 1u32)]);
        let plan = plan_get(&sources, &have, "minecraft:stick", 4).expect("plan");
        assert_eq!(plan.steps.len(), 2, "{}", plan.summary());
        // The table probe cloned the planner; the real one still sees the log it started with.
        assert_eq!(plan.have_after.get("minecraft:oak_log"), Some(&0), "{}", plan.summary());
        assert_eq!(plan.have_after.get("minecraft:stick"), Some(&4), "{}", plan.summary());
        // One log makes four planks and two of them are spare: they stay in the pool, they are not invented
        // twice by a second craft.
        assert_eq!(plan.have_after.get("minecraft:oak_planks"), Some(&2), "{}", plan.summary());
    }

    #[test]
    fn a_table_is_inserted_before_the_three_by_three_step() {
        let mut sources = Sources::default();
        sources.add(TABLE_BLOCK, Source::Craft { inputs: vec![("minecraft:oak_planks".into(), 4)], output: 1, table: false });
        sources.add("minecraft:oak_planks", Source::Mine { block: "minecraft:oak_log".into(), tool: None });
        sources.add(
            "minecraft:wooden_pickaxe",
            Source::Craft {
                inputs: vec![("minecraft:oak_planks".into(), 3), ("minecraft:stick".into(), 2)],
                output: 1,
                table: true,
            },
        );
        sources.add("minecraft:stick", Source::Craft { inputs: vec![("minecraft:oak_planks".into(), 2)], output: 4, table: false });
        let plan = plan_get(&sources, &BTreeMap::new(), "minecraft:wooden_pickaxe", 1).expect("plan");
        let place = plan
            .steps
            .iter()
            .position(|step| matches!(step, Step::PlaceTable { .. }))
            .expect("table placed");
        let pick = plan
            .steps
            .iter()
            .position(|step| matches!(step, Step::Craft { item, table: true, .. } if item == "minecraft:wooden_pickaxe"))
            .expect("pickaxe crafted");
        assert!(place < pick, "{}", plan.summary());
    }
}
