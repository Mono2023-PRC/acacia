//! The tool catalogue: one spec per tool, the JSON Schema the model sees, and the parse step that
//! turns `arguments` into a [`Call`] the shell can execute.
//!
//! Schemas are hand-written (there is no `schemars`/`jsonschema` in the workspace): the same shapes
//! drafted in `docs/agent-design.md` ("Schema drafts"). Validation is [`super::args::Args`] in one
//! pass, so the model always gets `invalid_argument` with the argument name.

use std::time::Duration;

use acacia_bot::interact::Face;
use serde_json::{json, Value};

use super::args::Args;
use super::result::Failure;

/// Which half of the catalogue a tool belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    /// Read-only; never takes the action slot.
    Perceive,
    /// Writes to the world; one at a time.
    Act,
}

impl Table {
    pub fn as_str(self) -> &'static str {
        match self {
            Table::Perceive => "perceive",
            Table::Act => "act",
        }
    }
}

/// One catalogue entry.
#[derive(Debug, Clone, Copy)]
pub struct ToolSpec {
    pub name: &'static str,
    pub table: Table,
    pub summary: &'static str,
    pub timeout: Duration,
    pub write: bool,
}

impl ToolSpec {
    /// The `tools/list` entry.
    pub fn json(self) -> Value {
        json!({"name": self.name, "description": self.summary, "inputSchema": schema(self.name)})
    }
}

/// The catalogue, in the order the model sees it.
pub static SPECS: &[ToolSpec] = &[
    ToolSpec { name: "status", table: Table::Perceive, write: false, timeout: Duration::from_secs(15),
        summary: "Connection state, position, health/food, the running action and the last error." },
    ToolSpec { name: "observe", table: Table::Perceive, write: false, timeout: Duration::from_secs(30),
        summary: "The world around the bot: self, inventory, entities, nearby resources and the scan. Full by default; delta:true reports only the changes since the last snapshot." },
    ToolSpec { name: "find", table: Table::Perceive, write: false, timeout: Duration::from_secs(60),
        summary: "Find blocks by name in a radius, nearest first. Accepts exact names, suffix (*_log) and prefix (minecraft:coal_*) patterns." },
    ToolSpec { name: "inventory", table: Table::Perceive, write: false, timeout: Duration::from_secs(15),
        summary: "Items held and carried, armour, free slots." },
    ToolSpec { name: "block", table: Table::Perceive, write: false, timeout: Duration::from_secs(15),
        summary: "Read one block: name, solidity, break time with the held tool, whether the chunk is loaded." },
    ToolSpec { name: "entities", table: Table::Perceive, write: false, timeout: Duration::from_secs(15),
        summary: "Nearby entities sorted by distance, with a hostile flag." },
    ToolSpec { name: "recipes", table: Table::Perceive, write: false, timeout: Duration::from_secs(30),
        summary: "Recipes the server sent, inputs, and whether they fit a 2x2 grid." },
    ToolSpec { name: "capabilities", table: Table::Perceive, write: false, timeout: Duration::from_secs(30),
        summary: "What can be done right now: tools, craftable items, stations in reach, food." },
    ToolSpec { name: "move_to", table: Table::Act, write: true, timeout: Duration::from_secs(120),
        summary: "Walk to a position with A* pathfinding; may dig or bridge when allowed." },
    ToolSpec { name: "dig", table: Table::Act, write: true, timeout: Duration::from_secs(120),
        summary: "Break the block at a position. The result carries position_desync_blocks and server_reference (confirmations/confirmed_ticks/fresh), on failure too." },
    ToolSpec { name: "place", table: Table::Act, write: true, timeout: Duration::from_secs(120),
        summary: "Place the held block against a face of a position. The result carries position_desync_blocks and server_reference (confirmations/confirmed_ticks/fresh), on failure too." },
    ToolSpec { name: "climb_out", table: Table::Act, write: true, timeout: Duration::from_secs(90),
        summary: "Climb back up to a surface height (digs a side wall, jumps, waits to land, repeats); already_out when the bot is not in a pit." },
    ToolSpec { name: "attack", table: Table::Act, write: true, timeout: Duration::from_secs(180),
        summary: "Fight an entity by runtime id, kind, or nearest: walk into reach, then swing until it dies (once:true swings without approaching)." },
    ToolSpec { name: "container", table: Table::Act, write: true, timeout: Duration::from_secs(90),
        summary: "Open a container and list, take, put or close; keeps it open only when asked." },
    ToolSpec { name: "craft", table: Table::Act, write: true, timeout: Duration::from_secs(180),
        summary: "Craft an item from the recipe book (default), or lay the ingredients out by hand." },
    ToolSpec { name: "smelt", table: Table::Act, write: true, timeout: Duration::from_secs(180),
        summary: "Smelt in a furnace: input item, count, optional fuel." },
    ToolSpec { name: "eat", table: Table::Act, write: true, timeout: Duration::from_secs(60),
        summary: "Eat food (the named one, or the best one carried) to restore hunger." },
    ToolSpec { name: "equip", table: Table::Act, write: true, timeout: Duration::from_secs(60),
        summary: "Equip armour or move an item into the hand/offhand: item (alias from) and slot (alias to)." },
    ToolSpec { name: "look", table: Table::Act, write: false, timeout: Duration::from_secs(30),
        summary: "Point the view at yaw/pitch or at a block position." },
    ToolSpec { name: "chat", table: Table::Act, write: false, timeout: Duration::from_secs(30),
        summary: "Send a chat line (plain text, no commands). Disabled unless configured." },
    ToolSpec { name: "hold", table: Table::Act, write: false, timeout: Duration::from_secs(60),
        summary: "Put an item from the inventory into the hand and select it." },
    ToolSpec { name: "sleep", table: Table::Act, write: true, timeout: Duration::from_secs(180),
        summary: "Sleep in a nearby bed (or the one at a position): walk there, lie down, report whether the player is asleep." },
    ToolSpec { name: "pickup", table: Table::Act, write: true, timeout: Duration::from_secs(120),
        summary: "Walk to a dropped item and wait for the server to hand it over; returns the stacks gained, what is left nearby, and the position reference. Pass the runtime id from entities{} (a radius-only search often misses)." },
    ToolSpec { name: "stop", table: Table::Act, write: true, timeout: Duration::from_secs(30),
        summary: "Drop everything the bot is doing (walk, mine, use, sleep, ride) and return to idle." },
    ToolSpec { name: "trade", table: Table::Act, write: true, timeout: Duration::from_secs(120),
        summary: "Trade with a villager: target is a runtime_id or nearest, offer is an index from trades or \"first_payable\" (the default: the first offer the inventory covers). Sends the server's own trade request; got/paid come from the inventory diff." },
    ToolSpec { name: "trades", table: Table::Perceive, write: false, timeout: Duration::from_secs(60),
        summary: "Read a villager's offers (buy_a/buy_b/sell, uses/max_uses) before trading: opens the trade screen, reads it, closes it again. Each offer says payable and, when it is not, what is missing; payable_offers lists the ones a trade would cover right now." },
    ToolSpec { name: "drop", table: Table::Act, write: true, timeout: Duration::from_secs(60),
        summary: "Drop a stack from the main inventory (slot, or the first stack of item): the way out when the hotbar is full of junk. Returns dropped item/count and the remaining count." },
];

