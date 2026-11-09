//! Walking to dropped items and waiting for the server to hand them over.
//!
//! A drop is an entity of kind [`ITEM_KIND`] carrying the stack in [`Entity::item`]; the server gives
//! it to whichever player is close enough (vanilla: one block), then removes the entity and updates
//! the inventory (`TakeItemEntity` + slot update, or an `InventoryTransaction`). This module is
//! DeepClef's `@get` / pickup task on top of the primitives the crate already had: [`Bot::attack`]
//! for the swing, [`Bot::goto_with`] for the walk, and the entity tracker for the drop.
//!
//! Needs [`crate::BotConfig::trackers`] entity tracking (`Trackers { entities: true, .. }`) and a
//! physics bot (the walk is pathfinding). An idle bot can still pick up a drop at its feet: the walk is
//! skipped and only the wait runs.

use std::time::{Duration, Instant};

use acacia_physics::{BlockPos, Vec3};

use crate::pathfind::{Goal, GotoOpts};
use crate::state::{Entity, ITEM_KIND};
use crate::{ActionError, Bot};

/// The server hands a drop over within this many blocks of the feet (vanilla's pickup radius).
pub const PICKUP_RANGE: f32 = 1.4;
/// How long to stand on a drop before checking whether the walk should be redone.
const WAIT: Duration = Duration::from_secs(2);
/// Never chase one drop for longer than this (items can sit behind a wall, in lava, under a block).
const TIMEOUT: Duration = Duration::from_secs(30);
/// Walks at one drop before giving up: a drop that keeps sliding (on ice, in water) or a burst from
/// one killed mob needs a few.
const TRIES: usize = 8;
/// Stacks `Bot::collect_all` collects before returning (a guard against a drop that is never picked
/// up and a world that keeps spawning items).
const MAX_STACKS: usize = 64;

/// Options for one walk to a drop.
#[derive(Debug, Clone)]
pub struct CollectOpts {
    /// Break the blocks between the bot and the drop. On by default: a block the bot just mined leaves
    /// its own tunnel wall in the way, and a pickup that cannot dig either fails outright or needs the
    /// caller to dig and pick up in two steps (t103).
    pub allow_dig: bool,
    /// Ticks without coming a block closer before the walk is redone: short attempts, the loop re-plans
    /// (the drop may slide).
    pub give_up_ticks: Option<u32>,
    /// Never chase one drop for longer than this (items can sit behind a wall, in lava, under a block).
    pub timeout: Duration,
    /// Walks at one drop before giving up (a drop that keeps sliding, or a burst from one killed mob).
    pub tries: usize,
}

impl Default for CollectOpts {
    fn default() -> Self {
        Self { allow_dig: true, give_up_ticks: Some(60), timeout: TIMEOUT, tries: TRIES }
    }
}

impl CollectOpts {
    /// The walk settings these options ask for.
    fn goto(&self) -> GotoOpts {
        let mut opts = GotoOpts { give_up_ticks: self.give_up_ticks, ..GotoOpts::default() };
        opts.search.allow_dig = self.allow_dig;
        opts
    }
}

impl Bot {
    /// Every tracked dropped item, in tracking order.
    pub fn dropped_items(&self) -> impl Iterator<Item = &Entity> {
        self.state.entities.iter().filter(|e| e.kind == ITEM_KIND && e.item.is_some())
    }

    /// Whether `runtime_id` is a tracked dropped item.
    pub fn is_dropped_item(&self, runtime_id: u64) -> bool {
        self.state.entities.get(runtime_id).is_some_and(|e| e.kind == ITEM_KIND && e.item.is_some())
    }

    /// Whether a dropped item sits within `radius` blocks of the centre of `pos`: the pick-up a broken block
    /// leaves behind, which tells a break that the server did run it even when the cell update is late.
    pub(crate) fn drop_near(&self, pos: BlockPos, radius: f32) -> bool {
        let c = [pos[0] as f32 + 0.5, pos[1] as f32 + 0.5, pos[2] as f32 + 0.5];
        self.dropped_items().any(|e| {
            let f = e.feet();
            let d2 = (f.x - c[0]).powi(2) + (f.y - c[1]).powi(2) + (f.z - c[2]).powi(2);
            d2 <= radius * radius
        })
    }

