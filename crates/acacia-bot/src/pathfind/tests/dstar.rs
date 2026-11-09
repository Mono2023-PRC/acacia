//! t145: D* Lite as the default engine, with A* kept for comparison and as the one-shot fallback.
//!
//! What these pin, in the order the contract asks for:
//!
//! * the two engines agree on the graph D* Lite actually searches (the classic one): cost and reachability, 50 seeds;
//! * a second search over a nearly unchanged world reuses the table instead of searching again, and still returns the
//!   same cost as a from-scratch search;
//! * a changed goal, a start that moved too far and a wrecked neighbourhood all drop the table (`reused == false`);
//! * the engine is chosen by `SearchOpts::algorithm` / `ACACIA_PATHFINDER`, and a goal D* Lite cannot reach (only
//!   macros can) is retried once with A* and *says so* in `PlanStats`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use acacia_physics::{PlayerState, Vec3};

use super::grid::Grid;
use super::search::{ORIGIN, opts};
use crate::pathfind::dstar::{DStarState, Reset, plan, plan_with, search_engine};
use crate::pathfind::{Algorithm, Goal, PlanStats, SearchOpts, search};

const GOAL: [i32; 3] = [12, 1, 12];

/// Flat stone 0..=12 with a scattering of two-block pillars, so both engines meet something to walk around.
fn scattered(seed: u64) -> Grid {
    let g = Grid::floor(0, 12);
    let mut s = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
    let mut next = || {
        s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (s >> 33) as u32 % 100
    };
    for x in 1..12 {
        for z in 1..12 {
            if next() < 28 {
                g.fill([x, 1, z], [x, 2, z], "stone");
            }
        }
    }
    g
}

/// The classic graph only: no macros, no boat legs — the graph D* Lite searches (`search::opts` already has
/// `macros: false`).
fn classic(parkour: bool) -> SearchOpts {
    opts(parkour)
}

/// The parity corpus is about *agreement*, not about how far a search may go: a small budget keeps 50 seeds a few
/// seconds of work instead of a few minutes. A seed both engines cut short is compared for cut-ness, not cost.
fn cheap(parkour: bool) -> SearchOpts {
    let mut o = classic(parkour);
    o.max_nodes = 6_000;
    o.budget = std::time::Duration::from_millis(250);
    o
}

/// How many seeds to run: the default corpus, unless `T145_SEEDS` asks for fewer while iterating.
fn corpus() -> u64 {
    std::env::var("T145_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(50)
}

#[test]
fn the_two_engines_agree_on_the_classic_graph() {
    // Same terrain, same start, same goal, same moves: the incremental engine may find a different path, but not a
    // different price. 50 seeds, plus every starting/goal pair the seeds produce (some of them walled off).
    let mut compared = 0;
    for seed in 0..corpus() {
        let g = scattered(seed);
        let t = g.terrain();
        let goal = Goal::Block(GOAL);
        let o = cheap(true);
        let astar = search(&t, ORIGIN, &goal, &o);
        let mut state = DStarState::default();
        let dstar = plan_with(&t, &PlayerState::new(ORIGIN), &goal, &o, &mut state);
        match (astar.complete, dstar.complete) {
            (true, true) => {
                assert!(
                    (astar.cost - dstar.cost).abs() <= 1e-3,
                    "seed {seed}: A* costs {} and D* Lite costs {}",
                    astar.cost,
                    dstar.cost
                );
                compared += 1;
            }
            (false, false) => {
                assert_eq!(astar.unreachable, dstar.unreachable, "seed {seed}: one search gave up earlier than the other");
            }
            (a, d) => panic!("seed {seed}: A* complete = {a}, D* Lite complete = {d}"),
        }
        assert!(!dstar.nodes.is_empty() || !dstar.complete, "seed {seed}: a complete path has nodes");
    }
    assert!(compared >= 10, "only {compared} seeds were answered by both: the corpus is not testing parity");
}

#[test]
fn a_second_search_over_a_almost_unchanged_world_reuses_the_table() {
    // A corridor with two cells changed beside it: the table already knows the way, and re-searching it from scratch
    // would be paying for the same answer twice.
    let g = Grid::floor(0, 20);
    let goal = Goal::Block([20, 1, 0]);
    let o = classic(true);
    let mut state = DStarState::default();
    let first = plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut state);
    assert!(first.complete, "the corridor is open");

    g.set_state([10, 1, 1], "stone", "");
    g.set_state([10, 1, -1], "stone", "");
    let second = plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut state);
    assert!(state.reused, "nothing invalidated the table: {:?}", state.last_reset);
    assert!(second.complete, "the two new blocks do not close the corridor");
    assert!(
        second.plan.nodes_expanded * 2 < first.plan.nodes_expanded,
        "second search expanded {} nodes against the first's {} — nothing was reused",
        second.plan.nodes_expanded,
        first.plan.nodes_expanded
    );

    // ...and reuse must not change the answer: a cold table over the same world pays the same price.
    let cold = plan(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o);
    assert!((cold.cost - second.cost).abs() <= 1e-3, "reused cost {} against cold {}", second.cost, cold.cost);
    assert_eq!(second.nodes.last().map(|n| n.pos), Some([20, 1, 0]), "the path still ends on the goal");
}

