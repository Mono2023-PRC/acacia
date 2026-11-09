//! The MCP tool catalogue: the JSON Schemas returned by `tools/list` and the dispatch behind
//! `tools/call`.
//!
//! Every tool result is a JSON object, wrapped by [`tool_ok`] or [`tool_error`]. Argument errors are
//! tool errors (isError), not protocol errors; only a malformed `tools/call` envelope is a JSON-RPC
//! error.

use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot};

use acacia_bot::interact::Face;
use crate::bot::{self, Command, Reply, Shared};

/// How long a tool waits for the owner task beyond the timeout it was given itself.
const REPLY_SLACK_SECS: u64 = 30;

/// `tools/list` result.
pub fn list() -> Value {
    json!({
        "tools": [
            {
                "name": "netease_connect",
                "description": "Connect the Bedrock bot to the local Shuttler proxy (offline authentication) and wait until it has spawned. Replaces any existing connection.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "address": {
                            "type": "string",
                            "description": "Proxy address to dial.",
                            "default": "127.0.0.1:19132"
                        },
                        "name": {
                            "type": "string",
                            "description": "Offline player name shown to the server.",
                            "default": "AIBot"
                        },
                        "timeout_s": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Seconds allowed for the connection and the spawn.",
                            "default": 60
                        }
                    },
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_status",
                "description": "Current bot snapshot: position, block position, dimension, health, food, saturation and game mode, or {connected:false}. Answers from a shared snapshot, so it stays fast while another tool is running.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_move_to",
                "description": "Walk to a block position with the acacia pathfinder. Returns once arrived, when the timeout passes, or when navigation fails.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer", "description": "Target block x."},
                        "y": {"type": "integer", "description": "Target block y (feet)."},
                        "z": {"type": "integer", "description": "Target block z."},
                        "exact": {
                            "type": "boolean",
                            "description": "true: reach that exact block (Goal::Block); false: any y at that x/z (Goal::XZ).",
                            "default": true
                        },
                        "timeout_s": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Seconds before the move is abandoned.",
                            "default": 60
                        },
                        "allow_dig": {
                            "type": "boolean",
                            "description": "Let the path break blocks in the way.",
                            "default": false
                        },
                        "allow_bridge": {
                            "type": "boolean",
                            "description": "Let the path place blocks to bridge gaps and pillar up.",
                            "default": false
                        }
                    },
                    "required": ["x", "y", "z"],
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_observe",
                "description": "Structured picture of the bot's situation: self (position, view, health, standing_on), held item, inventory totals, nearby entities (kind, distance, direction, hostile), notable blocks (nearest position, distance, direction, count) and what changed since the previous call. Read this before planning instead of guessing.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "radius": {
                            "type": "integer",
                            "minimum": 2,
                            "maximum": 24,
                            "description": "Horizontal scan radius in blocks (vertical is feet-4..feet+4).",
                            "default": 8
                        }
                    },
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_dig",
                "description": "Break the block at a position with the held item. Needs a physics bot and the block within reach; the server confirms the break.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer", "description": "Block x."},
                        "y": {"type": "integer", "description": "Block y."},
                        "z": {"type": "integer", "description": "Block z."}
                    },
                    "required": ["x", "y", "z"],
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_place",
                "description": "Place the held block against a face of a block: the target cell is the neighbour of (x,y,z) in the face direction. Needs a block in hand (dig one first) and the spot within reach; the server confirms the placement.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer", "description": "Block x to click against."},
                        "y": {"type": "integer", "description": "Block y to click against."},
                        "z": {"type": "integer", "description": "Block z to click against."},
                        "face": {
                            "type": "string",
                            "enum": ["up", "down", "north", "south", "east", "west"],
                            "description": "Face of the clicked block the new block goes on: up/down, north=-z, south=+z, west=-x, east=+x."
                        }
                    },
                    "required": ["x", "y", "z"],
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_find",
                "description": "Search the loaded world for blocks by name and list their positions sorted by distance. Use exact names (minecraft:birch_log), a suffix pattern (*_log) or a prefix pattern (minecraft:coal_*).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "block": {
                            "type": "string",
                            "description": "Block name or pattern: exact, *_log (suffix), minecraft:coal_* (prefix)."
                        },
                        "radius": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 64,
                            "description": "Horizontal search radius in blocks.",
                            "default": 16
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 64,
                            "description": "How many matches to return.",
                            "default": 10
                        }
                    },
                    "required": ["block"],
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_recipes",
                "description": "List the recipes the client received whose result item name contains `item` (e.g. pickaxe, wooden_pickaxe, plank). Use it when netease_craft answers no_recipe: it shows the station tag, the grid, and the ingredients the client actually holds.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "item": {
                            "type": "string",
                            "description": "Substring of the result item name, e.g. pickaxe or minecraft:birch_planks."
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 64,
                            "description": "How many recipes to return.",
                            "default": 10
                        }
                    },
                    "required": ["item"],
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_craft",
                "description": "Craft an item with the server's recipe book: the bot's own 2x2 grid by default, or pass a crafting-table position for a 3x3 recipe. Ingredients come from the inventory and the holder refills itself.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "item": {
                            "type": "string",
                            "description": "Item name to craft, e.g. minecraft:crafting_table."
                        },
                        "count": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 64,
                            "description": "How many to end up with.",
                            "default": 1
                        },
                        "mode": {
                            "type": "string",
                            "enum": ["book", "grid"],
                            "description": "grid = lay the ingredients out in the crafting grid by hand (default; NetEase servers accept this); book = click the recipe book, which the NetEase server rejects with status 35 (MismatchedRecipeForInputGridItems).",
                            "default": "grid"
                        },
                        "table": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "minItems": 3,
                            "maxItems": 3,
                            "description": "Optional crafting table block position [x, y, z] for 3x3 recipes."
                        }
                    },
                    "required": ["item"],
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_chat",
                "description": "Send a chat message as the player (not a command).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "description": "Message text."}
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_block",
                "description": "Read one block cell from the client's world view: name, whether it is solid, and whether its chunk is loaded. Use it to check whether a cell is free before placing, and whether a placement actually landed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer"},
                        "y": {"type": "integer"},
                        "z": {"type": "integer"}
                    },
                    "required": ["x", "y", "z"],
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_hold",
                "description": "Put an item into the held hotbar slot (moving it out of the backpack first if needed). Use before netease_place to place a specific block, e.g. a crafting table.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "item": {"type": "string", "description": "Item name, e.g. minecraft:crafting_table."}
                    },
                    "required": ["item"]
                }
            },
            {
                "name": "netease_look",
                "description": "Point the bot's view at yaw/pitch in degrees (0 faces +Z, -90 looks up). The rotation goes out with the next PlayerAuthInput.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "yaw": {"type": "number", "description": "Yaw in degrees; wrapped into -180..180."},
                        "pitch": {"type": "number", "description": "Pitch in degrees, clamped to -90..90."}
                    },
                    "required": ["yaw", "pitch"],
                    "additionalProperties": false
                }
            },
            {
                "name": "netease_disconnect",
                "description": "Close the connection and release the bot. A later netease_connect creates a fresh one.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }
        ]
    })
}

