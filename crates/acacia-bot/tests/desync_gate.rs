//! The desync gate for **block** clicks (`place` / `dig`), contract `acacia_bot::desync` (t90).
//!
//! t88 live: `position_confirmations=0`, the server reference frozen at spawn, `place` refused with
//! `position_desync` (4.19 blocks). Entity interactions had already been routed through `may_interact` (t72), but
//! `place` still compared the raw drift against `DESYNC_LIMIT` — a reference that is only ever the spawn point can
//! be 4 blocks off after a short walk, so the refusal was about the bot, not about the click.
//!
//! The other direction — a *fresh* server confirmation that really is out of range must still refuse — cannot be
//! staged against the fake server (the correction that creates the drift also re-syncs the body, and the falling
//! spawn body keeps getting reset), so it is pinned at the decision level in `desync::tests`
//! (`a_block_click_is_only_refused_by_a_fresh_reference`), which is the function both block paths call.
//!
//! Pinned end to end here: a stale/unconfirmed reference never blocks a placement — the cell's own reason wins.

use std::time::{Duration, Instant};

use acacia_bot::client::{Client, TransportKind};
use acacia_bot::interact::Face;
use acacia_bot::pathfind::Goal;
use acacia_bot::proto::packets::{InventoryContent, UpdateSubchunkBlocks};
use acacia_bot::proto::types::{BlockCoordinates, BlockUpdate, BlockUpdateTransitionType, ContainerSlotType, FullContainerName, ItemV4, ItemV4Extra, WindowIDVarint};
use acacia_bot::{ActionError, Bot, BotConfig};
use acacia_physics::BlockPos;
use acacia_testserver::{FakeServer, Script};
use tokio::time::timeout;

const FLOOR: i32 = 12;

async fn join(name: &str) -> (FakeServer, Bot) {
    let server = FakeServer::start(Script::bds_spawn()).await.unwrap();
    let builder = Client::builder(server.addr().to_string()).offline(name).transport(TransportKind::RakNet);
    let bot = Bot::connect(builder, BotConfig { physics: true, ..BotConfig::default() }).await.expect("bot joins the fake server");
    server.spawned().await.expect("script reaches PlayerSpawn");
    (server, bot)
}

async fn until(bot: &mut Bot, secs: u64, what: &str, done: impl Fn(&Bot) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done(bot) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        let _ = timeout(Duration::from_millis(50), bot.next()).await;
    }
}

fn wire(bot: &Bot, name: &str) -> u32 {
    let world = bot.world().expect("world tracker");
    let id = world.registry().expect("registry").find(name, "").unwrap_or_else(|| panic!("{name} not in the registry"));
    world.view().expect("view").world().wire_id(id)
}

fn set_blocks(server: &FakeServer, cells: &[BlockPos], wire_id: u32) {
    let update = |&[x, y, z]: &BlockPos| BlockUpdate {
        position: BlockCoordinates { x, y, z },
        runtime_id: wire_id,
        flags: 3,
        entity_unique_id: 0,
        transition_type: BlockUpdateTransitionType::Create,
    };
    let [x, y, z] = cells[0];
    server.send(&UpdateSubchunkBlocks { x: x >> 4, y: y >> 4, z: z >> 4, blocks: cells.iter().map(update).collect(), extra: vec![] });
}

fn block_wire(bot: &Bot, [x, y, z]: BlockPos) -> Option<u32> {
    use acacia_world::BlockAccess;
    let view = bot.world()?.view()?;
    Some(view.world().wire_id(view.block(x, y, z)))
}

fn item(network_id: i16, count: u16) -> ItemV4 {
    let stack_id = (count > 0).then_some(i32::from(network_id));
    ItemV4 { network_id, count, metadata: 0, has_stack_id: stack_id.is_some(), stack_id, block_runtime_id: 0, extra: ItemV4Extra::Default(None) }
}

fn inventory(server: &FakeServer, slots: Vec<ItemV4>) {
    server.send(&InventoryContent {
        window_id: WindowIDVarint::from_raw(0),
        input: slots,
        container: FullContainerName { container_id: ContainerSlotType::Inventory, dynamic_container_id: None },
        storage_item: item(0, 0),
    });
}

