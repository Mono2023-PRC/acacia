//! The bot owner task.
//!
//! [`acacia_bot::Bot`] owns its game state and drives its connection tasks itself, so exactly one
//! task may touch it: this one. Everything else either reads the shared [`Snapshot`] (that is how
//! `netease_status` stays fast while a long action runs) or sends a [`Command`] down the channel.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use acacia_bot::client::Client;
use acacia_bot::pathfind::{Goal, GotoOpts, NavStatus, Navigator};
use acacia_bot::items::SlotRef;
use acacia_bot::state::{GameState, ItemStack, Trackers};
use acacia_bot::workstation::CraftMode;
use acacia_bot::ActionError;
use acacia_bot::interact::Face;
use acacia_bot::{Bot, BotConfig, BotEvent, Events};
use acacia_world::BlockAccess;
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

/// Ticks let through after the spawn so the first path search sees the ground around the bot.
const SETTLE_TICKS: u32 = 10;

/// Everything `netease_status` answers from. The owner task refreshes it; readers never touch the bot.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub connected: bool,
    pub name: Option<String>,
    pub address: Option<String>,
    /// Feet position.
    pub position: Option<[f32; 3]>,
    /// Feet block position (`Bot::block_position`).
    pub block_position: Option<[i32; 3]>,
    /// 0 overworld, 1 nether, 2 end.
    pub dimension: Option<i32>,
    pub health: Option<f32>,
    /// Hunger.
    pub food: Option<f32>,
    pub saturation: Option<f32>,
    pub game_mode: Option<String>,
    /// View direction in degrees (0 faces +Z, -90 looks up).
    pub yaw: Option<f32>,
    pub pitch: Option<f32>,
    /// The tool currently running on the owner task, if any.
    pub busy: Option<String>,
    /// Why the last connection attempt or session ended.
    pub last_error: Option<String>,
}

/// The snapshot shared by the owner task and the request handlers.
pub type Shared = Arc<Mutex<Snapshot>>;

/// A command for the owner task; every variant carries its own reply.
pub enum Command {
    Connect {
        address: String,
        name: String,
        timeout_s: u64,
        reply: Reply,
    },
    MoveTo {
        x: i32,
        y: i32,
        z: i32,
        exact: bool,
        timeout_s: u64,
        allow_dig: bool,
        allow_bridge: bool,
        reply: Reply,
    },
    Dig {
        pos: [i32; 3],
        reply: Reply,
    },
    Place {
        against: [i32; 3],
        face: Face,
        reply: Reply,
    },
    Chat {
        text: String,
        reply: Reply,
    },
    Look {
        yaw: f32,
        pitch: f32,
        reply: Reply,
    },
    Observe {
        radius: u8,
        reply: Reply,
    },
    Craft {
        item: String,
        count: u32,
        table: Option<[i32; 3]>,
        /// 用配方书（默认）还是手工摆格子。
        grid: bool,
        reply: Reply,
    },
    Find {
        block: String,
        radius: u8,
        limit: u8,
        reply: Reply,
    },
    Disconnect {
        reply: Reply,
    },
    Hold {
        item: String,
        reply: Reply,
    },
    Block {
        pos: [i32; 3],
        reply: Reply,
    },
    Recipes {
        item: String,
        limit: usize,
        reply: Reply,
    },
    Shutdown {
        reply: Reply,
    },
}

/// Where a command's outcome goes back to the request handler.
pub type Reply = oneshot::Sender<Result<Value, String>>;

impl Command {
    /// The reply channel, for commands that cannot run because the connection is gone.
    fn into_reply(self) -> Reply {
        match self {
            Command::Connect { reply, .. }
            | Command::MoveTo { reply, .. }
            | Command::Dig { reply, .. }
            | Command::Place { reply, .. }
            | Command::Chat { reply, .. }
            | Command::Look { reply, .. }
            | Command::Observe { reply, .. }
            | Command::Craft { reply, .. }
            | Command::Find { reply, .. }
            | Command::Recipes { reply, .. }
            | Command::Hold { reply, .. }
            | Command::Block { reply, .. }
            | Command::Disconnect { reply }
            | Command::Shutdown { reply } => reply,
        }
    }
}

/// A fresh, empty snapshot.
pub fn shared() -> Shared {
    Arc::new(Mutex::new(Snapshot::default()))
}

/// A copy of the snapshot, for `netease_status`.
pub fn read(shared: &Shared) -> Snapshot {
    lock(shared).clone()
}

/// The JSON view of a snapshot: the payload of `netease_status` and of a successful connect.
pub fn snapshot_json(snapshot: &Snapshot) -> Value {
    if !snapshot.connected {
        return json!({"connected": false, "busy": snapshot.busy, "last_error": snapshot.last_error});
    }
    json!({
        "connected": true,
        "name": snapshot.name,
        "address": snapshot.address,
        "position": position_json(snapshot.position),
        "block_position": snapshot.block_position,
        "dimension": snapshot.dimension.map(dimension_name),
        "dimension_id": snapshot.dimension,
        "health": snapshot.health,
        "food": snapshot.food,
        "saturation": snapshot.saturation,
        "game_mode": snapshot.game_mode,
        "yaw": snapshot.yaw,
        "pitch": snapshot.pitch,
        "busy": snapshot.busy,
    })
}

