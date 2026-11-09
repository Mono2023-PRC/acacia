//! Fighting: which entity to hit, getting into reach, hitting it until it drops, and backing off when
//! hurt. This is the composition layer over [`Bot::attack`] (one swing, with the human click gap and the
//! aim/anti-cheat handshake) that DeepClef's `@fight` / combat task provides: target selection, an
//! approach loop that re-plans after the target moves, a give-up timeout, and a retreat when health
//! falls below a threshold.
//!
//! What this does not do yet, and why:
//! - **Bows and crossbows**: out of scope for the melee loop; a ranged attack needs the charge/release
//!   timing of `Bot::consume`'s item use against a moving target, and no live server test exists yet.
//! - **Shields**: raising one is a `use_item` that must not be followed by a swing; a fight loop that
//!   blocks between hits is a separate task.
//! - **Fleeing into shelter / eating mid-fight**: see [`CombatOpts::retreat_below`] for the simple form
//!   (back off), and [`Bot::consume`] / [`crate::survival`] for the pieces a caller would combine.
//!
//! Target selection defaults to [`HOSTILE`] kinds: [`Bot::defend`] only fights those, while
//! [`Bot::fight`] fights whatever runtime id it is given (players, animals, a boat being collected).

use std::time::{Duration, Instant};

use acacia_physics::Vec3;

use crate::interact::ENTITY_REACH;
use crate::pathfind::{Goal, GotoOpts};
use crate::{ActionError, Bot};

/// Entity kinds [`Bot::defend`] treats as enemies: vanilla's always-hostile mobs, by identifier
/// without the `minecraft:` prefix (that prefix is optional in [`is_hostile`]).
///
/// Deliberately absent: `piglin` (neutral until provoked), `enderman` (neutral until looked at),
/// `zombified_piglin` (neutral), `wolf`/`bee`/`iron_golem` (neutral defenders), players (only
/// [`Bot::fight`] with their runtime id attacks one), and the passive animals.
pub const HOSTILE: &[&str] = &[
    // Overworld night and caves.
    "zombie",
    "zombie_villager",
    "husk",
    "drowned",
    "skeleton",
    "stray",
    "bogged",
    "wither_skeleton",
    "creeper",
    "spider",
    "cave_spider",
    "silverfish",
    "endermite",
    "slime",
    "magma_cube",
    "witch",
    "phantom",
    "pillager",
    "vindicator",
    "evoker",
    "ravager",
    "vex",
    "warden",
    "breeze",
    // Nether.
    "ghast",
    "blaze",
    "hoglin",
    "zoglin",
    "piglin_brute",
    // End and ocean.
    "shulker",
    "guardian",
    "elder_guardian",
    "ender_dragon",
    "wither",
];

/// Whether `kind` (`minecraft:zombie`, or bare `zombie`) is one of [`HOSTILE`].
pub fn is_hostile(kind: &str) -> bool {
    let bare = kind.strip_prefix("minecraft:").unwrap_or(kind);
    HOSTILE.contains(&bare)
}

/// How [`Bot::fight`] behaves.
#[derive(Debug, Clone)]
pub struct CombatOpts {
    /// How close to the target the bot stands before swinging (blocks, feet to feet). Kept under
    /// [`ENTITY_REACH`] so the target's own movement does not break the reach.
    pub range: f32,
    /// Give up with `Rejected` after this long (the target may be unreachable, regenerating, or a
    /// player that keeps running).
    pub timeout: Duration,
    /// Back off when health falls to this many points (out of 20); `None` never backs off.
    /// A retreat walks [`CombatOpts::flee_distance`] blocks away from the target and then the fight
    /// resumes: at most [`CombatOpts::MAX_RETREATS`] times per [`Bot::fight`] call.
    pub retreat_below: Option<f32>,
    /// How far away a retreat walks.
    pub flee_distance: f32,
    /// Path settings for the approach and the retreat (`give_up_ticks` bounds one approach attempt).
    pub goto: GotoOpts,
}

