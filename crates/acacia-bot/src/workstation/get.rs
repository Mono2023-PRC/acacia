//! `get <item> [count]`: the recursive plan from [`crate::workstation::sources`] run on the world.
//!
//! The planner decides *what* to do (mine a log, craft planks, craft sticks) without touching the
//! world; this module walks the plan and checks every step against the **inventory instance counts**
//! rather than trusting the primitive's return value. That check is what makes the difference between
//! "the craft call returned" and "I now hold four sticks": a server that echoes a craft it did not
//! actually do (or that ate the inputs) shows up as a step that changed nothing, and the run stops
//! with "step 2/3 changed nothing, still need 4 stick" instead of reporting success.
//!
//! Only the primitives that already exist are used: [`Bot::craft`], [`Bot::smelt`], [`Bot::break_block`],
//! [`Bot::collect_all`], [`Bot::place_block`] and the pathfinder. Nothing here opens a window by hand.

use std::fmt;

use acacia_physics::BlockPos;
use acacia_world::BlockAccess;

use super::sources::{plan_get, Plan, PlanError, Source, Sources, Step, TABLE_BLOCK};
use crate::interact::Face;
use crate::pathfind::{Goal, GotoOpts};
use crate::{ActionError, Bot};

/// How far to look for the block a plan wants to mine.
pub const SEARCH_RADIUS: i32 = 16;
/// How far to look for a crafting table or a furnace before giving up on a step.
pub const STATION_RADIUS: i32 = 12;
/// How close to stand before breaking a block (the block itself is occupied, so 2 is the tightest goal).
const MINE_REACH: f32 = 2.5;
/// How far to gather the drops of a broken block.
const PICKUP_RADIUS: f32 = 4.0;

/// Knobs for [`Bot::get`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GetOpts {
    /// Radius for the "where is that block" search.
    pub radius: i32,
    /// Radius for the "where is the table/furnace" search.
    pub station_radius: i32,
    /// Stop after this many plan steps (a runaway plan is a bug, not a reason to loop forever).
    pub max_steps: usize,
}

impl Default for GetOpts {
    fn default() -> Self {
        Self { radius: SEARCH_RADIUS, station_radius: STATION_RADIUS, max_steps: 24 }
    }
}

/// What one plan step did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRun {
    /// The step, rendered as the plan summary renders it (`mine 2 spruce_log`).
    pub step: String,
    /// The item count after the step.
    pub have_after: u32,
    /// True when the inventory changed for the better. A step that ran without moving the count is a
    /// failure, even when the primitive returned `Ok`.
    pub changed: bool,
}

/// The result of a [`Bot::get`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetReport {
    pub item: String,
    pub wanted: u32,
    pub have_before: u32,
    pub have_after: u32,
    /// `3 steps: mine 2 spruce_log, craft 4 spruce_planks, craft 4 stick`
    pub plan: String,
    pub steps: Vec<StepRun>,
}

impl GetReport {
    pub fn satisfied(&self) -> bool {
        self.have_after >= self.wanted
    }
}

impl fmt::Display for GetReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} x{} -> held {} ({})", self.item, self.wanted, self.have_after, self.plan)
    }
}

/// A plan that cannot be run, or a step that failed.
#[derive(Debug, Clone, PartialEq)]
pub enum GetError {
    /// [`plan_get`] refused: no source, a cycle, or too deep.
    Plan(PlanError),
    /// Step `index` (0-based) of `steps` failed; `missing` of `item` are still absent.
    Step { index: usize, steps: usize, step: String, reason: String, missing: u32 },
    /// The plan was longer than [`GetOpts::max_steps`].
    TooManySteps { steps: usize, max: usize },
}

impl fmt::Display for GetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GetError::Plan(e) => write!(f, "{e}"),
            GetError::Step { index, steps, step, reason, missing } => {
                write!(f, "step {}/{steps} (`{step}`) failed: {reason}; still missing {missing}", index + 1)
            }
            GetError::TooManySteps { steps, max } => write!(f, "the plan has {steps} steps, more than the limit of {max}"),
        }
    }
}

impl std::error::Error for GetError {}

impl From<PlanError> for GetError {
    fn from(e: PlanError) -> Self {
        GetError::Plan(e)
    }
}

impl From<GetError> for ActionError {
    fn from(e: GetError) -> Self {
        match e {
            GetError::Plan(PlanError::NoSource { .. }) => ActionError::NotPossible(e.to_string()),
            GetError::TooManySteps { .. } => ActionError::NotPossible(e.to_string()),
            other => ActionError::Rejected(other.to_string()),
        }
    }
}

/// Render a step the way [`Plan::summary`] does.
pub fn render(step: &Step) -> String {
    match step {
        Step::Craft { item, count, table } => format!("craft {count} {item}{}", if *table { " (table)" } else { "" }),
        Step::Mine { block, count, .. } => format!("mine {count} {block}"),
        Step::Smelt { item, count, .. } => format!("smelt {count} {item}"),
        Step::PlaceTable { item } => format!("place {item}"),
    }
}

