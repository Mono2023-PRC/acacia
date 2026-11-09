//! One real jump, and the observation of what it did.
//!
//! Until t81 nothing outside [`crate::escape`]'s private staircase helper ever set `Controls.jump`, so a caller
//! could "try a jump" without a single `jump=1` input leaving the client. The primitive here fixes the other half
//! of that: it presses the keys, and it reports what the *body* did (`y_before` / `y_max` / `y_after` / `rose`),
//! so "never sent" and "sent, no effect" stay distinguishable.
//!
//! t86 added the second half of the lesson (run224): the jump key went out, the *walk* key did not. `Controls`
//! belong to whoever writes them last, and a leftover pathfinder claim released mid-jump calls
//! `Controls::stop()` — which clears `forward` but not the key we re-press every tick. So every input a jump
//! needs is written **every tick** by the driver below, never once up front.

use acacia_physics::{BlockPos, WorldView};
use acacia_world::BlockAccess;

use crate::{ActionError, Bot};

/// Ticks a jump press is held while waiting for liftoff. Physics clears `JUMP_DELAY_TICKS` (10) inside this
/// window, so a press made right after a landing still gets through instead of being eaten by the cooldown.
const LAND_WAIT_TICKS: u32 = 20;

const LIFTOFF_TICKS: u32 = 25;
/// Ticks to wait for the landing after liftoff.
const LANDING_TICKS: u32 = 40;
/// How far the feet must end up higher for the jump to count as a block gained.
pub const RISE_THRESHOLD: f32 = 0.9;

/// How far from the feet a hop looks for a step, in blocks (a Chebyshev ring, diagonals included).
pub const STEP_SCAN_RADIUS: i32 = 3;

/// The cell a hop should aim at, or `None` when there is nowhere to step up.
///
/// A candidate is a horizontal offset `(dx, dz)` with the cell `here + (dx, 1, dz)` and the cell above it air and
/// the block under it solid — a block whose top face is one above the feet, with room to stand on it. The jump
/// never aims straight up: with `(0, 0)` excluded the movement vector is never `(0, 0)` (t84: a vertical press lifts
/// the feet and drops them back, `gained = 0.0`).
///
/// t91b ordered these by the angle to the goal first and distance second, and the hop went to a same-shape step
/// three blocks away instead of the plank under the bot's nose: healing means *one level up, as near as possible*,
/// not "the straightest step towards a goal that is 10 blocks off". So the ring (Chebyshev distance) is the first
/// key, and the angle to `toward` only breaks ties inside a ring — a step towards the goal wins a tie, but never
/// beats a nearer step.
pub fn jump_target<F>(here: [i32; 3], toward: [i32; 3], name_at: F) -> Option<[i32; 3]>
where
    F: Fn([i32; 3]) -> Option<String>,
{
    let is_air = |pos: [i32; 3]| name_at(pos).is_some_and(|name| name == "minecraft:air");
    let is_solid = |pos: [i32; 3]| name_at(pos).is_some_and(|name| name != "minecraft:air");
    let (gx, gz) = (toward[0] - here[0], toward[2] - here[2]);
    let goal_norm = ((gx * gx + gz * gz) as f32).sqrt();
    // (ring, -cos, distance left, scan order, cell)
    let mut candidates: Vec<(i32, f32, i32, usize, [i32; 3])> = Vec::new();
    let mut order = 0usize;
    for dz in -(STEP_SCAN_RADIUS)..=STEP_SCAN_RADIUS {
        for dx in -(STEP_SCAN_RADIUS)..=STEP_SCAN_RADIUS {
            if dx == 0 && dz == 0 {
                continue;
            }
            order += 1;
            let cell = [here[0] + dx, here[1] + 1, here[2] + dz];
            if !is_air(cell)
                || !is_air([cell[0], cell[1] + 1, cell[2]])
                || !is_solid([cell[0], cell[1] - 1, cell[2]])
            {
                continue;
            }
            let ring = dx.abs().max(dz.abs());
            let step_norm = ((dx * dx + dz * dz) as f32).sqrt();
            let cos = if goal_norm == 0.0 { 0.0 } else { (dx * gx + dz * gz) as f32 / (step_norm * goal_norm) };
            let left = (toward[0] - cell[0]).pow(2) + (toward[2] - cell[2]).pow(2);
            candidates.push((ring, -cos, left, order, cell));
        }
    }
    candidates.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.2.cmp(&b.2))
            .then(a.3.cmp(&b.3))
    });
    candidates.first().map(|(.., cell)| *cell)
}

/// The cell above `cell` when the body would not fit in `cell`: a hop needs **two** cells, feet and head.
///
/// t102, from the live run 20261109-020753: `dig` broke one block out of a wall, leaving a one-block hole. The
/// body is two blocks tall, so the hole is no door -- but nothing said so, the driver kept pressing forward+jump,
/// and the relay log shows five 15-second windows of hops that each rose `1.252` and came back down
/// (`dy_sum ~ -20.8`, `lines=200 truncated=1`) for a net movement of zero.
///
/// Unloaded chunks (`name_at` returning `None`) are *not* reported as blocked: the tool never invents a cell.
pub fn headroom_blocked_by<F>(cell: [i32; 3], name_at: F) -> Option<[i32; 3]>
where
    F: Fn([i32; 3]) -> Option<String>,
{
    blocked_above(cell, |pos| name_at(pos).map(|name| name != "minecraft:air"))
}

