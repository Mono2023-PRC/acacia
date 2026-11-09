use std::time::Duration;

use crate::human::{CLICK, HOTBAR_KEY, HOTBAR_SWITCH};
use crate::items::{Click, SlotRef};
use crate::state::{Inventory, ItemStack};
use crate::{ActionError, Bot};

/// How long to wait for the server to move a used armour piece into its slot.
const WEAR_TIMEOUT: Duration = Duration::from_secs(3);

/// How [`Bot::equip_by`] puts on armour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EquipMethod {
    /// Through the inventory screen: `Place` into the armour slot, or `Swap` with what is worn.
    #[default]
    Inventory,
    /// Holding the piece and using it (`UseItem` ClickAir), as a right-click in the world does; the
    /// server swaps it with what is worn. A single piece's swap is predicted, not awaited.
    Use,
}

/// Where [`Bot::equip`] puts an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// Into the hotbar and selected.
    Hand,
    Head,
    Chest,
    Legs,
    Feet,
    OffHand,
}

impl Destination {
    fn armor_slot(self) -> Option<u8> {
        match self {
            Destination::Head => Some(0),
            Destination::Chest => Some(1),
            Destination::Legs => Some(2),
            Destination::Feet => Some(3),
            Destination::Hand | Destination::OffHand => None,
        }
    }
}

/// The armour slot (0 helmet .. 3 boots) an item identifier can be worn in.
pub(crate) fn armor_slot(name: &str) -> Option<u8> {
    let n = name.strip_prefix("minecraft:").unwrap_or(name);
    if n.ends_with("_helmet") || n == "carved_pumpkin" || n.ends_with("_head") || n.ends_with("_skull") {
        Some(0)
    } else if n.ends_with("_chestplate") || n == "elytra" {
        Some(1)
    } else if n.ends_with("_leggings") {
        Some(2)
    } else if n.ends_with("_boots") {
        Some(3)
    } else {
        None
    }
}

/// The `ItemStackRequest` action shapes `equip` tries, in order.
///
/// t123 sent the only shape the bot had (`PlaceStackRequestAction`, slot → slot) and the server
/// answered `55 CannotPlaceItem` with zero containers changed in 4101 ms: the request arrived and
/// was refused on its semantics, not on the item or the slot. So the first shape here is the one the
/// protocol uses for an offhand/armour slot exchange, and the second is what a player's two clicks
/// on the inventory screen send.
///
/// **Window or no window is not a dimension on this server (t142).** Every `ItemStackRequest` the
/// bot sends already goes out with the player's own inventory window open: `items/actions.rs:179`
/// (`run_plan`) calls `:301` (`open_own_inventory_screen`) first and waits for `containers.open`
/// before the plan is sent. So the two "resend it inside the window" variants t140 added could never
/// have produced a different answer — and they did not: t141 measured them and the server still
/// answered `50 FailedToValidateDstSlot`. That rung is gone rather than kept as dead code; what the
/// account of `equip → offhand` still rests on is 2 shapes × 2 numberings, every one of them sent
/// with the window open, with `MobEquipment` never arriving and the offhand staying `null` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EquipAction {
    /// `SwapStackRequestAction` from the source slot to the destination (the offhand key's exchange;
    /// with an empty destination the same action moves the stack there).
    Swap,
    /// Two screen clicks in two requests: the stack onto the cursor (`Take`), then down into the
    /// destination (`Place` from the cursor) — the shape a player produces by hand.
    Clicks,
}

impl EquipAction {
    pub fn name(self) -> &'static str {
        match self {
            EquipAction::Swap => "swap",
            EquipAction::Clicks => "clicks",
        }
    }

}

/// The response status that means "the server could not carry out the move as asked".
pub(crate) const CANNOT_PLACE_ITEM: u8 = 55;

/// What one `equip` attempt did, for a run's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EquipAttempt {
    pub action: &'static str,
    /// The refusal as the protocol table names it (`55 CannotPlaceItem`), `None` when not refused.
    pub status: Option<String>,
    pub message: String,
}

