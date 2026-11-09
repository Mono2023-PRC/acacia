//! Regression tests for the *event waiting* in `dig`/`place` confirmation (contract: `acacia_bot::interact`,
//! `docs/verify.md` §9.7): NetEase sometimes takes seconds to echo the cell it changed, so an action the server
//! did carry out must not be reported as `no_effect`.
//!
//! The fake server plays the slow echo: the block update for the cell arrives 3 s after the action, while the
//! drop (dig) or the block (place) is already there. Physics bot on a stone floor laid over the replayed spawn.
use std::time::{Duration, Instant};

use acacia_bot::client::{Client, TransportKind};
use acacia_bot::proto::packets::{InventoryContent, PlayerAuthInput, UpdateSubchunkBlocks};
use acacia_bot::proto::types::{BlockCoordinates, BlockUpdate, BlockUpdateTransitionType, ContainerSlotType, FullContainerName, InputData, ItemV4, ItemV4Extra, WindowIDVarint};
use acacia_bot::interact::Face;
use acacia_bot::{Bot, BotConfig};
use acacia_physics::BlockPos;
use acacia_testserver::{FakeServer, Script};
use tokio::time::sleep;

/// Floor half-width around the spawn cell.
const FLOOR: i32 = 10;
/// How late the fake server echoes the cell: the order of magnitude relay saw live (1.3 s echo, 13 s pickup).
const ECHO_DELAY: Duration = Duration::from_secs(3);

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
        let _ = tokio::time::timeout(Duration::from_millis(50), bot.next()).await;
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

