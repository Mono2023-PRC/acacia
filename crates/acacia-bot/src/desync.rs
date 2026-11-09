//! How far the client's own prediction has drifted from the position the server reports.
//!
//! Every interaction the bot sends carries positions it computed from its own simulated body: a click far
//! from where the server thinks the bot is gets dropped (t27: 538 attack transactions, no `Hurt`, no health
//! change, the click 8 blocks off). The relay dropped NetEase's `CorrectPlayerMovePrediction` (Shuttler
//! `main.go` packet 161), so a client that never learns it drifts — these numbers are how we see it from the
//! bot side, and the guard keeps it from firing clicks that cannot land.

use acacia_physics::Vec3;

use crate::{ActionError, Bot};

/// Above this many blocks between the predicted and the reported position a click is not sent.
/// Above this many blocks between the predicted and the reported position a click is refused. Steady state on
/// the rental server is ~1.07 (network jitter plus the correction's round trip), so a 1-block limit refused every
/// interaction forever (t41); 2 leaves room for the jitter while still catching a real drift (t27: 8 blocks).
pub const DESYNC_LIMIT: f32 = 2.0;
/// Ticks after which the last server-reported position stops being evidence: vanilla movement is client-authoritative
/// and the server may never re-send it, so a difference from a reference older than this says nothing about who is
/// right (t61: 2.5 "blocks apart" while every interaction was accepted server-side).
pub const FRESH_REFERENCE_TICKS: u64 = 40;

/// Ticks since the last server-reported position, `None` when there never was one. A confirmation that predates the
/// movement simulation (the spawn one) still counts: the reference is that old, not absent.
pub fn confirmed_ticks(now: u64, last: Option<u64>, confirmations: u64) -> Option<u64> {
    match (last, confirmations) {
        (Some(last), _) => Some(now.saturating_sub(last)),
        (None, 0) => None,
        (None, _) => Some(now),
    }
}

/// Whether a drift may refuse an interaction at all: only with at least one server-confirmed position and a recent
/// one. `position_confirmations == 0` means every number we have is the spawn position (t71: the server position
/// froze at spawn while the client walked 25 blocks, so a 2-block limit would refuse everything forever).
pub fn may_interact(confirmations: u64, confirmed_ticks: Option<u64>, drift: Option<f32>) -> bool {
    if confirmations == 0 || !reference_is_fresh(confirmed_ticks) {
        return true;
    }
    drift.is_none_or(|blocks| blocks <= DESYNC_LIMIT)
}

/// Whether a reference confirmed `ticks` ago may still be used to call the client out of sync.
pub fn reference_is_fresh(confirmed_ticks: Option<u64>) -> bool {
    confirmed_ticks.is_some_and(|ticks| ticks <= FRESH_REFERENCE_TICKS)
}

/// How much further than the goal a `move_to` may end while the body is still catching up: the arrival is judged
/// on the server's position, which lags the client's own walking by up to the drift.
pub const ARRIVAL_SLACK: f32 = 1.5;
/// Ticks of drift after which [`Bot::desync_ticks`] is worth reporting as "stuck desynced".
pub const DESYNC_TICKS: u32 = 20;

/// The two positions and how far apart they are, for `status` and the trace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionDesync {
    /// The body the client simulates and interacts from (vanilla's own position).
    pub predicted: Vec3,
    /// The position the server last reported (`PlayerAuthInput` corrections live here).
    pub server: Vec3,
    /// Predicted minus server, per axis.
    pub delta: [f32; 3],
    /// Straight-line distance between the two.
    pub blocks: f32,
    /// Sum of the absolute per-axis differences.
    pub manhattan: f32,
}

