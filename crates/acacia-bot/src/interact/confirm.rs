//! Server-confirmed world actions: a break, place or door click is done only once the server's world
//! (the bot's tracked blocks, which change on `UpdateBlock` alone) shows the expected cell. On timeout the
//! cell is re-read: done, unchanged (retry) or changed otherwise (failed). Design:
//! acacia-pathfind/docs/confirmed-actions.md.

use std::time::Instant;

use acacia_client::proto::packets::{UpdateBlock, UpdateBlockSynced, UpdateSubchunkBlocks};
use acacia_client::proto::types::BlockCoordinates;
use acacia_client::proto::{Packet, RawPacket};
use acacia_physics::BlockPos;
use acacia_world::BlockState;

use super::wire::block_coordinates;
use crate::cancel::{Claim, Held};
use crate::state::ItemStack;
use crate::{ActionError, Bot};

/// What the cell must show for the action to be done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expect {
    /// Broken: air or liquid.
    Gone,
    /// Placed: anything that is neither.
    Solid,
    /// A block property, like a door's `open_bit`.
    Prop(&'static str, &'static str),
    /// Placed into a cell already holding block `.0` (a half slab made double): any other solid block.
    Replaced(u32),
}

impl Expect {
    fn met(self, id: u32, s: &BlockState) -> bool {
        match self {
            Expect::Gone => s.is_air() || s.is_liquid(),
            Expect::Solid => !s.is_air() && !s.is_liquid(),
            Expect::Prop(name, value) => s.property(name) == Some(value),
            Expect::Replaced(old) => id != old && !s.is_air() && !s.is_liquid(),
        }
    }
}

/// Why an action the bot sent did not take effect. The cases need different answers: resend the action, ask
/// for the section again, or report a refused/ineffective action to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The server refused it: its echo put the old block back (a claim, a region guard, too weak a tool).
    ServerRejected,
    /// The server changed the cell (an echo carried a new block) but the local view still shows the old one:
    /// the view is stale, so ask for the section again instead of sending the action once more.
    NoLocalChange,
    /// Nothing changed and no update arrived: the action had no effect at all.
    NoEffect,
    /// The cell now shows something else than the action would leave (someone else built there).
    Other { name: String },
    /// The cell is no longer loaded (its chunk was dropped).
    Unloaded,
}

impl Failure {
    /// The label callers and the agent layer key on: `server_rejected`, `no_local_change`, `no_effect`,
    /// `changed`, `unloaded`.
    #[allow(dead_code)] // read by the tests and by the agent layer's error mapping
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Failure::ServerRejected => "server_rejected",
            Failure::NoLocalChange => "no_local_change",
            Failure::NoEffect => "no_effect",
            Failure::Other { .. } => "changed",
            Failure::Unloaded => "unloaded",
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::ServerRejected => write!(f, "server_rejected: the server put the block back"),
            Failure::NoLocalChange => {
                write!(f, "no_local_change: the server changed the cell but the local view still shows the old block")
            }
            Failure::NoEffect => write!(f, "no_effect: the server sent no update for the cell"),
            Failure::Other { name } => write!(f, "changed: the cell now shows {name}"),
            Failure::Unloaded => write!(f, "unloaded: the cell is no longer loaded"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Wait,
    Done,
    /// Unchanged past the deadline (or the server restored it): send the action again.
    Retry,
    Failed(Failure),
}

/// One action awaiting its echo, tick-driven.
#[derive(Debug, Clone)]
pub(crate) struct Confirm {
    expect: Expect,
    /// Runtime id of the cell when the action went out.
    before: Option<u32>,
    deadline: u64,
    tries: u32,
    max_tries: u32,
    /// The bot wrote the expected block itself (a predicted placement): only the server's echo counts.
    predicted: bool,
    echo_seen: bool,
    /// An echo carried a block other than the one there when the action went out, yet the view kept the old.
    changed_echo: Option<u32>,
    /// An echo carried the very block that was there: the server showing the action had no effect.
    echo_old: bool,
    /// The server's echo said the action took effect while the tracked view still showed the old cell: the
    /// action is done, only the view lags (t100: a placement confirmed 7–8 s late, the read-back still air).
    view_stale: bool,
}

impl Confirm {
    /// `before`: the cell's runtime id as sent; `timeout`: ticks to wait for each try.
    pub(crate) fn new(expect: Expect, before: Option<u32>, now: u64, timeout: u64, max_tries: u32) -> Self {
        Self { expect, before, deadline: now + timeout, tries: 1, max_tries, predicted: false, echo_seen: false, changed_echo: None, echo_old: false, view_stale: false }
    }

