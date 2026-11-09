use std::time::Duration;

use acacia_client::proto::packets::{ContainerClose, Interact, InteractActionId};
use acacia_client::proto::types::{ItemStackRequestActionsItem as Action, ItemStackResponsesItem, WindowID, WindowType};
use acacia_world::BlockAccess;

use super::craft::Craft;
use super::plan::Plan;
use super::quick::quick_move_ops;
use super::request::{check_status_with_slots, describe_request, rejection_of, response_for, status_translated};
use super::slot::{open_container, BlockKind, OffhandAddress, Screen};
use super::{Op, SlotRef};
use crate::cancel::{Claim, Held};
use crate::state::{GameState, ItemStack};
use crate::{ActionError, Bot};

/// How long a write (`ItemStackRequest`) waits for the server's `ItemStackResponse`. Generous on
/// purpose: NetEase can take ~3 s on the first request after a window opens, and a timeout reported
/// as a failure makes callers repeat a world write the server already took.
const WRITE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(12);

/// `ItemStackResponse` status: the destination slot of a request did not validate (t127's offhand).
const FAILED_TO_VALIDATE_DST_SLOT: u8 = 50;
/// After a write timed out, how long the tracked slots get to show the result on their own before the
/// request is reported as unanswered: the server's slot updates can arrive without its response.
const READBACK_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for the server to confirm the own inventory opened. BDS may not answer, so
/// running out of time here is not an error.
const OPEN_TIMEOUT: Duration = Duration::from_secs(1);

/// Item transfers through server-authoritative `ItemStackRequest`s.
///
/// Every method waits for the server's `ItemStackResponse` (3 s) and then updates the tracked
/// slots, including items landing in empty slots; a rejection returns [`ActionError::Rejected`]
/// and leaves the tracked state as it was. [`ActionError::Timeout`] means the request was sent but
/// never answered, tracked state unchanged: Geyser often leaves clicks in server-side GUIs (chest
/// shops, menus) unanswered, so callers driving such GUIs treat it as sent and watch the window for
/// the effect. If no container is open, each call opens the player's
/// own inventory screen around the request and closes it afterwards: Geyser drops item requests
/// while no screen is open.
impl Bot {
    /// Moves `count` items from one slot onto an empty slot or a stack of the same item.
    pub async fn move_item(&mut self, from: SlotRef, to: SlotRef, count: u8) -> Result<(), ActionError> {
        self.item_stack_request(&[Op::Transfer { from, to, count }]).await
    }

    /// Exchanges the contents of two slots (which must not hold the same item).
    pub async fn swap_items(&mut self, a: SlotRef, b: SlotRef) -> Result<(), ActionError> {
        self.item_stack_request(&[Op::Swap { a, b }]).await
    }

    /// Throws `count` items from a slot into the world.
    pub async fn drop_item(&mut self, from: SlotRef, count: u8) -> Result<(), ActionError> {
        self.item_stack_request(&[Op::Drop { from, count }]).await
    }

    /// Deletes `count` items from a slot (creative mode only).
    pub async fn destroy_item(&mut self, from: SlotRef, count: u8) -> Result<(), ActionError> {
        self.item_stack_request(&[Op::Destroy { from, count }]).await
    }

    /// Picks the whole stack up into the cursor: a left click, which is how server GUI menus
    /// (chest-based shops through Geyser) see a button press. The server usually answers by
    /// resetting the menu and cursor with `InventorySlot`/`InventoryContent`, often without an
    /// `ItemStackResponse`: then this returns [`ActionError::Timeout`] after 3 s although the click went out.
    pub async fn pick_up(&mut self, from: SlotRef) -> Result<(), ActionError> {
        let count = from.stack(&self.state).map_or(0, |s| u8::try_from(s.count).unwrap_or(u8::MAX));
        self.item_stack_request(&[Op::Transfer { from, to: SlotRef::Cursor, count }]).await
    }

    /// Shift-click: moves the whole stack to the other side (container ↔ player inventory, or
    /// hotbar ↔ inventory). Returns the slots it went to.
    pub async fn quick_move(&mut self, from: SlotRef) -> Result<Vec<SlotRef>, ActionError> {
        let ops = quick_move_ops(&self.state, from)?;
        self.item_stack_request(&ops).await?;
        Ok(ops.iter().filter_map(|op| if let Op::Transfer { to, .. } = op { Some(*to) } else { None }).collect())
    }