/// Runs until the command channel closes or a shutdown arrives; the bot is disconnected on the way out.
pub async fn run(mut rx: mpsc::Receiver<Command>, snap: Shared) {
    let mut bot: Option<Bot> = None;
    loop {
        // Both slots are filled from the select arms only: the arms must not touch `bot`, whose
        // future the select is polling.
        let mut command: Option<Option<Command>> = None;
        let mut event: Option<Option<BotEvent>> = None;
        match bot.as_mut() {
            Some(b) => {
                tokio::select! {
                    incoming = rx.recv() => command = Some(incoming),
                    next = b.next() => event = Some(next),
                }
            }
            None => command = Some(rx.recv().await),
        }
        let mut lost = false;
        if let Some(event) = event {
            match event {
                Some(BotEvent::Disconnected(reason)) => {
                    eprintln!("[acacia-netease] connection dropped: {reason:?}");
                    lost = true;
                }
                Some(_) => {
                    if let Some(b) = bot.as_mut() {
                        refresh(b, &snap, None);
                    }
                }
                None => lost = true,
            }
        }
        if lost {
            bot = None;
            fail_queued(&mut rx, "not connected: the connection dropped");
            mark_disconnected(&snap, Some("connection dropped".to_owned()));
            continue;
        }
        match command {
            Some(None) => break,
            Some(Some(command)) => {
                if handle(&mut bot, command, &snap).await {
                    break;
                }
            }
            None => {}
        }
    }
    if let Some(mut bot) = bot.take() {
        bot.disconnect().await;
    }
    mark_disconnected(&snap, None);
}

/// Handles one command; `true` ends the owner task.
async fn handle(bot: &mut Option<Bot>, command: Command, snap: &Shared) -> bool {
    match command {
        Command::Connect { address, name, timeout_s, reply } => {
            let _ = reply.send(connect(bot, snap, address, name, timeout_s).await);
        }
        Command::MoveTo { x, y, z, exact, timeout_s, allow_dig, allow_bridge, reply } => {
            let _ = reply.send(move_to(bot, snap, x, y, z, exact, timeout_s, allow_dig, allow_bridge).await);
        }
        Command::Dig { pos, reply } => {
            let _ = reply.send(dig(bot, snap, pos).await);
        }
        Command::Place { against, face, reply } => {
            let _ = reply.send(place(bot, snap, against, face).await);
        }
        Command::Chat { text, reply } => {
            let _ = reply.send(chat(bot, text).await);
        }
        Command::Look { yaw, pitch, reply } => {
            let _ = reply.send(look(bot, snap, yaw, pitch).await);
        }
        Command::Observe { radius, reply } => {
            let _ = reply.send(observe(bot, radius));
        }
        Command::Craft { item, count, table, grid, reply } => {
            let _ = reply.send(craft(bot, snap, item, count, table, grid).await);
        }
        Command::Find { block, radius, limit, reply } => {
            let _ = reply.send(find(bot, block, radius, limit));
        }
        Command::Recipes { item, limit, reply } => {
            let _ = reply.send(recipes(bot, &item, limit));
        }
        Command::Hold { item, reply } => {
            let _ = reply.send(hold(bot, snap, item).await);
        }
        Command::Block { pos, reply } => {
            let _ = reply.send(block_at(bot, pos));
        }
        Command::Disconnect { reply } => {
            let _ = reply.send(disconnect(bot, snap).await);
        }
        Command::Shutdown { reply } => {
            if let Some(mut bot) = bot.take() {
                bot.disconnect().await;
            }
            mark_disconnected(snap, None);
            let _ = reply.send(Ok(json!({"shutdown": true})));
            return true;
        }
    }
    // A command that ran into a closed connection leaves no usable bot behind.
    let ended = bot.as_ref().and_then(|b| b.disconnect_reason().map(|reason| format!("connection ended: {reason:?}")));
    if let Some(reason) = ended {
        eprintln!("[acacia-netease] {reason}");
        bot.take();
        mark_disconnected(snap, Some(reason));
    }
    false
}

// ---------------------------------------------------------------- failures

/// 把 acacia 的动作错误翻成 LLM 能据此改方案的短标签，再配一句人话。
fn fail_reason(error: &ActionError) -> &'static str {
    match error {
        ActionError::Timeout => "timeout",
        ActionError::Disconnected => "disconnected",
        ActionError::Rejected(_) => "rejected",
        ActionError::NotVisible(_) => "not_visible",
        ActionError::Cancelled => "cancelled",
        ActionError::NotPossible(message) => classify_not_possible(message),
        _ => "refused",
    }
}

fn classify_not_possible(message: &str) -> &'static str {
    if message.starts_with("nothing to break") {
        "no_block"
    } else if message.contains("nothing held") {
        "no_item"
    } else if message.contains("blocks away") {
        "out_of_reach"
    } else if message.starts_with("no recipe for") {
        "no_recipe"
    } else if message.starts_with("missing ingredient") {
        "missing_ingredient"
    } else if message.contains("no room") {
        "no_space"
    } else if message.contains("physics bot") {
        "no_physics"
    } else if message.contains("game mode") {
        "game_mode"
    } else if message.starts_with("no aim") {
        "not_visible"
    } else {
        "refused"
    }
}

