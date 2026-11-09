//! D* Lite over the **classic** move graph (t145).
//!
//! ## What this searches, and what it does not
//!
//! Only [`moves::successors`]: walking, one-block steps, breaking, bridging, doors. The macro moves (`macros::successors`
//! — leaps, sprint hops, shortcut runs) are **not** searched here, and cannot be: a leap is solved *when the node is
//! popped* (`frontier::LeapCand` → `solve::Solver::solve`, a physics rollout), so the edge does not exist until the
//! search has already decided to look at that node. An incremental search has to be able to enumerate a node's
//! predecessors, and an edge that is only discovered on pop cannot be enumerated backwards.
//!
//! Two consequences, both deliberate and both visible to callers:
//!
//! 1. A path found here may cost **more** than the same search with macros on (A* crosses a two-block gap by a leap;
//!    this engine has to walk around it). We never fake macro edges to make the numbers match.
//! 2. A goal that is only reachable through a macro looks **unreachable** here. [`plan_with_fallback`] is what turns
//!    that into an A* retry with macros, and records why in [`PlanStats::engine_used`] / `fallback_reason`.
//!
//! ## The variant
//!
//! Classical D* Lite roots `g`/`rhs` at a single goal cell and grows them backwards. A [`Goal`] here is a *region*
//! (`Block`, `Near(..)`, `XZ`, `Y`, `Away`) with no single root cell, and `Away` has no finite cell set at all — so the
//! roles are swapped: `g`/`rhs` are the cost **from the start**, the start is the one seeded cell (`rhs[start] = 0`),
//! and a goal cell is discovered when it is expanded (it gets no successors — it is where the search stops). `g`, `rhs`,
//! `km`, `ComputeShortestPath` and `UpdateVertex` are the textbook ones; `h` (the goal heuristic) only orders the queue,
//! which is what keeps it A*-shaped instead of Dijkstra-shaped.
//!
//! ## Reuse and invalidation (§3 of the design note)
//!
//! The table lives on the [`Bot`](crate::Bot) (`dstar`, next to `plan_stats`) and survives between searches. A search
//! reuses it only when the goal is the same, the start is within [`START_RESET`] of the last one and the table has not
//! grown past [`MAX_KNOWN`]; otherwise it is dropped and the search starts cold (`reused = false` in the report).
//!
//! Cells that changed are found by re-reading the `Spot` of every known node and comparing it with the one remembered
//! from the last search — a spot is computed from the five cells of its column, so a change anywhere under a known node
//! shows up here, and a changed node is exactly a node whose outgoing edges may have changed. Each such node gets
//! `UpdateVertex`, then `ComputeShortestPath` settles the rest. That is the "world changed a little, search a little"
//! case; a node whose spot no longer exists (a wall went up in it) is dropped from the table.
//!
//! Known gap, stated rather than hidden: a block placed in a cell the search never stood on *and* whose change does not
//! alter any known node's spot can change an edge's cost without being noticed here. The follower re-plans on `Stuck` /
//! `OffPath` (and the walk itself re-checks every step), so the cost of that gap is a stale plan for one plan-length,
//! not a walked-into wall.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use acacia_physics::{BlockPos, PlayerState};

use super::astar::start_node;
use super::goal::Goal;
use super::costs::CLASSIC_STARTUP;
use super::moves::{self, Ctx, Edge, MoveKind};
use super::node::{Entry, KeyMap, Momentum, NodeKey, run_entry};
use super::opts::{Path, PlanStats, SearchOpts};
use super::terrain::{Blocks, Spot, Terrain};
use super::PathNode;

/// The start may drift this far (blocks) before the table is dropped: `km` grows with every move, and a long walk
/// would otherwise carry a huge offset through every key.
const START_RESET: f32 = 8.0;
/// Past this many remembered nodes the table is dropped instead of grown: it is a cache, not a map.
const MAX_KNOWN: usize = 120_000;
/// More changed nodes than this in one reuse check: drop the table instead of repairing it.
const MAX_DIRTY: usize = 64;
/// Slower than this (blocks per tick) on the ground counts as standing (the same rule as A*).
const AT_REST: f32 = 0.05;