/// The one ceiling rule in the crate (t110): the cell above `cell` when it is solid. Both entry points below are
/// thin spellings of this line, so the hop path (t102) and the pathfinder's four jump emitters cannot disagree.
fn blocked_above(cell: [i32; 3], solid: impl Fn([i32; 3]) -> Option<bool>) -> Option<[i32; 3]> {
    let head = [cell[0], cell[1] + 1, cell[2]];
    solid(head).filter(|blocked| *blocked).map(|_| head)
}

/// The cell that stops a jump pressed from `feet` before it starts: the body is two cells tall (feet, feet + 1), so a
/// solid cell at `feet + 2` turns every press into a ceiling bump. That is [`headroom_blocked_by`] of the cell one
/// above the feet — the landing cell of a one-block hop — expressed once, for every jump emitter in the pathfinder
/// (t110: `follow/steer.rs`, `maneuver.rs`, `shortcut.rs`, `leap.rs`).
///
/// t107 measured the cost of not asking: three windows under a one-block ceiling sent 131 `jump=1` packets with
/// `max_y` flat and `rise_ground=+0.000`; the fourth, with `allow_dig:true`, dug the hole wider and climbed
/// (`rise_ground=+1.200`) — so the answer here is "do not press", never "press harder".
pub fn jump_room_blocked_by(w: &(impl WorldView + ?Sized), feet: [i32; 3]) -> Option<[i32; 3]> {
    blocked_above([feet[0], feet[1] + 1, feet[2]], |pos| Some(solid_at(w, pos)))
}

/// The cell above `cell` when it is solid: the landing half of a hop, asked of a world (t110).
pub fn headroom_blocked_solid(w: &(impl WorldView + ?Sized), cell: [i32; 3]) -> Option<[i32; 3]> {
    blocked_above(cell, |pos| Some(solid_at(w, pos)))
}

/// Either end of a hop without room: the takeoff (`feet`) or the cell it lands on (`landing`). A press that cannot
/// lift (a ceiling over the head) and a press that cannot land (a ceiling over the target) are the same wasted packet,
/// so the pathfinder asks both with one call (t110).
pub fn hop_room_blocked_by(w: &(impl WorldView + ?Sized), feet: [i32; 3], landing: Option<[i32; 3]>) -> Option<[i32; 3]> {
    jump_room_blocked_by(w, feet).or_else(|| landing.and_then(|cell| headroom_blocked_solid(w, cell)))
}

/// The same question asked of a block-state lookup (the bot side has names and boxes but no `WorldView`): the cell
/// above `feet + 1` when `solid` says so. Both spellings end in [`blocked_above`] -- one rule, not two (t110).
pub fn jump_room_blocked_by_state<F>(feet: [i32; 3], solid: F) -> Option<[i32; 3]>
where
    F: Fn([i32; 3]) -> bool,
{
    blocked_above([feet[0], feet[1] + 1, feet[2]], |pos| Some(solid(pos)))
}

/// The cell the feet are in: the coordinate every ceiling question is asked from.
pub fn feet_cell(pos: [f32; 3]) -> [i32; 3] {
    [pos[0].floor() as i32, pos[1].floor() as i32, pos[2].floor() as i32]
}

/// Whether the cell would stop a rise: it has collision boxes and is not something a body climbs through. Ladders and
/// vines have a box and are exactly how a bot leaves a one-wide shaft, so they are not ceilings (t110: the ladder
/// course climbs three `ClimbUp` nodes).
pub fn solid_at(w: &(impl WorldView + ?Sized), pos: [i32; 3]) -> bool {
    if w.block(pos).climbable {
        return false;
    }
    // A cell "stops a rise" when something in it is head-sized, not when it merely has a box: a door's 3/16 panel, a
    // carpet, a button and a torch all have boxes and none of them stops a jump (t110: a builder working inside a
    // doorway reported `no stand` when a bare box test called the door frame a ceiling).
    let mut boxes = Vec::new();
    w.block_collisions(pos, &mut boxes);
    boxes.iter().any(|b| {
        let area = (b.max[0] - b.min[0]).max(0.0) * (b.max[2] - b.min[2]).max(0.0);
        area >= 0.5 && b.max[1] >= 0.5
    })
}

/// What a hop onto `cell` should do before it presses a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopPlan {
    /// Two cells of room: hop.
    Go,
    /// The head cell is solid and digging is allowed: break it first, then hop (t102: widen the hole, do not jump
    /// into its ceiling).
    DigFirst([i32; 3]),
    /// The head cell is solid and digging is not allowed: press nothing, report `no_headroom` with that cell.
    Blocked([i32; 3]),
    /// The same hop from the same cell already gained nothing: pressing again is the in-place loop.
    Refused,
}

/// The decision a hop makes before touching the controls.
///
/// Order matters: headroom is checked first, because "dig that cell" is a next step while "you already tried" is
/// not; and a refused hop is only refused **for the same start cell and the same target** -- once the body moved
/// or the target changed, the memory no longer applies.
pub fn hop_decision<F>(
    from: [i32; 3],
    cell: [i32; 3],
    allow_dig: bool,
    last_dead_hop: Option<([i32; 3], [i32; 3])>,
    name_at: F,
) -> HopPlan
where
    F: Fn([i32; 3]) -> Option<String>,
{
    if let Some(head) = headroom_blocked_by(cell, &name_at) {
        return if allow_dig { HopPlan::DigFirst(head) } else { HopPlan::Blocked(head) };
    }
    if last_dead_hop == Some((from, cell)) {
        return HopPlan::Refused;
    }
    HopPlan::Go
}