    /// The nearest dropped item within `radius` blocks of the feet, optionally only a stack of `item`
    /// (`minecraft:apple`). `None` without entity tracking or without a known own position.
    pub fn nearest_dropped_item(&self, radius: f32, item: Option<&str>) -> Option<u64> {
        let want = match item {
            Some(name) => Some(self.state.items.id(name)?),
            None => None,
        };
        let feet = self.feet();
        let drops = self
            .dropped_items()
            .filter(|e| want.is_none_or(|id| e.item.as_ref().is_some_and(|s| s.network_id == id)))
            .map(|e| {
                let f = e.feet();
                (e.runtime_id, [f.x, f.y, f.z])
            });
        nearest_by(drops, feet, radius).map(|(id, _)| id)
    }

    /// Walks to the dropped item and waits for the server to hand it over. Digs the blocks in the way
    /// ([`CollectOpts::default`]); for the blocked cell and the last walk's error use
    /// [`Bot::collect_with`].
    ///
    /// Done once the entity is gone (picked up, or despawned after five minutes) or the stack's count
    /// in the inventory rose. Walks again when the drop slid or the first attempt stopped short
    /// (`GotoOpts::give_up_ticks`), up to [`TRIES`] times and [`TIMEOUT`].
    ///
    /// Errors: `NotPossible` when the id is not a tracked drop (entity tracking off, or it was
    /// already taken), `Rejected` when it was never picked up, `Cancelled`/`Disconnected` as usual.
    pub async fn collect(&mut self, runtime_id: u64) -> Result<(), ActionError> {
        self.collect_with(runtime_id, CollectOpts::default()).await.map_err(CollectError::into_action)
    }

    /// [`Bot::collect`] with options: `allow_dig` lets the walk break the blocks between the bot and
    /// the drop, and a drop that was never handed over reports the cell that stopped the walk
    /// ([`PickupFailure::blocked_at`]) together with the last walk's error, so a caller can say "dig
    /// that one first" instead of "cannot reach".
    pub async fn collect_with(&mut self, runtime_id: u64, opts: CollectOpts) -> Result<(), CollectError> {
        let stack = self
            .state
            .entities
            .get(runtime_id)
            .filter(|e| e.kind == ITEM_KIND)
            .and_then(|e| e.item.clone())
            .ok_or(CollectError::NotDrop(runtime_id))?;
        let id = stack.network_id;
        let before = self.state.inventory.count_of(id);
        let deadline = Instant::now() + opts.timeout;
        let mut last_feet = None;
        let mut last_walk_error = None;
        let mut blocked_at = None;
        for _ in 0..opts.tries {
            if Instant::now() >= deadline {
                break;
            }
            let Some(feet) = self.state.entities.get(runtime_id).map(|e| { let f = e.feet(); [f.x, f.y, f.z] }) else {
                // Gone before we got there: picked up (by us or someone else) or despawned.
                return Ok(());
            };
            last_feet = Some(feet);
            if !self.within_pickup(feet) {
                let cell = cell_of(feet);
                // A walk that fails must not end the pickup: the drop may have moved behind the bot.
                if let Err(e) = self.goto_with(Goal::Near(cell, PICKUP_RANGE * 0.5), opts.goto()).await {
                    tracing::debug!(runtime_id, error = %e, "walk to the drop");
                    if blocked_at.is_none() {
                        let here = self.feet_cell().unwrap_or_else(|| cell_of(self.feet()));
                        blocked_at = blocked_between(here, cell, |p| self.solid(p));
                    }
                    last_walk_error = Some(e.to_string());
                }
            }
            match self.wait_until(WAIT, |bot, _| pickup_happened(bot, runtime_id, id, before)).await {
                Ok(()) => return Ok(()),
                Err(ActionError::Timeout) => {}
                Err(e) => return Err(CollectError::Action(e)),
            }
        }
        if pickup_happened(self, runtime_id, id, before).is_some() {
            return Ok(());
        }
        Err(CollectError::NotPickedUp(Box::new(PickupFailure {
            id: runtime_id,
            item: name_or_id(self, id),
            count: stack.count,
            at: last_feet,
            blocked_at,
            last_walk: last_walk_error,
        })))
    }