/// A floor under the spawn; returns the feet cell. The replay leaves the physics body a fraction above the
/// replayed spawn and it starts falling at once, so each round lays stone under **both** candidate heights (two
/// layers deep) and pumps a few ticks; rounds repeat until the bot is actually standing (t87's lesson).
async fn stand_on_floor(server: &FakeServer, bot: &mut Bot) -> BlockPos {
    let sp = &bot.state().player.position;
    let spawn = [sp.x.floor() as i32, sp.y.floor() as i32, sp.z.floor() as i32];
    let cells: Vec<BlockPos> = (-FLOOR..=FLOOR)
        .flat_map(|dx| (-FLOOR..=FLOOR).flat_map(move |dz| (1..=3).map(move |dy| [spawn[0] + dx, spawn[1] - dy, spawn[2] + dz])))
        .collect();
    let loaded = |bot: &Bot| bot.world().and_then(|w| w.view()).is_some_and(|v| cells.iter().all(|c| v.chunk(c[0] >> 4, c[2] >> 4).is_some()));
    until(bot, 30, "the spawn chunks", loaded).await;
    let stone = wire(bot, "minecraft:stone");
    for round in 0..6 {
        // Re-aim every round: the body keeps falling, so the layer that matters moves down.
        let f = match bot.movement().and_then(|m| m.position()) {
            Some(m) => [m[0].floor() as i32, (m[1] + 0.01).floor() as i32, m[2].floor() as i32],
            None => spawn,
        };
        let plate: Vec<BlockPos> = (-4..=4)
            .flat_map(|dx| (-4..=4).flat_map(move |dz| (1..=2).map(move |dy| [f[0] + dx, f[1] - dy, f[2] + dz])))
            .collect();
        set_blocks(server, &plate, stone);
        for _ in 0..40 {
            let _ = timeout(Duration::from_millis(50), bot.next()).await;
            if bot.movement().is_some_and(|m| m.is_started() && m.on_ground()) {
                return feet(bot);
            }
        }
        assert!(round < 5, "the bot never stood on the floor");
    }
    feet(bot)
}

fn feet(bot: &Bot) -> BlockPos {
    let f = bot.movement().and_then(|m| m.position()).expect("movement started");
    [f[0].floor() as i32, (f[1] + 0.01).floor() as i32, f[2].floor() as i32]
}

/// Walks a few blocks away from the spawn cell, so the frozen (never re-sent) server reference drifts past
/// `DESYNC_LIMIT` and goes stale — the t88 situation.
async fn drift_away_from_the_spawn_reference(bot: &mut Bot, feet: BlockPos) {
    let walked = timeout(Duration::from_secs(20), bot.goto(Goal::XZ(feet[0] + 5, feet[2]))).await;
    assert!(walked.is_ok(), "the walk itself timed out");
    until(bot, 5, "the movement simulation to catch up", |bot| {
        bot.position_desync().is_some_and(|d| d.blocks > 2.0)
    })
    .await;
    // And the reference really is stale, or this test would not be about t88.
    assert!(bot.position_confirmations() <= 1, "the fake server must not confirm the position on its own");
    let ticks = bot.server_confirmed_ticks();
    assert!(ticks.is_none_or(|t| t > acacia_bot::desync::FRESH_REFERENCE_TICKS), "the spawn reference is stale ({ticks:?})");
}

/// A stone block in reach, so the placement has something to be refused by; waits until the bot sees it.
async fn stone_beside(server: &FakeServer, bot: &mut Bot, feet: BlockPos) -> BlockPos {
    let cell = [feet[0] + 1, feet[1], feet[2]];
    let stone = wire(bot, "minecraft:stone");
    set_blocks(server, &[cell], stone);
    until(bot, 5, "the stone beside the feet", |bot| block_wire(bot, cell) == Some(stone)).await;
    cell
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_reference_does_not_block_a_placement() {
    let (server, mut bot) = join("DesyncPlaceStale").await;
    let start = stand_on_floor(&server, &mut bot).await;
    let dirt = bot.state().items.id("minecraft:dirt").expect("dirt in the item registry") as i16;
    inventory(&server, vec![item(dirt, 16)]);
    until(&mut bot, 5, "the dirt in the hotbar", |bot| !bot.state().inventory.held().is_empty()).await;

    drift_away_from_the_spawn_reference(&mut bot, start).await;
    let feet = feet(&bot);
    let target = stone_beside(&server, &mut bot, feet).await;
    assert_eq!(block_wire(&bot, target), Some(wire(&bot, "minecraft:stone")), "the stone is in the bot's view");

    // The cell is occupied, so the placement cannot succeed — but the reason must be the cell, not a drift measured
    // against a reference the server never refreshed (t88: `place` said `position_desync`, desync 4.19).
    let result = bot.place_block([target[0], target[1] - 1, target[2]], Face::Up).await;
    match result {
        Ok(()) => panic!("the cell is occupied by stone, the placement cannot succeed"),
        Err(ActionError::NotPossible(why)) => {
            assert!(!why.contains("position_desync"), "a stale reference must not refuse a placement: {why}");
            assert!(why.contains("occupied"), "the refusal has to name the real cause: {why}");
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}
