//! Structured, token-bounded perception: one snapshot of what the model needs to see.
//!
//! The owner task builds it after every command and every refresh tick; tools only read it (or ask
//! for a rebuild) and the renderer keeps the payload inside the token budget of
//! `docs/agent-design.md` ("Perception", ≈400–600 tokens full, ≈80–180 delta-only).
//!
//! Everything here is plain data: no `Bot`, no `GameState`, so it can be cloned, compared and
//! rendered from any task — that is what makes `status` answerable while a long action runs.

use std::collections::{BTreeMap, HashMap};

use acacia_bot::state::GameState;
use acacia_bot::Bot;
use acacia_world::BlockAccess;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How much of the world the snapshot carries. `brief` is the orientation view (≈120 tokens).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Detail {
    Brief,
    #[default]
    Full,
}

impl Detail {
    pub fn parse(text: &str) -> Option<Detail> {
        match text {
            "brief" => Some(Detail::Brief),
            "full" => Some(Detail::Full),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Perception {
    pub tick: u64,
    pub me: Me,
    pub held: Option<Item>,
    /// Aggregated by identifier, count desc.
    pub inventory: Vec<Item>,
    /// Head, chest, legs, feet.
    pub armor: [Option<Item>; 4],
    pub entities: Vec<EntityView>,
    pub resources: Vec<ResourceView>,
    /// The screen the bot has open (a container, a workstation, the own inventory).
    pub open_window: Option<OpenWindow>,
    /// `None` on the first snapshot of a process and after a reset.
    pub delta: Option<Delta>,
    /// The delta was abandoned because too much changed: the body is a full snapshot.
    pub delta_reset: bool,
    pub scan: Scan,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Me {
    pub pos: [f32; 3],
    pub block: [i32; 3],
    pub dim: String,
    pub health: f32,
    pub food: f32,
    pub saturation: f32,
    pub on_ground: bool,
    pub yaw: f32,
    pub pitch: f32,
    pub game_mode: String,
    pub standing_on: Option<String>,
    pub in_water: bool,
    pub in_lava: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityView {
    pub id: u64,
    pub kind: String,
    /// The username of a player entity.
    pub name: Option<String>,
    pub dist: f32,
    pub dir: &'static str,
    pub dy: f32,
    pub hostile: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceView {
    pub block: String,
    pub nearest: [i32; 3],
    pub dist: f32,
    pub dir: &'static str,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub item: String,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenWindow {
    pub window: i32,
    pub pos: Option<[i32; 3]>,
    pub title: Option<String>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockChange {
    pub block: String,
    pub pos: [i32; 3],
    pub replaced: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Delta {
    pub since_tick: u64,
    /// dx, dy, dz since the previous snapshot.
    pub moved: Option<[f32; 3]>,
    pub health: Option<(f32, f32)>,
    pub food: Option<(f32, f32)>,
    pub added: Vec<BlockChange>,
    pub removed: Vec<BlockChange>,
    pub entities_added: Vec<EntityView>,
    pub entities_removed: Vec<u64>,
    pub items_gained: Vec<Item>,
    pub items_lost: Vec<Item>,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.moved.is_none()
            && self.health.is_none()
            && self.food.is_none()
            && self.added.is_empty()
            && self.removed.is_empty()
            && self.entities_added.is_empty()
            && self.entities_removed.is_empty()
            && self.items_gained.is_empty()
            && self.items_lost.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Scan {
    pub radius: u8,
    pub y: [i32; 2],
    pub solid_cells: u32,
    pub available: bool,
}

/// Caps that keep the rendered snapshot inside its token budget.
pub mod caps {
    // The model acts on "what can I use right now", not on a full census: the most useful few plus a
    // count of the rest. Everything beyond this was measured to be re-read every step without ever
    // changing a decision (t20: 587K prompt tokens over 36 steps).
    pub const INVENTORY: usize = 10;
    pub const INVENTORY_BRIEF: usize = 5;
    pub const ENTITIES: usize = 8;
    pub const ENTITIES_BRIEF: usize = 5;
    pub const RESOURCES: usize = 8;
    pub const RESOURCES_BRIEF: usize = 5;
    pub const WINDOW_SLOTS: usize = 27;
    pub const DELTA_LIST: usize = 32;
    /// More changed blocks than this and the delta is abandoned for a full snapshot.
    pub const DELTA_MAX_CHANGES: usize = 40;
}

/// Builds a snapshot. `memo` is the previous scan (`[i32;3]` → runtime block id) kept by the owner
/// task, and `memo_tick` the tick it was taken: a delta needs both, and keeping them outside the
/// process (unlike the bridge's `OBSERVE_MEMO` static) means two bots in one process do not mix.
pub fn build(
    bot: &mut Bot,
    radius: u8,
    previous: Option<&Perception>,
    memo: &mut HashMap<[i32; 3], u32>,
    memo_tick: &mut u64,
) -> Perception {
    // The view direction lives in the movement controls (PlayerAuthInput sends it); without physics
    // fall back to what the server last told us.
    let view_deg = match bot.controls() {
        Some(controls) => (controls.yaw, controls.pitch),
        None => {
            let player = &bot.state().player;
            (player.yaw, player.pitch)
        }
    };
    let simulated = bot.movement().and_then(|movement| movement.position());
    let on_ground = bot.movement().is_some_and(|movement| movement.on_ground());
    let radius = radius.clamp(2, 24);
    let r = i32::from(radius);

    let state: &GameState = bot.state();
    let player = &state.player;
    let feet = simulated.unwrap_or([player.position.x, player.position.y, player.position.z]);
    let (px, py, pz) = (feet[0].floor() as i32, feet[1].floor() as i32, feet[2].floor() as i32);

    // Inventory aggregated by item name (names come from the server's palette, never a client table).
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for (_window, _slot, stack) in state.inventory.iter() {
        if stack.is_empty() {
            continue;
        }
        let name = state.items.name(stack.network_id).unwrap_or("unknown").to_owned();
        *counts.entry(name).or_insert(0) += stack.count as u32;
    }
    let mut inventory: Vec<Item> =
        counts.iter().map(|(item, count)| Item { item: item.clone(), count: *count }).collect();
    inventory.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.item.cmp(&b.item)));

    let armor = [0usize, 1, 2, 3].map(|index| armor_item(state, index));

    let mut entities: Vec<EntityView> = Vec::new();
    for entity in state.entities.iter() {
        if entity.runtime_id == player.runtime_entity_id {
            continue;
        }
        let (dx, dy, dz) = (entity.position.x - feet[0], entity.position.y - feet[1], entity.position.z - feet[2]);
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        if dist > radius as f32 * 2.5 {
            continue;
        }
        entities.push(EntityView {
            id: entity.runtime_id,
            kind: entity.kind.clone(),
            name: entity.username.clone(),
            dist: round1(dist),
            dir: bearing(dx, dz),
            dy: round1(dy),
            hostile: is_hostile(&entity.kind),
        });
    }
    entities.sort_by(|a, b| a.dist.total_cmp(&b.dist));
    entities.truncate(caps::ENTITIES);

    // Scan the same box the bridge scans, so two bots looking at one place see the same world.
    let mut resources: HashMap<&'static str, (f64, [i32; 3], u32)> = HashMap::new();
    let mut current: HashMap<[i32; 3], u32> = HashMap::new();
    let mut standing_on: Option<&'static str> = None;
    let mut solid_cells = 0u32;
    let mut scanned = false;
    let mut feet_block: Option<&'static str> = None;
    let mut added_blocks: Vec<BlockChange> = Vec::new();
    let mut removed_blocks: Vec<BlockChange> = Vec::new();
    if let Some(world) = bot.world() {
        if let (Some(view), Some(registry)) = (world.view(), world.registry()) {
            scanned = true;
            for x in (px - r)..=(px + r) {
                for z in (pz - r)..=(pz + r) {
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
                        let dist = f64::from(dx * dx + dz * dz).sqrt();
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
            if let Some(id) = current.get(&[px, py, pz]) {
                feet_block = registry.get(*id).map(|block| block.name);
            }
            // Block changes against the previous scan, named through the same server registry: a
            // delta that calls a block "unknown" is useless to the model.
            if previous.is_some() && !memo.is_empty() {
                for (pos, id) in current.iter() {
                    match memo.get(pos) {
                        Some(old) if old == id => {}
                        old => {
                            let block = registry.get(*id).map_or("unknown", |block| block.name).to_owned();
                            let replaced = old.and_then(|old| registry.get(*old)).map(|block| block.name.to_owned());
                            added_blocks.push(BlockChange { block, pos: *pos, replaced });
                        }
                    }
                }
                for (pos, id) in memo.iter() {
                    let inside = (px - r..=px + r).contains(&pos[0])
                        && (py - 4..=py + 4).contains(&pos[1])
                        && (pz - r..=pz + r).contains(&pos[2]);
                    if inside && !current.contains_key(pos) {
                        removed_blocks.push(BlockChange {
                            block: registry.get(*id).map_or("unknown", |block| block.name).to_owned(),
                            pos: *pos,
                            replaced: None,
                        });
                    }
                }
            }
        }
    }
    let mut resources: Vec<ResourceView> = resources
        .into_iter()
        .map(|(block, (dist, nearest, count))| ResourceView {
            block: block.to_owned(),
            nearest,
            dist: round1(dist as f32),
            dir: bearing((nearest[0] - px) as f32, (nearest[2] - pz) as f32),
            count,
        })
        .collect();
    resources.sort_by(|a, b| a.dist.total_cmp(&b.dist).then_with(|| a.block.cmp(&b.block)));
    resources.truncate(caps::RESOURCES);

    let open_window = state.containers.open.as_ref().map(|open| OpenWindow {
        window: open.window_id,
        pos: open.position.as_ref().map(|c| [c.x, c.y, c.z]),
        title: open.title.clone().map(|title| strip_codes(&title)),
        items: open
            .iter()
            .map(|(_slot, stack)| Item {
                item: state.items.name(stack.network_id).unwrap_or("unknown").to_owned(),
                count: stack.count as u32,
            })
            .collect(),
    });

    let tick = state.entities.now();
    let since_tick = *memo_tick;
    let mut delta = previous.map(|previous| {
        diff(previous, feet, player.health, player.hunger, &inventory, &entities, added_blocks, removed_blocks)
    });

    // A delta that hides half the world is worse than a full snapshot: abandon it.
    let mut delta_reset = false;
    if let Some(d) = &delta {
        if d.added.len() >= caps::DELTA_MAX_CHANGES || d.removed.len() >= caps::DELTA_MAX_CHANGES {
            delta = None;
            delta_reset = true;
        }
    }
    if let Some(d) = delta.as_mut() {
        d.since_tick = since_tick;
        d.added.truncate(caps::DELTA_LIST);
        d.removed.truncate(caps::DELTA_LIST);
        d.entities_added.truncate(caps::DELTA_LIST);
        d.entities_removed.truncate(caps::DELTA_LIST);
        d.items_gained.truncate(caps::DELTA_LIST);
        d.items_lost.truncate(caps::DELTA_LIST);
    }
    *memo = current;
    *memo_tick = tick;

    let me = Me {
        pos: [round2(feet[0]), round2(feet[1]), round2(feet[2])],
        block: [px, py, pz],
        dim: dimension_name(player.dimension),
        health: player.health,
        food: player.hunger,
        saturation: player.saturation,
        on_ground,
        yaw: round1(view_deg.0),
        pitch: round1(view_deg.1),
        game_mode: format!("{:?}", player.game_mode).to_lowercase(),
        standing_on: standing_on.map(str::to_owned),
        in_water: feet_block == Some("minecraft:water"),
        in_lava: feet_block == Some("minecraft:lava"),
    };

    Perception {
        tick,
        me,
        held: stack_item(state, state.inventory.held()),
        inventory,
        armor,
        entities,
        resources,
        open_window,
        delta,
        delta_reset,
        scan: Scan { radius, y: [py - 4, py + 4], solid_cells, available: scanned },
    }
}

fn armor_item(state: &GameState, index: usize) -> Option<Item> {
    let stack = state.inventory.armor.get(index)?;
    if stack.is_empty() {
        return None;
    }
    Some(Item { item: state.items.name(stack.network_id).unwrap_or("unknown").to_owned(), count: stack.count as u32 })
}

fn stack_item(state: &GameState, stack: &acacia_bot::state::ItemStack) -> Option<Item> {
    if stack.is_empty() {
        return None;
    }
    Some(Item { item: state.items.name(stack.network_id).unwrap_or("unknown").to_owned(), count: stack.count as u32 })
}

fn diff(
    previous: &Perception,
    feet: [f32; 3],
    health: f32,
    food: f32,
    inventory: &[Item],
    entities: &[EntityView],
    added: Vec<BlockChange>,
    removed: Vec<BlockChange>,
) -> Delta {
    let mut delta = Delta { added, removed, ..Delta::default() };
    let moved = [feet[0] - previous.me.pos[0], feet[1] - previous.me.pos[1], feet[2] - previous.me.pos[2]];
    if moved.iter().any(|axis| axis.abs() >= 0.05) {
        delta.moved = Some([round1(moved[0]), round1(moved[1]), round1(moved[2])]);
    }
    if (previous.me.health - health).abs() > f32::EPSILON {
        delta.health = Some((previous.me.health, health));
    }
    if (previous.me.food - food).abs() > f32::EPSILON {
        delta.food = Some((previous.me.food, food));
    }

    let seen: Vec<u64> = entities.iter().map(|entity| entity.id).collect();
    for entity in entities {
        if !previous.entities.iter().any(|old| old.id == entity.id) {
            delta.entities_added.push(entity.clone());
        }
    }
    for old in &previous.entities {
        if !seen.contains(&old.id) {
            delta.entities_removed.push(old.id);
        }
    }

    let before: BTreeMap<&str, u32> = previous.inventory.iter().map(|item| (item.item.as_str(), item.count)).collect();
    let now: BTreeMap<&str, u32> = inventory.iter().map(|item| (item.item.as_str(), item.count)).collect();
    for (item, count) in &now {
        match before.get(item) {
            Some(old) if old == count => {}
            Some(old) => delta.items_gained.push(Item { item: (*item).to_owned(), count: count.saturating_sub(*old) }),
            None => delta.items_gained.push(Item { item: (*item).to_owned(), count: *count }),
        }
    }
    for (item, count) in &before {
        match now.get(item) {
            Some(new) if new >= count => {}
            Some(new) => delta.items_lost.push(Item { item: (*item).to_owned(), count: count - new }),
            None => delta.items_lost.push(Item { item: (*item).to_owned(), count: *count }),
        }
    }

    delta
}

/// Renders the snapshot for the model. `delta_only` emits just the changes (skipped when the caller
/// asked for a full view or the delta was reset).
/// Renders one `observe` answer and reports whether the model was handed a **full** body.
///
/// A delta is only useful on top of a body the model already has, so it is refused — and the full
/// snapshot is sent with `full_fallback: true` — whenever the caller asked for a delta but
/// - this run never sent a full body yet (`full_sent`, e.g. the first call after connect), or
/// - the snapshot lost its baseline (`perception.delta` is `None`: first snapshot of a process, a
///   reconnect, a dimension change), or
/// - too much changed and [`Perception::delta_reset`] fired.
///
/// The caller stores the returned flag and passes it back next time.
pub fn render_observe(perception: &Perception, detail: Detail, delta_requested: bool, full_sent: bool) -> (Value, bool) {
    let fallback = delta_requested && (!full_sent || perception.delta_reset || perception.delta.is_none());
    if delta_requested && !fallback {
        return (render(perception, detail, true), false);
    }
    let mut out = render(perception, detail, false);
    if fallback {
        // Say it in the body: the model asked for a delta and got the whole world instead.
        out["full_fallback"] = json!(true);
    }
    (out, true)
}

pub fn render(perception: &Perception, detail: Detail, delta_only: bool) -> Value {
    let brief = detail == Detail::Brief;
    let me = json!({
        "pos": perception.me.pos,
        "block": perception.me.block,
        "dim": perception.me.dim,
        "health": perception.me.health,
        "food": perception.me.food,
        "sat": perception.me.saturation,
        "on_ground": perception.me.on_ground,
        "yaw": perception.me.yaw,
        "pitch": perception.me.pitch,
        "game_mode": perception.me.game_mode,
        "standing_on": perception.me.standing_on,
        "in_water": perception.me.in_water,
        "in_lava": perception.me.in_lava,
    });

    if delta_only && !perception.delta_reset {
        if let Some(delta) = &perception.delta {
            return json!({
                "tick": perception.tick,
                "me": {"pos": perception.me.pos, "health": perception.me.health, "food": perception.me.food},
                "delta": render_delta(delta),
            });
        }
    }

    let inventory_cap = if brief { caps::INVENTORY_BRIEF } else { caps::INVENTORY };
    let entity_cap = if brief { caps::ENTITIES_BRIEF } else { caps::ENTITIES };
    let resource_cap = if brief { caps::RESOURCES_BRIEF } else { caps::RESOURCES };

    let mut out = json!({
        "tick": perception.tick,
        "me": me,
        "held": item_json(perception.held.as_ref()),
        "inventory": perception
            .inventory
            .iter()
            .take(inventory_cap)
            .map(item_json_ref)
            .collect::<Vec<_>>(),
        "armor": perception.armor.iter().map(|slot| item_json(slot.as_ref())).collect::<Vec<_>>(),
        "entities": perception
            .entities
            .iter()
            .take(entity_cap)
            .map(entity_json)
            .collect::<Vec<_>>(),
        "resources": perception
            .resources
            .iter()
            .take(resource_cap)
            .map(resource_json)
            .collect::<Vec<_>>(),
        "window": perception.open_window.as_ref().map(window_json),
    });
    if !brief {
        out["scan"] = json!({
            "radius": perception.scan.radius,
            "y": perception.scan.y,
            "solid_cells": perception.scan.solid_cells,
            "available": perception.scan.available,
        });
    }
    // "前 N 个 + 其余计数": a count tells the model there is more without paying for the list.
    if perception.inventory.len() > inventory_cap {
        out["inventory_more"] = json!(perception.inventory.len() - inventory_cap);
    }
    if perception.entities.len() > entity_cap {
        out["entities_more"] = json!(perception.entities.len() - entity_cap);
    }
    if perception.resources.len() > resource_cap {
        out["resources_more"] = json!(perception.resources.len() - resource_cap);
    }
    if perception.delta_reset {
        out["delta_reset"] = json!(true);
    } else if let Some(delta) = &perception.delta {
        out["delta"] = render_delta(delta);
    }
    out
}

pub fn render_delta(delta: &Delta) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(moved) = delta.moved {
        out.insert("moved".to_owned(), json!(moved));
    }
    if let Some((before, after)) = delta.health {
        out.insert("health".to_owned(), json!([before, after]));
    }
    if let Some((before, after)) = delta.food {
        out.insert("food".to_owned(), json!([before, after]));
    }
    if !delta.added.is_empty() {
        out.insert(
            "added".to_owned(),
            json!(delta
                .added
                .iter()
                .map(|change| json!({"block": change.block, "pos": change.pos, "replaced": change.replaced}))
                .collect::<Vec<_>>()),
        );
    }
    if !delta.removed.is_empty() {
        out.insert(
            "removed".to_owned(),
            json!(delta
                .removed
                .iter()
                .map(|change| json!({"block": change.block, "pos": change.pos}))
                .collect::<Vec<_>>()),
        );
    }
    if !delta.entities_added.is_empty() {
        out.insert("entities_added".to_owned(), json!(delta.entities_added.iter().map(entity_json).collect::<Vec<_>>()));
    }
    if !delta.entities_removed.is_empty() {
        out.insert("entities_removed".to_owned(), json!(delta.entities_removed));
    }
    if !delta.items_gained.is_empty() {
        out.insert("items_gained".to_owned(), json!(delta.items_gained.iter().map(item_json_ref).collect::<Vec<_>>()));
    }
    if !delta.items_lost.is_empty() {
        out.insert("items_lost".to_owned(), json!(delta.items_lost.iter().map(item_json_ref).collect::<Vec<_>>()));
    }
    Value::Object(out)
}

fn item_json(item: Option<&Item>) -> Value {
    match item {
        Some(item) => json!({"item": item.item, "count": item.count}),
        None => Value::Null,
    }
}

fn item_json_ref(item: &Item) -> Value {
    json!({"item": item.item, "count": item.count})
}

fn entity_json(entity: &EntityView) -> Value {
    json!({
        "id": entity.id,
        "kind": entity.kind,
        "name": entity.name,
        "dist": entity.dist,
        "dir": entity.dir,
        "dy": entity.dy,
        "hostile": entity.hostile,
    })
}

fn resource_json(resource: &ResourceView) -> Value {
    json!({
        "block": resource.block,
        "nearest": resource.nearest,
        "dist": resource.dist,
        "dir": resource.dir,
        "count": resource.count,
    })
}

fn window_json(window: &OpenWindow) -> Value {
    json!({
        "window": window.window,
        "pos": window.pos,
        "title": window.title,
        "items": window.items.iter().take(caps::WINDOW_SLOTS).map(item_json_ref).collect::<Vec<_>>(),
        "slots": window.items.len(),
    })
}

/// What counts as a resource worth naming: logs, ores, and the blocks a survival task needs.
pub fn is_interesting_block(name: &str) -> bool {
    name.ends_with("_log")
        || name.ends_with("_wood")
        || name.ends_with("_stem")
        || name.ends_with("_hyphae")
        || name.ends_with("_bed")
        || name.ends_with("_door")
        || name.contains("_ore")
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

/// Hostile kinds, by substring, as the bridge lists them.
pub fn is_hostile(kind: &str) -> bool {
    [
        "zombie", "skeleton", "creeper", "spider", "witch", "pillager", "vindicator", "evoker", "enderman", "slime", "drowned",
        "husk", "stray", "phantom", "blaze", "piglin", "ravager",
    ]
    .iter()
    .any(|hostile| kind.contains(hostile))
}

/// 8-way direction; `-Z` is north and `+X` is east, as the bridge prints.
pub fn bearing(dx: f32, dz: f32) -> &'static str {
    let angle = dx.atan2(-dz).to_degrees();
    let index = (((angle + 22.5).rem_euclid(360.0)) / 45.0).floor() as usize % 8;
    ["north", "northeast", "east", "southeast", "south", "southwest", "west", "northwest"][index]
}

pub fn dimension_name(id: i32) -> String {
    match id {
        0 => "overworld".to_owned(),
        1 => "nether".to_owned(),
        2 => "end".to_owned(),
        other => format!("unknown({other})"),
    }
}

fn round1(value: f32) -> f32 {
    (value * 10.0).round() / 10.0
}

fn round2(value: f32) -> f32 {
    (value * 100.0).round() / 100.0
}

/// Bedrock titles carry `§` formatting codes; they are noise to the model.
fn strip_codes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me(pos: [f32; 3]) -> Me {
        Me { pos, health: 20.0, food: 20.0, ..Me::default() }
    }

    fn entity(id: u64) -> EntityView {
        EntityView { id, kind: "minecraft:zombie".to_owned(), name: None, dist: 3.0, dir: "north", dy: 0.0, hostile: true }
    }

    fn item(name: &str, count: u32) -> Item {
        Item { item: name.to_owned(), count }
    }

    #[test]
    fn diff_reports_blocks_entities_and_items() {
        let previous = Perception {
            tick: 10,
            me: me([0.0, 64.0, 0.0]),
            inventory: vec![item("minecraft:oak_log", 2)],
            ..Perception::default()
        };
        let added = vec![BlockChange {
            block: "minecraft:stone".to_owned(),
            pos: [1, 64, 0],
            replaced: Some("minecraft:air".to_owned()),
        }];
        let delta = diff(
            &previous,
            [1.0, 64.0, 0.0],
            18.0,
            20.0,
            &[item("minecraft:oak_log", 5), item("minecraft:dirt", 1)],
            &[entity(7)],
            added,
            Vec::new(),
        );
        assert_eq!(delta.moved, Some([1.0, 0.0, 0.0]));
        assert_eq!(delta.health, Some((20.0, 18.0)));
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.entities_added.len(), 1);
        assert_eq!(delta.items_gained.iter().find(|item| item.item == "minecraft:oak_log").expect("gained").count, 3);
        assert_eq!(delta.items_gained.iter().find(|item| item.item == "minecraft:dirt").expect("gained").count, 1);
        assert!(delta.items_lost.is_empty());
    }

    #[test]
    fn diff_reports_removed_entities_and_lost_items() {
        let previous = Perception {
            me: me([0.0, 64.0, 0.0]),
            inventory: vec![item("minecraft:bread", 4)],
            entities: vec![entity(7), entity(9)],
            ..Perception::default()
        };
        let delta = diff(&previous, [0.0, 64.0, 0.0], 20.0, 20.0, &[], &[entity(7)], Vec::new(), Vec::new());
        assert_eq!(delta.entities_removed, vec![9]);
        assert_eq!(delta.items_lost.iter().find(|item| item.item == "minecraft:bread").expect("lost").count, 4);
        assert!(delta.moved.is_none(), "standing still is not a move");
    }

    #[test]
    fn an_empty_delta_is_recognised() {
        assert!(Delta::default().is_empty());
    }

    #[test]
    fn render_applies_the_caps_and_brief_drops_the_scan() {
        let perception = Perception {
            tick: 1,
            me: me([0.0, 64.0, 0.0]),
            inventory: (0..40).map(|index| item(&format!("item{index}"), index + 1)).collect(),
            entities: (0..20).map(entity).collect(),
            scan: Scan { radius: 8, y: [60, 68], solid_cells: 10, available: true },
            ..Perception::default()
        };
        let full = render(&perception, Detail::Full, false);
        assert_eq!(full["inventory"].as_array().expect("inventory").len(), caps::INVENTORY);
        assert_eq!(full["entities"].as_array().expect("entities").len(), caps::ENTITIES);
        assert!(full.get("scan").is_some());

        let brief = render(&perception, Detail::Brief, false);
        assert_eq!(brief["inventory"].as_array().expect("inventory").len(), caps::INVENTORY_BRIEF);
        assert_eq!(brief["entities"].as_array().expect("entities").len(), caps::ENTITIES_BRIEF);
        assert!(brief.get("scan").is_none());
        assert!(
            serde_json::to_string(&brief).expect("json").len() < serde_json::to_string(&full).expect("json").len(),
            "brief must be smaller than full"
        );
    }

    #[test]
    fn a_delta_only_render_is_a_fraction_of_a_full_one() {
        let perception = Perception {
            tick: 5,
            me: me([0.0, 64.0, 0.0]),
            inventory: vec![item("minecraft:oak_log", 8)],
            resources: (0..12)
                .map(|index| ResourceView {
                    block: format!("minecraft:ore{index}"),
                    nearest: [index, 60, 0],
                    dist: index as f32,
                    dir: "north",
                    count: 1,
                })
                .collect(),
            delta: Some(Delta { moved: Some([1.0, 0.0, 0.0]), ..Delta::default() }),
            scan: Scan { radius: 8, y: [60, 68], solid_cells: 10, available: true },
            ..Perception::default()
        };
        let full = serde_json::to_string(&render(&perception, Detail::Full, false)).expect("json");
        let delta = serde_json::to_string(&render(&perception, Detail::Full, true)).expect("json");
        assert!(delta.len() * 3 < full.len(), "delta {} vs full {}", delta.len(), full.len());
        assert!(delta.contains("\"delta\""));
    }

    #[test]
    fn interesting_blocks_and_hostiles_match_the_bridge() {
        for name in ["minecraft:oak_log", "minecraft:coal_ore", "minecraft:crafting_table", "minecraft:birch_door"] {
            assert!(is_interesting_block(name), "{name} should be interesting");
        }
        for name in ["minecraft:stone", "minecraft:dirt"] {
            assert!(!is_interesting_block(name), "{name} is not a resource");
        }
        assert!(is_hostile("minecraft:zombie"));
        assert!(is_hostile("minecraft:drowned"));
        assert!(!is_hostile("minecraft:cow"));
    }

    #[test]
    fn bearings_are_eight_way_with_negative_z_north() {
        assert_eq!(bearing(0.0, -1.0), "north");
        assert_eq!(bearing(1.0, 0.0), "east");
        assert_eq!(bearing(0.0, 1.0), "south");
        assert_eq!(bearing(-1.0, 0.0), "west");
        assert_eq!(bearing(1.0, -1.0), "northeast");
    }

    #[test]
    fn dimension_names_are_stable() {
        assert_eq!(dimension_name(0), "overworld");
        assert_eq!(dimension_name(1), "nether");
        assert_eq!(dimension_name(2), "end");
        assert_eq!(dimension_name(7), "unknown(7)");
    }

    #[test]
    fn titles_lose_their_formatting_codes() {
        assert_eq!(strip_codes("\u{00a7}6Chest"), "Chest");
    }


    fn snapshot() -> Perception {
        Perception {
            tick: 42,
            me: me([0.5, 64.0, 0.5]),
            inventory: vec![item("minecraft:oak_log", 3)],
            entities: vec![entity(7)],
            resources: vec![ResourceView {
                block: "minecraft:oak_log".to_owned(),
                nearest: [3, 64, 2],
                dist: 4.0,
                dir: "east",
                count: 5,
            }],
            scan: Scan { radius: 8, y: [60, 68], solid_cells: 120, available: true },
            ..Perception::default()
        }
    }

    #[test]
    fn the_first_observe_is_the_full_body() {
        let (out, full) = render_observe(&snapshot(), Detail::Full, false, false);
        assert!(full, "the first answer hands over a body");
        for key in ["tick", "me", "inventory", "entities", "resources", "scan"] {
            assert!(out.get(key).is_some(), "the first observe carries {key}: {out}");
        }
        assert!(out.get("full_fallback").is_none(), "a full body that was asked for is not a fallback: {out}");
    }

    #[test]
    fn a_delta_without_a_body_sent_yet_falls_back_to_full() {
        // The shell refreshes its own snapshot after connect; the model has still seen nothing.
        let (out, full) = render_observe(&snapshot(), Detail::Full, true, false);
        assert!(full);
        assert_eq!(out["full_fallback"], true, "{out}");
        assert!(out.get("inventory").is_some(), "the body comes with the fallback: {out}");
    }

    #[test]
    fn a_delta_without_a_baseline_falls_back_to_full() {
        // delta == None: first snapshot of a process, after a reconnect, after a dimension change.
        let (out, full) = render_observe(&snapshot(), Detail::Full, true, true);
        assert!(full);
        assert_eq!(out["full_fallback"], true, "{out}");
    }

    #[test]
    fn a_reset_baseline_falls_back_to_full() {
        let mut perception = snapshot();
        perception.delta_reset = true;
        let (out, full) = render_observe(&perception, Detail::Full, true, true);
        assert!(full);
        assert_eq!(out["full_fallback"], true, "{out}");
        assert!(out.get("scan").is_some());
    }

    #[test]
    fn a_delta_on_top_of_a_body_is_a_delta() {
        let mut perception = snapshot();
        perception.delta = Some(Delta { since_tick: 40, moved: Some([1.0, 0.0, 0.5]), ..Delta::default() });
        let (out, full) = render_observe(&perception, Detail::Full, true, true);
        assert!(!full, "no body was handed over");
        assert!(out.get("delta").is_some(), "{out}");
        assert!(out.get("inventory").is_none(), "a delta does not repeat the body: {out}");
        assert!(out.get("full_fallback").is_none());
    }
}
