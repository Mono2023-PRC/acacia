//! Server-authoritative block breaking through `PlayerAuthInput` block actions (Geyser
//! `BlockBreakHandler`, Boar `ServerBreakBlockValidator`): `StartBreak`, `ContinueBreak` every tick,
//! then `ContinueBreak` + `PredictBreak` + `AbortBreak` on the tick the vanilla break time runs out
//! ([`wire::break_step`]). Geyser counts one tick of progress for each of Start, Continue and Predict,
//! so the prediction lands exactly at full progress.

use std::time::Instant;

use acacia_client::proto::nbt::Value;
use acacia_client::proto::types::Action;
use acacia_physics::{BlockPos, Vec3};
use acacia_world::{Mining, Tool};

use super::break_time::{break_ticks, BreakConditions, Progress};
use super::confirm::{Confirm, Expect, Verdict};
use super::geometry::{block_box_distance, block_distance, facing_face, BLOCK_REACH};
use crate::pathfind::GotoOpts;
use super::wire::{self, SwingSource};
use super::Face;
use crate::cancel::Held;
use crate::state::{effect, ItemStack};
use crate::{ActionError, Bot};

/// Tries of a break the server shows no change for.
const MAX_TRIES: u32 = 3;
/// How long a break the server shows no change for keeps being watched after its tries ran out, in ticks (6 s):
/// the server's cell update trails the governor's round-trip estimate by up to seconds on NetEase, and the drop
/// it spawns tells the same story a little later.
const SETTLE_TICKS: u64 = 120;
/// A dropped item this close to the broken cell is that block's drop.
const DROP_RADIUS: f32 = 3.0;
/// Ticks to wait for the fall after breaking the block under the feet (a 4-block drop takes ~18).
const LAND_TICKS: u32 = 40;
/// Vanilla keeps swinging while mining; one swing lasts 6 ticks.
const SWING_EVERY: u32 = 5;
/// Bedrock enchantment ids in the item NBT `ench` list.
pub(crate) const EFFICIENCY: i16 = 15;
const AQUA_AFFINITY: i16 = 8;

impl Bot {
    /// Breaks the block at `pos` with the held item, taking the vanilla break time (see
    /// [`break_ticks`]), and waits until the server shows air there, mining again (up to 3 tries)
    /// while it shows the block unchanged past the governor's timeout. Physics bots only; the bot
    /// should stand still while mining. A cancelled or dropped break is aborted (see [`crate::cancel`]).
    pub async fn break_block(&mut self, pos: BlockPos) -> Result<(), ActionError> {
        if !self.has_physics() {
            return Err(ActionError::NotPossible("block breaking needs a physics bot".into()));
        }
        // A dig aimed from a body the server has elsewhere is refused there (t37): the drift, when there is
        // one, explains such a refusal (the note is used below), but it does not block the dig by itself.
        let drift = self.desync_refusal();
        let (_, state) = self.block_at(pos).ok_or_else(|| ActionError::NotPossible(format!("{pos:?} is not loaded")))?;
        if state.is_air() || state.is_liquid() {
            return Err(ActionError::NotPossible(format!("nothing to break at {pos:?}")));
        }
        let (name, mining) = (state.name, state.mining);
        break_ticks(&mining, &self.break_conditions()).ok_or_else(|| ActionError::NotPossible(format!("{name} cannot be broken")))?;
        let eye = self.eye_position();
        let distance = block_distance(eye, pos);
        // Reach is measured to what the block actually occupies: the cell's box, and — because a stale eye
        // after a fall once refused the block under the feet — any cell touching the bot's own body.
        let nearest = block_box_distance(eye, pos);
        if distance > BLOCK_REACH && nearest > BLOCK_REACH && !self.touches_body(pos) {
            return Err(ActionError::NotPossible(format!(
                "{name} at {pos:?} is {distance:.2} blocks away (nearest face {nearest:.2})"
            )));
        }
        let (face, aim) = self.visible_aim(pos, None)?;
        let slot = self.admit_action().await?;
        let breaking = self.claim(Held::Break(pos));
        // Breaking the cell the bot stands on drops it: let the fall finish before the caller measures
        // reach from the eye again (mid-air positions make the server refuse the next dig).
        let supporting = self.stands_on(pos);
        let before = self.feet_y();
        let result = self.break_confirmed(pos, face, aim, mining).await;
        self.unclaim(breaking);
        self.release(slot);
        if result.is_ok() && supporting {
            self.wait_landing(before, LAND_TICKS).await?;
        }
        result.map_err(|e| match e {
            ActionError::Rejected(why) => ActionError::Rejected(format!("breaking {name} at {pos:?}: {why}")),
            // Only a refusal the bot could not attribute is blamed on the drift: a break that says "nothing to
            // break" was not the server's doing.
            ActionError::NotPossible(why) if drift.is_some() => {
                ActionError::NotPossible(format!("{}; {}", drift.unwrap_or_default(), why))
            }
            e => e,
        })
    }

