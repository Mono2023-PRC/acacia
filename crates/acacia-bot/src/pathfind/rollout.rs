//! What a controller sees of the player each tick, in rollouts and live alike, and running a controller
//! forward on a fork of the simulation.

use acacia_physics::{PlayerState, Vec3, WorldView};

use super::control::{Controller, Phase};
use crate::movement::{Controls, Rollout};

/// A controller's run: `bodies[0]` is the start, one more per simulated tick.
#[derive(Debug, Clone)]
pub struct Trace {
    pub bodies: Vec<Body>,
    /// The controller finished the move (`Phase::Done`).
    pub landed: bool,
    pub ticks: u32,
}

/// Runs `ctl` on `r` from controls `c` until it finishes, fails or `max` ticks pass.
pub fn run<W: WorldView + ?Sized>(r: &mut Rollout, ctl: &mut impl Controller, mut c: Controls, max: u32, w: &W) -> Trace {
    let mut bodies = vec![Body::of(&r.st)];
    loop {
        let b = bodies[bodies.len() - 1];
        let phase = ctl.step(&b, &mut |pc: &Controls| peek(r, pc, w), w, &mut c);
        let ticks = bodies.len() as u32 - 1;
        if phase != Phase::Going || ticks >= max {
            return Trace { bodies, landed: phase == Phase::Done, ticks };
        }
        r.step(&c, w);
        bodies.push(Body::of(&r.st));
    }
}

/// Rollout ticks one follower tick may simulate, and those it has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spend {
    pub ticks: u64,
    pub limit: u64,
    /// Something was not simulated for want of budget.
    pub short: bool,
}

impl Spend {
    pub fn new(limit: u64) -> Self {
        Self { ticks: 0, limit, short: false }
    }

    pub fn left(&self) -> u64 {
        self.limit.saturating_sub(self.ticks)
    }

    /// A rollout of up to `n` ticks fits; marks the spend short if not.
    pub fn affords(&mut self, n: u64) -> bool {
        let fits = self.ticks + n <= self.limit;
        self.short |= !fits;
        fits
    }
}

/// The body one tick after `r` with `c` held; `r` is left as it was.
pub fn peek(r: &Rollout, c: &Controls, w: &(impl WorldView + ?Sized)) -> Body {
    let mut f = r.clone();
    f.step(c, w);
    Body::of(&f.st)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Body {
    /// Feet.
    pub pos: Vec3,
    pub vel: Vec3,
    /// Degrees, as last sent.
    pub yaw: f32,
    pub on_ground: bool,
    pub h_collision: bool,
    /// Hit a ceiling this tick.
    pub bonk: bool,
    /// Blocks fallen since last on the ground.
    pub fall: f32,
}

impl Body {
    pub fn of(st: &PlayerState) -> Self {
        Self {
            pos: st.pos,
            vel: st.vel,
            yaw: st.yaw,
            on_ground: st.on_ground,
            h_collision: st.collide_x || st.collide_z,
            bonk: st.collide_y && !st.on_ground,
            fall: st.fall_distance,
        }
    }

    pub fn h_speed(&self) -> f32 {
        self.vel[0].hypot(self.vel[2])
    }

    pub fn cell(&self) -> [i32; 2] {
        [self.pos[0].floor() as i32, self.pos[2].floor() as i32]
    }

    /// Largest per-axis position difference.
    pub fn error(&self, other: &Body) -> f32 {
        (0..3).map(|i| (self.pos[i] - other.pos[i]).abs()).fold(0.0, f32::max)
    }
}