/// One entry of the open list; ordered so that a [`BinaryHeap`] pops the smallest `(k1, k2)` first.
#[derive(Clone, Copy, Debug)]
struct QItem {
    k1: f32,
    k2: f32,
    key: NodeKey,
}

impl PartialEq for QItem {
    fn eq(&self, other: &Self) -> bool {
        self.k1 == other.k1 && self.k2 == other.k2
    }
}

impl Eq for QItem {}

impl PartialOrd for QItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed: `BinaryHeap` is a max-heap, and the search wants the smallest key.
        other.k1.total_cmp(&self.k1).then_with(|| other.k2.total_cmp(&self.k2))
    }
}

/// Why the last search did not use the table it was given (`reused = false`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reset {
    /// No table yet.
    #[default]
    Cold,
    /// Another goal.
    Goal,
    /// The start moved further than [`START_RESET`].
    Start,
    /// Too many remembered nodes.
    Size,
    /// Too many cells changed at once to repair incrementally.
    World,
    /// A caller asked for a from-scratch search.
    Forced,
}

impl Reset {
    pub fn name(self) -> &'static str {
        match self {
            Reset::Cold => "cold",
            Reset::Goal => "goal_changed",
            Reset::Start => "start_moved",
            Reset::Size => "table_full",
            Reset::World => "world_changed",
            Reset::Forced => "forced",
        }
    }
}

/// The incremental table (see the module docs). Lives on the bot; [`DStarState::default`] is a cold one.
#[derive(Default)]
pub struct DStarState {
    /// Cost from the start, and the one-step lookahead D* Lite keeps next to it.
    g: KeyMap<f32>,
    rhs: KeyMap<f32>,
    /// The classic move chosen to leave a node (toward the goal), and the motion it arrives with.
    via: KeyMap<Edge>,
    /// The node that move comes from. `Edge` names where a step *ends*, so the parent has to be kept separately.
    parent: KeyMap<NodeKey>,
    /// Who can step *into* a node, and with which edge: `rhs` is a minimum over this list, so a repair costs the
    /// number of ways in rather than the size of the table. Filled while a node is expanded.
    preds: KeyMap<Vec<(NodeKey, Edge)>>,
    /// The edges a node was last seen to have. A changed cell has to have the edges on both sides refreshed.
    out: KeyMap<Vec<Edge>>,
    /// The spot each known node was last seen standing on: this is what makes "the world changed here" cheap.
    known: KeyMap<Spot>,
    open: BinaryHeap<QItem>,
    /// The `k1` a key was last queued with; a popped item whose key no longer matches is stale (lazy deletion).
    queued: KeyMap<f32>,
    /// The offset D* Lite adds to every key so that the queue stays valid while the start moves.
    km: f32,
    start: Option<BlockPos>,
    goal: Option<Goal>,
    /// What the last search did, for tests and for the report.
    pub last_reset: Reset,
    /// `true` when the last search reused the table.
    pub reused: bool,
    /// The best goal node found so far: the incumbent an A*-shaped search can stop against, and what makes a second
    /// search with nothing changed cost almost nothing.
    best_goal: Option<(f32, NodeKey)>,
    /// Searches served by this table.
    pub searches: u64,
}

impl DStarState {
    /// Drops everything: the next search starts cold.
    pub fn reset(&mut self) {
        self.g.clear();
        self.rhs.clear();
        self.via.clear();
        self.parent.clear();
        self.preds.clear();
        self.out.clear();
        self.known.clear();
        self.open.clear();
        self.queued.clear();
        self.km = 0.0;
        self.start = None;
        self.goal = None;
        self.best_goal = None;
    }

