//! Client-side movement for server-authoritative servers: runs the physics simulation every 50 ms
//! and reports the result in `PlayerAuthInput`, obeying teleports, corrections and knockback.

mod auth_input;
mod controls;
pub(crate) mod equipment;
mod flight;
mod fork;
mod glide;
mod idle;
mod jump;
mod keys;
mod lifecycle;
mod packets;
mod rewind;
mod rotation_noise;
#[cfg(test)]
mod tests;

pub use controls::Controls;
pub(crate) use controls::{attach_actions, VehicleCorrection};
pub use fork::Rollout;
pub use jump::{
    detour_candidates, feet_cell, headroom_blocked_by, headroom_blocked_solid, hop_decision, hop_room_blocked_by, jump_room_blocked_by, jump_room_blocked_by_state, jump_target, solid_at, HopPlan, Jump,
    STEP_SCAN_RADIUS,
};
pub use idle::Idle;

use acacia_client::proto::packets::{PlayerAuthInput, PlayerAuthInputBlockActionItem, PlayerAuthInputInputMode, PlayerAuthInputInteractionModel};
use acacia_physics::{self as physics, Effects, Equipment, PlayerState, Vec3, WorldView};

use keys::KeyLatch;
use rewind::History;
pub(crate) use rotation_noise::RotationNoise;

/// Players' wire positions are eye positions this far above the feet.
pub(crate) const EYE_HEIGHT: f32 = crate::state::PlayerState::EYE_HEIGHT;

pub struct Movement {
    pub controls: Controls,
    /// [`crate::BotConfig::mouse_input`].
    pub(crate) mouse_input: bool,
    /// Block actions to send with the next tick's `PlayerAuthInput`.
    pub(crate) pending_actions: Vec<PlayerAuthInputBlockActionItem>,
    /// Server corrections received: each one means the simulation disagreed with the server.
    pub corrections: u32,
    /// Server teleports received (including setbacks).
    pub teleports: u32,
    /// Corrections of a vehicle the bot drives: each one means the vehicle simulation disagreed.
    pub vehicle_corrections: u32,
    /// The latest vehicle correction, for the vehicle simulation to take.
    pub(crate) vehicle_correction: Option<VehicleCorrection>,
    /// The server's vehicle motion in the latest vehicle correction.
    pub last_vehicle_delta: Option<Vec3>,
    /// Ticks spent with the spawn chunk loaded but movement not yet started.
    pub(crate) spawn_wait: u32,
    /// The last hop that gained nothing: `(cell it started in, cell it aimed at)`. An identical hop is refused
    /// instead of pressed again -- t102's in-place loop (five 15-second windows of hops, net movement zero).
    pub(crate) last_dead_hop: Option<([i32; 3], [i32; 3])>,
    /// The movement attribute of the simulation a dimension change stopped: the next one arrives with it.
    carried_attribute: Option<f32>,
    /// Holding "use" on an item (eating, drinking), which slows movement.
    pub(crate) using_item: bool,
    /// An elytra is worn (set by the bot every tick).
    pub(crate) elytra: bool,
    physics: Option<PlayerState>,
    tick: u64,
    /// When `tick` started counting: the packet's tick follows this clock, not the loop's iterations.
    tick_start: std::time::Instant,
    /// Corrections applied at once because their tick had already passed ([`packets`] `correction_lands_now`).
    pub late_corrections: u64,
    /// Our tick when the server last told us where the player is (`MovePlayer` or an applied correction).
    /// `None` until then: vanilla movement is client-authoritative, so the reference can be stale forever.
    pub last_confirmed: Option<u64>,
    /// Replays keep scheduling every correction for its recorded tick: they re-simulate a recording, they are not
    /// a live client whose tick may be ahead of the server's.
    recorded_corrections: bool,
    /// Replays advance the tick by one per recorded input, not by the wall clock (their inputs are not real time).
    recorded_ticks: bool,
    prev_jump: bool,
    /// A teleport superseded by its correction still needs `HandledTeleport` sent.
    ack_teleport: bool,
    /// The queued teleport is a BDS one, which applies liquid currents on its hold tick.
    current_on_landing: bool,
    keys: KeyLatch,
    effects: Effects,
    equipment: Equipment,
    /// The flying flag of the server's last `UpdateAbilities` (see `set_abilities`).
    server_flying: Option<bool>,
    history: History,
    /// Solid entity boxes for the next tick and the ones seen last ([`Movement::set_solids`]).
    solids: Vec<physics::Aabb>,
    seen_solids: Vec<physics::Aabb>,
    /// Replay only: the recorded `WantDown` (bot traces before 2026-10-02 never sent it), else it follows sneak.
    pub(crate) recorded_want_down: Option<bool>,
    last_setback: Option<u64>,
    /// Live bots only: sims, tests and replays send `controls` rotations exactly.
    pub(crate) rotation_noise: Option<RotationNoise>,
    /// The next inputs carry no movement keys: Oomph bans moving on both sides of an `ItemStackRequest`.
    pub(crate) still_ticks: u8,
    /// The last jump **the body** made, whichever emitter pressed the key: the pathfinder's own `steer`/`maneuver`/
    /// `shortcut`/`leap`, the bot-level `hop_toward`, or a replay.
    ///
    /// t112: a `move_to` that climbed a step was finished by the pathfinder's `Ascend`, so the agent's `jump_last`
    /// (filled only by the heal path) had nothing to show, although the relay saw `jump=1` and the feet rose a block
    /// -- the nine runs before it all reported "no sample" for the same reason. The emitters only write
    /// `controls.jump`, so the one place that sees every jump is here, where the key is turned into a tick.
    last_jump: Option<Jump>,
    /// Jumps finished so far. Callers compare it before and after an action to know whether `last_jump` is theirs:
    /// a record left over from an earlier walk must not be reported as this walk's evidence.
    jump_count: u64,
    /// The jump in progress: it opens when the feet leave the ground and closes when they land again.
    airborne: Option<Airborne>,
    /// The cell a bot-level hop aims at, so the jump it ends up making can say what it was aiming for.
    pub(crate) hop_target: Option<[i32; 3]>,
}