/// [`PositionDesync`] from two positions: the pure arithmetic, so it is testable without a server.
pub fn desync_of(predicted: Vec3, server: Vec3) -> PositionDesync {
    let delta: [f32; 3] = std::array::from_fn(|i| predicted[i] - server[i]);
    PositionDesync {
        predicted,
        server,
        delta,
        blocks: (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt(),
        manhattan: delta.iter().map(|d| d.abs()).sum(),
    }
}

impl Bot {
    /// The predicted-versus-reported position gap, or `None` for a bot without a simulated body.
    pub fn position_desync(&self) -> Option<PositionDesync> {
        let predicted = self.movement.as_ref().and_then(|m| m.position())?;
        let p = &self.state.player.position;
        Some(desync_of(predicted, [p.x, p.y, p.z]))
    }

    /// The block cell the **server** has the bot in: what `status.block_position` shows and the only position an
    /// arrival may be judged by (t37: `move_to` reported `arrived` while the server never moved the bot).
    pub fn server_block(&self) -> Option<acacia_physics::BlockPos> {
        let p = &self.state.player.position;
        Some([p.x.floor() as i32, (p.y + 0.01).floor() as i32, p.z.floor() as i32])
    }

    /// Ticks in a row [`Bot::position_desync`] has been over [`DESYNC_LIMIT`] (0 when in sync).
    pub fn desync_ticks(&self) -> u32 {
        self.desync_ticks
    }

    /// Counts this tick's drift: the tick loop calls it, nothing else does.
    pub(crate) fn note_desync(&mut self) {
        let drifting = self.position_desync().is_some_and(|d| d.blocks > DESYNC_LIMIT);
        self.desync_ticks = if drifting { self.desync_ticks.saturating_add(1) } else { 0 };
    }

    /// Ticks since the server last said where the player is (`None` = never confirmed).
    pub fn server_confirmed_ticks(&self) -> Option<u64> {
        let movement = self.movement.as_ref()?;
        confirmed_ticks(movement.current_tick(), movement.last_confirmed, self.state.player.confirmations)
    }

    /// How many times the server has written the player position (spawn counts once): `0` means the reference the
    /// drift is measured against has never been refreshed (t63).
    pub fn position_confirmations(&self) -> u64 {
        self.state.player.confirmations
    }

    /// Whether the stored server position is recent enough to be used as evidence at all.
    pub fn server_reference_is_fresh(&self) -> bool {
        reference_is_fresh(self.server_confirmed_ticks())
    }

    /// The drift as one sentence for a failure message, **but only when it may refuse anything**: exactly the gate
    /// [`may_interact`] applies. A drift measured against a reference the server never refreshed is not evidence
    /// (t88: `place` was refused with 4.19 blocks against the spawn point while `position_confirmations` was 0 —
    /// the false signal t69/t72 already fixed for `trade`).
    pub(crate) fn desync_refusal(&self) -> Option<String> {
        let d = self.position_desync()?;
        let confirmations = self.position_confirmations();
        let confirmed_ticks = self.server_confirmed_ticks();
        if may_interact(confirmations, confirmed_ticks, Some(d.blocks)) {
            if d.blocks > DESYNC_LIMIT {
                tracing::debug!(
                    target: "acacia_bot::position",
                    drift = d.blocks,
                    confirmations,
                    ?confirmed_ticks,
                    "position drift ignored: the server reference is not fresh"
                );
            }
            return None;
        }
        Some(format!(
            "position_desync: the client's body is {:.2} blocks from the position the server reports (predicted {:?}, server {:?}, {} ticks)",
            d.blocks, d.predicted, d.server, self.desync_ticks
        ))
    }

    /// Refuses an action whose positions would be computed from a body the server has left behind: the click is
    /// dropped server-side (t27), so sending it only burns the caller's budget. The single gate for every path —
    /// entity interactions (`interact/entity.rs`), `place` (`interact/item_use.rs`) and `dig` (`interact/breaking.rs`).
    pub(crate) fn check_position_synced(&self) -> Result<(), ActionError> {
        match self.desync_refusal() {
            Some(note) => Err(ActionError::NotPossible(format!("{note}; move or wait for the position to sync before clicking"))),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gap_is_per_axis_and_manhattan() {
        let d = desync_of([1.0, 34.0, 2.0], [1.0, 42.0, 2.0]);
        assert_eq!(d.delta, [0.0, -8.0, 0.0]);
        assert!((d.blocks - 8.0).abs() < 1e-6);
        assert!((d.manhattan - 8.0).abs() < 1e-6);
        assert!(d.blocks > DESYNC_LIMIT);
    }

    #[test]
    fn an_unconfirmed_reference_never_blocks_an_interaction() {
        // The frozen-at-spawn case: no confirmation at all, 11 blocks of "drift" — and the click must still go out.
        assert!(may_interact(0, None, Some(11.39)));
        assert!(may_interact(0, Some(500), Some(20.58)));
        // Confirmed recently but really drifting: refused.
        assert!(!may_interact(3, Some(2), Some(11.39)));
        // Confirmed long ago: not evidence either way.
        assert!(may_interact(3, Some(400), Some(11.39)));
        // In sync: fine.
        assert!(may_interact(3, Some(2), Some(1.07)));
        assert!(may_interact(3, Some(2), None));
    }

    #[test]
    fn a_block_click_is_only_refused_by_a_fresh_reference() {
        // t88, the numbers from the live session: `place` refused with 4.19 blocks against the spawn point while
        // `position_confirmations` was 0. Both block paths (`place`, `dig`) now ask this predicate through
        // `desync_refusal`, so these four lines are the whole contract.
        assert!(may_interact(0, None, Some(4.19)), "no confirmation at all: the click goes out");
        assert!(may_interact(0, Some(500), Some(4.19)), "a spawn-only reference is not evidence");
        assert!(may_interact(4, Some(FRESH_REFERENCE_TICKS + 1), Some(4.19)), "a stale reference proves nothing");
        assert!(!may_interact(4, Some(1), Some(4.19)), "fresh confirmation, really 4.19 blocks off: refused");
        assert!(may_interact(4, Some(1), Some(1.07)), "fresh and in sync (the server's steady-state jitter): allowed");
    }

    #[test]
    fn no_confirmation_at_all_is_reported_as_none() {
        // t63: the point of the diagnostic is to say "the server never told us" rather than to print a 0.
        assert_eq!(confirmed_ticks(900, None, 0), None);
        assert_eq!(confirmed_ticks(900, None, 1), Some(900), "only the spawn write: as old as the simulation");
        assert_eq!(confirmed_ticks(900, Some(880), 3), Some(20));
    }

    #[test]
    fn an_old_reference_proves_nothing() {
        // t61: the server rarely re-sends a client-authoritative position, so a difference from a reference that is
        // minutes old cannot be used to refuse an interaction or to call the client stale.
        assert!(!reference_is_fresh(None), "never confirmed");
        assert!(!reference_is_fresh(Some(FRESH_REFERENCE_TICKS + 1)));
        assert!(reference_is_fresh(Some(0)));
        assert!(reference_is_fresh(Some(FRESH_REFERENCE_TICKS)));
    }

    #[test]
    fn a_six_block_gap_is_six_blocks_not_zero() {
        // t44: `status` reported 0.00 while the server was six blocks away, because the two numbers came from
        // different sources. The one function both use must say 6.
        let d = desync_of([150033.0, 29.0, 149855.0], [150033.0, 35.0, 149855.0]);
        assert!((d.blocks - 6.0).abs() < 1e-6, "{d:?}");
        assert!(d.blocks > DESYNC_LIMIT, "and it is over the interaction limit");
    }

    #[test]
    fn a_body_the_server_agrees_with_is_in_sync() {
        let d = desync_of([0.5, 64.0, 0.5], [0.5, 64.0, 0.5]);
        assert!(d.blocks <= DESYNC_LIMIT && d.manhattan == 0.0);
    }
}