    /// The cell shows the bot's own prediction until the server's echo replaces it.
    pub(crate) fn predicted(self) -> Self {
        Self { predicted: true, ..self }
    }

    pub(crate) fn tries(&self) -> u32 {
        self.tries
    }

    /// No echo arrived since the last (re)send: the cell still holds the bot's prediction, if any.
    pub(crate) fn unechoed(&self) -> bool {
        !self.echo_seen
    }

    /// The cell as of tick `now`; `echoed`: the block an `UpdateBlock` for it carried since the last check.
    pub(crate) fn check(&mut self, now: u64, cell: Option<(u32, &BlockState)>, echoed: Option<u32>) -> Verdict {
        self.echo_seen |= echoed.is_some();
        let Some((id, state)) = cell else { return Verdict::Failed(Failure::Unloaded) };
        if let Some(new) = echoed {
            if self.before.is_none_or(|b| b != new) {
                self.changed_echo = Some(new);
            } else {
                self.echo_old = true;
            }
        }
        let ours = self.predicted && !self.echo_seen;
        if self.expect.met(id, state) && !ours {
            return Verdict::Done;
        }
        let unchanged = ours || self.before.is_none_or(|b| b == id);
        if now < self.deadline {
            return Verdict::Wait;
        }
        match (unchanged, self.tries < self.max_tries) {
            (true, true) => Verdict::Retry,
            (true, false) => {
                // The server did change the cell (an echo carried another block) yet the view never caught
                // up: a stale view to resync, not an action to send again. An echo that only repeated the
                // old block is the server refusing; no echo at all means the action had no effect.
                if self.changed_echo.is_some() {
                    Verdict::Failed(Failure::NoLocalChange)
                } else if self.echo_old {
                    Verdict::Failed(Failure::ServerRejected)
                } else {
                    Verdict::Failed(Failure::NoEffect)
                }
            }
            (false, _) => Verdict::Failed(Failure::Other { name: state.name.to_string() }),
        }
    }

    /// What to report when the settle window ends without the expected cell: `Unloaded` for a dropped chunk,
    /// `NoLocalChange` when an echo carried a new block the view never showed, `NoEffect` otherwise.
    pub(crate) fn failure(&self, cell: Option<&str>) -> Failure {
        match cell {
            None => Failure::Unloaded,
            Some(_) if self.changed_echo.is_some() => Failure::NoLocalChange,
            Some(_) if self.echo_old => Failure::ServerRejected,
            Some(_) => Failure::NoEffect,
        }
    }

    /// The cell shows what the action must leave, counting only the server's own word: a predicted
    /// placement shows the bot's block until the echo replaces it.
    pub(crate) fn satisfied(&self, cell: Option<(u32, &BlockState)>, echoed: bool) -> bool {
        let Some((id, state)) = cell else { return false };
        self.expect.met(id, state) && (echoed || !self.predicted)
    }

    /// The action went out again at `now`.
    pub(crate) fn retried(&mut self, now: u64, timeout: u64) {
        self.tries += 1;
        self.deadline = now + timeout;
        self.echo_seen = false;
    }

    /// The server's echo is the server's own word: it carried `id`/`state`, which is what the action leaves,
    /// so the action is done whatever the tracked view shows. `cell` is that view as it stands; when it
    /// disagrees the view is marked stale instead of the action being sent again (t100).
    ///
    /// `id` must be in the same id space as [`Self::before`] and the tracked cell (registry runtime ids); a
    /// raw `UpdateBlock.block_runtime_id` is translated first (see [`Bot::echoed_block`]).
    pub(crate) fn echo_decides(&mut self, id: u32, state: &BlockState, cell: Option<(u32, &BlockState)>) -> bool {
        if !self.expect.met(id, state) {
            return false;
        }
        self.echo_seen = true;
        if !self.satisfied(cell, true) {
            self.view_stale = true;
        }
        true
    }

