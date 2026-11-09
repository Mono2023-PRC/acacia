//! The leap controller: sprint at a target cell, jump and land standing in it (or fall into its water).

use acacia_physics::{BlockPos, WorldView};

use super::control::{Controller, LAND_EPS, Phase, centre, standing_on, yaw_to};
use super::profile::Aim;
use super::rollout::Body;
use crate::movement::Controls;
use crate::movement::{feet_cell, hop_room_blocked_by};

/// Ticks a leap may take, run-up included.
pub(crate) const MAX_LEAP_TICKS: u16 = 40;
/// Below the target's floor by this much, a leap has failed.
const FELL: f32 = 1.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JumpAt {
    /// On the last tick walking on would still be on the ground.
    Edge,
    /// On the move's k-th tick (0 = first), or the first one on the ground after it.
    Tick(u8),
    /// Once this far (blocks) along the move from where it started.
    Along(f32),
    /// Never: run off the edge and drop.
    Never,
}

/// Sprints at `target`, jumps and lands standing in its cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeapCtl {
    pub target: BlockPos,
    /// Feet height standing on the target.
    pub feet: f32,
    /// The target is water: done once the feet are in its cell.
    pub wet: bool,
    pub jump: JumpAt,
    /// Let go of forward after this many ticks in the air.
    pub coast: Option<u8>,
    /// Degrees added to the line at the target while on the ground: the takeoff yaw.
    pub yaw_off: f32,
    /// Keep turning at the target in the air (until coasting).
    pub air_aim: bool,
    /// A hop on flat ground: coming down beside the target at its level is as good.
    pub slack: bool,
    pub aim: Aim,
    t: u16,
    air: u16,
    grounded: bool,
    jumped: bool,
    origin: Option<[f32; 2]>,
    takeoff: Option<u16>,
}

impl LeapCtl {
    pub fn new(target: BlockPos, feet: f32, jump: JumpAt, coast: Option<u8>, aim: Aim) -> Self {
        let (t, air, grounded, jumped, origin, takeoff) = (0, 0, false, false, None, None);
        Self { target, feet, wet: false, jump, coast, yaw_off: 0.0, air_aim: false, slack: false, aim, t, air, grounded, jumped, origin, takeoff }
    }

    /// The tick the jump was pressed on, once it was.
    pub fn takeoff(&self) -> Option<u16> {
        self.takeoff
    }

    /// Left the ground since the move started.
    pub fn airborne(&self) -> bool {
        self.air > 0
    }

    fn landed(&self, b: &Body) -> bool {
        if self.wet {
            return b.cell() == [self.target[0], self.target[2]] && b.pos[1] < (self.target[1] + 1) as f32;
        }
        if self.slack {
            let [x, z] = b.cell();
            return b.on_ground && (b.pos[1] - self.feet).abs() < LAND_EPS && (x - self.target[0]).abs() <= 1 && (z - self.target[2]).abs() <= 1;
        }
        standing_on(b, self.target, self.feet)
    }

    fn along(&self, origin: [f32; 2], b: &Body) -> f32 {
        let [tx, tz] = centre(self.target);
        let (sx, sz) = (tx - origin[0], tz - origin[1]);
        ((b.pos[0] - origin[0]) * sx + (b.pos[2] - origin[1]) * sz) / sx.hypot(sz).max(1e-3)
    }
}

impl Controller for LeapCtl {
    fn step<W: WorldView + ?Sized>(&mut self, b: &Body, peek: &mut dyn FnMut(&Controls) -> Body, w: &W, c: &mut Controls) -> Phase {
        let origin = *self.origin.get_or_insert([b.pos[0], b.pos[2]]);
        if self.air > 0 && self.landed(b) {
            return Phase::Done;
        }
        // Landed elsewhere, or a jump pressed on the ground did not lift off.
        if b.on_ground && (self.air > 0 || self.jumped) {
            return Phase::Failed;
        }
        if self.t >= MAX_LEAP_TICKS || b.pos[1] < self.feet - FELL {
            return Phase::Failed;
        }
        // Falling below a dry target's floor in the air, it can no longer be reached.
        if !self.wet && self.air > 0 && b.vel[1] < 0.0 && b.pos[1] < self.feet - LAND_EPS {
            return Phase::Failed;
        }
        let tick = self.t;
        self.t += 1;
        // Air before the first ground contact (a start still settling) is no leap.
        self.grounded |= b.on_ground;
        if !b.on_ground && self.grounded {
            self.air += 1;
        }
        (c.strafe, c.sneak, c.jump, c.pitch) = (0.0, false, false, 0.0);
        let coasting = self.coast.is_some_and(|k| self.air >= u16::from(k));
        if b.on_ground {
            c.yaw = self.aim.turn(b.yaw, yaw_to(b, centre(self.target)) + self.yaw_off);
        } else if self.air_aim && !coasting {
            c.yaw = self.aim.turn(b.yaw, yaw_to(b, centre(self.target)));
        }
        (c.forward, c.sprint) = if coasting { (0.0, false) } else { (1.0, true) };
        if b.on_ground {
            // t110: a press under a ceiling is a bump, not a leap -- ask the one ceiling rule (takeoff and landing)
            // before pressing. A watery target has no head cell to speak of.
            let landing = (!self.wet).then(|| [self.target[0], self.feet.floor() as i32, self.target[2]]);
            let room = hop_room_blocked_by(w, feet_cell(b.pos), landing);
            c.jump = room.is_none() && match self.jump {
                JumpAt::Edge => !peek(c).on_ground,
                JumpAt::Tick(k) => tick >= u16::from(k),
                JumpAt::Along(x) => self.along(origin, b) >= x,
                JumpAt::Never => false,
            };
            if c.jump {
                (self.jumped, self.takeoff) = (true, Some(tick));
            }
        }
        Phase::Going
    }
}
