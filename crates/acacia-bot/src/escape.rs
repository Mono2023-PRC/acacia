//! Getting out of a pit the bot has dug itself into.
//!
//! A hole has no walkable path, so the pathfinder cannot help: vanilla's own answer is a staircase — dig the
//! block above the pit's wall, then jump onto the wall (one block at a time, however deep the pit is). The
//! second resort, placing a block under the feet, needs a placement the pit floor often refuses (the cell is
//! the bot's own), so it is left to [`crate::build`] rather than guessed at here.

use acacia_physics::BlockPos;

use crate::{ActionError, Bot};

/// Horizontal directions, in the order a stuck bot tries them.
const DIRS: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];
/// Ticks one jump is given: a vanilla jump peaks around tick 10 and lands by 14.
const JUMP_TICKS: u32 = 16;
/// Ticks to let the bot settle after a jump that did not get it out before trying the next side.
const SETTLE_TICKS: u32 = 6;
/// Steps [`Bot::climb_out`] takes by default.
pub const DEFAULT_STEPS: u32 = 12;
/// Ticks to wait for the landing after one climb step.
const LANDING_TICKS: u32 = 20;

impl Bot {
    /// The eye's block, or the feet cell when physics has not started: the cell a pit is measured from.
    pub fn feet_cell(&self) -> Option<BlockPos> {
        self.block_position()
    }

    /// The feet's height, from the simulated body when there is one.
    pub fn feet_y(&self) -> Option<f32> {
        self.movement.as_ref().and_then(|m| m.position()).map(|p| p[1]).or(Some(self.state.player.position.y))
    }

    /// Whether the bot stands in a pit it cannot walk out of: every horizontal neighbour at feet level is solid
    /// (a shrub or water does not count as a wall).
    pub fn in_hole(&self) -> bool {
        let Some(feet) = self.block_position() else { return false };
        DIRS.iter().all(|&(dx, dz)| self.solid_cell([feet[0] + dx, feet[1], feet[2] + dz]))
    }

    /// Whether the cell at `pos` is solid (known, not air and not liquid): the public form of the check the
    /// example programs and callers outside the crate need.
    pub fn is_solid(&self, pos: BlockPos) -> bool {
        self.solid_cell(pos)
    }

    fn solid_cell(&self, pos: BlockPos) -> bool {
        self.block_at(pos).is_some_and(|(_, s)| !s.is_air() && !s.is_liquid())
    }

    /// Climbs out of the pit the bot is in, one staircase step at a time: digs the block above a side wall and
    /// jumps onto it, trying all four sides before giving up. Returns the number of steps taken; `Ok(0)` when
    /// the bot was not in a pit to begin with.
    ///
    /// Fails with a `Rejected` naming the height it is stuck at when no side can be climbed (bedrock walls,
    /// nothing breakable), so a caller can tell "still trapped" from "the dig was refused".
    pub async fn climb_out(&mut self, max_steps: u32) -> Result<u32, ActionError> {
        if !self.has_physics() {
            return Err(ActionError::NotPossible("climbing out needs a physics bot".into()));
        }
        let mut steps = 0;
        while self.in_hole() {
            if steps >= max_steps {
                return Err(ActionError::Rejected(format!(
                    "still in a pit at y={:?} after {steps} steps: no side could be climbed",
                    self.feet_y()
                )));
            }
            if !self.climb_step().await? {
                return Err(ActionError::Rejected(format!(
                    "still in a pit at y={:?}: no side wall has a block above it that can be dug and jumped onto",
                    self.feet_y()
                )));
            }
            steps += 1;
        }
        Ok(steps)
    }

    /// [`Bot::climb_out`] that also insists on reaching `surface_y` (the height the bot fell from): a caller that
    /// knows where the surface is gets "back on top", not "no longer boxed in" (t29: five runs each climbed one
    /// block of three and the start sank three a run).
    pub async fn climb_out_to(&mut self, surface_y: f32, max_steps: u32) -> Result<u32, ActionError> {
        if !self.has_physics() {
            return Err(ActionError::NotPossible("climbing out needs a physics bot".into()));
        }
        let mut steps = 0;
        while steps < max_steps && self.feet_y().is_none_or(|y| y < surface_y - 0.5) {
            if !self.climb_step().await? {
                break;
            }
            steps += 1;
            self.settle_landing().await?;
        }
        let y = self.feet_y();
        if y.is_none_or(|y| y < surface_y - 0.5) {
            return Err(ActionError::Rejected(format!(
                "climb_out_to: still at y={y:?} after {steps} steps, the surface is y={surface_y:.2}: no side wall has a block above it that can be dug and jumped onto"
            )));
        }
        Ok(steps)
    }

    /// Waits for the landing after a climb step, so the height a caller reads is a standing height and not a
    /// jump's peak (t29 caught `climb_out` returning at y+0.17 mid-air).
    async fn settle_landing(&mut self) -> Result<(), ActionError> {
        for _ in 0..LANDING_TICKS {
            if self.movement.as_ref().is_some_and(|m| m.on_ground()) {
                return Ok(());
            }
            self.next_tick(|_, _| false).await?;
        }
        Ok(())
    }

    /// One staircase step: dig the cell above a side wall, jump toward it, and report whether the feet rose.
    async fn climb_step(&mut self) -> Result<bool, ActionError> {
        let Some(feet) = self.block_position() else { return Ok(false) };
        let before = self.feet_y();
        for (dx, dz) in DIRS {
            let wall = [feet[0] + dx, feet[1], feet[2] + dz];
            if !self.solid_cell(wall) {
                continue;
            }
            let above = [wall[0], wall[1] + 1, wall[2]];
            if self.solid_cell(above) && self.break_block(above).await.is_err() {
                // Bedrock, or a block the server will not let go: try the next side.
                continue;
            }
            if self.solid_cell(above) {
                continue;
            }
            let yaw = (-(dx as f32)).atan2(dz as f32).to_degrees();
            self.jump_forward(yaw).await?;
            if self
                .feet_y()
                .is_some_and(|y| before.is_none_or(|b| y >= b + 0.9))
            {
                return Ok(true);
            }
            self.wait_ticks(SETTLE_TICKS).await?;
        }
        Ok(false)
    }

    /// Holds forward + jump for a tick stream, facing `yaw`; the controls are released afterwards.
    async fn jump_forward(&mut self, yaw: f32) -> Result<(), ActionError> {
        for _ in 0..JUMP_TICKS {
            if let Some(c) = self.controls() {
                c.forward = 1.0;
                c.strafe = 0.0;
                c.jump = true;
                c.sprint = false;
                c.yaw = yaw;
            }
            self.next_tick(|_, _| false).await?;
        }
        if let Some(c) = self.controls() {
            c.forward = 0.0;
            c.jump = false;
        }
        Ok(())
    }
}