    /// The answer the table is holding: the nodes from the goal backwards to the start. Empty when there is none.
    fn chain(&self, start: NodeKey) -> Vec<NodeKey> {
        let mut out = Vec::new();
        let mut at = self.best_goal.map(|(_, k)| k);
        let mut seen = std::collections::HashSet::new();
        while let Some(k) = at {
            if k == start || !seen.insert(k) {
                break;
            }
            out.push(k);
            at = self.parent.get(&k).copied();
        }
        out
    }

    /// Nodes remembered (the cost of a reuse check is linear in this).
    pub fn known(&self) -> usize {
        self.known.len()
    }

    fn g_of(&self, k: NodeKey) -> f32 {
        self.g.get(&k).copied().unwrap_or(f32::INFINITY)
    }

    fn rhs_of(&self, k: NodeKey) -> f32 {
        self.rhs.get(&k).copied().unwrap_or(f32::INFINITY)
    }

    fn set_g(&mut self, k: NodeKey, v: f32) {
        if v.is_infinite() {
            self.g.remove(&k);
        } else {
            self.g.insert(k, v);
        }
    }

    fn set_rhs(&mut self, k: NodeKey, v: f32) {
        if v.is_infinite() {
            self.rhs.remove(&k);
        } else {
            self.rhs.insert(k, v);
        }
    }

    fn key_of(&self, k: NodeKey, h: f32) -> (f32, f32) {
        let m = self.g_of(k).min(self.rhs_of(k));
        (m + h + self.km, m)
    }

    fn push(&mut self, k: NodeKey, k1: f32, k2: f32) {
        self.open.push(QItem { k1, k2, key: k });
        self.queued.insert(k, k1);
    }

    /// Pops the smallest live item, dropping stale duplicates (an entry whose key was replaced by a better one).
    fn pop(&mut self) -> Option<NodeKey> {
        while let Some(item) = self.open.pop() {
            match self.queued.get(&item.key) {
                Some(&k1) if k1 == item.k1 => {
                    self.queued.remove(&item.key);
                    return Some(item.key);
                }
                _ => continue,
            }
        }
        None
    }
}

/// Plans with the engine [`SearchOpts::algorithm`] asks for, and retries **once** with A* when D* Lite cannot get
/// there (t145): D* Lite searches the classic graph only, so a goal behind a leap looks unreachable to it and an A*
/// retry — which has the macros — is the honest answer rather than a refusal.
///
/// The swap is never silent: the returned path's [`PlanStats::engine_used`] names the engine that produced it and
/// [`PlanStats::fallback_reason`] says why the first one was dropped (`no_path` or `budget`).
pub fn search_engine<B: Blocks>(
    t: &Terrain<B>,
    live: &PlayerState,
    goal: &Goal,
    opts: &SearchOpts,
    cache: &super::cache::RolloutCache,
    state: Option<&std::sync::Mutex<Option<DStarState>>>,
) -> Path {
    if opts.algorithm == super::opts::Algorithm::AStar {
        return super::astar::search_cached(t, live, goal, opts, cache);
    }
    // Dynamic threats are priced by whoever is chasing the walker, not by the terrain: `hazards.edge(from, to)` is
    // re-evaluated every tick, and `favour` bends the estimate towards where the bot wants to go *now*. A table that
    // froze either would describe a world that never existed, so those searches go straight to A* — the one thing a
    // cache must not do is make a stale answer look authoritative.
    // Two different reasons can make a search uncacheable, and they are handled differently by whoever reads the
    // trace: `hazards` is by design (a threat's cost is re-derived every tick), `favour` is our own optimisation after
    // a dropped follower. `hazards` wins when both are present, because `favour` only bends the estimate on top of it.
    let dynamic = if opts.hazards.is_some() {
        Some("hazards")
    } else if opts.favour.is_some() {
        Some("favour")
    } else {
        None
    };
    if let Some(why) = dynamic {
        let mut path = super::astar::search_cached(t, live, goal, opts, cache);
        path.plan.engine_used = PlanStats::ASTAR;
        path.plan.fallback_reason = Some("dynamic");
        // The table was never consulted, so this is the reason *why*, not a retry after a D* Lite failure (t153).
        path.plan.dynamic = Some(why);
        // This path came out of A*: no table was read, so it must not inherit a `reused` from the previous walk.
        path.plan.reused = false;
        return path;
    }
    let mut guard = state.and_then(|m| m.lock().ok());
    let first = match guard.as_mut() {
        Some(slot) => plan_with(t, live, goal, opts, slot.get_or_insert_with(DStarState::default)),
        None => plan(t, live, goal, opts),
    };
    if !(first.unreachable || first.plan.budget_exhausted) {
        return first;
    }
    let why = if first.plan.budget_exhausted { "budget" } else { "no_path" };
    let mut retry = super::astar::search_cached(t, live, goal, opts, cache);
    retry.plan.engine_used = PlanStats::ASTAR;
    retry.plan.fallback_reason = Some(why);
    // `reused` describes the search that produced the answer, and this one did not use the table.
    retry.plan.reused = false;
    // `dynamic` stays `None` here (t153): the table *was* tried — this is a retry after D* Lite gave up, which is what
    // `fallback_reason` says. Only "never tried" belongs in `dynamic`.
    retry
}

