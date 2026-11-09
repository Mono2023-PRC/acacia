use std::time::Instant;

use acacia_client::proto::types::Action;
use acacia_physics::{BlockPos, Vec3};

use super::confirm::{unpredict, Confirm, Expect, Verdict};
use super::geometry::{block_distance, block_min, relative, BLOCK_REACH};
use super::legacy::{self, SlotChange};
use super::wire::{self, SwingSource};
use super::{is_interactive, Face};
use crate::human::STOP_USE_ON;
use crate::reflex::Reflex;
use crate::state::ItemStack;
use crate::{ActionError, Bot};

/// Clicks of a placement the server shows no block for.
const MAX_TRIES: u32 = 3;
/// How long a placement the server shows no block for keeps being watched after its tries ran out, in ticks
/// (12 s): NetEase takes seconds to echo the cell — t100 measured **7–8 s** on a rental server (three clicks
/// at 02:24:45–46, the `UpdateBlock` at 02:24:53), which the old 6 s window reported as `no_effect` for a
/// board that had really been placed and really left the hand.
pub(crate) const SETTLE_TICKS: u64 = 240;

impl Bot {
    /// Swings the main hand (`Animate`), as a click on nothing does.
    pub fn swing(&mut self) {
        self.swing_from(None);
    }

    pub(crate) fn swing_from(&mut self, source: Option<SwingSource>) {
        self.client.send(&wire::swing(self.runtime_id(), source));
    }

    /// Uses the held item in the air (`UseItem` ClickAir): eat, drink, throw, draw a bow, raise a
    /// shield. Food finishes on its own; bows and crossbows need [`Bot::release_item`]. Geyser
    /// aims throws with the rotation of the last `PlayerAuthInput`, so physics bots should set
    /// [`crate::movement::Controls`] a tick before.
    pub fn use_item(&mut self) {
        self.client.send(&wire::click_air(self.hand()));
    }

    /// Releases the item in use (`ReleaseItem` Release): shoots a drawn bow.
    pub fn release_item(&mut self) {
        self.client.send(&wire::release(self.hand()));
    }

    /// Right-clicks the block at `pos` with the held item (`UseItem` ClickBlock): opens doors, presses
    /// buttons, uses containers. Clicks a point the eye sees on its shape, on `face` when that face shows,
    /// else on another; physics bots face it first. `NotVisible` when the block is hidden.
    pub async fn use_item_on_block(&mut self, pos: BlockPos, face: Face) -> Result<(), ActionError> {
        let distance = block_distance(self.eye_position(), pos);
        if distance > BLOCK_REACH {
            return Err(ActionError::NotPossible(format!("block {pos:?} is {distance:.2} blocks away")));
        }
        let (face, aim) = self.visible_aim(pos, Some(face))?;
        self.look_at(aim).await?;
        self.click_block(pos, face, relative(pos, aim), SwingSource::Interact)
    }

    /// Clicks the door, gate or trapdoor at `pos` and waits for the server to show it `open` (or closed). One
    /// click only: a second would toggle it back were the first just late.
    pub(crate) async fn door_confirmed(&mut self, pos: BlockPos, open: bool) -> Result<(), ActionError> {
        let distance = block_distance(self.eye_position(), pos);
        if distance > BLOCK_REACH {
            return Err(ActionError::NotPossible(format!("door {pos:?} is {distance:.2} blocks away")));
        }
        let (face, aim) = self.visible_aim(pos, None)?;
        self.look_at(aim).await?;
        let slot = self.admit_action().await?;
        let mut c = self.confirmation(pos, Expect::Prop("open_bit", if open { "1" } else { "0" }), 1);
        let clicked = self.click_block(pos, face, relative(pos, aim), SwingSource::Interact);
        let verdict = match clicked {
            Ok(()) => self.await_confirm(pos, &mut c, Instant::now()).await,
            Err(e) => Err(e),
        };
        self.release(slot);
        match verdict? {
            Verdict::Done => Ok(()),
            _ => Err(ActionError::Rejected(format!("the door at {pos:?} did not move"))),
        }
    }

    /// Right-clicks the block at `pos` (on `face` when it shows) and waits for the server to show the cell
    /// changed: a use toggle (`open_bit`, a repeater's delay) or a held tool's work (tilled dirt). One click
    /// only, as [`Self::door_confirmed`].
    pub(crate) async fn use_confirmed(&mut self, pos: BlockPos, face: Face) -> Result<(), ActionError> {
        let distance = block_distance(self.eye_position(), pos);
        if distance > BLOCK_REACH {
            return Err(ActionError::NotPossible(format!("block {pos:?} is {distance:.2} blocks away")));
        }
        let before = self.block_at(pos).map(|(id, _)| id).ok_or_else(|| ActionError::NotPossible(format!("{pos:?} is not loaded")))?;
        let (face, aim) = self.visible_aim(pos, Some(face))?;
        self.look_at(aim).await?;
        let slot = self.admit_action().await?;
        let mut c = self.confirmation(pos, Expect::Replaced(before), 1);
        let verdict = match self.click_block(pos, face, relative(pos, aim), SwingSource::Interact) {
            Ok(()) => self.await_confirm(pos, &mut c, Instant::now()).await,
            Err(e) => Err(e),
        };
        self.release(slot);
        match verdict? {
            Verdict::Done => Ok(()),
            _ => Err(ActionError::Rejected(format!("using {pos:?} changed nothing"))),
        }
    }

