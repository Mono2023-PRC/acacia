//! The capabilities `t4` added, against the fake server: collecting a drop, depositing into an open
//! container, and the `activity`/`stop_all` pair. The wire shapes are the ones the trackers already
//! read in `tests/geyser_containers.rs` and `src/state/entities/tests.rs`.

use std::time::{Duration, Instant};

use acacia_bot::client::{Client, TransportKind};
use acacia_bot::proto::packets::{AddItemEntity, ContainerOpen, InventoryContent, ItemStackRequest, ItemStackResponse, TakeItemEntity};
use acacia_bot::proto::types::{
    BlockCoordinates, ContainerSlotType, FullContainerName, ItemStackResponsesItem, ItemStackResponsesItemStatus, ItemV4, ItemV4Extra,
    Vec3f, WindowID, WindowIDVarint, WindowType,
};
use acacia_bot::state::Trackers;
use acacia_bot::{Activity, Bot, BotConfig};
use acacia_testserver::{FakeServer, Script};
use tokio::time::timeout;

/// The chest the deposit test opens (a position the fake spawn's terrain never has blocks at).
const CHEST: BlockCoordinates = BlockCoordinates { x: 1, y: 64, z: 1 };

async fn join(name: &str) -> (FakeServer, Bot) {
    let server = FakeServer::start(Script::bds_spawn()).await.unwrap();
    let builder = Client::builder(server.addr().to_string()).offline(name).transport(TransportKind::RakNet);
    let config = BotConfig { physics: true, trackers: Trackers { entities: true, ..Trackers::default() }, ..BotConfig::default() };
    let bot = Bot::connect(builder, config).await.expect("bot joins the fake server");
    server.spawned().await.expect("script reaches PlayerSpawn");
    (server, bot)
}

/// Polls the bot until `done` holds.
async fn until(bot: &mut Bot, secs: u64, what: &str, done: impl Fn(&Bot) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done(bot) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        let _ = timeout(Duration::from_millis(50), bot.next()).await;
    }
}

fn item(network_id: i16, count: u16) -> ItemV4 {
    let stack_id = (count > 0).then_some(i32::from(network_id));
    ItemV4 { network_id, count, metadata: 0, has_stack_id: stack_id.is_some(), stack_id, block_runtime_id: 0, extra: ItemV4Extra::Default(None) }
}

fn inventory(server: &FakeServer, window: i32, slots: Vec<ItemV4>) {
    server.send(&InventoryContent {
        window_id: WindowIDVarint::from_raw(i64::from(window)),
        input: slots,
        container: FullContainerName { container_id: ContainerSlotType::Inventory, dynamic_container_id: None },
        storage_item: item(0, 0),
    });
}