/// A cold search with a fresh table.
pub fn plan<B: Blocks>(t: &Terrain<B>, live: &PlayerState, goal: &Goal, opts: &SearchOpts) -> Path {
    let mut state = DStarState::default();
    plan_with(t, live, goal, opts, &mut state)
}

/// One search, reusing `state` when it is still valid (see the module docs).
pub fn plan_with<B: Blocks>(t: &Terrain<B>, live: &PlayerState, goal: &Goal, opts: &SearchOpts, state: &mut DStarState) -> Path {
    let stats = |nodes: usize, cut: bool, reached: bool, short: Option<f32>| PlanStats {
        nodes_expanded: nodes,
        max_nodes: opts.max_nodes,
        budget_exhausted: cut,
        target_region_reached: reached,
        closest_short: short,
        engine_used: PlanStats::DSTAR,
        fallback_reason: None,
        cost: 0.0,
        reused: false,
        dynamic: None,
    };
    let empty = |complete, stats: PlanStats| Path {
        nodes: Vec::new(),
        complete,
        unreachable: false,
        cost: 0.0,
        visited: stats.nodes_expanded,
        macro_stats: Default::default(),
        plan: stats,
    };
    let Some((pos, spot)) = start_node(t, live.pos) else {
        return empty(false, stats(0, false, false, None));
    };
    if goal.success(pos) {
        return empty(true, stats(0, false, true, Some(0.0)));
    }
    let entry = match live.on_ground {
        false => Entry::Air,
        true if live.vel[0].hypot(live.vel[2]) < AT_REST => Entry::Rest,
        true => run_entry(live.vel),
    };
    let start = NodeKey { entry, ..NodeKey::rest(pos) };

    // Reuse, or start over. A table is only worth keeping while it describes the same question from nearly the same
    // place (the module docs have the reasoning; `Reset` names the outcome for the report and the tests).
    let mut reset = match (&state.start, &state.goal) {
        (Some(_), Some(g)) if g == goal => None,
        (Some(_), Some(_)) => Some(Reset::Goal),
        // A table with no question attached is not a table we can reuse.
        (Some(_), None) | (None, Some(_)) => Some(Reset::Cold),
        (None, None) => Some(Reset::Cold),
    };
    if reset.is_none() && state.known.len() > MAX_KNOWN {
        reset = Some(Reset::Size);
    }
    if reset.is_none() {
        let last = state.start.expect("checked above");
        if horizontal(pos, last) > START_RESET {
            reset = Some(Reset::Start);
        }
    }
    if let Some(why) = reset {
        state.reset();
        state.last_reset = why;
        state.reused = false;
    } else {
        state.reused = true;
        state.last_reset = Reset::Cold;
        // The robot moved since the last search: D* Lite's `km` keeps the old keys ordered against the new ones.
        if let Some(last) = state.start {
            let d = goal.estimate(last, opts.speed(), false) - goal.estimate(pos, opts.speed(), false);
            state.km += d.max(0.0);
        }
    }
    state.searches += 1;
    state.start = Some(pos);
    state.goal = Some(goal.clone());

    let speed = opts.speed();
    let ctx = Ctx {
        parkour: opts.parkour,
        dig: opts.allow_dig,
        bridge: opts.allow_bridge,
        doors: opts.open_doors,
        kit: &opts.kit,
        blocks_left: 0,
        // Classic moves only: the macros are solved on pop and cannot be enumerated backwards (module docs).
        macros: None,
        keep: opts.keep.as_deref(),
        no_body: opts.no_body.as_deref(),
        boats: None,
    };
    // What changed since the last search: a known node whose spot is not the one we remembered (or is no longer
    // standable). Only the cells on the answer we are holding can change that answer.
    if state.reused {
        // What changed is not just "a cell we remembered moved": taking a wall away creates places that were never in
        // `known` at all, and the only way to see that is to ask the terrain again. Re-deriving every known node's edges
        // is the honest version of "did anything I planned on move", and it is still far cheaper than a cold search —
        // what a repair keeps is the settled `g` values, not the successor lists.
        let at_rest = live.vel[0].hypot(live.vel[2]) < AT_REST;
        let keys: Vec<NodeKey> = state.known.keys().copied().collect();
        let mut dirty: Vec<NodeKey> = Vec::new();
        for k in keys {
            let was = state.known.get(&k).copied();
            match (was, t.spot(k.pos)) {
                (Some(was), Some(now)) if same_spot(was, now) => {}
                (_, Some(now)) => {
                    state.known.insert(k, now);
                    dirty.push(k);
                }
                (_, None) => {
                    dirty.push(k);
                    continue;
                }
            }
            let before = state.out.get(&k).cloned().unwrap_or_default();
            let targets = refresh(state, t, &ctx, k, k == start && at_rest);
            let after = state.out.get(&k).cloned().unwrap_or_default();
            if !same_edges(&before, &after) {
                // Only the moves that actually changed cost anything to repair: re-deriving an unchanged list is
                // silent, so a search over a world nobody touched still expands nothing.
                dirty.push(k);
                for to in targets {
                    update_vertex(state, to, start, speed, goal);
                }
            }
        }
        if dirty.len() > MAX_DIRTY {
            // Large changes are not "a little incremental work": the table is cheaper to drop than to repair.
            state.reset();
            state.last_reset = Reset::World;
            state.reused = false;
            state.start = Some(pos);
            state.goal = Some(goal.clone());
        } else if !dirty.is_empty() {
            // `repair`, not `reset`: keep `g`/`rhs`/`km` and re-derive only the costs the change reaches.
            for cell in dirty {
                repair(state, t, &ctx, cell, start, speed, goal);
            }
        }
    }

    // Seed the start: `rhs` is "the best one-step lookahead", and for the root it is 0 by definition.
    state.known.insert(start, spot);
    // The root of the search: cost so far is zero, and there is no way into it to look ahead from.
    state.set_g(start, 0.0);
    state.set_rhs(start, 0.0);
    update_vertex(state, start, start, speed, goal);
    // The root is consistent by construction (`g == rhs == 0`), so `UpdateVertex` has no reason to queue it: it is
    // the one node that has to be pushed by hand, or the search would begin with an empty queue.
    {
        let h = goal.estimate(start.pos, speed, start.entry == Entry::Rest);
        let (k1, k2) = state.key_of(start, h);
        state.push(start, k1, k2);
    }

    let (mut expanded, mut cut) = (0usize, false);
    let mut closest: Option<f32> = None;
    let mut found: Option<NodeKey> = None;
    while let Some(u) = state.pop() {
        let h = goal.estimate(u.pos, speed, u.entry == Entry::Rest);
        let (k1, k2) = state.key_of(u, h);
        let g = state.g_of(u);
        let rhs = state.rhs_of(u);
        // Nothing left in the queue can beat what we already hold: stop (this is what makes an unchanged re-search
        // cost almost nothing, and it is the same rule A* uses with an incumbent).
        if let Some((best, _)) = state.best_goal
            && k1 >= best + state.km
        {
            state.push(u, k1, k2);
            break;
        }
        if g < rhs {
            // A cheaper way in was found after this entry was queued: the newer entry is still in the heap.
            continue;
        }
        // `g > rhs` (or no `g` at all): the node has never been settled. Its `rhs` is the best one-step lookahead,
        // so that is its cost — settling here is what makes the queue drain instead of re-raising the same node.
        if g > rhs {
            state.set_g(u, rhs);
            update_vertex(state, u, start, speed, goal);
        }
        let g = if g > rhs { rhs } else { g };
        if let Some(d) = goal.region_distance(u.pos) {
            closest = Some(closest.map_or(d, |c: f32| c.min(d)));
        }
        if goal.success(u.pos) && g.is_finite() {
            // A goal is an answer, not a place to search from: keep the cheapest one and let the queue prove it.
            if state.best_goal.is_none_or(|(best, _)| g < best) {
                state.best_goal = Some((g, u));
            }
            found = Some(u);
            continue;
        }
        expanded += 1;
        if expanded >= opts.max_nodes {
            cut = true;
            break;
        }
        // Settle this node and relax its successors: the classic D* Lite update, with the roles of start and goal
        // swapped (module docs).
        let spot = state.known.get(&u).copied().or_else(|| t.spot(u.pos));
        let Some(spot) = spot else { continue };
        let mom = state.via.get(&u).map(|e| e.mom).unwrap_or_else(|| Momentum::snap(live, u.pos));
        let mut out: Vec<Edge> = Vec::new();
        moves::successors(t, u.pos, spot, (u.entry, &mom), &ctx, &mut out);
        // Same price list as A* (`astar.rs`): leaving the start from standing pays the run-up on every classic walk, so
        // the two engines compare on one cost model rather than two.
        if u == start && live.vel[0].hypot(live.vel[2]) < AT_REST {
            let walks = |e: &&mut Edge| {
                e.mac.is_none()
                    && e.work.is_empty()
                    && !e.spot.wet
                    && matches!(e.kind, MoveKind::Walk | MoveKind::Ascend | MoveKind::Descend | MoveKind::Parkour)
            };
            out.iter_mut().filter(walks).for_each(|e| e.cost += CLASSIC_STARTUP);
        }
        state.out.insert(u, out.clone());
        for e in out {
            let to = NodeKey { entry: e.entry, ..NodeKey::rest(e.pos) };
            if !state.known.contains_key(&to) {
                // A work edge (dig/bridge) ends somewhere the world does not let you stand **yet** — it promises the
                // spot the move creates, and the edge carries it. Asking the terrain here would throw those edges away:
                // that is why this engine used to stop dead at the first wall it could have dug through (t146).
                let sp = t.spot(e.pos).unwrap_or(e.spot);
                state.known.insert(to, sp);
            }
            // Who can step into `to`, so that a repair never has to re-run every successor of every known node.
            let into = state.preds.entry(to).or_default();
            into.retain(|&(p, _)| p != u);
            into.push((u, e));
            if g + e.cost < state.rhs_of(to) {
                state.via.insert(to, e);
                state.parent.insert(to, u);
                // The goal node is an answer, not a place to expand: notice it here, so a repair that changes its cost
                // does not have to pop it again to be believed.
                if goal.success(to.pos) && g + e.cost < state.best_goal.map_or(f32::INFINITY, |(b, _)| b) {
                    state.best_goal = Some((g + e.cost, to));
                }
                update_vertex(state, to, start, speed, goal);
            }
        }
    }

    if let Some(best) = state.best_goal {
        found = Some(best.1);
    }
    // A repair can also make the held answer go away (a wall where it used to walk). The chain is checked against the
    // edges we now believe in, so a stale path is reported as "no answer" rather than handed to the walker.
    if found.is_some_and(|k| !chain_stands(state, start, k)) {
        found = None;
        state.best_goal = None;
    }
    let nodes = found.map(|goal_node| rebuild(state, start, goal_node)).unwrap_or_default();
    let reached = found.is_some();
    // A goal found by relaxation has `rhs` set and `g` not yet settled: its price is whichever is finite.
    let cost = found.map_or(0.0, |k| state.g_of(k).min(state.rhs_of(k)));
    let unreachable = !reached && !cut;
    let mut path = Path {
        nodes,
        complete: reached,
        unreachable,
        cost,
        visited: expanded,
        macro_stats: Default::default(),
        plan: stats(expanded, cut, reached, closest),
    };
    // What the caller compares between two plans (t150), and whether this search was incremental: read from the table
    // *here*, because an unreachable answer drops it a few lines down and `reused` would then describe the reset.
    path.plan.cost = path.cost;
    path.plan.reused = state.reused;
    // The table only pays for itself while it describes a path that still exists.
    if unreachable {
        state.reset();
    }
    if path.nodes.is_empty() && !reached {
        path.complete = false;
    }
    path
}