    /// Mines, then waits for the server's echo; re-mines while the server shows the block unchanged.
    async fn break_confirmed(&mut self, pos: BlockPos, face: Face, aim: Vec3, mining: Mining) -> Result<(), ActionError> {
        let t = Instant::now();
        self.look_at(aim).await?;
        self.act_timing.add(crate::act_timing::Phase::Aim, t.elapsed());
        let mut confirm: Option<Confirm> = None;
        loop {
            let t = Instant::now();
            let gone = self.mine(pos, face, mining).await?;
            self.act_timing.add(crate::act_timing::Phase::Mine, t.elapsed());
            if gone {
                return Ok(());
            }
            let sent_at = Instant::now();
            let c = match &mut confirm {
                Some(c) => {
                    c.retried(self.governor.now(), self.governor.timeout_ticks());
                    self.governor.sent(true);
                    c
                }
                None => confirm.insert(self.confirmation(pos, Expect::Gone, MAX_TRIES)),
            };
            match self.await_confirm(pos, c, sent_at).await? {
                Verdict::Done => return Ok(()),
                Verdict::Retry => self.abort_break(pos),
                Verdict::Failed(_) => {
                    // The server can break the block and send the cell update seconds later than the governor
                    // expects (relay run115: the drop appeared and was picked up while the client had already
                    // given up). Watch the cell for a while longer, counting the drop or an inventory gain as
                    // the same event, before calling the break refused.
                    let total = inventory_total(self);
                    let also_done = |bot: &Bot| inventory_total(bot) > total || bot.drop_near(pos, DROP_RADIUS);
                    let Some(failure) = self.settle_confirm(pos, c, SETTLE_TICKS, also_done).await? else {
                        return Ok(());
                    };
                    self.abort_break(pos);
                    return Err(ActionError::Rejected(format!(
                        "{failure}; last seen {}; {}",
                        cell_name(self, pos),
                        drop_report(self, pos)
                    )));
                }
                Verdict::Wait => unreachable!("await_confirm decides"),
            }
        }
    }

    /// Cancels breaking the block at `pos` (`AbortBreak` with the next tick).
    pub fn abort_break(&mut self, pos: BlockPos) {
        let face = facing_face(self.eye_position(), pos);
        if let Some(m) = self.movement.as_mut() {
            m.pending_actions.push(wire::block_action(Action::AbortBreak, pos, face));
        }
    }

    /// Ends a break that finished early (`AbortBreak` with the next tick; see [`wire::break_step`]).
    pub(crate) fn end_break(&mut self, pos: BlockPos, face: Face) {
        if let Some(m) = self.movement.as_mut() {
            m.pending_actions.push(wire::block_action(Action::AbortBreak, pos, face));
        }
    }

    /// Sends the block actions until the progress, summed at each tick's break speed, is full. `Ok(true)` if
    /// the block disappeared on its own before the prediction was due.
    async fn mine(&mut self, pos: BlockPos, face: Face, mining: Mining) -> Result<bool, ActionError> {
        let mut progress = Progress::default();
        loop {
            let tick = progress.ticks();
            let ticks_now = break_ticks(&mining, &self.break_conditions()).unwrap_or(u32::MAX);
            let last = progress.step(ticks_now);
            if let Some(m) = self.movement.as_mut() {
                m.pending_actions.extend(wire::break_step(tick == 0, last, pos, face, false));
            }
            if tick % SWING_EVERY == 0 {
                self.swing_from(Some(SwingSource::Mine));
            }
            self.next_tick(|_, _| false).await?;
            if last {
                return Ok(false);
            }
            if self.is_air(pos) {
                self.end_break(pos, face);
                return Ok(true);
            }
        }
    }

    fn is_air(&self, pos: BlockPos) -> bool {
        self.block_at(pos).is_some_and(|(_, s)| s.is_air())
    }