/// Where a `detour` should step aside to, nearest first: standable neighbours, the ones perpendicular to the goal
/// first (that is what "step aside" means), then the ones pointing back along it.
///
/// t91b again: the old `detour` turned the goal direction 90° and walked into whatever was there, whatever it was.
/// Probing the neighbours and picking one the body can actually stand in means a detour is a move, not a hope.
pub fn detour_candidates<F>(here: [i32; 3], toward: [i32; 3], name_at: F) -> Vec<[i32; 3]>
where
    F: Fn([i32; 3]) -> Option<String>,
{
    let is_air = |pos: [i32; 3]| name_at(pos).is_some_and(|name| name == "minecraft:air");
    let is_solid = |pos: [i32; 3]| name_at(pos).is_some_and(|name| name != "minecraft:air");
    let (gx, gz) = (toward[0] - here[0], toward[2] - here[2]);
    let goal_norm = ((gx * gx + gz * gz) as f32).sqrt();
    let mut out: Vec<(i32, f32, usize, [i32; 3])> = Vec::new();
    let mut order = 0usize;
    for dz in -2..=2 {
        for dx in -2..=2 {
            if dx == 0 && dz == 0 {
                continue;
            }
            order += 1;
            let cell = [here[0] + dx, here[1], here[2] + dz];
            if !is_air(cell) || !is_air([cell[0], cell[1] + 1, cell[2]]) || !is_solid([cell[0], cell[1] - 1, cell[2]]) {
                continue;
            }
            let ring = dx.abs().max(dz.abs());
            let step_norm = ((dx * dx + dz * dz) as f32).sqrt();
            let cos = if goal_norm == 0.0 { 0.0 } else { (dx * gx + dz * gz) as f32 / (step_norm * goal_norm) };
            // Perpendicular first: |cos| ascending, ties by scan order.
            out.push((ring, cos.abs(), order, cell));
        }
    }
    out.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.2.cmp(&b.2))
    });
    out.into_iter().map(|(.., cell)| cell).collect()
}

/// What one jump did — an observation, not an intent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Jump {
    /// Whether a jump input was actually pressed (the keys really went out).
    pub sent: bool,
    /// Why the jump did not gain a block: `not_on_ground`, `no_liftoff`, `no_gain`; `None` when it did.
    pub reason: Option<&'static str>,
    /// The cell the jump was aimed at; `None` for a jump in place.
    pub target: Option<BlockPos>,
    /// Feet height when the jump started.
    pub y_before: f32,
    /// The block the bot was standing on when it jumped (`y_before` floored); `None` when it started airborne.
    pub y_ground: Option<f32>,
    /// Highest feet height seen during the jump.
    pub y_max: f32,
    /// Feet height after landing (or when the wait ran out, mid-air).
    pub y_after: f32,
    /// Whether the feet ended on a higher **block** (`floor(y_after) > floor(y_before)`), not a float rise.
    pub rose: bool,
    /// Ticks the jump key was held down.
    pub pressed_ticks: u32,
    /// Ticks from the press until the body left the ground; `None` when it never did.
    pub airborne_ticks: Option<u32>,
}

impl Jump {
    /// The block the feet started on — `floor(y_before)`, the same integer grid `target` speaks.
    ///
    /// t95: this server's standing height is not a whole block (`y = 70.5625`), and a hop onto a one-block step
    /// ends at `y = 71.0`. Measured in floats that is a rise of `0.4375` — below any sane threshold — so a real
    /// block climbed was reported as "landed back". In blocks it is `70 -> 71`: the only number worth reading.
    pub fn from_ground_block(&self) -> i32 {
        self.y_before.floor() as i32
    }

    /// The block the feet ended on — `floor(y_after)`.
    pub fn to_ground_block(&self) -> i32 {
        self.y_after.floor() as i32
    }

    /// A one-word verdict for logs and for callers that classify failures.
    ///
    /// `jump_ok` / `jump_up_landed` / `jump_landed_back` are mutually exclusive and all speak in blocks:
    /// * `jump_ok` — the body ended on a higher block, and on the aimed cell when there was one;
    /// * `jump_up_landed` — it did get up a block, but not onto `target`;
    /// * `jump_landed_back` — it came down on the block it started from (and only then);
    /// * `jump_fell` — it ended lower than it started.
    ///
    /// The earlier `jumped`/`no_gain` split was float-based and could not tell the first two apart from a failure.
    pub fn kind(&self) -> &'static str {
        if let Some(reason) = self.reason {
            return reason;
        }
        let (from, to) = (self.from_ground_block(), self.to_ground_block());
        match to.cmp(&from) {
            std::cmp::Ordering::Greater => match self.target {
                // A hop that aimed somewhere and did not arrive: up, but not where it meant to be.
                Some(target) if to != target[1] => "jump_up_landed",
                _ => "jump_ok",
            },
            std::cmp::Ordering::Equal => "jump_landed_back",
            std::cmp::Ordering::Less => "jump_fell",
        }
    }

    /// The cell that is in the way of this hop, for a `no_headroom` verdict: the head cell above `target`.
    /// `None` for every other outcome -- a jump that merely failed does not know what to blame.
    pub fn blocked(&self) -> Option<BlockPos> {
        match (self.reason, self.target) {
            (Some("no_headroom"), Some(t)) => Some([t[0], t[1] + 1, t[2]]),
            _ => None,
        }
    }

    /// The height gained in **blocks** (`to_ground_block - from_ground_block`).
    pub fn gained(&self) -> f32 {
        (self.to_ground_block() - self.from_ground_block()) as f32
    }
}

