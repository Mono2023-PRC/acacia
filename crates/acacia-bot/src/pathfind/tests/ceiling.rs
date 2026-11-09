//! t110: the ceiling rule in front of every jump press.
//!
//! t107 measured the bug on the live server: under a one-block ceiling the client sent 131 `jump=1` packets in three
//! windows (`max_y` flat, `rise_ground=+0.000`), and only `allow_dig:true` — which widened the hole — climbed
//! (`rise_ground=+1.200`). t102 had taught the *hop* path to look at the head cell; the pathfinder's own emitters
//! (steering, pillar, shortcut, leap) never asked.
//!
//! These tests drive the real follower over a synthetic course, one tick at a time, and record the jump bit on every
//! tick — not just at the end: a single press is a packet.

use acacia_physics::Vec3;

use super::grid::Grid;
use super::search::{ORIGIN, opts};
use super::sim::{perform, sense};
use crate::movement::{feet_cell, hop_room_blocked_by, jump_room_blocked_by};
use crate::movement::Movement;
use crate::pathfind::{FollowStatus, Follower, Goal, MoveKind, PathNode, search};

/// Drives the follower along `path` to `goal`, returning the jump bit of every tick, the statuses, and where it ended.
/// Anything but `Moving`/`Work`/`Arrived` re-plans from the current position, the way the navigator does.
fn drive(g: &Grid, goal: Goal, path: Vec<PathNode>, max_ticks: u32) -> (Vec<bool>, Vec<FollowStatus>, Vec3) {
    let world = g.physics();
    let mut m = Movement::new();
    m.start(ORIGIN, 0.0, 0.0);
    let opts = opts(true);
    let mut partial = false;
    let mut follower = Follower::new(path, ORIGIN, 0.25);
    let (mut jumps, mut statuses, mut replans) = (Vec::new(), Vec::new(), 0u32);
    for _ in 0..max_ticks {
        let s = sense(g, &m);
        let pos = s.pos;
        let live = m.fork();
        let status = follower.tick(&s, live.as_ref(), &world, &mut m.controls);
        jumps.push(m.controls.jump);
        statuses.push(status);
        match status {
            FollowStatus::Moving => {}
            FollowStatus::Work => {
                let node = *follower.pending_work().expect("work due");
                perform(g, &world, &mut m, &node);
                follower.work_done();
            }
            FollowStatus::Arrived if !partial => return (jumps, statuses, pos),
            _ => {
                replans += 1;
                let again = search(&g.terrain(), pos, &goal, &opts);
                if again.nodes.is_empty() || replans > 40 {
                    return (jumps, statuses, pos);
                }
                partial = !again.complete;
                follower = Follower::new(again.nodes, pos, 0.25);
            }
        }
        m.tick(&world);
    }
    (jumps, statuses, m.position().unwrap())
}

/// A one-block step at x = 3 spanning z, with the goal three blocks past it.
fn step_course() -> Grid {
    let g = Grid::floor(-8, 14);
    g.fill([3, 1, -8], [3, 1, 14], "stone");
    g
}

fn statuses_seen(statuses: &[FollowStatus]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for status in statuses {
        let name = format!("{status:?}");
        if !seen.contains(&name) {
            seen.push(name);
        }
    }
    seen
}

#[test]
fn a_one_block_ceiling_silences_every_jump_press() {
    let g = step_course();
    let goal = Goal::XZ(9, 0);
    // Planned over open sky: the step is an `Ascend` node, the plan the client would have made before t110.
    let path = search(&g.terrain(), ORIGIN, &goal, &opts(true));
    assert!(
        path.nodes.iter().any(|n| n.kind == MoveKind::Ascend),
        "the course has to need a jump: {:?}",
        path.nodes.iter().map(|n| n.kind).collect::<Vec<_>>()
    );
    // Then the ceiling is there, right over the step's head cell (y = 3): standing on the step needs two cells.
    g.fill([3, 3, -8], [3, 3, 14], "stone");
    assert_eq!(hop_room_blocked_by(&g.physics(), [2, 1, 0], Some([3, 2, 0])), Some([3, 3, 0]), "no room");

    let (jumps, statuses, end) = drive(&g, goal, path.nodes, 240);
    assert!(jumps.len() >= 8, "the follower actually ran before giving the node back: {} ticks", jumps.len());
    for (tick, jump) in jumps.iter().enumerate() {
        assert!(!jump, "tick {tick} pressed jump under a one-block ceiling (t107: 131 such packets)");
    }
    assert!(
        statuses.contains(&FollowStatus::OffPath),
        "a node it cannot jump onto must go back to the planner, not sit in place; statuses seen: {:?}, ended at {end:?}",
        statuses_seen(&statuses)
    );
    assert!(end[0] < 4.0, "it never climbed the capped step: {end:?}");
}

#[test]
fn with_room_the_same_course_still_jumps_and_climbs() {
    let g = step_course();
    assert_eq!(jump_room_blocked_by(&g.physics(), [0, 1, 0]), None, "open sky");
    let goal = Goal::XZ(9, 0);
    let path = search(&g.terrain(), ORIGIN, &goal, &opts(true));
    let (jumps, _, end) = drive(&g, goal, path.nodes, 240);
    assert!(jumps.iter().any(|j| *j), "the step is climbed by jumping: {} ticks", jumps.len());
    assert!(end[0] > 7.0, "walked up the step and on to the goal, ended at {end:?}");
}

#[test]
fn the_ceiling_rule_is_asked_of_the_feet_cell_and_reports_the_blocking_cell() {
    let g = Grid::floor(-4, 4);
    let feet = feet_cell([0.5, 1.0, 0.5]);
    assert_eq!(feet, [0, 1, 0]);
    // Two cells of body: what counts is the cell above the head, so the floor under the feet is irrelevant.
    assert_eq!(jump_room_blocked_by(&g.physics(), feet), None);
    g.fill([0, 3, 0], [0, 3, 0], "stone");
    assert_eq!(jump_room_blocked_by(&g.physics(), feet), Some([0, 3, 0]));
    // Water has no collision boxes: swimming up is not a ceiling.
    g.set_state([0, 3, 0], "water", "");
    assert_eq!(jump_room_blocked_by(&g.physics(), feet), None);
}