    /// An echo confirmed the action while the view kept the old cell: the view, not the action, is behind.
    pub(crate) fn view_stale(&self) -> bool {
        self.view_stale
    }
}

/// The stack to put back after a refused placement: ours, unless the server has set the slot since.
pub(crate) fn unpredict(now: &ItemStack, predicted: &ItemStack, before: &ItemStack) -> Option<ItemStack> {
    (now == predicted && predicted != before).then(|| before.clone())
}

/// The slot with a failed placement's item given back, whatever later predictions did to it since: with several
/// placements from one stack in flight, [`unpredict`]'s exact match never held, the count fell behind the
/// server's, and a slot read empty that the server still filled (live: moves into it refused, status 50).
/// `None`: the slot now holds something else; the server's own update decides it.
pub(crate) fn give_back(now: &ItemStack, predicted: &ItemStack, before: &ItemStack) -> Option<ItemStack> {
    let consumed = before.count.checked_sub(if predicted.is_empty() { 0 } else { predicted.count }).filter(|c| *c > 0)?;
    if now.is_empty() {
        return Some(ItemStack { count: consumed, ..before.clone() });
    }
    (now.network_id == before.network_id && now.stack_network_id == before.stack_network_id).then(|| ItemStack { count: now.count + consumed, ..now.clone() })
}

/// A server block update for layer 0 of `at`, in any of the packets the world tracker applies: the block id
/// the update carries, i.e. what the server says the cell is now.
fn update_at(p: &RawPacket, at: &BlockCoordinates) -> Option<u32> {
    match p.id {
        UpdateBlock::ID => p.decode::<UpdateBlock>().ok().filter(|u| u.position == *at && u.layer == 0).map(|u| u.block_runtime_id),
        UpdateBlockSynced::ID => {
            p.decode::<UpdateBlockSynced>().ok().filter(|u| u.position == *at && u.layer == 0).map(|u| u.block_runtime_id)
        }
        UpdateSubchunkBlocks::ID => {
            p.decode::<UpdateSubchunkBlocks>().ok().and_then(|u| u.blocks.iter().find(|b| b.position == *at).map(|b| b.runtime_id))
        }
        _ => None,
    }
}

impl Bot {
    /// Writes block `wire` (the server's id form) into the tracked world at `pos` ahead of the server, as the
    /// vanilla client does when it places: physics then agrees with a server that already has it. Returns the
    /// block it replaced (wire id), for [`Bot::unpredict_block`].
    pub(crate) fn predict_block(&self, pos: BlockPos, wire: u32) -> Option<u32> {
        let view = self.world.as_ref()?.view()?;
        let (id, _) = self.block_at(pos)?;
        let old = view.world().wire_id(id);
        view.set_block(pos[0], pos[1], pos[2], 0, wire);
        Some(old)
    }

    /// Puts back the block a refused or unanswered prediction replaced (no ghost block stays behind).
    pub(crate) fn unpredict_block(&self, pos: BlockPos, old: u32) {
        if let Some(view) = self.world.as_ref().and_then(|w| w.view()) {
            view.set_block(pos[0], pos[1], pos[2], 0, old);
        }
    }

    /// Asks the server for the section holding `pos` again: the view there is stale (an echo carried a block
    /// the tracker never applied).
    pub(crate) fn request_section(&mut self, pos: BlockPos) {
        if let Some(world) = self.world.as_mut() {
            world.request_section(pos[0], pos[1], pos[2]);
        }
    }

    /// An echo (`UpdateBlock`, `UpdateBlockSynced`, `UpdateSubchunkBlocks`) carrying `wire`, translated into
    /// the id space the tracked cell is read in. An echo carries a **wire** id while [`Bot::block_at`] answers
    /// with a **registry** runtime id; comparing the two made every echo look like a change, so
    /// `NoLocalChange` was reported for a cell the server had refused outright and `ServerRejected` was
    /// unreachable (t100).
    fn echoed_block(&self, wire: u32) -> Option<(u32, &BlockState)> {
        let world = self.world()?;
        let id = world.view()?.world().runtime_id(wire);
        world.registry()?.get(id).map(|state| (id, state))
    }

    /// Starts confirming an action on `pos` sent this tick.
    pub(crate) fn confirmation(&self, pos: BlockPos, expect: Expect, max_tries: u32) -> Confirm {
        let before = self.block_at(pos).map(|(id, _)| id);
        Confirm::new(expect, before, self.governor.now(), self.governor.timeout_ticks(), max_tries)
    }