    /// Break-time inputs from the tracked state, this tick.
    pub fn break_conditions(&self) -> BreakConditions {
        let inv = &self.state.inventory;
        let held = inv.held();
        let eye = self.eye_position();
        let eye_block = [eye[0].floor() as i32, eye[1].floor() as i32, eye[2].floor() as i32];
        let helmet = inv.armor.first().cloned().unwrap_or_default();
        let effects = &self.state.player.effects;
        let level = |id| u8::try_from(effects.level(id)).unwrap_or(u8::MAX);
        BreakConditions {
            tool: self.state.items.name(held.network_id).and_then(Tool::from_identifier),
            efficiency: enchantment_level(held, EFFICIENCY),
            haste: level(effect::HASTE).max(level(effect::CONDUIT_POWER)),
            mining_fatigue: level(effect::MINING_FATIGUE),
            underwater: self.block_at(eye_block).is_some_and(|(_, s)| s.is_water()) && enchantment_level(&helmet, AQUA_AFFINITY) == 0,
            // Non-physics bots can't break blocks; true keeps the formula's default.
            on_ground: self.movement.as_ref().is_none_or(|m| m.on_ground()),
        }
    }
}

/// Level of enchantment `id` in the item's NBT `ench` list, 0 if absent.
pub(crate) fn enchantment_level(item: &ItemStack, id: i16) -> u8 {
    let Some(Value::List(list)) = item.nbt.as_ref().and_then(|n| n.value.get("ench")) else { return 0 };
    list.items
        .iter()
        .find(|e| matches!(e.get("id"), Some(Value::Short(i)) if *i == id))
        .and_then(|e| match e.get("lvl") {
            Some(Value::Short(l)) => u8::try_from(*l).ok(),
            _ => None,
        })
        .unwrap_or(0)
}

impl Bot {
    /// The cell the bot's feet are in, from the simulated body or, before physics starts, from the state.
    pub(crate) fn feet_block(&self) -> Option<BlockPos> {
        self.block_position().or_else(|| {
            let p = &self.state.player.position;
            Some([p.x.floor() as i32, (p.y + 0.01).floor() as i32, p.z.floor() as i32])
        })
    }

    /// Whether the block at `pos` is the cell the bot stands on: breaking it drops the bot into the hole.
    pub(crate) fn stands_on(&self, pos: BlockPos) -> bool {
        self.has_physics() && self.feet_block() == Some([pos[0], pos[1] + 1, pos[2]])
    }

    /// Whether `pos` touches the bot's own body (its feet or head cell, or a horizontal neighbour of either).
    pub(crate) fn touches_body(&self, pos: BlockPos) -> bool {
        let Some(feet) = self.feet_block() else { return false };
        let (dx, dy, dz) = (pos[0] - feet[0], pos[1] - feet[1], pos[2] - feet[2]);
        dx.abs() <= 1 && dz.abs() <= 1 && (-1..=2).contains(&dy)
    }

    /// Waits for the fall a break under the feet starts, so the next action measures reach from solid ground (a
    /// dig aimed mid-fall is what the server refuses with "no change"). A bot that is *still* standing on air
    /// after `ticks` is reported: its own physics is not moving it, and silently carrying on hides that.
    async fn wait_landing(&mut self, before: Option<f32>, ticks: u32) -> Result<(), ActionError> {
        for _ in 0..ticks {
            let landed = self.movement.as_ref().is_some_and(|m| m.on_ground())
                && self.feet_y().is_some_and(|y| before.is_none_or(|b| y < b - 0.5));
            if landed {
                return Ok(());
            }
            self.next_tick(|_, _| false).await?;
        }
        let feet = self.feet_block();
        if let Some(f) = feet
            && self.block_at([f[0], f[1] - 1, f[2]]).is_some_and(|(_, s)| s.is_air())
        {
            return Err(ActionError::Rejected(format!(
                "broke the block under the feet but the bot is still at y={:?} after {ticks} ticks, standing on air at {f:?}: its physics did not move it",
                self.feet_y()
            )));
        }
        Ok(())
    }
}

/// Everything the bot carries, as one number: a break that yields a drop raises it.
fn inventory_total(bot: &Bot) -> u32 {
    bot.state().inventory.iter().map(|(_, _, s)| u32::from(s.count)).sum()
}

/// The cell as the bot's view shows it now, for a failure message.
fn cell_name(bot: &Bot, pos: BlockPos) -> String {
    bot.block_at(pos).map_or_else(|| "unloaded".to_string(), |(_, s)| s.name.to_string())
}