impl Bot {
    /// How many of `item` the inventory holds, counted through the same identifiers the planner uses.
    pub fn count_item(&self, item: &str) -> u32 {
        self.state
            .inventory_summary()
            .into_iter()
            .filter(|(name, _)| name == item)
            .map(|(_, count)| count)
            .sum()
    }

    /// The source table this bot plans with: the server's crafting-table recipes plus the built-in
    /// mine/smelt tables.
    pub fn sources(&self) -> Sources {
        Sources::from_state(&self.state)
    }

    /// The plan for `count` of `item` given what is held right now. Pure: nothing runs.
    pub fn plan_get(&self, item: &str, count: u32) -> Result<Plan, PlanError> {
        let have = crate::workstation::sources::have_from(self.state.inventory_summary());
        plan_get(&self.sources(), &have, item, count)
    }

    /// Come into `count` of `item`, planning and then running the steps.
    ///
    /// Every step is followed by a recount: a step whose count did not move fails the run with the step
    /// index and what is still missing. A plan that is already satisfied by the inventory runs nothing.
    pub async fn get(&mut self, item: &str, count: u32, opts: GetOpts) -> Result<GetReport, GetError> {
        let have_before = self.count_item(item);
        let plan = self.plan_get(item, count)?;
        if plan.steps.len() > opts.max_steps {
            return Err(GetError::TooManySteps { steps: plan.steps.len(), max: opts.max_steps });
        }
        let mut report =
            GetReport { item: item.to_owned(), wanted: count, have_before, have_after: have_before, plan: plan.summary(), steps: Vec::new() };
        for (index, step) in plan.steps.iter().enumerate() {
            if self.count_item(item) >= count {
                break;
            }
            let before = self.count_item(item);
            let step_name = render(step);
            self.run_step(step, opts).await.map_err(|reason| GetError::Step {
                index,
                steps: plan.steps.len(),
                step: step_name.clone(),
                reason,
                missing: count.saturating_sub(self.count_item(item)),
            })?;
            let after = self.count_item(item);
            let changed = after > before;
            report.steps.push(StepRun { step: step_name.clone(), have_after: after, changed });
            if !changed {
                return Err(GetError::Step {
                    index,
                    steps: plan.steps.len(),
                    step: step_name,
                    reason: format!("it ran but the count stayed at {before}"),
                    missing: count.saturating_sub(after),
                });
            }
        }
        report.have_after = self.count_item(item);
        Ok(report)
    }

    /// Run one plan step, returning a human-readable reason when it failed.
    async fn run_step(&mut self, step: &Step, opts: GetOpts) -> Result<(), String> {
        match step {
            Step::Mine { block, .. } => {
                let pos = self
                    .nearest_block(block, opts.radius)
                    .ok_or_else(|| format!("no {block} within {} blocks", opts.radius))?;
                self.walk_near(pos).await.map_err(|e| format!("walking to {pos:?}: {e}"))?;
                self.break_block(pos).await.map_err(|e| format!("breaking {block} at {pos:?}: {e}"))?;
                // The drop is an item entity; collecting is best effort (a full inventory leaves it on
                // the ground, which the count check will report).
                let _ = self.collect_all(PICKUP_RADIUS).await;
                Ok(())
            }
            Step::PlaceTable { .. } => {
                let (against, face, cell) = self
                    .free_floor_cell(opts.station_radius)
                    .ok_or_else(|| format!("no free floor within {} blocks to put the table on", opts.station_radius))?;
                self.hold_one(TABLE_BLOCK).await?;
                self.walk_near(cell).await.map_err(|e| format!("walking to {cell:?}: {e}"))?;
                self.place_block(against, face).await.map_err(|e| format!("placing the table at {cell:?}: {e}"))
            }
            Step::Craft { item, count, table } => {
                let at = if *table {
                    Some(self.station(TABLE_BLOCK, opts.station_radius).ok_or_else(|| {
                        format!("`{item}` needs a 3x3 grid but no {TABLE_BLOCK} is within {} blocks", opts.station_radius)
                    })?)
                } else {
                    None
                };
                if at.is_some_and(|pos| !self.in_reach(pos)) {
                    let pos = at.expect("checked");
                    self.walk_near(pos).await.map_err(|e| format!("walking to the table at {pos:?}: {e}"))?;
                }
                self.craft(item, *count, at).await.map(|_| ()).map_err(|e| format!("crafting {count} {item}: {e}"))
            }
            Step::Smelt { item, input, count, fuel } => {
                let pos = self
                    .station("minecraft:furnace", opts.station_radius)
                    .ok_or_else(|| format!("smelting {item} needs a furnace and none is within {} blocks", opts.station_radius))?;
                if !self.in_reach(pos) {
                    self.walk_near(pos).await.map_err(|e| format!("walking to the furnace at {pos:?}: {e}"))?;
                }
                let count = u8::try_from(*count).unwrap_or(u8::MAX);
                let fuel = fuel.as_ref().map(|f| (f.as_str(), 1u8));
                self.smelt(pos, input, count, fuel).await.map(|_| ()).map_err(|e| format!("smelting {count} {item}: {e}"))
            }
        }
    }