/// A jump between leaving the ground and landing: what the recorder needs to fill in a [`Jump`] afterwards.
struct Airborne {
    y_before: f32,
    y_ground: Option<f32>,
    max_y: f32,
    pressed_ticks: u32,
    start_tick: u64,
}

/// A jump still in the air after this many ticks is recorded as it stands: a body that fell into water, was
/// teleported or died mid-air must not swallow the record of the jump that started it.
const AIRBORNE_LIMIT_TICKS: u64 = 80;

/// A correction moving the bot further than this (blocks) is a setback: paths and actions pause.
pub const SETBACK: f32 = 0.05;

impl Movement {
    /// Replays drive the tick from the recording, not from the wall clock (see [`Movement::recorded_ticks`]).
    /// The tick the next `PlayerAuthInput` will report.
    pub(crate) fn current_tick(&self) -> u64 {
        self.tick
    }

    pub(crate) fn use_recorded_ticks(&mut self) {
        self.recorded_ticks = true;
        self.recorded_corrections = true;
    }

    pub fn new() -> Self {
        Self {
            controls: Controls::default(),
            mouse_input: false,
            pending_actions: Vec::new(),
            corrections: 0,
            teleports: 0,
            vehicle_corrections: 0,
            vehicle_correction: None,
            last_vehicle_delta: None,
            spawn_wait: 0,
            last_dead_hop: None,
            carried_attribute: None,
            using_item: false,
            elytra: false,
            physics: None,
            tick: 0,
            tick_start: std::time::Instant::now(),
            late_corrections: 0,
            last_confirmed: None,
            recorded_corrections: false,
            recorded_ticks: false,
            prev_jump: false,
            ack_teleport: false,
            current_on_landing: false,
            recorded_want_down: None,
            keys: KeyLatch::default(),
            effects: Effects::default(),
            equipment: Equipment::default(),
            server_flying: None,
            history: History::default(),
            solids: Vec::new(),
            seen_solids: Vec::new(),
            last_setback: None,
            rotation_noise: None,
            still_ticks: 0,
            last_jump: None,
            jump_count: 0,
            airborne: None,
            hop_target: None,
        }
    }

    /// The last jump the body made, whoever pressed the key: the pathfinder's four emitters and the bot's own hop
    /// all end up writing `controls.jump`, so this is the only place that sees every one of them (t112).
    pub fn last_jump(&self) -> Option<&Jump> {
        self.last_jump.as_ref()
    }

    /// How many jumps have finished: compare before and after an action to tell a fresh record from a stale one.
    pub fn jump_count(&self) -> u64 {
        self.jump_count
    }