/// Looks a spec up by name.
pub fn spec(name: &str) -> Option<&'static ToolSpec> {
    SPECS.iter().find(|spec| spec.name == name)
}

/// A parsed, validated tool call.
#[derive(Debug, Clone)]
pub enum Call {
    Status,
    Observe { radius: u8, detail: crate::perceive::Detail, delta: bool },
    Find { block: String, radius: u8, limit: u8 },
    // (radius keeps the *requested* value; the shell clamps the scan and says so)
    Inventory { detail: crate::perceive::Detail },
    Block { at: Option<[i32; 3]>, offset: Option<[i32; 3]> },
    Entities { radius: u8, kinds: Option<Vec<String>> },
    Recipes { item: Option<String>, limit: usize },
    Capabilities { keyword: Option<String> },
    MoveTo { at: Option<[i32; 3]>, xz: Option<[i32; 2]>, exact: bool, allow_dig: bool, allow_bridge: bool, timeout_s: u64 },
    Dig { at: [i32; 3], face: Face },
    Place { at: Option<[i32; 3]>, face: Face, item: Option<String>, auto: bool, radius: f32 },
    Attack { target: String, radius: f32, times: u8, once: bool, allow_dig: bool, timeout_s: u64 },
    ClimbOut { surface_y: Option<i32> },
    Container { at: Option<[i32; 3]>, op: ContainerOp, item: Option<String>, count: u8, slot: Option<u8>, keep_open: bool },
    Craft { item: String, count: u32, mode: CraftMode, table: Option<[i32; 3]> },
    Smelt { at: [i32; 3], input: String, count: u8, fuel: Option<(String, u8)> },
    Eat { food: Option<String> },
    Equip { item: String, slot: EquipSlot },
    Look { yaw: Option<f32>, pitch: Option<f32>, at: Option<[i32; 3]> },
    Chat { text: String, recipient: Option<String> },
    Hold { item: String, slot: Option<u8> },
    Sleep { at: Option<[i32; 3]>, radius: u8, timeout_s: u64 },
    Pickup { id: Option<u64>, item: Option<String>, radius: u8, timeout_s: u64 },
    Stop,
    Drop { slot: Option<u8>, item: Option<String>, count: u8 },
    Trade { target: String, offer: OfferChoice, times: u32, radius: f32 },
    Trades { target: String, radius: f32 },
}

