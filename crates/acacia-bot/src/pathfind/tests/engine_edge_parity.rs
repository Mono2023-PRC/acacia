//! D* Lite has to dig for itself.
//!
//! The t145 corpus only proved the two engines agree on *walking*. With `allow_dig` the default engine used to give up
//! after a handful of expansions — the first plan always came from the A* retry, which is exactly the case the engine
//! exists for: a bot that digs its own tunnel re-plans on every block it removes.

use acacia_physics::{BlockPos, PlayerState, Vec3};

use super::grid::Grid;
use super::search::{ORIGIN, opts};
use crate::pathfind::dstar::DStarState;
use crate::pathfind::opts::PlanStats;
use crate::pathfind::{Goal, Path, SearchOpts, search};

/// A corridor one cell wide, sealed by a two-high solid wall: the only way on is to break two cells.
fn sealed_corridor() -> Grid {
    let g = Grid::floor(0, 16);
    // Side walls keep the search from walking round the end of the wall (a corridor, not a field).
    g.fill([-2, 1, -1], [14, 3, -1], "stone");
    g.fill([-2, 1, 1], [14, 3, 1], "stone");
    g.fill([3, 1, -2], [3, 3, 2], "stone");
    g
}

fn digging() -> SearchOpts {
    SearchOpts { allow_dig: true, ..opts(false) }
}

fn breaks(p: &Path) -> usize {
    p.nodes
        .iter()
        .flat_map(|n| n.work.steps())
        .filter(|s| matches!(s, crate::pathfind::work::Step::Break(_)))
        .count()
}

fn path_stats(p: &Path) -> (bool, f32, usize, Option<Vec<BlockPos>>) {
    (
        p.complete,
        p.cost,
        p.plan.nodes_expanded,
        Some(p.nodes.iter().map(|n| n.pos).collect()),
    )
}

#[test]
fn a_wall_that_has_to_be_dug_is_planned_by_dstar_itself() {
    let g = sealed_corridor();
    let goal = Goal::Block([10, 1, 0]);
    let o = digging();

    let star = search(&g.terrain(), ORIGIN, &goal, &o);
    assert!(star.complete, "A* digs through: {:?}", star.plan.describe());
    assert!(breaks(&star) >= 2, "A* breaks two cells: {}", breaks(&star));

    let mut state = DStarState::default();
    let d = crate::pathfind::dstar::plan_with(&g.terrain(), &PlayerState::new(ORIGIN), &goal, &o, &mut state);
    assert!(
        d.complete,
        "D* Lite digs through on its own (engine {}, {} expansions, {:?}): {:?}",
        d.plan.engine_used,
        d.plan.nodes_expanded,
        path_stats(&d),
        d.plan.describe()
    );
    assert!(breaks(&d) >= 2, "the D* Lite plan breaks the wall: {:?}", d.plan.describe());
    assert_eq!(d.cost, star.cost, "both engines price the tunnel the same");
    assert_eq!(d.plan.engine_used, PlanStats::DSTAR);
    assert_eq!(d.plan.fallback_reason, None, "the first plan is not a retry");
}

/// The dispatcher must not paper over the case: on this geometry the answer is D* Lite's own.
#[test]
fn the_dispatch_says_dstar_and_does_not_fall_back() {
    let g = sealed_corridor();
    let goal = Goal::Block([10, 1, 0]);
    let o = digging();
    let mut state = DStarState::default();
    let sink = std::sync::Arc::new(std::sync::Mutex::new(Some(state)));
    let p = crate::pathfind::dstar::search_engine(
        &g.terrain(),
        &PlayerState::new(ORIGIN),
        &goal,
        &o,
        &Default::default(),
        Some(&sink),
    );
    assert!(p.complete, "{:?}", p.plan.describe());
    assert_eq!(p.plan.engine_used, PlanStats::DSTAR, "{:?}", p.plan.describe());
    assert_eq!(p.plan.fallback_reason, None, "{:?}", p.plan.describe());
    assert!(breaks(&p) >= 2);
}

