//! Jump timing and step-up, measured on the real simulation: what the user saw in game (t70) was "jumps too low to
//! clear a one block step", "jumps in place without moving" and "twitchy repeated jumps". These pin the numbers.

use super::grid::Grid;
use super::search::opts;
use super::sim::run_with;
use crate::movement::Movement;
use crate::pathfind::Goal;

/// A jump from standing: apex above the feet and how far a forward jump carries.
fn jump(forward: f32, sprint: bool, ticks: u32) -> (f32, f32, u32) {
    let g = Grid::floor(-8, 8);
    let world = g.physics();
    let mut m = Movement::new();
    m.start([0.5, 1.0, 0.5], 180.0, 0.0);
    m.controls.forward = forward;
    m.controls.sprint = sprint;
    let (mut apex, mut left_ground, mut in_air) = (1.0f32, 0, 0);
    for _ in 0..ticks {
        // Hold jump only while on the ground: vanilla ignores a held jump in the air and our physics has its own
        // cooldown, so holding it forever would look like "twitching" even when the controller is right.
        m.controls.jump = m.on_ground();
        m.tick(&world);
        let y = m.position().expect("started")[1];
        apex = apex.max(y);
        if m.on_ground() {
            left_ground += 1;
        } else {
            in_air += 1;
        }
    }
    let end = m.position().expect("started");
    (apex - 1.0, ((end[0] - 0.5).powi(2) + (end[2] - 0.5).powi(2)).sqrt(), in_air)
}

#[test]
fn a_standing_jump_reaches_a_block_and_a_quarter() {
    let (apex, _, air) = jump(0.0, false, 20);
    println!("standing: apex {apex:.3} air {air} ticks");
    assert!((apex - 1.25).abs() < 0.05, "vanilla apex is ~1.25, got {apex:.3}");
    assert!(air > 0, "the bot left the ground");
}

#[test]
fn a_forward_jump_carries_the_body() {
    let (apex, walked, air) = jump(1.0, true, 20);
    println!("sprinting forward: apex {apex:.3} carried {walked:.3} blocks in {air} air ticks");
    assert!(apex > 1.0, "still a jump");
    // Vanilla: a sprint jump covers about 2.5 blocks, a walk jump about 1.5. Anything near zero is the "jumps in
    // place" the user saw.
    assert!(walked > 1.0, "a forward jump must carry the body, carried {walked:.3}");
}

#[test]
fn a_one_block_step_is_cleared_by_the_follower() {
    // Floor at y = 0 (feet y = 1), a one block step at x = 3, goal on top of it and beyond.
    let g = Grid::floor(-8, 8);
    // A full width step, so the walk cannot go round its end. Digging off: it has to jump.
    g.fill([3, 1, -8], [3, 1, 8], "stone");
    let mut search = opts(false);
    search.allow_dig = false;
    let run = run_with(&g, [0.5, 1.0, 0.5], Goal::Block([6, 1, 0]), 300, &search);
    println!("step: end {:?} ticks {} replans {}", run.end, run.ticks, run.replans);
    // Climbing the step and dropping back to the floor beyond it is the point: it must not stop at the wall.
    assert!(run.end[0] > 5.0, "the follower got past the step, ended at {:?}", run.end);
    assert!(run.replans <= 1, "and did it without struggling, {} replans", run.replans);
}

#[test]
fn a_two_block_wall_is_not_climbed() {
    let g = Grid::floor(-8, 8);
    g.fill([3, 1, -8], [3, 2, 8], "stone");
    // Digging off: the only way past would be climbing, and two blocks is too high for a jump, so the search must
    // give up instead of planning something the body cannot do.
    let mut search = opts(false);
    search.allow_dig = false;
    let path = crate::pathfind::search(&g.terrain(), [0.5, 1.0, 0.5], &Goal::Block([6, 1, 0]), &search);
    println!("wall: complete {} nodes {}", path.complete, path.nodes.len());
    assert!(!path.complete, "a two block wall with no digging must not be planned through");
    assert!(path.nodes.iter().all(|n| n.pos[0] < 3), "and no planned node may sit inside the wall");
}

/// t112: nine runs reported "no `jump_ok` sample" although the relay saw `jump=1` and the feet rose a block. The jump
/// was pressed by the *pathfinder* (`steer`/`maneuver`/`shortcut`/`leap`), and only the heal path recorded anything,
/// so the successful step came back with no evidence at all.
///
/// The recorder lives where the key becomes a tick, so this pins the wiring, not the recorder's arithmetic: nothing
/// here calls it, the controls are just pressed as the follower presses them.
#[test]
fn the_body_records_a_jump_whichever_emitter_pressed_the_key() {
    let g = Grid::floor(-8, 8);
    let world = g.physics();
    let mut m = Movement::new();
    m.start([0.5, 1.0, 0.5], 180.0, 0.0);
    assert_eq!(m.jump_count(), 0, "nothing has jumped yet");
    assert!(m.last_jump().is_none());
    let mut ticks = 0;
    while m.jump_count() == 0 && ticks < 60 {
        // Exactly what `pathfind/maneuver.rs:35` does: press while standing, release in the air.
        m.controls.jump = m.on_ground();
        m.tick(&world);
        ticks += 1;
    }
    let jump = m.last_jump().expect("the recorder saw the jump the controls pressed");
    println!("recorded after {ticks} ticks: kind={} gained={} pressed={:?}", jump.kind(), jump.gained(), jump.pressed_ticks);
    assert!(jump.sent, "the key really went out");
    assert_eq!(jump.kind(), "jump_landed_back", "a hop in place comes back down on the same block");
    assert_eq!(jump.gained(), 0.0);
    assert!(jump.pressed_ticks > 0 && jump.airborne_ticks.is_some(), "how long the key was held is part of the record");
    assert_eq!(m.jump_count(), 1);
}

/// The same, but the jump that matters: `move_to` onto a one block step. The record has to say "+1 block", the number
/// the agent side turns into `jump_ok` (t96: floats read 0.4375 for a whole block climbed).
#[test]
fn a_recorded_step_up_says_one_block_not_a_float() {
    let g = Grid::floor(-8, 8);
    g.fill([3, 1, -8], [3, 1, 8], "stone");
    let world = g.physics();
    let mut m = Movement::new();
    // Yaw 270 faces +X (vanilla: 0 is +Z, 90 is -X), so forward walks into the step at x = 3.
    m.start([0.5, 1.0, 0.5], 270.0, 0.0);
    let (mut up, mut ticks) = (false, 0);
    while ticks < 60 && !up {
        m.controls.forward = 1.0;
        m.controls.jump = m.on_ground();
        m.tick(&world);
        ticks += 1;
        up = m.jump_count() > 0 && m.last_jump().is_some_and(|j| j.gained() >= 1.0);
    }
    let jump = m.last_jump().expect("a jump happened");
    println!("step up: kind={} from={} to={} gained={}", jump.kind(), jump.from_ground_block(), jump.to_ground_block(), jump.gained());
    assert!(up, "the body climbed the step and the record says so: {jump:?}");
    assert_eq!(jump.to_ground_block(), jump.from_ground_block() + 1);
}