/// `true` when two reads of the same column describe the same place (the fields `moves` reads a spot for).
fn same_spot(a: Spot, b: Spot) -> bool {
    a.feet.to_bits() == b.feet.to_bits() && a.kind == b.kind && a.wet == b.wet
}

fn horizontal(a: BlockPos, b: BlockPos) -> f32 {
    let (dx, dz) = ((a[0] - b[0]) as f32, (a[2] - b[2]) as f32);
    dx.hypot(dz)
}

/// One changed cell, repaired in place: drop it if it is no longer a place to stand, otherwise take the terrain's word
/// for its spot and re-price the ways into it. Its own edges were refreshed by the caller, which is the only place that
/// can tell "the same moves, re-derived" from "the world actually changed here".
fn repair<B: Blocks>(
    state: &mut DStarState,
    t: &Terrain<B>,
    _ctx: &Ctx<'_>,
    cell: NodeKey,
    start: NodeKey,
    speed: super::goal::Speed,
    goal: &Goal,
) {
    match t.spot(cell.pos) {
        Some(now) => {
            state.known.insert(cell, now);
        }
        None => {
            // No longer a place to stand: it keeps no cost, and `rhs` tells whoever pointed at it.
            state.known.remove(&cell);
            state.set_g(cell, f32::INFINITY);
            state.set_rhs(cell, f32::INFINITY);
            state.via.remove(&cell);
            state.parent.remove(&cell);
        }
    }
    update_vertex(state, cell, start, speed, goal);
}

