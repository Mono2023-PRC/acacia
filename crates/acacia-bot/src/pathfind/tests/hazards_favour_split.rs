//! `fallback=astar:dynamic` used to carry two very different situations in one value. `hazards` means the terrain
//! cannot be cached by design (a threat's cost is re-derived every tick); `favour` means *we* bent the search towards
//! the old path after a dropped follower. Whoever reads a `[plan ...]` line has to be able to tell them apart, and to
//! tell both from "the table was tried and gave up" (`fallback_reason`).

use std::sync::Arc;

use acacia_physics::PlayerState;

use super::grid::Grid;
use super::hazards::{field, threat};
use super::search::{opts, ORIGIN};
use crate::pathfind::dstar::search_engine;
use crate::pathfind::favour::Favour;
use crate::pathfind::hazard::Class;
use crate::pathfind::{Goal, Path, PlanStats, SearchOpts};

fn walk(g: &Grid, opts: &SearchOpts, hazards: Option<Arc<crate::pathfind::hazard::Hazards>>) -> Path {
    let terrain = g.terrain().with_hazards(hazards);
    search_engine(&terrain, &PlayerState::new(ORIGIN), &Goal::Block([8, 1, 0]), opts, &Default::default(), None)
}

#[test]
fn a_threat_in_reach_says_hazards_and_never_opened_the_table() {
    let g = Grid::floor(-4, 12);
    let base = opts(false);
    let threats = field(vec![threat(Class::Solid, [4.5, 1.0, 0.5])], base.profile);
    // The field goes in both places, exactly as the follower does it: the terrain prices the threat, the options say
    // "this search is dynamic".
    let o = SearchOpts { hazards: Some(threats.clone()), ..base };
    let walked = walk(&g, &o, Some(threats));
    assert_eq!(walked.plan.engine_used, PlanStats::ASTAR);
    assert_eq!(walked.plan.dynamic, Some("hazards"), "a threat is the reason the table was never used");
    assert_eq!(walked.plan.fallback_reason, Some("dynamic"), "the old field keeps its value: the rename must not move it");
}

#[test]
fn a_replan_bent_towards_the_old_path_says_favour() {
    let g = Grid::floor(-4, 12);
    let o = SearchOpts { favour: Some(Favour::new((0..=8).map(|x| [x, 1, 0])).into()), ..opts(false) };
    let walked = walk(&g, &o, None);
    assert_eq!(walked.plan.dynamic, Some("favour"), "our own optimising is not a threat");
    assert_eq!(walked.plan.engine_used, PlanStats::ASTAR);
    assert_eq!(walked.plan.fallback_reason, Some("dynamic"));
}

#[test]
fn an_ordinary_walk_opens_the_table_and_says_nothing() {
    let g = Grid::floor(-4, 12);
    let walked = walk(&g, &opts(false), None);
    assert!(walked.complete);
    assert_eq!(walked.plan.engine_used, PlanStats::DSTAR, "nothing dynamic: D* Lite does the work");
    assert_eq!(walked.plan.dynamic, None, "so there is no reason to report");
    assert_eq!(walked.plan.fallback_reason, None);
}