/// Whether a drop from the broken block is around, for a failure message.
fn drop_report(bot: &Bot, pos: BlockPos) -> &'static str {
    if bot.drop_near(pos, DROP_RADIUS) {
        "a drop is near"
    } else {
        "no drop seen"
    }
}

// --- t143: walk in, then dig ---------------------------------------------------------------------
//
// t127/t132 on the live server: 78 of 80 `dig` calls answered `out_of_reach` for targets a couple of
// blocks away, and the caller had to `move_to` first and then `dig` — two round trips for one block,
// and a refusal that read the same whether the bot could not walk there or simply was not near enough.
//
// So the tool walks before it refuses. What it may walk to is fixed and small: the ring of cells
// around the target, at its own level or one below. It never targets the cell under the bot's feet
// (breaking the support is how a heal loses the height it just gained, t95) and it never digs
// anything to get closer — a walk that needs a tunnel is the caller's call, not a side effect of `dig`.

/// The cells a dig may stand on to reach `block`: its 8 horizontal neighbours, at the block's level and
/// one below (reaching up or down one block is what the arm does anyway). The block itself is never a
/// target — standing on the block being broken is standing on what falls.
pub fn dig_stand_cells(block: BlockPos) -> Vec<BlockPos> {
    let mut cells = Vec::with_capacity(16);
    for dz in -1..=1 {
        for dx in -1..=1 {
            if dx == 0 && dz == 0 {
                continue;
            }
            cells.push([block[0] + dx, block[1], block[2] + dz]);
            cells.push([block[0] + dx, block[1] - 1, block[2] + dz]);
        }
    }
    cells
}

/// In reach already, or the cell to walk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigApproach {
    InReach,
    Walk(BlockPos),
}

/// The pure half of walk-then-dig: is the block reachable from `eye`, and if not, which ring cell is
/// both standable and nearest.
///
/// `standable(cell)` is `Some(true)` when the bot could stand there (two air cells, solid floor),
/// `None` when the world does not know the cell yet.
pub fn dig_approach<F>(eye: Vec3, block: BlockPos, reach: f32, standable: F) -> Option<DigApproach>
where
    F: Fn(BlockPos) -> Option<bool>,
{
    if block_box_distance(eye, block) <= reach {
        return Some(DigApproach::InReach);
    }
    let mut best: Option<(f32, BlockPos)> = None;
    for cell in dig_stand_cells(block) {
        if standable(cell) != Some(true) {
            continue;
        }
        // Walk cost, not reach: the standing cell's centre, at its feet height.
        let at = [cell[0] as f32 + 0.5, cell[1] as f32, cell[2] as f32 + 0.5];
        let d = (0..3).map(|i| (at[i] - eye[i]).powi(2)).sum::<f32>().sqrt();
        if best.is_none_or(|(best_d, _)| d < best_d) {
            best = Some((d, cell));
        }
    }
    best.map(|(_, cell)| DigApproach::Walk(cell))
}

/// How the walk to a dig target failed, in the vocabulary the tool layer reports.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WalkFailure {
    /// No route: the search found no connected way to any cell within reach.
    NoPath { stuck_at: Option<BlockPos> },
    /// A route existed but the body could not walk it (stuck, knocked off, gave up on the clock).
    Blocked { stuck_at: Option<BlockPos> },
}

/// Reads a `goto` failure: no route at all, or a route the body could not follow.
///
/// The pathfinder's own words are the only signal it leaves on the error, and `PlanStats` (t117) says
/// whether the search even reached the goal's neighbourhood — that is the part that separates "there
/// is no way" from "there is a way and we could not walk it".
pub(crate) fn classify_walk(message: &str, stats: Option<&crate::pathfind::PlanStats>) -> WalkFailure {
    let reached = stats.is_some_and(|s| s.target_region_reached);
    let cut = stats.is_some_and(|s| s.budget_exhausted);
    let says_no_path = message.contains("cannot reach") || message.contains("no path") || message.contains("unreachable");
    if reached && !says_no_path {
        return WalkFailure::Blocked { stuck_at: None };
    }
    if says_no_path || cut {
        return WalkFailure::NoPath { stuck_at: None };
    }
    // No stats and no "cannot reach": the follower stopped for a reason of its own.
    WalkFailure::Blocked { stuck_at: None }
}

