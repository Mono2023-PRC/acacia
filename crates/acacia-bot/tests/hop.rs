//! t87: a hop must carry the bot sideways even when a dead claim is released on its first tick
//! (contract: `acacia_bot::movement` / `Bot::hop_toward`).
//!
//! run224 caught the accident in the wild: `jump=1` was sent, the move vector was `(0,0)`, the feet never moved.
//! A `move_to` had timed out, its dropped future left a `Held::Processes` claim behind, and the claim was released
//! on the first tick of the healing hop — `release_orphans() → release_keys() → Controls::stop()` cleared `forward`
//! after `hop_toward` had written it once.
//!
//! What these tests guard: the hop carries the bot sideways after a `goto` was dropped mid-flight, it reports where
//! it was aimed (`target`/`y_ground`), and it leaves no key held. They do **not** reproduce the ordering itself —
//! the fake server never leaves a claim in `bot.cancel.residue` the way a dropped `goto_with` does in the live
//! client, so with both fixes reverted they still pass. The reproduction is the unit test
//! `movement::jump::tests::a_steered_jump_keeps_walking_after_the_keys_are_cleared`, which clears the keys between
//! presses exactly as `Controls::stop()` does.

use std::time::{Duration, Instant};

use acacia_bot::client::{Client, TransportKind};
use acacia_bot::pathfind::Goal;
use acacia_bot::proto::packets::UpdateSubchunkBlocks;
use acacia_bot::proto::types::{BlockCoordinates, BlockUpdate, BlockUpdateTransitionType};
use acacia_bot::{Bot, BotConfig};
use acacia_physics::BlockPos;
use acacia_testserver::{FakeServer, Script};
use tokio::time::timeout;

/// Floor half-width around the spawn cell.
const FLOOR: i32 = 10;

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

/// Lays a stone floor under the spawn and waits until the bot stands on it; returns the feet cell.
///
/// The floor goes at both candidate heights — the spawn packet's and the simulated body's — because the two can
/// disagree for the first few ticks and a floor at the wrong height means a bot in free fall.
async fn stand_on_floor(server: &FakeServer, bot: &mut Bot) -> BlockPos {
    let p = &bot.state().player.position;
    let feet = [p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32];
    let sim = bot.movement().and_then(|m| m.position()).map(|q| [q[0].floor() as i32, q[1].floor() as i32, q[2].floor() as i32]);
    let mut floor: Vec<BlockPos> = Vec::new();
    for base in std::iter::once(feet).chain(sim.filter(|s| *s != feet)) {
        floor.extend((-FLOOR..=FLOOR).flat_map(|dx| (-FLOOR..=FLOOR).map(move |dz| [base[0] + dx, base[1] - 1, base[2] + dz])));
    }
    let loaded = |bot: &Bot| bot.world().and_then(|w| w.view()).is_some_and(|v| floor.iter().all(|c| v.chunk(c[0] >> 4, c[2] >> 4).is_some()));
    until(bot, 20, "the spawn chunks", loaded).await;
    let stone = wire(bot, "minecraft:stone");
    for cell in &floor {
        // One update per chunk column keeps the packet small enough for the fake server's budget.
        set_blocks(server, &[*cell], stone);
    }
    let standing = |bot: &Bot| bot.movement().is_some_and(|m| m.is_started() && m.on_ground());
    until(bot, 20, "the bot to stand on the floor", standing).await;
    let f = bot.movement().and_then(|m| m.position()).expect("movement started");
    [f[0].floor() as i32, (f[1] + 0.01).floor() as i32, f[2].floor() as i32]
}

fn horizontal(p: [f32; 3]) -> [f32; 2] {
    [p[0], p[2]]
}

/// The bug t87 fixes, end to end: drop a `goto` mid-flight (as `walk_supervised`'s timeout does), then hop onto
/// the block beside the feet. Before the fix the jump key went out and the walk key did not — the feet never moved.
#[tokio::test]
async fn a_hop_still_walks_after_a_dropped_goto_leaves_a_claim_behind() {
    let (server, mut bot) = join("hop-claim").await;
    let feet = stand_on_floor(&server, &mut bot).await;
    // The block the hop aims at: one step beside the feet, one block up.
    let target: BlockPos = [feet[0] + 1, feet[1] + 1, feet[2]];
    set_blocks(&server, &[[target[0], target[1] - 1, target[2]]], wire(&bot, "minecraft:stone"));

    // A goto that is still running when its future is dropped: its claim stays registered (that is the accident).
    let walking = bot.movement().and_then(|m| m.position()).expect("position");
    let dropped = timeout(Duration::from_millis(1500), bot.goto(Goal::XZ(feet[0] + 6, feet[2]))).await;
    assert!(dropped.is_err(), "the goto should still have been walking when it was dropped");
    let walked = bot.movement().and_then(|m| m.position()).expect("position");
    assert!(((walked[0] - walking[0]).powi(2) + (walked[2] - walking[2]).powi(2)).sqrt() > 0.1, "the goto never started walking, so it never claimed the processes");

    let before = bot.movement().and_then(|m| m.position()).expect("position");
    let jump = bot.hop_toward(target).await.expect("the hop runs");
    let after = bot.movement().and_then(|m| m.position()).expect("position");

    assert!(jump.sent, "no jump was sent: {jump:?}");
    assert_eq!(jump.target, Some(target), "the hop must say where it was aimed: {jump:?}");
    assert_eq!(jump.y_ground, Some(feet[1] as f32), "{jump:?}");
    let [bx, bz] = horizontal(before);
    let [ax, az] = horizontal(after);
    let moved = ((ax - bx).powi(2) + (az - bz).powi(2)).sqrt();
    assert!(moved > 0.2, "the hop never moved horizontally ({bx:.2},{bz:.2}) → ({ax:.2},{az:.2}): {jump:?}");
    // And the keys are not left held once the hop is over.
    let controls = bot.controls().copied().expect("physics controls");
    assert!(!controls.jump && controls.forward == 0.0, "keys left down after the hop: {controls:?}");
}

/// A hop in place must not walk (the fix must not turn every jump into a sprint).
#[tokio::test]
async fn a_jump_in_place_leaves_the_controls_alone() {
    let (server, mut bot) = join("hop-place").await;
    let _feet = stand_on_floor(&server, &mut bot).await;
    let before = bot.movement().and_then(|m| m.position()).expect("position");
    let jump = bot.jump().await.expect("the jump runs");
    let after = bot.movement().and_then(|m| m.position()).expect("position");
    assert!(jump.sent, "{jump:?}");
    assert_eq!(jump.target, None);
    let [bx, bz] = horizontal(before);
    let [ax, az] = horizontal(after);
    assert!(((ax - bx).powi(2) + (az - bz).powi(2)).sqrt() < 0.2, "a jump in place walked: ({bx:.2},{bz:.2}) → ({ax:.2},{az:.2})");
    let controls = bot.controls().copied().expect("physics controls");
    assert!(!controls.jump && controls.forward == 0.0, "keys left down after the jump: {controls:?}");
}