/// How `equip` moved (or failed to move) an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EquipReport {
    /// The shape that worked, or — when every shape failed — the last one tried.
    pub action_used: &'static str,
    /// The refusal of the last attempt, translated through the protocol table.
    pub status_translated: Option<String>,
    pub attempts: Vec<EquipAttempt>,
}

/// The shapes to try for a move of `count` items out of a stack of `stack_count`.
///
/// `Clicks` picks the whole stack up, so it is only a candidate when the whole stack is what moves;
/// a partial move keeps the single `Swap` shape (which carries a count).
pub(crate) fn equip_steps(count: u8, stack_count: u16) -> Vec<EquipAction> {
    let whole = u16::from(count) >= stack_count;
    [EquipAction::Swap, EquipAction::Clicks]
        .into_iter()
        .filter(|step| whole || !matches!(step, EquipAction::Clicks))
        .collect()
}

/// The shape to try after `tried`, given the status of the last refusal.
///
/// Only `CannotPlaceItem` is worth another shape: an `Error`(1) or a timeout says nothing about the
/// action type, and re-sending the same move under a new name would only repeat the refusal.
pub(crate) fn next_equip_action(steps: &[EquipAction], tried: &[EquipAction], refusal: Option<u8>) -> Option<EquipAction> {
    if refusal != Some(CANNOT_PLACE_ITEM) {
        return None;
    }
    steps.iter().copied().find(|step| !tried.contains(step))
}

/// The hotbar slot an item from the inventory goes to: the first empty one, else the selected one.
pub(crate) fn hotbar_target(inv: &Inventory) -> u8 {
    (0..Inventory::HOTBAR_SLOTS).find(|&i| inv.main[usize::from(i)].is_empty()).unwrap_or(inv.selected_hotbar_slot)
}

impl Bot {
    /// The first main-inventory slot holding `name` (`minecraft:bread`), hotbar first.
    pub fn find_item(&self, name: &str) -> Option<SlotRef> {
        let id = self.state.items.id(name)?;
        let main = &self.state.inventory.main;
        (0..main.len()).find(|&i| !main[i].is_empty() && main[i].network_id == id).map(|i| SlotRef::Main(i as u8))
    }

    /// Puts the item in `from` on (armour, offhand) through the inventory screen, or brings it to
    /// the hotbar and selects it (`Hand`). A worn item is swapped back into `from`.
    pub async fn equip(&mut self, from: SlotRef, to: Destination) -> Result<(), ActionError> {
        self.equip_reporting(from, to, EquipMethod::Inventory).await.map(|_| ())
    }

    /// [`Bot::equip`], reporting which request shape moved the item and what the server said about
    /// the ones that did not.
    pub async fn equip_reporting(&mut self, from: SlotRef, to: Destination, method: EquipMethod) -> Result<EquipReport, ActionError> {
        self.equip_by_reporting(from, to, method).await
    }

    /// [`Bot::equip`] choosing how armour goes on. With [`EquipMethod::Use`] the piece is first
    /// brought to the hand, and a worn piece ends up in the hand's slot.
    pub async fn equip_by(&mut self, from: SlotRef, to: Destination, method: EquipMethod) -> Result<(), ActionError> {
        self.equip_by_reporting(from, to, method).await.map(|_| ())
    }

    async fn equip_by_reporting(&mut self, from: SlotRef, to: Destination, method: EquipMethod) -> Result<EquipReport, ActionError> {
        // A report from an earlier call must not answer for this one, whether this one succeeds or
        // fails before any request goes out (an empty slot, the wrong armour type).
        self.last_equip_report = None;
        let result = self.equip_by_inner(from, to, method).await;
        if let Ok(report) = &result {
            self.last_equip_report = Some(report.clone());
        }
        result
    }