/// Why a `dig_at` stopped without breaking the block.
#[derive(Debug, Clone, PartialEq)]
pub enum DigStop {
    /// No way to get within reach (including "there is no cell next to it to stand in").
    NoPath { stuck_at: Option<BlockPos>, note: &'static str },
    /// A route existed but the walk could not follow it.
    BlockedByTerrain { stuck_at: Option<BlockPos> },
    /// Walked as close as the route allowed and the block is still out of the arm's reach.
    OutOfReach { distance: f32, reach: f32, from: BlockPos },
    /// The break itself was refused; carries the action's own words, which map to their own reasons.
    Break(String),
}

impl DigStop {
    /// The failure vocabulary for the walk half: `no_path` / `blocked_by_terrain` / `out_of_reach`.
    /// `None` for a break refusal, which keeps whatever reason its own message maps to.
    pub fn reason(&self) -> Option<&'static str> {
        Some(match self {
            DigStop::NoPath { .. } => "no_path",
            DigStop::BlockedByTerrain { .. } => "blocked_by_terrain",
            DigStop::OutOfReach { .. } => "out_of_reach",
            DigStop::Break(_) => return None,
        })
    }

    pub fn describe(&self) -> String {
        match self {
            DigStop::NoPath { note, .. } => format!("cannot walk up to the block: {note}"),
            DigStop::BlockedByTerrain { .. } => "cannot walk up to the block: the way there is blocked".to_string(),
            DigStop::OutOfReach { distance, reach, from } => format!(
                "walked as close as the route allowed ({from:?}) and the block is still {distance:.2} blocks away (reach {reach:.2})"
            ),
            DigStop::Break(why) => why.clone(),
        }
    }

    pub fn details(&self) -> serde_json::Value {
        match self {
            DigStop::NoPath { stuck_at, note } => serde_json::json!({"stuck_at": stuck_at, "note": note}),
            DigStop::BlockedByTerrain { stuck_at } => serde_json::json!({"stuck_at": stuck_at}),
            DigStop::OutOfReach { distance, reach, from } => {
                serde_json::json!({"distance": round2(*distance), "reach": round2(*reach), "from": from})
            }
            DigStop::Break(_) => serde_json::Value::Null,
        }
    }
}

fn round2(value: f32) -> f32 {
    (value * 100.0).round() / 100.0
}

/// What a `dig_at` did.
#[derive(Debug, Clone, PartialEq)]
pub struct DigReport {
    /// The cell walked to, `None` when the block was already in reach.
    pub walked_to: Option<BlockPos>,
    pub distance_before: f32,
    pub distance_after: f32,
    pub reach: f32,
}

impl DigReport {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "walked_to": self.walked_to,
            "distance_before": round2(self.distance_before),
            "distance_after": round2(self.distance_after),
            "reach": round2(self.reach),
        })
    }
}

/// Opts for [`Bot::dig_at`].
#[derive(Debug, Clone)]
pub struct DigOpts {
    /// Walk to a cell within reach first; `false` keeps the plain "refuse, let the caller move" answer.
    pub walk: bool,
    /// Give up on a walk after this many ticks without getting closer.
    pub give_up_ticks: u32,
}

impl Default for DigOpts {
    fn default() -> Self {
        Self { walk: true, give_up_ticks: 400 }
    }
}

/// What the approach needs from a body, split out so the three endings are testable without a server.
pub(crate) trait DigWalker {
    fn eye(&self) -> Vec3;
    /// `Some(true)` when the bot could stand in `cell`: two air cells, a solid floor under them.
    fn standable(&self, cell: BlockPos) -> Option<bool>;
    /// Feet block position, for the report.
    fn feet(&self) -> Option<BlockPos>;
    fn walk_to(&mut self, cell: BlockPos) -> impl std::future::Future<Output = Result<(), WalkFailure>>;
}