    /// Moves a stack of the open container into the player's inventory.
    pub async fn move_to_inventory(&mut self, container_slot: u8) -> Result<Vec<SlotRef>, ActionError> {
        self.quick_move(SlotRef::Container(container_slot)).await
    }

    /// Moves the whole stack in main inventory slot `inventory_slot` (0-35) into `container_slot`.
    ///
    /// Kept for callers that already picked the slot; [`Bot::put_into_container`] is the one that
    /// takes a count and picks the slot itself.
    pub async fn put_in_container(&mut self, inventory_slot: u8, container_slot: u8) -> Result<(), ActionError> {
        self.put_into_container(inventory_slot, 0, Some(container_slot)).await.map(|_| ())
    }

    /// Puts `count` of main inventory slot `inventory_slot` into the open container, choosing the
    /// destination when `dest` is `None`: a stack of the same item with room first, else the first
    /// empty slot, else [`ActionError::NotPossible`] with a `container_full` message (t132 F2: the
    /// old path only ever wrote slot 0).
    ///
    /// `count == 0` takes the whole stack; a short source takes what there is and says so in
    /// [`PutResult::note`]. The reported [`PutResult::moved`] is read back from the container slot
    /// after the request, never the requested number (t132 F1: the tool reported 2 while the wire
    /// carried the whole 61-item stack).
    pub async fn put_into_container(&mut self, inventory_slot: u8, count: u16, dest: Option<u8>) -> Result<PutResult, ActionError> {
        let from = SlotRef::Main(inventory_slot);
        let Some((item, available)) = from.stack(&self.state).map(|s| (s.network_id, s.count)) else {
            return Err(ActionError::NotPossible(format!("no inventory slot {inventory_slot}")));
        };
        if available == 0 {
            return Err(ActionError::NotPossible(format!("inventory slot {inventory_slot} is empty")));
        }
        let (take, note) = put_take(available, count);
        let slot = match dest {
            Some(slot) => {
                let held = open_container(&self.state).and_then(|c| c.slot(usize::from(slot)));
                if let Some(held) = held
                    && !held.is_empty()
                    && held.network_id != item
                {
                    return Err(ActionError::NotPossible(format!(
                        "container slot {slot} holds a different item ({item} cannot merge into {})",
                        held.network_id
                    )));
                }
                slot
            }
            None => {
                let Some(open) = open_container(&self.state) else {
                    return Err(ActionError::NotPossible("no container is open".into()));
                };
                match put_target(&open.slots, item, take) {
                    Some(target) => target.slot(),
                    None => {
                        return Err(ActionError::NotPossible(format!(
                            "container_full: no slot can take {take} more of item {item} (every slot holds a different item, or a full stack)"
                        )));
                    }
                }
            }
        };
        let before = put_count_at(&self.state, slot);
        self.move_item(from, SlotRef::Container(slot), u8::try_from(take).unwrap_or(u8::MAX)).await?;
        let after = put_count_at(&self.state, slot);
        let moved = put_moved(before, after, take);
        tracing::debug!(?slot, take, before, after, moved, ?note, "put into container");
        Ok(PutResult { container_slot: slot, asked: count, moved, note })
    }

    /// Moves every stack of the open container into the player's inventory, stopping when it is
    /// full. Returns how many stacks moved.
    pub async fn take_all(&mut self) -> Result<usize, ActionError> {
        let Some(open) = open_container(&self.state) else {
            return Err(ActionError::NotPossible("no container is open".into()));
        };
        let slots: Vec<u8> = open.iter().filter_map(|(i, _)| u8::try_from(i).ok()).collect();
        let mut moved = 0;
        for slot in slots {
            match self.move_to_inventory(slot).await {
                Ok(_) => moved += 1,
                Err(ActionError::NotPossible(_)) if moved > 0 => break,
                Err(e) => return Err(e),
            }
        }
        Ok(moved)
    }

    /// Sends `ops` as one `ItemStackRequest` and applies the result once the server accepts it.
    pub async fn item_stack_request(&mut self, ops: &[Op]) -> Result<(), ActionError> {
        self.run_plan(|state, screen| Plan::build(state, screen, ops)).await
    }