/// The body a jump drives, split out so the driver can be tested without a server.
pub(crate) trait JumpHost {
    fn on_ground(&self) -> bool;
    fn feet_y(&self) -> Option<f32>;
    /// Press or release the jump key. Must really write `Controls.jump`.
    fn press_jump(&mut self, down: bool);
    /// Hold or release the walk key. Re-applied every tick by the driver: a leftover claim can clear it mid-jump.
    fn press_forward(&mut self, down: bool);
    /// Advance one tick (physics + packets).
    async fn step(&mut self) -> Result<(), ActionError>;
}

/// Presses jump until the body leaves the ground, then waits for the landing, reporting what happened.
///
/// `steer` also drives the walk key (a jump that is meant to carry the bot onto a neighbour); both inputs are
/// rewritten on every tick, so whatever released the keys between ticks cannot silently turn the hop into a
/// jump in place.
pub(crate) async fn jump_with<H: JumpHost + ?Sized>(host: &mut H, steer: bool) -> Jump {
    let started_on_ground = host.on_ground();
    let y_before = host.feet_y().unwrap_or(0.0);
    let mut jump = Jump {
        sent: false,
        reason: Some("not_on_ground"),
        target: None,
        y_before,
        y_ground: started_on_ground.then(|| y_before.floor()),
        y_max: y_before,
        y_after: y_before,
        rose: false,
        pressed_ticks: 0,
        airborne_ticks: None,
    };
    if !started_on_ground {
        // Never press the key from the air: it would report a jump the body could not make.
        return jump;
    }
    for tick in 0..LIFTOFF_TICKS {
        if steer {
            host.press_forward(true);
        }
        host.press_jump(true);
        jump.sent = true;
        jump.pressed_ticks += 1;
        host.step().await.ok();
        jump.y_max = jump.y_max.max(host.feet_y().unwrap_or(jump.y_max));
        if !host.on_ground() {
            jump.airborne_ticks = Some(tick + 1);
            break;
        }
    }
    host.press_jump(false);
    if jump.airborne_ticks.is_none() {
        if steer {
            host.press_forward(false);
        }
        jump.reason = Some("no_liftoff");
        jump.y_after = host.feet_y().unwrap_or(y_before);
        return jump;
    }
    for _ in 0..LANDING_TICKS {
        // Still steering while airborne: the horizontal push is what carries the bot over the lip.
        if steer {
            host.press_forward(true);
        }
        host.step().await.ok();
        jump.y_max = jump.y_max.max(host.feet_y().unwrap_or(jump.y_max));
        if host.on_ground() {
            break;
        }
    }
    if steer {
        host.press_forward(false);
    }
    jump.y_after = host.feet_y().unwrap_or(jump.y_max);
    // Blocks, not floats (t96): `70.5625 -> 71.0` is one block up, and `rose`/`reason` must say so.
    jump.rose = jump.to_ground_block() > jump.from_ground_block();
    jump.reason = (!jump.rose).then_some("no_gain");
    jump
}

impl JumpHost for Bot {
    fn on_ground(&self) -> bool {
        self.movement.as_ref().is_some_and(|m| m.on_ground())
    }

    fn feet_y(&self) -> Option<f32> {
        self.movement
            .as_ref()
            .and_then(|m| m.position())
            .map(|p| p[1])
            .or(Some(self.state.player.position.y))
    }

    fn press_jump(&mut self, down: bool) {
        if let Some(c) = self.controls() {
            c.jump = down;
        }
    }

    fn press_forward(&mut self, down: bool) {
        if let Some(c) = self.controls() {
            c.forward = if down { 1.0 } else { 0.0 };
        }
    }

    async fn step(&mut self) -> Result<(), ActionError> {
        self.next_tick(|_, _| false).await.map(|_| ())
    }
}

impl Bot {
    /// Jumps in place once, and reports whether the body actually rose.
    ///
    /// The press is held until liftoff (so the `JUMP_DELAY_TICKS` cooldown cannot swallow it) and released the
    /// tick the feet leave the ground; the call then waits for the landing. Returns `Ok` with `sent: false` when
    /// the bot was airborne to begin with — a jump that could not be made is not an error.
    pub async fn jump(&mut self) -> Result<Jump, ActionError> {
        if !self.has_physics() {
            return Err(ActionError::NotPossible("jumping needs a physics bot".into()));
        }
        Ok(jump_with(self, false).await)
    }

    /// Jumps while walking toward `target`: turns to face it, holds forward through the liftoff and the flight
    /// and releases on landing. This is the "get onto the block next to me" move a healing path wants.
    ///
    /// Any action still in flight is abandoned first (`Bot::abandon_action`): the usual caller is a healing step
    /// right after a `move_to` timed out, and that dead goto's claim is still registered — when it is released on
    /// our first tick it calls `Controls::stop()` and would clear the very walk key this hop needs (t86/run224:
    /// `jump=1` sent with move vector `(0,0)`, feet never moving horizontally).
    pub async fn hop_toward(&mut self, target: BlockPos) -> Result<Jump, ActionError> {
        self.hop_toward_with(target, false).await
    }