    /// Records a jump from the tick that simulated it: opens one when the feet leave the ground, closes it on
    /// landing (or after [`AIRBORNE_LIMIT_TICKS`], for a body that fell into water or was teleported). Called from
    /// [`Movement::tick`] with the state around the physics step, so every emitter is covered.
    fn observe_jump(&mut self, ground_before: bool, y_before: f32, jump_pressed: bool, on_ground: bool, y_now: f32) {
        if self.airborne.is_none() {
            if ground_before && !on_ground {
                self.airborne = Some(Airborne {
                    y_before,
                    y_ground: Some(y_before.floor()),
                    max_y: y_now.max(y_before),
                    pressed_ticks: u32::from(jump_pressed),
                    start_tick: self.tick,
                });
            }
            return;
        }
        let too_old = {
            let a = self.airborne.as_mut().expect("checked above");
            a.max_y = a.max_y.max(y_now);
            if jump_pressed {
                a.pressed_ticks += 1;
            }
            !on_ground && self.tick.saturating_sub(a.start_tick) >= AIRBORNE_LIMIT_TICKS
        };
        if !on_ground && !too_old {
            return;
        }
        let Some(a) = self.airborne.take() else { return };
        self.jump_count += 1;
        self.last_jump = Some(Jump {
            sent: true,
            reason: None,
            target: self.hop_target.take(),
            y_before: a.y_before,
            y_ground: a.y_ground,
            y_max: a.max_y,
            y_after: y_now,
            rose: y_now.floor() > a.y_before.floor(),
            pressed_ticks: a.pressed_ticks,
            airborne_ticks: Some((self.tick.saturating_sub(a.start_tick)) as u32),
        });
    }

    /// Ticks since a correction moved the bot more than [`SETBACK`] (ulp-sized ones are not setbacks).
    pub fn setback_age(&self) -> Option<u64> {
        self.last_setback.map(|t| self.tick.saturating_sub(t))
    }

    /// The worn armour for the ticks from here on (see `equipment::worn`); true when it changed.
    pub(crate) fn set_equipment(&mut self, equipment: Equipment) -> bool {
        let changed = equipment != self.equipment;
        if changed {
            tracing::debug!(our_tick = self.tick, ?equipment, "equipment");
            self.equipment = equipment;
        }
        changed
    }

    /// The worn armour the simulation uses.
    pub fn equipment(&self) -> Equipment {
        self.equipment
    }

    /// The freeze the simulation holds, and the one the server has reported for input `tick`.
    pub(crate) fn freeze_at(&self, tick: u64) -> (f32, Option<f32>) {
        (self.physics.as_ref().map_or(0.0, |p| p.freeze), self.history.server_freeze(tick))
    }

    /// Simulated feet position.
    pub fn position(&self) -> Option<Vec3> {
        self.physics.as_ref().map(|p| p.pos)
    }

    /// The simulation's on-ground flag (false before movement starts).
    pub fn on_ground(&self) -> bool {
        self.physics.as_ref().is_some_and(|p| p.on_ground)
    }

    pub fn eye_position(&self) -> Option<Vec3> {
        self.physics.as_ref().map(PlayerState::eye_position)
    }

    /// The simulation is sprinting.
    pub fn sprinting(&self) -> bool {
        self.physics.as_ref().is_some_and(|p| p.sprinting)
    }

    /// The simulation is sneaking.
    pub fn sneaking(&self) -> bool {
        self.physics.as_ref().is_some_and(|p| p.sneaking)
    }

    /// Collision boxes of the solid entities as tracked now (world space; see [`physics::WithEntities`]). They
    /// take effect a tick late: strict BDS collides a tick with the shapes it fetched on the tick before.
    pub fn set_solids(&mut self, boxes: Vec<physics::Aabb>) {
        self.solids = std::mem::replace(&mut self.seen_solids, boxes);
    }