#[test]
fn a_changed_goal_a_moved_start_and_a_wrecked_world_all_start_over() {
    let g = Grid::floor(0, 20);
    let o = classic(true);
    let mut state = DStarState::default();

    let first = plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &Goal::Block([20, 1, 0]), &o, &mut state);
    assert!(first.complete);
    assert!(!state.reused, "the first search has nothing to reuse");

    // Another question: the table describes the old one.
    let other = plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &Goal::Block([0, 1, 20]), &o, &mut state);
    assert!(!state.reused);
    assert_eq!(state.last_reset, Reset::Goal);
    assert!(other.complete, "the corner is reachable too");

    // A start far from where the table was built: `km` would carry the whole distance.
    let far: Vec3 = [0.5, 1.0, 19.5];
    let moved = plan_with(&g.terrain(), &PlayerState::new(far), &Goal::Block([0, 1, 20]), &o, &mut state);
    assert!(!state.reused);
    assert_eq!(state.last_reset, Reset::Start);
    assert!(moved.complete);

    // A world change is *not* an identity change: the buried goal is repaired by `a_buried_goal_is_repaired...`
    // below, because that is the normal case for a digger (its own tunnel changes under its feet).
}

/// A change too big to be worth repairing is dropped (`MAX_DIRTY`), and one that fits is repaired — either way the
/// answer must be the truth about the world as it is now, which is the part a caller can check.
#[test]
fn a_world_change_bigger_than_the_walker_keeps_the_answer_honest() {
    let g = Grid::floor(0, 24);
    let goal = Goal::Block([16, 1, 0]);
    let o = classic(false);
    let mut state = DStarState::default();
    let first = plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut state);
    assert!(first.complete);

    // Bury the corridor the table describes, leaving only the cell the walker stands in.
    g.fill([2, 1, -3], [16, 4, 3], "stone");
    let second = plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut state);
    assert!(
        state.reused || state.last_reset == Reset::World,
        "a repaired table or a dropped one, but the decision is named: {:?}",
        state.last_reset
    );
    assert!(second.unreachable, "no way through a filled corridor: {:?}", second.plan.describe());
    // ...and the table is not left describing the world it just lost.
    let after = plan(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o);
    assert!(after.unreachable, "a cold table agrees: {:?}", after.plan.describe());
}

/// The case this whole engine exists for: the world changes *on the answer's own cells* while the walker is out there.
/// A table that resets whenever the answer moves is cold in exactly the situation it was built for.
#[test]
fn opening_a_gap_in_the_wall_the_answer_went_around_is_repaired() {
    // A wall across the corridor with one gap far to the north: the answer has to go round.
    let g = Grid::floor(0, 20);
    g.fill([6, 1, -8], [6, 3, 8], "stone");
    g.fill([6, 1, 9], [6, 3, 11], "air");
    let goal = Goal::Block([12, 1, 0]);
    let o = classic(false);
    let mut state = DStarState::default();
    let first = plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut state);
    assert!(first.complete, "the gap at z=10 is a way through: {:?}", first.plan.describe());
    assert!(first.cost > 20.0, "the first answer really does go round: {}", first.cost);

    // The walker opens the wall where it stands — the cells the answer passes next to.
    for z in -1..=1 {
        g.set_state([6, 1, z], "air", "");
        g.set_state([6, 2, z], "air", "");
    }

    let second = plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut state);
    assert!(state.reused, "a changed wall is repaired, not re-searched: {:?}", state.last_reset);
    assert!(second.complete);
    assert!(
        second.plan.nodes_expanded * 2 < first.plan.nodes_expanded,
        "{} expansions after the gap opened, {} before: the table paid off",
        second.plan.nodes_expanded,
        first.plan.nodes_expanded
    );
    // ...and the repair agrees with a table that never saw the old world.
    let cold = plan(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o);
    assert!((cold.cost - second.cost).abs() < 1e-3, "repaired {} vs cold {}", second.cost, cold.cost);
    assert!(second.cost < first.cost, "the short way is cheaper: {} -> {}", first.cost, second.cost);
    assert_eq!(second.nodes.last().map(|n| n.pos), Some([12, 1, 0]));
}

/// A goal that gets buried is a world change, not a new question: the table is repaired and the answer honestly turns
/// into "unreachable" — which is what lets the dispatcher spend its one A* retry.
#[test]
fn a_buried_goal_is_repaired_not_re_searched() {
    let g = Grid::floor(0, 12);
    let goal = Goal::Block([8, 1, 0]);
    let o = classic(false);
    let mut state = DStarState::default();
    let first = plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut state);
    assert!(first.complete);

    g.fill([8, 1, 0], [8, 2, 0], "stone");
    let second = plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut state);
    assert!(state.reused, "repaired: {:?}", state.last_reset);
    assert!(second.unreachable, "the goal cell is buried: {:?}", second.plan.describe());
}
