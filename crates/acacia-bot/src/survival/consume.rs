use acacia_client::proto::packets::{EntityEvent, EntityEventEventId};
use acacia_client::proto::types::{GameMode, InputData};

use super::equip::Destination;
use super::food::{always_consumable, use_ticks};
use super::item_use::{eating_data, ItemUse, Snapshot, UseTick};
use crate::human::USE_RELEASE;
use crate::interact::wire;
use crate::{ActionError, Bot};

/// Why `name` cannot be eaten right now, worded the way the tool layer recognises; `None` when it can.
///
/// The stomach decides from the player state alone, so it can be asked before anything is moved: `Bot::eat` calls it
/// before the equip, which is what keeps the "not hungry" path free of inventory writes (t124). Creative and
/// spectator bots eat for the effect, and `always_consumable` items (milk, golden apples) go down whatever the bar
/// says — both are vanilla rules and predate this gate.
pub(crate) fn eat_blocker(hunger: f32, game_mode: GameMode, name: &str) -> Option<String> {
    if use_ticks(name).is_none() {
        return Some(format!("cannot eat or drink {name:?}"));
    }
    let hungry = hunger < 20.0 || !matches!(game_mode, GameMode::Survival | GameMode::Adventure);
    (!hungry && !always_consumable(name)).then(|| format!("not hungry enough to eat {name}"))
}

impl Bot {
    /// Eats `name`, swapping it into the hand first — but only once the bot is actually hungry (t124).
    ///
    /// The order matters beyond tidiness: `t116` refused an `eat {}` with "not hungry enough", yet the apple had
    /// already been moved to the hand. Every write is a packet an anti-cheat sees and a trace has to explain, so the
    /// check runs before the swap. A hungry bot behaves exactly as before: find, equip, consume.
    pub async fn eat(&mut self, name: &str) -> Result<(), ActionError> {
        if let Some(why) = eat_blocker(self.state.player.hunger, self.state.player.game_mode, name) {
            return Err(ActionError::NotPossible(why));
        }
        let from = self.find_item(name).ok_or_else(|| ActionError::NotPossible(format!("no {name} in the inventory")))?;
        self.equip(from, Destination::Hand).await?;
        self.consume().await
    }

    /// Eats or drinks the held item: holds "use" until the server has consumed it, then lets go
    /// (sending no `ReleaseItem`: survival/item_use.rs).
    /// Waits for the server's inventory or hunger update (`Rejected` if none comes). Dropping the
    /// future does not cancel the use: it finishes on its own as long as [`Bot::next`] is polled.
    pub async fn consume(&mut self) -> Result<(), ActionError> {
        self.start_item_use(false)?;
        loop {
            self.next_tick(|_, _| false).await?;
            if let Some(result) = self.survival.result.take() {
                return result;
            }
        }
    }

    /// Holding "use" on a food or drink (an explicit [`Bot::consume`] or auto-eat).
    pub fn is_using_item(&self) -> bool {
        self.survival.item_use.is_some()
    }

    pub(crate) fn start_item_use(&mut self, auto: bool) -> Result<(), ActionError> {
        if self.survival.item_use.is_some() {
            return Err(ActionError::NotPossible("already using an item".into()));
        }
        let held = self.state.inventory.held();
        let name = self.state.items.name(held.network_id).unwrap_or_default();
        // One gate for both paths (t124): the hand-held `consume` and `Bot::eat`, which asks *before* it swaps the
        // item into the hand — a swap the server sees and nothing needs when the answer is "not hungry".
        if let Some(why) = eat_blocker(self.state.player.hunger, self.state.player.game_mode, name) {
            return Err(ActionError::NotPossible(why));
        }
        let ticks = use_ticks(name).expect("eat_blocker checked it is edible");
        let release = self.human.ticks_between(USE_RELEASE);
        let slot = self.state.inventory.selected_hotbar_slot;
        self.survival.item_use = Some(ItemUse::new(slot, self.use_snapshot(), ticks, release, auto));
        self.survival.result = None;
        self.client.send(&wire::click_air(self.hand()));
        self.queued_flags.push(InputData::StartUsingItem);
        self.set_using_item(true);
        Ok(())
    }