impl CombatOpts {
    /// Retreats per fight before the loop ignores `retreat_below`.
    pub const MAX_RETREATS: u32 = 3;

    /// Stand this close to swing.
    pub const DEFAULT_RANGE: f32 = ENTITY_REACH - 0.4;

    /// Approach attempts that get nowhere before the fight reports the target unreachable.
    pub const MAX_MISSES: u32 = 3;

    /// Swings in a row the server reports no hurt or health loss for, before the fight gives up instead of
    /// swinging for the whole timeout (t27: 538 swings, 0 `Hurt`).
    pub const MAX_SWINGS_WITHOUT_HIT: u32 = 5;
    /// Ticks to wait for a swing's hurt/health echo: the server reports it a few ticks after the transaction.
    pub const HIT_ECHO_TICKS: u32 = 8;
}

impl Default for CombatOpts {
    fn default() -> Self {
        Self {
            range: Self::DEFAULT_RANGE,
            timeout: Duration::from_secs(45),
            retreat_below: Some(5.0),
            flee_distance: 8.0,
            // An approach attempt is 3 s without getting closer; the loop re-plans until `timeout`.
            goto: GotoOpts { give_up_ticks: Some(60), ..GotoOpts::default() },
        }
    }
}

/// What a fight did, for the tool layer's result: no scraping numbers out of the error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FightReport {
    /// The entity fought.
    pub target: u64,
    /// Swings sent (each one a `UseItemOnEntity` Attack transaction).
    pub swings: u32,
    /// Swings the server answered: a hurt animation, or the target's health dropping.
    pub hits: u32,
    /// Retreats taken because the bot's own health was low.
    pub retreats: u32,
}

impl FightReport {
    /// Whether the target took damage at all.
    pub fn landed(&self) -> bool {
        self.hits > 0
    }
}

impl Bot {
    /// The nearest [`HOSTILE`] entity within `radius` blocks of the eye, with its distance.
    /// Needs [`crate::BotConfig::trackers`] entity tracking.
    pub fn nearest_enemy(&self, radius: f32) -> Option<(u64, f32)> {
        nearest_by(
            self.state.entities.iter().filter(|e| is_hostile(&e.kind)).map(|e| (e.runtime_id, [e.position.x, e.position.y, e.position.z])),
            self.eye_position(),
            radius,
        )
    }

    /// Every tracked [`HOSTILE`] entity, nearest first.
    pub fn enemies(&self) -> Vec<(u64, f32)> {
        let eye = self.eye_position();
        let mut all: Vec<(u64, f32)> = self
            .state
            .entities
            .iter()
            .filter(|e| is_hostile(&e.kind))
            .map(|e| (e.runtime_id, dist(eye, [e.position.x, e.position.y, e.position.z])))
            .collect();
        all.sort_by(|a, b| a.1.total_cmp(&b.1));
        all
    }

    /// Fights the entity with `runtime_id` until it is gone (dead, despawned or out of tracking), the
    /// call is cancelled, or [`CombatOpts::timeout`] passes.
    ///
    /// The target must be tracked ([`crate::BotConfig::trackers`] entity tracking, i.e. a physics bot
    /// with `entities: true`) and within sight of a physics bot. One iteration is: back off if hurt,
    /// swing when in reach, else walk to within [`CombatOpts::range`] and swing next time. A swing the
    /// server's reach check refuses (`NotPossible`/`NotVisible`: the target moved) is retried rather
    /// than returned, so a moving target does not end the fight.
    pub async fn fight(&mut self, runtime_id: u64, opts: &CombatOpts) -> Result<(), ActionError> {
        self.fight_with_report(runtime_id, opts).await.map(|_| ())
    }