/// The walk half of `dig_at`: decide, walk, then measure again from where the walk ended.
pub(crate) async fn walk_within_reach<W: DigWalker + ?Sized>(
    walker: &mut W,
    block: BlockPos,
    reach: f32,
    walk: bool,
) -> Result<Option<BlockPos>, DigStop> {
    let eye = walker.eye();
    let decision = if walk {
        dig_approach(eye, block, reach, |cell| walker.standable(cell))
    } else {
        // `walk: false` is "do not walk": with no standable cell the decision is the empty one.
        dig_approach(eye, block, reach, |_| Some(false))
    };
    let target = match decision {
        Some(DigApproach::InReach) => return Ok(None),
        Some(DigApproach::Walk(cell)) => cell,
        None if walk => {
            return Err(DigStop::NoPath { stuck_at: Some(block), note: "no cell next to it has room to stand in" });
        }
        None => {
            let distance = block_box_distance(walker.eye(), block);
            return Err(DigStop::OutOfReach { distance, reach, from: walker.feet().unwrap_or(block) });
        }
    };
    match walker.walk_to(target).await {
        Ok(()) => {}
        Err(WalkFailure::NoPath { stuck_at }) => {
            return Err(DigStop::NoPath { stuck_at: stuck_at.or(Some(target)), note: "the search found no way there" });
        }
        Err(WalkFailure::Blocked { stuck_at }) => {
            return Err(DigStop::BlockedByTerrain { stuck_at: stuck_at.or(Some(target)) });
        }
    }
    let distance = block_box_distance(walker.eye(), block);
    if distance > reach {
        return Err(DigStop::OutOfReach { distance, reach, from: walker.feet().unwrap_or(target) });
    }
    Ok(Some(target))
}

/// Why `dig_at` did not break the block: the walk half (`DigStop`) or the break itself.
#[derive(Debug)]
pub enum DigFailure {
    Walk(DigStop),
    Break(ActionError),
}

impl Bot {
    /// Breaks the block at `pos`, walking into reach first when it is further than the arm (t143).
    ///
    /// The walk only ever aims at a [`dig_stand_cells`] cell of the target, so it cannot break the
    /// support under the bot and cannot dig anything on the way.
    pub async fn dig_at(&mut self, pos: BlockPos, opts: DigOpts) -> Result<DigReport, DigFailure> {
        let reach = BLOCK_REACH;
        let distance_before = block_box_distance(self.eye_position(), pos);
        // A body without physics cannot walk: it keeps the old single-shot answer.
        let walked_to = if self.has_physics() {
            walk_within_reach(self, pos, reach, opts.walk).await.map_err(DigFailure::Walk)?
        } else {
            None
        };
        let distance_after = block_box_distance(self.eye_position(), pos);
        self.break_block(pos).await.map_err(DigFailure::Break)?;
        Ok(DigReport { walked_to, distance_before, distance_after, reach })
    }
}

impl Bot {
    /// Whether `pos` is a cell the bot could stand on: known to the world, and with a collision box.
    fn solid_at(&self, pos: BlockPos) -> Option<bool> {
        Some(!self.block_state(pos)?.boxes.is_empty())
    }
}

impl DigWalker for Bot {
    fn eye(&self) -> Vec3 {
        Bot::eye_position(self)
    }

    fn standable(&self, cell: BlockPos) -> Option<bool> {
        let solid = |pos: BlockPos| self.solid_at(pos);
        Some(solid(cell)? && !solid([cell[0], cell[1] + 1, cell[2]])? && solid([cell[0], cell[1] - 1, cell[2]])?)
    }

    fn feet(&self) -> Option<BlockPos> {
        self.block_position()
    }

    async fn walk_to(&mut self, cell: BlockPos) -> Result<(), WalkFailure> {
        let opts = GotoOpts { give_up_ticks: Some(400), ..GotoOpts::default() };
        match self.goto_with(crate::pathfind::Goal::Block(cell), opts).await {
            Ok(()) => Ok(()),
            Err(error) => Err(classify_walk(&error.to_string(), self.last_plan_stats().as_ref())),
        }
    }
}

#[cfg(test)]
mod t143_tests {
    use super::*;

    /// A body whose answers are scripted: where the eye is, which cells are standable, and what a walk does.
    struct Fake {
        eye: Vec3,
        standable: Vec<(BlockPos, bool)>,
        /// Where the walk ends up; `None` keeps the body where it was.
        walk_to: Option<BlockPos>,
        walk_fails: Option<WalkFailure>,
        walked: Vec<BlockPos>,
    }

    impl Fake {
        fn new(eye: Vec3) -> Self {
            Self { eye, standable: Vec::new(), walk_to: None, walk_fails: None, walked: Vec::new() }
        }

        fn standing(mut self, cell: BlockPos) -> Self {
            self.standable.push((cell, true));
            self
        }
    }

    impl DigWalker for Fake {
        fn eye(&self) -> Vec3 {
            self.eye
        }

        fn standable(&self, cell: BlockPos) -> Option<bool> {
            Some(self.standable.iter().any(|(at, ok)| *at == cell && *ok))
        }

        fn feet(&self) -> Option<BlockPos> {
            Some([self.eye[0].floor() as i32, self.eye[1].floor() as i32, self.eye[2].floor() as i32])
        }