    /// Sends a crafting request: `craft`'s actions, then `ops`.
    pub(crate) async fn craft_request(&mut self, craft: &Craft, ops: &[Op]) -> Result<(), ActionError> {
        self.run_plan(|state, screen| Plan::craft(state, screen, craft, ops)).await
    }

    /// Sends `head` (see [`Plan::headed`]) followed by `ops`.
    pub(crate) async fn headed_request(&mut self, head: Vec<Action>, ops: &[Op]) -> Result<(), ActionError> {
        self.run_plan(|state, screen| Plan::headed(state, screen, head.clone(), ops)).await
    }

    async fn run_plan(&mut self, build: impl Fn(&GameState, Screen) -> Result<Plan, ActionError>) -> Result<(), ActionError> {
        build(&self.state, self.screen())?;
        let screen = self.open_own_inventory_screen().await?;
        let result = self.send_plan(&build).await;
        if let Some(screen) = screen {
            self.release(screen);
        }
        result
    }

    async fn send_plan(&mut self, build: impl Fn(&GameState, Screen) -> Result<Plan, ActionError>) -> Result<(), ActionError> {
        let mut screen = self.screen();
        // Everything this one call tried, oldest first: a failure has to account for both rounds, not
        // just the last one (the `jump_count`/`last_plan_stats` rule — a field must say whose it is).
        let mut tried: Vec<String> = Vec::new();
        // The ladder answers one specific refusal — the offhand's address/shape — never a general
        // "send it again": each rung costs exactly one refused request.
        let rungs = OffhandAddress::ladder();
        for (attempt, rung) in rungs.into_iter().enumerate() {
            screen = screen.with_offhand(rung);
            // Rebuilt after the screen opened: the server may have resent slots meanwhile.
            let plan = build(&self.state, screen)?;
            // Whether this request names the offhand at all — the only case the retry below is about.
            let names_offhand = plan.predicted(SlotRef::Offhand).is_some();
            let id = self.send_request(&plan);
            let sent = self.request_ids.last_request.join("; ");
            // `None`: the slots already show the result, so there is nothing left to finish.
            let Some(response) = self.await_response(id, &plan).await? else { return Ok(()) };
            match self.finish_request(plan, &response) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let code = self.request_ids.last_rejection.map(|r| r.code);
                    let status = code.map_or_else(|| "no status".to_string(), |c| status_translated(c));
                    tried.push(format!("{} → {status} (sent: {sent})", screen.offhand.label()));
                    let retry = escalates(attempt, rungs.len(), names_offhand, code);
                    if !retry {
                        return Err(enrich_with_attempts(e, &tried));
                    }
                    // `50 FailedToValidateDstSlot` is about the slot we named, not the action: keep the
                    // same ops and name the offhand the other way (window numbering ↔ container slot).
                    tracing::debug!(?e, ?screen.offhand, "the offhand slot was refused; naming it the other way");
                    screen = screen.with_offhand(screen.offhand.alternate());
                }
            }
        }
        Err(ActionError::Rejected(format!(
            "the offhand was refused under every shape we know (tried: {})",
            tried.join("; ")
        )))
    }

    /// Waits for the response to request `id`; `None` means the slots already show the result (the
    /// response was lost on the way and the server's own updates are the read-back).
    async fn await_response(&mut self, id: i32, plan: &Plan) -> Result<Option<ItemStackResponsesItem>, ActionError> {
        match self.wait_until(WRITE_RESPONSE_TIMEOUT, |_, packet| response_for(packet, id)).await {
            Ok(response) => Ok(Some(response)),
            Err(ActionError::Timeout) => {
                // The request may have succeeded without its response reaching us (NetEase answers the
                // first request after a window opens seconds late, and relay paths can drop one). The
                // server's own slot updates are the read-back: if the tracked slots now hold what the
                // plan predicted, report success instead of making the caller repeat the write.
                let readback = self.wait_until(READBACK_TIMEOUT, |bot, _| plan.applied(&bot.state).then_some(())).await;
                if readback.is_ok() {
                    tracing::debug!("item stack response timed out but the slots already show the result");
                    return Ok(None);
                }
                Err(ActionError::Timeout)
            }
            Err(e) => Err(e),
        }
    }

    /// Builds `ops` for the current screen and sends them without waiting; the caller matches the
    /// response with `response_for` and hands it to [`Bot::finish_request`].
    pub(crate) fn send_ops(&mut self, ops: &[Op]) -> Result<(Plan, i32), ActionError> {
        let plan = Plan::build(&self.state, self.screen(), ops)?;
        let id = self.send_request(&plan);
        Ok((plan, id))
    }

    /// Applies an accepted request's prediction and the server's slot updates.
    pub(crate) fn finish_request(&mut self, plan: Plan, response: &ItemStackResponsesItem) -> Result<(), ActionError> {
        tracing::debug!(?response, "item stack response");
        // Kept for callers that answer a refusal with a different request shape (`equip`'s ladder);
        // cleared by the next answered request, so a stale code cannot be read as this one's.
        self.request_ids.last_rejection = rejection_of(response);
        if let Err(e) = check_status_with_slots(response, &self.request_ids.last_request) {
            let main: Vec<String> = self.state.inventory.main.iter().enumerate().filter(|(_, s)| !s.is_empty()).map(|(i, s)| format!("{i}:{}x{}#{:?}", s.network_id, s.count, s.stack_network_id)).collect();
            tracing::debug!(%e, ?main, "item stack request refused; believed main inventory");
            return Err(e);
        }
        plan.commit(&mut self.state, response);
        Ok(())
    }

    fn send_request(&mut self, plan: &Plan) -> i32 {
        let id = self.request_ids.next();
        let request = plan.request(id);
        // Kept for the refusal message: it is the only record of which container/slot the server
        // validated (t131: `50 FailedToValidateDstSlot` names a slot, not an action).
        self.request_ids.last_request = describe_request(&request);
        tracing::debug!(?request, "item stack request");
        self.client.send(&request);
        if let Some(m) = self.movement.as_mut() {
            m.still_ticks = 1;
        }
        id
    }

    fn screen(&self) -> Screen {
        Screen::of(&self.state, self.container_block())
    }

    fn container_block(&self) -> BlockKind {
        let (Some(world), Some(pos)) = (self.world.as_ref(), open_container(&self.state).and_then(|c| c.position.as_ref())) else {
            return BlockKind::Other;
        };
        let (Some(view), Some(registry)) = (world.view(), world.registry()) else { return BlockKind::Other };
        registry.get(view.block(pos.x, pos.y, pos.z)).map_or(BlockKind::Other, |b| BlockKind::from_name(b.name))
    }

    /// Opens the own inventory screen unless some container is open; the claim that closes it if it did.
    pub(crate) async fn open_own_inventory_screen(&mut self) -> Result<Option<Claim>, ActionError> {
        if self.state.containers.open.is_some() {
            return Ok(None);
        }
        self.container_settled().await?;
        let screen = self.claim(Held::InventoryScreen);
        self.send_open_inventory();
        match self.wait_until(OPEN_TIMEOUT, |bot, _| bot.state.containers.open.is_some().then_some(())).await {
            Ok(()) | Err(ActionError::Timeout) => Ok(Some(screen)),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn send_open_inventory(&mut self) {
        self.client.send(&Interact {
            action_id: InteractActionId::OpenInventory,
            target_entity_id: self.state.player.runtime_entity_id,
            has_position: false,
            position: None,
        });
    }

    pub(crate) fn close_own_inventory_screen(&mut self) {
        // Vanilla closes the window id the server opened the screen as, with window type None.
        let window_id = self.state.containers.open.as_ref().map_or(WindowID::Inventory, |c| WindowID::from_raw(i64::from(c.window_id)));
        self.send_close(window_id.to_raw() as i32, ContainerClose { window_id, window_type: WindowType::None, server: false });
        // Cleared now rather than on the server's echo, so the next request reopens the screen.
        self.state.containers.open = None;
    }
}

/// Largest stack vanilla gives a stackable item. The protocol's item registry does not carry stack
/// sizes, so this is the only merge budget there is: a merge past it is refused rather than risked.
const MAX_MERGE_STACK: u16 = 64;

/// What a `put` did, as the container ended up — never the number it was asked for (t132 F1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutResult {
    /// The container slot the items went into.
    pub container_slot: u8,
    /// How many the caller asked for (`0` = the whole stack).
    pub asked: u16,
    /// How many the container actually gained, read back after the request.
    pub moved: u16,
    /// Why less than a whole stack was offered, when that is the case.
    pub note: Option<&'static str>,
}