    /// Places the held block or item (sign, bed, boat...) against `face` of the block at `against`.
    /// Waits until the server shows a block in the target cell, clicking again (up to 3 tries) while it
    /// stays empty past the governor's timeout; `Rejected` after that. Items that make entities, like
    /// boats, are not waited for.
    pub async fn place_block(&mut self, against: BlockPos, face: Face) -> Result<(), ActionError> {
        self.place_block_at(against, face, face.click_offset()).await
    }

    /// [`Bot::place_block`] clicking at `click` (relative to `against`'s minimum corner), which picks a slab's
    /// half or a stair's upside-down bit. A half slab in the target cell may be clicked into to make it double.
    pub async fn place_block_at(&mut self, against: BlockPos, face: Face, click: Vec3) -> Result<(), ActionError> {
        let (target, half_slab, against, face, click) = self.aim_placement(against, face, click).await?;
        let makes_entity = self.held_name().is_some_and(|n| n.ends_with("boat") || n.ends_with("minecart"));
        if makes_entity || self.block_at(target).is_none() {
            return self.click_block(against, face, click, SwingSource::Build);
        }
        let slot = self.admit_action().await?;
        let result = self.place_confirmed(against, face, click, half_slab).await;
        self.release(slot);
        result.map_err(|e| match e {
            ActionError::Rejected(why) => ActionError::Rejected(format!("placing at {target:?}: {why}")),
            e => e,
        })
    }

    /// Checks a placement against `face` of `against` and turns to the click point; returns the target cell,
    /// the half slab already in it (if any), and the block/face/click the bot actually aims at: when the
    /// requested face is hidden from the eye, another neighbour of the target cell is clicked (see
    /// [`Self::place_against`]) and the block still lands in the target cell.
    pub(crate) async fn aim_placement(
        &mut self,
        against: BlockPos,
        face: Face,
        click: Vec3,
    ) -> Result<(BlockPos, Option<u32>, BlockPos, Face, Vec3), ActionError> {
        if self.state.inventory.held().is_empty() {
            return Err(ActionError::NotPossible("nothing held to place".into()));
        }
        // A placement is aimed from the client's own body: a drifted body makes the server drop it (t37), which
        // is the real cause behind a `refused` the caller would otherwise read as "occupied".
        if let Some(note) = self.desync_refusal()
            && let Some((_, state)) = self.block_at(face.adjacent(against))
            && !state.is_air()
        {
            return Err(ActionError::NotPossible(format!("{note}; also {:?} is occupied by {}", face.adjacent(against), state.name)));
        }
        let target = face.adjacent(against);
        let mut half_slab = None;
        if let Some((id, s)) = self.block_at(target)
            && !s.is_air()
            && !s.is_liquid()
        {
            if !s.name.ends_with("_slab") && !adds_to(s.name, self.held_name()) {
                // Say *why* it cannot be filled: the bot's own body is a placement the caller can fix by
                // stepping aside, a block with nothing under it is not.
                let body = if self.touches_body(target) { "; that is the bot's own cell, step aside first" } else { "" };
                let support = if self.block_at([target[0], target[1] - 1, target[2]]).is_some_and(|(_, s)| s.is_air()) {
                    "; nothing under it either"
                } else {
                    ""
                };
                return Err(ActionError::NotPossible(format!("{target:?} is occupied by {}{body}{support}", s.name)));
            }
            half_slab = Some(id);
        }
        if self.block_at(against).is_some_and(|(_, s)| s.is_air()) {
            return Err(ActionError::NotPossible(format!("nothing to place against at {against:?}")));
        }
        let sneaking = self.movement.as_ref().is_some_and(|m| m.controls.sneak);
        if !sneaking && self.block_at(against).is_some_and(|(_, s)| is_interactive(s)) {
            return Err(ActionError::NotPossible(format!("clicking {against:?} would use it, not place against it")));
        }
        let distance = block_distance(self.eye_position(), against);
        if distance > BLOCK_REACH {
            return Err(ActionError::NotPossible(format!("block {against:?} is {distance:.2} blocks away")));
        }
        let into = half_slab.map(|_| target);
        let mut aim = (against, face, click);
        if !self.placement_visible(aim.0, aim.1, aim.2, into) {
            // A face below the eye's line (a block beside the feet) is met by its top first: the requested
            // side is hidden although the cell can be filled perfectly well from another neighbour.
            let Some(alt) = self.place_against(target, against, into) else {
                let body = if self.touches_body(target) { "target inside the bot's body" } else { "target clear of the bot" };
                let support = if self.block_at([target[0], target[1] - 1, target[2]]).is_some_and(|(_, s)| s.is_air()) {
                    "nothing under the target"
                } else {
                    "target supported"
                };
                return Err(ActionError::NotVisible(format!(
                    "{face:?} of {against:?} is hidden from the eye ({body}, {support})"
                )));
            };
            tracing::debug!(?target, requested = ?aim, chosen = ?alt, "placement re-aimed at another face");
            aim = (alt.0, alt.1, alt.1.click_offset());
        }
        let (against, face, click) = aim;
        let distance = block_distance(self.eye_position(), against);
        if distance > BLOCK_REACH {
            return Err(ActionError::NotPossible(format!("block {against:?} is {distance:.2} blocks away")));
        }
        let m = block_min(against);
        let point = [m[0] + click[0], m[1] + click[1], m[2] + click[2]];
        let t = Instant::now();
        self.look_at(point).await?;
        self.act_timing.add(crate::act_timing::Phase::Aim, t.elapsed());
        Ok((target, half_slab, against, face, click))
    }