/// Re-derives the edges out of `p`, rewrites its rows in `preds`, and returns where they lead.
fn refresh<B: Blocks>(
    state: &mut DStarState,
    t: &Terrain<B>,
    ctx: &Ctx<'_>,
    p: NodeKey,
    startup: bool,
) -> Vec<NodeKey> {
    let Some(spot) = state.known.get(&p).copied() else { return Vec::new() };
    let mom = state.via.get(&p).map(|e| e.mom).unwrap_or_default();
    let mut out: Vec<Edge> = Vec::new();
    moves::successors(t, p.pos, spot, (p.entry, &mom), ctx, &mut out);
    if startup {
        // Leaving the start from standing pays the run-up, exactly as the expansion loop prices it: a re-derivation
        // that dropped this would hand back an answer four points cheaper than the one A* would give.
        let walks = |e: &&mut Edge| {
            e.mac.is_none()
                && e.work.is_empty()
                && !e.spot.wet
                && matches!(e.kind, MoveKind::Walk | MoveKind::Ascend | MoveKind::Descend | MoveKind::Parkour)
        };
        out.iter_mut().filter(walks).for_each(|e| e.cost += CLASSIC_STARTUP);
    }
    let mut targets = Vec::with_capacity(out.len());
    for e in out.iter() {
        let to = NodeKey { entry: e.entry, ..NodeKey::rest(e.pos) };
        let into = state.preds.entry(to).or_default();
        into.retain(|&(m, _)| m != p);
        into.push((p, *e));
        targets.push(to);
    }
    state.out.insert(p, out);
    targets
}