    async fn equip_by_inner(&mut self, from: SlotRef, to: Destination, method: EquipMethod) -> Result<EquipReport, ActionError> {
        let stack = from.stack(&self.state).filter(|s| !s.is_empty()).cloned();
        let stack = stack.ok_or_else(|| ActionError::NotPossible(format!("{from:?} is empty")))?;
        let Some(slot) = to.armor_slot() else {
            return match (to, method) {
                (_, EquipMethod::Use) => Err(ActionError::NotPossible(format!("only armour can be put on by use, not {to:?}"))),
                (Destination::Hand, _) => self.equip_hand_reporting(from, &stack, true).await,
                _ => self.wear_reporting(from, &stack, SlotRef::Offhand, whole(&stack)).await,
            };
        };
        let name = self.state.items.name(stack.network_id).unwrap_or_default();
        if armor_slot(name) != Some(slot) {
            return Err(ActionError::NotPossible(format!("{name} cannot be worn as {to:?}")));
        }
        match method {
            EquipMethod::Inventory => self.wear_reporting(from, &stack, SlotRef::Armor(slot), 1).await,
            EquipMethod::Use => self.wear_by_use(from, &stack, slot).await.map(|()| EquipReport::worn_by_use()),
        }
    }

    async fn wear_by_use(&mut self, from: SlotRef, stack: &ItemStack, slot: u8) -> Result<(), ActionError> {
        if from != SlotRef::Main(self.state.inventory.selected_hotbar_slot) {
            self.equip_hand(from, stack).await?;
            let pause = self.human.between(HOTBAR_SWITCH);
            self.pause(pause).await?;
        }
        let before = self.state.inventory.armor[usize::from(slot)].clone();
        self.use_item();
        // BDS swaps hand and armour slot without telling this client.
        if stack.count == 1 {
            let inv = &mut self.state.inventory;
            let hand = usize::from(inv.selected_hotbar_slot);
            inv.main[hand] = std::mem::replace(&mut inv.armor[usize::from(slot)], inv.main[hand].clone());
            return Ok(());
        }
        let worn = self.wait_until(WEAR_TIMEOUT, |bot, _| {
            let armor = &bot.state.inventory.armor[usize::from(slot)];
            (*armor != before && armor.network_id == stack.network_id).then_some(())
        });
        match worn.await {
            Err(ActionError::Timeout) => Err(ActionError::Rejected("the server did not put the armour on".into())),
            other => other,
        }
    }

    /// Moves the item into an empty `to` with the shapes of [`equip_steps`], trying the next one only
    /// when the server refused the last with [`CANNOT_PLACE_ITEM`].
    async fn wear_reporting(&mut self, from: SlotRef, stack: &ItemStack, to: SlotRef, count: u8) -> Result<EquipReport, ActionError> {
        if from == to {
            return Ok(EquipReport::already_there());
        }
        if to.stack(&self.state).is_some_and(|s| !s.is_empty()) {
            if u16::from(count) < stack.count {
                return Err(ActionError::NotPossible(format!("{to:?} is occupied; move one item from the stack first")));
            }
            let report = self.swap_items(from, to).await.map(|()| EquipReport::single(EquipAction::Swap))?;
            self.last_equip_report = Some(report.clone());
            return Ok(report);
        }
        let steps = equip_steps(count, stack.count);
        let mut attempts: Vec<EquipAttempt> = Vec::new();
        for (i, step) in steps.iter().copied().enumerate() {
            match self.equip_step(step, from, to, count).await {
                Ok(()) => {
                    let report = EquipReport { action_used: step.name(), status_translated: None, attempts };
                    self.last_equip_report = Some(report.clone());
                    return Ok(report);
                }
                Err(e) => {
                    // The code is what decides whether another shape is worth a request; the text is
                    // only what a human reads afterwards.
                    let rejection = self.take_rejection();
                    attempts.push(EquipAttempt {
                        action: step.name(),
                        status: rejection.map(|r| r.translated()),
                        message: e.to_string(),
                    });
                    if next_equip_action(&steps[i + 1..], &[], rejection.map(|r| r.code)).is_none() {
                        // The refusal outlives the error: the code and what each shape was answered
                        // with are what a caller can act on, the message is only for a human.
                        self.last_equip_report = Some(EquipReport {
                            action_used: attempts.last().map_or("none", |a| a.action),
                            status_translated: attempts.last().and_then(|a| a.status.clone()),
                            attempts: attempts.clone(),
                        });
                        return Err(enrich_equip_failure(e, &attempts));
                    }
                }
            }
        }
        Err(ActionError::Rejected(format!("equip moved nothing: {}", attempts.iter().map(|a| a.action).collect::<Vec<_>>().join(", "))))
    }