    /// The eye sees `face` of `against` at `click` (block-relative), for a placement into `into`.
    fn placement_visible(&self, against: BlockPos, face: Face, click: Vec3, into: Option<BlockPos>) -> bool {
        let m = block_min(against);
        let point = [m[0] + click[0], m[1] + click[1], m[2] + click[2]];
        self.sight().is_none_or(|s| s.sees_face_into(self.eye_position(), against, face, point, into))
    }

    /// A neighbour of `target` whose face towards it the eye sees: the fallback click when the requested
    /// face is hidden. The ground below first (a floor fill), then the requested block, then the sides, the
    /// ceiling last; interactable blocks (a click would use them) and blocks out of reach are skipped.
    /// The block lands in `target` either way, but its orientation follows the new face (a torch goes on a
    /// wall rather than the floor), so this is a last resort, not the first choice.
    fn place_against(&self, target: BlockPos, requested: BlockPos, into: Option<BlockPos>) -> Option<(BlockPos, Face)> {
        let [x, y, z] = target;
        let order = [[x, y - 1, z], requested, [x - 1, y, z], [x + 1, y, z], [x, y, z - 1], [x, y, z + 1], [x, y + 1, z]];
        let eye = self.eye_position();
        for n in order {
            if n == target {
                continue;
            }
            let Some((_, s)) = self.block_at(n) else { continue };
            if s.is_air() || s.is_liquid() || is_interactive(s) {
                continue;
            }
            if block_distance(eye, n) > BLOCK_REACH {
                continue;
            }
            let Some(face) = face_towards(n, target) else { continue };
            if self.placement_visible(n, face, face.click_offset(), into) {
                return Some((n, face));
            }
        }
        None
    }

    /// Clicks, then waits for the server's echo; clicks again while the cell stays empty. A refused click's
    /// predicted stack change is undone (see [`unpredict`]).
    async fn place_confirmed(&mut self, against: BlockPos, face: Face, click: Vec3, half_slab: Option<u32>) -> Result<(), ActionError> {
        let target = face.adjacent(against);
        let expect = half_slab.map_or(Expect::Solid, Expect::Replaced);
        let mut confirm: Option<Confirm> = None;
        loop {
            let (slot, before) = (self.state.inventory.selected_hotbar_slot, self.state.inventory.held().clone());
            if before.is_empty() {
                return Err(ActionError::NotPossible("nothing held to place".into()));
            }
            let c = match &mut confirm {
                Some(c) => {
                    c.retried(self.governor.now(), self.governor.timeout_ticks());
                    self.governor.sent(true);
                    c
                }
                None if half_slab.is_some() => confirm.insert(self.confirmation(target, expect, MAX_TRIES)),
                None => confirm.insert(self.confirmation(target, expect, MAX_TRIES).predicted()),
            };
            self.click_block(against, face, click, SwingSource::Build)?;
            let predict = before.block_runtime_id != 0 && half_slab.is_none();
            let replaced = predict.then(|| self.predict_block(target, before.block_runtime_id)).flatten();
            let predicted = self.state.inventory.held().clone();
            let verdict = self.await_confirm(target, c, Instant::now()).await?;
            if verdict == Verdict::Done {
                return Ok(());
            }
            // A refused placement is only undone after the settle window: the server's echo can trail the
            // governor's estimate by seconds, and undoing a placement the server took leaves a ghost item in
            // hand.
            let mut failure = if let Verdict::Failed(f) = verdict.clone() { Some(f) } else { None };
            if failure.is_some() {
                match self.settle_confirm(target, c, SETTLE_TICKS, |_| false).await? {
                    None => return Ok(()),
                    Some(f) => failure = Some(f),
                }
            }
            if let Some(old) = replaced.filter(|_| c.unechoed()) {
                self.unpredict_block(target, old);
            }
            let main = &mut self.state.inventory.main[usize::from(slot)];
            if let Some(back) = unpredict(main, &predicted, &before) {
                tracing::debug!(?target, "placement refused; undoing the predicted stack change");
                *main = back;
            }
            match verdict {
                Verdict::Done => unreachable!("returned above"),
                Verdict::Retry => {}
                Verdict::Failed(_) => {
                    let f = failure.expect("set with the verdict");
                    let cell = self.block_at(target).map_or_else(|| "unloaded".to_string(), |(_, s)| s.name.to_string());
                    return Err(ActionError::Rejected(format!("{f}; last seen {cell}")));
                }
                Verdict::Wait => unreachable!("await_confirm decides"),
            }
        }
    }