/// Where a `put` should land in the open container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PutTarget {
    /// A stack of the same item with room left, in this slot.
    Merge(u8),
    /// The first empty slot, in this slot.
    Empty(u8),
}

impl PutTarget {
    pub(crate) fn slot(self) -> u8 {
        match self {
            PutTarget::Merge(slot) | PutTarget::Empty(slot) => slot,
        }
    }
}

/// How many items a request for `asked` may move out of a stack holding `available`, and why it is
/// fewer when it is. `asked == 0` means "the whole stack".
pub(crate) fn put_take(available: u16, asked: u16) -> (u16, Option<&'static str>) {
    if asked == 0 {
        return (available, None);
    }
    if asked > available {
        return (available, Some("the stack holds fewer than asked; only what there was was sent"));
    }
    (asked, None)
}

/// The destination for `take` of `item`: a stack of the same item that still has room (so a second
/// put of the same block merges instead of filling another slot), else the first empty slot, else
/// `None` — the container is full for this item.
///
/// Merging past [`MAX_MERGE_STACK`] is refused: without per-item stack sizes a tool (stack size 1)
/// would otherwise look mergeable. A stack that is visibly stackable (`count > 1`) or a request for
/// more than one item is enough evidence to merge.
pub(crate) fn put_target(slots: &[ItemStack], item: i32, take: u16) -> Option<PutTarget> {
    let mergeable = slots.iter().enumerate().find_map(|(index, stack)| {
        let slot = u8::try_from(index).ok()?;
        let stackable = stack.count > 1 || take > 1;
        (stack.network_id == item && stackable && stack.count.saturating_add(take) <= MAX_MERGE_STACK)
            .then_some(PutTarget::Merge(slot))
    });
    mergeable.or_else(|| {
        slots.iter().enumerate().find_map(|(index, stack)| {
            let slot = u8::try_from(index).ok()?;
            stack.is_empty().then_some(PutTarget::Empty(slot))
        })
    })
}