/// Whether two successor lists describe the same moves (order is not part of that). Edge lists are a handful of items,
/// so the pairwise comparison is cheaper than building a key type for them.
fn same_edges(a: &[Edge], b: &[Edge]) -> bool {
    fn same(x: &Edge, y: &Edge) -> bool {
        x.pos == y.pos
            && x.entry == y.entry
            && x.kind == y.kind
            && x.cost.to_bits() == y.cost.to_bits()
            && x.work.steps().eq(y.work.steps())
    }
    a.len() == b.len() && a.iter().all(|x| b.iter().any(|y| same(x, y)))
}

/// Whether every step of the held answer still exists in the table's edges.
fn chain_stands(state: &DStarState, start: NodeKey, goal: NodeKey) -> bool {
    let mut at = goal;
    let mut hops = 0usize;
    while at != start {
        let Some(&prev) = state.parent.get(&at) else { return false };
        let Some(out) = state.out.get(&prev) else { return false };
        if !out.iter().any(|e| e.pos == at.pos && e.entry == at.entry) {
            return false;
        }
        at = prev;
        hops += 1;
        if hops > 10_000 {
            return false;
        }
    }
    true
}

fn chebyshev(a: BlockPos, b: BlockPos) -> i32 {
    (a[0] - b[0]).abs().max((a[1] - b[1]).abs()).max((a[2] - b[2]).abs())
}