    /// [`Self::hop_toward`], but allowed to widen a one-block hole first.
    ///
    /// Three things a hop checks before it presses anything (t102): that the head cell is air (`no_headroom` with
    /// that cell otherwise -- never a hop into a ceiling), that the same hop from the same cell has not already
    /// gained nothing (`hop_no_progress`), and with `allow_dig`, that the blocking head cell can simply be broken
    /// instead of jumped at.
    pub async fn hop_toward_with(&mut self, target: BlockPos, allow_dig: bool) -> Result<Jump, ActionError> {
        if !self.has_physics() {
            return Err(ActionError::NotPossible("jumping needs a physics bot".into()));
        }
        // A hop asked for while the body is still in the air cannot press jump: `jump_with` would answer
        // `not_on_ground` and the caller would read that as "the terrain refused". Land first — bounded, because a body
        // falling into a void must not turn a hop into a wait.
        let mut waited = 0u32;
        while !self.movement.as_ref().is_some_and(|m| m.on_ground()) && waited < LAND_WAIT_TICKS {
            self.next_tick(|_, _| false).await?;
            waited += 1;
        }
        let Some(eye) = self.movement.as_ref().and_then(|m| m.eye_position()) else {
            return Err(ActionError::NotPossible("hop_toward needs a simulated eye".into()));
        };
        let Some(from) = self.feet_cell() else {
            return Err(ActionError::NotPossible("hop_toward needs a known feet cell".into()));
        };
        let last_dead = self.movement.as_ref().and_then(|m| m.last_dead_hop);
        let mut plan = hop_decision(from, target, allow_dig, last_dead, |p| name_of(self, p));
        if let HopPlan::DigFirst(head) = plan {
            // Widen the hole: one block out of a wall is not a door (t102).
            self.break_block(head).await?;
            let from = self.feet_cell().unwrap_or(from);
            plan = hop_decision(from, target, false, last_dead, |p| name_of(self, p));
        }
        let (y, grounded) = (self.feet_y().unwrap_or(0.0), self.movement.as_ref().is_some_and(|m| m.on_ground()));
        match plan {
            HopPlan::Go => {}
            // Digging was allowed but the cell is still solid (unbreakable, unloaded, or another block appeared):
            // say so instead of hopping into it.
            HopPlan::Blocked(_) | HopPlan::DigFirst(_) => {
                return Ok(unpressed_jump(target, "no_headroom", y, grounded));
            }
            HopPlan::Refused => return Ok(unpressed_jump(target, "hop_no_progress", y, grounded)),
        }
        self.abandon_action();
        let centre = [target[0] as f32 + 0.5, target[1] as f32 + 0.5, target[2] as f32 + 0.5];
        if let Some(c) = self.controls() {
            c.look_at(eye, centre.into());
            c.forward = 1.0;
        }
        let before = self.feet_cell().unwrap_or(from);
        // Say what this hop aims at, so the body-level recorder (t112) stamps `target` on the jump it sees.
        if let Some(m) = self.movement.as_mut() {
            m.hop_target = Some(target);
        }
        let mut jump = jump_with(self, true).await;
        if let Some(m) = self.movement.as_mut() {
            // Nobody left the ground, so nothing will collect it.
            if m.hop_target == Some(target) {
                m.hop_target = None;
            }
        }
        jump.target = Some(target);
        let after = self.feet_cell().unwrap_or(before);
        // Remember a hop that went nowhere, so the next identical attempt is refused instead of pressed (t102).
        if jump.sent && jump.gained() < 1.0 && before == after {
            if let Some(m) = self.movement.as_mut() {
                m.last_dead_hop = Some((before, target));
            }
        }
        Ok(jump)
    }
}

/// The block name at `pos`, when the world view knows it.
fn name_of(bot: &Bot, [x, y, z]: [i32; 3]) -> Option<String> {
    let world = bot.world()?;
    let (view, registry) = (world.view()?, world.registry()?);
    registry.get(view.block(x, y, z)).map(|state| state.name.to_owned())
}