    /// [`Bot::fight`], also returning what happened: swings sent, swings the server acknowledged, retreats.
    pub async fn fight_with_report(&mut self, runtime_id: u64, opts: &CombatOpts) -> Result<FightReport, ActionError> {
        let report = |swings: u32, hits: u32, retreats: u32| FightReport { target: runtime_id, swings, hits, retreats };
        // Every exit stores the counts, so a caller given an error still learns how many swings went out and
        // how many the server counted (`Bot::last_fight`).
        macro_rules! finish {
            ($swings:expr, $hits:expr, $retreats:expr) => {{
                let r = report($swings, $hits, $retreats);
                self.last_fight = Some(r);
                return Ok(r);
            }};
        }
        macro_rules! give_up {
            ($swings:expr, $hits:expr, $retreats:expr, $err:expr) => {{
                self.last_fight = Some(report($swings, $hits, $retreats));
                return Err($err);
            }};
        }
        let deadline = Instant::now() + opts.timeout;
        let mut retreats = 0;
        let mut swings = 0u32;
        // Approach attempts in a row that did not get closer: a target a pit or a cliff away is reported as
        // unreachable instead of burning the timeout, and a target below/above gets a path that may dig.
        let mut misses = 0u32;
        let mut hits = 0u32;
        // Swings in a row with no hurt and no health loss: a click the server is not counting (a drifted body,
        // an anticheat's reach check) ends the fight with a reason instead of burning the timeout.
        let mut dry = 0u32;
        loop {
            self.cancel_point()?;
            if Instant::now() >= deadline {
                give_up!(
                    swings,
                    hits,
                    retreats,
                    ActionError::Rejected(format!(
                        "the fight with entity {runtime_id} did not end in {:?} ({swings} swings, {hits} hits, {retreats} retreats)",
                        opts.timeout
                    ))
                );
            }
            let Some((feet, health)) = self.state.entities.get(runtime_id).map(|e| {
                let f = e.feet();
                (f, e.health())
            }) else {
                // Removed from tracking: killed (BDS removes the corpse), despawned or unloaded.
                finish!(swings, hits, retreats);
            };
            if health.is_some_and(|h| h <= 0.0) {
                finish!(swings, hits, retreats);
            }
            let eye = self.eye_position();
            let target = [feet.x, feet.y, feet.z];
            let distance = dist(eye, [feet.x, feet.y + 0.9, feet.z]);
            if opts.retreat_below.is_some_and(|below| self.state.player.health <= below) && retreats < CombatOpts::MAX_RETREATS {
                retreats += 1;
                let cell = feet_cell(feet);
                tracing::debug!(runtime_id, retreats, health = self.state.player.health, "backing off");
                self.goto_with(Goal::Away(cell, opts.flee_distance), opts.goto.clone()).await?;
                // A retreat the server refuses (no path back) must not end the fight.
                let _ = &target;
                continue;
            }
            if distance <= ENTITY_REACH {
                let mark = self.hurt_mark(runtime_id);
                match self.attack(runtime_id).await {
                    Ok(()) => {
                        swings += 1;
                        if self.swing_landed(runtime_id, mark).await? {
                            hits += 1;
                            dry = 0;
                        } else {
                            dry += 1;
                            if dry >= CombatOpts::MAX_SWINGS_WITHOUT_HIT {
                                give_up!(
                                    swings,
                                    hits,
                                    retreats,
                                    ActionError::Rejected(format!(
                                        "no_effect_on_target: {swings} swings at entity {runtime_id}, none hurt it ({dry} in a row): the server is not counting these clicks (check the body's position sync)",
                                    ))
                                );
                            }
                        }
                    }
                    Err(ActionError::NotPossible(_) | ActionError::NotVisible(_)) => {
                        // Moved out of reach or behind a block between the aim and the swing: approach again.
                        self.next_tick(|_, _| false).await?;
                    }
                    Err(e) => give_up!(swings, hits, retreats, e),
                }
                continue;
            }
            let cell = feet_cell(feet);
            let goal = Goal::Near(cell, opts.range.min(ENTITY_REACH - 0.2));
            let mine = self.block_position().map_or(0, |p| cell[1] - p[1]);
            let mut approach = opts.goto.clone();
            if mine.abs() >= 2 {
                // A target two or more blocks below (a pit) or above (a slope) is not walkable: let the path
                // dig down to it, as a player would.
                approach.search.allow_dig = true;
            }
            match self.goto_with(goal, approach).await {
                Ok(()) => misses = 0,
                // The target moved while we walked, or the way is blocked right now: try again.
                Err(ActionError::NotPossible(_) | ActionError::Rejected(_)) => {
                    misses += 1;
                    if misses >= CombatOpts::MAX_MISSES {
                        let where_ = if mine < 0 { "below" } else { "above" };
                        give_up!(
                            swings,
                            hits,
                            retreats,
                            ActionError::NotPossible(format!(
                                "unreachable target: entity {runtime_id} at {cell:?} is {} blocks {where_} the bot; no path there in {misses} tries ({swings} swings, {hits} hits)",
                                mine.abs()
                            ))
                        );
                    }
                    self.next_tick(|_, _| false).await?;
                }
                Err(e) => give_up!(swings, hits, retreats, e),
            }
        }
    }