/// Lays a stone floor under the spawn and waits until the bot stands on it; returns the feet cell.
async fn stand_on_floor(server: &FakeServer, bot: &mut Bot) -> BlockPos {
    let p = &bot.state().player.position;
    let feet = [p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32];
    let floor: Vec<BlockPos> =
        (-FLOOR..=FLOOR).flat_map(|dx| (-FLOOR..=FLOOR).map(move |dz| [feet[0] + dx, feet[1] - 1, feet[2] + dz])).collect();
    let loaded = |bot: &Bot| bot.world().and_then(|w| w.view()).is_some_and(|v| floor.iter().all(|c| v.chunk(c[0] >> 4, c[2] >> 4).is_some()));
    until(bot, 20, "the spawn chunks", loaded).await;
    set_blocks(server, &floor, wire(bot, "minecraft:stone"));
    let standing = |bot: &Bot| bot.movement().is_some_and(|m| m.is_started() && m.on_ground());
    until(bot, 20, "the bot to stand on the floor", standing).await;
    let f = bot.movement().and_then(|m| m.position()).expect("movement started");
    [f[0].floor() as i32, (f[1] + 0.01).floor() as i32, f[2].floor() as i32]
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

/// A block beside the feet, in reach: what the bot digs.
async fn wall(server: &FakeServer, bot: &mut Bot, feet: BlockPos, name: &str) -> (BlockPos, u32) {
    let cell = [feet[0] + 1, feet[1], feet[2]];
    let wire_id = wire(bot, name);
    set_blocks(server, &[cell], wire_id);
    until(bot, 5, "the wall block", |bot| block_wire(bot, cell) == Some(wire_id)).await;
    (cell, wire_id)
}

/// The break's block actions ride in `PlayerAuthInput` with the `BlockAction` input flag: without the flag the
/// server never sees them (relay run123: zero `PlayerAction` packets, so `BlockActions` is the only carrier).
fn break_actions_have_the_flag(server: &FakeServer, cell: BlockPos) -> bool {
    let mut inputs = server.received().into_iter().filter_map(|r| r.packet.decode::<PlayerAuthInput>().ok());
    inputs.any(|i| {
        let ours = i.block_action.as_ref().is_some_and(|a| a.iter().any(|a| (a.position.x, a.position.y, a.position.z) == (cell[0], cell[1], cell[2])));
        ours && i.input_data.contains(&InputData::BlockAction)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_break_whose_cell_update_is_3s_late_still_succeeds() {
    let (server, mut bot) = join("SlowDigTester").await;
    let feet = stand_on_floor(&server, &mut bot).await;
    let (cell, dirt) = wall(&server, &mut bot, feet, "minecraft:dirt").await;
    let air = wire(&bot, "minecraft:air");

    let started = Instant::now();
    let (result, ()) = tokio::join!(bot.break_block(cell), async {
        sleep(ECHO_DELAY).await;
        set_blocks(&server, &[cell], air);
    });
    let took = started.elapsed();
    assert!(result.is_ok(), "a break the server carried out 3 s late must not be refused: {result:?}");
    assert!(took >= ECHO_DELAY, "the break settled before the echo: {took:?}");
    assert_ne!(block_wire(&bot, cell), Some(dirt), "the cell is air in the bot's view");
    assert!(break_actions_have_the_flag(&server, cell), "StartBreak/ContinueBreak must ride PlayerAuthInput with InputData::BlockAction");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placement_whose_cell_update_is_3s_late_still_succeeds() {
    let (server, mut bot) = join("SlowPlaceTester").await;
    let feet = stand_on_floor(&server, &mut bot).await;
    let dirt_id = bot.state().items.id("minecraft:dirt").expect("dirt in the item registry") as i16;
    inventory(&server, vec![item(dirt_id, 16)]);
    until(&mut bot, 5, "the dirt in the hotbar", |bot| !bot.state().inventory.held().is_empty()).await;
    let (target, wire_id) = wall(&server, &mut bot, feet, "minecraft:dirt").await;
    let air = wire(&bot, "minecraft:air");
    // Clear the cell first: the placement fills it.
    set_blocks(&server, &[target], air);
    until(&mut bot, 5, "the empty cell", |bot| block_wire(&bot, target) == Some(air)).await;
    let against = [target[0], target[1] - 1, target[2]];

    let started = Instant::now();
    let (result, ()) = tokio::join!(bot.place_block(against, Face::Up), async {
        sleep(ECHO_DELAY).await;
        set_blocks(&server, &[target], wire_id);
    });
    let took = started.elapsed();
    assert!(result.is_ok(), "a placement the server carried out 3 s late must not be refused: {result:?}");
    assert!(took >= ECHO_DELAY, "the placement settled before the echo: {took:?}");
    assert_eq!(block_wire(&bot, target), Some(wire_id), "the cell shows the placed block");
}

/// A block beside and one level *below* the feet: its top face is the only exposed one (the side facing the bot
/// is the floor it stands on). This is the "dig into the ground next to you" case the real server refused with
/// `not_visible: is behind other blocks` before the face-probe visibility (t16).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_block_beside_and_below_the_feet_is_diggable() {
    let (server, mut bot) = join("SideDigTester").await;
    let feet = stand_on_floor(&server, &mut bot).await;
    let target = [feet[0] + 1, feet[1] - 1, feet[2]];
    let dirt = wire(&bot, "minecraft:dirt");
    let air = wire(&bot, "minecraft:air");
    set_blocks(&server, &[target], dirt);
    until(&mut bot, 5, "the dirt beside the feet", |bot| block_wire(&bot, target) == Some(dirt)).await;

    let (result, ()) = tokio::join!(bot.break_block(target), async {
        sleep(Duration::from_millis(300)).await;
        set_blocks(&server, &[target], air);
    });
    assert!(result.is_ok(), "a block one level below and beside the feet must be visible and breakable: {result:?}");
    assert_eq!(block_wire(&bot, target), Some(air), "the cell is air afterwards");
}

/// Digging the block the bot stands on drops it one level: the break waits for the landing, and the very next
/// dig (the cell under the new feet) works from solid ground instead of mid-fall.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn digging_the_block_under_the_feet_lands_and_the_shaft_continues() {
    let (server, mut bot) = join("ShaftTester").await;
    let feet = stand_on_floor(&server, &mut bot).await;
    let dirt = wire(&bot, "minecraft:dirt");
    let air = wire(&bot, "minecraft:air");
    let shaft: Vec<BlockPos> = (1..=3).map(|d| [feet[0], feet[1] - d, feet[2]]).collect();
    // The fake world below the laid floor is air: a plate at the bottom of the shaft catches the bot, as the
    // real terrain would.
    let bottom = [feet[0], feet[1] - 4, feet[2]];
    set_blocks(&server, &[bottom], wire(&bot, "minecraft:stone"));
    set_blocks(&server, &shaft, dirt);
    until(&mut bot, 5, "the dirt under the feet", |bot| block_wire(&bot, shaft[0]) == Some(dirt)).await;

    for (i, cell) in shaft.iter().copied().enumerate() {
        let (result, ()) = tokio::join!(bot.break_block(cell), async {
            sleep(Duration::from_millis(300)).await;
            set_blocks(&server, &[cell], air);
        });
        assert!(result.is_ok(), "shaft dig {} at {cell:?}: {result:?}", i + 1);
        let at = bot.movement().and_then(|m| m.position()).expect("movement started");
        let landed = feet[1] - 1 - i as i32;
        assert_eq!(at[1].floor() as i32, landed, "after dig {} the bot fell to y={landed} (position {at:?})", i + 1);
        assert!(bot.movement().is_some_and(|m| m.on_ground()), "after dig {} the bot stands on the shaft floor", i + 1);
    }
}

/// The t19 hole: the bot digs itself into a 1x1 shaft and cannot walk out. [`Bot::climb_out`] takes the vanilla
/// staircase — the wall's cell above the pit is open, so one jump from the pit floor lands on the wall.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bot_in_a_one_block_pit_climbs_out() {
    let (server, mut bot) = join("PitTester").await;
    let _ = stand_on_floor(&server, &mut bot).await;
    let feet = bot.block_position().expect("movement started");
    let dirt = wire(&bot, "minecraft:dirt");
    let walls: Vec<BlockPos> = [(1, 0), (-1, 0), (0, 1), (0, -1)].iter().map(|&(dx, dz)| [feet[0] + dx, feet[1], feet[2] + dz]).collect();
    set_blocks(&server, &walls, dirt);
    until(&mut bot, 5, "the pit walls", |bot| walls.iter().all(|&w| block_wire(bot, w) == Some(dirt))).await;
    assert!(bot.in_hole(), "four walls at feet level make a pit the bot cannot walk out of");

    let steps = bot.climb_out(4).await.expect("climbing out of a one block pit");
    let now = bot.block_position().expect("movement started");
    assert!(steps >= 1, "climbing out took at least one staircase step");
    assert!(now[1] > feet[1], "the bot rose: {feet:?} -> {now:?}");
    assert!(!bot.in_hole(), "and is out of the pit it could not walk out of");
}