    /// One attempt at moving the item with `step`.
    async fn equip_step(&mut self, step: EquipAction, from: SlotRef, to: SlotRef, _count: u8) -> Result<(), ActionError> {
        match step {
            EquipAction::Swap => self.swap_items(from, to).await,
            EquipAction::Clicks => {
                // A click picks the whole stack up; `equip_steps` only offers this shape when the
                // whole stack is what moves, so the count needs no splitting here.
                self.click_slot(from, Click::Left).await?;
                if let Err(e) = self.click_slot(to, Click::Left).await {
                    // The stack is on the cursor now: put it back, so a refused equip leaves nothing
                    // hanging there for the next request to carry.
                    if let Err(back) = self.click_slot(from, Click::Left).await {
                        tracing::debug!(%back, "could not put the cursor stack back after a refused place");
                    }
                    return Err(e);
                }
                Ok(())
            }
        }
    }

    async fn equip_hand(&mut self, from: SlotRef, stack: &ItemStack) -> Result<(), ActionError> {
        self.equip_hand_paced(from, stack, true).await
    }

    async fn equip_hand_reporting(&mut self, from: SlotRef, stack: &ItemStack, human: bool) -> Result<EquipReport, ActionError> {
        if from == SlotRef::Main(self.state.inventory.selected_hotbar_slot) {
            return Ok(EquipReport::already_there());
        }
        let (target, range) = match from {
            SlotRef::Main(i) if i < Inventory::HOTBAR_SLOTS => (i, HOTBAR_KEY),
            _ => {
                let target = hotbar_target(&self.state.inventory);
                let report = self.wear_reporting(from, stack, SlotRef::Main(target), whole(stack)).await?;
                if human {
                    let pause = self.human.between(CLICK);
                    self.pause(pause).await?;
                }
                self.select_hotbar(target)?;
                return Ok(report);
            }
        };
        if human {
            let pause = self.human.between(range);
            self.pause(pause).await?;
        }
        self.select_hotbar(target)?;
        Ok(EquipReport::single(EquipAction::Swap))
    }

    /// Brings `from` to the hand; `human`: pause as a player does first (a key press for a hotbar slot, a
    /// screen click for one moved up from the inventory), none when false (the Optimal profile).
    pub(crate) async fn equip_hand_paced(&mut self, from: SlotRef, stack: &ItemStack, human: bool) -> Result<(), ActionError> {
        self.equip_hand_reporting(from, stack, human).await.map(|_| ())
    }

}

impl EquipReport {
    fn already_there() -> Self {
        Self { action_used: "none", status_translated: None, attempts: Vec::new() }
    }

    fn single(step: EquipAction) -> Self {
        Self { action_used: step.name(), status_translated: None, attempts: Vec::new() }
    }

    fn worn_by_use() -> Self {
        Self { action_used: "use", status_translated: None, attempts: Vec::new() }
    }