/// `tools/call`: runs one tool. `Err` is a JSON-RPC error (malformed envelope only).
pub async fn call(params: &Value, cmd_tx: &mpsc::Sender<Command>, snapshot: &Shared) -> Result<Value, (i64, String)> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err((-32602, "invalid params: 'name' must be a string".to_owned()));
    };
    let args = match params.get("arguments") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(value @ Value::Object(_)) => value.clone(),
        Some(_) => return Ok(tool_error("'arguments' must be a JSON object")),
    };
    let outcome: Result<Value, String> = match name {
        "netease_connect" => connect(&args, cmd_tx).await,
        "netease_status" => Ok(bot::snapshot_json(&bot::read(snapshot))),
        "netease_move_to" => move_to(&args, cmd_tx).await,
        "netease_observe" => observe(&args, cmd_tx).await,
        "netease_dig" => dig(&args, cmd_tx).await,
        "netease_place" => place(&args, cmd_tx).await,
        "netease_find" => find(&args, cmd_tx).await,
        "netease_craft" => craft(&args, cmd_tx).await,
        "netease_recipes" => recipes(&args, cmd_tx).await,
        "netease_chat" => chat(&args, cmd_tx).await,
        "netease_look" => look(&args, cmd_tx).await,
        "netease_hold" => hold(&args, cmd_tx).await,
        "netease_block" => block(&args, cmd_tx).await,
        "netease_disconnect" => disconnect(cmd_tx).await,
        other => return Ok(tool_error(format!("unknown tool: {other}"))),
    };
    Ok(match outcome {
        Ok(value) => tool_ok(value),
        Err(message) => tool_error(message),
    })
}