    /// Runs the item use one client tick on.
    pub(crate) fn tick_item_use(&mut self) {
        let snapshot = self.use_snapshot();
        let (selected, alive) = (self.state.inventory.selected_hotbar_slot, self.state.player.alive);
        let Some(item_use) = self.survival.item_use.as_mut() else { return };
        match item_use.tick(&snapshot, selected, alive) {
            UseTick::Nothing => {}
            UseTick::Effect => {
                let held = self.state.inventory.held();
                self.client.send(&EntityEvent {
                    runtime_entity_id: self.runtime_id(),
                    event_id: EntityEventEventId::EatingItem,
                    data: eating_data(held.network_id, held.metadata),
                    fire_at_position: None,
                });
            }
            UseTick::Finish => {
                self.client.send(&wire::finish_use(self.hand()));
            }
            UseTick::Done(result) => {
                let auto = item_use.auto;
                self.survival.item_use = None;
                self.set_using_item(false);
                if let Err(e) = &result {
                    tracing::debug!(error = %e, auto, "item use ended");
                }
                if !auto {
                    self.survival.result = Some(result);
                }
            }
        }
    }

    fn use_snapshot(&self) -> Snapshot {
        let held = self.state.inventory.held();
        let p = &self.state.player;
        Snapshot { network_id: held.network_id, count: held.count, hunger: p.hunger, saturation: p.saturation }
    }

    fn set_using_item(&mut self, on: bool) {
        if let Some(m) = self.movement.as_mut() {
            m.using_item = on;
        }
    }
}


#[cfg(test)]
mod t124_tests {

    use std::time::Duration;

    use acacia_client::proto::packets::{InventoryContent, MobEquipment, PlayerHotbar, UpdateAttributes};
    use acacia_client::proto::Packet;
    use acacia_client::proto::types::{
        ContainerSlotType, FullContainerName, GameMode, ItemV4, ItemV4Extra, PlayerAttributesItem, WindowIDVarint,
    };
    use acacia_testserver::{FakeServer, Script};
    use tokio::time::timeout;

    use crate::client::{Client, TransportKind};
    use crate::proto::packets::{InventoryTransaction, ItemStackRequest};
    use super::eat_blocker;
    use crate::{Bot, BotConfig};

    #[test]
    fn the_stomach_is_asked_before_anything_is_moved() {
        use GameMode::{Creative, Survival};
        // The exact sentence `acacia-agent` matches on: it rewrites the hint from it, so it must not drift.
        assert_eq!(eat_blocker(20.0, Survival, "minecraft:apple").as_deref(), Some("not hungry enough to eat minecraft:apple"));
        assert_eq!(eat_blocker(19.9, Survival, "minecraft:apple"), None, "a bar short of full is hungry");
        assert_eq!(eat_blocker(20.0, Creative, "minecraft:apple"), None, "creative eats for the effect, not the bar");
        assert_eq!(eat_blocker(20.0, Survival, "minecraft:golden_apple"), None, "always consumable, full or not");
        assert_eq!(eat_blocker(20.0, Survival, "minecraft:milk_bucket"), None, "drinking is never gated by the bar");
        // Not food at all: still refused, and still without touching the inventory.
        assert_eq!(eat_blocker(1.0, Survival, "minecraft:stone").as_deref(), Some("cannot eat or drink \"minecraft:stone\""));
    }

    async fn join(name: &str) -> (FakeServer, Bot) {
        let server = FakeServer::start(Script::bds_spawn()).await.unwrap();
        let builder = Client::builder(server.addr().to_string()).offline(name).transport(TransportKind::RakNet);
        let mut bot = Bot::connect(builder, BotConfig::default()).await.expect("bot joins the fake server");
        server.spawned().await.expect("script reaches PlayerSpawn");
        // The item registry goes out after the client's cache-miss exchange, so the names exist a few packets later than
        // the spawn (the same wait `tests/desync_gate.rs` relies on to seed its inventory).
        until(&mut bot, 10, "the item registry", |bot| !bot.state().items.is_empty()).await;
        (server, bot)
    }

    /// Polls the bot until `done` holds.
    async fn until(bot: &mut Bot, secs: u64, what: &str, done: impl Fn(&Bot) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while !done(bot) {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            let _ = timeout(Duration::from_millis(50), bot.next()).await;
        }
    }

    fn item(network_id: i16, count: u16) -> ItemV4 {
        let stack_id = (count > 0).then_some(i32::from(network_id));
        ItemV4 { network_id, count, metadata: 0, has_stack_id: stack_id.is_some(), stack_id, block_runtime_id: 0, extra: ItemV4Extra::Default(None) }
    }