/// Which of a villager's offers `trade` should use (t83): an explicit index, or the first one the
/// inventory can pay for right now.
///
/// The default is `FirstPayable`, not `0`: the model used to pick `offer: 0` (32 rotten_flesh) while 64
/// emeralds sat in the bag and `offer: 1` was a one-emerald trade away — the tool had told it nothing
/// about which offers were affordable, so it guessed from the list order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferChoice {
    /// An index into `trades`' list; unknown costs still fail as `missing_offer_cost` (unchanged behaviour).
    Index(u32),
    /// The first offer whose `buy_a`+`buy_b` the current inventory covers.
    FirstPayable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerOp {
    List,
    Take,
    Put,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CraftMode {
    Grid,
    Book,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EquipSlot {
    Hand,
    Head,
    Chest,
    Legs,
    Feet,
    Offhand,
}

impl Call {
    pub fn name(&self) -> &'static str {
        match self {
            Call::Status => "status",
            Call::Observe { .. } => "observe",
            Call::Find { .. } => "find",
            Call::Inventory { .. } => "inventory",
            Call::Block { .. } => "block",
            Call::Entities { .. } => "entities",
            Call::Recipes { .. } => "recipes",
            Call::Capabilities { .. } => "capabilities",
            Call::MoveTo { .. } => "move_to",
            Call::Dig { .. } => "dig",
            Call::Place { .. } => "place",
            Call::Attack { .. } => "attack",
            Call::ClimbOut { .. } => "climb_out",
            Call::Container { .. } => "container",
            Call::Craft { .. } => "craft",
            Call::Smelt { .. } => "smelt",
            Call::Eat { .. } => "eat",
            Call::Equip { .. } => "equip",
            Call::Look { .. } => "look",
            Call::Chat { .. } => "chat",
            Call::Hold { .. } => "hold",
            Call::Sleep { .. } => "sleep",
            Call::Pickup { .. } => "pickup",
            Call::Stop => "stop",
            Call::Drop { .. } => "drop",
            Call::Trade { .. } => "trade",
            Call::Trades { .. } => "trades",
        }
    }

    pub fn is_write(&self) -> bool {
        spec(self.name()).is_some_and(|spec| spec.write)
    }
}

const FACES: [&str; 6] = ["up", "down", "north", "south", "east", "west"];

fn face(name: &str) -> Face {
    match name {
        "down" => Face::Down,
        "north" => Face::North,
        "south" => Face::South,
        "east" => Face::East,
        "west" => Face::West,
        _ => Face::Up,
    }
}

/// Parses `arguments` for `name`.
pub fn parse(name: &str, arguments: Option<&Value>) -> Result<Call, Failure> {
    let args = Args::new(spec(name).map_or("tool", |spec| spec.name), arguments)?;
    let detail = |args: &Args, key: &str| -> Result<crate::perceive::Detail, Failure> {
        match args.string(key)? {
            None => Ok(crate::perceive::Detail::Full),
            Some(text) => {
                crate::perceive::Detail::parse(&text).ok_or_else(|| Failure::invalid_argument("detail must be \"brief\" or \"full\""))
            }
        }
    };
    let call = match name {
        "status" => Call::Status,
        "observe" => Call::Observe {
            radius: args.u8("radius", 8, 2, 24)?,
            detail: detail(&args, "detail")?,
            delta: args.boolean("delta", false)?,
        },
        "find" => Call::Find {
            block: args.required_string("block")?,
            radius: args.u8("radius", 16, 1, 255)?,
            limit: args.u8("limit", 10, 1, 64)?,
        },
        "inventory" => Call::Inventory { detail: detail(&args, "detail")? },
        "block" => Call::Block { at: args.block_pos("at")?, offset: args.offset("offset")? },
        "entities" => Call::Entities {
            radius: args.u8("radius", 16, 1, 48)?,
            kinds: args.string_list("kinds")?,
        },
        "recipes" => Call::Recipes {
            item: args.string("item")?,
            limit: args.uint("limit", 10, 1, 64)? as usize,
        },
        "capabilities" => Call::Capabilities { keyword: args.string("keyword")? },
        "move_to" => {
            let at = args.block_pos("at")?;
            let xz = args.i32_pair("xz")?;
            if at.is_none() && xz.is_none() {
                return Err(Failure::invalid_argument("move_to needs either at [x,y,z] or xz [x,z]"));
            }
            Call::MoveTo {
                at,
                xz,
                exact: args.boolean("exact", false)?,
                allow_dig: args.boolean("allow_dig", false)?,
                allow_bridge: args.boolean("allow_bridge", false)?,
                timeout_s: args.uint("timeout_s", 60, 1, 600)?,
            }
        }
        "dig" => Call::Dig { at: args.required_block_pos("at")?, face: face(args.variant("face", &FACES, Some("up"))?) },
        "place" => {
            let auto = args.boolean("auto", false)?;
            let at = args.block_pos("at")?;
            if at.is_none() && !auto {
                return Err(crate::tools::Failure::invalid_argument(
                    "place needs at (the block to place against) or auto:true (pick a free neighbouring cell)",
                ));
            }
            Call::Place {
                at,
                face: face(args.variant("face", &FACES, Some("up"))?),
                item: args.string("item")?,
                auto,
                radius: args.f32("radius", Some(4.0))?.clamp(1.0, 8.0),
            }
        }
        "climb_out" => Call::ClimbOut { surface_y: args.optional_f32("surface_y")?.map(|y| y.round() as i32) },
        "attack" => Call::Attack {
            target: args.required_string("target")?,
            radius: args.f32("radius", Some(16.0))?.clamp(1.0, 32.0),
            times: args.u8("times", 1, 1, 10)?,
            once: args.boolean("once", false)?,
            allow_dig: args.boolean("allow_dig", false)?,
            timeout_s: args.uint("timeout_s", 45, 1, 300)?,
        },
        "container" => Call::Container {
            at: args.block_pos("at")?,
            op: match args.variant("op", &["list", "take", "put", "close"], None)? {
                "list" => ContainerOp::List,
                "take" => ContainerOp::Take,
                "put" => ContainerOp::Put,
                _ => ContainerOp::Close,
            },
            item: args.string("item")?,
            count: args.u8("count", 1, 1, 64)?,
            slot: args.optional_u8("slot", 0, 255)?,
            keep_open: args.boolean("keep_open", false)?,
        },
        "craft" => Call::Craft {
            item: args.required_string("item")?,
            count: args.u32("count", 1, 1, 64)?,
            mode: match args.variant("mode", &["book", "grid"], Some("book"))? {
                "grid" => CraftMode::Grid,
                _ => CraftMode::Book,
            },
            table: args.block_pos("table")?,
        },
        "smelt" => Call::Smelt {
            at: args.required_block_pos("at")?,
            input: args.required_string("input")?,
            count: args.u8("count", 1, 1, 64)?,
            fuel: match args.string("fuel")? {
                Some(fuel) => Some((fuel, args.u8("fuel_count", 1, 1, 64)?)),
                None => None,
            },
        },
        "eat" => Call::Eat { food: args.string("food")? },
        "equip" => Call::Equip { item: equip_item(&args)?, slot: equip_slot(&args)? },
        "look" => {
            let at = args.block_pos("at")?;
            let yaw = args.optional_f32("yaw")?;
            let pitch = args.optional_f32("pitch")?.unwrap_or(0.0);
            if at.is_none() && yaw.is_none() {
                return Err(Failure::invalid_argument("look needs yaw (and optional pitch) or at [x,y,z]"));
            }
            Call::Look { yaw, pitch: Some(pitch), at }
        }
        "chat" => Call::Chat { text: args.required_string("text")?, recipient: args.string("recipient")? },
        "hold" => Call::Hold { item: args.required_string("item")?, slot: args.optional_u8("slot", 0, 8)? },
        "sleep" => Call::Sleep {
            at: args.block_pos("at")?,
            radius: args.u8("radius", 8, 1, 32)?,
            timeout_s: args.uint("timeout_s", 90, 1, 600)?,
        },
        "pickup" => Call::Pickup {
            id: args.int("id", Some(0)).ok().filter(|id| *id > 0).map(|id| id as u64),
            item: args.string("item")?,
            radius: args.u8("radius", 6, 1, 32)?,
            timeout_s: args.uint("timeout_s", 60, 1, 300)?,
        },
        "stop" => Call::Stop,
        "trade" => Call::Trade {
            target: args.required_string("target")?,
            offer: offer_choice(&args)?,
            times: args.u32("times", 1, 1, 16)?,
            radius: args.f32("radius", Some(16.0))?.clamp(1.0, 64.0),
        },
        "trades" => Call::Trades {
            target: args.required_string("target")?,
            radius: args.f32("radius", Some(16.0))?.clamp(1.0, 64.0),
        },
        "drop" => {
            let slot = args.optional_u8("slot", 0, 35)?;
            let item = args.string("item")?;
            if slot.is_none() && item.is_none() {
                return Err(crate::tools::Failure::invalid_argument(
                    "drop needs slot (main inventory index, 0-35) or item (the first stack of that item)",
                ));
            }
            Call::Drop { slot, item, count: args.u8("count", 1, 1, 64)? }
        }
        other => return Err(Failure::unknown_tool(other)),
    };
    Ok(call)
}

/// `offer` is an index into `trades`' list, the keyword `first_payable`, or absent (which means
/// `first_payable`). Anything else is an argument error, so the model gets a message it can act on.
fn offer_choice(args: &Args) -> Result<OfferChoice, Failure> {
    match args.raw("offer") {
        None => Ok(OfferChoice::FirstPayable),
        Some(Value::String(text)) if text == "first_payable" => Ok(OfferChoice::FirstPayable),
        Some(Value::String(text)) => Err(Failure::invalid_argument(format!(
            "offer must be an index (0-64) or \"first_payable\", not {text:?}"
        ))),
        Some(_) => Ok(OfferChoice::Index(args.u32("offer", 0, 0, 64)?)),
    }
}

/// The slots `equip` accepts, in the order the schema lists them.
const EQUIP_SLOTS: &[&str] = &["hand", "head", "chest", "legs", "feet", "offhand"];

/// `equip`'s item argument, under both names it is known by.
///
/// The protocol key is `item`, but acacia-bot's own parameter is called `from` (`survival/equip.rs`)
/// and a model reading the failure it caused has typed that name (t116, run
/// `20261109-083456-t116`): one wasted call every time. Both names are accepted, they mean the same
/// thing, and two *different* values are a conflict worth an error rather than a silent pick.
fn equip_item(args: &Args) -> Result<String, Failure> {
    let item = args.string("item")?;
    let from = args.string("from")?;
    match (item, from) {
        (Some(item), Some(from)) if item != from => Err(Failure::invalid_argument(format!(
            "equip got item={item:?} and from={from:?}: they are the same argument (item is the protocol \
             name, from is the implementation name), pass exactly one — \
             equip{{\"item\":\"minecraft:bread\",\"to\":\"offhand\"}}"
        ))),
        (Some(item), _) => Ok(item),
        (None, Some(from)) => Ok(from),
        (None, None) => Err(Failure::invalid_argument(
            "equip needs an item: pass it as \"item\" (or its alias \"from\"), plus an optional \"slot\" \
             (or its alias \"to\": hand/head/chest/legs/feet/offhand) — \
             equip{\"item\":\"minecraft:bread\",\"to\":\"offhand\"}",
        )),
    }
}

/// `equip`'s destination slot, under both names it is known by (`slot`, and `to` as acacia-bot and
/// the docs spell it). Defaults to the hand, as the schema says.
fn equip_slot(args: &Args) -> Result<EquipSlot, Failure> {
    let slot = args.string("slot")?;
    let to = args.string("to")?;
    let name = match (slot, to) {
        (Some(slot), Some(to)) if slot != to => {
            return Err(Failure::invalid_argument(format!(
                "equip got slot={slot:?} and to={to:?}: they are the same argument, pass exactly one — \
                 equip{{\"item\":\"minecraft:bread\",\"to\":\"offhand\"}}"
            )));
        }
        (Some(slot), _) => slot,
        (None, Some(to)) => to,
        (None, None) => return Ok(EquipSlot::Hand),
    };
    EQUIP_SLOTS.iter().copied().find(|candidate| *candidate == name).map(|slot| match slot {
        "head" => EquipSlot::Head,
        "chest" => EquipSlot::Chest,
        "legs" => EquipSlot::Legs,
        "feet" => EquipSlot::Feet,
        "offhand" => EquipSlot::Offhand,
        _ => EquipSlot::Hand,
    }).ok_or_else(|| Failure::invalid_argument(format!("slot (alias to) must be one of {EQUIP_SLOTS:?}, not {name:?}")))
}

/// The JSON Schema for `name`, as `tools/list` returns it.
pub fn schema(name: &str) -> Value {
    match name {
        "status" => json!({"type": "object", "properties": {}, "additionalProperties": false}),
        "observe" => json!({
            "type": "object",
            "properties": {
                "radius": {"type": "integer", "minimum": 2, "maximum": 24, "default": 8,
                           "description": "Horizontal scan radius in blocks"},
                "detail": {"type": "string", "enum": ["brief", "full"], "default": "full"},
                "delta": {"type": "boolean", "default": false,
                          "description": "Only the change since the last snapshot. The first call, a reconnect and any lost baseline answer the full body with full_fallback:true"}
            },
            "additionalProperties": false
        }),
        "find" => json!({
            "type": "object",
            "required": ["block"],
            "properties": {
                "block": {"type": "string",
                          "description": "Block to look for: exact (minecraft:stone), a wildcard (*_bed, minecraft:*_bed, *bed*), or an alias (bed, wool, planks, log, door, carpet). Case-insensitive; `bed` means any colour bed"},
                "radius": {"type": "integer", "minimum": 1, "maximum": 255, "default": 16,
                           "description": "Horizontal search radius. Scans are capped at 64 blocks; the answer reports requested_radius and effective_radius, and says when the target may simply be farther away"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 64, "default": 10}
            },
            "additionalProperties": false
        }),
        "inventory" => json!({
            "type": "object",
            "properties": {"detail": {"type": "string", "enum": ["brief", "full"], "default": "full"}},
            "additionalProperties": false
        }),
        "block" => json!({
            "type": "object",
            "properties": {
                "at": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3},
                "offset": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3,
                           "description": "Relative to the bot's feet block"}
            },
            "additionalProperties": false
        }),
        "entities" => json!({
            "type": "object",
            "properties": {
                "radius": {"type": "integer", "minimum": 1, "maximum": 48, "default": 16},
                "kinds": {"type": "array", "items": {"type": "string"}, "description": "Only these kinds, e.g. [\"minecraft:zombie\"]"}
            },
            "additionalProperties": false
        }),
        "recipes" => json!({
            "type": "object",
            "properties": {
                "item": {"type": "string", "description": "Filter by output name substring"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 64, "default": 10}
            },
            "additionalProperties": false
        }),
        "capabilities" => json!({
            "type": "object",
            "properties": {"keyword": {"type": "string", "description": "Filter craftable items by substring"}},
            "additionalProperties": false
        }),
        "move_to" => json!({
            "type": "object",
            "properties": {
                "at": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3,
                       "description": "Target block [x,y,z]"},
                "xz": {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2,
                       "description": "Target column [x,z]: arrive at any y"},
                "exact": {"type": "boolean", "default": false},
                "allow_dig": {"type": "boolean", "default": false, "description": "May break blocks on the way"},
                "allow_bridge": {"type": "boolean", "default": false, "description": "May place blocks to cross gaps"},
                "timeout_s": {"type": "integer", "minimum": 1, "maximum": 600, "default": 60}
            },
            "additionalProperties": false
        }),
        "dig" => json!({
            "type": "object",
            "required": ["at"],
            "properties": {
                "at": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3},
                "face": {"type": "string", "enum": ["up", "down", "north", "south", "east", "west"], "default": "up"}
            },
            "additionalProperties": false
        }),
        "place" => json!({
            "type": "object",
            "properties": {
                "at": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3,
                       "description": "The block to place against; the item goes into the neighbouring cell"},
                "face": {"type": "string", "enum": ["up", "down", "north", "south", "east", "west"], "default": "up"},
                "item": {"type": "string", "description": "Optional: hold this item first"},
                "auto": {"type": "boolean", "default": false,
                         "description": "Pick the spot yourself: air cell, solid floor under it, within radius; tries the three nearest"},
                "radius": {"type": "number", "minimum": 1, "maximum": 8, "default": 4,
                           "description": "How far to look for a spot when auto is on"}
            },
            "anyOf": [{"required": ["at"]}, {"required": ["auto"]}],
            "additionalProperties": false
        }),
        "climb_out" => json!({
            "type": "object",
            "properties": {
                "surface_y": {"type": "integer", "description": "Height to climb back to (the y the bot stood at before it fell in); omit for the last stable height, or 3 blocks above the feet"}
            },
            "additionalProperties": false
        }),
        "attack" => json!({
            "type": "object",
            "required": ["target"],
            "properties": {
                "target": {"type": "string", "description": "runtime id, \"nearest\" for the closest hostile, or an entity kind like zombie"},
                "radius": {"type": "number", "minimum": 1, "maximum": 32, "default": 16,
                           "description": "How far to look for a kind/nearest target"},
                "times": {"type": "integer", "minimum": 1, "maximum": 10, "default": 1,
                          "description": "Swings made with once:true; the default fight keeps swinging until the target dies"},
                "once": {"type": "boolean", "default": false,
                         "description": "Swing without approaching: answers out_of_reach when the target is out of the arm's reach"},
                "allow_dig": {"type": "boolean", "default": false,
                              "description": "Let the approach dig when the target is a couple of blocks below or above"},
                "timeout_s": {"type": "integer", "minimum": 1, "maximum": 300, "default": 45}
            },
            "additionalProperties": false
        }),
        "container" => json!({
            "type": "object",
            "required": ["op"],
            "properties": {
                "at": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3,
                       "description": "Container position; required when nothing is open yet"},
                "op": {"type": "string", "enum": ["list", "take", "put", "close"]},
                "item": {"type": "string", "description": "take/put: item to move"},
                "count": {"type": "integer", "minimum": 1, "maximum": 64, "default": 1},
                "slot": {"type": "integer", "minimum": 0, "description": "Container slot for take, inventory slot for put"},
                "keep_open": {"type": "boolean", "default": false}
            },
            "additionalProperties": false
        }),
        "craft" => json!({
            "type": "object",
            "required": ["item"],
            "properties": {
                "item": {"type": "string", "description": "minecraft:oak_planks"},
                "count": {"type": "integer", "minimum": 1, "maximum": 64, "default": 1},
                "mode": {"type": "string", "enum": ["book", "grid"], "default": "book",
                         "description": "book uses the server recipe book (default); grid lays the ingredients out by hand"},
                "table": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3}
            },
            "additionalProperties": false
        }),
        "smelt" => json!({
            "type": "object",
            "required": ["at", "input"],
            "properties": {
                "at": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3},
                "input": {"type": "string", "description": "Item to smelt, e.g. minecraft:raw_iron"},
                "count": {"type": "integer", "minimum": 1, "maximum": 64, "default": 1},
                "fuel": {"type": "string", "description": "Fuel item, e.g. minecraft:coal"},
                "fuel_count": {"type": "integer", "minimum": 1, "maximum": 64, "default": 1}
            },
            "additionalProperties": false
        }),
        "eat" => json!({
            "type": "object",
            "properties": {"food": {"type": "string", "description": "Item to eat; default the best food carried"}},
            "additionalProperties": false
        }),
        "equip" => json!({
            "type": "object",
            "anyOf": [{"required": ["item"]}, {"required": ["from"]}],
            "properties": {
                "item": {"type": "string", "description": "Item name, e.g. minecraft:bread"},
                "from": {"type": "string", "description": "Alias of `item` (acacia-bot calls the parameter `from`)"},
                "slot": {"type": "string", "enum": ["hand", "head", "chest", "legs", "feet", "offhand"], "default": "hand"},
                "to": {"type": "string", "enum": ["hand", "head", "chest", "legs", "feet", "offhand"],
                       "description": "Alias of `slot`"}
            },
            "additionalProperties": false
        }),
        "look" => json!({
            "type": "object",
            "properties": {
                "yaw": {"type": "number", "description": "Degrees; 0 faces +Z, -90 looks up in pitch"},
                "pitch": {"type": "number", "minimum": -90, "maximum": 90},
                "at": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3}
            },
            "additionalProperties": false
        }),
        "chat" => json!({
            "type": "object",
            "required": ["text"],
            "properties": {
                "text": {"type": "string", "description": "Plain text, no slash commands"},
                "recipient": {"type": "string", "description": "Whisper this player instead of public chat"}
            },
            "additionalProperties": false
        }),
        "hold" => json!({
            "type": "object",
            "required": ["item"],
            "properties": {
                "item": {"type": "string"},
                "slot": {"type": "integer", "minimum": 0, "maximum": 8, "description": "Hotbar slot to use instead of searching"}
            },
            "additionalProperties": false
        }),
        "sleep" => json!({
            "type": "object",
            "properties": {
                "at": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3,
                       "description": "Bed position; omit to use the nearest bed"},
                "radius": {"type": "integer", "minimum": 1, "maximum": 32, "default": 8},
                "timeout_s": {"type": "integer", "minimum": 1, "maximum": 600, "default": 90}
            },
            "additionalProperties": false
        }),
        "pickup" => json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Entity runtime_id of the drop (the `id` `entities` reports). Preferred when you already know which drop you mean"},
                "item": {"type": "string", "description": "Only pick up a drop of this item name"},
                "radius": {"type": "integer", "minimum": 1, "maximum": 32, "default": 6},
                "timeout_s": {"type": "integer", "minimum": 1, "maximum": 300, "default": 60}
            },
            "additionalProperties": false
        }),
        "stop" => json!({"type": "object", "properties": {}, "additionalProperties": false}),
        "trade" => json!({
            "type": "object",
            "required": ["target"],
            "properties": {
                "target": {"type": "string", "description": "Required. Either the trader's runtime_id as entities reports it, or \"nearest\" to pick the closest villager/wandering_trader within radius (ambient mobs are ignored). Trade once per offer: the window opens and closes on each call"},
                "offer": {"oneOf": [
                              {"type": "integer", "minimum": 0, "maximum": 64},
                              {"type": "string", "const": "first_payable"}
                          ],
                          "default": "first_payable",
                          "description": "Index in trades' list, or \"first_payable\" (the default) for the first offer the inventory can pay for right now. trades marks each offer payable and lists payable_offers"},
                "times": {"type": "integer", "minimum": 1, "maximum": 16, "default": 1,
                          "description": "How many times to trade; do not exceed max_uses - uses"},
                "radius": {"type": "number", "minimum": 1, "maximum": 64, "default": 16,
                           "description": "Search radius when target is nearest"}
            },
            "additionalProperties": false
        }),
        "trades" => json!({
            "type": "object",
            "required": ["target"],
            "properties": {
                "target": {"type": "string", "description": "runtime_id of the trader, or \"nearest\""},
                "radius": {"type": "number", "minimum": 1, "maximum": 64, "default": 16}
            },
            "additionalProperties": false
        }),
        "drop" => json!({
            "type": "object",
            "properties": {
                "slot": {"type": "integer", "minimum": 0, "maximum": 35,
                         "description": "Main inventory index: 0-8 hotbar, 9-35 backpack"},
                "item": {"type": "string", "description": "Item name: drops the first stack of it (used when slot is omitted)"},
                "count": {"type": "integer", "minimum": 1, "maximum": 64, "default": 1}
            },
            "anyOf": [{"required": ["slot"]}, {"required": ["item"]}],
            "additionalProperties": false
        }),
        _ => json!({"type": "object", "properties": {}, "additionalProperties": false}),
    }
}