/// How many items the container slot actually gained: what it holds now minus what it held before,
/// capped by what the request offered (a slot that was already counting something else cannot make
/// the move look bigger than it was).
pub(crate) fn put_moved(before: u16, after: u16, take: u16) -> u16 {
    after.saturating_sub(before).min(take)
}

/// The count in container slot `slot` of the open container, `0` when the slot is not tracked yet.
fn put_count_at(state: &GameState, slot: u8) -> u16 {
    open_container(state).and_then(|c| c.slot(usize::from(slot))).map_or(0, |s| s.count)
}

/// Keeps the attempt list a request is entitled to report when it fails: every numbering this call
/// sent, in order, so the caller does not have to guess which round the status belongs to.
fn enrich_with_attempts(err: ActionError, tried: &[String]) -> ActionError {
    match err {
        ActionError::Rejected(message) => {
            ActionError::Rejected(format!("{message} (tried: {})", tried.join("; ")))
        }
        other => other,
    }
}

#[cfg(test)]
mod t131_tests {
    use super::*;

    /// t131: a request that was sent twice must report both numberings, oldest first — a caller
    /// reading only the last status would think the second one was the only thing tried.
    #[test]
    fn a_failure_names_every_numbering_it_tried() {
        let tried = vec![
            "offhand/0 (the offhand container's own slot) → 50 FailedToValidateDstSlot (sent: Swap Offhand/0#11 <- Hotbar/3#-1)"
                .to_string(),
            "offhand/1 (window numbering) → 50 FailedToValidateDstSlot (sent: Swap Offhand/1#11 <- Hotbar/3#-1)"
                .to_string(),
        ];
        let err = enrich_with_attempts(ActionError::Rejected("the server said no".into()), &tried);
        let ActionError::Rejected(message) = err else { panic!("expected a rejection, got {err:?}") };
        assert!(message.contains("offhand/0"), "{message}");
        assert!(message.contains("offhand/1"), "{message}");
        assert!(message.contains("Swap Offhand/0#11"), "the sent slots stay readable: {message}");
        let first = message.find("offhand/0").expect("first round");
        let second = message.find("offhand/1").expect("second round");
        assert!(first < second, "oldest first: {message}");
    }

    /// The same helper must not touch the other variants: a timeout has no attempts to add.
    #[test]
    fn a_timeout_keeps_its_own_shape() {
        let err = enrich_with_attempts(ActionError::Timeout, &["offhand/0 → 50".to_string()]);
        assert!(matches!(err, ActionError::Timeout));
    }
}
#[cfg(test)]
mod t136_tests {
    use super::*;