/// A jump that never pressed a key: the verdict, and the target it declined.
fn unpressed_jump(target: BlockPos, reason: &'static str, y: f32, on_ground: bool) -> Jump {
    Jump {
        sent: false,
        reason: Some(reason),
        target: Some(target),
        y_before: y,
        y_ground: on_ground.then(|| y.floor()),
        y_max: y,
        y_after: y,
        rose: false,
        pressed_ticks: 0,
        airborne_ticks: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body that only leaves the ground when the jump key is down *and* the cooldown has expired — the same
    /// gate the physics uses, which is what makes a press-after-landing unreliable without holding it.
    struct FakeHost {
        ground: bool,
        y: f32,
        x: f32,
        delay: u32,
        hold: bool,
        forward: bool,
        /// Every tick's inputs, in order: `(jump, forward)` as the driver last left them.
        seen: Vec<(bool, bool)>,
        /// A tick on which something else (`Controls::stop()` from a released claim) clears both keys.
        clear_on: Option<usize>,
        /// Clear the keys at the start of every tick up to and including this one: several claims released in a row.
        clear_through: Option<usize>,
        airborne: bool,
        /// How many times the body left the ground: one call may lift off at most once (t102).
        liftoffs: u32,
    }

    impl FakeHost {
        fn new(ground: bool, y: f32) -> Self {
            Self {
                ground,
                y,
                x: 0.0,
                delay: 0,
                hold: false,
                forward: false,
                seen: Vec::new(),
                clear_on: None,
                clear_through: None,
                airborne: false,
                liftoffs: 0,
            }
        }
    }

    impl JumpHost for FakeHost {
        fn on_ground(&self) -> bool {
            self.ground
        }

        fn feet_y(&self) -> Option<f32> {
            Some(self.y)
        }

        fn press_jump(&mut self, down: bool) {
            self.hold = down;
        }

        fn press_forward(&mut self, down: bool) {
            self.forward = down;
        }

        async fn step(&mut self) -> Result<(), ActionError> {
            let tick = self.seen.len();
            self.seen.push((self.hold, self.forward));
            if self.clear_on == Some(tick) || self.clear_through.is_some_and(|n| tick <= n) {
                // `release_keys()` → `Controls::stop()`.
                self.hold = false;
                self.forward = false;
            }
            if self.hold && self.ground && self.delay == 0 {
                self.y += 1.25;
                self.ground = false;
                self.airborne = true;
                self.liftoffs += 1;
                self.delay = 10;
                return Ok(());
            }
            if self.airborne {
                // Falls back onto the block it jumped from, one block higher than where it started.
                self.y -= 0.25;
                self.ground = true;
                self.airborne = false;
                if self.forward {
                    self.x += 0.5;
                }
                return Ok(());
            }
            if self.delay > 0 {
                self.delay -= 1;
            }
            Ok(())
        }
    }

    #[test]
    fn a_one_block_hole_is_not_a_door() {
        // t102, live run 20261109-020753: `dig` left a 1x1x1 hole in a wall; the body needs two cells, so the
        // forward+jump press looped for 15 s (five such windows in the relay log, `dy_sum ~ -20.8`, net zero).
        let hole = [150012, 60, 149830];
        let head = [150012, 61, 149830];
        let names = move |p: [i32; 3]| Some(if p == head { "minecraft:stone".to_owned() } else { "minecraft:air".to_owned() });
        assert_eq!(headroom_blocked_by(hole, names), Some(head));
        // allow_dig:false -- say which cell to dig, press nothing.
        assert_eq!(hop_decision(hole, hole, false, None, names), HopPlan::Blocked(head));
        // allow_dig:true -- dig that cell first instead of jumping into its ceiling.
        assert_eq!(hop_decision(hole, hole, true, None, names), HopPlan::DigFirst(head));
        // An open step (two cells of room) is still a hop, and an unloaded chunk is never called blocked.
        let open = move |_: [i32; 3]| Some("minecraft:air".to_owned());
        assert_eq!(hop_decision(hole, hole, false, None, open), HopPlan::Go);
        assert_eq!(headroom_blocked_by(hole, |_| None), None);
    }

    #[test]
    fn a_hop_that_gained_nothing_is_not_pressed_again_from_the_same_cell() {
        let here = [150012, 60, 149830];
        let step = [150013, 61, 149830];
        let open = |_: [i32; 3]| Some("minecraft:air".to_owned());
        // First attempt: allowed.
        assert_eq!(hop_decision(here, step, false, None, open), HopPlan::Go);
        // Same cell, same target, and the last one gained nothing: refuse instead of pressing again.
        assert_eq!(hop_decision(here, step, false, Some((here, step)), open), HopPlan::Refused);
        // The body moved (or the target changed): the memory no longer applies.
        assert_eq!(hop_decision([150012, 60, 149831], step, false, Some((here, step)), open), HopPlan::Go);
        assert_eq!(hop_decision(here, [150014, 61, 149830], false, Some((here, step)), open), HopPlan::Go);
    }

    #[test]
    fn a_hop_without_headroom_reports_the_cell_in_the_way_and_presses_nothing() {
        let target = [150012, 61, 149830];
        let jump = unpressed_jump(target, "no_headroom", 60.5625, true);
        assert!(!jump.sent && jump.pressed_ticks == 0, "no key may go out: {jump:?}");
        assert_eq!(jump.kind(), "no_headroom");
        assert_eq!(jump.blocked(), Some([150012, 62, 149830]), "the cell above the target is what blocks it");
        // Every other verdict blames nothing.
        assert_eq!(unpressed_jump(target, "hop_no_progress", 60.0, true).blocked(), None);
        assert_eq!(unpressed_jump(target, "not_on_ground", 60.0, false).kind(), "not_on_ground");
    }

    #[tokio::test]
    async fn one_call_lifts_off_at_most_once() {
        // t102: the relay shows five 15-second windows in which the body rose 1.252 and came back, over and over.
        // The driver itself must never chain liftoffs -- refusing a repeat is the caller's decision (HopPlan).
        let mut host = FakeHost::new(true, 64.0);
        let jump = jump_with(&mut host, true).await;
        assert_eq!(host.liftoffs, 1, "one call, one liftoff: {:?}", host.seen);
        assert!(jump.sent && jump.rose, "{jump:?}");
        // Both keys are released on landing: the next call has to press them again on purpose.
        assert!(!host.hold && !host.forward, "keys left down: {:?}", host.seen);
    }

    #[tokio::test]
    async fn the_keys_really_get_pressed_and_a_rise_is_observed() {
        let mut host = FakeHost::new(true, 64.0);
        let jump = jump_with(&mut host, false).await;
        // The guard for t81: the old healing path reported a jump without ever pressing the key.
        assert!(host.seen.iter().any(|&(jump, _)| jump), "no jump press reached the controls: {:?}", host.seen);
        assert!(jump.sent && jump.rose, "{jump:?}");
        assert_eq!(jump.kind(), "jump_ok");
        assert_eq!(jump.airborne_ticks, Some(1));
        assert_eq!(jump.y_ground, Some(64.0));
        assert_eq!(jump.target, None);
        assert!((jump.y_max - 65.25).abs() < 0.01, "apex: {}", jump.y_max);
        assert!((jump.y_after - 65.0).abs() < 0.01, "landed on: {}", jump.y_after);
        // Released again: a caller must not leave the key down.
        assert!(!host.hold);
        assert!(!host.forward, "a jump in place must not walk");
        assert!(jump.gained() >= RISE_THRESHOLD);
    }

    /// Build a jump result directly, so a test can pin the exact numbers a session produced.
    #[allow(clippy::too_many_arguments)]
    fn observed(y_before: f32, y_max: f32, y_after: f32, target: Option<[i32; 3]>, reason: Option<&'static str>) -> Jump {
        Jump {
            sent: true,
            reason,
            target,
            y_before,
            y_ground: Some(y_before.floor()),
            y_max,
            y_after,
            rose: y_after.floor() > y_before.floor(),
            pressed_ticks: 6,
            airborne_ticks: Some(3),
        }
    }

    #[test]
    fn the_live_hop_from_70_5625_onto_a_step_at_71_0_is_one_block() {
        // t95, verbatim from the live session: the body stood at y = 70.5625 (this server's standing height is not
        // a whole block), hopped onto the plank it had placed, and landed at y = 71.0 with `on_ground` for four
        // seconds. Measured in floats that rise is 0.4375 — under any threshold — and the old code called it
        // `jump_landed_back` / `gained: 0.4375`. In blocks it is 70 -> 71, and that is the fact to report.
        let jump = observed(70.5625, 71.2, 71.0, Some([150013, 71, 149828]), None);
        assert_eq!(jump.from_ground_block(), 70);
        assert_eq!(jump.to_ground_block(), 71);
        assert_eq!(jump.gained(), 1.0);
        assert_eq!(jump.kind(), "jump_ok");
        // The raw floats stay available for logs, they just no longer decide the verdict.
        assert_eq!(jump.y_before, 70.5625);
        assert_eq!(jump.y_after, 71.0);
    }

    #[test]
    fn up_landed_landed_back_and_fell_are_three_different_things() {
        let target = Some([150013, 71, 149828]);
        // Came down on the block it started from: the only case allowed to say `jump_landed_back`.
        let back = observed(70.5625, 71.2, 70.5625, target, None);
        assert_eq!(back.kind(), "jump_landed_back");
        assert_eq!(back.gained(), 0.0);
        // Got up a block, but not onto the cell it aimed at.
        let missed = observed(70.5625, 72.2, 72.0, target, None);
        assert_eq!(missed.kind(), "jump_up_landed");
        assert_eq!(missed.gained(), 2.0);
        // Ended lower than it started (knocked off, or fell into a hole).
        let fell = observed(70.5625, 71.2, 69.5, target, None);
        assert_eq!(fell.kind(), "jump_fell");
        assert_eq!(fell.gained(), -1.0);
        // Mutually exclusive by construction — spelled out so a future edit cannot collapse two of them.
        let kinds = [back.kind(), missed.kind(), fell.kind()];
        assert_eq!(kinds.iter().collect::<std::collections::HashSet<_>>().len(), 3, "{kinds:?}");
    }

    #[tokio::test]
    async fn a_cooldown_never_swallows_the_press() {
        // Right after a landing the cooldown is still counting down; the press is held until it clears.
        let mut host = FakeHost::new(true, 64.0);
        host.delay = 7;
        let jump = jump_with(&mut host, false).await;
        assert!(jump.rose, "{jump:?}");
        assert_eq!(jump.airborne_ticks, Some(8), "held through the 7 remaining cooldown ticks");
        assert_eq!(jump.pressed_ticks, 8);
    }

    #[tokio::test]
    async fn an_airborne_bot_reports_not_sent_and_presses_nothing() {
        let mut host = FakeHost::new(false, 66.0);
        let jump = jump_with(&mut host, true).await;
        assert!(!jump.sent, "{jump:?}");
        assert_eq!(jump.kind(), "not_on_ground");
        assert_eq!(jump.y_ground, None, "it was standing on nothing");
        assert!(host.seen.is_empty(), "keys were pressed from the air: {:?}", host.seen);
    }

    #[tokio::test]
    async fn a_press_that_never_lifts_off_is_sent_but_not_tried() {
        // The body refuses to leave the ground (a ceiling, or a moved-out-from-under-feet). The press went out,
        // so `sent` is true — but it must not read as a successful attempt.
        let mut host = FakeHost::new(true, 64.0);
        host.delay = u32::MAX;
        let jump = jump_with(&mut host, false).await;
        assert!(jump.sent && !jump.rose, "{jump:?}");
        assert_eq!(jump.kind(), "no_liftoff");
        assert!(jump.airborne_ticks.is_none());
        assert_eq!(jump.pressed_ticks, LIFTOFF_TICKS);
        assert!(!host.hold, "held down forever: `seen` only samples the start of a tick");
    }

    #[tokio::test]
    async fn a_steered_jump_keeps_walking_after_the_keys_are_cleared() {
        // t86/run224: the jump key was re-pressed every tick and survived; the walk key was written once and a
        // leftover claim's `Controls::stop()` cleared it on the first tick — jump=1 sent, move vector (0,0).
        let mut host = FakeHost::new(true, 71.0);
        host.clear_on = Some(0);
        let jump = jump_with(&mut host, true).await;
        assert!(jump.sent, "{jump:?}");
        // The tick after the clear must have forward down again; the driver rewrites it before every tick.
        let after: Vec<(bool, bool)> = host.seen.iter().skip(1).copied().collect();
        assert!(!after.is_empty());
        assert!(after.iter().all(|&(_, forward)| forward), "forward stayed cleared: {:?}", host.seen);
        assert!(host.x > 0.0, "the hop never moved horizontally");
        assert!(!host.forward, "walk key left down");
        assert_eq!(jump.y_ground, Some(71.0));
    }

    #[tokio::test]
    async fn a_claim_released_over_the_first_few_ticks_cannot_stop_the_hop() {
        // t86's accident, three ticks in a row: `Controls::stop()` runs before the physics of each of those ticks,
        // so the press made for that tick is wiped before it can act. The driver simply presses again next tick,
        // which is why the hop still leaves the ground and still travels — one clear can no longer disable it.
        let mut host = FakeHost::new(true, 71.0);
        host.clear_through = Some(2);
        let jump = jump_with(&mut host, true).await;
        assert!(jump.sent && jump.rose, "{jump:?}");
        assert!(host.x > 0.0, "the hop never moved horizontally: {:?}", host.seen);
        assert!(!host.forward && !host.hold, "keys left down");
    }

    #[tokio::test]
    async fn a_jump_in_place_never_touches_the_walk_key() {
        let mut host = FakeHost::new(true, 64.0);
        host.clear_on = Some(0);
        let jump = jump_with(&mut host, false).await;
        assert!(jump.rose, "{jump:?}");
        assert!(host.seen.iter().all(|&(_, forward)| !forward), "{:?}", host.seen);
    }

    /// A world with a solid floor at `y <= floor_y` plus the given extra blocks (the steps, the walls).
    fn world(floor_y: i32, steps: &'static [[i32; 3]]) -> impl Fn([i32; 3]) -> Option<String> {
        move |pos: [i32; 3]| {
            let solid = pos[1] <= floor_y || steps.contains(&pos);
            Some(if solid { "minecraft:stone" } else { "minecraft:air" }.to_owned())
        }
    }

    #[test]
    fn the_nearest_step_beats_the_straightest_one() {
        // t91b, in shape: the bot stands at [100, 70, 100] with a plank under its nose ([101, 70, 101]: side-front,
        // one block), and a same-shape step three blocks towards the goal ([100, 70, 97], goal at [100, 70, 90]).
        // Ordering by the angle to the goal first sent the hop to the far one — `gained = 0.0`. The near one wins.
        let steps: &'static [[i32; 3]] = &[[101, 70, 101], [100, 70, 97]];
        assert_eq!(jump_target([100, 70, 100], [100, 70, 90], world(69, steps)), Some([101, 71, 101]));
    }

    #[test]
    fn a_nearer_step_wins_even_when_it_points_away_from_the_goal() {
        // Healing means "one level up, as near as possible": a step behind the bot is still a step up, and taking
        // it beats walking three blocks to the straightest one.
        let steps: &'static [[i32; 3]] = &[[99, 70, 100], [100, 70, 97]];
        assert_eq!(jump_target([100, 70, 100], [100, 70, 90], world(69, steps)), Some([99, 71, 100]));
    }

    #[test]
    fn inside_one_ring_the_step_towards_the_goal_still_wins() {
        // The direction preference did not go away, it moved to the tie-break: same ring, north step and east step,
        // the goal decides which one.
        let steps: &'static [[i32; 3]] = &[[11, 64, 10], [10, 64, 9]];
        assert_eq!(jump_target([10, 64, 10], [10, 64, 3], world(63, steps)), Some([10, 65, 9]));
        assert_eq!(jump_target([10, 64, 10], [20, 64, 10], world(63, steps)), Some([11, 65, 10]));
    }

    #[test]
    fn a_step_still_needs_room_above_and_a_block_below() {
        // Room above: the step block is there, the cell above it is solid — the body could not stand there.
        let blocked: &'static [[i32; 3]] = &[[11, 64, 10], [11, 65, 10]];
        assert_eq!(jump_target([10, 64, 10], [20, 64, 10], world(63, blocked)), None);
        // Nothing below: air all the way down is a fall, not a step.
        assert_eq!(jump_target([10, 70, 10], [20, 70, 10], world(69, &[])), None);
        // And the cell the body already stands in is never a target (that was the t84 vertical press).
        let self_step: &'static [[i32; 3]] = &[[10, 64, 10]];
        assert_eq!(jump_target([10, 64, 10], [20, 64, 10], world(63, self_step)), None);
    }

    #[test]
    fn a_detour_probes_the_neighbours_instead_of_walking_into_a_wall() {
        // Goal to the east: the perpendicular (cos = 0) neighbours are north and south. North is walkable, so that is
        // the detour; the old code turned the goal direction 90° and walked into whatever happened to be there.
        let here = [10, 64, 10];
        let all = world(63, &[]);
        let picked = detour_candidates(here, [20, 64, 10], &all);
        assert_eq!(picked.first().copied(), Some([10, 64, 9]), "{picked:?}");
        assert!(picked.iter().all(|c| c != &here), "a detour moves: {picked:?}");
        // Block the north neighbour at feet height: the south one takes over, still perpendicular.
        let walled: &'static [[i32; 3]] = &[[10, 64, 9]];
        let picked = detour_candidates(here, [20, 64, 10], world(63, walled));
        assert_eq!(picked.first().copied(), Some([10, 64, 11]), "{picked:?}");
        // Nothing standable around ⇒ an empty probe, so the caller moves nowhere instead of hoping.
        assert!(detour_candidates([10, 90, 10], [20, 90, 10], world(63, &[])).is_empty());
    }
}