// ---------------------------------------------------------------- observe

/// 上一次 observe 的扫描结果，用来算“这段时间世界变了什么”。
#[derive(Default)]
struct ObserveMemo {
    tick: u64,
    blocks: HashMap<[i32; 3], u32>,
}

static OBSERVE_MEMO: LazyLock<Mutex<ObserveMemo>> = LazyLock::new(|| Mutex::new(ObserveMemo::default()));

/// 值得对 LLM 点名的方块：木材、矿石、液体、常见工作站。
fn is_interesting_block(name: &str) -> bool {
    name.ends_with("_log")
        || name.ends_with("_wood")
        || name.ends_with("_stem")
        || name.ends_with("_hyphae")
        || name.contains("_ore")
        || name.ends_with("_bed")
        || name.ends_with("_door")
        || matches!(
            name,
            "minecraft:water"
                | "minecraft:lava"
                | "minecraft:crafting_table"
                | "minecraft:furnace"
                | "minecraft:chest"
                | "minecraft:barrel"
                | "minecraft:torch"
                | "minecraft:anvil"
        )
}

/// 会打人的生物：只按名字粗判，够 LLM 决定“要不要躲”。
fn is_hostile(kind: &str) -> bool {
    [
        "zombie", "skeleton", "creeper", "spider", "witch", "pillager", "vindicator", "evoker",
        "enderman", "slime", "drowned", "husk", "stray", "phantom", "blaze", "piglin", "ravager",
    ]
    .iter()
    .any(|needle| kind.contains(needle))
}

/// 世界坐标里的八向方位：-Z 是北，+X 是东。
fn bearing(dx: f32, dz: f32) -> &'static str {
    let deg = dx.atan2(-dz).to_degrees().rem_euclid(360.0);
    match deg {
        d if d < 22.5 || d >= 337.5 => "N",
        d if d < 67.5 => "NE",
        d if d < 112.5 => "E",
        d if d < 157.5 => "SE",
        d if d < 202.5 => "S",
        d if d < 247.5 => "SW",
        d if d < 292.5 => "W",
        _ => "NW",
    }
}

fn round1(value: f32) -> f64 {
    (value as f64 * 10.0).round() / 10.0
}

fn round2(value: f32) -> f64 {
    (value as f64 * 100.0).round() / 100.0
}

fn item_json(state: &GameState, stack: &ItemStack) -> Value {
    if stack.is_empty() {
        return Value::Null;
    }
    json!({
        "item": state.items.name(stack.network_id).unwrap_or("unknown"),
        "count": stack.count,
    })
}