async fn open_chest(server: &FakeServer, bot: &mut Bot) {
    server.send(&ContainerOpen { window_id: WindowID::First, window_type: WindowType::Container, coordinates: CHEST, runtime_entity_id: -1 });
    server.send(&InventoryContent {
        window_id: WindowIDVarint::from_raw(1),
        input: vec![item(0, 0)],
        container: FullContainerName { container_id: ContainerSlotType::Container, dynamic_container_id: None },
        storage_item: item(0, 0),
    });
    until(bot, 5, "the chest open with contents", |bot| bot.open_container().is_some_and(|c| !c.slots.is_empty())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drop_at_the_feet_is_collected() {
    let (server, mut bot) = join("Collector").await;
    // The StartGame position is a placeholder until the script's own move lands: let the spawn settle,
    // then drop at the feet (the fake spawn is all air, so a started physics bot would fall away).
    bot.wait_ticks(20).await.expect("spawn settle");
    let p = bot.movement().and_then(|m| m.position()).map_or_else(
        || {
            let p = bot.state().player.position.clone();
            Vec3f { x: p.x, y: p.y, z: p.z }
        },
        |[x, y, z]| Vec3f { x, y, z },
    );
    let id = 5i16;
    server.send(&AddItemEntity {
        entity_id_self: 70,
        runtime_entity_id: 700,
        item: item(id, 2),
        position: p,
        velocity: Vec3f { x: 0.0, y: 0.0, z: 0.0 },
        metadata: Vec::new(),
        is_from_fishing: false,
    });
    until(&mut bot, 5, "the drop tracked", |bot| bot.is_dropped_item(700)).await;
    assert_eq!(bot.nearest_dropped_item(4.0, None), Some(700));
    assert_eq!(bot.nearest_dropped_item(4.0, Some("minecraft:apple")), None, "the item registry has no apple");

    let target = u32::try_from(bot.state().player.runtime_entity_id).expect("runtime id");
    let handover = async {
        // The server hands the drop over a moment after the bot has stood on it (the wait polls 2 s).
        tokio::time::sleep(Duration::from_millis(150)).await;
        server.send(&TakeItemEntity { runtime_entity_id: 700, target });
        inventory(&server, 0, vec![item(id, 2)]);
    };
    let (collected, ()) = tokio::join!(bot.collect(700), handover);
    collected.expect("the drop is collected");
    assert_eq!(bot.state().inventory.count_of(i32::from(id)), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collecting_a_drop_that_was_already_taken_is_refused() {
    let (server, mut bot) = join("LateCollector").await;
    let p = bot.state().player.position.clone();
    let id = 5i16;
    server.send(&AddItemEntity {
        entity_id_self: 71,
        runtime_entity_id: 701,
        item: item(id, 1),
        position: Vec3f { x: p.x, y: p.y, z: p.z },
        velocity: Vec3f { x: 0.0, y: 0.0, z: 0.0 },
        metadata: Vec::new(),
        is_from_fishing: false,
    });
    until(&mut bot, 5, "the drop tracked", |bot| bot.is_dropped_item(701)).await;
    // Someone else took it before the walk: the entity disappears without an inventory change.
    server.send(&acacia_bot::proto::packets::RemoveEntity { entity_id_self: 71 });
    until(&mut bot, 5, "the drop gone", |bot| !bot.is_dropped_item(701)).await;
    // The id is no longer a tracked drop, so the call refuses before waiting for anything.
    let err = bot.collect(701).await.expect_err("no such drop");
    assert!(matches!(err, acacia_bot::ActionError::NotPossible(_)), "{err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deposit_moves_a_stack_into_the_open_chest() {
    let (server, mut bot) = join("Depositor").await;
    let id = 6i16;
    inventory(&server, 0, vec![item(id, 8)]);
    until(&mut bot, 5, "the stack tracked", |bot| bot.state().inventory.main[0].count == 8).await;
    open_chest(&server, &mut bot).await;
    assert_eq!(bot.activity(), Activity::Container);

    let responder = async {
        let request = server.recv::<ItemStackRequest>().await.expect("an item stack request went out");
        let entry = ItemStackResponsesItem { status: ItemStackResponsesItemStatus::Ok, request_id: request.requests[0].request_id, containers: None };
        server.send(&ItemStackResponse { responses: vec![entry] });
    };
    let (moved, ()) = tokio::join!(bot.deposit_all([CHEST.x, CHEST.y, CHEST.z]), responder);
    let moved = moved.expect("the deposit is accepted");
    assert_eq!(moved, 1, "one stack went in");
    assert_eq!(bot.state().inventory.count_of(i32::from(id)), 0, "the inventory gave the stack up");
    assert!(bot.open_container().expect("the chest stays open").iter().any(|(_, s)| s.network_id == i32::from(id)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_deposit_is_refused_and_things_stay_as_they_were() {
    let (server, mut bot) = join("EmptyDepositor").await;
    open_chest(&server, &mut bot).await;
    let err = bot.deposit_all([CHEST.x, CHEST.y, CHEST.z]).await.expect_err("nothing to deposit");
    assert!(matches!(err, acacia_bot::ActionError::NotPossible(_)), "{err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_reports_walking_and_stop_all_clears_it() {
    let (server, mut bot) = join("Idler").await;
    assert_eq!(bot.activity(), Activity::Idle);
    assert!(!bot.is_busy());
    bot.controls().expect("physics bot").forward = 1.0;
    assert_eq!(bot.activity(), Activity::Walking);
    assert!(bot.is_busy());
    bot.stop_all();
    assert_eq!(bot.activity(), Activity::Idle);
    let c = bot.controls().expect("physics bot");
    assert_eq!((c.forward, c.strafe, c.jump), (0.0, 0.0, false));
    // Still connected: stop_all ends actions, not the session.
    let _ = server;
}

/// NetEase can answer the first `ItemStackRequest` after a window opens seconds late (relay run84:
/// the server produced the planks and answered Status=0, the client reported `timed out` at 3 s).
/// The write now waits 12 s, so a slow answer is an answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_item_stack_response_is_not_a_failure() {
    let (server, mut bot) = join("SlowResponse").await;
    let id = 6i16;
    inventory(&server, 0, vec![item(id, 4)]);
    until(&mut bot, 5, "the stack tracked", |bot| bot.state().inventory.main[0].count == 4).await;
    open_chest(&server, &mut bot).await;

    let responder = async {
        let request = server.recv::<ItemStackRequest>().await.expect("a request went out");
        // Longer than the old 3 s wait, shorter than the new 12 s one.
        tokio::time::sleep(Duration::from_millis(4500)).await;
        let entry = ItemStackResponsesItem { status: ItemStackResponsesItemStatus::Ok, request_id: request.requests[0].request_id, containers: None };
        server.send(&ItemStackResponse { responses: vec![entry] });
    };
    let (moved, ()) = tokio::join!(bot.deposit_all([CHEST.x, CHEST.y, CHEST.z]), responder);
    assert_eq!(moved.expect("a slow answer is still an answer"), 1);
    assert_eq!(bot.state().inventory.count_of(i32::from(id)), 0);
}