    /// A food the fake server's item registry actually knows, so the test does not depend on which capture the script
    /// was made from (the pure test above pins the wording on `minecraft:apple` regardless).
    fn a_food(bot: &Bot) -> &'static str {
        const CANDIDATES: [&str; 5] = ["minecraft:apple", "minecraft:bread", "minecraft:cooked_beef", "minecraft:carrot", "minecraft:golden_apple"];
        CANDIDATES
            .into_iter()
            .find(|name| bot.state().items.id(name).is_some())
            .unwrap_or_else(|| panic!("no food in the registry ({} items)", bot.state().items.len()))
    }

    /// Puts three of `name` in the tenth main-inventory slot of the inventory window.
    fn give(server: &FakeServer, bot: &Bot, name: &str) {
        let id = bot.state().items.id(name).expect("the registry has it") as i16;
        let slots = (0..36).map(|i| if i == 9 { item(id, 3) } else { item(0, 0) }).collect();
        server.send(&InventoryContent {
            window_id: WindowIDVarint::from_raw(0),
            input: slots,
            container: FullContainerName { container_id: ContainerSlotType::Inventory, dynamic_container_id: None },
            storage_item: item(0, 0),
        });
    }

    /// Every packet since `from` that would move an item: equip, hotbar select, a stack request or a transaction.
    fn inventory_writes(server: &FakeServer, from: usize) -> Vec<&'static str> {
        let kinds = [
            (MobEquipment::ID, "MobEquipment"),
            (PlayerHotbar::ID, "PlayerHotbar"),
            (ItemStackRequest::ID, "ItemStackRequest"),
            (InventoryTransaction::ID, "InventoryTransaction"),
        ];
        server.received().into_iter().skip(from).filter_map(|r| kinds.iter().find(|(id, _)| *id == r.packet.id).map(|(_, name)| *name)).collect()
    }

    /// `minecraft:player.hunger` at `current`, as the server would report it after a meal or a jog.
    fn set_hunger(server: &FakeServer, bot: &Bot, current: f32) {
        server.send(&UpdateAttributes {
            runtime_entity_id: bot.state().player.runtime_entity_id,
            attributes: vec![PlayerAttributesItem {
                min: 0.0,
                max: 20.0,
                current,
                default_min: 0.0,
                default_max: 20.0,
                default: 20.0,
                name: "minecraft:player.hunger".into(),
                modifiers: Vec::new(),
            }],
            tick: 0,
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unhungry_bot_never_touches_its_inventory() {
        let (server, mut bot) = join("Unhungry").await;
        let food = a_food(&bot);
        give(&server, &bot, food);
        until(&mut bot, 5, "the food in the inventory", |bot| bot.find_item(food).is_some()).await;
        assert!(bot.state().player.hunger >= 20.0, "the fake spawn starts full");

        let from = server.received().len();
        let error = bot.eat(food).await.expect_err("a full bar refuses the food");
        let text = error.to_string();
        assert!(text.contains(&format!("not hungry enough to eat {food}")), "{text}");
        // The whole point of the ticket: the refusal costs nothing on the wire.
        assert_eq!(inventory_writes(&server, from), Vec::<&str>::new(), "the refusal must not write the inventory");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hungry_bot_still_equips_the_item_first() {
        let (server, mut bot) = join("Hungry").await;
        let food = a_food(&bot);
        give(&server, &bot, food);
        until(&mut bot, 5, "the food in the inventory", |bot| bot.find_item(food).is_some()).await;
        set_hunger(&server, &bot, 17.0);
        until(&mut bot, 5, "a hunger bar below full", |bot| bot.state().player.hunger < 20.0).await;

        let from = server.received().len();
        // The fake server never confirms the bite, so the call stays in `consume` — which is exactly what we want to look
        // at from here: the *first* thing a hungry bot does is still the equip.
        let eating = timeout(Duration::from_secs(2), bot.eat(food)).await;
        assert!(eating.is_err(), "the fake server confirms nothing, so the eat cannot finish: {eating:?}");
        let seen = inventory_writes(&server, from);
        assert!(seen.contains(&"ItemStackRequest"), "a hungry bot swaps the item into the hand: {seen:?}");
    }
}
