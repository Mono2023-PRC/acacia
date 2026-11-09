//! What a search may do, and what it returns.

use std::sync::Arc;
use std::time::Duration;

use acacia_physics::BlockPos;

use super::boat::{BoatSource, HEURISTIC_FLOOR};
use super::costs;
use super::favour::Favour;
use super::goal::Speed;
use super::hazard::Hazards;
use super::kit::Kit;
use super::macros::MacroStats;
use super::profile::MovementProfile;
use super::PathNode;

#[derive(Debug, Clone)]
pub struct SearchOpts {
    pub max_nodes: usize,
    pub budget: Duration,
    /// Allow gap jumps.
    pub parkour: bool,
    /// Break blocks in the way (costed by break time with the best tool in `kit`).
    pub allow_dig: bool,
    /// Place scaffold blocks from `kit` to cross gaps and to pillar up.
    pub allow_bridge: bool,
    /// Open wooden doors, fence gates and trapdoors in the way.
    pub open_doors: bool,
    /// Tools and blocks to plan with; [`Bot::goto`](crate::Bot::goto) fills it from the inventory.
    pub kit: Kit,
    /// How macro moves are planned and run.
    pub profile: MovementProfile,
    /// Plan macro moves (leaps found by simulation) instead of the classic gap jump.
    pub macros: bool,
    /// Sprint-jump along flat ground (macro moves). [`Bot::goto`](crate::Bot::goto) turns it off at food 6 or less.
    pub hops: bool,
    /// Rollout ticks the search may simulate for macro moves; once spent, nodes get classic moves only.
    pub max_rollout_ticks: u64,
    /// Cells of the path being replaced, made cheaper to keep a re-plan on it; the navigator fills it.
    pub favour: Option<Arc<Favour>>,
    /// Cells the path's own work never places into or breaks: what a caller is building there (the builder:
    /// every cell its structure fills; live sim: a pillar through a torch's cell replaced the torch, again and
    /// again).
    pub keep: Option<Arc<std::collections::HashSet<BlockPos>>>,
    /// Cells no move may end with the body in (feet or head) or stand on: the builder's thin targets (panes, fences,
    /// walls), beside which a body fits but which then wall it in (live: a window cell, every walk "Stuck" after; a
    /// walk along placed pane tops slid off into one).
    pub no_body: Option<Arc<std::collections::HashSet<BlockPos>>>,
    /// Cross still water by boat, bank to bank (physics bots with the entity tracker).
    pub boats: bool,
    /// What a boat leg can board; [`Bot::goto`](crate::Bot::goto) fills it, and with nothing no boat leg is planned.
    pub boat_source: BoatSource,
    /// Entities to keep away from (score only) and the cells solid ones fill; attach the same to the search's
    /// terrain ([`Terrain::with_hazards`](super::Terrain::with_hazards)).
    pub hazards: Option<Arc<Hazards>>,
}

impl Default for SearchOpts {
    fn default() -> Self {
        Self {
            max_nodes: 200_000,
            budget: Duration::from_secs(2),
            parkour: true,
            allow_dig: false,
            allow_bridge: false,
            open_doors: true,
            kit: Kit::default(),
            profile: MovementProfile::Optimal,
            macros: true,
            hops: true,
            max_rollout_ticks: 400_000,
            favour: None,
            keep: None,
            no_body: None,
            boats: false,
            boat_source: BoatSource::default(),
            hazards: None,
        }
    }
}

impl SearchOpts {
    /// The heuristic's speed: macro moves sprint-jump, and change height on the way; a boat may cross water
    /// faster still (design decision 7).
    pub(crate) fn speed(&self) -> Speed {
        let mut s = if self.macros {
            Speed { per_block: costs::macro_ticks_per_block(self.profile), startup: costs::MACRO_STARTUP, overlap: true }
        } else {
            Speed::CLASSIC
        };
        if self.plans_boats() {
            s.per_block = s.per_block.min(HEURISTIC_FLOOR);
        }
        s
    }

    pub(crate) fn plans_boats(&self) -> bool {
        self.boats && self.boat_source.any()
    }
}

/// What a search covered, so a refusal can say which kind of refusal it is (t117).
///
/// A caller that gets "no path" cannot tell `the goal is walled off` from `the search stopped early` from the path
/// alone -- and those ask for opposite next moves (change the goal vs. raise the budget or split the trip). The
/// numbers here are the difference: `budget_exhausted` says whether the search stopped because it was told to, and
/// `target_region_reached` says whether it ever came near enough to the goal to have an opinion about it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanStats {
    /// Nodes taken off the frontier (the `visited` count of the search that produced this path).
    pub nodes_expanded: usize,
    /// The budget this search ran under ([`SearchOpts::max_nodes`] as it was then, not the default).
    pub max_nodes: usize,
    /// Stopped at `max_nodes` or the time budget instead of running out of places to look.
    pub budget_exhausted: bool,
    /// Some expanded node was inside the goal's neighbourhood (see [`Goal::region_distance`]).
    pub target_region_reached: bool,
    /// How many blocks short of that neighbourhood the closest expanded node was; `None` when the goal has no
    /// neighbourhood to measure against (`XZ`, `Y`, `Away`).
    pub closest_short: Option<f32>,
    /// What this path costs, in the units the search minimises — the number a caller compares between two plans,
    /// and the one that has to survive a repair (t150). A refused search prices what it did reach, so `0.0` means
    /// "nothing to price" (a plan that starts on the goal), never "free".
    pub cost: f32,
}

impl Default for PlanStats {
    /// A search that has not run yet: nothing expanded, nothing priced.
    fn default() -> Self {
        Self { nodes_expanded: 0, max_nodes: 0, budget_exhausted: false, target_region_reached: false, closest_short: None, cost: 0.0 }
    }
}

impl PlanStats {
    /// `true` when the search said everything it could: not cut short, and it did reach the goal's neighbourhood.
    pub fn conclusive(&self) -> bool {
        !self.budget_exhausted && self.target_region_reached
    }

    /// One line a message can carry: what was searched and what that leaves open.
    pub fn describe(&self) -> String {
        let mut text = format!("{} nodes of at most {}", self.nodes_expanded, self.max_nodes);
        if let Some(short) = self.closest_short {
            text.push_str(&format!(", closest {short:.1} blocks short of the goal"));
        }
        if self.budget_exhausted {
            text.push_str(" (search budget ran out)");
        }
        text
    }

    /// The same numbers as JSON, for a failure message that a tool layer parses instead of reading prose.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "nodes_expanded": self.nodes_expanded,
            "max_nodes": self.max_nodes,
            "budget_exhausted": self.budget_exhausted,
            "target_region_reached": self.target_region_reached,
            "closest_short": self.closest_short,
            "cost": self.cost,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Path {
    /// Nodes after the start, in order.
    pub nodes: Vec<PathNode>,
    /// False when the search ran out of budget or options: `nodes` then leads to the node that
    /// looked closest to the goal and still has a way on (never into a pit).
    pub complete: bool,
    /// The search saw every cell it could reach, all loaded, without finding the goal: no path exists
    /// with these moves (until the world changes).
    pub unreachable: bool,
    /// Estimated ticks (undiscounted by [`SearchOpts::favour`]).
    pub cost: f32,
    /// Nodes expanded.
    pub visited: usize,
    pub macro_stats: MacroStats,
    /// The same search, counted for a caller that has to explain the failure (t117).
    pub plan: PlanStats,
}