        async fn walk_to(&mut self, cell: BlockPos) -> Result<(), WalkFailure> {
            self.walked.push(cell);
            if let Some(failure) = &self.walk_fails {
                return Err(failure.clone());
            }
            if let Some(dest) = self.walk_to {
                self.eye = [dest[0] as f32 + 0.5, dest[1] as f32 + 1.62, dest[2] as f32 + 0.5];
            }
            Ok(())
        }
    }

    const REACH: f32 = BLOCK_REACH;

    /// t127/t132: the block was a few blocks away and the answer was `out_of_reach` with nothing walked.
    /// Now the body walks to the ring cell next to it and the dig goes ahead.
    #[tokio::test]
    async fn a_target_ten_blocks_away_is_walked_into_reach() {
        let block: BlockPos = [0, 1, 0];
        let mut body = Fake::new([0.5, 2.62, 10.5]).standing([0, 1, 1]);
        body.walk_to = Some([0, 1, 1]);
        let walked = walk_within_reach(&mut body, block, REACH, true).await.expect("walked into reach");
        assert_eq!(walked, Some([0, 1, 1]), "the cell next to the block");
        assert_eq!(body.walked, vec![[0, 1, 1]], "one walk, to the ring cell, not to the block itself");
        // From there the block is half a block away: the dig itself would now be in reach.
        assert!(block_box_distance(body.eye, block) <= REACH);
    }

    /// Standing next to it is not the same as being able to reach it: a body that walks as far as the
    /// route allows and is still too short says so, with the numbers.
    #[tokio::test]
    async fn a_walk_that_ends_short_still_says_out_of_reach() {
        let block: BlockPos = [0, 1, 0];
        // The ring cell is next to the block; the body claims the walk and then barely moves (the shape
        // t44/t46 saw on the wire, where the follower answered `arrived` for a body two blocks behind).
        let mut body = Fake::new([0.5, 2.62, 10.5]).standing([0, 1, 1]);
        let stop = walk_within_reach(&mut body, block, REACH, true).await.expect_err("still too far");
        assert_eq!(stop.reason(), Some("out_of_reach"));
        match &stop {
            DigStop::OutOfReach { distance, reach, .. } => {
                assert!(*distance > REACH, "the distance is the measured one: {distance}");
                assert_eq!(*reach, REACH);
            }
            other => panic!("expected out_of_reach, got {other:?}"),
        }
        let distance = stop.details()["distance"].as_f64().expect("a number");
        assert!((distance - 9.52).abs() < 0.01, "measured from the box: {distance}");
        assert_eq!(stop.details()["from"], serde_json::json!([0, 2, 10]), "the cell the body was still in");
    }

    /// No way there: the search found nothing, and the failure names the cell it aimed at.
    #[tokio::test]
    async fn a_walk_with_no_route_is_no_path_not_out_of_reach() {
        let block: BlockPos = [0, 1, 0];
        let mut body = Fake::new([0.5, 2.62, 10.5]).standing([0, 1, 1]);
        body.walk_fails = Some(WalkFailure::NoPath { stuck_at: None });
        let stop = walk_within_reach(&mut body, block, REACH, true).await.expect_err("no route");
        assert_eq!(stop.reason(), Some("no_path"));
        assert_eq!(stop.details()["stuck_at"], serde_json::json!([0, 1, 1]), "where it was walking to");
        assert!(!stop.describe().contains("blocks away"), "not mistaken for a reach problem: {}", stop.describe());
    }

    /// A route the body could not walk is its own answer — the third ending, not a synonym of the others.
    #[tokio::test]
    async fn a_route_the_body_cannot_walk_is_blocked_by_terrain() {
        let block: BlockPos = [0, 1, 0];
        let mut body = Fake::new([0.5, 2.62, 10.5]).standing([0, 1, 1]);
        body.walk_fails = Some(WalkFailure::Blocked { stuck_at: Some([0, 49, 7]) });
        let stop = walk_within_reach(&mut body, block, REACH, true).await.expect_err("blocked");
        assert_eq!(stop.reason(), Some("blocked_by_terrain"));
        assert_eq!(stop.details()["stuck_at"], serde_json::json!([0, 49, 7]));
    }