/// 把 bot 的状态压成“一个人一眼能看懂的场景”：自身、背包、附近的生物与资源、以及自上次以来的变化。
fn observe(bot: &mut Option<Bot>, radius: u8) -> Result<Value, String> {
    let Some(bot) = bot.as_mut() else {
        return Err("not connected: run netease_connect first".to_owned());
    };
    // 视线在 controls 里（PlayerAuthInput 发的就是它）；没有物理时退回服务端给的值。
    let view_deg = match bot.controls() {
        Some(controls) => (controls.yaw, controls.pitch),
        None => {
            let player = &bot.state().player;
            (player.yaw, player.pitch)
        }
    };
    let simulated = bot.movement().and_then(|movement| movement.position());
    let on_ground = bot.movement().is_some_and(|movement| movement.on_ground());
    let radius = radius.clamp(2, 24) as i32;

    let state = bot.state();
    let player = &state.player;
    let feet = simulated.unwrap_or([player.position.x, player.position.y, player.position.z]);
    let (px, py, pz) = (feet[0].floor() as i32, feet[1].floor() as i32, feet[2].floor() as i32);

    // 背包：按物品名汇总（名字来自服务器的物品调色板，不让模型猜）。
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for (_window, _slot, stack) in state.inventory.iter() {
        if stack.is_empty() {
            continue;
        }
        let name = state.items.name(stack.network_id).unwrap_or("unknown").to_owned();
        *counts.entry(name).or_insert(0) += stack.count as u32;
    }

    // 生物：只报附近 2.5 倍半径内的，按距离排序。
    let mut entities: Vec<Value> = Vec::new();
    for entity in state.entities.iter() {
        if entity.runtime_id == player.runtime_entity_id {
            continue;
        }
        let (dx, dy, dz) = (
            entity.position.x - feet[0],
            entity.position.y - feet[1],
            entity.position.z - feet[2],
        );
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        if dist > radius as f32 * 2.5 {
            continue;
        }
        entities.push(json!({
            "kind": entity.kind,
            "name": entity.username,
            "dist": round1(dist),
            "dir": bearing(dx, dz),
            "dy": round1(dy),
            "hostile": is_hostile(&entity.kind),
        }));
    }
    entities.sort_by(|a, b| {
        a["dist"].as_f64().unwrap_or(f64::MAX).total_cmp(&b["dist"].as_f64().unwrap_or(f64::MAX))
    });
    entities.truncate(12);

    // 附近方块：扫一个盒子，把“值得关心的”按类型聚合（最近位置 + 数量），顺带记录全量用于差分。
    let mut resources: HashMap<&'static str, (f64, [i32; 3], u32)> = HashMap::new();
    let mut current: HashMap<[i32; 3], u32> = HashMap::new();
    let mut standing_on: Option<&'static str> = None;
    let mut solid_cells = 0usize;
    let mut scanned = false;
    if let Some(world) = bot.world() {
        if let (Some(view), Some(registry)) = (world.view(), world.registry()) {
            scanned = true;
            for x in (px - radius)..=(px + radius) {
                for z in (pz - radius)..=(pz + radius) {
                    for y in (py - 4)..=(py + 4) {
                        let id = view.block(x, y, z);
                        let Some(block) = registry.get(id) else { continue };
                        if block.name == "minecraft:air" {
                            continue;
                        }
                        solid_cells += 1;
                        current.insert([x, y, z], id);
                        if !is_interesting_block(block.name) {
                            continue;
                        }
                        let (dx, dz) = ((x - px) as f32, (z - pz) as f32);
                        let dist = ((dx * dx + dz * dz) as f64).sqrt();
                        let entry = resources.entry(block.name).or_insert((dist, [x, y, z], 0));
                        entry.2 += 1;
                        if dist < entry.0 {
                            *entry = (dist, [x, y, z], entry.2);
                        }
                    }
                }
            }
            if let Some(id) = current.get(&[px, py - 1, pz]) {
                standing_on = registry.get(*id).map(|block| block.name);
            }
        }
    }

    let mut resources_json: Vec<Value> = resources
        .iter()
        .map(|(name, (dist, pos, count))| {
            json!({
                "block": name,
                "nearest": pos,
                "dist": (*dist * 10.0).round() / 10.0,
                "dir": bearing((pos[0] - px) as f32, (pos[2] - pz) as f32),
                "count": count,
            })
        })
        .collect();
    resources_json.sort_by(|a, b| {
        a["dist"].as_f64().unwrap_or(f64::MAX).total_cmp(&b["dist"].as_f64().unwrap_or(f64::MAX))
    });
    resources_json.truncate(12);

    // 差分：只报本次扫描盒子里出现/消失的格子，第一次调用不报。
    let now = state.entities.now();
    let mut added: Vec<Value> = Vec::new();
    let mut removed: Vec<Value> = Vec::new();
    let since_tick;
    {
        let registry = bot.world().and_then(|world| world.registry().cloned());
        let mut memo = OBSERVE_MEMO.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        since_tick = memo.tick;
        if !memo.blocks.is_empty() {
            for (pos, id) in current.iter() {
                match memo.blocks.get(pos) {
                    Some(previous) if previous == id => {}
                    previous => {
                        let name = registry
                            .as_ref()
                            .and_then(|registry| registry.get(*id))
                            .map_or("unknown", |block| block.name);
                        let replaced = previous
                            .and_then(|id| registry.as_ref().and_then(|registry| registry.get(*id)))
                            .map(|block| block.name);
                        added.push(json!({"block": name, "pos": pos, "replaced": replaced}));
                    }
                }
            }
            for (pos, id) in memo.blocks.iter() {
                if (px - radius..=px + radius).contains(&pos[0])
                    && (py - 4..=py + 4).contains(&pos[1])
                    && (pz - radius..=pz + radius).contains(&pos[2])
                    && !current.contains_key(pos)
                {
                    let name = registry
                        .as_ref()
                        .and_then(|registry| registry.get(*id))
                        .map_or("unknown", |block| block.name);
                    removed.push(json!({"block": name, "pos": pos}));
                }
            }
        }
        memo.blocks = current;
        memo.tick = now;
    }
    added.truncate(32);
    removed.truncate(32);

    Ok(json!({
        "tick": now,
        "self": {
            "position": {"x": round2(feet[0]), "y": round2(feet[1]), "z": round2(feet[2])},
            "block_position": [px, py, pz],
            "yaw": round1(view_deg.0),
            "pitch": round1(view_deg.1),
            "on_ground": on_ground,
            "physics": simulated.is_some(),
            "health": player.health,
            "food": player.hunger,
            "saturation": player.saturation,
            "dimension": player.dimension,
            "game_mode": format!("{:?}", player.game_mode).to_lowercase(),
            "standing_on": standing_on,
        },
        "held": item_json(state, state.inventory.held()),
        "inventory": counts
            .iter()
            .map(|(name, count)| json!({"item": name, "count": count}))
            .collect::<Vec<_>>(),
        "entities": entities,
        "resources": resources_json,
        "delta": {"since_tick": since_tick, "added": added, "removed": removed},
        "scan": {"radius": radius, "y": [py - 4, py + 4], "solid_cells": solid_cells, "available": scanned},
    }))
}