async fn connect(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let address = string_arg(args, "address", "127.0.0.1:19132")?;
    let name = string_arg(args, "name", "AIBot")?;
    let timeout_s = timeout_arg(args, 60)?;
    let reply_timeout = Duration::from_secs(timeout_s.saturating_add(REPLY_SLACK_SECS));
    send_command(
        cmd_tx,
        |reply| Command::Connect { address, name, timeout_s, reply },
        reply_timeout,
    )
    .await
}

async fn move_to(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let x = required_i32(args, "x")?;
    let y = required_i32(args, "y")?;
    let z = required_i32(args, "z")?;
    let exact = bool_arg(args, "exact", true)?;
    let timeout_s = timeout_arg(args, 60)?;
    let allow_dig = bool_arg(args, "allow_dig", false)?;
    let allow_bridge = bool_arg(args, "allow_bridge", false)?;
    let reply_timeout = Duration::from_secs(timeout_s.saturating_add(REPLY_SLACK_SECS));
    send_command(
        cmd_tx,
        |reply| Command::MoveTo { x, y, z, exact, timeout_s, allow_dig, allow_bridge, reply },
        reply_timeout,
    )
    .await
}

async fn observe(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let radius = args.get("radius").and_then(Value::as_u64).unwrap_or(8).clamp(2, 24) as u8;
    send_command(cmd_tx, |reply| Command::Observe { radius, reply }, Duration::from_secs(30)).await
}

async fn dig(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let pos = [required_i32(args, "x")?, required_i32(args, "y")?, required_i32(args, "z")?];
    send_command(cmd_tx, |reply| Command::Dig { pos, reply }, Duration::from_secs(180)).await
}

async fn place(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let against = [required_i32(args, "x")?, required_i32(args, "y")?, required_i32(args, "z")?];
    let face = match string_arg(args, "face", "up")?.to_ascii_lowercase().as_str() {
        "up" => Face::Up,
        "down" => Face::Down,
        "north" => Face::North,
        "south" => Face::South,
        "east" => Face::East,
        "west" => Face::West,
        other => return Err(format!("unknown face {other:?}: use up/down/north/south/east/west")),
    };
    send_command(cmd_tx, |reply| Command::Place { against, face, reply }, Duration::from_secs(180)).await
}

async fn find(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let block = args
        .get("block")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "missing required argument: block".to_owned())?;
    let radius = args.get("radius").and_then(Value::as_u64).unwrap_or(16).clamp(1, 64) as u8;
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, 64) as u8;
    send_command(
        cmd_tx,
        |reply| Command::Find { block, radius, limit, reply },
        Duration::from_secs(60),
    )
    .await
}

async fn recipes(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let item = args
        .get("item")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "missing required argument: item".to_owned())?;
    if item.trim().is_empty() {
        return Err("item must not be empty".to_owned());
    }
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, 64) as usize;
    send_command(
        cmd_tx,
        |reply| Command::Recipes { item, limit, reply },
        Duration::from_secs(30),
    )
    .await
}

async fn craft(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let item = args
        .get("item")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "missing required argument: item".to_owned())?;
    if item.trim().is_empty() {
        return Err("item must not be empty".to_owned());
    }
    let count = args.get("count").and_then(Value::as_u64).unwrap_or(1).clamp(1, 64) as u32;
    let grid = match args.get("mode").and_then(Value::as_str).unwrap_or("grid") {
        "book" => false,
        "grid" => true,
        other => return Err(format!("unknown mode {other:?}: use book or grid")),
    };
    let table = match args.get("table") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let array = value
                .as_array()
                .filter(|array| array.len() == 3)
                .ok_or_else(|| "table must be [x, y, z]".to_owned())?;
            let mut pos = [0i32; 3];
            for (index, slot) in array.iter().enumerate() {
                pos[index] = slot
                    .as_i64()
                    .ok_or_else(|| "table coordinates must be integers".to_owned())?
                    as i32;
            }
            Some(pos)
        }
    };
    send_command(
        cmd_tx,
        |reply| Command::Craft { item, count, table, grid, reply },
        Duration::from_secs(240),
    )
    .await
}