    /// Lets ticks pass until `c` decides. A first try's echo feeds the governor's round-trip estimate.
    pub(crate) async fn await_confirm(&mut self, pos: BlockPos, c: &mut Confirm, sent_at: Instant) -> Result<Verdict, ActionError> {
        let at = block_coordinates(pos);
        let t = Instant::now();
        loop {
            let mut echoed = None;
            let _ = self
                .next_tick(|_, p| match update_at(p, &at) {
                    Some(id) => {
                        echoed = Some(id);
                        true
                    }
                    None => false,
                })
                .await?;
            let echoed_reg = echoed.and_then(|wire| self.echoed_block(wire));
            let mut verdict = c.check(self.governor.now(), self.block_at(pos), echoed_reg.map(|(id, _)| id));
            // The echo itself is the server's word: an `UpdateBlock` carrying the block the action leaves
            // ends the wait at once, even while the tracked view still shows the old cell (t100: the board
            // was really placed 7–8 s late, the read-back was still air).
            if verdict != Verdict::Done
                && let Some((id, state)) = echoed_reg
                && c.echo_decides(id, state, self.block_at(pos))
            {
                tracing::debug!(?pos, view_stale = c.view_stale(), "the server's echo confirmed the action");
                verdict = Verdict::Done;
            }
            if verdict == Verdict::Done && c.tries() == 1 && echoed.is_some() {
                self.governor.sample(sent_at.elapsed());
            }
            if verdict != Verdict::Wait {
                self.act_timing.add(crate::act_timing::Phase::Echo, t.elapsed());
                if verdict == Verdict::Retry {
                    self.act_timing.retry();
                }
                let held = self.state.items.name(self.state.inventory.held().network_id).unwrap_or("-");
                let cell = self.block_at(pos).map_or_else(|| "unloaded".to_string(), |(_, s)| s.name.to_string());
                tracing::debug!(?pos, ?verdict, tries = c.tries(), ?echoed, ms = t.elapsed().as_millis() as u64, held, ?cell, "action confirmation");
                return Ok(verdict);
            }
        }
    }

    /// After the tries ran out, keeps watching the cell for `ticks` (checked every tick, i.e. every 50 ms, with
    /// `also_done` letting a break count its drop or an inventory gain as the same event) before calling the
    /// action refused. The server's update can trail the governor's round-trip estimate by seconds (relay
    /// run115: the block broke, its drop appeared and was picked up while the client had already given up).
    ///
    /// An echo carrying a new block that the view never shows means the view is stale: the section is asked
    /// for again once, and the action is not resent.
    pub(crate) async fn settle_confirm(
        &mut self,
        pos: BlockPos,
        c: &mut Confirm,
        ticks: u64,
        mut also_done: impl FnMut(&Bot) -> bool,
    ) -> Result<Option<Failure>, ActionError> {
        let at = block_coordinates(pos);
        let until = self.governor.now() + ticks;
        let mut asked = false;
        // An echo seen this tick counts as the server's word for a predicted placement (only `check` used to
        // record it; here the echo may be the very event being waited for).
        let mut echoed = false;
        loop {
            if c.satisfied(self.block_at(pos), echoed) || also_done(self) {
                return Ok(None);
            }
            if self.governor.now() >= until {
                let cell = self.block_at(pos).map(|(_, s)| s.name);
                return Ok(Some(c.failure(cell)));
            }
            let mut update = None;
            let _ = self
                .next_tick(|_, p| match update_at(p, &at) {
                    Some(id) => {
                        update = Some(id);
                        true
                    }
                    None => false,
                })
                .await?;
            echoed = update.is_some();
            let echoed_reg = update.and_then(|wire| self.echoed_block(wire));
            if echoed {
                c.echo_seen = true;
            }
            if let Some((id, state)) = echoed_reg {
                // The echo is the server's own word: done the moment it carries what the action leaves,
                // without waiting out the window and without asking for the section again (t100).
                if c.echo_decides(id, state, self.block_at(pos)) {
                    tracing::debug!(?pos, view_stale = c.view_stale(), "the server's echo confirmed the action in the settle window");
                    return Ok(None);
                }
                if c.before.is_none_or(|b| b != id) {
                    c.changed_echo = Some(id);
                    if !asked && !c.satisfied(self.block_at(pos), true) {
                        // The server says the cell changed to something else; the view disagrees. Ask for
                        // the section instead of hammering the action again.
                        self.request_section(pos);
                        asked = true;
                    }
                }
            }
        }
    }

    /// Lets ticks pass until the governor allows another action, then counts it as sent; [`Bot::release`] settles it.
    pub(crate) async fn admit_action(&mut self) -> Result<Claim, ActionError> {
        let t = Instant::now();
        while !self.governor.may_send() {
            self.next_tick(|_, _| false).await?;
        }
        self.act_timing.add(crate::act_timing::Phase::Admit, t.elapsed());
        self.governor.sent(false);
        Ok(self.claim(Held::Slot))
    }

    /// The governor every world action asks before going out.
    pub fn governor(&self) -> &crate::governor::Governor {
        &self.governor
    }