#[cfg(test)]
mod offer_tests {
    //! t83: `offer` grew a keyword, and the old numeric form must keep parsing exactly as before.
    use super::*;

    fn trade(arguments: Value) -> Result<OfferChoice, Failure> {
        match parse("trade", Some(&arguments)) {
            Ok(Call::Trade { offer, .. }) => Ok(offer),
            Ok(other) => panic!("trade parsed as {other:?}"),
            Err(failure) => Err(failure),
        }
    }

    #[test]
    fn omitting_offer_means_the_first_payable_one() {
        assert_eq!(trade(json!({"target": "nearest"})).unwrap(), OfferChoice::FirstPayable);
    }

    #[test]
    fn the_keyword_parses() {
        assert_eq!(trade(json!({"target": "nearest", "offer": "first_payable"})).unwrap(), OfferChoice::FirstPayable);
    }

    #[test]
    fn a_number_is_still_an_index() {
        assert_eq!(trade(json!({"target": "nearest", "offer": 1})).unwrap(), OfferChoice::Index(1));
        assert_eq!(trade(json!({"target": "nearest", "offer": 0})).unwrap(), OfferChoice::Index(0));
        assert_eq!(trade(json!({"target": "nearest", "offer": 64})).unwrap(), OfferChoice::Index(64));
        // Out of range clamps, as it did before: the parser never invented a reason for it.
        assert_eq!(trade(json!({"target": "nearest", "offer": 99})).unwrap(), OfferChoice::Index(64));
    }