/// Dials the proxy, joins offline as `name`, and waits until the physics simulation runs.
async fn connect(bot: &mut Option<Bot>, snap: &Shared, address: String, name: String, timeout_s: u64) -> Result<Value, String> {
    if let Some(mut previous) = bot.take() {
        previous.disconnect().await;
    }
    mark_disconnected(snap, None);
    {
        let mut snapshot = lock(snap);
        snapshot.name = Some(name.clone());
        snapshot.address = Some(address.clone());
        snapshot.busy = Some("netease_connect".to_owned());
    }
    let timeout_s = timeout_s.max(1);
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    let result = async {
        // Shuttler has authentication disabled, so the bot joins offline.
        let builder = Client::builder(address.clone()).offline(name).login_timeout(Duration::from_secs(timeout_s));
        let config = BotConfig {
            physics: true,
            trackers: Trackers { entities: true, ..Trackers::default() },
            // Health/food changes refresh the snapshot between ticks.
            events: Events::HEALTH,
            ..BotConfig::default()
        };
        let mut joined = tokio::time::timeout_at(deadline, Bot::connect(builder, config))
            .await
            .map_err(|_| format!("timed out after {timeout_s}s dialling {address}"))?
            .map_err(|e| format!("could not join {address}: {e}"))?;
        // Leaving a half-spawned session behind would keep the server slot until it times out.
        if let Err(e) = wait_for_spawn(&mut joined, deadline, timeout_s, &address).await {
            joined.disconnect().await;
            return Err(e);
        }
        Ok::<Bot, String>(joined)
    }
    .await;
    match result {
        Ok(connected) => {
            *bot = Some(connected);
            let snapshot = match bot.as_mut() {
                Some(b) => refresh(b, snap, None),
                None => return Err("internal error: the bot vanished after connecting".to_owned()),
            };
            Ok(snapshot_json(&snapshot))
        }
        Err(message) => {
            mark_disconnected(snap, Some(message.clone()));
            Err(message)
        }
    }
}

/// Waits until the physics simulation runs (the spawn is done and `block_position` exists), then
/// lets [`SETTLE_TICKS`] pass so the first path search sees the ground.
async fn wait_for_spawn(joined: &mut Bot, deadline: Instant, timeout_s: u64, address: &str) -> Result<(), String> {
    while !(joined.movement().is_some_and(|m| m.is_started()) && joined.block_position().is_some()) {
        if Instant::now() >= deadline {
            return Err(format!("joined {address} but did not spawn within {timeout_s}s"));
        }
        joined.wait_ticks(1).await.map_err(|e| format!("waiting for spawn failed: {e}"))?;
    }
    for _ in 0..SETTLE_TICKS {
        if Instant::now() >= deadline || joined.wait_ticks(1).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Ticks a [`Navigator`] until it arrives, the deadline passes, or navigation fails.
#[allow(clippy::too_many_arguments)]
async fn move_to(
    bot: &mut Option<Bot>,
    snap: &Shared,
    x: i32,
    y: i32,
    z: i32,
    exact: bool,
    timeout_s: u64,
    allow_dig: bool,
    allow_bridge: bool,
) -> Result<Value, String> {
    let Some(bot) = bot.as_mut() else {
        return Err("not connected: run netease_connect first".to_owned());
    };
    let goal = if exact { Goal::Block([x, y, z]) } else { Goal::XZ(x, z) };
    let mut opts = GotoOpts::default();
    opts.search.allow_dig = allow_dig;
    opts.search.allow_bridge = allow_bridge;
    let mut navigator = Navigator::new(goal, opts);
    set_busy(snap, Some("netease_move_to"));
    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_s.max(1));
    let mut arrived = false;
    let detail;
    loop {
        let status = navigator.tick(bot).await;
        // The snapshot is what `netease_status` reads while this command holds the owner task.
        refresh(bot, snap, Some("netease_move_to"));
        match status {
            Ok(NavStatus::Arrived) => {
                arrived = true;
                detail = format!("arrived at [{x}, {y}, {z}]");
                break;
            }
            Ok(status) => {
                if Instant::now() >= deadline {
                    detail = format!("timed out after {}s while {status:?}", timeout_s.max(1));
                    break;
                }
            }
            Err(e) => {
                detail = format!("navigation failed: {e}");
                break;
            }
        }
    }
    let elapsed_s = started.elapsed().as_secs_f32();
    let snapshot = refresh(bot, snap, None);
    let reason = if arrived {
        Value::Null
    } else if detail.starts_with("timed out") {
        json!("timeout")
    } else if detail.starts_with("navigation failed") {
        json!("no_path")
    } else {
        json!("failed")
    };
    Ok(json!({
        "ok": arrived,
        "reason": reason,
        "arrived": arrived,
        "elapsed_s": elapsed_s,
        "position": position_json(snapshot.position),
        "block_position": snapshot.block_position,
        "detail": detail,
    }))
}

/// 名字匹配：`minecraft:birch_log` 精确匹配，`*_log` 按后缀，`minecraft:*` 按前缀。
fn block_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_prefix('*') {
        Some(suffix) => match suffix.strip_suffix('*') {
            Some(inner) => name.contains(inner),
            None => name.ends_with(suffix),
        },
        None => match pattern.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == pattern,
        },
    }
}