async fn chat(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let text = required_string(args, "text")?;
    send_command(cmd_tx, |reply| Command::Chat { text, reply }, Duration::from_secs(30)).await
}

async fn block(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let pos = [required_i32(args, "x")?, required_i32(args, "y")?, required_i32(args, "z")?];
    send_command(cmd_tx, |reply| Command::Block { pos, reply }, Duration::from_secs(30)).await
}

async fn hold(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let item = required_string(args, "item")?;
    send_command(cmd_tx, |reply| Command::Hold { item, reply }, Duration::from_secs(60)).await
}

async fn look(args: &Value, cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    let yaw = required_f32(args, "yaw")?;
    let pitch = required_f32(args, "pitch")?;
    send_command(cmd_tx, |reply| Command::Look { yaw, pitch, reply }, Duration::from_secs(30)).await
}

async fn disconnect(cmd_tx: &mpsc::Sender<Command>) -> Result<Value, String> {
    send_command(cmd_tx, |reply| Command::Disconnect { reply }, Duration::from_secs(30)).await
}

/// Sends a command and waits for its reply; `timeout` guards against a stalled owner task.
async fn send_command(
    cmd_tx: &mpsc::Sender<Command>,
    command: impl FnOnce(Reply) -> Command,
    timeout: Duration,
) -> Result<Value, String> {
    let (reply, reply_rx) = oneshot::channel();
    cmd_tx.send(command(reply)).await.map_err(|_| "the bot task has stopped".to_owned())?;
    match tokio::time::timeout(timeout, reply_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("the bot task dropped the reply".to_owned()),
        Err(_) => Err("timed out waiting for the bot task".to_owned()),
    }
}

/// A successful tool result: the object as both readable text and structured content.
fn tool_ok(value: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": value.to_string()}],
        "structuredContent": value,
        "isError": false,
    })
}

/// A failed tool result the model can read and retry from.
fn tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": message.into()}],
        "isError": true,
    })
}

fn required_i32(args: &Value, key: &str) -> Result<i32, String> {
    let value = args.get(key).ok_or_else(|| format!("missing required argument '{key}'"))?;
    let number = as_i64(value).ok_or_else(|| format!("'{key}' must be an integer"))?;
    i32::try_from(number).map_err(|_| format!("'{key}' is out of range"))
}

fn string_arg(args: &Value, key: &str, default: &str) -> Result<String, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default.to_owned()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("'{key}' must be a string")),
    }
}

fn required_string(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("'{key}' must be a string")),
        None => Err(format!("missing required argument '{key}'")),
    }
}

fn required_f32(args: &Value, key: &str) -> Result<f32, String> {
    let value = args.get(key).ok_or_else(|| format!("missing required argument '{key}'"))?;
    let number = value.as_f64().ok_or_else(|| format!("'{key}' must be a number"))?;
    if !number.is_finite() {
        return Err(format!("'{key}' must be a finite number"));
    }
    Ok(number as f32)
}

fn bool_arg(args: &Value, key: &str, default: bool) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value.as_bool().ok_or_else(|| format!("'{key}' must be a boolean")),
    }
}

fn timeout_arg(args: &Value, default: u64) -> Result<u64, String> {
    match args.get("timeout_s") {
        None | Some(Value::Null) => Ok(default),
        Some(value) => as_i64(value)
            .filter(|seconds| *seconds > 0)
            .map(|seconds| seconds as u64)
            .ok_or_else(|| "'timeout_s' must be a positive integer".to_owned()),
    }
}

/// Integers as JSON numbers; `12.0` is accepted because models sometimes emit it.
fn as_i64(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    let number = value.as_f64()?;
    (number.is_finite() && number.fract() == 0.0).then_some(number as i64)
}