    #[test]
    fn any_other_string_is_an_argument_error_the_model_can_fix() {
        let failure = trade(json!({"target": "nearest", "offer": "0"})).unwrap_err();
        assert_eq!(failure.reason, "invalid_argument");
        assert!(failure.message.contains("first_payable"), "{}", failure.message);
        assert!(failure.message.contains("\"0\""), "the bad value is quoted back: {}", failure.message);
    }

    #[test]
    fn the_schema_advertises_both_forms_and_defaults_to_first_payable() {
        let schema = schema("trade");
        let offer = &schema["properties"]["offer"];
        assert_eq!(offer["default"], json!("first_payable"));
        assert_eq!(offer["oneOf"][0]["type"], json!("integer"));
        assert_eq!(offer["oneOf"][1]["const"], json!("first_payable"));
        assert!(offer["description"].as_str().unwrap_or_default().contains("payable"));
    }
}

#[cfg(test)]
mod equip_tests {
    use super::*;

    fn equip(arguments: Value) -> Result<Call, Failure> {
        parse("equip", Some(&arguments))
    }

    fn item_of(call: Call) -> (String, EquipSlot) {
        match call {
            Call::Equip { item, slot } => (item, slot),
            other => panic!("expected equip, got {other:?}"),
        }
    }

    /// t116 F2: the bot's own parameter is called `from`, the protocol key is `item`, and a model that
    /// read one failure typed the other name. Both must work.
    #[test]
    fn the_item_argument_answers_to_from_as_well() {
        assert_eq!(item_of(equip(json!({"from": "minecraft:bread"})).expect("from")), ("minecraft:bread".to_owned(), EquipSlot::Hand));
        assert_eq!(item_of(equip(json!({"item": "minecraft:bread"})).expect("item")), ("minecraft:bread".to_owned(), EquipSlot::Hand));
    }