    pub fn governor_mut(&mut self) -> &mut crate::governor::Governor {
        &mut self.governor
    }
}

#[cfg(test)]
#[path = "confirm_tests.rs"]
mod tests;

#[cfg(test)]
mod t104_tests {
    //! t100's two place defects, at the decision level: the echo must be judged in the id space of the
    //! tracked cell, and an echo that carries the block the action leaves must end the wait however late it
    //! arrives and whatever the local view shows.

    use super::*;
    use crate::interact::item_use::SETTLE_TICKS;

    use acacia_world::BlockRegistry;

    struct Ids {
        registry: std::sync::Arc<BlockRegistry>,
        air: u32,
        stone: u32,
        planks: u32,
    }

    fn blocks() -> Ids {
        let registry = BlockRegistry::vanilla_arc();
        let first = |name: &str| registry.states_of(name).next().expect(name).0;
        let (air, stone, planks) = (first("minecraft:air"), first("minecraft:stone"), first("minecraft:oak_planks"));
        Ids { registry, air, stone, planks }
    }

    #[test]
    fn a_slow_echo_seven_and_a_half_seconds_late_is_success() {
        // t100: the clicks went out at 02:24:45–46, the server's UpdateBlock for the target cell only at
        // 02:24:53 — 7–8 s, past the old 6 s window. 1 tick = 50 ms.
        assert!(SETTLE_TICKS * 50 >= 10_000, "the settle window must outlast a 7–8 s echo, not {} ms", SETTLE_TICKS * 50);
        let b = blocks();
        let (air, stone) = (b.registry.get(b.air).expect("air"), b.registry.get(b.stone).expect("stone"));
        let mut c = Confirm::new(Expect::Solid, Some(b.air), 0, SETTLE_TICKS, 1);
        let mut decided = None;
        // The loop `settle_confirm` runs: the view never catches up, the echo is the only word.
        for tick in 0..SETTLE_TICKS {
            let cell = Some((b.air, air));
            let echo = (tick == 150).then_some((b.stone, stone));
            if let Some((id, state)) = echo
                && c.echo_decides(id, state, cell)
            {
                decided = Some(tick);
                break;
            }
            assert_eq!(c.check(tick, cell, echo.map(|(id, _)| id)), Verdict::Wait, "tick {tick}");
        }
        assert_eq!(decided, Some(150), "an echo 7.5 s late is still the placement happening");
        assert!(c.view_stale(), "the view kept the old cell, so it is the view that is behind");
    }

    #[test]
    fn an_echo_of_the_old_block_is_the_server_refusing() {
        // t100 F2: `UpdateBlock` ids are wire ids, the tracked cell a registry id; comparing them made every
        // echo look like a change, so this branch never ran and a refusal was reported as `no_local_change`.
        let b = blocks();
        let air = b.registry.get(b.air).expect("air");
        let mut c = Confirm::new(Expect::Solid, Some(b.air), 0, 10, 1);
        assert_eq!(c.check(4, Some((b.air, air)), Some(b.air)), Verdict::Wait);
        assert_eq!(c.check(11, Some((b.air, air)), Some(b.air)), Verdict::Failed(Failure::ServerRejected));
        assert_eq!(c.failure(Some("minecraft:air")), Failure::ServerRejected);
        assert_eq!(Failure::ServerRejected.kind(), "server_rejected");
    }

    #[test]
    fn an_echo_of_the_new_block_wins_even_when_the_view_lags() {
        let b = blocks();
        let (air, planks) = (b.registry.get(b.air).expect("air"), b.registry.get(b.planks).expect("planks"));
        let mut c = Confirm::new(Expect::Solid, Some(b.air), 0, 20, 3);
        let cell = Some((b.air, air));
        // The server says planks, the tracked cell still says air: `check` alone would wait out the window.
        assert_eq!(c.check(1, cell, Some(b.planks)), Verdict::Wait);
        assert!(c.echo_decides(b.planks, planks, cell), "the server's own word decides");
        assert!(c.view_stale());
        assert!(!c.echo_decides(b.air, air, cell), "the old block again is not a placement");
    }

    #[test]
    fn an_echo_of_another_block_is_not_a_success() {
        // A break whose cell came back as planks (someone built there) is not "gone".
        let b = blocks();
        let (stone, planks) = (b.registry.get(b.stone).expect("stone"), b.registry.get(b.planks).expect("planks"));
        let mut c = Confirm::new(Expect::Gone, Some(b.stone), 0, 20, 3);
        assert!(!c.echo_decides(b.planks, planks, Some((b.stone, stone))));
        assert_eq!(c.check(25, Some((b.planks, planks)), Some(b.planks)), Verdict::Failed(Failure::Other { name: "minecraft:oak_planks".into() }));
    }
}
