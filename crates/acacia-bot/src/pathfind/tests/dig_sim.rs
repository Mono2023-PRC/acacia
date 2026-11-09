//! A walk with `allow_dig` must clear every cell it needs *before* it goes, and must never press
//! jump to force its way through.
//!
//! The live session (2026-11-09 18:18) pressed jump **1288** times with the body pinned at
//! `[150183.83, -19.21544, 149878.1]` (`着地=false`), while the model ended up digging the same four cells
//! by hand. These runs drive the real follower and the real movement simulation, and count both the jumps
//! the controls carried and the cells the moves broke.

use acacia_physics::Vec3;

use super::grid::Grid;
use super::search::ORIGIN;
use super::sim::{perform, sense};
use crate::movement::Movement;
use crate::pathfind::{search, FollowStatus, Follower, Goal, SearchOpts};

struct Walk {
    arrived: bool,
    /// Ticks whose controls carried `jump`.
    jumps: u32,
    replans: u32,
    /// Cells the moves broke on their way.
    breaks: u32,
    end: Vec3,
}

fn walk(g: &Grid, start: Vec3, goal: Goal, opts: &SearchOpts, max_ticks: u32) -> Walk {
    let world = g.physics();
    let mut m = Movement::new();
    m.start(start, 0.0, 0.0);
    let plan = |pos: Vec3| {
        let path = search(&g.terrain(), pos, &goal, opts);
        (path.complete, Follower::new(path.nodes, pos, 0.25))
    };
    let (mut complete, mut follower) = plan(start);
    let (mut replans, mut jumps, mut breaks) = (0, 0, 0);
    for _ in 0..max_ticks {
        let s = sense(g, &m);
        let live = m.fork();
        if m.controls.jump {
            jumps += 1;
        }
        match follower.tick(&s, live.as_ref(), &world, &mut m.controls) {
            FollowStatus::Moving => {}
            FollowStatus::Work => {
                let node = *follower.pending_work().expect("work due");
                breaks += node.work.breaks() as u32;
                perform(g, &world, &mut m, &node);
                follower.work_done();
            }
            FollowStatus::Arrived if complete => {
                let end = m.position().expect("started");
                return Walk { arrived: true, jumps, replans, breaks, end };
            }
            _ => {
                replans += 1;
                let pos = s.pos;
                (complete, follower) = plan(pos);
            }
        }
        m.tick(&world);
    }
    Walk { arrived: false, jumps, replans, breaks, end: m.position().expect("started") }
}

fn digs() -> SearchOpts {
    SearchOpts { allow_dig: true, ..SearchOpts::default() }
}

/// A wall two cells high: the feet cell and the head cell both have to go.
fn two_high_wall() -> Grid {
    let g = Grid::floor(-10, 10);
    g.fill([2, 1, -10], [2, 2, 10], "stone");
    g
}

#[test]
fn a_two_high_wall_is_cleared_without_a_single_jump() {
    let g = two_high_wall();
    let r = walk(&g, ORIGIN, Goal::Block([4, 1, 0]), &digs(), 400);
    assert!(r.arrived, "walking through a dug wall must work: end {:?}, {} re-plans, {} breaks", r.end, r.replans, r.breaks);
    assert_eq!(r.jumps, 0, "a body that is digging its way through has nothing to jump for");
    assert!(r.breaks >= 2, "feet and head both have to go, broke {}", r.breaks);
}

/// A one-wide shaft: the only way out is sideways through the wall beside the body.
#[test]
fn a_one_wide_shaft_is_opened_sideways_without_a_jump() {
    let g = Grid::floor(-10, 10);
    // The body stands at [0,1,0]; stone all around it at feet and head level.
    g.fill([-1, 1, -1], [1, 2, 1], "stone");
    g.set_state([0, 1, 0], "air", "");
    g.set_state([0, 2, 0], "air", "");
    let r = walk(&g, ORIGIN, Goal::Block([4, 1, 0]), &digs(), 400);
    assert!(r.arrived, "a walled-in body must dig its way out: end {:?}, {} re-plans, {} breaks", r.end, r.replans, r.breaks);
    assert_eq!(r.jumps, 0, "in a one-wide shaft a jump hits the ceiling: nothing to gain");
}

/// Breaking one cell is not a licence to jump: if the body still cannot go, it re-plans (the follower's
/// `OffPath`/`Stuck`), and the only thing that changes is the plan — never the jump key.
#[test]
fn a_dug_out_step_that_still_does_not_fit_is_replanned_not_jumped() {
    let g = Grid::floor(-10, 10);
    // Two high, but the cell above the head is also solid: even dug, a one-cell hole is not a door.
    g.fill([2, 1, -10], [2, 3, 10], "stone");
    g.set_state([2, 2, 0], "air", "");
    let r = walk(&g, ORIGIN, Goal::Block([4, 1, 0]), &digs(), 400);
    assert!(r.arrived, "the planner must find the cells it really needs: end {:?}, {} re-plans", r.end, r.replans);
    assert_eq!(r.jumps, 0, "a hole that is too short is not something to jump at");
}