    /// Picks up every dropped item within `radius` blocks of the feet, nearest first. Returns how many
    /// were collected; drops that stay out of reach are skipped, not retried.
    pub async fn collect_all(&mut self, radius: f32) -> Result<usize, ActionError> {
        let mut picked = 0;
        let mut skip: Vec<u64> = Vec::new();
        for _ in 0..MAX_STACKS {
            let Some(id) = self.nearest_dropped_item(radius, None).filter(|id| !skip.contains(id)) else { break };
            match self.collect(id).await {
                Ok(()) => picked += 1,
                Err(ActionError::NotPossible(_) | ActionError::Rejected(_) | ActionError::Timeout | ActionError::NotVisible(_)) => {
                    tracing::debug!(id, "drop left behind");
                    skip.push(id);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(picked)
    }

    /// Own feet position, simulated for a physics bot and the server's last position otherwise.
    fn feet(&self) -> Vec3 {
        if let Some(p) = self.movement.as_ref().and_then(|m| m.position()) {
            return p;
        }
        let p = &self.state.player.position;
        [p.x, p.y, p.z]
    }

    /// The own feet are close enough to a drop at `feet` for the server to hand it over.
    fn within_pickup(&self, feet: [f32; 3]) -> bool {
        dist(self.feet(), feet) <= PICKUP_RANGE
    }

    /// Whether a cell would stop a body: it has a collision box (air, liquids and fire have none, and a
    /// flower's box does not stop a walk — the search is what decides that; this is only for naming the
    /// cell a failed walk ran into). `None` where the world is not loaded (never counts as blocking).
    fn solid(&self, pos: BlockPos) -> Option<bool> {
        self.block_state(pos).map(|b| !b.boxes.is_empty())
    }
}

/// Whether the drop was picked up: the entity is gone, or the stack's count in the inventory rose.
fn pickup_happened(bot: &Bot, runtime_id: u64, id: i32, before: u32) -> Option<()> {
    (bot.state.entities.get(runtime_id).is_none() || bot.state.inventory.count_of(id) > before).then_some(())
}

/// Why a drop was not collected.
#[derive(Debug)]
pub enum CollectError {
    /// The id is not a tracked dropped item (entity tracking off, or it was already taken).
    NotDrop(u64),
    /// The walk and the wait ran; the server never handed the drop over.
    NotPickedUp(Box<PickupFailure>),
    /// The wait itself was interrupted (cancelled, disconnected).
    Action(ActionError),
}

impl CollectError {
    /// The cell that stopped the walk to the drop, when one was found.
    pub fn blocked_at(&self) -> Option<BlockPos> {
        match self {
            CollectError::NotPickedUp(f) => f.blocked_at,
            _ => None,
        }
    }

    /// The last walk's error, verbatim.
    pub fn last_walk(&self) -> Option<&str> {
        match self {
            CollectError::NotPickedUp(f) => f.last_walk.as_deref(),
            _ => None,
        }
    }

    /// The error as the action-level type [`Bot::collect`] has always returned.
    fn into_action(self) -> ActionError {
        match self {
            CollectError::NotDrop(id) => ActionError::NotPossible(format!(
                "entity {id} is not a tracked dropped item (is Trackers::entities on?)"
            )),
            CollectError::NotPickedUp(f) => ActionError::Rejected(f.to_string()),
            CollectError::Action(e) => e,
        }
    }
}

impl std::fmt::Display for CollectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CollectError::NotDrop(id) => write!(f, "entity {id} is not a tracked dropped item"),
            CollectError::NotPickedUp(failure) => failure.fmt(f),
            CollectError::Action(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for CollectError {}

/// A drop that was never handed over: what it was, where it sat, what stopped the walk and why.
#[derive(Debug, Clone, PartialEq)]
pub struct PickupFailure {
    /// The drop entity.
    pub id: u64,
    /// Its item (`minecraft:raw_iron`, or `#id` when the name is unknown).
    pub item: String,
    /// Stack size of the drop.
    pub count: u16,
    /// Where the drop was last seen (its feet).
    pub at: Option<[f32; 3]>,
    /// The cell on the way to the drop that a body cannot pass (solid at feet or head height): what has
    /// to be dug first. `None` when the walk failed for another reason (no path found, terrain).
    pub blocked_at: Option<BlockPos>,
    /// The last walk's error, verbatim.
    pub last_walk: Option<String>,
}

impl std::fmt::Display for PickupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the drop (entity {}, {} x{}) was not picked up at {:?}", self.id, self.item, self.count, self.at)?;
        if let Some([x, y, z]) = self.blocked_at {
            write!(f, "; blocked at [{x}, {y}, {z}] — dig that one first")?;
        }
        if let Some(walk) = &self.last_walk {
            write!(f, "; last walk: {walk}")?;
        }
        Ok(())
    }
}

/// The first cell on the straight walk from `here` to `cell` that a body cannot pass: solid at feet or
/// at head height (`y + 1`). Sampled along the longer horizontal axis, so a wall beside the line is not
/// reported and the target cell itself never is.
///
/// `solid` returns `None` where the world is not loaded (not blocking) and `Some(true)` for a cell a
/// body cannot stand in or walk through.
pub(crate) fn blocked_between(
    here: BlockPos,
    cell: BlockPos,
    solid: impl Fn(BlockPos) -> Option<bool>,
) -> Option<BlockPos> {
    let (dx, dz) = (cell[0] - here[0], cell[2] - here[2]);
    let steps = dx.abs().max(dz.abs());
    if steps == 0 {
        return None;
    }
    for step in 1..steps {
        let t = step as f32 / steps as f32;
        let x = here[0] + (dx as f32 * t).round() as i32;
        let z = here[2] + (dz as f32 * t).round() as i32;
        let y = here[1];
        for probe in [[x, y, z], [x, y + 1, z]] {
            if solid(probe) == Some(true) {
                return Some(probe);
            }
        }
    }
    None
}

fn cell_of(feet: [f32; 3]) -> [i32; 3] {
    [feet[0].floor() as i32, feet[1].floor() as i32, feet[2].floor() as i32]
}

fn name_or_id(bot: &Bot, id: i32) -> String {
    bot.state.items.name(id).map_or_else(|| format!("#{id}"), str::to_owned)
}

fn dist(a: Vec3, b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// The nearest `(runtime_id, position)` within `radius` of `from`.
fn nearest_by(items: impl Iterator<Item = (u64, [f32; 3])>, from: Vec3, radius: f32) -> Option<(u64, f32)> {
    let mut best: Option<(u64, f32)> = None;
    for (id, pos) in items {
        let d = dist(from, pos);
        if d <= radius && best.is_none_or(|(_, best)| d < best) {
            best = Some((id, d));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_drop_within_reach_wins() {
        let drops = [(1, [0.0, 0.0, 2.0]), (2, [0.0, 0.0, 1.0]), (3, [0.0, 0.0, 30.0])];
        assert_eq!(nearest_by(drops.into_iter(), [0.0, 0.0, 0.0], 10.0), Some((2, 1.0)));
        assert_eq!(nearest_by(drops.into_iter(), [0.0, 0.0, 0.0], 0.5), None);
    }

    #[test]
    fn a_drop_only_counts_inside_the_pickup_radius() {
        let feet = [10.0, 64.0, -3.0];
        assert!(dist(feet, [10.5, 64.0, -3.0]) <= PICKUP_RANGE);
        assert!(dist(feet, [10.0, 64.9, -3.0]) <= PICKUP_RANGE, "the server picks up diagonally");
        assert!(dist(feet, [10.0, 64.0, -0.9]) > PICKUP_RANGE, "three blocks away is a walk");
    }

    #[test]
    fn the_drop_cell_keeps_a_resting_item_on_its_block() {
        assert_eq!(cell_of([4.9, 64.0, -2.1]), [4, 64, -3]);
        assert_eq!(cell_of([4.9, 64.99, -2.1]), [4, 64, -3]);
        // A stack resting on the ground sits at the block top: its cell is the feet cell there.
        assert_eq!(cell_of([4.9, 65.0, -2.1]), [4, 65, -3]);
    }

    #[test]
    fn the_cell_that_stops_the_walk_is_named() {
        // The t103 shape: one solid cell between the bot and its drop.
        let wall = [[3, 1, 0]];
        let solid = |p: BlockPos| Some(wall.contains(&p));
        assert_eq!(blocked_between([0, 1, 0], [6, 1, 0], solid), Some([3, 1, 0]));
        assert_eq!(blocked_between([0, 1, 0], [6, 1, 0], |_| Some(false)), None);
        // A head-height block stops the body just as well, and is named where it is.
        let head = [[3, 2, 0]];
        assert_eq!(blocked_between([0, 1, 0], [6, 1, 0], |p| Some(head.contains(&p))), Some([3, 2, 0]));
        // The target itself is never "in the way", and unloaded cells never block.
        assert_eq!(blocked_between([0, 1, 0], [6, 1, 0], |p| Some(p == [6, 1, 0])), None);
        assert_eq!(blocked_between([0, 1, 0], [6, 1, 0], |_| None), None);
        // Nothing between the bot and its own feet cell.
        assert_eq!(blocked_between([0, 1, 0], [0, 1, 0], solid), None);
    }

    #[test]
    fn collecting_digs_by_default_and_can_be_told_not_to() {
        let defaults = CollectOpts::default();
        assert!(defaults.allow_dig, "a pickup behind the ore's own wall must open the way");
        assert!(defaults.goto().search.allow_dig);
        assert_eq!(defaults.give_up_ticks, Some(60));
        let strict = CollectOpts { allow_dig: false, ..defaults };
        assert!(!strict.goto().search.allow_dig, "and a caller can still ask for a walk that never digs");
    }

    #[test]
    fn the_failure_message_names_the_block_and_the_walk() {
        let failure = PickupFailure {
            id: 2675,
            item: "minecraft:raw_iron".to_owned(),
            count: 1,
            at: Some([150018.7, 53.92, 149834.44]),
            blocked_at: Some([150017, 54, 149834]),
            last_walk: Some("cannot reach Near([150018, 53, 149834], 0.7)".to_owned()),
        };
        let text = failure.to_string();
        assert!(text.starts_with("the drop (entity 2675, minecraft:raw_iron x1) was not picked up"), "{text}");
        assert!(text.contains("blocked at [150017, 54, 149834] — dig that one first"), "{text}");
        assert!(text.contains("last walk: cannot reach"), "{text}");
        let err = CollectError::NotPickedUp(Box::new(failure));
        assert_eq!(err.blocked_at(), Some([150017, 54, 149834]));
        assert_eq!(err.last_walk(), Some("cannot reach Near([150018, 53, 149834], 0.7)"));
        // The old action-level error keeps its wording, so existing callers still classify it the same.
        let action = err.into_action();
        assert!(matches!(action, ActionError::Rejected(_)));
        assert!(action.to_string().contains("the drop (entity 2675,"), "{action}");
    }

    #[test]
    fn a_wall_between_the_bot_and_its_drop_needs_digging() {
        use acacia_world::{BlockIds, BlockRegistry, ChunkView, World};

        use crate::pathfind::{Goal, SearchOpts, Terrain, search};

        let registry = BlockRegistry::vanilla_arc();
        let mut view = ChunkView::new(World::new(registry.clone(), 0, BlockIds::Runtime));
        for cx in -1..=1 {
            for cz in -1..=1 {
                view.insert_sub_chunk(cx, 0, cz, &[8, 0]).expect("empty section");
            }
        }
        let stone = registry.states_of("minecraft:stone").next().expect("stone").0;
        for x in -2..=6 {
            for z in -2..=2 {
                view.set_block(x, 0, z, 0, stone);
            }
        }
        // A full width wall two blocks high at x = 3: no way round, no way over without digging.
        for z in -2..=2 {
            for y in 1..=2 {
                view.set_block(3, y, z, 0, stone);
            }
        }
        let terrain = Terrain::new(&view, &registry);
        let (start, goal) = ([0.5, 1.0, 0.5], Goal::Block([6, 1, 0]));

        let mut walk_only = SearchOpts::default();
        walk_only.allow_dig = false;
        let refused = search(&terrain, start, &goal, &walk_only);
        assert!(!refused.complete, "a two block wall is not walked through");

        let mut digging = SearchOpts::default();
        digging.allow_dig = true;
        let planned = search(&terrain, start, &goal, &digging);
        assert!(planned.complete, "with digging the route opens");
        assert!(planned.nodes.iter().any(|n| n.pos[0] == 3), "the path goes through the dug wall");
    }
}