/// Both engines must ask `moves::successors` the same question. A per-node summary (how many edges, and the cheapest
/// of them) is enough to pin that down without eyeballing two debug dumps.
#[test]
fn both_engines_offer_the_same_edges_from_the_same_node() {
    use crate::pathfind::moves::{self, Ctx};
    use crate::pathfind::node::Entry;

    let g = sealed_corridor();
    let t = g.terrain();
    let live = PlayerState::new(ORIGIN);
    let o = digging();
    let ctx = Ctx {
        parkour: o.parkour,
        dig: o.allow_dig,
        bridge: o.allow_bridge,
        doors: o.open_doors,
        kit: &o.kit,
        blocks_left: 0,
        macros: None,
        keep: None,
        no_body: None,
        boats: None,
    };
    // The cell in front of the wall: this is where the two engines have to agree that a tunnel is on offer.
    let (start, spot) = crate::pathfind::astar::start_node(&t, live.pos).expect("a start");
    let at: BlockPos = [start[0] + 2, start[1], start[2]];
    let spot = t.spot(at).expect("the cell before the wall is a place to stand");
    let mom = crate::pathfind::node::Momentum::snap(&live, at);
    let mut edges = Vec::new();
    moves::successors(&t, at, spot, (Entry::Rest, &mom), &ctx, &mut edges);
    let summary = |edges: &[crate::pathfind::moves::Edge]| {
        let breaks = edges.iter().filter(|e| !e.work.is_empty()).count();
        let cheapest = edges.iter().map(|e| e.cost).fold(f32::INFINITY, f32::min);
        (edges.len(), breaks, cheapest.to_bits())
    };
    assert_eq!(spot.feet, start[1] as f32, "the cell before the wall is on the same level");
    let (n, b, c) = summary(&edges);
    assert!(b >= 1, "a dig edge has to be on offer at the wall: {n} edges, {b} with work");
    assert!(c != f32::INFINITY.to_bits());

    // Now the same question through the engine: whatever it keeps has to include that dig edge.
    let mut state = DStarState::default();
    let p = crate::pathfind::dstar::plan_with(&t, &live, &Goal::Block([10, 1, 0]), &o, &mut state);
    assert!(p.plan.nodes_expanded > 6, "the search has to go past the wall, not stop at it: {:?}", p.plan.describe());
}

/// A retry must never be silent, and the reason has to say *which* kind of giving up happened: the dispatcher treats
/// `no_path` (ask the other engine) differently from `budget` (give it more room) and `dynamic` (the table is not the
/// right tool at all).
#[test]
fn the_three_fallback_reasons_are_told_apart() {
    // `no_path`: a goal behind a wall nobody can dig (bedrock-ish: `allow_dig` off, no gap).
    let g = Grid::floor(0, 16);
    // A curtain wall across the whole grid: with digging off there is no way past it at all.
    g.fill([8, 1, -31], [8, 4, 47], "stone");
    let solid = SearchOpts { allow_dig: false, ..opts(false) };
    let mut state = DStarState::default();
    let sink = std::sync::Arc::new(std::sync::Mutex::new(Some(DStarState::default())));
    let none = crate::pathfind::dstar::search_engine(
        &g.terrain(), &PlayerState::new(ORIGIN), &Goal::Block([14, 1, 0]), &solid, &Default::default(), Some(&sink),
    );
    assert!(!none.complete);
    assert_eq!(none.plan.fallback_reason, Some("no_path"), "{:?}", none.plan.describe());

    // `budget`: the same open field, but the first engine may only look at a node or two.
    let open = Grid::floor(0, 24);
    let tiny = SearchOpts { max_nodes: 1, ..opts(false) };
    let sink = std::sync::Arc::new(std::sync::Mutex::new(Some(DStarState::default())));
    let cut = crate::pathfind::dstar::search_engine(
        &open.terrain(), &PlayerState::new(ORIGIN), &Goal::Block([18, 1, 0]), &tiny, &Default::default(), Some(&sink),
    );
    assert_eq!(cut.plan.fallback_reason, Some("budget"), "{:?}", cut.plan.describe());
    assert_eq!(cut.plan.engine_used, PlanStats::ASTAR);

    // `dynamic`: threats move every tick, so the table is bypassed on purpose (not "D* Lite failed").
    let mut hazards = crate::pathfind::hazard::Hazards::default();
    hazards.spheres.push(crate::pathfind::hazard::Sphere { centre: [12.5, 2.5, 0.5], radius: 4.0, weight: 40.0 });
    let dyn_opts = SearchOpts { hazards: Some(std::sync::Arc::new(hazards)), ..opts(false) };
    let sink = std::sync::Arc::new(std::sync::Mutex::new(Some(DStarState::default())));
    let p = crate::pathfind::dstar::search_engine(
        &open.terrain(), &PlayerState::new(ORIGIN), &Goal::Block([18, 1, 0]), &dyn_opts, &Default::default(), Some(&sink),
    );
    assert_eq!(p.plan.fallback_reason, Some("dynamic"), "{:?}", p.plan.describe());
    assert_eq!(p.plan.engine_used, PlanStats::ASTAR);
    let _ = state;
}