    fn stack(id: i32, count: u16) -> ItemStack {
        ItemStack {
            network_id: id,
            count,
            metadata: 0,
            block_runtime_id: 0,
            stack_network_id: None,
            custom_name: None,
            lore: Vec::new(),
            nbt: None,
        }
    }

    /// t132 F1: a put of 2 out of 61 must offer 2 — the wire used to carry the whole stack.
    #[test]
    fn a_put_only_offers_what_was_asked_for() {
        assert_eq!(put_take(61, 2), (2, None));
        assert_eq!(put_take(61, 0), (61, None), "0 means the whole stack");
        assert_eq!(put_take(61, 61), (61, None));
        let (take, note) = put_take(3, 9);
        assert_eq!(take, 3, "a short source gives what it has");
        assert!(note.is_some(), "and says so instead of silently sending less");
    }

    #[test]
    fn the_same_item_merges_before_an_empty_slot_is_used() {
        let slots = [stack(4, 61), stack(0, 0)];
        assert_eq!(put_target(&slots, 4, 2), Some(PutTarget::Merge(0)));
    }

    #[test]
    fn a_full_stack_is_not_merged_past_what_vanilla_allows() {
        let slots = [stack(4, 64), stack(0, 0), stack(0, 0)];
        assert_eq!(put_target(&slots, 4, 2), Some(PutTarget::Empty(1)));
    }

    /// t132 F2: slot 0 held cobblestone, so dirt has to go to the first empty slot instead of the
    /// whole call being refused for "the destination holds a different item".
    #[test]
    fn a_slot_holding_something_else_sends_the_put_to_the_next_one() {
        let slots = [stack(4, 61), stack(0, 0), stack(0, 0)];
        assert_eq!(put_target(&slots, 9, 2), Some(PutTarget::Empty(1)));
    }

    #[test]
    fn a_container_with_no_room_says_it_is_full() {
        let slots = [stack(4, 64), stack(9, 12)];
        assert_eq!(put_target(&slots, 4, 2), None, "a full stack and a stranger");
        assert_eq!(put_target(&slots, 9, 64), None, "a stranger with a full stack of its own");
    }

    /// Stack size 1 for tools: merging one into another would be refused by the server, so the
    /// chooser must not offer it.
    #[test]
    fn a_single_tool_does_not_merge_into_another_tool() {
        let slots = [stack(300, 1), stack(0, 0)];
        assert_eq!(put_target(&slots, 300, 1), Some(PutTarget::Empty(1)));
    }

    /// t132 F1 in the other direction: the reported count is the container's readback, never the
    /// number that was asked for.
    #[test]
    fn the_moved_count_is_read_back_and_never_the_request() {
        assert_eq!(put_moved(0, 61, 61), 61, "the wire carried 61, so 61 is what happened");
        assert_eq!(put_moved(61, 63, 2), 2);
        assert_eq!(put_moved(0, 0, 2), 0, "the server moved nothing");
        assert_eq!(put_moved(10, 12, 2), 2, "a slot that already held items is a delta, not a total");
        assert_eq!(put_moved(5, 5, 2), 0);
    }
}

/// Whether a refusal earns the next shape: only the offhand, only `50 FailedToValidateDstSlot`, and
/// only while a shape is left. Any other status is about something this ladder is not varying, and a
/// third guess is worth less than the evidence (t138).
fn escalates(attempt: usize, rungs: usize, names_offhand: bool, code: Option<u8>) -> bool {
    names_offhand && code == Some(FAILED_TO_VALIDATE_DST_SLOT) && attempt + 1 < rungs
}

#[cfg(test)]
mod t138_tests {
    use super::*;

    #[test]
    fn only_a_refused_offhand_earns_the_next_shape() {
        // 50 + offhand + a shape left ⇒ escalate; that is the whole ladder, no other path.
        assert!(escalates(0, 2, true, Some(50)));
        // Anything else stops: another status, no offhand in the request, or no shape left.
        assert!(!escalates(0, 2, true, Some(54)), "54 is about the action, not the slot");
        assert!(!escalates(0, 2, true, None), "no status is not a refusal of the slot");
        assert!(!escalates(0, 2, false, Some(50)), "nothing to re-address without the offhand");
        assert!(!escalates(1, 2, true, Some(50)), "the last shape has no next one");
    }
}