    /// Simulates one tick and returns the `PlayerAuthInput` describing it.
    pub fn tick(&mut self, world: &impl WorldView) -> Option<PlayerAuthInput> {
        let st = self.physics.as_mut()?;
        let mut c = self.controls;
        if let Some(noise) = self.rotation_noise.as_mut() {
            (c.yaw, c.pitch) = noise.apply(c.yaw, c.pitch);
        }
        // After the noise, which is wider than the guard's nudge and would put angles back on step edges.
        if st.gliding {
            (c.yaw, c.pitch) = physics::steady_glide_look(c.yaw, c.pitch);
        }
        if self.still_ticks > 0 {
            self.still_ticks -= 1;
            (c.forward, c.strafe) = (0.0, 0.0);
        }
        let replayed = self.history.apply(st, world);
        if replayed > 0 {
            tracing::debug!(tick = self.tick, replayed, "rewound");
        }
        let solids = std::mem::take(&mut self.solids);
        let world = &physics::WithEntities { world, boxes: &solids };
        st.equipment = self.equipment;
        self.keys.mouse_input = self.mouse_input;
        let input = self.keys.advance(&c, st, world, self.using_item, self.recorded_want_down.unwrap_or(c.sneak));
        let jump = input.jump;
        let (was_sprinting, was_sneaking, was_swimming, was_gliding) = (st.sprinting, st.sneaking, st.swimming, st.gliding);
        let was_flying = st.flight.flying;
        let knockback = st.knockback;
        st.equipment.elytra = self.elytra;
        // The jump recorder reads the feet and the ground state around the physics step (t112).
        let (ground_before, y_before) = (st.on_ground, st.pos[1]);
        let mut out = physics::tick(st, &input, world);
        let (ground_after, y_after) = (st.on_ground, st.pos[1]);
        if !st.gliding {
            self.controls.glide = false;
        }
        if was_flying != st.flight.flying {
            self.controls.fly = st.flight.flying;
        }
        if out.teleported && std::mem::take(&mut self.current_on_landing) {
            physics::apply_current(st, world);
            out.delta = st.vel;
        }
        out.teleported |= std::mem::take(&mut self.ack_teleport);
        let edges = auth_input::Edges {
            sprint: (
                st.sprinting && !was_sprinting || st.sprint_start_cancelled,
                !st.sprinting && was_sprinting || st.sprint_start_cancelled,
            ),
            sneak: (st.sneaking && !was_sneaking, !st.sneaking && was_sneaking),
            jump: (jump && !self.prev_jump, !jump && self.prev_jump),
            swim: (st.swimming && !was_swimming, !st.swimming && was_swimming),
            glide: (st.gliding && !was_gliding, !st.gliding && was_gliding),
            fly: (st.flight.flying && !was_flying, !st.flight.flying && was_flying),
            sneaking: st.sneaking,
            sprinting: st.sprinting,
            sprint_key: c.sprint,
        };
        self.prev_jump = jump;
        // The packet's `Tick` is the server clock's count: a loop that only manages 5 ticks a second must not
        // report tick+1 every 200 ms (relay t39: 760 packets in 2.5 min, 4x behind, inputs dropped as stale).
        // Replays are unaffected: they run faster than the wall clock, so `wall` stays behind `tick + 1`.
        self.tick = if self.recorded_ticks {
            self.tick + 1
        } else {
            let wall = self.tick_start.elapsed().as_millis() as u64 / 50;
            wall.max(self.tick + 1)
        };
        self.history.record(self.tick, input, knockback, st, solids);
        // Movement starts only after the bot has left the loading screen (bot.rs).
        let mut packet = auth_input::build(&input, &out, &edges, self.tick, true);
        if self.mouse_input {
            packet.input_mode = PlayerAuthInputInputMode::Mouse;
            packet.interaction_model = PlayerAuthInputInteractionModel::Crosshair;
        }
        tracing::trace!(tick = self.tick, forward = c.forward, sprint = c.sprint, sprinting = st.sprinting, swimming = st.swimming, speed = st.movement_speed, freeze = st.freeze, yaw = c.yaw, pitch = c.pitch,
            pos = ?out.position, delta = ?out.delta, teleported = out.teleported, flags = ?packet.input_data, "auth input");
        // What the packet says, once a second: t37 saw 5 `PlayerAuthInput`/s that the server never acted on, and
        // the only way to tell a wrong `delta`/`on_ground`/flag from a wrong position is to look at the packet.
        if self.tick % 20 == 0 {
            tracing::info!(
                target: "acacia_bot::auth",
                "[auth] tick={} pos=[{:.2}, {:.2}, {:.2}] delta=[{:.4}, {:.4}, {:.4}] on_ground={} teleported={} flags={:?}",
                self.tick, out.position[0], out.position[1], out.position[2],
                out.delta[0], out.delta[1], out.delta[2], st.on_ground, out.teleported, packet.input_data,
            );
        }
        self.observe_jump(ground_before, y_before, jump, ground_after, y_after);
        Some(packet)
    }
}

impl Default for Movement {
    fn default() -> Self { Self::new() }
}