    /// The counts of the last [`Bot::fight`] / [`Bot::fight_with_report`], even when it failed: swings sent,
    /// swings the server counted as hits, retreats (`FightReport`).
    pub fn last_fight(&self) -> Option<FightReport> {
        self.last_fight
    }

    /// Fights the nearest [`HOSTILE`] entity within `radius` blocks; `Ok(None)` when there is none.
    /// The id fought is returned so a caller can log or loot it.
    pub async fn defend(&mut self, radius: f32, opts: &CombatOpts) -> Result<Option<u64>, ActionError> {
        let Some((id, _)) = self.nearest_enemy(radius) else { return Ok(None) };
        self.fight(id, opts).await?;
        Ok(Some(id))
    }
}

impl Bot {
    /// The target's hurt/health fingerprint before a swing; [`Bot::swing_landed`] compares against it.
    fn hurt_mark(&self, runtime_id: u64) -> (u32, Option<f32>) {
        (self.state.hurts.count(runtime_id), self.state.entities.get(runtime_id).and_then(|e| e.health()))
    }

    /// Whether the swing just sent was counted by the server: a hurt animation, or the target's health dropping.
    /// Waits up to [`CombatOpts::HIT_ECHO_TICKS`] for the echo (the transaction and the hurt are not one tick).
    async fn swing_landed(&mut self, runtime_id: u64, before: (u32, Option<f32>)) -> Result<bool, ActionError> {
        for _ in 0..CombatOpts::HIT_ECHO_TICKS {
            let (hurts, health) = self.hurt_mark(runtime_id);
            if hurts > before.0 {
                return Ok(true);
            }
            if let (Some(now), Some(then)) = (health, before.1) {
                if now < then {
                    return Ok(true);
                }
            }
            self.next_tick(|_, _| false).await?;
        }
        Ok(false)
    }
}

/// Feet block position of a wire position.
fn feet_cell(feet: acacia_client::proto::types::Vec3f) -> [i32; 3] {
    [feet.x.floor() as i32, (feet.y + 0.01).floor() as i32, feet.z.floor() as i32]
}

/// Distance from `a` to `b` (blocks).
pub(crate) fn dist(a: Vec3, b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// The nearest `(runtime_id, position)` within `radius` of `from`. Ties go to the earlier entry.
pub(crate) fn nearest_by(items: impl Iterator<Item = (u64, [f32; 3])>, from: Vec3, radius: f32) -> Option<(u64, f32)> {
    let mut best: Option<(u64, f32)> = None;
    for (id, pos) in items {
        let d = dist(from, pos);
        if d <= radius && best.is_none_or(|(_, best)| d < best) {
            best = Some((id, d));
        }
    }
    best
}

#[cfg(test)]
mod tests;