    pub(super) fn held_name(&self) -> Option<&str> {
        self.state.items.name(self.state.inventory.held().network_id)
    }

    /// Vanilla's block click: StartItemUseOn, the swing, the ClickBlock (placing: with the held
    /// stack's predicted change, then the new stack in MobEquipment), StopItemUseOn a moment later.
    pub(super) fn click_block(&mut self, pos: BlockPos, face: Face, click: Vec3, source: SwingSource) -> Result<(), ActionError> {
        let block = self.block_wire_id(pos)?;
        tracing::debug!(?pos, ?face, block, "click block");
        let held = self.state.inventory.held().clone();
        let is_sign = self.held_name().is_some_and(|n| n.ends_with("sign"));
        let result = use_on_result(&held, is_sign, pos, face);
        if self.reflexes.hovered.take().is_some() {
            self.client.send(&wire::mouse_over_nothing());
        }
        self.client.send(&wire::item_use_on(self.runtime_id(), Action::StartItemUseOn, pos, result, face as i32));
        self.swing_from(Some(source));
        let packet = wire::click_block(self.hand(), pos, face, click, block);
        if source != SwingSource::Build {
            self.client.send(&packet);
        } else {
            let slot = self.state.inventory.selected_hotbar_slot;
            let left = one_used(&held);
            // Capture: sign 2 -> 1 had a legacy id, bed and boat 1 -> 0 had none.
            let legacy_id = (!left.is_empty()).then(|| self.reflexes.next_legacy_id());
            self.client.send(&legacy::with_slot_change(packet, legacy_id, &SlotChange { slot, old: &held, new: &left }));
            self.state.inventory.main[usize::from(slot)] = left;
            if is_sign {
                self.client.send(&wire::click_air(self.hand()));
            }
            self.equip_now();
        }
        self.later(STOP_USE_ON, Reflex::StopUseOn(result));
        Ok(())
    }
}

/// Vanilla's `result_position`: the cell a placement would fill when the hand is empty or holds a
/// block or sign, else the clicked block (beds, boats, tools, other items).
pub(crate) fn use_on_result(held: &ItemStack, is_sign: bool, pos: BlockPos, face: Face) -> BlockPos {
    if held.is_empty() || held.block_runtime_id != 0 || is_sign {
        face.adjacent(pos)
    } else {
        pos
    }
}

/// Placing `held` into a cell holding `cell` adds one to it (candles, sea pickles, turtle eggs).
pub(crate) fn adds_to(cell: &str, held: Option<&str>) -> bool {
    held == Some(cell) && (cell.ends_with("candle") || matches!(cell, "minecraft:sea_pickle" | "minecraft:turtle_egg"))
}

/// The stack left after placing one.
pub(crate) fn one_used(held: &ItemStack) -> ItemStack {
    match held.count {
        0 | 1 => ItemStack::default(),
        n => ItemStack { count: n - 1, ..held.clone() },
    }
}

/// The face of `from` that `to` touches: `to` must be the cell adjacent to `from` through it.
fn face_towards(from: BlockPos, to: BlockPos) -> Option<Face> {
    let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    Face::ALL.into_iter().find(|face| face.offset() == d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_face_points_at_the_cell_it_touches() {
        assert_eq!(face_towards([0, 0, 0], [0, 1, 0]), Some(Face::Up));
        assert_eq!(face_towards([0, 0, 0], [1, 0, 0]), Some(Face::East));
        assert_eq!(face_towards([0, 0, 0], [0, 0, -1]), Some(Face::North));
        assert_eq!(face_towards([0, 0, 0], [1, 1, 0]), None, "not adjacent");
        assert_eq!(face_towards([0, 0, 0], [0, 0, 0]), None);
    }
}