    /// The docs and acacia-bot spell the destination `to`; the schema spells it `slot`.
    #[test]
    fn the_destination_answers_to_to_as_well() {
        let (_, slot) = item_of(equip(json!({"item": "minecraft:shield", "to": "offhand"})).expect("to"));
        assert_eq!(slot, EquipSlot::Offhand);
        let (_, slot) = item_of(equip(json!({"from": "minecraft:shield", "to": "head"})).expect("both aliases"));
        assert_eq!(slot, EquipSlot::Head);
    }

    /// Two names for one argument is a convenience; two *different* values is a mistake, and picking
    /// one silently would leave the model believing the other took effect.
    #[test]
    fn two_names_with_different_values_are_a_conflict() {
        for (arguments, both) in [
            (json!({"item": "minecraft:bread", "from": "minecraft:apple"}), ["item", "from"]),
            (json!({"item": "minecraft:bread", "slot": "head", "to": "offhand"}), ["slot", "to"]),
        ] {
            let failure = equip(arguments.clone()).expect_err("conflict");
            assert_eq!(failure.reason, "invalid_argument", "{arguments}");
            assert!(failure.message.contains("same argument"), "{arguments}: {}", failure.message);
            assert!(failure.message.contains(both[0]) && failure.message.contains(both[1]), "{arguments}: {}", failure.message);
        }
    }