    /// No cell in the ring has room to stand: that is still "cannot get there", never "too far to reach".
    #[tokio::test]
    async fn a_blocked_in_ring_with_nowhere_to_stand_is_no_path() {
        let block: BlockPos = [0, 1, 0];
        let mut body = Fake::new([0.5, 2.62, 10.5]);
        let stop = walk_within_reach(&mut body, block, REACH, true).await.expect_err("no standing spot");
        assert_eq!(stop.reason(), Some("no_path"));
        assert!(body.walked.is_empty(), "nothing to walk to, so nothing walked");
        assert_eq!(stop.details()["stuck_at"], serde_json::json!([0, 1, 0]), "the block itself is what could not be approached");
    }

    /// Already close enough: no walk at all (the old behaviour, kept for the common case).
    #[tokio::test]
    async fn a_block_already_within_reach_is_not_walked_to() {
        let block: BlockPos = [0, 1, 0];
        let mut body = Fake::new([0.5, 2.62, 1.5]).standing([0, 1, 1]);
        let walked = walk_within_reach(&mut body, block, REACH, true).await.expect("in reach");
        assert_eq!(walked, None);
        assert!(body.walked.is_empty());
    }

    /// `walk: false` is the caller saying "just tell me I am too far": no walking, and the answer is the
    /// reach one, not a path one.
    #[tokio::test]
    async fn walk_false_refuses_without_walking() {
        let block: BlockPos = [0, 1, 0];
        let mut body = Fake::new([0.5, 2.62, 10.5]).standing([0, 1, 1]);
        let stop = walk_within_reach(&mut body, block, REACH, false).await.expect_err("too far");
        assert_eq!(stop.reason(), Some("out_of_reach"));
        assert!(body.walked.is_empty(), "a refusal to walk must not walk");
    }

    /// The ring is the block's own neighbours: the block is never a target (standing on what you break
    /// is standing on what falls), and a two-cell-high stack is never half-broken on the way in.
    #[test]
    fn the_ring_never_contains_the_block_itself() {
        let block: BlockPos = [5, 64, 9];
        let cells = dig_stand_cells(block);
        assert_eq!(cells.len(), 16, "8 neighbours, each at the block's level and one below");
        assert!(!cells.contains(&block));
        assert!(cells.iter().all(|cell| (cell[0] - block[0]).abs() <= 1 && (cell[2] - block[2]).abs() <= 1));
        assert!(cells.iter().all(|cell| cell[1] == block[1] || cell[1] == block[1] - 1), "never above the target");
    }

    /// The nearest ring cell wins, not the first one scanned — otherwise a walk to the far side of the
    /// same block would be as likely as the near one (t93's lesson, in the dig ring).
    #[test]
    fn the_nearest_standing_cell_is_the_one_walked_to() {
        let block: BlockPos = [0, 1, 0];
        let eye = [0.5, 2.62, 10.5];
        // Both are in the ring and standable, and the scan order reaches z=-1 before z=+1: the far corner
        // is offered first, while the body standing at z=10.5 is nearer the z=+1 side.
        let standable = |cell: BlockPos| Some(cell[1] == 1 && ((cell[0], cell[2]) == (1, -1) || (cell[0], cell[2]) == (0, 1)));
        let decision = dig_approach(eye, block, REACH, standable);
        assert_eq!(decision, Some(DigApproach::Walk([0, 1, 1])));
    }

    /// The classifier reads `PlanStats`: reaching the goal's neighbourhood and still failing is a walk
    /// problem, not a map problem.
    #[test]
    fn a_search_that_reached_the_goal_but_could_not_walk_it_is_not_a_no_path() {
        let reached = crate::pathfind::PlanStats {
            nodes_expanded: 40,
            max_nodes: 50_000,
            budget_exhausted: false,
            target_region_reached: true,
            closest_short: Some(0.0),
            ..crate::pathfind::PlanStats::default()
        };
        let cut = crate::pathfind::PlanStats {
            nodes_expanded: 40,
            max_nodes: 40,
            budget_exhausted: true,
            target_region_reached: false,
            closest_short: Some(3.0),
            ..crate::pathfind::PlanStats::default()
        };
        assert!(matches!(classify_walk("walk gave up", Some(&reached)), WalkFailure::Blocked { .. }));
        assert!(matches!(classify_walk("walk gave up", Some(&cut)), WalkFailure::NoPath { .. }));
        assert!(matches!(classify_walk("cannot reach Block([1, 2, 3]) from [0, 0, 0]", None), WalkFailure::NoPath { .. }));
        assert!(matches!(classify_walk("the way is blocked", None), WalkFailure::Blocked { .. }));
    }
}
