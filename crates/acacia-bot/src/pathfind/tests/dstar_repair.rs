//! Digging your own answer apart is a *repair*, not a cold start.
//!
//! This was asked for and could not be had: with `allow_dig` the default engine never produced a plan of its own
//! (it fell back to A* after four expansions), so there was no table to repair. t146 fixed that, and this closes the
//! loop: plan a tunnel, actually take the two cells the plan says to break out of the world, and search again.
//!
//! A bot that digs is the case D* Lite exists for — every block it removes changes the map under its own answer. If
//! touching a cell on the answer reset the table, the engine would be cold at exactly the moment it should be warm.

use acacia_physics::{BlockPos, PlayerState};

use super::grid::Grid;
use super::search::{ORIGIN, opts};
use crate::pathfind::dstar::{self, DStarState, Reset};
use crate::pathfind::work::Step;
use crate::pathfind::{Goal, Path, SearchOpts, search};

/// A long corridor one cell wide, sealed by a two-high solid wall: the only way on is to break two cells.
///
/// It is deliberately long — a short corridor would let the first search finish in so few expansions that "half of
/// them" says nothing about reuse.
fn sealed_corridor() -> Grid {
    let g = Grid::floor(0, 30);
    g.fill([-2, 1, -1], [28, 3, -1], "stone");
    g.fill([-2, 1, 1], [28, 3, 1], "stone");
    g.fill([12, 1, -2], [12, 3, 2], "stone");
    g
}

fn digging() -> SearchOpts {
    SearchOpts { allow_dig: true, ..opts(false) }
}

fn break_cells(p: &Path) -> Vec<BlockPos> {
    p.nodes
        .iter()
        .flat_map(|n| n.work.steps())
        .filter_map(|s| match s {
            Step::Break(pos) => Some(pos),
            _ => None,
        })
        .collect()
}

#[test]
fn removing_two_cells_from_the_answer_is_repaired_not_re_searched() {
    let mut g = sealed_corridor();
    let goal = Goal::Block([20, 1, 0]);
    let o = digging();

    let mut state = DStarState::default();
    let first = dstar::plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut state);
    assert!(first.complete, "D* Lite digs the wall itself: {:?}", first.plan.describe());
    let cells = break_cells(&first);
    assert!(cells.len() >= 2, "the answer breaks two cells: {cells:?}");

    // Do the digging. A pathfind test has no server to press a button against, so the world edit *is* the observable
    // result of `Bot::break_block`: the two cells come from the plan's own `Work`, not from a hand-picked coordinate.
    for cell in cells.iter().take(2) {
        g.fill(*cell, *cell, "air");
    }

    let second = dstar::plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut state);
    assert!(second.complete, "after digging: {:?}", second.plan.describe());
    assert!(state.reused, "the table is repaired, not re-searched: {:?}", state.last_reset);
    assert_ne!(state.last_reset, Reset::World, "a dug cell is a repair, never a reset: {:?}", state.last_reset);
    assert!(
        second.plan.nodes_expanded * 2 < first.plan.nodes_expanded,
        "repair expands {} nodes, the first search expanded {}: {:?}",
        second.plan.nodes_expanded,
        first.plan.nodes_expanded,
        second.plan.describe()
    );

    println!(
        "expansions: first {} -> repair {} (cost {} -> {})",
        first.plan.nodes_expanded, second.plan.nodes_expanded, first.cost, second.cost
    );

    // Same terrain, cold table: the repaired answer has to price the world exactly as a full search does.
    let cold = dstar::plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut DStarState::default());
    assert!(cold.complete, "cold: {:?}", cold.plan.describe());
    assert_eq!(cold.cost, second.cost, "repair priced {} but a cold search prices {}", second.cost, cold.cost);

    // And the other engine agrees on that price — the engines-agree invariant, kept honest here after a real world change.
    let star = search(&g.terrain(), ORIGIN, &goal, &o);
    assert!(star.complete, "A*: {:?}", star.plan.describe());
    assert_eq!(star.cost, second.cost, "A* prices {} on the dug terrain, D* Lite {}", star.cost, second.cost);

    // The tunnel we dug is on the answer, so the repaired path still breaks fewer cells than the first one had to.
    assert!(break_cells(&second).len() <= cells.len(), "digging two cells cannot need more digging: {:?}", break_cells(&second));
}