    /// Same value under both names is not a conflict: the model repeated itself, it did not disagree.
    #[test]
    fn the_same_value_under_both_names_is_accepted() {
        let (item, slot) = item_of(equip(json!({"item": "minecraft:bread", "from": "minecraft:bread", "slot": "offhand", "to": "offhand"})).expect("agree"));
        assert_eq!((item.as_str(), slot), ("minecraft:bread", EquipSlot::Offhand));
    }

    /// The failure has to be actionable on its own: name both keys, and show an example that the
    /// parser actually accepts (an example the tool rejects would be the same trap again).
    #[test]
    fn a_missing_item_names_both_keys_and_shows_a_runnable_example() {
        let failure = equip(json!({"to": "offhand"})).expect_err("no item");
        assert_eq!(failure.reason, "invalid_argument");
        assert!(failure.message.contains("\"item\"") && failure.message.contains("\"from\""), "{}", failure.message);
        let example = json!({"item": "minecraft:bread", "to": "offhand"});
        assert!(failure.message.contains("minecraft:bread") && failure.message.contains("offhand"), "{}", failure.message);
        assert!(equip(example).is_ok(), "the example in the message must be runnable as-is");
    }

    /// Every key the failure names has to be listed in the schema, or the model cannot use it.
    #[test]
    fn the_schema_lists_every_key_the_failure_names() {
        let schema = schema("equip");
        let properties = &schema["properties"];
        for key in ["item", "from", "slot", "to"] {
            assert!(properties.get(key).is_some(), "the equip schema does not list {key}: {schema}");
        }
        assert!(schema.get("required").is_none(), "item/from are alternatives: a top-level `required` would reject one of them");
        let any_of = schema["anyOf"].as_array().expect("anyOf");
        assert_eq!(any_of, &vec![json!({"required": ["item"]}), json!({"required": ["from"]})]);
    }
}