/// D* Lite's `UpdateVertex`: a node's `rhs` is the cheapest way into it from a settled predecessor. The ways in are kept
/// as they are discovered (`preds`, filled while expanding), so this costs the node's in-degree — not the table.
fn update_vertex(
    state: &mut DStarState,
    k: NodeKey,
    start: NodeKey,
    speed: super::goal::Speed,
    goal: &Goal,
) {
    if k != start {
        // `rhs` is the cheapest way *into* `k`: a minimum over the ways in, not over the table.
        let mut best = f32::INFINITY;
        let mut best_edge: Option<(NodeKey, Edge)> = None;
        if let Some(into) = state.preds.get(&k) {
            for &(m, e) in into {
                // A way in from the node itself is a cycle, and cycles are never cheaper than zero: skipping it keeps
                // `via` a tree (the parent chain is what `rebuild` walks).
                if m == k {
                    continue;
                }
                let g = state.g_of(m);
                if !g.is_finite() {
                    continue;
                }
                if g + e.cost < best {
                    best = g + e.cost;
                    best_edge = Some((m, e));
                }
            }
        }
        state.set_rhs(k, best);
        match best_edge {
            Some((m, e)) => {
                state.via.insert(k, e);
                state.parent.insert(k, m);
            }
            None => {
                state.via.remove(&k);
                state.parent.remove(&k);
            }
        }
    }
    let g = state.g_of(k);
    let rhs = state.rhs_of(k);
    if g != rhs {
        let h = goal.estimate(k.pos, speed, k.entry == Entry::Rest);
        let (k1, k2) = state.key_of(k, h);
        state.push(k, k1, k2);
    }
}

/// Walks the parents from the goal node back to the start and returns the path in walking order.
fn rebuild(state: &DStarState, start: NodeKey, goal_node: NodeKey) -> Vec<PathNode> {
    let mut chain: Vec<(NodeKey, Edge)> = Vec::new();
    let mut at = goal_node;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..10_000 {
        if at == start || !seen.insert(at) {
            break;
        }
        let Some(e) = state.via.get(&at).copied() else { break };
        let Some(prev) = state.parent.get(&at).copied() else { break };
        chain.push((at, e));
        at = prev;
    }
    chain.reverse();
    chain
        .into_iter()
        .map(|(k, e)| PathNode {
            pos: k.pos,
            feet: e.spot.feet,
            kind: e.kind,
            wet: e.spot.wet,
            work: e.work,
            entry: e.entry,
            mom: e.mom,
            mac: None,
            cost: e.cost,
        })
        .collect()
}