    /// Puts `item` in the hand: selects it when it already sits in the hotbar, otherwise moves one stack
    /// into the first empty hotbar slot and selects that.
    async fn hold_one(&mut self, item: &str) -> Result<(), String> {
        let hotbar_empty = (0..crate::state::Inventory::HOTBAR_SLOTS).find(|slot| {
            crate::items::SlotRef::Main(*slot).stack(&self.state).is_none_or(|s| s.is_empty())
        });
        let mut source = None;
        for slot in 0..crate::state::Inventory::MAIN_SLOTS as u8 {
            let reference = crate::items::SlotRef::Main(slot);
            if reference.stack(&self.state).and_then(|s| self.state.item_name(s)) == Some(item) {
                source = Some(slot);
                break;
            }
        }
        let slot = source.ok_or_else(|| format!("{item} is not in the inventory"))?;
        if slot < crate::state::Inventory::HOTBAR_SLOTS {
            return self.select_hotbar(slot).map_err(|e| e.to_string());
        }
        let empty = hotbar_empty.ok_or_else(|| "the hotbar has no empty slot for the item".to_owned())?;
        self.move_item(crate::items::SlotRef::Main(slot), crate::items::SlotRef::Main(empty), 1)
            .await
            .map_err(|e| format!("moving {item} to the hotbar: {e}"))?;
        self.select_hotbar(empty).map_err(|e| e.to_string())
    }

    /// Walks to touching distance of `pos`, if physics is on.
    async fn walk_near(&mut self, pos: BlockPos) -> Result<(), ActionError> {
        if !self.has_physics() || self.in_reach(pos) {
            return Ok(());
        }
        let opts = GotoOpts { tolerance: 0.3, ..GotoOpts::default() };
        self.goto_with(Goal::Near(pos, MINE_REACH), opts).await.map(|_| ())
    }

    /// Whether the eye is close enough to use the block at `pos`.
    fn in_reach(&self, pos: BlockPos) -> bool {
        crate::interact::block_distance(self.eye_position(), pos) <= crate::interact::BLOCK_REACH
    }

    /// The nearest block named `name` within `radius` of the feet.
    fn nearest_block(&self, name: &str, radius: i32) -> Option<BlockPos> {
        let feet = self.block_position()?;
        let world = self.world.as_ref()?;
        let (view, registry) = (world.view()?, world.registry()?);
        let mut best: Option<(i32, BlockPos)> = None;
        for x in feet[0] - radius..=feet[0] + radius {
            for y in (feet[1] - radius).max(-64)..=(feet[1] + radius).min(320) {
                for z in feet[2] - radius..=feet[2] + radius {
                    let Some(state) = registry.get(view.block(x, y, z)) else { continue };
                    if state.name != name {
                        continue;
                    }
                    let d = (x - feet[0]).pow(2) + (y - feet[1]).pow(2) + (z - feet[2]).pow(2);
                    if best.is_none_or(|(bd, _)| d < bd) {
                        best = Some((d, [x, y, z]));
                    }
                }
            }
        }
        best.map(|(_, pos)| pos)
    }

    /// The nearest block of `name` a station step can use, with its position.
    fn station(&self, name: &str, radius: i32) -> Option<BlockPos> {
        self.nearest_block(name, radius)
    }

    /// A free cell standing on something solid, with the block and face to click for a placement, within
    /// `radius` of the feet and outside the bot's own two cells.
    fn free_floor_cell(&self, radius: i32) -> Option<(BlockPos, Face, BlockPos)> {
        let feet = self.block_position()?;
        let world = self.world.as_ref()?;
        let (view, registry) = (world.view()?, world.registry()?);
        let name_at = |p: [i32; 3]| -> Option<&'static str> {
            registry.get(view.block(p[0], p[1], p[2])).map(|s| s.name)
        };
        let mut best: Option<(i32, BlockPos, Face, BlockPos)> = None;
        for x in feet[0] - radius..=feet[0] + radius {
            for z in feet[2] - radius..=feet[2] + radius {
                for y in (feet[1] - 1)..=(feet[1] + 1) {
                    let cell = [x, y, z];
                    if cell == feet || (cell[0] == feet[0] && cell[2] == feet[2] && cell[1] == feet[1] + 1) {
                        continue;
                    }
                    let Some(here) = name_at(cell) else { continue };
                    if here != "minecraft:air" && !here.ends_with("_air") {
                        continue;
                    }
                    let below = [x, y - 1, z];
                    let Some(floor) = name_at(below) else { continue };
                    if floor == "minecraft:air" || floor.ends_with("_air") {
                        continue;
                    }
                    let d = (x - feet[0]).pow(2) + (y - feet[1]).pow(2) + (z - feet[2]).pow(2);
                    if best.is_none_or(|(bd, ..)| d < bd) {
                        best = Some((d, below, Face::Up, cell));
                    }
                }
            }
        }
        best.map(|(_, against, face, cell)| (against, face, cell))
    }

    /// Every source the planner has for `item`, for diagnostics and tests.
    pub fn item_sources(&self, item: &str) -> Vec<Source> {
        self.sources().get(item).to_vec()
    }
}
