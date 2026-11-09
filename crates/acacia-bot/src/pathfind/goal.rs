use std::f32::consts::SQRT_2;

use acacia_physics::BlockPos;

use super::costs::{self, COST_HEURISTIC, JUMP_PENALTY, SPRINT, WALK};

/// Where to go, in feet block positions.
#[derive(Debug, Clone, PartialEq)]
pub enum Goal {
    Block(BlockPos),
    /// Within `radius` blocks (Euclidean) of the position.
    Near(BlockPos, f32),
    XZ(i32, i32),
    Y(i32),
    /// Any of the goals.
    Or(Vec<Goal>),
    /// At least `radius` blocks (Euclidean) from the position: fleeing it.
    Away(BlockPos, f32),
}

/// How a goal turns distance into ticks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Speed {
    /// Ticks per horizontal block.
    pub per_block: f32,
    /// Ticks lost getting up to speed from standing (at most; less over short distances).
    pub startup: f32,
    /// Height is gained or lost during the horizontal run (macro moves), not after it: the larger term counts.
    pub overlap: bool,
}

impl Speed {
    /// Classic moves: sprinting, then climbing or falling (azalea's heuristics, which Baritone also uses).
    pub const CLASSIC: Speed = Speed { per_block: COST_HEURISTIC, startup: 0.0, overlap: false };

    fn combine(&self, xz: f32, y: f32) -> f32 {
        if self.overlap { xz.max(y) } else { xz + y }
    }

    /// Ticks for `blocks` horizontally; `rest`: starting from standing. Classic moves cost `SPRINT` a block
    /// from rest, so the startup never exceeds what they save over `per_block`.
    pub(crate) fn run(&self, blocks: f32, rest: bool) -> f32 {
        let startup = if rest { self.startup.min((SPRINT - self.per_block).max(0.0) * blocks) } else { 0.0 };
        blocks * self.per_block + startup
    }
}

impl Goal {
    pub fn success(&self, p: BlockPos) -> bool {
        match self {
            Goal::Block(g) => p == *g,
            Goal::Near(g, r) => dist_sq(p, *g) <= r * r,
            Goal::XZ(x, z) => p[0] == *x && p[2] == *z,
            Goal::Y(y) => p[1] == *y,
            Goal::Or(goals) => goals.iter().any(|g| g.success(p)),
            Goal::Away(from, r) => dist_sq(p, *from) >= r * r,
        }
    }

    /// How far `p` is from the goal's neighbourhood, in blocks: `0.0` once inside it (t117).
    ///
    /// Not [`Goal::success`], which is a single cell (or a radius the body must stand inside). This is the area a
    /// search has to have *covered* before "no path" means anything: a search that stopped while still 30 blocks away
    /// has not learned that the goal is unreachable, only that it ran out of nodes, and the two need different
    /// answers from whoever asked. `None` for goals with no neighbourhood (`XZ`, `Y`, `Away`).
    pub fn region_distance(&self, p: BlockPos) -> Option<f32> {
        match self {
            // The cell and the neighbours a body could stand on to be "at" it.
            Goal::Block(g) => Some((dist_sq(p, *g).sqrt() - 1.0).max(0.0)),
            // A radius goal that has been reached would have ended the search, so count its own radius as inside:
            // what is being asked is whether the search ever got to where the goal starts to matter.
            Goal::Near(g, r) => Some((dist_sq(p, *g).sqrt() - r).max(0.0)),
            Goal::Or(goals) => goals.iter().filter_map(|g| g.region_distance(p)).reduce(f32::min),
            _ => None,
        }
    }

    /// Estimated ticks to the goal for classic moves.
    pub fn heuristic(&self, p: BlockPos) -> f32 {
        self.estimate(p, Speed::CLASSIC, false)
    }

    /// Estimated ticks to the goal at speed `s`; `rest`: from standing.
    pub(crate) fn estimate(&self, p: BlockPos, s: Speed, rest: bool) -> f32 {
        match self {
            Goal::Block(g) => s.combine(s.run(xz((g[0] - p[0]) as f32, (g[2] - p[2]) as f32), rest), y((g[1] - p[1]) as f32)),
            Goal::Near(g, r) => {
                let d = dist_sq(p, *g).sqrt();
                let exact = Goal::Block(*g).estimate(p, s, rest);
                if d <= *r { 0.0 } else { exact * (1.0 - r / d) }
            }
            Goal::XZ(x, z) => s.run(xz((x - p[0]) as f32, (z - p[2]) as f32), rest),
            Goal::Y(gy) => y((gy - p[1]) as f32),
            Goal::Or(goals) => goals.iter().map(|g| g.estimate(p, s, rest)).fold(f32::INFINITY, f32::min),
            Goal::Away(from, r) => s.run((r - dist_sq(p, *from).sqrt()).max(0.0), rest),
        }
    }
}

fn dist_sq(a: BlockPos, b: BlockPos) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]].map(|v| v as f32);
    d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
}

/// Octile distance (blocks).
fn xz(dx: f32, dz: f32) -> f32 {
    let (x, z) = (dx.abs(), dz.abs());
    let (diagonal, straight) = if x < z { (x, z - x) } else { (z, x - z) };
    diagonal * SQRT_2 + straight
}

fn y(dy: f32) -> f32 {
    if dy > 0.0 {
        // Jumping forward is the usual way up; the xz term already counted one sprint block.
        (costs::jump_one_block().max(WALK) + JUMP_PENALTY - SPRINT) * dy
    } else {
        costs::fall_ticks(2.0) / 2.0 * -dy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_near_goal_accepts_standing_beside_an_occupied_target() {
        // t46: `move_to [x, y, z+3]` on the same level was refused with "no path" because the goal box only fit
        // the (occupied) target cell. A `Near` box must accept the standing spots around it...
        let goal = Goal::Near([150039, 35, 149855], 1.5);
        assert!(goal.success([150039, 35, 149854]));
        assert!(goal.success([150039, 35, 149856]));
        assert!(!goal.success([150039, 35, 149851]), "but not a cell four blocks away");
        // ...and its estimate stays finite for every one of them (the search can aim at them).
        for p in [[150039, 35, 149854], [150039, 35, 149856], [150040, 35, 149855]] {
            assert!(goal.estimate(p, Speed::CLASSIC, false).is_finite(), "{p:?}");
        }
        // A wider box is what the tool falls back to when the tight one has no free spot.
        assert!(Goal::Near([150039, 35, 149855], 3.0).success([150039, 35, 149852]));
    }
}