    /// The one-line account of the attempts, for a failure message.
    pub fn summarize(&self) -> String {
        self.attempts
            .iter()
            .map(|a| match &a.status {
                Some(status) => format!("{} → {status}", a.action),
                None => format!("{} → {}", a.action, a.message),
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Keeps the failure's kind (a refusal stays a refusal) and says which shapes were tried and what
/// each one was answered with.
fn enrich_equip_failure(err: ActionError, attempts: &[EquipAttempt]) -> ActionError {
    let report = EquipReport {
        action_used: attempts.last().map_or("none", |a| a.action),
        status_translated: attempts.last().and_then(|a| a.status.clone()),
        attempts: attempts.to_vec(),
    };
    let tried = report.summarize();
    match err {
        ActionError::Rejected(message) => ActionError::Rejected(format!("{message} (tried: {tried})")),
        ActionError::NotPossible(message) => ActionError::NotPossible(format!("{message} (tried: {tried})")),
        other => other,
    }
}

fn whole(stack: &ItemStack) -> u8 {
    u8::try_from(stack.count).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod t125_tests {
    use super::{equip_steps, next_equip_action, EquipAction, CANNOT_PLACE_ITEM};

    /// The ladder is a response to one specific refusal: `CannotPlaceItem` says the move was
    /// understood and could not be carried out, so another shape may work. Anything else gets no
    /// second request.
    #[test]
    fn only_cannot_place_item_moves_down_the_ladder() {
        let steps = [EquipAction::Swap, EquipAction::Clicks];
        assert_eq!(next_equip_action(&steps[1..], &[], Some(CANNOT_PLACE_ITEM)), Some(EquipAction::Clicks));
        assert_eq!(next_equip_action(&steps[1..], &[], Some(1)), None, "a plain Error says nothing about the shape");
        assert_eq!(next_equip_action(&steps[1..], &[], None), None, "no refusal, no second request");
        assert_eq!(next_equip_action(&steps[1..], &[], Some(54)), None, "CannotSwapItem is a refusal of the *other* shape");
        assert_eq!(next_equip_action(&[], &[], Some(CANNOT_PLACE_ITEM)), None, "the ladder ends");
    }

    /// The first shape is the protocol's exchange for an offhand/armour slot; the click shape is
    /// second. A partial move never clicks, because a click carries the whole stack.
    #[test]
    fn a_whole_stack_tries_the_exchange_then_two_clicks() {
        assert_eq!(equip_steps(12, 12), vec![EquipAction::Swap, EquipAction::Clicks]);
        assert_eq!(equip_steps(1, 1), vec![EquipAction::Swap, EquipAction::Clicks]);
        assert_eq!(equip_steps(1, 12), vec![EquipAction::Swap], "one of twelve cannot be clicked out");
        assert_eq!(EquipAction::Swap.name(), "swap");
        assert_eq!(EquipAction::Clicks.name(), "clicks");
    }
}

#[cfg(test)]
mod t129_tests {
    use std::time::Duration;

    use acacia_client::proto::Packet;
    use acacia_client::proto::packets::{InventoryContent, ItemStackResponse};
    use acacia_client::proto::types::{
        ContainerSlotType, FullContainerName, ItemStackResponsesItem, ItemStackResponsesItemStatus, ItemV4, ItemV4Extra,
        WindowIDVarint,
    };
    use acacia_testserver::{FakeServer, Script};
    use tokio::time::timeout;

    use super::{Destination, EquipMethod};
    use crate::client::{Client, TransportKind};
    use crate::items::SlotRef;
    use crate::proto::packets::ItemStackRequest;
    use crate::state::Inventory;
    use crate::{Bot, BotConfig};

    fn item(network_id: i16, count: u16) -> ItemV4 {
        let stack_id = (count > 0).then_some(i32::from(network_id));
        ItemV4 { network_id, count, metadata: 0, has_stack_id: stack_id.is_some(), stack_id, block_runtime_id: 0, extra: ItemV4Extra::Default(None) }
    }

    async fn join(name: &str) -> (FakeServer, Bot) {
        let server = FakeServer::start(Script::bds_spawn()).await.unwrap();
        let builder = Client::builder(server.addr().to_string()).offline(name).transport(TransportKind::RakNet);
        let mut bot = Bot::connect(builder, BotConfig::default()).await.expect("bot joins the fake server");
        server.spawned().await.expect("script reaches PlayerSpawn");
        until(&mut bot, 10, "the item registry", |bot| !bot.state().items.is_empty()).await;
        (server, bot)
    }

    async fn until(bot: &mut Bot, secs: u64, what: &str, done: impl Fn(&Bot) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while !done(bot) {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            let _ = timeout(Duration::from_millis(50), bot.next()).await;
        }
    }

    /// Puts `count` of the registry's first item in main-inventory slot `slot`.
    fn give(server: &FakeServer, bot: &Bot, slot: usize, count: u16) -> SlotRef {
        let id = bot.state().items.id("minecraft:stone").or_else(|| bot.state().items.id("minecraft:dirt")).unwrap_or(1) as i16;
        let slots = (0..36).map(|i| if i == slot { item(id, count) } else { item(0, 0) }).collect();
        server.send(&InventoryContent {
            window_id: WindowIDVarint::from_raw(0),
            input: slots,
            container: FullContainerName { container_id: ContainerSlotType::Inventory, dynamic_container_id: None },
            storage_item: item(0, 0),
        });
        SlotRef::Main(slot as u8)
    }

    /// The item is already in the selected hotbar slot: `equip` to the hand moves nothing, so it sends
    /// no request and still reports what it did — and reading the report is what clears it.
    #[tokio::test]
    async fn a_successful_equip_leaves_one_report_and_reading_it_clears_it() {
        let (server, mut bot) = join("t129-success").await;
        let selected = bot.state().inventory.selected_hotbar_slot as usize;
        let from = give(&server, &bot, selected, 3);
        until(&mut bot, 5, "the stack in the hotbar", |bot| {
            bot.state().inventory.main[selected].count == 3
        })
        .await;
        let report = bot.equip_reporting(from, Destination::Hand, EquipMethod::Inventory).await.expect("to the hand");
        assert_eq!(report.action_used, "none", "nothing had to move");
        let taken = bot.take_equip_report().expect("a report is left behind");
        assert_eq!(taken.action_used, "none");
        assert_eq!(bot.take_equip_report(), None, "reading it clears it");
        assert!(taken.attempts.is_empty());
        let _ = Inventory::HOTBAR_SLOTS;
    }

    /// The t123 shape: the server refuses every shape with `55 CannotPlaceItem`. The error can only say
    /// so in prose, so the report is where a caller reads the code — and each shape that was tried.
    #[tokio::test]
    async fn a_refused_equip_still_reports_the_status_and_the_shapes_it_tried() {
        let (server, mut bot) = join("t129-refused").await;
        let slot = Inventory::HOTBAR_SLOTS as usize + 1;
        let from = give(&server, &bot, slot, 3);
        until(&mut bot, 5, "the stack in the inventory", |bot| bot.state().inventory.main[slot].count == 3).await;

        // Its own scope: the pinned call borrows the bot until it is dropped, and the report is read
        // from the bot right after.
        let refusal = {
        let call = bot.equip_reporting(from, Destination::OffHand, EquipMethod::Inventory);
        tokio::pin!(call);
        // Answer every request the way t123's server did: understood, and refused.
        loop {
            tokio::select! {
                result = &mut call => break result,
                request = server.recv::<ItemStackRequest>() => {
                    let request = request.expect("the bot sent a request");
                    for one in &request.requests {
                        server.send(&ItemStackResponse {
                            responses: vec![ItemStackResponsesItem {
                                status: ItemStackResponsesItemStatus::Unknown(55),
                                request_id: one.request_id,
                                containers: None,
                            }],
                        });
                    }
                }
            }
        }
        };
        let error = refusal.expect_err("the server refused every shape");
        assert!(error.to_string().contains("CannotPlaceItem"), "the prose still names it: {error}");

        let report = bot.take_equip_report().expect("a refused equip leaves a report too");
        assert_eq!(report.status_translated.as_deref(), Some("55 CannotPlaceItem"));
        assert_eq!(
            report.attempts.len(),
            2,
            "the exchange, then the two clicks: {:?}",
            report.attempts.iter().map(|a| a.action).collect::<Vec<_>>()
        );
        assert!(report.attempts.iter().all(|a| a.status.as_deref() == Some("55 CannotPlaceItem")));
        assert_eq!(report.action_used, report.attempts.last().expect("one attempt").action);
        assert_eq!(bot.take_equip_report(), None, "reading it clears it");
    }
}