/// 在半径内按名字找方块，按距离排序返回坐标。拾取掉落物、找矿、找树都靠它。
fn find(bot: &mut Option<Bot>, block: String, radius: u8, limit: u8) -> Result<Value, String> {
    let Some(bot) = bot.as_mut() else {
        return Err("not connected: run netease_connect first".to_owned());
    };
    let radius = radius.clamp(1, 64) as i32;
    let limit = limit.clamp(1, 64) as usize;
    let feet = bot
        .movement()
        .and_then(|movement| movement.position())
        .or_else(|| {
            let player = &bot.state().player;
            Some([player.position.x, player.position.y, player.position.z])
        })
        .unwrap_or([0.0; 3]);
    let (px, py, pz) = (feet[0].floor() as i32, feet[1].floor() as i32, feet[2].floor() as i32);

    let mut matches: Vec<(f64, &'static str, [i32; 3])> = Vec::new();
    let mut scanned = 0usize;
    if let Some(world) = bot.world() {
        if let (Some(view), Some(registry)) = (world.view(), world.registry()) {
            for x in (px - radius)..=(px + radius) {
                for z in (pz - radius)..=(pz + radius) {
                    for y in (py - 8)..=(py + 16) {
                        scanned += 1;
                        let Some(state) = registry.get(view.block(x, y, z)) else { continue };
                        if state.name == "minecraft:air" || !block_matches(&block, state.name) {
                            continue;
                        }
                        let (dx, dy, dz) = (
                            (x - px) as f64,
                            (y - py) as f64,
                            (z - pz) as f64,
                        );
                        matches.push(((dx * dx + dy * dy + dz * dz).sqrt(), state.name, [x, y, z]));
                    }
                }
            }
        }
    }
    matches.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total = matches.len();
    let results: Vec<Value> = matches
        .iter()
        .take(limit)
        .map(|(dist, name, pos)| {
            json!({
                "block": name,
                "pos": pos,
                "dist": (dist * 10.0).round() / 10.0,
                "dir": bearing((pos[0] - px) as f32, (pos[2] - pz) as f32),
            })
        })
        .collect();
    Ok(json!({
        "block": block,
        "radius": radius,
        "total": total,
        "matches": results,
        "scanned": scanned,
    }))
}

/// 列出配方书里「结果物品名包含 item」的配方，用来排查「服务器发了配方但客户端匹配不上」。
fn recipes(bot: &mut Option<Bot>, item: &str, limit: usize) -> Result<Value, String> {
    let Some(bot) = bot.as_mut() else {
        return Err("not connected: run netease_connect first".to_owned());
    };
    let state = bot.state();
    let book = state.recipes.book();
    let want = item.trim().to_ascii_lowercase();
    let mut out: Vec<Value> = Vec::new();
    let mut total = 0usize;
    for recipe in book.iter() {
        let outputs: Vec<String> = recipe
            .outputs
            .iter()
            .map(|stack| {
                state
                    .item_name(stack)
                    .map_or_else(|| format!("#{}", stack.network_id), str::to_owned)
            })
            .collect();
        if !outputs.iter().any(|name| name.to_ascii_lowercase().contains(&want)) {
            continue;
        }
        total += 1;
        if out.len() >= limit {
            continue;
        }
        let inputs: Vec<String> = recipe
            .inputs
            .iter()
            .map(|input| format!("{input:?}"))
            .collect();
        out.push(json!({
            "network_id": recipe.network_id,
            "id": recipe.id,
            "block": recipe.block,
            "kind": format!("{:?}", recipe.kind),
            "fits_2x2": recipe.fits_2x2(),
            "inputs": inputs,
            "outputs": outputs,
        }));
    }
    Ok(json!({
        "item": item,
        "book_len": book.len(),
        "total": total,
        "recipes": out,
    }))
}

/// Uses the server's recipe book to craft `item`: the bot's own 2x2 grid, or the given crafting table for a 3x3.
/// 读一个格子的方块（客户端当前的世界视图，不改动任何东西）。
fn block_at(bot: &Option<Bot>, pos: [i32; 3]) -> Result<Value, String> {
    let Some(bot) = bot.as_ref() else {
        return Err("not connected".to_owned());
    };
    let Some(world) = bot.world() else {
        return Err("world not ready".to_owned());
    };
    let (Some(view), Some(registry)) = (world.view(), world.registry()) else {
        return Err("world not ready".to_owned());
    };
    let loaded = view.chunk(pos[0] >> 4, pos[2] >> 4).is_some();
    let id = view.block(pos[0], pos[1], pos[2]);
    let block = registry.get(id);
    Ok(json!({
        "pos": pos,
        "loaded": loaded,
        "block": block.map_or("unknown", |block| block.name),
        "solid": block.is_some_and(|block| block.is_solid()),
        "runtime_id": id,
    }))
}

/// 把某个物品拿到手上（必要时先从背包挪到快捷栏）。
async fn hold(bot: &mut Option<Bot>, snap: &Shared, item: String) -> Result<Value, String> {
    set_busy(snap, Some("netease_hold"));
    let result = hold_inner(bot, &item).await;
    set_busy(snap, None);
    result
}

async fn hold_inner(bot: &mut Option<Bot>, item: &str) -> Result<Value, String> {
    let Some(bot) = bot.as_mut() else {
        return Err("not connected".to_owned());
    };
    let Some(network_id) = bot.state().items.id(item) else {
        return Err(format!("unknown item {item}"));
    };
    let found = bot
        .state()
        .inventory
        .find(|stack| stack.network_id == network_id)
        .map(|(window, slot, stack)| (window, slot, stack.count));
    let Some((window, slot, count)) = found else {
        return Err(format!("no {item} in the inventory"));
    };
    if window != acacia_bot::state::Inventory::WINDOW_INVENTORY {
        return Err(format!("{item} is not in the main inventory"));
    }
    let slot = u8::try_from(slot).map_err(|_| format!("slot {slot} out of range"))?;
    let mut held = slot;
    if slot >= acacia_bot::state::Inventory::HOTBAR_SLOTS {
        let free = (0..acacia_bot::state::Inventory::HOTBAR_SLOTS).find(|i| {
            bot.state().inventory.main.get(usize::from(*i)).is_none_or(|stack| stack.is_empty())
        });
        let Some(target) = free else { return Err("no free hotbar slot".to_owned()) };
        bot.move_item(SlotRef::Main(slot), SlotRef::Main(target), u8::try_from(count).unwrap_or(1))
            .await
            .map_err(|e| format!("moving {item} to the hotbar failed: {e}"))?;
        held = target;
    }
    bot.select_hotbar(held).map_err(|e| format!("selecting hotbar slot {held} failed: {e}"))?;
    let mut out = json!({
        "ok": true,
        "item": item,
        "slot": held,
        "count": count,
        "detail": format!("holding {item} in hotbar slot {held}"),
    });
    let held_stack = bot.state().inventory.held();
    out["held"] = json!(format!("{}x {}", held_stack.count, held_stack.network_id));
    out["held_empty"] = json!(held_stack.is_empty());
    Ok(out)
}

async fn craft(
    bot: &mut Option<Bot>,
    snap: &Shared,
    item: String,
    count: u32,
    table: Option<[i32; 3]>,
    grid: bool,
) -> Result<Value, String> {
    let Some(bot) = bot.as_mut() else {
        return Err("not connected: run netease_connect first".to_owned());
    };
    set_busy(snap, Some("netease_craft"));
    let mode = if grid { CraftMode::Grid } else { CraftMode::RecipeBook };
    let outcome = bot.craft_with(&item, count, table, mode).await;
    refresh(bot, snap, None);
    let mut result = Map::new();
    match &outcome {
        Ok(made) => {
            result.insert("ok".to_owned(), json!(true));
            result.insert("crafted".to_owned(), json!(made));
            result.insert("detail".to_owned(), json!(format!("crafted {made} x {item}")));
        }
        Err(error) => {
            result.insert("ok".to_owned(), json!(false));
            result.insert("reason".to_owned(), json!(fail_reason(error)));
            result.insert("detail".to_owned(), json!(error.to_string()));
        }
    }
    result.insert("item".to_owned(), json!(item));
    result.insert("requested".to_owned(), json!(count));
    result.insert("table".to_owned(), json!(table));
    result.insert("mode".to_owned(), json!(if grid { "grid" } else { "book" }));
    Ok(Value::Object(result))
}

/// Breaks one block and reports what was there, when the world tracker knows it.
async fn dig(bot: &mut Option<Bot>, snap: &Shared, pos: [i32; 3]) -> Result<Value, String> {
    let Some(bot) = bot.as_mut() else {
        return Err("not connected: run netease_connect first".to_owned());
    };
    set_busy(snap, Some("netease_dig"));
    let block = block_name(bot, pos);
    let outcome = bot.break_block(pos).await;
    refresh(bot, snap, None);
    let mut result = Map::new();
    result.insert("ok".to_owned(), json!(outcome.is_ok()));
    if let Err(error) = &outcome {
        result.insert("reason".to_owned(), json!(fail_reason(error)));
    }
    if let Some(block) = block {
        result.insert("block".to_owned(), json!(block));
    }
    result.insert(
        "detail".to_owned(),
        json!(match &outcome {
            Ok(()) => format!("broke the block at {pos:?}"),
            Err(e) => e.to_string(),
        }),
    );
    Ok(Value::Object(result))
}

/// Sends a chat line (as the player, not a command).
async fn place(bot: &mut Option<Bot>, snap: &Shared, against: [i32; 3], face: Face) -> Result<Value, String> {
    let Some(bot) = bot.as_mut() else {
        return Err("not connected: run netease_connect first".to_owned());
    };
    set_busy(snap, Some("netease_place"));
    let target = face.adjacent(against);
    let held = bot.state().inventory.held().clone();
    let outcome = bot.place_block(against, face).await;
    refresh(bot, snap, None);
    let mut result = Map::new();
    result.insert("ok".to_owned(), json!(outcome.is_ok()));
    if let Err(error) = &outcome {
        result.insert("reason".to_owned(), json!(fail_reason(error)));
    }
    result.insert("against".to_owned(), json!(against));
    result.insert("target".to_owned(), json!(target));
    result.insert(
        "detail".to_owned(),
        json!(match &outcome {
            Ok(()) => format!("placed the held block against {against:?} into {target:?}"),
            Err(e) => e.to_string(),
        }),
    );
    result.insert("held_empty".to_owned(), json!(held.is_empty()));
    Ok(Value::Object(result))
}

async fn chat(bot: &mut Option<Bot>, text: String) -> Result<Value, String> {
    let Some(bot) = bot.as_mut() else {
        return Err("not connected: run netease_connect first".to_owned());
    };
    let sent = bot.client().chat(text);
    let mut result = Map::new();
    result.insert("sent".to_owned(), json!(sent));
    if !sent {
        result.insert("detail".to_owned(), json!("the connection has closed"));
    }
    Ok(Value::Object(result))
}

/// Sets yaw/pitch on the movement controls, which the next `PlayerAuthInput` carries to the server.
async fn look(bot: &mut Option<Bot>, snap: &Shared, yaw: f32, pitch: f32) -> Result<Value, String> {
    let Some(bot) = bot.as_mut() else {
        return Err("not connected: run netease_connect first".to_owned());
    };
    let Some(controls) = bot.controls() else {
        return Ok(json!({"ok": false, "detail": "look not supported by this build: the bot has no physics controls"}));
    };
    let yaw = wrap_yaw(yaw);
    let pitch = pitch.clamp(-90.0, 90.0);
    (controls.yaw, controls.pitch) = (yaw, pitch);
    // Let one tick pass so the rotation goes out with the next input instead of only living locally.
    bot.wait_ticks(1).await.map_err(|e| format!("applying the rotation failed: {e}"))?;
    refresh(bot, snap, None);
    Ok(json!({
        "ok": true,
        "yaw": yaw,
        "pitch": pitch,
        "detail": "rotation set on the movement controls; the next PlayerAuthInput sends it",
    }))
}

/// Closes the connection; a later connect creates a fresh bot.
async fn disconnect(bot: &mut Option<Bot>, snap: &Shared) -> Result<Value, String> {
    let was_connected = bot.is_some();
    if let Some(mut bot) = bot.take() {
        bot.disconnect().await;
    }
    mark_disconnected(snap, None);
    Ok(json!({"disconnected": true, "was_connected": was_connected}))
}

/// The name of the block at `pos`, when the chunk is loaded and the registry knows the state.
fn block_name(bot: &Bot, [x, y, z]: [i32; 3]) -> Option<String> {
    let world = bot.world()?;
    let (view, registry) = (world.view()?, world.registry()?);
    registry.get(view.block(x, y, z)).map(|state| state.name.to_owned())
}

/// Copies the bot's live state into the snapshot and returns the copy.
fn refresh(bot: &mut Bot, shared: &Shared, busy: Option<&str>) -> Snapshot {
    // 物理客户端的视线在 controls 里（PlayerAuthInput 发的就是它）；没有物理时退回服务端给的值。
    let view = match bot.controls() {
        Some(controls) => (controls.yaw, controls.pitch),
        None => {
            let player = &bot.state().player;
            (player.yaw, player.pitch)
        }
    };
    let player = &bot.state().player;
    let position = bot.movement().and_then(|movement| movement.position());
    let block_position = bot.block_position();
    let mut snapshot = lock(shared);
    snapshot.connected = true;
    snapshot.busy = busy.map(str::to_owned);
    snapshot.position = position.or(Some([player.position.x, player.position.y, player.position.z]));
    snapshot.block_position = block_position;
    snapshot.dimension = Some(player.dimension);
    snapshot.health = Some(player.health);
    snapshot.food = Some(player.hunger);
    snapshot.saturation = Some(player.saturation);
    snapshot.yaw = Some(view.0);
    snapshot.pitch = Some(view.1);
    let game_mode = &player.game_mode;
    snapshot.game_mode = Some(format!("{game_mode:?}").to_lowercase());
    snapshot.clone()
}

/// Marks the snapshot disconnected and forgets the position and vitals of the old session.
fn mark_disconnected(shared: &Shared, error: Option<String>) {
    let mut snapshot = lock(shared);
    snapshot.connected = false;
    snapshot.busy = None;
    snapshot.name = None;
    snapshot.address = None;
    snapshot.position = None;
    snapshot.block_position = None;
    snapshot.dimension = None;
    snapshot.health = None;
    snapshot.food = None;
    snapshot.saturation = None;
    snapshot.game_mode = None;
    snapshot.last_error = error;
}

/// Records the running tool (or clears it) without touching anything else.
fn set_busy(shared: &Shared, busy: Option<&str>) {
    lock(shared).busy = busy.map(str::to_owned);
}

/// Answers every queued command with `why` instead of letting it wait for a connection that is gone.
fn fail_queued(rx: &mut mpsc::Receiver<Command>, why: &str) {
    while let Ok(command) = rx.try_recv() {
        let _ = command.into_reply().send(Err(why.to_owned()));
    }
}

fn lock(shared: &Shared) -> MutexGuard<'_, Snapshot> {
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn position_json(position: Option<[f32; 3]>) -> Value {
    match position {
        Some([x, y, z]) => json!({"x": x, "y": y, "z": z}),
        None => Value::Null,
    }
}

fn dimension_name(id: i32) -> String {
    match id {
        0 => "overworld".to_owned(),
        1 => "nether".to_owned(),
        2 => "end".to_owned(),
        _ => format!("unknown({id})"),
    }
}

/// Wraps an angle into -180..=180, the range `Controls` sends.
fn wrap_yaw(yaw: f32) -> f32 {
    let wrapped = yaw.rem_euclid(360.0);
    if wrapped > 180.0 { wrapped - 360.0 } else { wrapped }
}
