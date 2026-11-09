//! The shell task: the one place that owns the [`Bot`] and turns tool calls into bot calls.
//!
//! Ownership is the same contract as `crates/acacia-netease/src/bot.rs`: `acacia_bot::Bot` owns its
//! game state and drives its own connection tasks, so exactly one task may touch it. Everything else
//! reads the shared [`ShellState`] (built by [`perceive`]) or sends a [`Command`] down the channel
//! and awaits its reply.
//!
//! Two consequences the model notices:
//! - a `move_to` refreshes the snapshot every tick (via `bot.next()` events), so `status` stays live;
//! - a dropped action future (a tool timeout) leaves no stuck keys: `acacia-bot`'s cancel residue
//!   releases them at the bot's next wait (see `acacia-bot/src/cancel/mod.rs`).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use acacia_bot::client::Client;
use acacia_bot::combat::CombatOpts;
use acacia_bot::interact::{DigFailure, DigOpts, DigStop, Face, ENTITY_REACH};
use acacia_bot::items::SlotRef;
use acacia_bot::pathfind::{CollectError, CollectOpts, Goal, GotoOpts, PlanStats};
use acacia_bot::state::{GameState, ItemStack, Trackers};
use acacia_bot::survival::{Destination, EquipAttempt, EquipMethod, EquipReport};
use acacia_bot::workstation::CraftMode as BotCraftMode;
use acacia_bot::{ActionError, Activity, Bot, BotConfig, BotEvent, Events};
use acacia_world::BlockAccess;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::config::{ChatConfig, SafetyConfig};
use crate::perceive::{self, Detail, Perception};
use crate::tools::{ContainerOp, CraftMode, EquipSlot, Failure, Table, Call};

/// Ticks let through after the spawn so the first path search sees the ground around the bot.
/// Ticks to let the server's position catch up with the client's before the body is called stale (1 s at 20 t/s).
const CLIENT_STALE_TICKS: u32 = 20;
const SETTLE_TICKS: u32 = 10;
/// Slack added on top of a tool's own timeout when waiting for the owner task.
const REPLY_SLACK: Duration = Duration::from_secs(30);

/// Where a command's outcome goes back to the request handler.
pub type Reply = oneshot::Sender<Result<Value, Failure>>;

/// A unit of work for the owner task.
pub enum CommandKind {
    Call(Call),
    Connect { address: String, name: String, timeout_s: u64 },
    Disconnect,
    Shutdown,
}

pub struct Command {
    pub kind: CommandKind,
    /// When the command was enqueued; the reply carries the wait so the trace shows back-pressure.
    pub enqueued: Instant,
    pub reply: Reply,
}

impl Command {
    pub fn new(kind: CommandKind) -> (Self, oneshot::Receiver<Result<Value, Failure>>) {
        let (reply, rx) = oneshot::channel();
        (Self { kind, enqueued: Instant::now(), reply }, rx)
    }

    fn into_reply(self) -> Reply {
        self.reply
    }
}

/// Everything the reader side sees: the last perception, the delta baseline, and what is running.
#[derive(Default)]
pub struct ShellState {
    pub connected: bool,
    pub name: Option<String>,
    pub address: Option<String>,
    pub perception: Option<Perception>,
    /// The snapshot the next delta is taken against.
    pub previous: Option<Perception>,
    /// Whether the model has ever been handed a full `observe` body in this connection. The shell
    /// refreshes the snapshot for its own state right after connecting; that first frame is not a
    /// body the model saw, so a delta is refused until a full one is actually sent.
    pub full_observe_sent: bool,
    /// Scan memo: block position → runtime id, and the tick it was taken.
    pub memo: HashMap<[i32; 3], u32>,
    pub memo_tick: u64,
    pub busy: Option<&'static str>,
    /// `Bot::activity()` as of the last refresh: what the bot is doing, in one word.
    pub activity: Option<&'static str>,
    /// The last height the bot stood at outside a pit: `climb_out` climbs back to it when the model
    /// does not name one. Recalling it here (not from `me.position`, which moves *into* the pit) is
    /// what makes the climb a "back up to where I was" and not a "one step and stop".
    pub last_surface_y: Option<f32>,
    /// `pickup` calls in a row that found nothing on the ground. Two in a row means the server hands
    /// mining products straight into the inventory (the NetEase rental server does), and the tool
    /// says so instead of letting the model hunt for drops that will never exist.
    pub empty_pickups: u32,
    pub last_error: Option<String>,
    pub tick: u64,
}

pub type Shared = Arc<Mutex<ShellState>>;

/// The settings the shell needs at runtime (the rest lives in the agent).
#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// Radius of the snapshot refreshed after each command and each tick.
    pub observe_radius: u8,
    pub chat: ChatConfig,
    pub safety: SafetyConfig,
    /// `false` in `serve`: nothing can ask for confirmation, so protected writes are refused.
    pub interactive: bool,
    /// Print one `[plan] …` line per planning operation to stderr (t149, `ACACIA_PLAN_TRACE`). Off by default:
    /// the line is diagnostic and `serve` shares stderr with whatever supervises it.
    pub plan_trace: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            observe_radius: 8,
            chat: ChatConfig::default(),
            safety: SafetyConfig::default(),
            interactive: false,
            plan_trace: false,
        }
    }
}

/// A handle to the shell task.
#[derive(Clone)]
pub struct ShellHandle {
    cmd_tx: mpsc::Sender<Command>,
    shared: Shared,
    config: ShellConfig,
}

impl ShellHandle {
    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    pub fn config(&self) -> &ShellConfig {
        &self.config
    }

    /// Reads the shared snapshot (never blocks on the bot).
    pub fn state(&self) -> MutexGuard<'_, ShellState> {
        lock(&self.shared)
    }

    pub fn connected(&self) -> bool {
        self.state().connected
    }

    pub async fn connect(&self, address: String, name: String, timeout_s: u64) -> Result<Value, Failure> {
        let (command, rx) = Command::new(CommandKind::Connect { address, name, timeout_s });
        self.send(command, rx, Duration::from_secs(timeout_s.max(1) + REPLY_SLACK.as_secs())).await
    }

    pub async fn disconnect(&self) -> Result<Value, Failure> {
        let (command, rx) = Command::new(CommandKind::Disconnect);
        self.send(command, rx, Duration::from_secs(30)).await
    }

    pub async fn shutdown(&self) {
        let (command, rx) = Command::new(CommandKind::Shutdown);
        let _ = self.send(command, rx, Duration::from_secs(10)).await;
    }

    /// Executes one validated tool call with the tool's own timeout.
    pub async fn call(&self, call: Call) -> Result<Value, Failure> {
        let timeout = crate::tools::spec(call.name()).map_or(Duration::from_secs(60), |spec| spec.timeout);
        let (command, rx) = Command::new(CommandKind::Call(call));
        self.send(command, rx, timeout + REPLY_SLACK).await
    }

    async fn send(
        &self,
        command: Command,
        rx: oneshot::Receiver<Result<Value, Failure>>,
        timeout: Duration,
    ) -> Result<Value, Failure> {
        if self.cmd_tx.send(command).await.is_err() {
            return Err(Failure::disconnected("the shell task has stopped"));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Failure::new("refused", "the shell task dropped the reply")),
            Err(_) => Err(Failure::new("timeout", format!("no reply within {}s", timeout.as_secs()))),
        }
    }
}

/// Spawns the owner task. `bot` may be `None` (nothing connected yet).
pub fn spawn(bot: Option<Bot>, shared: Shared, config: ShellConfig) -> ShellHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    tokio::task::spawn_local(run(cmd_rx, bot, shared.clone(), config.clone()));
    ShellHandle { cmd_tx, shared, config }
}

/// A fresh, empty shared state.
pub fn shared() -> Shared {
    Arc::new(Mutex::new(ShellState::default()))
}

fn lock(shared: &Shared) -> MutexGuard<'_, ShellState> {
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs until the command channel closes or a shutdown arrives; the bot is disconnected on the way out.
async fn run(mut rx: mpsc::Receiver<Command>, mut bot: Option<Bot>, shared: Shared, config: ShellConfig) {
    loop {
        // Both slots are filled from the select arms only: the arms must not touch `bot`, whose
        // future the select is polling.
        let mut command: Option<Option<Command>> = None;
        let mut event: Option<Option<BotEvent>> = None;
        match bot.as_mut() {
            Some(joined) => {
                tokio::select! {
                    incoming = rx.recv() => command = Some(incoming),
                    next = joined.next() => event = Some(next),
                }
            }
            None => command = Some(rx.recv().await),
        }

        let mut lost = false;
        if let Some(event) = event {
            match event {
                Some(BotEvent::Disconnected(reason)) => {
                    eprintln!("[acacia-agent] connection dropped: {reason:?}");
                    lost = true;
                }
                Some(_) => {
                    if let Some(joined) = bot.as_mut() {
                        refresh(joined, &shared, &config);
                    }
                }
                None => lost = true,
            }
        }
        if lost {
            bot = None;
            fail_queued(&mut rx, "disconnected: the connection dropped");
            mark_disconnected(&shared, Some("connection dropped".to_owned()));
            continue;
        }

        match command {
            Some(None) => break,
            Some(Some(command)) => {
                let started = Instant::now();
                let queued_ms = (started - command.enqueued).as_millis() as u64;
                let _ = queued_ms;
                let Command { kind, enqueued: _, reply } = command;
                let shutdown = matches!(kind, CommandKind::Shutdown);
                let outcome = execute(&mut bot, &shared, &config, kind).await;
                let _ = reply.send(outcome);
                if bot.as_ref().and_then(Bot::disconnect_reason).is_some() {
                    if let Some(joined) = bot.as_mut() {
                        let reason = joined.disconnect_reason().map(|reason| format!("connection ended: {reason:?}"));
                        mark_disconnected(&shared, reason);
                    }
                    bot = None;
                }
                if shutdown {
                    break;
                }
            }
            None => {}
        }
    }
    if let Some(mut joined) = bot.take() {
        joined.disconnect().await;
    }
    mark_disconnected(&shared, None);
}

fn mark_disconnected(shared: &Shared, error: Option<String>) {
    let mut state = lock(shared);
    state.connected = false;
    state.busy = None;
    state.perception = None;
    state.previous = None;
    state.memo.clear();
    // A new connection starts without a body the model has seen: the next observe is full.
    state.full_observe_sent = false;
    state.last_error = error;
}

/// Answers every queued command with `why` instead of letting it wait for a connection that is gone.
fn fail_queued(rx: &mut mpsc::Receiver<Command>, why: &str) {
    while let Ok(command) = rx.try_recv() {
        let _ = command.into_reply().send(Err(Failure::disconnected(why.to_owned())));
    }
}

/// Rebuilds the snapshot and stores it (plus the new delta baseline).
fn refresh(bot: &mut Bot, shared: &Shared, config: &ShellConfig) {
    let mut state = lock(shared);
    let mut memo = std::mem::take(&mut state.memo);
    let mut memo_tick = state.memo_tick;
    let perception = perceive::build(bot, config.observe_radius, state.previous.as_ref(), &mut memo, &mut memo_tick);
    state.memo = memo;
    state.memo_tick = memo_tick;
    state.tick = perception.tick;
    state.connected = true;
    state.activity = Some(activity_name(bot.activity()));
    // Standing outside a pit is the baseline a later `climb_out` climbs back to.
    if !bot.in_hole() {
        state.last_surface_y = bot.feet_y();
    }
    state.previous = Some(perception.clone());
    state.perception = Some(perception);
}

/// Handles one command. Returns nothing; the caller ends the loop on `Shutdown`.
async fn execute(
    bot: &mut Option<Bot>,
    shared: &Shared,
    config: &ShellConfig,
    kind: CommandKind,
) -> Result<Value, Failure> {
    match kind {
        CommandKind::Shutdown => {
            if let Some(mut joined) = bot.take() {
                joined.disconnect().await;
            }
            mark_disconnected(shared, None);
            Ok(json!({"shutdown": true}))
        }
        CommandKind::Disconnect => {
            let was_connected = bot.is_some();
            if let Some(mut joined) = bot.take() {
                joined.disconnect().await;
            }
            mark_disconnected(shared, None);
            Ok(json!({"disconnected": true, "was_connected": was_connected}))
        }
        CommandKind::Connect { address, name, timeout_s } => connect(bot, shared, config, address, name, timeout_s).await,
        CommandKind::Call(call) => {
            let is_write = call.is_write();
            if is_write {
                lock(shared).busy = Some(call.name());
            }
            let outcome = call_tool(bot, shared, config, call).await;
            lock(shared).busy = None;
            outcome
        }
    }
}

async fn connect(
    bot: &mut Option<Bot>,
    shared: &Shared,
    config: &ShellConfig,
    address: String,
    name: String,
    timeout_s: u64,
) -> Result<Value, Failure> {
    if let Some(mut previous) = bot.take() {
        previous.disconnect().await;
    }
    mark_disconnected(shared, None);
    {
        let mut state = lock(shared);
        state.name = Some(name.clone());
        state.address = Some(address.clone());
        state.busy = Some("connect");
    }
    let timeout_s = timeout_s.max(1);
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    // Shuttler has authentication disabled, so the bot joins offline.
    let builder = Client::builder(address.clone()).offline(name).login_timeout(Duration::from_secs(timeout_s));
    let bot_config = BotConfig {
        physics: true,
        trackers: Trackers { entities: true, ..Trackers::default() },
        events: Events::HEALTH,
        ..BotConfig::default()
    };
    let joined = tokio::time::timeout_at(deadline, Bot::connect(builder, bot_config)).await;
    // "No server write at all" is a conclusion worth a line of its own, not an absence of logging (t63).
    let mut joined = match joined {
        Err(_) => {
            lock(shared).busy = None;
            let message = format!("timed out after {timeout_s}s dialling {address}");
            mark_disconnected(shared, Some(message.clone()));
            return Err(Failure::disconnected(message));
        }
        Ok(Err(error)) => {
            lock(shared).busy = None;
            let message = format!("could not join {address}: {error}");
            mark_disconnected(shared, Some(message.clone()));
            return Err(Failure::disconnected(message));
        }
        Ok(Ok(joined)) => joined,
    };
    // Wait for the physics simulation (spawn done), then let SETTLE_TICKS pass.
    while !(joined.movement().is_some_and(|movement| movement.is_started()) && joined.block_position().is_some()) {
        if Instant::now() >= deadline {
            joined.disconnect().await;
            let message = format!("joined {address} but did not spawn within {timeout_s}s");
            lock(shared).busy = None;
            mark_disconnected(shared, Some(message.clone()));
            return Err(Failure::disconnected(message));
        }
        if let Err(error) = joined.wait_ticks(1).await {
            joined.disconnect().await;
            lock(shared).busy = None;
            let message = format!("waiting for spawn failed: {error}");
            mark_disconnected(shared, Some(message.clone()));
            return Err(Failure::disconnected(message));
        }
    }
    for _ in 0..SETTLE_TICKS {
        if Instant::now() >= deadline || joined.wait_ticks(1).await.is_err() {
            break;
        }
    }
    if joined.position_confirmations() == 0 && joined.server_confirmed_ticks().is_none() {
        // Say it out loud: "the server has not told us where we are" is the finding t61/t63 were after. acacia-agent
        // has no tracing subscriber, so the line goes to stderr *and* into the connect result (the JSONL trace).
        eprintln!("[position] source=none confirmations=0 (the server has not reported a position yet)");
    }
    *bot = Some(joined);
    lock(shared).busy = None;
    let joined = bot.as_mut().expect("just stored");
    refresh(joined, shared, config);
    let state = lock(shared);
    Ok(json!({
        "connected": true,
        "name": state.name,
        "address": state.address,
        "position": state.perception.as_ref().map(|p| p.me.pos),
        "block_position": state.perception.as_ref().map(|p| p.me.block),
        "dimension": state.perception.as_ref().map(|p| p.me.dim.clone()),
        "health": state.perception.as_ref().map(|p| p.me.health),
        "food": state.perception.as_ref().map(|p| p.me.food),
        // 0 with a null `server_confirmed_ticks` means the server has not told us where the player is yet, so the
        // drift that `status.position_desync` reports is measured against the spawn position (t61/t63).
        "position_confirmations": bot.as_ref().map(|bot| bot.position_confirmations()),
        "server_confirmed_ticks": bot.as_ref().and_then(|bot| bot.server_confirmed_ticks()),
    }))
}

/// Dispatches one validated call.
async fn call_tool(bot: &mut Option<Bot>, shared: &Shared, config: &ShellConfig, call: Call) -> Result<Value, Failure> {
    // Reads that can be answered from the shared snapshot even while an action runs.
    if matches!(call, Call::Status) {
        let state = lock(shared);
        return Ok(status_json(&state, bot.as_ref()));
    }
    if let Call::Observe { radius, detail, delta } = call {
        let Some(joined) = bot.as_mut() else { return Err(not_connected()) };
        let perception = rebuild(joined, shared, radius);
        let full_sent = lock(shared).full_observe_sent;
        let (rendered, full) = perceive::render_observe(&perception, detail, delta, full_sent);
        if full {
            lock(shared).full_observe_sent = true;
        }
        return Ok(rendered);
    }
    let Some(joined) = bot.as_mut() else { return Err(not_connected()) };
    match call {
        Call::Status | Call::Observe { .. } => unreachable!("handled above"),
        Call::Find { block, radius, limit } => find(&*joined, &block, radius, limit),
        Call::Inventory { detail } => inventory(&*joined, detail),
        Call::Block { at, offset } => block_at(&*joined, at, offset),
        Call::Entities { radius, kinds } => entities(&*joined, radius, kinds),
        Call::Recipes { item, limit } => recipes(&*joined, item.as_deref(), limit),
        Call::Capabilities { keyword } => capabilities(&*joined, keyword.as_deref()),
        Call::MoveTo { at, xz, exact, allow_dig, allow_bridge, timeout_s } => {
            let result = move_to(joined, shared, at, xz, exact, allow_dig, allow_bridge, timeout_s).await;
            trace_plan(joined, config.plan_trace, &mut |line| eprintln!("{line}"));
            result
        }
        // t94: both outcomes of `place`/`dig` carry the position-sync snapshot, so a refusal (or a success) is
        // evidence on its own instead of needing a `status` on either side.
        Call::Dig { at, face } => {
            let result = dig(joined, at, face).await;
            // t149: a `dig` far enough to need an approach walked there, and that search is worth a line even
            // when the block broke on the first swing.
            trace_plan(joined, config.plan_trace, &mut |line| eprintln!("{line}"));
            let snapshot = bot_sync(joined);
            synced(result, &snapshot)
        }
        Call::Place { at, face, item, auto, radius } => {
            let result = place(joined, at, face, item, auto, radius).await;
            trace_plan(joined, config.plan_trace, &mut |line| eprintln!("{line}"));
            let snapshot = bot_sync(joined);
            synced(result, &snapshot)
        }
        Call::ClimbOut { surface_y } => climb_out(joined, shared, surface_y).await,
        Call::Attack { target, radius, times, once, allow_dig, timeout_s } => {
            attack(joined, target, radius, times, once, allow_dig, timeout_s).await
        }
        Call::Container { at, op, item, count, slot, keep_open } => {
            container(joined, at, op, item, count, slot, keep_open).await
        }
        Call::Craft { item, count, mode, table } => craft(joined, item, count, mode, table).await,
        Call::Smelt { at, input, count, fuel } => smelt(joined, at, input, count, fuel).await,
        Call::Eat { food } => eat(joined, food).await,
        Call::Equip { item, slot } => equip(joined, item, slot).await,
        Call::Look { yaw, pitch, at } => look(joined, yaw, pitch, at).await,
        Call::Chat { text, recipient } => chat(joined, config, text, recipient),
        Call::Hold { item, slot } => hold(joined, item, slot).await,
        Call::Sleep { at, radius, timeout_s } => sleep(joined, at, radius, timeout_s).await,
        Call::Pickup { id, item, radius, timeout_s } => {
            pickup(joined, shared, id, item, radius, timeout_s, PICKUP_ALLOW_DIG).await
        }
        Call::Drop { slot, item, count } => drop_items(joined, slot, item, count).await,
        Call::Trade { target, offer, times, radius } => trade(joined, target, offer, times, radius).await,
        Call::Trades { target, radius } => trades(joined, target, radius).await,
        Call::Stop => {
            joined.stop_all();
            Ok(json!({"stopped": true, "activity": activity_name(joined.activity())}))
        }
    }
}

/// The one-word name of an [`Activity`] (the field `status` exposes).
fn activity_name(activity: Activity) -> &'static str {
    match activity {
        Activity::Idle => "idle",
        Activity::Walking => "walking",
        Activity::Mining => "mining",
        Activity::UsingItem => "using_item",
        Activity::Sleeping => "sleeping",
        Activity::Riding => "riding",
        Activity::Container => "container",
    }
}

fn not_connected() -> Failure {
    Failure::disconnected("the bot is not connected: connect first")
}

fn status_json(state: &ShellState, bot: Option<&Bot>) -> Value {
    let me = state.perception.as_ref().map(|perception| &perception.me);
    // The body the client simulates against the one the server reports: when these drift apart, every click
    // the bot sends is computed from the wrong place and the server drops it (t27). The relay used to drop
    // NetEase's position correction, so this is the number that says whether a fix landed.
    let desync = bot.and_then(|bot| bot.position_desync());
    json!({
        "connected": state.connected,
        "name": state.name,
        "address": state.address,
        "tick": state.tick,
        "busy": state.busy,
        "activity": state.activity,
        "last_error": state.last_error,
        "position": me.map(|me| me.pos),
        // The live cells, not the perception snapshot: the snapshot is only rebuilt by `refresh`, so a bot being
        // pushed or falling kept reporting the cell it stood in minutes ago (t44: y=29 against the server's 35).
        "block_position": bot.and_then(|bot| bot.block_position()).or_else(|| me.map(|me| me.block)),
        "perception_block_position": me.map(|me| me.block),
        "dimension": me.map(|me| me.dim.clone()),
        "health": me.map(|me| me.health),
        "food": me.map(|me| me.food),
        "saturation": me.map(|me| me.saturation),
        "game_mode": me.map(|me| me.game_mode.clone()),
        "on_ground": me.map(|me| me.on_ground),
        "standing_on": me.and_then(|me| me.standing_on.clone()),
        // How many ticks ago the server last said where the player is, and whether that reference may be used to
        // call the body out of sync at all (t61: vanilla movement is client-authoritative, the server may never
        // re-send it, and a difference from a frozen reference refused every interaction forever).
        "server_confirmed_ticks": bot.and_then(|bot| bot.server_confirmed_ticks()),
        // 0 = the server has never told us where the player is, which is the fact t61/t63 needed: the drift is then
        // measured against the spawn position and proves nothing.
        "position_confirmations": bot.map(|bot| bot.position_confirmations()),
        "server_reference_fresh": bot.is_some_and(|bot| bot.server_reference_is_fresh()),
        // The same snapshot `place`/`dig` attach, folded into one object (t94): one function feeds both, so the
        // acting tools can be read without a `status` next to them.
        "position_desync_blocks": desync.map(|d| round_hundredth(d.blocks)),
        "server_reference": bot.map(bot_sync),
        "late_corrections": bot.and_then(|bot| bot.movement().map(|movement| movement.late_corrections)),
        // The cell the server has the bot in: the only position an arrival or a "did it move" check may use
        // (`MovePlayer` count does not work for that — t41 had 0 while the bot really moved).
        "server_block_position": bot.and_then(|bot| bot.server_block()),
        "client_position": desync.map(|d| d.predicted),
        "position_desync": desync.map(|d| json!({
            "blocks": round_hundredth(d.blocks),
            "manhattan": round_hundredth(d.manhattan),
            "delta": d.delta.map(round_hundredth),
            "limit": acacia_bot::desync::DESYNC_LIMIT,
        })),
        "desync_ticks": bot.map(|bot| bot.desync_ticks()),
    })
}

/// Builds a snapshot at `radius` (through the same memo as the periodic refresh) and stores it.
fn rebuild(bot: &mut Bot, shared: &Shared, radius: u8) -> Perception {
    let mut state = lock(shared);
    let mut memo = std::mem::take(&mut state.memo);
    let mut memo_tick = state.memo_tick;
    let perception = perceive::build(bot, radius, state.previous.as_ref(), &mut memo, &mut memo_tick);
    state.memo = memo;
    state.memo_tick = memo_tick;
    state.tick = perception.tick;
    state.connected = true;
    state.activity = Some(activity_name(bot.activity()));
    // Standing outside a pit is the baseline a later `climb_out` climbs back to.
    if !bot.in_hole() {
        state.last_surface_y = bot.feet_y();
    }
    state.previous = Some(perception.clone());
    state.perception = Some(perception.clone());
    perception
}

// ---------------------------------------------------------------- perception tools

/// Aliases the model reaches for instead of a colour: `bed` covers every `*_bed`.
const BLOCK_ALIASES: [(&str, &str); 8] = [
    ("bed", "*_bed"),
    ("beds", "*_bed"),
    ("wool", "*_wool"),
    ("planks", "*_planks"),
    ("log", "*_log"),
    ("logs", "*_log"),
    ("door", "*_door"),
    ("carpet", "*_carpet"),
];

/// The selector a `find` really uses: an alias expanded (`bed` → `*_bed`), a bare name given its
/// namespace (`stone` → `minecraft:stone`), wildcards kept as written, everything lower-cased.
///
/// t69: the model asked for `minecraft:white_bed`, found nothing, said "checking red bed", and ran out
/// of steps before ever calling `sleep`. A wildcard or an alias must do what the model means.
fn find_selector(block: &str) -> String {
    let raw = block.trim().to_ascii_lowercase();
    if raw.is_empty() {
        return raw;
    }
    if let Some((_, pattern)) = BLOCK_ALIASES.iter().find(|(alias, _)| *alias == raw) {
        return (*pattern).to_owned();
    }
    if raw.contains('*') || raw.contains(':') {
        return raw;
    }
    format!("minecraft:{raw}")
}

/// What a finished walk means for the body (t130).
///
/// `exact:false` asks for a *box* around `at` ([`Goal::Near`], 1.5 blocks), so a bot already standing
/// inside that box satisfies the pathfinder without taking a step. That is an arrival only when the body
/// already stood on the cell that was asked for; every other zero-move finish has to say "nothing moved".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkEnd {
    /// The body moved, or the caller asked for an exact cell and the walk judged it by `block_position`.
    Arrived,
    /// The body never left its cell, and that cell is not the one that was asked for.
    StoodStill,
    /// The body never moved because it was already standing on `at`.
    AlreadyThere,
}

/// The verdict for a finished `exact:false` walk (t130). `exact:true` keeps its own rule — it compares
/// `block_position` with `at` — so it is always [`WalkEnd::Arrived`] here and its branch is untouched.
fn walk_end(start: Option<[i32; 3]>, end: Option<[i32; 3]>, at: Option<[i32; 3]>, exact: bool) -> WalkEnd {
    if exact {
        return WalkEnd::Arrived;
    }
    // An unreadable position proves nothing: never accuse the walk of standing still on a `None`.
    let stood = matches!((start, end), (Some(before), Some(after)) if before == after);
    if !stood {
        return WalkEnd::Arrived;
    }
    if matches!((end, at), (Some(after), Some(goal)) if after == goal) { WalkEnd::AlreadyThere } else { WalkEnd::StoodStill }
}

/// The answer for a `move_to{exact:false}` that finished without the body leaving its cell (t130).
///
/// The reason stays `timeout` — nothing was walked, and the walk spent none of the server's patience —
/// but `details.moved_zero` says the one thing the caller must not misread, and the message spells out
/// that this is *not* an arrival.
fn moved_zero_failure(start: Option<[i32; 3]>, at: Option<[i32; 3]>, budget_s: u64, elapsed: std::time::Duration) -> Failure {
    Failure::new(
        "timeout",
        format!(
            "the walk ended without moving the body: block_position stayed {start:?} ({:.2} s of a {} s budget), so nothing was walked and this is not an arrival — the goal box (Near, 1.5 blocks) already fitted the cell the bot stood in. Ask for a cell at least 2 blocks away, or use exact:true to name the cell to stand on",
            elapsed.as_secs_f32(),
            budget_s
        ),
    )
    .with_details(json!({
        "moved_zero": true,
        "start_block": start,
        "block_position": start,
        "at": at,
        "budget_s": budget_s,
        "elapsed_s": elapsed.as_secs_f32(),
    }))
    .with_hint("一步没走成：这次 move_to 的起点和终点是同一格（exact:false 的 1.5 格框把原地也圈了进去），不能当成\"已经到达\"。要真的走过去，给一个至少 2 格之外的坐标；要站到某一格上就用 exact:true。游标不要据此前进")
}

/// Name matching: exact, `*_log` suffix, `minecraft:coal_*` prefix, or `*inner*`. Case-insensitive.
fn block_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    let (pattern, name) = (pattern.as_str(), name.as_str());
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

fn distance(from: [f32; 3], to: [f32; 3]) -> f32 {
    let (dx, dy, dz) = (from[0] - to[0], from[1] - to[1], from[2] - to[2]);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Which failure a broken `pickup` really was (t69: eleven tries all answered `no_block`, a reason from the
/// *block* branch, for drops 0.2 blocks away).
fn pickup_reason(tracked: bool, error: &str) -> &'static str {
    if !tracked || error.contains("not an item") || error.contains("is not a dropped item") {
        "entity_gone"
    } else if error.contains("no path") || error.contains("unreachable") || error.contains("could not reach") {
        "no_path"
    } else {
        "rejected"
    }
}

/// Distance from the bot's own body to an entity, for `pickup` failures.
fn distance_to(bot: &Bot, runtime_id: u64) -> Option<f32> {
    let entity = bot.state().entities.get(runtime_id)?;
    let feet = bot.movement().and_then(|movement| movement.position())?;
    let p = &entity.position;
    Some(round_hundredth(
        ((feet[0] - p.x).powi(2) + (feet[1] - p.y).powi(2) + (feet[2] - p.z).powi(2)).sqrt(),
    ))
}

/// Whether `pickup` lets the walk dig (t103/t106). On: a block the bot just mined leaves its own
/// tunnel wall between it and the drop, and digging by hand then picking up in a second call is
/// exactly the round trip the model already does when the tool cannot (`t106`).
const PICKUP_ALLOW_DIG: bool = true;

/// The walk settings one `pickup` asks for.
///
/// `allow_dig` comes from the tool (on unless it said otherwise) and the timeouts are the tool's
/// own: `acacia-bot` would silently cap a chase at 30 s while `pickup {timeout_s: 60}` promised 60.
fn collect_opts(allow_dig: bool, timeout_s: u64) -> CollectOpts {
    CollectOpts { allow_dig, timeout: Duration::from_secs(timeout_s), ..CollectOpts::default() }
}

/// The two-step hint a blocked pickup gets (t106): dig the cell that stopped the walk, then ask again
/// for the same id. Same voice as `… is behind … — dig that one first` (t101) and the `no_headroom`
/// hop hint (t105): name the cell, then the one call that fixes it.
fn pickup_blocked_hint(blocked: [i32; 3], runtime_id: u64) -> String {
    let [x, y, z] = blocked;
    format!(
        "挡路的是 [{x}, {y}, {z}] 那格：先 dig [{x}, {y}, {z}] 把它挖开，再 pickup {{id: {runtime_id}}} 一次（别在原地反复拾）"
    )
}

/// The old `pickup` hint, for the failures that are not about a wall.
const PICKUP_GONE_HINT: &str = "掉落物可能已经消失或被别人捡走：重新 entities{} 找一次，必要时先 move_to 靠近";

/// One failed walk to a drop, framed for the model (t106).
///
/// The message keeps the prefix `picking up {id} failed: …` and `acacia-bot`'s own text verbatim —
/// [`pickup_reason`] classifies on that text, so rewording it would silently re-file every failure.
/// What is new is `details.blocked_at` / `details.last_walk` and the two-step hint.
fn pickup_failure(
    tracked: bool,
    runtime_id: u64,
    error: CollectError,
    last_distance: Option<f32>,
) -> Failure {
    let blocked = error.blocked_at();
    let last_walk = error.last_walk().map(str::to_owned);
    let text = error.to_string();
    let reason = pickup_reason(tracked, &text);
    let mut details = json!({"id": runtime_id, "last_distance": last_distance});
    if let Some(cell) = blocked {
        details["blocked_at"] = json!(cell);
    }
    if let Some(walk) = &last_walk {
        details["last_walk"] = json!(walk);
    }
    let hint = match blocked {
        Some(cell) => pickup_blocked_hint(cell, runtime_id),
        None => PICKUP_GONE_HINT.to_owned(),
    };
    Failure::new(reason, format!("picking up {runtime_id} failed: {text}"))
        .with_details(details)
        .with_hint(hint)
}

/// The position-sync snapshot every acting tool reports (t94).
///
/// t91 could only be judged by a `status` inserted right before and right after the action: the `place` that
/// succeeded at desync 4.16 with 0 confirmations carried none of that, so two actions with a call in between left
/// the evidence unrecoverable. The numbers are `status`'s own — `position_desync.blocks`,
/// `position_confirmations`, `server_confirmed_ticks`, `server_reference_fresh` — folded into one object so `place`
/// and `dig` are self-evidencing.
fn sync_snapshot(desync_blocks: Option<f32>, confirmations: u64, confirmed_ticks: Option<u64>, fresh: bool) -> Value {
    json!({
        "position_desync_blocks": desync_blocks.map(round_hundredth),
        "server_reference": {
            "confirmations": confirmations,
            "confirmed_ticks": confirmed_ticks,
            "fresh": fresh,
        },
    })
}

/// [`sync_snapshot`] for a live body. One function, so `status`, `place` and `dig` cannot drift apart.
fn bot_sync(bot: &Bot) -> Value {
    sync_snapshot(
        bot.position_desync().map(|d| d.blocks),
        bot.position_confirmations(),
        bot.server_confirmed_ticks(),
        bot.server_reference_is_fresh(),
    )
}

/// Adds the snapshot to a success body without dropping what the tool already said.
fn attach_sync(mut value: Value, snapshot: &Value) -> Value {
    match (value.as_object_mut(), snapshot.as_object()) {
        (Some(object), Some(fields)) => {
            for (key, field) in fields {
                object.insert(key.clone(), field.clone());
            }
            value
        }
        _ => value,
    }
}

/// Adds the snapshot to a failure's `details`, keeping the tool's own fields (`tried`, `held`, …).
fn attach_sync_failure(mut failure: Failure, snapshot: &Value) -> Failure {
    let details = failure.details.get_or_insert_with(|| json!({}));
    let merged = attach_sync(std::mem::replace(details, Value::Null), snapshot);
    *details = merged;
    failure
}

/// Applies the snapshot to either outcome, so no branch of `place`/`dig` can forget it.
fn synced(result: Result<Value, Failure>, snapshot: &Value) -> Result<Value, Failure> {
    result
        .map(|value| attach_sync(value, snapshot))
        .map_err(|failure| attach_sync_failure(failure, snapshot))
}

/// Every `pickup` success carries the same numbers as `move_to`: the model kept re-calling `status` to get them.
fn pickup_result(
    bot: &Bot,
    before: &std::collections::BTreeMap<String, u32>,
    after: &std::collections::BTreeMap<String, u32>,
    id: Option<u64>,
) -> Value {
    json!({
        "collected": gained(before, after),
        "position": feet_of(bot),
        "id": id,
        "block_position": bot.block_position(),
        "position_desync_blocks": bot.position_desync().map(|d| round_hundredth(d.blocks)),
        "client_position": bot.movement().and_then(|movement| movement.position()),
        "server_reference": if bot.server_reference_is_fresh() { "fresh" } else { "stale" },
    })
}

/// The solid block that keeps the body off `at`, a step exactly one block above `feet`: the cell above it. `None`
/// when `at` is level with or below the feet (nothing to climb) or when the head cell is free — and `None` for an
/// unloaded cell, which is not evidence of a wall.
///
/// This is the agent-side twin of the bot's `no_headroom` verdict ([`acacia_bot::movement::hop_decision`], t102):
/// one rule, one wording, so the pre-flight refusal and the healed walk cannot disagree (t108 P1).
fn no_headroom_over(bot: &Bot, feet: [i32; 3], at: [i32; 3]) -> Option<[i32; 3]> {
    headroom_blocked(feet, at, |pos| solid_at(bot, pos))
}

/// [`no_headroom_over`] without a world, so the rule itself can be tested.
fn headroom_blocked(feet: [i32; 3], at: [i32; 3], solid: impl Fn([i32; 3]) -> bool) -> Option<[i32; 3]> {
    if at[1] != feet[1] + 1 {
        return None;
    }
    let head = [at[0], at[1] + 1, at[2]];
    solid(head).then_some(head)
}

/// Both cells of a failed placement, in one sentence: the support the click needed and the cell the block was going
/// into (t108 P2 — the model reads the second one).
fn no_support_message(at: [i32; 3], target: [i32; 3]) -> String {
    format!("nothing to place against at {at:?} (target {target:?}): {at:?} is not a block, so there is no face to click")
}

fn no_support_hint(at: [i32; 3]) -> String {
    format!("先在 {at:?} 垫一块，或用 place{{auto:true}} 让工具自己挑有支撑的格子")
}


/// Whether the cell is a block (not air, not a liquid's air variant), from the bot's own view.
fn solid_at(bot: &Bot, pos: [i32; 3]) -> bool {
    block_name(bot, pos).is_some_and(|name| name != "minecraft:air" && !name.ends_with("_air"))
}

/// What every `move_to` failure carries: where the walk stood, what it tried, and the last jump it observed. Kept
/// as a function so "a `refused` with `details: null`" cannot come back (t107: the corridor run answered
/// `{reason:"refused"}` after 36 s with nothing to read).
fn walk_details(tried: Vec<&'static str>, stuck_at: Option<[i32; 3]>, target: Option<[i32; 3]>, healed_once: bool, jump_last: Option<&JumpRecord>) -> Value {
    let mut details = json!({
        "tried": tried,
        "stuck_at": stuck_at,
        "target": target,
        "healed_once": healed_once,
    });
    details["jump_last"] = jump_last.map_or(Value::Null, JumpRecord::to_json);
    details
}

/// The search statistics of the refusal this walk produced, merged into `details` as `plan` (t117 → t120).
///
/// A caller that reads `no path` cannot tell *the goal is walled off* from *the search stopped early*, and those ask
/// for opposite next moves. `budget_exhausted` and `target_region_reached` are that difference, so they travel as
/// numbers next to `tried`, never as prose the caller would have to cut out of `message`.
///
/// `None` means **this walk was not refused by the planner** (it timed out, or the terrain answered first), and then
/// no `plan` key is written at all: attaching the previous walk's numbers to this failure would be worse than saying
/// nothing, since the caller would act on a budget that is not the one that ran out.
/// One stderr line naming the search a planning operation just ran (t149): `[plan] nodes=320 cost=12.50`.
///
/// The field order is fixed because these lines are read by eye in a real-server log and diffed between two walks.
/// The engine and its fallback used to be part of the line; with A* as the only planner (t154) there is nothing to name.
fn plan_line(stats: &PlanStats) -> String {
    format!("[plan] nodes={} cost={:.2}", stats.nodes_expanded, stats.cost)
}

/// The one thing the trace needs from the bot, so the take-and-print rule can be tested without a server (t149).
///
/// `take`, not `last`: a search's numbers belong to the operation that ran it, and a reader that does not drain
/// hands the next operation its predecessor's cost (the P2 case: two walks in a row must not report the same
/// `cost`).
trait PlanSource {
    fn take_plan(&self) -> Option<PlanStats>;
}

impl PlanSource for Bot {
    fn take_plan(&self) -> Option<PlanStats> {
        self.take_plan_stats()
    }
}

/// Reports the plan of the operation that just finished, exactly once, and always drains the slot.
///
/// Draining happens whether or not the switch is on: a stale report must not survive into the next operation even
/// in a run that never asks for the line. Nothing is formatted when the switch is off.
fn trace_plan<S: PlanSource + ?Sized>(source: &S, on: bool, emit: &mut dyn FnMut(&str)) {
    let Some(stats) = source.take_plan() else { return };
    if !on {
        return;
    }
    emit(&plan_line(&stats));
}

fn with_plan(mut failure: Failure, stats: Option<PlanStats>) -> Failure {
    let Some(stats) = stats else { return failure };
    let mut details = failure.details.take().unwrap_or_else(|| json!({}));
    details["plan"] = stats.to_json();
    failure.details = Some(details);
    failure
}

/// The feet cell, preferring the cell the client puts the body in over a raw `floor` of the float position: a body
/// settling at y = 2.999 floors to 2, and then the step rule (a goal exactly one block above the feet) stops matching
/// the very cell the walk afterwards stands in — which is one way t109's B1 pre-flight came back empty (t111).
fn stand_feet(bot: &Bot) -> [i32; 3] {
    bot.block_position().unwrap_or_else(|| feet_of(bot))
}

/// The head cell that keeps a two-block body off the step `at`: the single place the "one block up, ceiling solid"
/// rule is applied, with its `exact` gate. `None` for a goal that is not a step or whose ceiling is free.
fn step_headroom(feet: [i32; 3], at: Option<[i32; 3]>, exact: bool, solid: impl Fn([i32; 3]) -> bool) -> Option<[i32; 3]> {
    let at = at?;
    if !exact {
        return None;
    }
    headroom_blocked(feet, at, solid)
}

/// What `move_to` does about the cell over the step it was asked to stand on.
///
/// The distinction that matters is permission, not geometry (t113): `blocked_by_terrain` is the right answer only
/// when the caller did not allow digging. With `allow_dig: true` the same ceiling is work to do, and refusing there
/// made every one-block step under a ceiling unreachable no matter what the caller said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadroomStep {
    /// Not a step, or the head cell is free: path as usual.
    Walk,
    /// The head cell is solid and digging was not allowed: refuse before pathing (t108/t109 behaviour).
    Refuse { head: [i32; 3] },
    /// The head cell is solid and digging was allowed: break it, then walk (t113).
    DigFirst { head: [i32; 3] },
}

fn headroom_step(allow_dig: bool, blocked: Option<[i32; 3]>) -> HeadroomStep {
    match blocked {
        None => HeadroomStep::Walk,
        Some(head) if allow_dig => HeadroomStep::DigFirst { head },
        Some(head) => HeadroomStep::Refuse { head },
    }
}

/// Breaks the one cell that keeps the body off the step, through whatever the caller passes as a digger.
///
/// Kept a separate function with the dig injected so the order -- dig first, then walk -- is something a test can
/// watch (a recorder instead of the bot) rather than a comment: `move_to` passes `Bot::break_block`, the test passes
/// a closure that appends to a list.
async fn dig_the_ceiling<D, Fut>(head: [i32; 3], dig: D) -> Result<(), String>
where
    D: FnOnce([i32; 3]) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    dig(head).await
}

/// The refusal for a step whose head cell cannot be stood through: the message t108/t109 verified on the server, the
/// `details` every `move_to` failure carries (with `step_head`/`blocked_at`/`jump_last`), and -- only when we tried to
/// dig and could not -- what the dig said, so a failed attempt is never mistaken for "digging was not allowed".
fn ceiling_failure_with(
    feet: [i32; 3],
    at: [i32; 3],
    head: [i32; 3],
    solid: impl Fn([i32; 3]) -> bool,
    dig_failed: Option<String>,
) -> Failure {
    let mut failure = Failure::new(
        "blocked_by_terrain",
        format!("cannot stand on {at:?}: {head:?} is in the way — dig that one first"),
    )
    .with_details(enrich_move_details(None, feet, Some(at), true, solid));
    failure.hint = Some(match &dig_failed {
        // Digging was not allowed, so the model is the one who has to break it.
        None => no_headroom_hint(head),
        // We were allowed to dig and could not: say why, and do not repeat "dig that one first" as if nobody tried.
        Some(why) => format!("允许挖路了，但挖不掉 {head:?}（{why}）；先手动 dig 那一格，或换条不走这个台阶的路"),
    });
    if let Some(why) = dig_failed
        && let Some(details) = failure.details.as_mut()
    {
        details["ceiling_dig"] = json!({ "cell": head, "failed": why });
    }
    failure
}

/// Every `move_to` failure's `details`, whichever arm produced it (t111). A walk that recorded something keeps its
/// record; a planner-stage refusal gets the same fields with the place the body stood, plus — when the goal is a step
/// under a solid ceiling — the jump that was *not* sent: `kind: "no_headroom"`, `sent: false`, `pressed_ticks: 0`,
/// `why: "no_room_above_launch"`. Nothing is pressed to produce that record (t109: the window's relay `jump=1` was 0
/// and must stay 0), so it is a statement about the terrain, not about a key.
fn enrich_move_details(
    existing: Option<Value>,
    feet: [i32; 3],
    at: Option<[i32; 3]>,
    exact: bool,
    solid: impl Fn([i32; 3]) -> bool,
) -> Value {
    let mut details = match existing {
        Some(Value::Object(map)) => Value::Object(map),
        _ => walk_details(Vec::new(), Some(feet), at, false, None),
    };
    for key in ["tried", "stuck_at", "target"] {
        // A `null` is as uninformative as a missing key: the wrapper knows where the body stood, so say it.
        if details.get(key).is_none_or(Value::is_null) {
            details[key] = match key {
                "tried" => json!([]),
                "stuck_at" => json!(feet),
                _ => json!(at),
            };
        }
    }
    // The cell over a step, recorded either way: when it is solid this is the obstruction, and when it is free it is
    // the proof that the refusal was not caused by an unloaded view (t111's `step_head`, now filled for every failure
    // so a unit test can see it without a bot; the caller overwrites it with the block name when it has a world).
    if let Some(at) = at
        && exact
        && at[1] == feet[1] + 1
    {
        let cell = [at[0], at[1] + 1, at[2]];
        details["step_head"] = json!({ "cell": cell, "solid": solid(cell) });
    }
    if let Some(head) = step_headroom(feet, at, exact, &solid) {
        details["feet"] = json!(feet);
        details["blocked_at"] = json!(head);
        details["jump_last"] = json!({
            "kind": "no_headroom",
            "sent": false,
            "pressed_ticks": 0,
            "why": "no_room_above_launch",
            "blocked": head,
            "target": at,
        });
    }
    details
}

fn feet_of(bot: &Bot) -> [i32; 3] {
    let feet = bot
        .movement()
        .and_then(|movement| movement.position())
        .unwrap_or_else(|| {
            let player = &bot.state().player;
            [player.position.x, player.position.y, player.position.z]
        });
    [feet[0].floor() as i32, feet[1].floor() as i32, feet[2].floor() as i32]
}

/// The scan is capped: 64 blocks already visits 416k cells (t69 asked for 160, got 64, and read the
/// inevitable miss as "there is no bed anywhere").
const FIND_RADIUS_CAP: u8 = 64;

/// How far below and above the bot's feet a `find` looks. The window is narrow on purpose (a bed on
/// another floor is *not* visited), so a miss has to say where it looked — see [`find_hint`].
const FIND_SCAN_DOWN: i32 = 8;
const FIND_SCAN_UP: i32 = 16;

/// The y band this scan really covered for a bot standing at `py`, as `(low, high)` (t126).
fn find_scan_y(py: i32) -> (i32, i32) {
    (py - FIND_SCAN_DOWN, py + FIND_SCAN_UP)
}

/// `(requested, effective)` for a `find` radius. Pure, so the "never clamp silently" promise is a test.
fn find_scan_radius(requested: u8) -> (u8, u8) {
    let requested = requested.max(1);
    (requested, requested.min(FIND_RADIUS_CAP))
}

fn find(bot: &Bot, block: &str, radius: u8, limit: u8) -> Result<Value, Failure> {
    // `bed` / `*_bed` / `minecraft:*_bed` all mean "any bed"; the answer says which selector ran.
    let selector = find_selector(block);
    let (requested_radius, effective_radius) = find_scan_radius(radius);
    let radius = i32::from(effective_radius);
    let limit = usize::from(limit.clamp(1, 64));
    let [px, py, pz] = feet_of(bot);
    let mut matches: Vec<(f64, &'static str, [i32; 3])> = Vec::new();
    let mut scanned = 0usize;
    if let Some(world) = bot.world() {
        if let (Some(view), Some(registry)) = (world.view(), world.registry()) {
            for x in (px - radius)..=(px + radius) {
                for z in (pz - radius)..=(pz + radius) {
                    for y in (py - FIND_SCAN_DOWN)..=(py + FIND_SCAN_UP) {
                        scanned += 1;
                        let Some(state) = registry.get(view.block(x, y, z)) else { continue };
                        if state.name == "minecraft:air" || !block_matches(&selector, state.name) {
                            continue;
                        }
                        let (dx, dy, dz) = ((x - px) as f64, (y - py) as f64, (z - pz) as f64);
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
                "dir": perceive::bearing((pos[0] - px) as f32, (pos[2] - pz) as f32),
            })
        })
        .collect();
    let mut out = json!({
        "block": block,
        "matched_selector": selector,
        "requested_radius": requested_radius,
        "effective_radius": radius,
        "radius": radius,
        "total": total,
        "matches": results,
        "scanned": scanned,
    });
    if requested_radius > FIND_RADIUS_CAP {
        // Never clamp silently: the miss that cost t69 its whole run was a search range, not a colour.
        out["clamped"] = json!(true);
    }
    if let Some(hint) = find_hint(total, &selector, requested_radius, radius, scanned, py) {
        out["hint"] = json!(hint);
    }
    Ok(out)
}

/// The hint a `find` answer carries, or `None` when the answer needs none (t126).
///
/// A hit says nothing extra unless the radius was cut — only then is the answer narrower than what was
/// asked. A miss says **where it looked**: t123's `find{block:"*_bed", radius:32}` returned
/// `{total:0, scanned:105625}` three times and the report spent paragraphs explaining that the band,
/// not the world, was empty. Pure, so both halves are a test without a bot.
fn find_hint(
    total: usize,
    selector: &str,
    requested_radius: u8,
    radius: i32,
    scanned: usize,
    py: i32,
) -> Option<String> {
    let capped = requested_radius > FIND_RADIUS_CAP;
    if total > 0 {
        return capped.then(|| {
            format!("只在 {FIND_RADIUS_CAP} 格内搜索（请求 {requested_radius} 格）：目标可能更远，先 move_to 靠近再 find")
        });
    }
    let (low, high) = find_scan_y(py);
    let cap_suffix = if capped {
        format!("; the scan was capped at {FIND_RADIUS_CAP} blocks, so move closer before searching again")
    } else {
        String::new()
    };
    if scanned == 0 {
        // Nothing was looked at: saying "0 cells had no bed" would blame a window that was never scanned.
        return Some(format!(
            "nothing matched {selector}: no cells were scanned yet (the world is not loaded) — the window would be y=[{low},{high}], radius={radius}; try again after the next observe"
        ));
    }
    // A miss is where the model used to start guessing colours: name the way out instead.
    Some(format!(
        "nothing matched {selector} in y=[{low},{high}], radius={radius}, {scanned} cells — a block on another floor is outside that window: move_to that y and find again; the block may also be a colour variant — try a wildcard like *_bed, an alias like bed, or a shorter name{cap_suffix}"
    ))
}

fn inventory(bot: &Bot, detail: Detail) -> Result<Value, Failure> {
    let state = bot.state();
    let mut slots: Vec<Value> = Vec::new();
    let mut free = 0usize;
    let mut counts: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for (index, stack) in state.inventory.main.iter().enumerate() {
        if stack.is_empty() {
            free += 1;
            continue;
        }
        let name = state.items.name(stack.network_id).unwrap_or("unknown").to_owned();
        *counts.entry(name.clone()).or_insert(0) += stack.count as u32;
        if detail == Detail::Full {
            slots.push(json!({"slot": index, "hotbar": index < usize::from(acacia_bot::state::Inventory::HOTBAR_SLOTS), "item": name, "count": stack.count}));
        }
    }
    let armor: Vec<Value> = (0..4)
        .map(|index| {
            state.inventory.armor.get(index).filter(|stack| !stack.is_empty()).map_or(Value::Null, |stack| {
                json!({"item": state.items.name(stack.network_id).unwrap_or("unknown"), "count": stack.count})
            })
        })
        .collect();
    let held = item_json(state, state.inventory.held());
    let mut aggregated: Vec<Value> = counts.iter().map(|(item, count)| json!({"item": item, "count": count})).collect();
    aggregated.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
    Ok(json!({
        "held": held,
        "hotbar_slot": state.inventory.selected_hotbar_slot,
        "items": aggregated,
        "armor": armor,
        "offhand": item_json(state, &state.inventory.offhand),
        "free_slots": free,
        "slots": slots,
    }))
}

fn item_json(state: &GameState, stack: &ItemStack) -> Value {
    if stack.is_empty() {
        return Value::Null;
    }
    json!({"item": state.items.name(stack.network_id).unwrap_or("unknown"), "count": stack.count})
}

fn block_at(bot: &Bot, at: Option<[i32; 3]>, offset: Option<[i32; 3]>) -> Result<Value, Failure> {
    let pos = match (at, offset) {
        (Some(at), _) => at,
        (None, Some([dx, dy, dz])) => {
            let [px, py, pz] = feet_of(bot);
            [px + dx, py + dy, pz + dz]
        }
        (None, None) => return Err(Failure::invalid_argument("block needs at [x,y,z] or offset [dx,dy,dz]")),
    };
    let Some(world) = bot.world() else { return Err(Failure::disconnected("the world is not ready")) };
    let (Some(view), Some(registry)) = (world.view(), world.registry()) else {
        return Err(Failure::disconnected("the world is not ready"));
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

fn entities(bot: &Bot, radius: u8, kinds: Option<Vec<String>>) -> Result<Value, Failure> {
    let radius = f32::from(radius.clamp(1, 48));
    let [px, py, pz] = feet_of(bot);
    let feet = [px as f32, py as f32, pz as f32];
    let mut out: Vec<(f32, Value)> = Vec::new();
    for entity in bot.state().entities.iter() {
        if entity.runtime_id == bot.state().player.runtime_entity_id {
            continue;
        }
        if let Some(kinds) = &kinds {
            if !kinds.iter().any(|kind| kind == &entity.kind) {
                continue;
            }
        }
        let (dx, dy, dz) = (entity.position.x - feet[0], entity.position.y - feet[1], entity.position.z - feet[2]);
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        if dist > radius {
            continue;
        }
        out.push((
            dist,
            json!({
                "id": entity.runtime_id,
                "kind": entity.kind,
                "name": entity.username,
                "dist": (dist * 10.0).round() / 10.0,
                "dir": perceive::bearing(dx, dz),
                "dy": (dy * 10.0).round() / 10.0,
                "hostile": perceive::is_hostile(&entity.kind),
                "health": entity.attributes.get("minecraft:health"),
            }),
        ));
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    let results: Vec<Value> = out.into_iter().take(24).map(|(_, value)| value).collect();
    Ok(json!({"radius": radius, "entities": results}))
}

fn recipes(bot: &Bot, item: Option<&str>, limit: usize) -> Result<Value, Failure> {
    let state = bot.state();
    let book = state.recipes.book();
    let want = item.map(str::to_ascii_lowercase);
    let mut out: Vec<Value> = Vec::new();
    let mut total = 0usize;
    for recipe in book.iter() {
        let outputs: Vec<String> = recipe
            .outputs
            .iter()
            .map(|stack| state.item_name(stack).map_or_else(|| format!("#{}", stack.network_id), str::to_owned))
            .collect();
        if let Some(want) = &want {
            if !outputs.iter().any(|name| name.to_ascii_lowercase().contains(want)) {
                continue;
            }
        }
        total += 1;
        if out.len() >= limit {
            continue;
        }
        let inputs: Vec<String> = recipe.inputs.iter().map(|input| format!("{input:?}")).collect();
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
    Ok(json!({"item": item, "book_len": book.len(), "total": total, "recipes": out}))
}

fn capabilities(bot: &Bot, keyword: Option<&str>) -> Result<Value, Failure> {
    let state = bot.state();
    let filter = |name: &str| keyword.is_none_or(|keyword| name.to_ascii_lowercase().contains(&keyword.to_ascii_lowercase()));
    let grid2: Vec<String> = bot
        .craftable(false)
        .iter()
        .filter_map(|stack| state.items.name(stack.network_id).map(str::to_owned))
        .filter(|name| filter(name))
        .collect();
    let grid3: Vec<String> = bot
        .craftable(true)
        .iter()
        .filter_map(|stack| state.items.name(stack.network_id).map(str::to_owned))
        .filter(|name| filter(name))
        .collect();
    let mut stations: Vec<String> = Vec::new();
    let [px, py, pz] = feet_of(bot);
    if let Some(world) = bot.world() {
        if let (Some(view), Some(registry)) = (world.view(), world.registry()) {
            for x in (px - 6)..=(px + 6) {
                for z in (pz - 6)..=(pz + 6) {
                    for y in (py - 3)..=(py + 3) {
                        let Some(block) = registry.get(view.block(x, y, z)) else { continue };
                        if matches!(
                            block.name,
                            "minecraft:crafting_table" | "minecraft:furnace" | "minecraft:anvil" | "minecraft:chest" | "minecraft:barrel"
                        ) {
                            stations.push(format!("{} at [{x},{y},{z}]", block.name));
                        }
                    }
                }
            }
        }
    }
    stations.sort();
    stations.dedup();
    stations.truncate(8);
    let held = item_json(state, state.inventory.held());
    let beds: Vec<Value> = find(bot, "*_bed", 16, 3)
        .ok()
        .and_then(|found| found.get("matches").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let dropped_items = bot.dropped_items().count();
    Ok(json!({
        "held": held,
        "health": state.player.health,
        "food": state.player.hunger,
        "game_mode": format!("{:?}", state.player.game_mode).to_lowercase(),
        "craftable_2x2": grid2,
        "craftable_3x3": grid3,
        "stations": stations,
        "beds": beds,
        "dropped_items": dropped_items,
        "note": "craftable lists the first outputs the recipe book and your inventory afford",
    }))
}

// ---------------------------------------------------------------- action tools

/// The body of the `climb_out` tool, shared with the tests: `already_out` means the bot was not in a
/// pit to begin with, so nothing was dug. `surface_y` is the height that was climbed back to (or
/// asked for) — `from_y`/`to_y` alone cannot tell "climbed the right amount" from "stepped once".
fn climb_json(already_out: bool, surface_y: f32, from_y: Option<f32>, to_y: Option<f32>, steps: u32, landed: bool, out: bool) -> Value {
    json!({
        "already_out": already_out,
        "surface_y": round_tenth(surface_y),
        "from_y": from_y.map(round_tenth),
        "to_y": to_y.map(round_tenth),
        "steps": steps,
        "landed": landed,
        "out": out,
    })
}

/// Which height a climb aims for: the caller's `surface_y`, else the last height the bot stood at
/// outside a pit, else three blocks above the feet (a typical pit). Pure, so the choice is testable
/// without a server.
fn climb_target(explicit: Option<i32>, baseline: Option<f32>, feet_y: f32) -> f32 {
    match explicit {
        Some(y) => y as f32,
        None => baseline.unwrap_or(feet_y + 3.0),
    }
}

fn on_ground_now(bot: &Bot) -> bool {
    bot.movement().is_some_and(|movement| movement.on_ground())
}

/// Climbs back to the surface height; returns `(climbed, surface_y)` for `move_to`'s read-back.
async fn climb_back(bot: &mut Bot, shared: &Shared) -> (bool, Option<f32>) {
    let from_y = bot.feet_y().unwrap_or(0.0);
    let baseline = lock(shared).last_surface_y;
    let target = climb_target(None, baseline, from_y);
    let climbed = bot.climb_out_to(target, acacia_bot::escape::DEFAULT_STEPS).await.is_ok();
    if climbed && on_ground_now(bot) && !bot.in_hole() {
        if let Some(y) = bot.feet_y() {
            lock(shared).last_surface_y = Some(y);
        }
    }
    (climbed, Some(target))
}

/// `climb_out`: climb back up to `surface_y` (or the recorded one), reporting the landing.
async fn climb_out(bot: &mut Bot, shared: &Shared, surface_y: Option<i32>) -> Result<Value, Failure> {
    let from_y = bot.feet_y();
    let baseline = lock(shared).last_surface_y;
    let target = climb_target(surface_y, baseline, from_y.unwrap_or(0.0));
    if !bot.in_hole() {
        return Ok(climb_json(true, target, from_y, from_y, 0, on_ground_now(bot), true));
    }
    let steps = bot
        .climb_out_to(target, acacia_bot::escape::DEFAULT_STEPS)
        .await
        .map_err(|error| action_failure(bot, &error))?;
    let to_y = bot.feet_y();
    let landed = on_ground_now(bot);
    let out = !bot.in_hole();
    if landed && out {
        if let Some(y) = to_y {
            lock(shared).last_surface_y = Some(y);
        }
    }
    Ok(climb_json(false, target, from_y, to_y, steps, landed, out))
}

/// Maps a failure to the taxonomy, then replaces the generic hint with one built from what the bot
/// can see: which block hid the target, which neighbour cell is free.
fn action_failure(bot: &Bot, error: &ActionError) -> Failure {
    let mut failure = crate::tools::result::from_action_error(error);
    // `wait_landing` saying "standing on air … physics did not move it" means the bot is over a hole:
    // no amount of digging in place fixes that, climbing out does.
    if failure.message.contains("standing on air") || failure.message.contains("physics did not move it") {
        return failure.with_hint("脚下没有落点（站在空气上）：先 climb_out 爬出坑，再 dig/place");
    }
    match failure.reason {
        "not_visible" => failure.hint = Some(occlusion_hint(bot, &failure.message)),
        "no_space" | "refused" => {
            if let Some(hint) = free_cell_hint(bot) {
                failure.hint = Some(hint);
            }
        }
        _ => {}
    }
    failure
}

/// One line naming what stands between the eye and the target.
///
/// acacia-bot says *what* was hidden but not *by what*; the block on the segment eye → target is
/// what the player would see, and it is what the model needs to dig out or step around.
fn occlusion_hint(bot: &Bot, message: &str) -> String {
    let Some(target) = first_position(message) else {
        return "看不到目标：换一个面对着它，或先挖开相邻格（没算出是哪一格挡住的）".to_owned();
    };
    match first_block_on_the_way(bot, [target[0] as f32 + 0.5, target[1] as f32 + 0.5, target[2] as f32 + 0.5]) {
        Some((name, pos)) => format!("被 {name}（[{},{},{}]）挡住：换个面点目标，或先挖开它", pos[0], pos[1], pos[2]),
        None => "看不见目标但没找到遮挡块（可能刚好在边缘）：后退一格再试，或换个面".to_owned(),
    }
}

/// A same-level neighbour that is air with something solid under it — where a placement can go when
/// the requested cell is refused.
fn free_cell_hint(bot: &Bot) -> Option<String> {
    let [x, y, z] = feet_of(bot);
    for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        let cell = [x + dx, y, z + dz];
        if block_name(bot, cell).is_some_and(|name| name == "minecraft:air")
            && block_name(bot, [cell[0], cell[1] + 1, cell[2]]).is_some_and(|name| name == "minecraft:air")
            && block_name(bot, [cell[0], cell[1] - 1, cell[2]]).is_some_and(|name| name != "minecraft:air")
        {
            return Some(format!("换个位置：[{},{},{}] 是同层空位、下方有支撑", cell[0], cell[1], cell[2]));
        }
    }
    Some("换个方向或换一格（同层没找到现成空位）：先 dig 掉挡路的方块再 place".to_owned())
}

/// The first `[x, y, z]` in a message like `block [1, 2, 3] is behind other blocks` or
/// `[North] of [10, 64, -2] is hidden from the eye` (the face name is skipped).
fn first_position(message: &str) -> Option<[i32; 3]> {
    let mut rest = message;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let close = after.find(']')?;
        let parts: Vec<&str> = after[..close].split(',').collect();
        if parts.len() == 3 {
            let mut out = [0i32; 3];
            let mut parsed = true;
            for (index, part) in parts.iter().enumerate() {
                match part.trim().parse() {
                    Ok(value) => out[index] = value,
                    Err(_) => {
                        parsed = false;
                        break;
                    }
                }
            }
            if parsed {
                return Some(out);
            }
        }
        rest = &after[close..];
    }
    None
}

/// The first solid block strictly between the eye and `target` (0.25-block sampling).
fn first_block_on_the_way(bot: &Bot, target: [f32; 3]) -> Option<(String, [i32; 3])> {
    let eye = bot.eye_position();
    let steps = 24;
    let mut last: Option<[i32; 3]> = None;
    for step in 1..steps {
        let t = step as f32 / steps as f32;
        let point = [eye[0] + (target[0] - eye[0]) * t, eye[1] + (target[1] - eye[1]) * t, eye[2] + (target[2] - eye[2]) * t];
        let cell = [point[0].floor() as i32, point[1].floor() as i32, point[2].floor() as i32];
        if Some(cell) == last {
            continue;
        }
        last = Some(cell);
        if cell == [target[0].floor() as i32, target[1].floor() as i32, target[2].floor() as i32] {
            continue;
        }
        if let Some(name) = block_name(bot, cell) {
            if name != "minecraft:air" && !name.ends_with("_air") {
                return Some((name, cell));
            }
        }
    }
    None
}

/// `move_to`, plus the promise that a failed walk always says what it saw (t111). The enrichment lives in the
/// wrapper rather than in each `return Err`: t109's B1 called `move_to{exact:true}` at a step with a solid ceiling
/// and the answer was a planner-stage `refused` with **no `details` key at all**, so one exit was reached that the
/// body's tail arm did not cover. A wrapper cannot be bypassed by a new early return.
async fn move_to(
    bot: &mut Bot,
    shared: &Shared,
    at: Option<[i32; 3]>,
    xz: Option<[i32; 2]>,
    exact: bool,
    allow_dig: bool,
    allow_bridge: bool,
    timeout_s: u64,
) -> Result<Value, Failure> {
    let outcome = move_to_inner(bot, shared, at, xz, exact, allow_dig, allow_bridge, timeout_s).await;
    let Err(mut failure) = outcome else { return outcome };
    let feet = stand_feet(bot);
    let head = step_headroom(feet, at, exact, |pos| solid_at(bot, pos));
    failure.details = Some(enrich_move_details(failure.details.take(), feet, at, exact, |pos| {
        solid_at(bot, pos)
    }));
    // Whatever the verdict, a goal that is a one-block step says what the head cell looked like from here: if the
    // rule ever misses again on the machine, the record answers *why* (`block` null = the cell was not in the view,
    // `solid` false = the body would fit) instead of costing another run to guess (t111).
    if let Some(at) = at
        && at[1] == feet[1] + 1
        && let Some(details) = failure.details.as_mut()
    {
        let cell = [at[0], at[1] + 1, at[2]];
        details["step_head"] = json!({
            "cell": cell,
            "block": block_name(bot, cell),
            "solid": solid_at(bot, cell),
        });
    }
    if let Some(head) = head
        && !failure.message.contains(&format!("{head:?}"))
    {
        failure.message = format!("{} — {head:?} is in the way — dig that one first", failure.message);
        if failure.hint.is_none() {
            failure.hint = Some(no_headroom_hint(head));
        }
    }
    Err(failure)
}

async fn move_to_inner(
    bot: &mut Bot,
    shared: &Shared,
    at: Option<[i32; 3]>,
    xz: Option<[i32; 2]>,
    exact: bool,
    allow_dig: bool,
    allow_bridge: bool,
    timeout_s: u64,
) -> Result<Value, Failure> {
    let goal = match (at, xz, exact) {
        (Some(at), _, true) => Goal::Block(at),
        (Some(at), _, false) => Goal::Near(at, 1.5),
        (None, Some([x, z]), _) => Goal::XZ(x, z),
        (None, None, _) => return Err(Failure::invalid_argument("move_to needs at or xz")),
    };
    let mut opts = GotoOpts::default();
    opts.search.allow_dig = allow_dig;
    opts.search.allow_bridge = allow_bridge;
    // Climb back to the surface first: a bot standing in a pit pathfinds badly (t19), and the old
    // "stop as soon as one side opens" climb left it three blocks lower every run (t35).
    let (climbed_out, surface_y) = if bot.in_hole() { climb_back(bot, shared).await } else { (false, None) };
    // t108 P1: a step one block above the feet whose head cell is solid can never be climbed onto — the body is two
    // blocks tall. The pathfinder happily emits the jump anyway (t107: 131 `jump=1` packets in three windows with
    // `rise_ground=+0.000`), so say it *before* pathing, in the same voice and with the same `blocked_by_terrain`
    // the heal path uses: one situation, one reason, and the cell to dig first in `details`.
    // t113: the same rule must not refuse when the caller said digging is allowed. `allow_dig: true` is an explicit
    // "you may break terrain to get there", so a solid ceiling is a step in the job instead of the end of it: break
    // that cell and walk on. Only a dig that cannot be done (out of reach, not visible, server refused, still solid
    // after the echo) falls back to the old refusal, so nothing is silently swallowed. `allow_dig: false` keeps the
    // message, the details and the hint it had in t108/t109, character for character.
    if exact && let Some(at) = at {
        let feet = stand_feet(bot);
        let blocked = no_headroom_over(bot, feet, at);
        match headroom_step(allow_dig, blocked) {
            HeadroomStep::Walk => {}
            HeadroomStep::Refuse { head } => {
                return Err(ceiling_failure_with(feet, at, head, |pos| solid_at(bot, pos), None));
            }
            HeadroomStep::DigFirst { head } => {
                let dug = dig_the_ceiling(head, |cell| {
                    let bot = &mut *bot;
                    async move { bot.break_block(cell).await.map_err(|error| error.to_string()) }
                })
                .await;
                if let Err(why) = dug {
                    return Err(ceiling_failure_with(feet, at, head, |pos| solid_at(bot, pos), Some(why)));
                }
                // The echo can lag (t104: seven to eight seconds on this server) or the block can be one we may not
                // break; if it is *still* there, the step is still impossible and the model has to hear it.
                if let Some(still) = no_headroom_over(bot, stand_feet(bot), at) {
                    let why = format!("{still:?} is still solid after digging it");
                    return Err(ceiling_failure_with(
                        stand_feet(bot),
                        at,
                        still,
                        |pos| solid_at(bot, pos),
                        Some(why),
                    ));
                }
            }
        }
    }
    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_s.clamp(1, 600));
    let jumps_before = bot.jump_count().unwrap_or(0);
    // t130: where the body stood when the walk started, so "the walk said it was done" can be checked
    // against "the body actually went somewhere".
    let start_block = bot.block_position();
    let walk = walk_supervised(bot, goal.clone(), opts.clone(), at.or(xz.map(|[x, z]| [x, 0, z])), allow_dig, deadline).await;
    let outcome = walk.outcome;
    // t89: the jump record survives progress, so every result — success, timeout, blocked — can carry it.
    //
    // t112: `walk.jump_last` is written by the *heal* path only, so a step climbed by the pathfinder's own `Ascend`
    // walked and arrived with no record at all — nine runs of "no `jump_ok` sample" were this, not a body that never
    // jumped (the relay saw `jump=1` and `rise_ground=+1.200` in the same second). The body-level recorder in
    // `acacia-bot` sees every emitter, so take its record when this walk produced none and that jump is this walk's.
    let jump_last = walk.jump_last.clone().or_else(|| {
        let fresh = bot.jump_count().is_some_and(|count| count > jumps_before);
        fresh.then(|| bot.last_jump()).flatten().map(JumpRecord::from_body)
    });
    let with_jump = |value: Value| -> Value { attach_jump(value, jump_last.as_ref()) };
    let blocks_short = walk
        .stuck_at
        .map(|b| b.iter().zip(at.unwrap_or(b)).map(|(a, b)| (a - b).abs()).sum::<i32>())
        .unwrap_or(0);
    let jumped = jump_last.as_ref().map(|jump| format!("; {}", jump.attempt.describe())).unwrap_or_default();
    if walk.timed_out {
        // The chunk deadline came first: the body was not refused by the terrain, it ran out of time (t88: a run
        // whose jump worked answered `refused` + "not possible: timed out" with `details: null`, so 30 s of
        // evidence read as nothing happened). `walk_timeout_failure` carries where it stood, what it tried and
        // what the last jump did.
        return Err(with_plan(
            walk_timeout_failure(
                timeout_s.clamp(1, 600),
                walk.tried,
                walk.stuck_at,
                at,
                jump_last.as_ref(),
                walk.healed_once,
            ),
            bot.last_plan_stats(),
        ));
    }
    if outcome.is_err() && (!walk.tried.is_empty() || jump_last.is_some()) {
        // Found a route but could not walk it: say so instead of reporting a plain timeout (t68: six `move_to`s
        // in a row spent 30 s each standing still and the model read it as "arrived"). The jump is named with the
        // observation that came back, so "never sent" and "sent, no lift" cannot hide behind a bare `tried` (t82).
        let mut details = walk_details(walk.tried.clone(), walk.stuck_at, at, walk.healed_once, jump_last.as_ref());
        details["blocks_short"] = json!(blocks_short);
        details["attempts"] = json!(walk.attempts);
        return Err(with_plan(Failure::new(
            "blocked_by_terrain",
            format!(
                "the bot stood at {:?} for {} attempts without progress, {} blocks short of {at:?}; tried {:?}{jumped}",
                walk.stuck_at,
                walk.attempts,
                blocks_short,
                walk.tried,
            ),
        )
        .with_details(details)
        .with_hint(blocked_by_terrain_hint(walk.healed_once, jump_last.as_ref())), bot.last_plan_stats()));
    }
    let block_position = bot.block_position();
    let feet = bot.movement().and_then(|movement| movement.position());
    match outcome {
        Ok(()) => {
            // t130: `exact:false` walks to a *box* around `at` (Near, 1.5 blocks), so a bot already standing
            // inside that box "arrives" without taking a step. t127 saw 46 `{arrived: true, elapsed_s: 0.0004}`
            // answers that were one cell short, and the caller's cursor never moved again for the rest of the
            // run. A finished walk that never changed `block_position` says so instead of claiming arrival;
            // only standing on the asked-for cell itself counts as already being there.
            if walk_end(start_block, block_position, at, exact) == WalkEnd::StoodStill {
                bot.stop_pathing();
                return Err(with_plan(
                    moved_zero_failure(start_block, at, timeout_s.clamp(1, 600), started.elapsed()),
                    bot.last_plan_stats(),
                ));
            }
            // The walk's own success is the *prediction's*: t37 had `arrived: true` while the server never moved
            // the bot (predicted +5 blocks, zero corrections). Judge the arrival by the server's position.
            let desync = bot.position_desync();
            let server_block = bot.server_block();
            // The server's position lags the client's walking by the drift: a 1-block box refused a walk that had
            // really covered 2 blocks (t41, desync 1.07). Let the box grow with the drift, up to a cap.
            let reach = 1.0 + desync.map(|d| d.blocks.min(acacia_bot::desync::ARRIVAL_SLACK)).unwrap_or(0.0);
            let server_arrived = inside_goal(server_block, at, xz, exact, reach);
            // The client's own cell must agree too: a stalled prediction (t44: y=29 while the server had 35) made
            // `arrived` true for a walk that never happened.
            let client_block = bot.block_position();
            let client_arrived = inside_goal(client_block, at, xz, exact, reach);
            if client_arrived && !server_arrived && !bot.server_reference_is_fresh() {
                // The server has not told us where the player is for over FRESH_REFERENCE_TICKS, so `server_block`
                // is a snapshot from some earlier moment (often spawn). The difference proves nothing — the walk
                // itself is client-authoritative and the server accepted it (t61: every interaction at the old
                // reference position succeeded, which it could not have from 2.5 blocks away).
                bot.stop_pathing();
                return Ok(with_jump(json!({
                    "arrived": true,
                    "client_arrived": true,
                    "server_arrived": false,
                    "server_reference": "stale",
                    "server_confirmed_ticks": bot.server_confirmed_ticks(),
                    "settled_ticks": 0,
                    "server_block_position": bot.server_block(),
                    "position": feet,
                    "block_position": client_block,
                    "elapsed_s": started.elapsed().as_secs_f32(),
                })));
            }
            if client_arrived && !server_arrived {
                // The server's position trails the client's by a few ticks (t45: it caught up inside a second).
                // Give it a settle window before calling the body stale.
                // A full second: t48 still reported `client_stale` for a three block walk the server caught up
                // with in well under that (t45 measured about a second at worst).
                let mut settled = false;
                let mut waited = 0u32;
                for _ in 0..CLIENT_STALE_TICKS {
                    waited += 1;
                    if bot.wait_ticks(1).await.is_err() {
                        break;
                    }
                    let Some(b) = bot.server_block() else { break };
                    // The drift shrinks as the server catches up, so the box grows and shrinks with it.
                    let reach_now = 1.0
                        + bot.position_desync().map(|d| d.blocks.min(acacia_bot::desync::ARRIVAL_SLACK)).unwrap_or(0.0);
                    let inside = inside_goal(Some(b), at, xz, exact, reach_now);
                    if inside {
                        settled = true;
                        break;
                    }
                }
                if settled {
                    bot.stop_pathing();
                    return Ok(with_jump(json!({
                        "arrived": true,
                        "client_arrived": true,
                        "server_arrived": true,
                        "settled_ticks": waited,
                        "server_block_position": bot.server_block(),
                        "position": feet,
                        "block_position": bot.block_position(),
                        "elapsed_s": started.elapsed().as_secs_f32(),
                    })));
                }
                bot.stop_pathing();
                return Err(Failure::new(
                    "client_stale",
                    format!(
                        "the client finished the walk at {client_block:?} but the server still has the bot at {:?} after {waited} ticks ({:.2} blocks apart, x {} y {} z {}): the body is not following the server",
                        bot.server_block(),
                        desync.map(|d| d.blocks).unwrap_or(0.0),
                        at.map_or("-".to_owned(), |at| (client_block.map_or(0, |b| (b[0] - at[0]).abs())).to_string()),
                        at.map_or("-".to_owned(), |at| (client_block.map_or(0, |b| (b[1] - at[1]).abs())).to_string()),
                        at.map_or("-".to_owned(), |at| (client_block.map_or(0, |b| (b[2] - at[2]).abs())).to_string()),
                    ),
                )
                .with_details(json!({
                    "settled_ticks": waited,
                    "client_block": client_block,
                    "server_block": bot.server_block(),
                    "position_desync_blocks": desync.map(|d| round_hundredth(d.blocks)),
                }))
                .with_hint("客户端位置没跟上服务端：看 status.block_position 与 server_block_position 是否一致，别再继续挖/放"));
            }
            if !server_arrived {
                bot.stop_pathing();
                return Err(Failure::new(
                    "no_progress",
                    format!(
                        "the client reached the goal but the server still has the bot at {server_block:?} (client {} blocks away: predicted {:?} vs server {:?})",
                        desync.map(|d| round_hundredth(d.blocks)).unwrap_or(0.0),
                        desync.map(|d| d.predicted),
                        desync.map(|d| d.server),
                    ),
                )
                .with_hint("服务端没接受这次移动：看 status.position_desync 是否 >1（客户端预测与服务端位置失同步），先不要继续挖/放，等位置同步或换一条路"));
            }
            Ok(with_jump(json!({
            "arrived": client_arrived && server_arrived,
            "client_arrived": client_arrived,
            "server_arrived": server_arrived,
            "server_reference": if bot.server_reference_is_fresh() { "fresh" } else { "stale" },
            "position_desync_blocks": bot.position_desync().map(|d| round_hundredth(d.blocks)),
            "client_position": bot.movement().and_then(|movement| movement.position()),
            "server_confirmed_ticks": bot.server_confirmed_ticks(),
            "settled_ticks": 0,
            "server_block_position": server_block,
            "position_desync_blocks": desync.map(|d| round_hundredth(d.blocks)),
            "position": feet,
            "block_position": block_position,
            "climbed_out": climbed_out,
            "surface_y": surface_y,
            "elapsed_s": started.elapsed().as_secs_f32(),
            })))
        }
        Err(error) => {
            bot.stop_pathing();
            // `no path` for a 3-block walk to an occupied cell is usually the goal, not the terrain: a `Near` box
            // that only fits the blocked cell has no standing spot. Retry once around the same point with a wider
            // box, which is what "walk as close as you can" means.
            if let ActionError::Rejected(message) | ActionError::NotPossible(message) = &error {
                if message.contains("no path") {
                    let wider = match goal {
                        Goal::Near(at, radius) if radius < 3.0 => Goal::Near(at, 3.0),
                        Goal::Block(at) => Goal::Near(at, 2.0),
                        other => other,
                    };
                    let retry = tokio::time::timeout_at(deadline, bot.goto_with(wider.clone(), opts)).await;
                    if let Ok(Ok(())) = retry {
                        let server_block = bot.server_block();
                        return Ok(with_jump(json!({
                            "arrived": true,
                            "client_arrived": true,
                            "server_arrived": server_block.is_some(),
                            "widened_goal": format!("{wider:?}"),
                            "server_block_position": server_block,
                            "position": bot.movement().and_then(|movement| movement.position()),
                            "block_position": bot.block_position(),
                            "elapsed_s": started.elapsed().as_secs_f32(),
                        })));
                    }
                }
            }
            let mut failure = action_failure(bot, &error);
            if bot.in_hole() {
                failure.hint = Some("还在坑里：先 climb_out 爬出来，再 move_to".to_owned());
            }
            // t108 P1: whatever the wording, a failed walk says what it saw. `no path` / `refused` used to answer
            // with `details: null`, so a 36 s corridor attempt (`no_headroom`, `pressed_ticks`) read as nothing.
            failure.details = Some(walk_details(walk.tried, walk.stuck_at, at, walk.healed_once, jump_last.as_ref()));
            Err(with_plan(failure, bot.last_plan_stats()))
        }
    }
}

async fn dig(bot: &mut Bot, at: [i32; 3], _face: Face) -> Result<Value, Failure> {
    let name = block_name(bot, at);
    // t143: a target further than the arm walks in first (the walk only ever aims at the ring of cells
    // around the block, so it cannot break the support under the bot or dig on its way in).
    match bot.dig_at(at, DigOpts::default()).await {
        Ok(report) => Ok(json!({"broke": true, "pos": at, "block": name, "approach": report.json()})),
        Err(DigFailure::Break(error)) => Err(action_failure(bot, &error)),
        Err(DigFailure::Walk(stop)) => Err(dig_failure(stop)),
    }
}

/// A walk that did not end with the block in reach: three reasons, each with the numbers behind it.
///
/// The three are mutually exclusive by construction (see `DigStop`): `no_path` is "there is no way
/// there", `blocked_by_terrain` is "there is a way and the body could not walk it", `out_of_reach` is
/// "we got as close as the route allowed and it is still too far".
fn dig_failure(stop: DigStop) -> Failure {
    let reason = stop.reason().unwrap_or("no_path");
    let mut failure = Failure::new(reason, stop.describe());
    failure.details = Some(stop.details());
    failure.hint = Some(match reason {
        "no_path" => "没有路走到那一格旁边：换一个目标，或先 move_to 到更近的开阔处".to_string(),
        "blocked_by_terrain" => "有方向能到但走不通：先 dig 掉挡路的方块，或者绕过去".to_string(),
        _ => "走到最近的可站格仍然够不着：站到那一格旁边再 dig（细节里有距离与 reach）".to_string(),
    });
    failure
}

async fn place(
    bot: &mut Bot,
    at: Option<[i32; 3]>,
    face: Face,
    item: Option<String>,
    auto: bool,
    radius: f32,
) -> Result<Value, Failure> {
    // `auto:true` without `at` is the only way to reach the candidate branch; `at` alone is a direct
    // placement, and both set means the explicit cell wins.
    let _ = auto;
    let mut swapped_with = Value::Null;
    if let Some(item) = &item {
        let held = hold_item(bot, item, None).await?;
        // The hotbar may have been full and a slot swapped for us: tell the model, so a "why is my
        // pickaxe gone" is answered by the tool result and not by another `inventory` call.
        if let Some(slot) = held.get("swapped_with") {
            if !slot.is_null() {
                swapped_with = slot.clone();
            }
        }
    }
    let held = bot.state().inventory.held().clone();
    if held.is_empty() {
        return Err(Failure::new("no_item", "nothing held: hold a block first"));
    }
    // t108 P2: the support cell the caller named may be air (nothing to click) while a perfectly good cell is one
    // step away. Keep the refusal, try the same candidates `auto` uses, and only fail when both are spent.
    let mut fell_back_from: Option<[i32; 3]> = None;
    let mut first_failure: Option<Failure> = None;
    if let Some(at) = at {
        match bot.place_block(at, face).await {
            Ok(()) => {
                return Ok(json!({
                    "placed": true,
                    "against": at,
                    "target": face.adjacent(at),
                    "held": item_json(bot.state(), &held),
                    "swapped_with": swapped_with,
                }));
            }
            Err(error) => {
                let mut failure = place_failure(bot, &error);
                if failure.message.contains("nothing to place against") {
                    // Name both cells: the bot named the cell it could not click, which is the *support* the block
                    // would have leaned on, while the model thinks of the cell the block goes into (t107 F3: two
                    // builds read `nothing to place against at [150030,2,149829]` and gave up on the wall).
                    failure.message = no_support_message(at, face.adjacent(at));
                    failure.hint = Some(no_support_hint(at));
                }
                fell_back_from = Some(at);
                first_failure = Some(failure);
            }
        }
    }
    // `auto:true`: the model should not have to guess offsets one at a time (t20: six tries, t45:
    // six rejected offsets). Pick the free cells itself and report what each attempt said.
    let feet = feet_of(bot);
    let candidates: Vec<[i32; 3]> = {
        let name_at = |pos: [i32; 3]| block_name(bot, pos);
        place_candidates(feet, radius.round() as i32, name_at)
    };
    if candidates.is_empty() {
        return Err(Failure::new(
            "no_space",
            format!("no free cell with a solid floor within {} blocks of {:?}", radius.round() as i32, feet),
        )
        .with_hint("先 dig 开一块平地，或换到开阔处再 place"));
    }
    let mut tried: Vec<Value> = Vec::new();
    for cell in candidates.iter().take(3) {
        // Place onto the floor under the cell: that block's top face is the one the click can reach.
        match bot.place_block([cell[0], cell[1] - 1, cell[2]], Face::Up).await {
            Ok(()) => {
                let mut data = json!({
                    "placed": true,
                    "at": cell,
                    "cell": cell,
                    "target": cell,
                    "held": item_json(bot.state(), &held),
                    "swapped_with": swapped_with,
                });
                if !tried.is_empty() {
                    data["tried"] = json!(tried);
                }
                if let Some(at) = fell_back_from {
                    // The cell the caller named did not take it; say which one did (t108 P2).
                    data["fell_back_from"] = json!(at);
                }
                if let Some(first) = first_failure.take() {
                    data["first_failure"] = json!({ "message": first.message, "hint": first.hint });
                }
                return Ok(data);
            }
            Err(error) => {
                let failure = place_failure(bot, &error);
                tried.push(json!({
                    "at": cell,
                    "reason": failure.reason,
                    "message": failure.message,
                }));
            }
        }
    }
    // A named cell that failed keeps its own (more specific) failure when even the candidates come up empty: the
    // model asked for *that* cell, so the answer is about that cell, with the tested candidates attached.
    if let Some(mut failure) = first_failure.take() {
        failure.details = Some(json!({
            "against": fell_back_from,
            "target": fell_back_from.map(|at| face.adjacent(at)),
            "candidates_tried": tried,
        }));
        return Err(failure);
    }
    let summary = tried
        .iter()
        .map(|entry| {
            let at = &entry["at"];
            let reason = entry["reason"].as_str().unwrap_or("refused");
            format!("{at}: {reason}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    Err(Failure::new("no_space", format!("no spot took the block; tried {summary}"))
        .with_hint("换一处开阔地，或先 dig 掉挡路的方块再 place"))
}

/// `place` failures, with the one case the reason table flattens: the server answering with *nothing*.
///
/// t98 placed a block at a cell that stayed air, with the floor under it solid, `desync=0.0` and zero position
/// confirmations, and got `no_effect: the server sent no update for the cell` — which the shared `rejected` hint
/// turned into "the server refused it (anticheat, claim or game rule); stop trying". Nothing in that run supported
/// either reading, and a block that really never echoed looks exactly the same on the next call. So the hint is
/// ordered instead of final: read the cell back, then try a neighbour once, and only then call it a hard refusal.
fn place_failure(bot: &Bot, error: &acacia_bot::ActionError) -> Failure {
    let mut failure = action_failure(bot, error);
    if let Some(hint) = place_failure_hint(&failure.message) {
        failure.hint = Some(hint);
    }
    failure
}

/// The ordered hint a `place` whose cell never echoed gets, or `None` when the refusal said something else.
///
/// Split out of [`place_failure`] so the ordering — read back, then a neighbour, then a hard refusal — is pinned by
/// a test rather than by the next session's prose (t99). A hint that opens with "stop trying" is what turned a
/// one-block jitter into "the server refuses placements here".
fn place_failure_hint(message: &str) -> Option<String> {
    let no_effect = message.contains("no_effect") || message.contains("no local change");
    no_effect.then(|| {
        "不一定被拒了：① 等 1~2 s 用 block 回读该格（服务端有时不回 block update）；② 同一块放到相邻格再试一次；③ 两步都不行才按硬拒绝处理（领地 / 反作弊 / 游戏规则）。别对同一格原地连点"
            .to_owned()
    })
}

/// Candidate cells for `place {auto:true}`: air at the cell and above it, solid right below, and not
/// the bot's own feet. Sorted by distance so the nearest three are tried first. Pure, so the filter
/// is testable with a fake world.
fn place_candidates<F>(feet: [i32; 3], radius: i32, name_at: F) -> Vec<[i32; 3]>
where
    F: Fn([i32; 3]) -> Option<String>,
{
    let is_air = |pos: [i32; 3]| name_at(pos).is_some_and(|name| name == "minecraft:air");
    let is_solid = |pos: [i32; 3]| name_at(pos).is_some_and(|name| name != "minecraft:air");
    let mut out: Vec<([i32; 3], i32)> = Vec::new();
    for dx in -radius..=radius {
        for dz in -radius..=radius {
            let distance = dx * dx + dz * dz;
            if distance > radius * radius {
                continue;
            }
            for dy in -1..=1 {
                let cell = [feet[0] + dx, feet[1] + dy, feet[2] + dz];
                if cell == feet {
                    continue;
                }
                if !is_air(cell) || !is_air([cell[0], cell[1] + 1, cell[2]]) {
                    continue;
                }
                if !is_solid([cell[0], cell[1] - 1, cell[2]]) {
                    continue;
                }
                out.push((cell, distance + dy.abs() * 2));
            }
        }
    }
    out.sort_by_key(|(cell, score)| (*score, cell[0], cell[2], cell[1]));
    out.into_iter().map(|(cell, _)| cell).collect()
}

/// What `attack` does with one resolved target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// `once:true`: swing `times` times and stop, without walking.
    Swing,
    /// The default: `Bot::fight` — it swings when the target is already in reach and walks in when it
    /// is not. Only an approach that gets nowhere answers `unreachable_target`, so a 7.7-block target
    /// is approached, never refused with `out_of_reach`.
    Fight,
}

fn plan(once: bool) -> Plan {
    if once { Plan::Swing } else { Plan::Fight }
}

/// One entity an `attack` target may resolve to.
#[derive(Debug, Clone)]
struct Target {
    runtime_id: u64,
    kind: String,
    distance: f32,
}

impl Target {
    fn json(&self) -> Value {
        json!({"runtime_id": self.runtime_id, "kind": self.kind, "distance": round_tenth(self.distance)})
    }
}

/// Two decimals, for the drift numbers `status` reports.
fn round_hundredth(value: f32) -> f32 {
    (value * 100.0).round() / 100.0
}

fn round_tenth(value: f32) -> f32 {
    (value * 10.0).round() / 10.0
}

/// Picks the nearest candidate that matches `target`, within `radius`.
///
/// `"nearest"` matches every entity; anything else is a kind, with or without the `minecraft:`
/// namespace (`zombie` finds `minecraft:zombie`).
fn pick_target(target: &str, radius: f32, candidates: impl IntoIterator<Item = Target>) -> Option<Target> {
    let bare = |kind: &str| kind.strip_prefix("minecraft:").unwrap_or(kind).to_owned();
    let wanted = bare(target);
    let mut best: Option<Target> = None;
    for candidate in candidates {
        if candidate.distance > radius {
            continue;
        }
        let matches = target == "nearest" || {
            let kind = bare(&candidate.kind);
            kind == wanted || kind.contains(&wanted) || wanted.contains(&kind)
        };
        if !matches {
            continue;
        }
        if best.as_ref().is_none_or(|current| candidate.distance < current.distance) {
            best = Some(candidate);
        }
    }
    best
}

/// `Bot::fight` gave up on its own clock rather than on a death it could see.
fn fight_gave_up(message: &str) -> bool {
    message.contains("did not end in")
}

/// `Bot::fight` reports the swing count only when it gives up; a successful kill leaves it unknown.
fn swings_in(message: &str) -> Option<u32> {
    let rest = &message[message.find('(')? + 1..];
    rest[..rest.find(" swings")?].parse().ok()
}

/// Fights a target: resolve it, then either swing in place or hand it to `Bot::fight`, which walks
/// into reach and swings until the entity dies.
///
/// A target out of reach is **not** refused: `out_of_reach` is only what `once:true` can answer.
async fn attack(
    bot: &mut Bot,
    target: String,
    radius: f32,
    times: u8,
    once: bool,
    allow_dig: bool,
    timeout_s: u64,
) -> Result<Value, Failure> {
    let eye = bot.eye_position();
    let me = bot.state().player.runtime_entity_id;
    let candidates = bot.state().entities.iter().filter(|entity| entity.runtime_id != me).map(|entity| {
        let feet = entity.feet();
        Target {
            runtime_id: entity.runtime_id,
            kind: entity.kind.clone(),
            distance: distance(eye, [feet.x, feet.y + 0.9, feet.z]),
        }
    });
    let chosen = match target.parse::<u64>() {
        Ok(id) => candidates.into_iter().find(|candidate| candidate.runtime_id == id).ok_or_else(|| {
            Failure::new(
                "no_block",
                format!("entity {id} is not tracked (entity tracking off, or it is gone); use target:\"nearest\""),
            )
        })?,
        Err(_) => pick_target(&target, radius, candidates).ok_or_else(|| {
            Failure::new(
                "no_entity",
                format!("no attackable entity matching {target:?} within {radius} blocks; call entities to see what is around"),
            )
        })?,
    };

    let started = Instant::now();
    match plan(once) {
        Plan::Swing => {
            let hits = times.max(1);
            for _ in 0..hits {
                match bot.attack(chosen.runtime_id).await {
                    Ok(()) => {}
                    Err(error) => {
                        let mut failure = action_failure(bot, &error);
                        if !once && failure.reason == "out_of_reach" {
                            failure.message = format!(
                                "{} (target is {} blocks away; call again with once:false so it can walk in)",
                                failure.message, round_tenth(chosen.distance)
                            );
                        }
                        return Err(failure);
                    }
                }
            }
            Ok(json!({
                "target": chosen.json(),
                "hits": hits,
                "once": once,
                "approached": false,
                "elapsed_s": round_tenth(started.elapsed().as_secs_f32()),
            }))
        }
        Plan::Fight => {
            let before = bot.state().entities.get(chosen.runtime_id).and_then(|entity| entity.health());
            let mut opts = CombatOpts::default();
            opts.timeout = Duration::from_secs(timeout_s);
            if allow_dig {
                opts.goto.search.allow_dig = true;
            }
            let outcome = bot.fight(chosen.runtime_id, &opts).await;
            let elapsed_s = round_tenth(started.elapsed().as_secs_f32());
            match outcome {
                Ok(()) => {
                    let after = bot.state().entities.get(chosen.runtime_id).and_then(|entity| entity.health());
                    let gone = bot.state().entities.get(chosen.runtime_id).is_none();
                    let fight = bot.last_fight();
                    Ok(json!({
                        "target": chosen.json(),
                        // `FightReport` (t30): swings sent, swings the server acknowledged as hits, retreats.
                        "hits": fight.map(|report| report.hits),
                        "swings": fight.map(|report| report.swings),
                        "retreats": fight.map(|report| report.retreats),
                        "target_health_before": before,
                        "target_health_after": after,
                        "killed": gone || after.is_some_and(|health| health <= 0.0),
                        "approached": chosen.distance > ENTITY_REACH,
                        "elapsed_s": elapsed_s,
                    }))
                }
                Err(error) => {
                    let gone = bot.state().entities.get(chosen.runtime_id).is_none();
                    let mut failure = action_failure(bot, &error);
                    let fight = bot.last_fight();
                    // The report is the contract now; the message regex stays only for older callers.
                    let swings = fight.map(|report| report.swings).or_else(|| swings_in(&failure.message));
                    // `Bot::fight` only returns Ok on a death it can see (health <= 0, or the entity
                    // dropped out of tracking *before* the clock ran out). On the rental server the
                    // death event is often missing while the entity does vanish — judge by that
                    // external fact instead of handing the model a `rejected` for a kill.
                    if gone && fight_gave_up(&failure.message) {
                        return Ok(json!({
                            "target": chosen.json(),
                            "hits": fight.map(|report| report.hits),
                            "swings": swings,
                            "retreats": fight.map(|report| report.retreats),
                            "target_health_before": before,
                            "target_health_after": Value::Null,
                            "killed": true,
                            "approached": chosen.distance > ENTITY_REACH,
                            "elapsed_s": elapsed_s,
                            "note": "the fight clock ran out, but the target is no longer tracked: treated as a kill",
                        }));
                    }
                    if let Some(swings) = swings {
                        let message = failure.message.clone();
                        failure.message = format!("{message}; gave up after {swings} swings");
                    }
                    if fight_gave_up(&failure.message) {
                        failure.hint = Some("目标还在但这一场没打完：再调一次 attack，或先撤退/进食再打".to_owned());
                    }
                    Err(failure)
                }
            }
        }
    }
}

async fn container(
    bot: &mut Bot,
    at: Option<[i32; 3]>,
    op: ContainerOp,
    item: Option<String>,
    count: u8,
    slot: Option<u8>,
    keep_open: bool,
) -> Result<Value, Failure> {
    if op == ContainerOp::Close {
        if bot.open_container().is_none() {
            return Ok(already_closed_json());
        }
        bot.close_container().await.map_err(|error| action_failure(bot, &error))?;
        return Ok(json!({"closed": true}));
    }
    if bot.open_container().is_none() {
        let Some(at) = at else {
            return Err(Failure::invalid_argument("container needs at [x,y,z] when nothing is open"));
        };
        bot.open_container_at(at).await.map_err(|error| action_failure(bot, &error))?;
    }
    let mut moved = Value::Null;
    match op {
        ContainerOp::List | ContainerOp::Close => {}
        ContainerOp::Take => {
            let target = item.as_deref();
            let slots: Vec<u8> = bot
                .open_container()
                .map(|open| {
                    open.iter()
                        .filter(|(_, stack)| {
                            let name = bot.state().items.name(stack.network_id).unwrap_or("");
                            target.is_none_or(|want| name == want || name.ends_with(want))
                        })
                        .map(|(index, _)| index as u8)
                        .take(usize::from(count).max(1))
                        .collect()
                })
                .unwrap_or_default();
            if slots.is_empty() {
                return Err(Failure::new(
                    "no_item",
                    format!("the open container holds nothing matching {:?}", item.unwrap_or_default()),
                ));
            }
            for slot in &slots {
                bot.move_to_inventory(*slot).await.map_err(|error| action_failure(bot, &error))?;
            }
            moved = json!({"taken_slots": slots});
        }
        ContainerOp::Put => {
            let target = item.as_deref().ok_or_else(|| Failure::invalid_argument("container put needs item"))?;
            let network_id = bot.state().items.id(target).ok_or_else(|| Failure::new("no_item", format!("unknown item {target}")))?;
            let inventory_slot = bot
                .state()
                .inventory
                .find(|stack| stack.network_id == network_id)
                .map(|(window, slot, _)| (window, slot));
            let Some((window, inventory_slot)) = inventory_slot else {
                return Err(Failure::new("no_item", format!("no {target} in the inventory")));
            };
            if window != acacia_bot::state::Inventory::WINDOW_INVENTORY {
                return Err(Failure::new("no_item", format!("{target} is not in the main inventory")));
            }
            // The slot is chosen by the bot: `Container::iter()` only yields non-empty stacks, so a
            // `find(is_empty)` here could never match and always fell back to slot 0 (t132 F2: 26 free
            // slots, and the call still wrote slot 0 — or was refused for the item already there).
            let put = bot
                .put_into_container(inventory_slot as u8, u16::from(count), slot)
                .await
                .map_err(|error| action_failure(bot, &error))?;
            moved = put_moved_json(target, put.moved, put.asked, put.container_slot, put.note);
        }
    }
    let window = bot.open_container().map(|open| {
        let state = bot.state();
        json!({
            "window": open.window_id,
            "title": open.title,
            "items": open.iter().map(|(slot, stack)| json!({
                "slot": slot,
                "item": state.items.name(stack.network_id).unwrap_or("unknown"),
                "count": stack.count,
            })).collect::<Vec<_>>(),
        })
    });
    if !keep_open {
        if bot.open_container().is_some() {
            if let Err(error) = bot.close_container().await {
                eprintln!("[acacia-agent] closing the container failed: {error}");
            }
        }
    }
    Ok(json!({"op": format!("{op:?}").to_lowercase(), "moved": moved, "window": window, "kept_open": keep_open}))
}



/// What `close` answers when no window is left: not a failure, but "the previous op already did it".
/// t137 F2 — `keep_open:false` closes the window by itself, and `closed:false` next to `ok:true` read
/// as "the close did not work".
fn already_closed_json() -> Value {
    json!({
        "closed": true,
        "detail": "already_closed",
        "hint": "the previous container op closed the window itself (keep_open was false)",
    })
}

/// What a `put` reports: the count the container actually gained, next to the number that was asked
/// for — t132 F1 is exactly this pair disagreeing while the tool printed the request back as a fact.
fn put_moved_json(target: &str, moved: u16, asked: u16, container_slot: u8, note: Option<&str>) -> Value {
    let mut out = json!({
        "moved": target,
        "count": moved,
        "asked": asked,
        "container_slot": container_slot,
    });
    // No `note` key at all when there is nothing to say: an empty string reads as a problem.
    if let Some(note) = note {
        out["note"] = json!(note);
    }
    out
}

/// How many of `name` the bot carries right now (all inventory windows, hotbar first).
/// Item name to carried count, over the whole inventory.
/// What the inventory gained between two counts.
fn gained(before: &std::collections::BTreeMap<String, u32>, after: &std::collections::BTreeMap<String, u32>) -> Vec<Value> {
    let mut out = Vec::new();
    for (name, count) in after {
        let gained = count.saturating_sub(before.get(name).copied().unwrap_or(0));
        if gained > 0 {
            out.push(json!({"item": name, "count": gained}));
        }
    }
    out
}

fn inventory_counts(bot: &Bot) -> std::collections::BTreeMap<String, u32> {
    let state = bot.state();
    let mut counts: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for (_window, _slot, stack) in state.inventory.iter() {
        if stack.is_empty() {
            continue;
        }
        if let Some(name) = state.items.name(stack.network_id) {
            *counts.entry(name.to_owned()).or_default() += u32::from(stack.count);
        }
    }
    counts
}

/// Crafts through the recipe book (default) or by laying the grid out.
///
/// `acacia-bot` already waits 12 s for a write ack and re-reads the plan for 3 s after a timeout, so
/// this tool adds no second read-back: it reports the failure and tells the model to look at the
/// inventory before retrying (a world write is not idempotent, but a re-read is cheap).
async fn craft(bot: &mut Bot, item: String, count: u32, mode: CraftMode, table: Option<[i32; 3]>) -> Result<Value, Failure> {
    // A 3×3 recipe without a table used to fall back to 2×2 and fail far away (t54: the model read
    // the later `place`'s `no_item` as "not enough material"). Ask the recipe book first.
    if mode == CraftMode::Book && table.is_none() {
        let recipes = bot.recipes_for(&item);
        let has_any = !recipes.is_empty();
        let has_2x2 = recipes.iter().any(|recipe| recipe.fits_2x2());
        if needs_table(has_any, has_2x2) {
            return Err(Failure::new(
                "needs_table",
                format!("{item} has no 2x2 recipe: it needs a crafting table (3x3)"),
            )
            .with_hint("该配方需要 3×3：先放/找工作台，再用 table:[x,y,z] 重试（不确定就先 recipes {item} 看 fits_2x2）"));
        }
    }
    let bot_mode = match mode {
        CraftMode::Book => BotCraftMode::RecipeBook,
        CraftMode::Grid => BotCraftMode::Grid,
    };
    let mode_name = match mode {
        CraftMode::Book => "book",
        CraftMode::Grid => "grid",
    };
    match bot.craft_with(&item, count, table, bot_mode).await {
        Ok(made) => Ok(json!({
            "crafted": made,
            "item": item,
            "requested": count,
            "mode": mode_name,
            "table": table,
        })),
        Err(ActionError::Timeout) => Err(Failure::new(
            "timeout",
            format!("crafting {item} timed out; the ack may still arrive, so run inventory or observe before retrying"),
        )),
        Err(error) => Err(action_failure(bot, &error)),
    }
}

/// Sleeps in a bed: the one at `at`, or the nearest one within `radius`.
///
/// Returns `slept` rather than a bare ok, because a server can accept the click and still wake the
/// player (a monster nearby, or morning); `woke_reason` says so when that is visible.
/// Whether a recipe book entry needs the 3×3 grid: there is a recipe, and none of them fits 2×2.
/// Pure, so the boundary between `no_recipe`, `missing_ingredient` and `needs_table` is testable.
fn needs_table(has_any_recipe: bool, has_2x2_recipe: bool) -> bool {
    has_any_recipe && !has_2x2_recipe
}

/// Whether a `sleep` refusal is the server's "the bed is too far away" (`tile.bed.tooFar` / `bed is too far away`).
fn bed_too_far(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("toofar") || lower.contains("too far")
}

/// A refused `sleep`. The too-far case carries the desync hint: t88 was refused twice with the bed right there
/// (position desync 4.19) and slept 2.4 s after the drift went back to 0, so "too far" is usually the server's
/// stale idea of where the body is — not a walk the model still owes.
fn bed_refusal(message: String) -> Failure {
    if bed_too_far(&message) {
        Failure::new(
            "rejected",
            format!(
                "{message} (the server thinks the body is not beside the bed; t88 saw exactly this at position desync 4.19 and the same call slept 2.4 s later, once the drift was 0)"
            ),
        )
        .with_hint("多半是位置不同步：先 status 看 position_desync_blocks 与 server_block_position，等 desync 归零（或换一格再睡）再 sleep，别反复点床")
    } else {
        Failure::new("rejected", message)
    }
}

/// The `sleep` result. `woke_reason` is always present: `null` when the body is asleep, and the honest reason
/// when it is not — the server never said why, so the field names what it can be (t89).
fn sleep_result(slept: bool, bed: Option<[i32; 3]>, feet: [i32; 3]) -> Value {
    json!({
        "slept": slept,
        "bed_pos": bed,
        "position": feet,
        "woke_reason": if slept {
            Value::Null
        } else {
            json!("the server never confirmed sleep: the player was not asleep when the call returned (daytime, monsters nearby, or the bed refused). No server wake message arrived")
        },
    })
}

async fn sleep(bot: &mut Bot, at: Option<[i32; 3]>, radius: u8, timeout_s: u64) -> Result<Value, Failure> {
    if bot.is_sleeping() {
        let mut value = sleep_result(true, None, feet_of(bot));
        value["already_sleeping"] = json!(true);
        return Ok(value);
    }
    let bed = match at {
        Some(pos) => {
            let name = block_name(bot, pos).unwrap_or_default();
            if !block_matches("*bed", &name) {
                return Err(Failure::new(
                    "no_block",
                    format!("{pos:?} is not a bed (found {name:?}); pass the bed's coordinates"),
                ));
            }
            pos
        }
        None => {
            let found = find(bot, "*_bed", radius, 1)?;
            let pos = found
                .get("matches")
                .and_then(Value::as_array)
                .and_then(|matches| matches.first())
                .and_then(|first| first.get("pos"))
                .and_then(block_pos_of);
            match pos {
                Some(pos) => pos,
                None => {
                    return Err(Failure::new(
                        "no_block",
                        format!("no bed within {radius} blocks; craft and place one, or pass its coordinates"),
                    ))
                }
            }
        }
    };

    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    let feet = feet_of(bot);
    let gap = ((bed[0] - feet[0]).pow(2) + (bed[1] - feet[1]).pow(2) + (bed[2] - feet[2]).pow(2)) as f32;
    if gap > 9.0 {
        let goal = Goal::Near(bed, 2.0);
        match tokio::time::timeout_at(deadline, bot.goto_with(goal, GotoOpts::default())).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(action_failure(bot, &error)),
            Err(_) => {
                bot.stop_all();
                return Err(Failure::new("timeout", format!("walking to the bed at {bed:?} timed out")));
            }
        }
    }

    match tokio::time::timeout_at(deadline, bot.sleep(bed)).await {
        Ok(Ok(())) => {}
        Ok(Err(ActionError::NotPossible(message))) => return Err(Failure::new("no_block", message)),
        Ok(Err(ActionError::Rejected(message))) => return Err(bed_refusal(message)),
        Ok(Err(error)) => return Err(action_failure(bot, &error)),
        Err(_) => {
            bot.stop_all();
            return Err(Failure::new("timeout", format!("lying down at {bed:?} timed out")));
        }
    }
    Ok(sleep_result(bot.is_sleeping(), Some(bed), feet_of(bot)))
}

/// Walks to one dropped item and waits for the server to hand it over.
///
/// `acacia-bot`'s `collect` owns the walk, the retries for a sliding drop and the "entity gone or the
/// count rose" completion test; this tool adds the model-facing framing: the stacks the inventory
/// gained, and how many drops are still within reach.
async fn pickup(
    bot: &mut Bot,
    shared: &Shared,
    id: Option<u64>,
    item: Option<String>,
    radius: u8,
    timeout_s: u64,
    allow_dig: bool,
) -> Result<Value, Failure> {
    let radius = f32::from(radius.clamp(1, 32));
    // `entities` reports runtime ids; the model kept passing `radius` only and got `no_block` back
    // (t69). An id is exact, so it wins over a search.
    if let Some(runtime_id) = id {
        if bot.state().entities.get(runtime_id).is_none() {
            return Err(Failure::new(
                "no_such_entity",
                format!("no entity {runtime_id} is tracked: dropped items are the runtime ids `entities` lists, not block positions"),
            )
            .with_hint("可能已经被自动捡走了（走近掉落物就会捡）：先 inventory 看数量，再 entities{} 拿新 id"));
        }
        let before = inventory_counts(bot);
        if let Err(error) = bot.collect_with(runtime_id, collect_opts(allow_dig, timeout_s)).await {
            let tracked = bot.state().entities.get(runtime_id).is_some();
            let last_distance = distance_to(bot, runtime_id);
            return Err(pickup_failure(tracked, runtime_id, error, last_distance));
        }
        lock(shared).empty_pickups = 0;
        let after = inventory_counts(bot);
        return Ok(pickup_result(bot, &before, &after, Some(runtime_id)));
    }
    let before = inventory_counts(bot);
    let Some(runtime_id) = bot.nearest_dropped_item(radius, item.as_deref()) else {
        let empty = {
            let mut state = lock(shared);
            state.empty_pickups = state.empty_pickups.saturating_add(1);
            state.empty_pickups
        };
        if empty >= 2 {
            // Server trait, not a failure to retry: on the NetEase rental server a broken block goes
            // straight into the inventory, so there is never a drop entity to walk to.
            return Err(Failure::new(
                "no_such_entity",
                "no dropped item nearby (2nd empty pickup in a row): this server puts broken blocks straight into the inventory",
            )
            .with_hint("本服务器挖掘产物直接进背包：不用再调 pickup，改查 inventory 确认数量"));
        }
        let label = item.as_deref().map(|name| format!(" named {name}")).unwrap_or_default();
        return Err(Failure::new(
            "no_such_entity",
            format!("no dropped item{label} within {radius} blocks; move closer, or check inventory — this server may hand mining products over directly"),
        ));
    };

    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    let walking = bot.collect_with(runtime_id, collect_opts(allow_dig, timeout_s));
    match tokio::time::timeout_at(deadline, walking).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            // t69: the model only ever searched by name and got `rejected` back — neither the id it walked to nor
            // the reason survived. Name both now, so a retry can be an exact `pickup{id}`.
            let tracked = bot.state().entities.get(runtime_id).is_some();
            return Err(pickup_failure(tracked, runtime_id, error, distance_to(bot, runtime_id)));
        }
        Err(_) => {
            bot.stop_all();
            return Err(Failure::new(
                "timeout",
                format!("picking up entity {runtime_id} timed out; the drop may be out of reach"),
            ));
        }
    }

    lock(shared).empty_pickups = 0;
    let after = inventory_counts(bot);
    let collected: Vec<Value> = after
        .iter()
        .filter_map(|(name, count)| {
            let gained = count.saturating_sub(before.get(name).copied().unwrap_or(0));
            (gained > 0).then(|| json!({"item": name, "count": gained}))
        })
        .collect();
    let feet = feet_of(bot);
    let remaining = bot
        .dropped_items()
        .filter(|entity| {
            let at = entity.feet();
            let dx = at.x - feet[0] as f32;
            let dy = at.y - feet[1] as f32;
            let dz = at.z - feet[2] as f32;
            dx * dx + dy * dy + dz * dz <= radius * radius
        })
        .count();
    Ok(json!({
        "collected": collected,
        "remaining": remaining,
        "position": feet,
        "runtime_id": runtime_id,
        "block_position": bot.block_position(),
        "position_desync_blocks": bot.position_desync().map(|d| round_hundredth(d.blocks)),
        "client_position": bot.movement().and_then(|movement| movement.position()),
        "server_reference": if bot.server_reference_is_fresh() { "fresh" } else { "stale" },
    }))
}

/// `[i32;3]` from a JSON array, for positions that came out of [`find`].
fn block_pos_of(value: &Value) -> Option<[i32; 3]> {
    let array = value.as_array()?;
    if array.len() != 3 {
        return None;
    }
    let mut out = [0i32; 3];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = i32::try_from(array[index].as_i64()?).ok()?;
    }
    Some(out)
}

async fn smelt(bot: &mut Bot, at: [i32; 3], input: String, count: u8, fuel: Option<(String, u8)>) -> Result<Value, Failure> {
    let fuel_ref = fuel.as_ref().map(|(name, count)| (name.as_str(), *count));
    let made = bot.smelt(at, &input, count, fuel_ref).await.map_err(|error| action_failure(bot, &error))?;
    Ok(json!({"smelted": made, "input": input, "count": count, "at": at}))
}

const FOODS: [&str; 18] = [
    "minecraft:bread",
    "minecraft:cooked_beef",
    "minecraft:cooked_porkchop",
    "minecraft:cooked_chicken",
    "minecraft:cooked_mutton",
    "minecraft:cooked_cod",
    "minecraft:cooked_salmon",
    "minecraft:baked_potato",
    "minecraft:apple",
    "minecraft:carrot",
    "minecraft:golden_apple",
    "minecraft:melon_slice",
    "minecraft:cookie",
    "minecraft:beef",
    "minecraft:porkchop",
    "minecraft:chicken",
    "minecraft:mutton",
    "minecraft:potato",
];

async fn eat(bot: &mut Bot, food: Option<String>) -> Result<Value, Failure> {
    let name = match food {
        Some(name) => name,
        None => {
            let carried: Vec<String> = bot
                .state()
                .inventory
                .iter()
                .filter(|(_, _, stack)| !stack.is_empty())
                .filter_map(|(_, _, stack)| bot.state().items.name(stack.network_id).map(str::to_owned))
                .collect();
            FOODS.iter().find(|food| carried.iter().any(|item| item == *food)).map(|food| (*food).to_owned()).ok_or_else(|| {
                Failure::new("no_item", "no food in the inventory")
            })?
        }
    };
    let from = bot.find_item(&name).ok_or_else(|| Failure::new("no_item", format!("no {name} in the inventory")))?;
    bot.equip(from, Destination::Hand).await.map_err(|error| action_failure(bot, &error))?;
    if let Err(error) = bot.consume().await {
        let failure = action_failure(bot, &error);
        return Err(eat_failure(bot, failure));
    }
    let health = bot.state().player.health;
    let hunger = bot.state().player.hunger;
    Ok(json!({"ate": name, "health": health, "food": hunger}))
}

/// A refusal to eat says something about the stomach, not about where the bot stands.
///
/// Every `refused` action used to inherit the "try another spot" hint, which is wrong here: a full
/// food bar is what stops the bite, and t116 saw `eat {}` answered with a hint that pushed the model
/// to move. The numbers come from the live player state, so the model can decide whether eating is
/// worth another call at all.
fn eat_failure(bot: &Bot, failure: Failure) -> Failure {
    eat_failure_with(bot.state().player.hunger, bot.state().player.saturation, failure)
}

/// The decision itself, with the state passed in so it can be pinned without a connected bot.
fn eat_failure_with(food: f32, saturation: f32, failure: Failure) -> Failure {
    if failure.reason != "refused" || !failure.message.to_ascii_lowercase().contains("not hungry") {
        return failure;
    }
    failure.with_hint(not_hungry_hint(food, saturation)).with_details(json!({"food": food, "saturation": saturation}))
}

/// "You are not hungry" said with the real food/saturation, plus what to do instead of nibbling.
fn not_hungry_hint(food: f32, saturation: f32) -> String {
    format!(
        "当前 food={food:.1}/saturation={saturation:.1}，你还不饿（food 满格时吃不下去）：食物留着，先去挖矿、奔跑或战斗把饥饿值消耗掉再吃；这跟你站在哪儿无关，别为此到处走"
    )
}

/// The per-shape history of an equip, or `None` when nothing was tried (t134).
///
/// `None` rather than `[]` on purpose: an empty list reads as "tried nothing", which is the opposite of
/// "worked on the first request".
fn attempts_json(attempts: &[EquipAttempt]) -> Option<Value> {
    if attempts.is_empty() {
        return None;
    }
    Some(json!(
        attempts
            .iter()
            .map(|a| json!({"action": a.action, "status_translated": a.status, "message": a.message}))
            .collect::<Vec<_>>()
    ))
}

/// The answer of `equip`, built from the bot's own report so its shape can be pinned without a bot (t128).
///
/// `action_used` is the request shape that actually moved the item (`swap`, `clicks`, `use`, or `none`
/// when it was already where it belonged). `status_translated` is `null` on success - the rungs below it
/// may each carry their own refusal, but the move itself was not refused, and a made-up value would read
/// as an answer we do not have.
///
/// `attempts` is **omitted** when the first shape worked: an empty list would read as "tried nothing".
fn equip_json(item: &str, slot: EquipSlot, report: &EquipReport) -> Value {
    let mut out = json!({
        "equipped": item,
        "slot": format!("{slot:?}").to_lowercase(),
        "action_used": report.action_used,
        "status_translated": report.status_translated,
    });
    if let Some(attempts) = attempts_json(&report.attempts) {
        out["attempts"] = attempts;
    }
    out
}

/// A failed `equip`, carrying the protocol's own words for the refusal (t134).
///
/// The code comes from the [`EquipReport`] the bot kept after the ladder ended (`Bot::take_equip_report`)
/// - never from the `(tried: swap -> 55 CannotPlaceItem)` tail of the message: reading a code out of text
/// has been the wrong source three times over. When the report carries no code the value is `null`, never
/// a guess like "unknown", and `attempts` is left out entirely: an empty list would read as "tried nothing".
fn equip_failure(failure: Failure, item: &str, slot: EquipSlot, report: Option<&EquipReport>) -> Failure {
    let mut details = json!({
        "equipped": item,
        "slot": format!("{slot:?}").to_lowercase(),
    });
    if let Some(report) = report {
        details["action_used"] = json!(report.action_used);
        details["status_translated"] = json!(report.status_translated);
        if let Some(attempts) = attempts_json(&report.attempts) {
            details["attempts"] = attempts;
        }
    }
    failure.with_details(details)
}

async fn equip(bot: &mut Bot, item: String, slot: EquipSlot) -> Result<Value, Failure> {
    let from = bot.find_item(&item).ok_or_else(|| Failure::new("no_item", format!("no {item} in the inventory")))?;
    let destination = match slot {
        EquipSlot::Hand => Destination::Hand,
        EquipSlot::Head => Destination::Head,
        EquipSlot::Chest => Destination::Chest,
        EquipSlot::Legs => Destination::Legs,
        EquipSlot::Feet => Destination::Feet,
        EquipSlot::Offhand => Destination::OffHand,
    };
    let outcome = bot.equip_reporting(from, destination, EquipMethod::Inventory).await;
    // Drained on both paths: the report is cleared as it is read (t129), so taking it here means no code
    // can outlive the equip it belongs to - the next failure would otherwise quote this one's refusal.
    let report = bot.take_equip_report();
    match outcome {
        // `equip_reporting` already hands the report back on success; taking it too keeps the bot clean.
        Ok(from_call) => Ok(equip_json(&item, slot, report.as_ref().unwrap_or(&from_call))),
        Err(error) => Err(equip_failure(action_failure(bot, &error), &item, slot, report.as_ref())),
    }
}

async fn look(bot: &mut Bot, yaw: Option<f32>, pitch: Option<f32>, at: Option<[i32; 3]>) -> Result<Value, Failure> {
    if let Some([x, y, z]) = at {
        let target = [x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5];
        bot.look_at(target).await.map_err(|error| action_failure(bot, &error))?;
        let view = bot.controls().map(|controls| (controls.yaw, controls.pitch));
        return Ok(json!({"looked_at": [x, y, z], "yaw": view.map(|v| v.0), "pitch": view.map(|v| v.1)}));
    }
    let (Some(yaw), pitch) = (yaw, pitch.unwrap_or(0.0)) else {
        return Err(Failure::invalid_argument("look needs yaw (and optional pitch) or at [x,y,z]"));
    };
    let Some(controls) = bot.controls() else {
        return Ok(json!({"ok": false, "detail": "look not supported: the bot has no physics controls"}));
    };
    let yaw = wrap_yaw(yaw);
    let pitch = pitch.clamp(-90.0, 90.0);
    (controls.yaw, controls.pitch) = (yaw, pitch);
    bot.wait_ticks(1).await.map_err(|error| action_failure(bot, &error))?;
    Ok(json!({"yaw": yaw, "pitch": pitch}))
}

fn chat(bot: &mut Bot, config: &ShellConfig, text: String, recipient: Option<String>) -> Result<Value, Failure> {
    if !config.chat.enabled {
        return Err(Failure::new("rejected", "chat is disabled in this configuration (chat.enabled = false)"));
    }
    if recipient.is_some() {
        return Err(Failure::invalid_argument("whisper is not available: this build sends plain public chat only"));
    }
    let text = crate::agent::guard::sanitize_chat(&config.chat, &text)?;
    let sent = bot.client().chat(text.clone());
    Ok(json!({"sent": sent, "text": text, "detail": if sent { Value::Null } else { json!("the connection has closed") }}))
}

async fn hold(bot: &mut Bot, item: String, slot: Option<u8>) -> Result<Value, Failure> {
    hold_item(bot, &item, slot).await
}

/// A trader `kind` as `entities` reports it: `villager` and `wandering_trader` get `minecraft:`.
fn trader_kind(target: &str) -> String {
    let lowered = target.trim().to_ascii_lowercase();
    if lowered.contains(':') {
        lowered
    } else {
        format!("minecraft:{lowered}")
    }
}

/// `target` as a trader: a runtime id, `nearest` (any villager), or a kind from `entities`
/// (`minecraft:villager_v2`, `villager`, `wandering_trader`) meaning the nearest of that kind.
fn resolve_trader(bot: &Bot, target: &str, radius: f32) -> Result<u64, Failure> {
    if let Ok(id) = target.parse::<u64>() {
        return Ok(id);
    }
    // A kind the model copied out of `entities` used to be an `invalid_argument`; it is the same
    // search as `nearest`, narrowed to one kind.
    let kind_filter = if target == "nearest" { None } else { Some(trader_kind(target)) };
    let me = feet_of(bot);
    let found = bot
        .state()
        .entities
        .iter()
        .filter(|entity| match &kind_filter {
            Some(kind) => entity.kind.eq_ignore_ascii_case(kind) || entity.kind.eq_ignore_ascii_case(&kind.replace("_v2", "")),
            None => entity.kind == "minecraft:villager" || entity.kind == "minecraft:wandering_trader",
        })
        .filter_map(|entity| {
            let dx = entity.position.x - me[0] as f32;
            let dy = entity.position.y - me[1] as f32;
            let dz = entity.position.z - me[2] as f32;
            let distance = (dx * dx + dy * dy + dz * dz).sqrt();
            (distance <= radius).then_some((entity.runtime_id, distance, entity.kind.clone()))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1));
    match found {
        Some((runtime_id, _, _)) => Ok(runtime_id),
        None => Err(Failure::new(
            "no_villager",
            format!(
                "no {} within {} blocks; call entities to see what is around",
                kind_filter.as_deref().unwrap_or("villager or wandering_trader"),
                radius.round()
            ),
        )),
    }
}

/// What one trade of `offer` costs, `buy_b` folded in and a repeated item summed (t83).
///
/// `times` multiplies the price: `trades` asks with 1, `trade` with the count it was given.
fn offer_cost(offer: &acacia_bot::state::TradeOffer, times: u32) -> Vec<(String, u32)> {
    let mut cost: Vec<(String, u32)> = Vec::new();
    for part in [Some(&offer.buy_a), offer.buy_b.as_ref()].into_iter().flatten() {
        if part.name.is_empty() || part.count == 0 {
            continue;
        }
        let need = u32::from(part.count).saturating_mul(times.max(1));
        match cost.iter_mut().find(|(name, _)| *name == part.name) {
            Some((_, have)) => *have = have.saturating_add(need),
            None => cost.push((part.name.clone(), need)),
        }
    }
    cost
}

/// The parts of `offer`'s price the inventory cannot cover, as `(item, have, need)` (t83).
fn missing_parts(
    offer: &acacia_bot::state::TradeOffer,
    have: &BTreeMap<String, u32>,
    times: u32,
) -> Vec<(String, u32, u32)> {
    offer_cost(offer, times)
        .into_iter()
        .map(|(name, need)| {
            let have = have.get(&name).copied().unwrap_or(0);
            (name, have, need)
        })
        .filter(|(_, have, need)| have < need)
        .collect()
}

/// Indices of the offers a trade would cover right now, in list order (t83: the model needed this and
/// the tool did not say it — it picked the 32-rotten_flesh offer #0 while 64 emeralds sat in the bag).
fn payable_indices(offers: &[acacia_bot::state::TradeOffer], have: &BTreeMap<String, u32>, times: u32) -> Vec<usize> {
    offers
        .iter()
        .enumerate()
        .filter(|(_, offer)| missing_parts(offer, have, times).is_empty())
        .map(|(index, _)| index)
        .collect()
}

/// `minecraft:emerald` -> `emerald`: JSON keeps the full identifier, messages read as prose.
fn short_name(name: &str) -> &str {
    name.strip_prefix("minecraft:").unwrap_or(name)
}

/// `1 emerald -> 1 redstone`: the one-line shape of an offer, for a message the model can compare.
fn offer_label(offer: &acacia_bot::state::TradeOffer) -> String {
    let side = |trade: &acacia_bot::state::TradeItem| format!("{} {}", trade.count, short_name(&trade.name));
    let pay = match offer.buy_b.as_ref() {
        Some(second) => format!("{} + {}", side(&offer.buy_a), side(second)),
        None => side(&offer.buy_a),
    };
    format!("{pay} -> {}", side(&offer.sell))
}

fn price_text(cost: &[(String, u32)]) -> String {
    cost.iter().map(|(name, count)| format!("{count} {}", short_name(name))).collect::<Vec<_>>().join(" + ")
}

/// `missing_offer_cost`, worded so the next call is obvious: the offer, the shortfall, and the offers
/// that *would* trade right now (t83's run: 10 offers, #1 was one emerald away and never mentioned).
///
/// `index` is `None` when no offer is payable at all; then the closest one is named instead.
fn unpayable_failure(
    index: Option<usize>,
    offers: &[acacia_bot::state::TradeOffer],
    have: &BTreeMap<String, u32>,
    times: u32,
) -> Failure {
    let payable = payable_indices(offers, have, times);
    let index_asked = index;
    let payable_text = if payable.is_empty() {
        String::from("no offer is payable with this inventory")
    } else {
        let listed: Vec<String> = payable
            .iter()
            .take(5)
            .map(|&index| match offers.get(index) {
                Some(offer) => format!("#{index} ({})", offer_label(offer)),
                None => format!("#{index}"),
            })
            .collect();
        format!("payable offers: {}", listed.join(", "))
    };
    let chosen = index.or_else(|| {
        // Closest = fewest missing items, then the smallest absolute shortfall.
        offers
            .iter()
            .enumerate()
            .min_by_key(|(_, offer)| {
                let missing = missing_parts(offer, have, times);
                let shortfall: u32 = missing.iter().map(|(_, have, need)| need - have).sum();
                (missing.len(), shortfall)
            })
            .map(|(index, _)| index)
    });
    let (lead, details_missing) = match chosen.and_then(|index| offers.get(index).map(|offer| (index, offer))) {
        Some((index, offer)) => {
            let cost = offer_cost(offer, times);
            let held: Vec<u32> = cost.iter().map(|(name, _)| have.get(name).copied().unwrap_or(0)).collect();
            let held_text = held.iter().map(u32::to_string).collect::<Vec<_>>().join(" + ");
            let missing = missing_parts(offer, have, times);
            let details: Vec<Value> = missing
                .iter()
                .map(|(item, have, need)| json!({"item": item, "have": have, "need": need}))
                .collect();
            let lead = match index_asked {
                Some(asked) if asked == index => format!("offer #{index}"),
                _ => format!("closest offer #{index}"),
            };
            (format!("{lead} needs {}, you have {held_text}", price_text(&cost)), details)
        }
        None => (String::from("the trader listed no offers to price"), Vec::new()),
    };
    let hint = if payable.is_empty() {
        "no offer is affordable yet: gather what the closest one names (inventory shows the counts), or trade for something the bag already covers"
    } else {
        "retry with trade{offer:\"first_payable\"} (or one of payable_offers) instead of gathering first; trades lists the whole table with payable flags"
    };
    Failure::new("missing_offer_cost", format!("{lead}; {payable_text}"))
        .with_hint(hint)
        .with_details(json!({
            "offer_index": chosen,
            "missing": details_missing,
            "payable_offers": payable,
            "times": times,
        }))
}

/// One offer as JSON: prices, stock and level from the server's own trade window, plus whether this
/// inventory can pay for it and what it is short of (t83).
fn trade_offer_json(
    offer: &acacia_bot::state::TradeOffer,
    index: usize,
    have: &BTreeMap<String, u32>,
    times: u32,
) -> Value {
    let item = |trade: &acacia_bot::state::TradeItem| json!({"item": trade.name, "count": trade.count});
    let missing: Vec<Value> = missing_parts(offer, have, times)
        .into_iter()
        .map(|(item, have, need)| json!({"item": item, "have": have, "need": need}))
        .collect();
    json!({
        "index": index,
        "buy_a": item(&offer.buy_a),
        "buy_b": offer.buy_b.as_ref().map(item),
        "sell": item(&offer.sell),
        "uses": offer.uses,
        "max_uses": offer.max_uses,
        "tier": offer.tier,
        "left": (offer.max_uses - offer.uses).max(0),
        "payable": missing.is_empty(),
        "missing": missing,
    })
}

/// Opens the trader's screen and returns the offers it carries, closing it again. `trades` and
/// `trade{offer:"first_payable"}` share it: the price list only exists while the screen is open.
async fn read_offers(bot: &mut Bot, trader: u64) -> Result<Vec<acacia_bot::state::TradeOffer>, Failure> {
    // t79/t85: an empty screen right after the open is not proof that the villager lists nothing — the client can
    // read the window before the server has filled the recipe list (village present, `trades` answering `no_offers`).
    // So: open, wait, and if the list is still empty, close, reopen once and wait again before believing it.
    let mut attempts = 0;
    let mut offered = Vec::new();
    while attempts < 2 {
        attempts += 1;
        if let Err(error) = bot.interact_entity(trader).await {
            return Err(action_failure(bot, &error));
        }
        for _ in 0..20 {
            if !bot.trade_offers().is_empty() {
                break;
            }
            bot.wait_ticks(2).await.map_err(|error| action_failure(bot, &error))?;
        }
        offered = bot.trade_offers().to_vec();
        let _ = bot.close_container().await;
        if !offered.is_empty() {
            break;
        }
    }
    if offered.is_empty() {
        return Err(Failure::new(
            "no_offers",
            format!(
                "trader {trader} opened a trade screen with no offers twice in a row; either the list was still \
                 loading (retry trades) or this trader really lists none (it may be out of stock or need a job \
                 site block)"
            ),
        ));
    }
    Ok(offered)
}

/// `trades`: open the trade screen, read the offers, close it. Read-only: nothing is bought.
async fn trades(bot: &mut Bot, target: String, radius: f32) -> Result<Value, Failure> {
    let trader = resolve_trader(bot, &target, radius)?;
    let offers = read_offers(bot, trader).await?;
    let have = inventory_counts(bot);
    let table: Vec<Value> = offers.iter().enumerate().map(|(index, offer)| trade_offer_json(offer, index, &have, 1)).collect();
    let payable = payable_indices(&offers, &have, 1);
    Ok(json!({"trader": trader, "offers": table, "count": table.len(), "payable_offers": payable}))
}

/// `trade`: the server does the trading; this resolves the villager, picks the offer (`first_payable`
/// unless an index was given), reports what the inventory gained and lost, and maps the bot's failures
/// onto the reason vocabulary.
async fn trade(
    bot: &mut Bot,
    target: String,
    offer: crate::tools::spec::OfferChoice,
    times: u32,
    radius: f32,
) -> Result<Value, Failure> {
    use crate::tools::spec::OfferChoice;
    let trader = resolve_trader(bot, &target, radius)?;
    let have = inventory_counts(bot);
    let (index, chosen_by, known) = match offer {
        OfferChoice::Index(index) => (index as usize, "index", None),
        OfferChoice::FirstPayable => {
            let offers = read_offers(bot, trader).await?;
            match payable_indices(&offers, &have, times).first() {
                Some(&first) => (first, "first_payable", Some(offers)),
                None => return Err(unpayable_failure(None, &offers, &have, times)),
            }
        }
    };
    let before = have.clone();
    let made = match bot.trade(trader, index, times).await {
        Ok(made) => made,
        Err(error) if is_missing_cost(&error) => {
            // The bot's own message names one item and no offer; re-read the table and say it properly.
            let offers = match known {
                Some(offers) => offers,
                None => read_offers(bot, trader).await.unwrap_or_default(),
            };
            return Err(unpayable_failure(Some(index), &offers, &have, times));
        }
        Err(error) => return Err(action_failure(bot, &error)),
    };
    let after = inventory_counts(bot);
    let mut got: Vec<Value> = Vec::new();
    let mut paid: Vec<Value> = Vec::new();
    for (name, count) in &after {
        let gained = count.saturating_sub(before.get(name).copied().unwrap_or(0));
        if gained > 0 {
            got.push(json!({"item": name, "count": gained}));
        }
    }
    for (name, count) in &before {
        let lost = count.saturating_sub(after.get(name).copied().unwrap_or(0));
        if lost > 0 {
            paid.push(json!({"item": name, "count": lost}));
        }
    }
    // The window is closed by `trade`; the offers read before it are what names the one that was used.
    let used = known
        .as_ref()
        .and_then(|offers| offers.get(index))
        .map(|entry| trade_offer_json(entry, index, &before, times));
    Ok(json!({
        "traded": true,
        "trader": trader,
        "offer": used,
        "offer_index": index,
        "chosen_by": chosen_by,
        "times": made,
        "got": got,
        "paid": paid,
    }))
}

/// The bot's own cost failure (`missing_offer_cost: not enough X to pay`), caught so the shell can
/// reword it with the offer index and the payable alternatives.
fn is_missing_cost(error: &acacia_bot::ActionError) -> bool {
    match error {
        acacia_bot::ActionError::NotPossible(text) => text.starts_with("missing_offer_cost"),
        other => other.to_string().starts_with("missing_offer_cost"),
    }
}

/// `drop`: the escape hatch when the hotbar is full of junk (t48: four wooden pickaxes over one
/// crafting table). `slot` is a main-inventory index; `item` finds the first stack of that name.
async fn drop_items(bot: &mut Bot, slot: Option<u8>, item: Option<String>, count: u8) -> Result<Value, Failure> {
    let (index, stack) = match (slot, item) {
        (Some(index), _) => {
            let stack = bot.state().inventory.main.get(usize::from(index)).cloned();
            (usize::from(index), stack)
        }
        (None, Some(name)) => {
            let Some(network_id) = bot.state().items.id(&name) else {
                return Err(Failure::new("no_item", format!("unknown item {name}")));
            };
            let found = bot
                .state()
                .inventory
                .iter()
                .find(|(window, _, stack)| *window == acacia_bot::state::Inventory::WINDOW_INVENTORY && stack.network_id == network_id)
                .map(|(_, slot, stack)| (usize::from(slot), Some(stack.clone())));
            found.ok_or_else(|| Failure::new("no_item", format!("no {name} in the main inventory")))?
        }
        (None, None) => return Err(Failure::invalid_argument("drop needs slot or item")),
    };
    let Some(stack) = stack.filter(|stack| !stack.is_empty()) else {
        return Err(Failure::new("no_item", format!("slot {index} is empty")));
    };
    let name = bot.state().item_name(&stack).unwrap_or("unknown").to_owned();
    let count_missing = u8::try_from(stack.count).unwrap_or(u8::MAX);
    let count = count.min(count_missing);
    bot.drop_item(SlotRef::Main(u8::try_from(index).unwrap_or(0)), count)
        .await
        .map_err(|error| action_failure(bot, &error))?;
    Ok(json!({"dropped": {"item": name, "count": count}, "slot": index, "remaining": stack.count.saturating_sub(u16::from(count))}))
}

async fn hold_item(bot: &mut Bot, item: &str, slot: Option<u8>) -> Result<Value, Failure> {
    let Some(network_id) = bot.state().items.id(item) else {
        return Err(Failure::new("no_item", format!("unknown item {item}")));
    };
    let found = bot
        .state()
        .inventory
        .find(|stack| stack.network_id == network_id)
        .map(|(window, slot, stack)| (window, slot, stack.count));
    let Some((window, found_slot, count)) = found else {
        return Err(Failure::new("no_item", format!("no {item} in the inventory")));
    };
    if window != acacia_bot::state::Inventory::WINDOW_INVENTORY {
        return Err(Failure::new("no_item", format!("{item} is not in the main inventory")));
    }
    let mut target = slot.unwrap_or(found_slot.min(8) as u8);
    let mut swapped_with = None;
    if slot.is_none() && found_slot >= usize::from(acacia_bot::state::Inventory::HOTBAR_SLOTS) {
        let hotbar = usize::from(acacia_bot::state::Inventory::HOTBAR_SLOTS);
        let stack = |index: usize| bot.state().inventory.main.get(index).cloned();
        // A hotbar slot that already holds this item: just select it (t48 had three wooden pickaxes there).
        let same = (0..hotbar).find(|&index| stack(index).is_some_and(|s| s.network_id == network_id && !s.is_empty()));
        let free = (0..hotbar).find(|&index| stack(index).is_none_or(|s| s.is_empty()));
        match (same, free) {
            (Some(same), _) => target = same as u8,
            (None, Some(free)) => {
                bot.move_item(SlotRef::Main(found_slot as u8), SlotRef::Main(free as u8), u8::try_from(count).unwrap_or(1))
                    .await
                    .map_err(|error| action_failure(bot, &error))?;
                target = free as u8;
            }
            (None, None) => {
                // The hotbar is full: swap with the slot holding the smallest stack, which loses the least. A swap
                // keeps both stacks, unlike dropping one (t48: place failed forever while 13 main slots were free).
                let victim = (0..hotbar)
                    .min_by_key(|&index| stack(index).map_or(i32::MAX, |s| i32::from(s.count)))
                    .ok_or_else(|| Failure::new("hotbar_full", "no hotbar slot to swap with"))?;
                bot.swap_items(SlotRef::Main(found_slot as u8), SlotRef::Main(victim as u8))
                    .await
                    .map_err(|error| action_failure(bot, &error))?;
                target = victim as u8;
                swapped_with = Some(victim);
            }
        }
    }
    bot.select_hotbar(target).map_err(|error| action_failure(bot, &error))?;
    Ok(json!({"held": item, "slot": target, "count": count, "swapped_with": swapped_with}))
}

/// Whether `block` satisfies the request: within `reach` blocks on x/z, and on y too when the caller asked for an
/// exact cell. One function for the client's cell and the server's, so a "reached" answer means the same thing on
/// both sides (t52: 2.07 apart was one block of y plus two of x counted as a straight distance).
fn inside_goal(block: Option<[i32; 3]>, at: Option<[i32; 3]>, xz: Option<[i32; 2]>, exact: bool, reach: f32) -> bool {
    let Some(block) = block else { return false };
    match (at, xz) {
        (Some(at), _) => {
            (block[0] - at[0]).abs() as f32 <= reach
                && (block[2] - at[2]).abs() as f32 <= reach
                && (!exact || (block[1] - at[1]).abs() as f32 <= reach)
        }
        (None, Some([x, z])) => (block[0] - x).abs() as f32 <= reach && (block[2] - z).abs() as f32 <= reach,
        _ => false,
    }
}

/// Ticks (seconds) one `goto_with` attempt may take before the supervisor checks whether the body moved.
const WALK_CHUNK_SECS: u64 = 8;
/// Blocks the body must cover inside a chunk for the walk to count as progressing.
const WALK_PROGRESS: f32 = 1.0;

struct Walk {
    outcome: Result<(), acacia_bot::ActionError>,
    /// The steps that really ran in the *current* round, in order: the only thing `tried` may report (t82).
    /// Cleared whenever the body makes progress — that is still "this round's means" (t89).
    tried: Vec<&'static str>,
    /// The last jump ever observed in this walk, kept across progress (t89). Not cleared, ever: see [`JumpRecord`].
    jump_last: Option<JumpRecord>,
    /// How many healing steps were started, observed or not — the count the message reports.
    attempts: usize,
    stuck_at: Option<[i32; 3]>,
    /// The chunk deadline came before `goto_with` returned: the body never ran out of route, it ran out of time.
    timed_out: bool,
    /// A healing step *worked* in this walk (a hop that ended a whole block higher) and the body still ended up
    /// blocked. The model reads this as "the heal is not what is missing — the route or the goal is" (t97).
    healed_once: bool,
}

/// One observed jump, kept after the walk makes progress (t89).
///
/// `tried` answers "what did this round try" and is cleared on progress on purpose. A successful hop covers √2
/// blocks, i.e. more than `WALK_PROGRESS`, so before t89 it cleared itself on the very next check — `jump_ok`
/// could only ever be read in samples where the jump *failed*, and a run whose jump worked (t88: 6/6 windows with
/// `jump=1`, `rise_ground=+1.252`, `block_position` 70→71→72) ended in a bare `refused` with `details: null`.
/// The observation is now kept separately and reported as `details.jump_last`, on success as well as failure.
#[derive(Debug, Clone, PartialEq)]
struct JumpRecord {
    /// The step cell the hop aimed at; `None` when there was nowhere to hop (`no_step_candidate`).
    target: Option<[i32; 3]>,
    /// The feet height the hop started from — the ground it stood on.
    y_ground: f32,
    attempt: JumpAttempt,
}

impl JumpRecord {
    fn new(target: Option<[i32; 3]>, attempt: JumpAttempt) -> Self {
        Self { target, y_ground: attempt.y_before, attempt }
    }

    /// A record for a jump the **body** made (t112): the ground is the block it stood on, taken before the liftoff,
    /// not `y_before` -- the two differ on this server (feet at 70.5625), and `y_ground` is read in blocks.
    fn from_body(jump: &acacia_bot::movement::Jump) -> Self {
        Self { target: jump.target, y_ground: jump.y_ground.unwrap_or(jump.y_before), attempt: JumpAttempt::from_jump(jump) }
    }

    fn to_json(&self) -> Value {
        // The attempt's own fields (`kind`, `gained`, `y_before`/`y_max`/`y_after`, `sent`, `why`) plus where the
        // hop aimed and the ground it left: `kind`/`gained` are what t88 could not see.
        let mut value = self.attempt.to_json();
        value["target"] = json!(self.target);
        value["y_ground"] = json!(self.y_ground);
        value
    }
}

/// What one *real* jump attempt did — evidence, not intent.
///
/// t82: `walk_supervised` used to push the name `"jump"` into `tried` and then pathfind towards the step without
/// ever pressing the key. run213 answered `details.tried:["jump"]` for a cell it never jumped at, while the whole
/// log held two jump inputs, both from an unrelated staircase. A step may now enter `tried` only through this
/// struct, which the primitive fills in (`Bot::hop_toward`, t81).
#[derive(Debug, Clone, PartialEq)]
struct JumpAttempt {
    /// `no_step_candidate` (nowhere to hop, so no key was pressed), `jump_not_sent` (the primitive refused), or one
    /// of the four block terms of `jump_kind` (t99): `jump_ok`, `jump_up_landed`, `jump_landed_back`, `jump_fell`.
    kind: &'static str,
    sent: bool,
    y_before: f32,
    y_max: f32,
    y_after: f32,
    /// The primitive's own, stricter verdict: the feet ended `RISE_THRESHOLD` (0.9) higher, i.e. a whole block.
    rose: bool,
    /// Why the primitive called it a miss (`not_on_ground`, `no_liftoff`, `no_gain`), or the error text.
    why: Option<String>,
    /// The cell that is in the way, for `no_headroom`: the block above the aimed step (t105). `None` for every other
    /// outcome — a hop that merely failed does not know what to blame.
    blocked: Option<[i32; 3]>,
    /// How many ticks the jump key was really held. `0` with `sent: false` is the t105 shape: nothing was pressed
    /// because the ceiling is solid, which is a different fact from "pressed and nothing happened".
    pressed_ticks: u32,
}

impl JumpAttempt {
    fn from_jump(jump: &acacia_bot::movement::Jump) -> Self {
        // A verdict the primitive reached *before touching the controls* is its own kind (t105): the block grid can
        // say how high the body ended, but not "nothing was pressed because the ceiling is solid". `no_headroom` and
        // `hop_no_progress` are details, not new `fail_reason`s — the walk still fails as `blocked_by_terrain`.
        let kind = match jump.reason {
            Some("no_headroom") | Some("hop_no_progress") => jump.kind(),
            _ => jump_kind(jump.sent, jump.y_before, jump.y_after, jump.target),
        };
        Self {
            kind,
            sent: jump.sent,
            y_before: jump.y_before,
            y_max: jump.y_max,
            y_after: jump.y_after,
            rose: jump.rose,
            why: jump.reason.map(str::to_owned),
            blocked: jump.blocked(),
            pressed_ticks: jump.pressed_ticks,
        }
    }

    /// A jump that never left the client (no physics, or the primitive refused). Carries the reason so the model
    /// can tell "the body could not jump" from "the body jumped and stayed down".
    fn not_sent(why: impl Into<String>) -> Self {
        Self {
            kind: "jump_not_sent",
            sent: false,
            y_before: 0.0,
            y_max: 0.0,
            y_after: 0.0,
            rose: false,
            why: Some(why.into()),
            blocked: None,
            pressed_ticks: 0,
        }
    }

    /// Nothing to hop onto: no key was pressed, so this is neither a jump nor a failure of the body (t85).
    /// Without it the heal step aimed straight up, `hop_toward` got the vector `(0, 0)` and the press only
    /// lifted the feet back down onto the same block (t84: `sent=true`, `rise_ground=+1.252`, `gained=0.0`).
    fn no_step(here: [i32; 3]) -> Self {
        let y = here[1] as f32;
        Self {
            kind: "no_step_candidate",
            sent: false,
            y_before: y,
            y_max: y,
            y_after: y,
            rose: false,
            // No refusal to explain: the clause itself is the whole fact.
            why: None,
            blocked: None,
            pressed_ticks: 0,
        }
    }

    /// The rise in **blocks**, the same unit the bot layer uses (t99).
    ///
    /// `70.5625 -> 71.0` is one block climbed, not `0.4375`: t98 read the float and reported a successful hop as
    /// `jump_landed_back`. Whatever the bot layer already computes (`Jump::gained`, `from_ground_block`,
    /// `to_ground_block`) is mirrored here rather than re-derived.
    fn gained(&self) -> f32 {
        (self.y_after.floor() - self.y_before.floor()) as f32
    }

    /// The block the feet started on — `floor(y_before)`, the grid `target` speaks.
    fn from_ground_block(&self) -> i32 {
        self.y_before.floor() as i32
    }

    /// The block the feet ended on — `floor(y_after)`.
    fn to_ground_block(&self) -> i32 {
        self.y_after.floor() as i32
    }

    /// Only an input that really left the client may be reported as something the walk tried.
    fn observed(&self) -> bool {
        self.sent
    }

    fn to_json(&self) -> Value {
        json!({
            "kind": self.kind,
            "sent": self.sent,
            "y_before": self.y_before,
            "y_max": self.y_max,
            "y_after": self.y_after,
            "gained": self.gained(),
            "from_ground_block": self.from_ground_block(),
            "to_ground_block": self.to_ground_block(),
            "rose": self.rose,
            "why": self.why,
            "blocked": self.blocked,
            "pressed_ticks": self.pressed_ticks,
        })
    }

    /// One clause for the `blocked_by_terrain` message: whether it jumped at all, and which block it ended on.
    ///
    /// t99: the numbers are ground blocks now (`64 -> 65`), not floats (`64.00 -> 65.25`), because `64.00 -> 64.43`
    /// is exactly the reading that hid a climbed block from t98.
    fn describe(&self) -> String {
        let why = self.why.as_deref().map(|reason| format!(" ({reason})")).unwrap_or_default();
        let (from, to) = (self.from_ground_block(), self.to_ground_block());
        match self.kind {
            "no_step_candidate" => format!("there was nowhere to hop: no neighbour is a step one block up with room above it{why}"),
            "jump_not_sent" => format!("it never jumped{why}"),
            "jump_ok" => format!("a jump went out and ended a block higher ({from} -> {to}){why}"),
            "jump_up_landed" => {
                format!("a jump went out and got up a block, but not onto the aimed step ({from} -> {to}){why}")
            }
            "jump_landed_back" => {
                format!("a jump went out and came down on the block it started from ({from} -> {to}){why}")
            }
            "jump_fell" => format!("a jump went out and ended a block lower ({from} -> {to}){why}"),
            // t105: nothing was pressed. The ceiling, not the body, is what has to change.
            "no_headroom" => match self.blocked {
                Some(head) => {
                    format!("no key was pressed: {head:?} is solid above the step — dig that one first, then walk again")
                }
                None => "no key was pressed: the cell above the step is solid".to_owned(),
            },
            "hop_no_progress" => format!("no key was pressed: the same hop from the same cell already gained nothing{why}"),
            other => format!("a jump went out ({other}) and ended at ground block {to} (started {from}){why}"),
        }
    }
}

/// Adds `jump_last` to a tool result when the walk observed a jump. Kept as a function so the "success carries the
/// record too" promise is testable without a server (t89).
fn attach_jump(mut value: Value, jump_last: Option<&JumpRecord>) -> Value {
    if let Some(jump) = jump_last {
        value["jump_last"] = jump.to_json();
    }
    value
}

/// The one line the model reads when a hop was refused because the cell above the step is solid (t105).
///
/// Same shape as the `not_visible` occlusion hint (`block [x, y, z] is behind [a, b, c] — dig that one first`): name
/// the cell, say "dig it first, then repeat the move". A model that learned one of the two has learned both.
fn no_headroom_hint(head: [i32; 3]) -> String {
    format!("跳不上去是因为头顶那格是实心的：先 dig {head:?} 把洞挖开，再原样 move_to 一次（别在原地连跳）")
}

/// What to tell the model when the terrain blocks it (t89/t97/t105), extracted so the wording is pinned by a test.
///
/// The order is the order of the model's next action: a heal that already worked means the route is wrong, not the
/// body; a ceiling means one named cell has to go first; nowhere to hop means shift sideways; anything else is a
/// detour. `no_headroom` deliberately speaks in the same shape as the `not_visible` occlusion hint
/// (`block … is behind … — dig that one first`): one grammatical pattern the model has to learn once.
fn blocked_by_terrain_hint(healed_once: bool, jump_last: Option<&JumpRecord>) -> String {
    if healed_once {
        // A heal already worked once and the walk is still blocked: the next jump would only repeat it, so the
        // model should question the goal or the route, not the body (t97).
        return "自愈已经成功过一次（healed_once）却仍然被挡：这段路或这个目标本身有问题——换个入口/换 xz 目标，别再加 `timeout_s` 硬等"
            .to_owned();
    }
    if let Some(head) = jump_last.and_then(|jump| jump.attempt.blocked) {
        // t105: the ceiling above the step, not the step itself, is what has to change — and the exact cell is known.
        return no_headroom_hint(head);
    }
    if jump_last.is_some_and(|jump| jump.attempt.kind == "no_step_candidate") {
        // Nothing to hop onto: a jump would only lift the feet and drop them back (t85).
        return "这一格四周没有能跳上去的台阶（上方无空间或脚下没台阶）：先水平挪一格（move_to 换 xz），或 allow_dig:true 挖开"
            .to_owned();
    }
    "地形挡住了：换个方向绕（侧向 3~5 格再走），或 allow_dig:true 让它挖开".to_owned()
}

/// The failure a walk that ran out of its time budget returns (t89). Extracted so the payload — the steps that were
/// tried, where the body stood, and what the last jump did — is pinned by a test instead of only by a real run.
fn walk_timeout_failure(
    budget_s: u64,
    tried: Vec<&'static str>,
    stuck_at: Option<[i32; 3]>,
    target: Option<[i32; 3]>,
    jump_last: Option<&JumpRecord>,
    healed_once: bool,
) -> Failure {
    let blocks_short = stuck_at
        .map(|b| b.iter().zip(target.unwrap_or(b)).map(|(a, b)| (a - b).abs()).sum::<i32>())
        .unwrap_or(0);
    let jumped = jump_last.map(|jump| format!("; {}", jump.attempt.describe())).unwrap_or_default();
    let hint = if jump_last.is_some_and(|jump| jump.attempt.kind == "jump_ok") {
        "跳跃是成功的（jump_last.kind=jump_ok），只是这段路没在预算内走完：把路拆成两段 move_to，或加大 timeout_s"
    } else if let Some(head) = jump_last.and_then(|jump| jump.attempt.blocked) {
        // t105: the same wording as `blocked_by_terrain` — one grammatical pattern for one situation.
        return Failure::new(
            "timeout",
            format!(
                "the walk ran out of its {budget_s} s budget at {stuck_at:?} ({blocks_short} blocks short of {target:?}); tried {tried:?}{jumped}"
            ),
        )
        .with_details(json!({
            "tried": tried,
            "stuck_at": stuck_at,
            "target": target,
            "jump_last": jump_last.map(JumpRecord::to_json),
            "healed_once": healed_once,
        }))
        .with_hint(no_headroom_hint(head));
    } else if jump_last.is_some_and(|jump| jump.attempt.kind == "no_step_candidate") {
        "这一格四周没有能跳上去的台阶：先水平挪一格（move_to 换 xz），或 allow_dig:true 挖开"
    } else {
        "时间预算用完了：把这段路拆成两段 move_to，或加大 timeout_s；stuck_at 是最后停住的位置"
    };
    Failure::new(
        "timeout",
        format!(
            "the walk ran out of its {budget_s} s budget at {stuck_at:?} ({blocks_short} blocks short of {target:?}); tried {tried:?}{jumped}"
        ),
    )
    .with_details(json!({
        "tried": tried,
        "stuck_at": stuck_at,
        "target": target,
        "jump_last": jump_last.map(JumpRecord::to_json),
        "healed_once": healed_once,
    }))
    .with_hint(hint)
}

/// The state of a jump attempt, decided from the observation alone (t99).
///
/// t82 decided this from the *net* float gain and called everything short of a block `jump_no_effect`. t84 showed a
/// jump that rose 1.25 blocks and came back down onto the same block; t85 split that case by the peak. t96 then moved
/// the whole question to the block grid in the bot crate — and t98 proved the agent still had its own float copy:
/// this server's standing height is `y = 70.5625` and the step above it is `y = 71.0`, so a real block climbed
/// measured `0.4375` here and read as `jump_landed_back`.
///
/// The block grid now decides, in the same four mutually exclusive terms as `acacia_bot::movement::Jump::kind`
/// (pinned against it by `jump_tests::the_block_verdict_agrees_with_the_bot_layer`):
/// * `jump_ok` — ended a block higher, on the aimed cell (or with no cell aimed at);
/// * `jump_up_landed` — ended a block higher, but not on the aimed cell;
/// * `jump_landed_back` — ended on the block it started from (a press that never lifted reads the same way, with
///   `why` carrying the primitive's `no_liftoff`);
/// * `jump_fell` — ended lower than it started.
///
/// `sent == false` stays `jump_not_sent` (nothing left the client) and there is no birth of a `jump_no_liftoff`: the
/// peak no longer decides anything, so the double threshold went with it.
fn jump_kind(sent: bool, y_before: f32, y_after: f32, target: Option<[i32; 3]>) -> &'static str {
    if !sent {
        return "jump_not_sent";
    }
    let (from, to) = (y_before.floor() as i32, y_after.floor() as i32);
    if to > from {
        match target {
            Some(target) if to != target[1] => "jump_up_landed",
            _ => "jump_ok",
        }
    } else if to == from {
        "jump_landed_back"
    } else {
        "jump_fell"
    }
}

/// Whether a hop really lifted the body onto the next block (t97).
///
/// The test is on feet heights, not on the `kind` names: t96 is splitting `jump_landed_back` into
/// `jump_ok`/`jump_up_landed`/`jump_landed_back` and measuring `gained` in whole blocks, so a check written against
/// `kind == "jump_ok"` would read false today and mean something else tomorrow. A whole block of gain, or the ground
/// cell under the feet moving up, both say the step was climbed; `70.5625 -> 71.5625` is `gained 1.0`, while a hop
/// that rises and lands back on the same block is neither.
fn jump_climbed(attempt: &JumpAttempt) -> bool {
    const EPSILON: f32 = 1e-3;
    attempt.sent
        && (attempt.gained() >= 1.0 - EPSILON || attempt.y_after.floor() - attempt.y_before.floor() >= 1.0)
}

/// Records a jump for the model. A step that was never sent must not claim to have been tried (t82).
fn note_jump(tried: &mut Vec<&'static str>, attempt: &JumpAttempt) {
    if attempt.observed() {
        tried.push("jump");
    }
}

/// The eight horizontal neighbours, in the order that breaks ties: axes before diagonals.

/// The cell the jump step aims at: a neighbour one block up that the body can actually stand on.
///
/// t85: this used to be "one block up, one step towards the goal", which with a goal directly above degenerates
/// to the cell the body already stands in — `hop_toward` then gets the movement vector `(0, 0)` (relay packet 475
/// in t84) and the press lifts the feet straight up and back down: `sent=true`, `rise_ground=+1.252`, `gained=0.0`.
/// A vertical jump heals nothing, so the step now refuses to aim at one.
///
/// A candidate is a horizontal neighbour `c` with `c` and `c + up` air and `c - up` (the step block) solid, i.e. a
/// block whose top face is one above the feet. Candidates are ordered by the angle between `c - here` and
/// `toward - here`, then by the xz distance left to the goal, so the hop goes *towards* the goal when it can.
/// `None` means there is no step to hop onto; the caller reports `no_step_candidate` without pressing anything.
fn jump_target<F>(here: [i32; 3], toward: [i32; 3], name_at: F) -> Option<[i32; 3]>
where
    F: Fn([i32; 3]) -> Option<String>,
{
    // t93 moved the rule itself into the bot crate: the hop aims at the *nearest* standable step (Chebyshev ring
    // first) and only breaks ties inside a ring by the angle to the goal. Ordering by that angle first is what sent
    // t91b's hop three blocks past the plank under the bot's nose.
    acacia_bot::movement::jump_target(here, toward, name_at)
}

/// The healing steps, in order: a jump up towards the target, digging the cell in front (only with `allow_dig`), then
/// a sideways detour. Exposed for the unit test.
fn heal_steps(allow_dig: bool) -> Vec<&'static str> {
    let mut steps = vec!["jump"];
    if allow_dig {
        steps.push("dig");
    }
    steps.push("detour");
    steps
}

/// The healing step to run next, or `None` when the round has spent them all (t97).
///
/// The index follows what was *attempted*, not what was observed (t82): a jump that could not be sent still moves the
/// healing on. The one exception is the detour: it may run once per stuck round — after that, remaining stuck means
/// the route, not the step, is wrong.
fn next_step(steps: &[&'static str], attempted: &[&'static str]) -> Option<&'static str> {
    let step = *steps.get(attempted.len().min(steps.len() - 1))?;
    if step == "detour" && attempted.iter().any(|done| *done == "detour") {
        return None;
    }
    Some(step)
}

/// The walk's heal bookkeeping, split out so the t97 rules are pinned without a server.
///
/// Two facts live here: which steps this *round* has already started (the index into [`heal_steps`]), and whether a
/// heal has ever lifted the body (`healed_once`). A step that lifts the body does not continue into the next one — it
/// clears the round and hands control back to the pathfinder from the new height.
#[derive(Debug, Default, PartialEq)]
struct Heals {
    attempted: Vec<&'static str>,
    /// A heal worked at least once in this walk and the body still ended up blocked.
    healed: bool,
}

impl Heals {
    /// The step to run now, or `None` when the round has spent all of them.
    fn next(&self, allow_dig: bool) -> Option<&'static str> {
        next_step(&heal_steps(allow_dig), &self.attempted)
    }

    fn started(&mut self, step: &'static str) {
        self.attempted.push(step);
    }

    /// A heal lifted the body: the round is over, and a later failure must not name the step that worked.
    fn replanned(&mut self) {
        self.attempted.clear();
        self.healed = true;
    }

    /// A heal changed the *world* instead of the body (the dig step, t144): the round is over too, because the map
    /// the next step would have been judged against no longer exists — the walk has to re-plan from the new shape.
    /// Unlike [`Heals::replanned`] this does not claim the body was lifted: `healed` is what the report shows as
    /// "a heal worked", and digging a hole is not that.
    fn world_changed(&mut self) {
        self.attempted.clear();
    }

    /// How many steps this round started — the count the message reports.
    fn attempts(&self) -> usize {
        self.attempted.len()
    }
}

/// Walks to `goal` in chunks, healing when the body stops moving: `goto_with` on its own burns its whole budget
/// standing in front of a one-block step without saying anything (t68: six calls, 30 s each, zero blocks).
async fn walk_supervised(
    bot: &mut Bot,
    goal: Goal,
    opts: GotoOpts,
    toward: Option<[i32; 3]>,
    allow_dig: bool,
    deadline: Instant,
) -> Walk {
    let mut heals = Heals::default();
    let mut tried: Vec<&'static str> = Vec::new();
    // Kept across progress: the one record that survives a successful hop (t89).
    let mut jump_last: Option<JumpRecord> = None;
    let mut stuck_at = None;
    let mut last = bot.block_position();
    loop {
        let chunk_end = (Instant::now() + Duration::from_secs(WALK_CHUNK_SECS)).min(deadline);
        let chunk = tokio::time::timeout_at(chunk_end, bot.goto_with(goal.clone(), opts.clone())).await;
        match chunk {
            Ok(result) => {
                return Walk { outcome: result, tried, jump_last, attempts: heals.attempts(), stuck_at, timed_out: false, healed_once: heals.healed }
            }
            Err(_) if Instant::now() >= deadline => {
                return Walk {
                    outcome: Err(acacia_bot::ActionError::NotPossible("timed out".into())),
                    tried,
                    jump_last,
                    attempts: heals.attempts(),
                    stuck_at,
                    timed_out: true,
                    healed_once: heals.healed,
                };
            }
            Err(_) => {}
        }
        let now = bot.block_position();
        let moved = match (last, now) {
            (Some(a), Some(b)) => a.iter().zip(b).map(|(x, y)| (x - y) as f32).map(|d| d * d).sum::<f32>().sqrt(),
            _ => 0.0,
        };
        if moved >= WALK_PROGRESS {
            // Progress: the chunk was a real walk, so the next one starts clean. Only the *round* is cleared —
            // `jump_last` survives, because a hop that worked is exactly the evidence a later failure must show.
            heals.attempted.clear();
            tried.clear();
            stuck_at = None;
            last = now;
            continue;
        }
        stuck_at = now;
        // The step index follows what was *attempted*, not what was observed: a jump that could not be sent must
        // still move the healing on to digging or the detour instead of retrying itself forever.
        // One detour per stuck round is the whole allowance: it is a guess about direction, not a search, and
        // repeating it only burns the budget. Before t97 the index saturated on "detour" and the walk kept circling
        // until the deadline — the exact shape that carried t95's successful hop back down the step.
        let Some(step) = heals.next(allow_dig) else {
            return Walk {
                outcome: Err(acacia_bot::ActionError::NotPossible("no progress".into())),
                tried,
                jump_last,
                attempts: heals.attempts(),
                stuck_at,
                timed_out: false,
                healed_once: heals.healed,
            };
        };
        heals.started(step);
        let attempts = heals.attempts();
        let mut solved = false;
        if let Some(aim) = toward.or(now) {
            match step {
                "jump" => {
                    let here = now.unwrap_or(aim);
                    // The borrow of `bot` ends with this statement, before the jump below needs it mutable.
                    let target = jump_target(here, aim, |cell| block_name(bot, cell));
                    let attempt = match target {
                        // The walk's digging permission, read from the same `GotoOpts` the pathfinder was given
                        // rather than from a second copy of the flag: `allow_dig:true` must be able to widen the hole
                        // it stands in, or the hop can only ever answer `no_headroom` (t105).
                        Some(target) => jump_step(bot, target, opts.search.allow_dig).await,
                        // No step to hop onto: say so instead of pressing jump in place.
                        None => JumpAttempt::no_step(here),
                    };
                    // Recorded once, after the retry: a hop that never pressed a key must not enter `tried` (t82),
                    // and the retry must not enter it twice.
                    note_jump(&mut tried, &attempt);
                    // The one question this round asks: did the body end up a block higher (t97)?
                    solved = jump_climbed(&attempt);
                    jump_last = Some(JumpRecord::new(target, attempt));
                }
                other => {
                    // t144: the dig step breaks the cells in the way and hands the walk back to the planner from the
                    // new geometry. Before this, a dig that opened one cell was followed by the detour and then by the
                    // next round's jump from the same spot — the live shape of "dig one, jump, dig one", 1288 `jump=1`.
                    let broke = heal(bot, other, aim, opts.clone()).await;
                    tried.push(other);
                    if broke {
                        heals.world_changed();
                        stuck_at = None;
                    }
                }
            }
        }
        if solved {
            // The step worked, so this round's failures no longer describe the situation: drop them (t97 — `tried`
            // means "means that failed this round", and a jump that lifted the body is not one) and re-plan from the
            // new height. Letting the flow fall through into the detour is what undid the hop in t95: the detour only
            // knows the goal's direction, so it walked the body back down to y=70.5625 and the walk ended where it
            // started, one block lower than its own success.
            heals.replanned();
            tried.clear();
            stuck_at = None;
            // Standing on the step can be the goal itself: `move_to` at the block just above the feet. Report the
            // walk as done — asking `goto_with` to path towards the cell the body already stands in only drifts.
            if matches!((&goal, bot.block_position()), (Goal::Block(cell), Some(here)) if *cell == here) {
                return Walk {
                    outcome: Ok(()),
                    tried,
                    jump_last,
                    attempts,
                    stuck_at,
                    timed_out: false,
                    healed_once: heals.healed,
                };
            }
        }
        last = bot.block_position();
    }
}

/// The body a jump step drives, split out so the step can be proven to call it without a server.
trait Leaper {
    /// A hop onto `target`. `allow_dig` is the walk's own digging permission, forwarded unchanged: a `move_to` with
    /// `allow_dig:true` that lands in a one-block hole must be able to widen it instead of only reporting the ceiling
    /// (t105).
    async fn leap_toward(
        &mut self,
        target: [i32; 3],
        allow_dig: bool,
    ) -> Result<acacia_bot::movement::Jump, acacia_bot::ActionError>;

    /// Break the cell that blocks the hop's headroom. `true` when the break was issued without error.
    async fn clear_headroom(&mut self, cell: [i32; 3]) -> bool;
}

impl Leaper for Bot {
    async fn leap_toward(
        &mut self,
        target: [i32; 3],
        allow_dig: bool,
    ) -> Result<acacia_bot::movement::Jump, acacia_bot::ActionError> {
        // The one real implementation: a hop that presses `Controls.jump` until liftoff and reports the rise. Since
        // t102 the primitive itself checks headroom, refuses to press into a ceiling and (with `allow_dig`) digs the
        // blocking cell before re-planning.
        self.hop_toward_with(target, allow_dig).await
    }

    async fn clear_headroom(&mut self, cell: [i32; 3]) -> bool {
        self.break_block(cell).await.is_ok()
    }
}

/// One jump at the step in front. An error is still reported as `jump_not_sent` (with the reason), so a body that
/// cannot jump at all stays distinguishable from one that jumped and stayed down.
async fn jump_step<L: Leaper + ?Sized>(leaper: &mut L, target: [i32; 3], allow_dig: bool) -> JumpAttempt {
    let first = match leaper.leap_toward(target, allow_dig).await {
        Ok(jump) => JumpAttempt::from_jump(&jump),
        Err(error) => return JumpAttempt::not_sent(error.to_string()),
    };
    // t105: `no_headroom` is not the body failing to jump — the ceiling above the step is solid, and the hop did not
    // press a key at all. With digging allowed the fix is one dig at the named cell and one more ask; a second
    // `no_headroom` means digging cannot help here (unbreakable, unloaded, or a block fell back in), so the walk gives
    // up on jumping and moves on to dig/detour in its own order instead of pressing in place.
    if first.kind != "no_headroom" || !allow_dig {
        return first;
    }
    let Some(head) = first.blocked else { return first };
    if !leaper.clear_headroom(head).await {
        return first;
    }
    match leaper.leap_toward(target, allow_dig).await {
        Ok(jump) => JumpAttempt::from_jump(&jump),
        Err(error) => JumpAttempt::not_sent(error.to_string()),
    }
}

/// One healing action: dig the blocker, or step aside. The jump has its own step ([`jump_step`]) because it is
/// the only one whose outcome is observed.
/// Runs one heal step. `true` when it changed the world (the dig step broke something): the caller then hands the
/// walk back to the pathfinder instead of spending the rest of the round on guesses against a map that moved.
async fn heal(bot: &mut Bot, step: &str, toward: [i32; 3], opts: GotoOpts) -> bool {
    let here = bot.block_position().unwrap_or(toward);
    let (dx, dz) = (
        if toward[0] > here[0] { 1 } else if toward[0] < here[0] { -1 } else { 0 },
        if toward[2] > here[2] { 1 } else if toward[2] < here[2] { -1 } else { 0 },
    );
    if step == "dig" {
        // The cell at feet and head height in the walking direction: a one or two block step.
        let mut broke = false;
        for dy in [0, 1] {
            let cell = [here[0] + dx, here[1] + dy, here[2] + dz];
            broke |= bot.break_block(cell).await.is_ok();
        }
        return broke;
    }
    // Sideways: the perpendicular neighbours first, but only the ones the body can actually stand in (t93 — the old
    // code turned the goal direction 90° and walked into whatever was there). Two tries, then give up.
    let candidates = acacia_bot::movement::detour_candidates(here, toward, |cell| block_name(bot, cell));
    for cell in candidates.into_iter().take(2) {
        let _ = tokio::time::timeout(Duration::from_secs(4), bot.goto_with(Goal::Block(cell), opts.clone())).await;
    }
    false
}

fn wrap_yaw(yaw: f32) -> f32 {
    let wrapped = yaw.rem_euclid(360.0);
    if wrapped > 180.0 { wrapped - 360.0 } else { wrapped }
}

fn block_name(bot: &Bot, [x, y, z]: [i32; 3]) -> Option<String> {
    let world = bot.world()?;
    let (view, registry) = (world.view()?, world.registry()?);
    registry.get(view.block(x, y, z)).map(|state| state.name.to_owned())
}

/// The tool table a [`Table`] names, for the registry listing and the trace.
pub fn table_of(name: &str) -> Option<Table> {
    crate::tools::spec(name).map(|spec| spec.table)
}

/// The [`crate::tools::ToolBox`] the loop drives: a thin wrapper over the shell channel.
#[derive(Clone)]
pub struct ShellTools {
    pub shell: ShellHandle,
}

impl ShellTools {
    pub fn new(shell: ShellHandle) -> Self {
        Self { shell }
    }
}

impl crate::tools::ToolBox for ShellTools {
    async fn call(&mut self, call: Call) -> Result<Value, Failure> {
        self.shell.call(call).await
    }

    async fn reconnect(&mut self) -> bool {
        let (address, name) = {
            let state = self.shell.state();
            (state.address.clone(), state.name.clone())
        };
        match (address, name) {
            (Some(address), Some(name)) => self.shell.connect(address, name, 60).await.is_ok(),
            _ => false,
        }
    }

    async fn state(&mut self) -> Value {
        let mut state = self.shell.state();
        let Some(perception) = state.perception.clone() else {
            return json!({"connected": false, "last_error": state.last_error});
        };
        // The first body the model gets is the whole (brief) snapshot: a delta on top of a body it
        // never saw reads as "nothing changed" and leaves it blind on step one.
        let (rendered, full) = perceive::render_observe(&perception, Detail::Brief, true, state.full_observe_sent);
        if full {
            state.full_observe_sent = true;
        }
        rendered
    }
}

/// `pickup` used to answer every failure with `no_block`, a reason from the block branch (t69: eleven tries,
/// drops 0.2 blocks away). The four reasons below are what the model can act on.
#[test]
fn a_pickup_failure_names_what_went_wrong() {
    // The entity is gone from the snapshot: the drop was taken, despawned or merged.
    assert_eq!(pickup_reason(false, "anything"), "entity_gone");
    // The entity is still there but unreachable: a real pathing failure the model must route around.
    assert_eq!(pickup_reason(true, "no path to the item"), "no_path");
    assert_eq!(pickup_reason(true, "unreachable target"), "no_path");
    assert_eq!(pickup_reason(true, "could not reach the entity"), "no_path");
    // Anything else is the server's refusal (or a watchdog): worth one retry, not a re-plan.
    assert_eq!(pickup_reason(true, "picking up entity 2347 timed out"), "rejected");
    assert_eq!(pickup_reason(true, "the item is stuck in a wall"), "rejected");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: u64, kind: &str, distance: f32) -> Target {
        Target { runtime_id: id, kind: kind.to_owned(), distance }
    }

    #[test]
    fn the_default_goes_through_fight_not_a_single_swing() {
        // t19: a 7.7-block target answered out_of_reach on the first swing, because the tool swung
        // instead of fighting. The default plan is `Bot::fight`, which walks in (or swings when the
        // target is already close) — the reach check no longer gates the call.
        assert_eq!(plan(false), Plan::Fight);
        // `once:true` is the explicit "just hit it" switch: no approach, out_of_reach is honest.
        assert_eq!(plan(true), Plan::Swing);
    }

    #[test]
    fn nearest_picks_the_closest_entity() {
        let picked = pick_target(
            "nearest",
            16.0,
            vec![candidate(1, "minecraft:zombie", 9.0), candidate(2, "minecraft:skeleton", 3.0)],
        );
        assert_eq!(picked.expect("a target").runtime_id, 2);
        assert!(pick_target("nearest", 2.0, vec![candidate(1, "minecraft:zombie", 9.0)]).is_none());
    }

    #[test]
    fn a_kind_matches_with_and_without_the_namespace() {
        let entities = || vec![candidate(7, "minecraft:zombie", 4.0), candidate(8, "minecraft:skeleton", 2.0)];
        assert_eq!(pick_target("zombie", 16.0, entities()).expect("zombie").runtime_id, 7);
        assert_eq!(pick_target("minecraft:skeleton", 16.0, entities()).expect("skeleton").runtime_id, 8);
        assert!(pick_target("creeper", 16.0, entities()).is_none());
        assert!(pick_target("creeper", 1.0, vec![candidate(9, "minecraft:creeper", 12.0)]).is_none());
    }

    #[test]
    fn a_wide_radius_is_reported_not_silently_cut() {
        // t69 asked for 160, scanned 64, and read the miss as "there is no bed".
        assert_eq!(find_scan_radius(160), (160, 64));
        assert_eq!(find_scan_radius(64), (64, 64));
        assert_eq!(find_scan_radius(32), (32, 32));
        assert_eq!(find_scan_radius(0), (1, 1));
    }

    #[test]
    fn a_miss_names_the_band_it_scanned() {
        // t123: three runs answered `{total:0, scanned:105625}` and the report had to explain by hand that
        // the *window* was empty, not the world. The answer now says where it looked.
        let hint = find_hint(0, "*_bed", 32, 32, 105_625, 64).expect("a miss always explains itself");
        assert!(hint.contains("y=[56,80]"), "the band, not just the total: {hint}");
        assert!(hint.contains("radius=32"), "and the radius that was used: {hint}");
        assert!(hint.contains("105625 cells"), "and how much was actually visited: {hint}");
        assert!(hint.contains("another floor"), "and why a bed one level up is invisible: {hint}");
        assert!(hint.contains("*_bed"), "the selector that ran is still named: {hint}");
    }

    #[test]
    fn the_band_follows_the_bot() {
        assert_eq!(find_scan_y(64), (56, 80));
        assert_eq!(find_scan_y(70), (62, 86));
        assert_eq!(find_scan_y(-3), (-11, 13));
        let hint = find_hint(0, "minecraft:stone", 8, 8, 6_375, 70).expect("a miss");
        assert!(hint.contains("y=[62,86]"), "the band moves with the feet: {hint}");
    }

    #[test]
    fn a_hit_with_the_asked_radius_says_nothing_extra() {
        // Success must not grow a paragraph: no y band, no advice.
        assert_eq!(find_hint(3, "*_bed", 32, 32, 105_625, 64), None);
        assert_eq!(find_hint(1, "minecraft:stone", 8, 8, 6_375, 70), None);
    }

    #[test]
    fn a_hit_on_a_clamped_radius_still_says_it_was_cut() {
        let hint = find_hint(2, "*_bed", 160, 64, 105_625, 64).expect("a cut radius is always named");
        assert!(hint.contains("64"), "the cap is named on a hit: {hint}");
        assert!(!hint.contains("y=["), "a hit does not repeat the band: {hint}");
    }

    #[test]
    fn a_miss_on_a_clamped_radius_keeps_both_facts() {
        let hint = find_hint(0, "*_bed", 160, 64, 105_625, 64).expect("a miss");
        assert!(hint.contains("y=[56,80]"), "the band: {hint}");
        assert!(hint.contains("capped at 64 blocks"), "and that the radius was cut: {hint}");
    }

    #[test]
    fn a_miss_without_a_loaded_world_does_not_blame_the_window() {
        let hint = find_hint(0, "*_bed", 32, 32, 0, 64).expect("a miss");
        assert!(hint.contains("no cells were scanned"), "0 scanned is not '0 beds in that band': {hint}");
    }

    #[test]
    fn a_trader_target_can_be_a_kind() {
        assert_eq!(trader_kind("minecraft:villager_v2"), "minecraft:villager_v2");
        assert_eq!(trader_kind("villager"), "minecraft:villager");
        assert_eq!(trader_kind("WANDERING_TRADER"), "minecraft:wandering_trader");
    }

    #[test]
    fn a_bed_is_any_colour_bed() {
        // t69: `minecraft:white_bed` matched nothing while a red bed stood next to the bot.
        assert_eq!(find_selector("bed"), "*_bed");
        assert_eq!(find_selector("BED"), "*_bed");
        assert_eq!(find_selector("*_bed"), "*_bed");
        assert_eq!(find_selector("minecraft:*_bed"), "minecraft:*_bed");
        assert_eq!(find_selector("white_bed"), "minecraft:white_bed");
        assert_eq!(find_selector("minecraft:white_bed"), "minecraft:white_bed");
        // The alias and the wildcard must select exactly the same blocks.
        for name in ["minecraft:white_bed", "minecraft:red_bed", "minecraft:light_blue_bed"] {
            assert!(block_matches(&find_selector("bed"), name), "bed covers {name}");
            assert!(block_matches("*_bed", name), "*_bed covers {name}");
        }
        assert!(!block_matches(&find_selector("bed"), "minecraft:white_wool"));
        // Case-insensitive on both sides, and a bare name gets its namespace.
        assert!(block_matches("*_BED", "minecraft:white_bed"));
        assert!(block_matches("minecraft:white_bed", "minecraft:white_bed"));
        assert!(block_matches(&find_selector("stone"), "minecraft:stone"));
    }

    #[test]
    fn a_target_is_a_runtime_id_or_nearest() {
        // Parsing does not need a bot: the id form is accepted, anything else that is not "nearest" is
        // an argument error the model can fix (a typo must not become "no villager here").
        assert_eq!("1444".parse::<u64>().ok(), Some(1444));
        assert_eq!("nearest".parse::<u64>().ok(), None);
    }

    #[test]
    fn a_recipe_without_a_2x2_form_needs_a_table() {
        // The three failure reasons must not blur: no recipe at all is not "needs a table", and a
        // recipe that fits 2x2 may run without one (t54: stone_pickaxe silently went 2x2).
        assert!(needs_table(true, false), "3x3 only: needs_table");
        assert!(!needs_table(true, true), "a 2x2 recipe may craft in the grid");
        assert!(!needs_table(false, false), "no recipe at all stays no_recipe");
    }

    #[test]
    fn place_auto_picks_air_cells_with_a_floor_and_skips_the_bot() {
        let feet = [10, 64, 10];
        // A flat floor at y=63 everywhere; one cell occupied by a boulder at [11,64,10].
        let name_at = |pos: [i32; 3]| -> Option<String> {
            let name = if pos == [11, 64, 10] {
                "minecraft:stone"
            } else if pos[1] == 63 {
                "minecraft:grass_block"
            } else {
                "minecraft:air"
            };
            Some(name.to_owned())
        };
        let cells = place_candidates(feet, 4, name_at);
        assert!(!cells.is_empty());
        assert!(!cells.contains(&feet), "the bot's own cell is never a candidate");
        assert!(!cells.contains(&[11, 64, 10]), "an occupied cell is skipped");
        // Nearest first: a direct neighbour of the feet, at feet level or one below.
        let first = cells[0];
        let dx = (first[0] - feet[0]).abs();
        let dz = (first[2] - feet[2]).abs();
        assert!(dx <= 1 && dz <= 1, "first candidate is adjacent: {first:?}");
        // Every candidate has air above and something solid below.
        for cell in cells.iter().take(6) {
            assert_eq!(name_at([cell[0], cell[1] + 1, cell[2]]).as_deref(), Some("minecraft:air"));
            assert_ne!(name_at([cell[0], cell[1] - 1, cell[2]]).as_deref(), Some("minecraft:air"));
        }
    }

    #[test]
    fn place_auto_finds_nothing_on_a_floorless_world() {
        let name_at = |pos: [i32; 3]| -> Option<String> {
            Some(if pos[1] == 63 { "minecraft:stone" } else { "minecraft:air" }.to_owned())
        };
        // Every cell above the floor has air above it, but the cell *itself* at y=64 has floor at 63 —
        // so a world with a floor everywhere does find cells; a world of pure air does not.
        assert!(!place_candidates([10, 64, 10], 2, name_at).is_empty());
        let air = |_: [i32; 3]| Some("minecraft:air".to_owned());
        assert!(place_candidates([10, 64, 10], 2, air).is_empty());
    }

    #[test]
    fn climb_out_reports_already_out_when_there_is_no_pit() {
        // Outside a pit the tool must not dig anything: it answers already_out with steps 0.
        let value = climb_json(true, 64.0, Some(64.0), Some(64.0), 0, true, true);
        assert_eq!(value["already_out"], true);
        assert_eq!(value["steps"], 0);
        assert_eq!(value["surface_y"], 64.0);
        // Inside a pit it reports the two heights so the model can tell it really moved up.
        let value = climb_json(false, 64.0, Some(60.0), Some(63.0), 2, true, true);
        assert_eq!(value["already_out"], false);
        assert_eq!(value["surface_y"], 64.0);
        assert_eq!(value["from_y"], 60.0);
        assert_eq!(value["to_y"], 63.0);
        assert_eq!(value["landed"], true);
        assert_eq!(value["out"], true);
    }

    #[test]
    fn a_climb_aims_at_the_recorded_surface_not_one_step_up() {
        // t35: the old climb stopped as soon as a side opened, so the bot sank 3 blocks a run.
        // The aim is "where it last stood", which is what `climb_out_to` is for.
        assert_eq!(climb_target(Some(70), Some(64.0), 2.0), 70.0, "explicit wins");
        assert_eq!(climb_target(None, Some(64.0), 2.0), 64.0, "recalled surface next");
        assert_eq!(climb_target(None, None, 2.0), 5.0, "no baseline: three blocks up");
        // and the body always says which height was aimed at and whether it landed
        let value = climb_json(false, 64.0, Some(2.0), Some(5.0), 3, false, false);
        assert_eq!(value["surface_y"], 64.0);
        assert_eq!(value["landed"], false);
        assert_eq!(value["out"], false);
    }

    #[test]
    fn a_fight_that_ran_out_of_clock_is_recognised_and_positions_are_read() {
        assert!(fight_gave_up("the fight with entity 1444 did not end in 60s (9 swings, 0 retreats)"));
        assert!(!fight_gave_up("nothing to break"));
        assert_eq!(first_position("block [1, 2, 3] is behind other blocks"), Some([1, 2, 3]));
        assert_eq!(first_position("[North] of [10, 64, -2] is hidden from the eye"), Some([10, 64, -2]));
        assert_eq!(first_position("no coordinates here"), None);
    }

    #[test]
    fn the_swing_count_comes_back_from_the_bots_refusal() {
        let message = "the fight with entity 7 did not end in 45s (12 swings, 0 retreats)";
        assert_eq!(swings_in(message), Some(12));
        assert_eq!(swings_in("rejected by the server: nope"), None);
    }
}

#[cfg(test)]
mod arrival_tests {
    use super::inside_goal;

    #[test]
    fn the_healing_order_is_jump_then_dig_then_detour() {
        // t68: a route found but not walkable must be healed, then reported as terrain, not as a bare timeout.
        assert_eq!(super::heal_steps(false), vec!["jump", "detour"], "no digging without allow_dig");
        assert_eq!(super::heal_steps(true), vec!["jump", "dig", "detour"]);
    }

    #[test]
    fn the_server_catching_up_still_counts_as_arrived() {
        // t52: the client stood on the goal and the server was two blocks behind (one of them y) and the walk was
        // reported as `client_stale`. The box grows with the drift, and y is ignored unless an exact cell was asked.
        let goal = Some([150045, 65, 149856]);
        let server = Some([150043, 66, 149856]);
        assert!(inside_goal(goal, goal, None, false, 1.0), "the client itself is on the goal");
        assert!(!inside_goal(server, goal, None, false, 1.5), "two blocks of x are outside a 1.5 box");
        assert!(inside_goal(server, goal, None, false, 2.5), "but inside the drift-widened one");
        assert!(inside_goal(server, goal, None, true, 2.5), "an exact request also allows that one block of y");
        // The one block of y alone is enough when the request asked for it.
        assert!(!inside_goal(Some([150045, 66, 149856]), goal, None, true, 0.5));
        // An xz goal ignores y entirely.
        assert!(inside_goal(Some([150045, 70, 149856]), None, Some([150045, 149856]), false, 0.5));
    }
}

/// `pickup` failures (t106/t103): the walk digs by default, and when it still stops the model is
/// told the cell that stopped it instead of being told to retry. Offline: no bot, no server.
#[cfg(test)]
mod pickup_tests {
    use super::*;
    use acacia_bot::pathfind::PickupFailure;

    fn blocked_error(blocked: Option<[i32; 3]>) -> CollectError {
        CollectError::NotPickedUp(Box::new(PickupFailure {
            id: 2347,
            item: "minecraft:raw_iron".to_owned(),
            count: 1,
            at: Some([150018.5, 53.0, 149834.5]),
            blocked_at: blocked,
            last_walk: Some("cannot reach Near([150018, 53, 149834], 0.7)".to_owned()),
        }))
    }

    #[test]
    fn t106_a_blocked_pickup_names_the_cell_and_the_next_step() {
        // t103's live failure, replayed offline: the wall the bot itself had just mined around.
        let failure = pickup_failure(true, 2347, blocked_error(Some([150017, 54, 149834])), Some(2.5));
        let details = failure.details.clone().expect("details");
        assert_eq!(details["blocked_at"], json!([150017, 54, 149834]));
        assert_eq!(details["id"], json!(2347));
        assert_eq!(details["last_distance"], json!(2.5));
        let hint = failure.hint.clone().expect("hint");
        assert!(hint.contains("dig [150017, 54, 149834]"), "hint names the cell: {hint}");
        assert!(hint.contains("pickup {id: 2347}"), "hint names the retry call: {hint}");
        assert!(hint.contains("先") && hint.contains("再"), "hint is two steps in order: {hint}");
    }

    #[test]
    fn t106_a_failure_without_a_cell_keeps_the_old_wording() {
        let failure = pickup_failure(true, 2347, blocked_error(None), Some(2.8));
        let details = failure.details.clone().expect("details");
        assert!(details.get("blocked_at").is_none(), "no cell to name: {details}");
        assert_eq!(failure.hint.as_deref(), Some(PICKUP_GONE_HINT));
        // A drop the tracker lost keeps its own reason, and gets no dig advice.
        let gone = pickup_failure(false, 2347, CollectError::NotDrop(2347), None);
        assert_eq!(gone.reason, "entity_gone");
        assert_eq!(gone.hint.as_deref(), Some(PICKUP_GONE_HINT));
    }

    #[test]
    fn t106_the_message_keeps_the_text_the_classifier_reads() {
        // `acacia-bot`'s own sentence stays verbatim behind the old prefix: `pickup_reason` reads it,
        // and `cannot reach` (not `could not reach`) must stay `rejected`, not `no_path`.
        let failure = pickup_failure(true, 2347, blocked_error(Some([150017, 54, 149834])), None);
        assert!(failure.message.starts_with("picking up 2347 failed: the drop (entity 2347"), "{}", failure.message);
        assert!(failure.message.contains("blocked at [150017, 54, 149834]"), "{}", failure.message);
        assert_eq!(failure.reason, "rejected");
        assert_eq!(pickup_reason(true, &failure.message), "rejected");
        assert_eq!(pickup_reason(false, &failure.message), "entity_gone");
    }

    #[test]
    fn t106_digging_is_on_by_default_and_the_tool_owns_the_clock() {
        let dflt = collect_opts(PICKUP_ALLOW_DIG, 45);
        assert!(dflt.allow_dig, "a pickup must dig by default (t103)");
        assert_eq!(dflt.timeout, Duration::from_secs(45), "the tool's timeout_s, not the bot's 30 s");
        assert_eq!(dflt.tries, 8);
        assert_eq!(dflt.give_up_ticks, Some(60));
        // An explicit false keeps the pre-t103 walk exactly as it was: no digging, and now a
        // `blocked_at` for the caller to dig itself.
        let strict = collect_opts(false, 60);
        assert!(!strict.allow_dig);
        assert_eq!(strict.timeout, Duration::from_secs(60));
    }

    #[test]
    fn t106_the_wire_default_lets_the_walk_dig() {
        // The dispatch site reads one constant, so "is digging on for a plain `pickup`?" is a single
        // line to check when a live run cannot reach a drop it just mined next to.
        assert!(PICKUP_ALLOW_DIG);
    }
}

#[cfg(test)]
mod jump_tests {
    //! t82: the healing step that could claim to jump without jumping.
    //!
    //! run213 answered `blocked_by_terrain` with `details.tried:["jump"]` for a cell one block up, while the
    //! whole session log held two jump inputs, both from an unrelated staircase. These tests pin the two halves
    //! of the fix: the step really calls the primitive, and only an observed attempt reaches `tried`.
    use super::*;
    use acacia_bot::ActionError;

    /// A body that only records what it was asked to do, so a test can prove the step reached it.
    struct FakeLeaper {
        calls: Vec<[i32; 3]>,
        digs: Vec<[i32; 3]>,
        /// One reply per hop, in order; the last one repeats once the queue is down to a single entry.
        replies: Vec<Result<acacia_bot::movement::Jump, &'static str>>,
        /// Whether `clear_headroom` reports a successful break.
        digs_ok: bool,
        /// The `allow_dig` each hop was called with, so the walk's permission can be proven to reach the primitive.
        allow_digs: Vec<bool>,
    }

    impl FakeLeaper {
        fn jumping(y_before: f32, y_after: f32) -> Self {
            Self::scripted(vec![Ok(sample_jump(true, y_before, y_after, None))])
        }

        fn scripted(replies: Vec<Result<acacia_bot::movement::Jump, &'static str>>) -> Self {
            Self { calls: Vec::new(), digs: Vec::new(), replies, digs_ok: true, allow_digs: Vec::new() }
        }
    }

    impl Leaper for FakeLeaper {
        async fn leap_toward(
            &mut self,
            target: [i32; 3],
            allow_dig: bool,
        ) -> Result<acacia_bot::movement::Jump, ActionError> {
            self.calls.push(target);
            self.allow_digs.push(allow_dig);
            let reply = if self.replies.len() > 1 { self.replies.remove(0) } else { self.replies[0] };
            reply.map_err(|reason| ActionError::NotPossible(reason.to_owned()))
        }

        async fn clear_headroom(&mut self, cell: [i32; 3]) -> bool {
            self.digs.push(cell);
            self.digs_ok
        }
    }

    fn sample_jump(sent: bool, y_before: f32, y_after: f32, reason: Option<&'static str>) -> acacia_bot::movement::Jump {
        acacia_bot::movement::Jump {
            target: None,
            y_ground: None,
            sent,
            reason,
            y_before,
            y_max: y_before.max(y_after),
            y_after,
            rose: y_after.floor() > y_before.floor(),
            pressed_ticks: if sent { 6 } else { 0 },
            airborne_ticks: if sent { Some(4) } else { None },
        }
    }

    #[test]
    fn the_four_states_come_from_the_blocks_and_the_aimed_cell() {
        let step = Some([11, 65, 10]);
        // No input left the client: it never jumped, whatever the walk believed.
        assert_eq!(jump_kind(false, 64.0, 65.0, step), "jump_not_sent");
        // Sent and ended on the block it started from: a press that never lifted, or one that bounced.
        assert_eq!(jump_kind(true, 64.0, 64.0, step), "jump_landed_back");
        assert_eq!(jump_kind(true, 64.0, 64.02, step), "jump_landed_back");
        // Sent, ended a block higher, on the cell it aimed at.
        assert_eq!(jump_kind(true, 64.0, 65.0, step), "jump_ok");
        assert_eq!(jump_kind(true, 70.5625, 71.0, Some([11, 71, 10])), "jump_ok");
        // ... but onto some other block: the step it aimed at was not reached.
        assert_eq!(jump_kind(true, 64.0, 65.0, Some([11, 66, 10])), "jump_up_landed");
        // A body that ends lower than it started fell, and saying "landed back" hides that.
        assert_eq!(jump_kind(true, 64.0, 63.0, step), "jump_fell");
        assert_eq!(jump_kind(true, 65.25, 63.0, step), "jump_fell");
    }

    #[test]
    fn t99_a_climbed_block_is_not_a_landed_back_jump() {
        // The exact reading that lied in t98: standing on 70.5625, the step above at 71.0, so the float gain was
        // 0.4375 and the hop that had climbed a whole block was reported as `jump_landed_back`.
        let hop = JumpAttempt::from_jump(&sample_jump_at(70.5625, 71.0, Some([150012, 71, 149827])));
        assert_eq!(hop.kind, "jump_ok", "64.5625 -> 71.0 is a block climbed, not a bounce");
        assert_eq!(hop.gained(), 1.0, "gained is in blocks");
        assert_eq!(hop.from_ground_block(), 70);
        assert_eq!(hop.to_ground_block(), 71);
        let said = hop.describe();
        assert!(said.contains("70 -> 71"), "the message speaks in ground blocks: {said}");
        assert!(!said.contains("came down"), "and does not claim it came back down: {said}");
        // The float unit is what made this read wrong, so it must not come back through `gained`.
        assert_eq!(JumpAttempt::from_jump(&sample_jump(true, 64.0, 65.25, None)).gained(), 1.0);
    }

    #[test]
    fn t99_the_block_verdict_agrees_with_the_bot_layer() {
        // The agent keeps its own kind names (they are what `details.jump_last.kind` means to a model), but the
        // verdict itself must be the bot's: the same four block terms, derived the same way. If the bot layer ever
        // changes what a jump means, this test is where the agent finds out.
        let cases: [(f32, f32, [i32; 3]); 4] =
            [(64.0, 64.0, [11, 65, 10]), (64.0, 65.0, [11, 65, 10]), (64.0, 65.0, [11, 66, 10]), (64.0, 63.0, [11, 65, 10])];
        for (y_before, y_after, target) in cases {
            let mut jump = sample_jump(true, y_before, y_after, None);
            jump.target = Some(target);
            assert_eq!(
                jump_kind(jump.sent, jump.y_before, jump.y_after, jump.target),
                jump.kind(),
                "the agent and the bot disagree on {y_before} -> {y_after} towards {target:?}"
            );
        }
    }

    /// A jump that aimed at a cell, the way `hop_toward` reports it.
    fn sample_jump_at(y_before: f32, y_after: f32, target: Option<[i32; 3]>) -> acacia_bot::movement::Jump {
        let mut jump = sample_jump(true, y_before, y_after, None);
        jump.target = target;
        jump
    }


    #[test]
    fn a_jump_that_never_left_the_client_is_not_reported_as_tried() {
        // The exact run213 lie: `tried:["jump"]` with no jump input in the log.
        let mut tried = Vec::new();
        note_jump(&mut tried, &JumpAttempt::not_sent("not_on_ground"));
        assert!(tried.is_empty(), "an unsent jump must not appear in tried: {tried:?}");
        // A press that went out *is* a thing the walk tried, even when it gained nothing.
        note_jump(&mut tried, &JumpAttempt::from_jump(&sample_jump(true, 64.0, 64.0, Some("no_liftoff"))));
        assert_eq!(tried, vec!["jump"]);
        note_jump(&mut tried, &JumpAttempt::from_jump(&sample_jump(true, 64.0, 65.25, None)));
        assert_eq!(tried, vec!["jump", "jump"], "each observed attempt is its own entry");
    }

    #[tokio::test]
    async fn the_jump_step_calls_the_primitive_once_at_the_block_above() {
        // The whole point of t82: the branch must really reach `Bot::hop_toward`, not pathfind at the target.
        let mut body = FakeLeaper::jumping(64.0, 65.25);
        let attempt = jump_step(&mut body, [11, 65, 10], false).await;
        assert_eq!(body.calls, vec![[11, 65, 10]], "the primitive was called once, with the step cell");
        assert_eq!(attempt.kind, "jump_ok");
        assert!(attempt.sent && attempt.observed());
        assert_eq!(attempt.gained(), 1.0, "the rise is reported in blocks, not assumed");
    }

    #[tokio::test]
    async fn a_body_that_cannot_jump_still_reaches_the_primitive_and_says_why() {
        let mut body = FakeLeaper::scripted(vec![Err("jumping needs a physics bot")]);
        let attempt = jump_step(&mut body, [11, 65, 10], false).await;
        assert_eq!(body.calls.len(), 1, "the call happened; only the outcome was a refusal");
        assert_eq!(attempt.kind, "jump_not_sent");
        assert!(!attempt.observed());
        let why = attempt.why.as_deref().unwrap_or_default();
        assert!(why.contains("jumping needs a physics bot"), "the refusal is carried verbatim: {why}");
    }

    #[test]
    fn the_message_says_whether_it_jumped_and_which_block_it_ended_on() {
        // A model reading `blocked_by_terrain` must be able to tell "never tried" from "tried and failed", and t99
        // made the numbers ground blocks so that a climbed block cannot read as a bounce.
        let never = JumpAttempt::not_sent("not_on_ground").describe();
        assert!(never.contains("never jumped") && never.contains("not_on_ground"), "{never}");
        let down = JumpAttempt::from_jump(&sample_jump(true, 64.0, 64.02, Some("no_liftoff"))).describe();
        assert!(down.contains("went out") && down.contains("64 -> 64"), "{down}");
        assert!(down.contains("no_liftoff"), "the primitive's reason is carried: {down}");
        let up = JumpAttempt::from_jump(&sample_jump(true, 64.0, 65.25, None)).describe();
        assert!(up.contains("ended a block higher") && up.contains("64 -> 65"), "{up}");
        // The details carry the same facts as fields, for a driver that does not read prose.
        let json = JumpAttempt::from_jump(&sample_jump(true, 64.0, 65.25, None)).to_json();
        assert_eq!(json["kind"], "jump_ok");
        assert_eq!(json["sent"], true);
        assert_eq!(json["gained"], 1.0);
        assert_eq!(json["from_ground_block"], 64);
        assert_eq!(json["to_ground_block"], 65);
    }

    /// A world with a solid floor at y <= 63 and the given extra blocks (the steps) higher up.
    fn world_with(steps: &'static [[i32; 3]]) -> impl Fn([i32; 3]) -> Option<String> {
        move |pos: [i32; 3]| {
            let solid = pos[1] <= 63 || steps.contains(&pos);
            Some(if solid { "minecraft:stone" } else { "minecraft:air" }.to_owned())
        }
    }

    #[test]
    fn a_goal_straight_above_never_produces_a_vertical_jump() {
        // t84: goal = the cell above the body ⇒ the old `jump_target` returned that very cell ⇒ `hop_toward` got
        // `(0, 0)`, pressed jump in place and the feet came back down (`gained = 0.0`). No step here at all, so the
        // honest answer is "none": the caller reports `no_step_candidate` and does not press anything.
        assert_eq!(jump_target([10, 64, 10], [10, 70, 10], world_with(&[])), None);
        // With a step available the hop goes sideways, so the movement vector is never `(0, 0)`.
        let step = jump_target([10, 64, 10], [10, 70, 10], world_with(&[[11, 64, 10]])).expect("a step exists");
        assert_ne!((step[0], step[2]), (10, 10), "the hop must be horizontal, got {step:?}");
        assert_eq!(step[1], 65, "one block above the feet");
    }

    #[test]
    fn the_step_towards_the_goal_is_the_one_that_is_picked() {
        // Steps to the north (-z) and to the east (+x); the goal is north, so north wins despite the tie-break
        // order preferring +x.
        let steps: &'static [[i32; 3]] = &[[11, 64, 10], [10, 64, 9]];
        assert_eq!(jump_target([10, 64, 10], [10, 64, 3], world_with(steps)), Some([10, 65, 9]));
        // Same world, goal to the east.
        assert_eq!(jump_target([10, 64, 10], [20, 64, 10], world_with(steps)), Some([11, 65, 10]));
    }

    #[test]
    fn a_step_without_room_above_it_is_not_a_candidate() {
        // The step block is there but the cell above is solid: the body could not stand there.
        let steps: &'static [[i32; 3]] = &[[11, 64, 10], [11, 65, 10]];
        assert_eq!(jump_target([10, 64, 10], [20, 64, 10], world_with(steps)), None);
        // …and a hole with air under it is not a step either: the body would fall, not step up.
        let floating: &'static [[i32; 3]] = &[];
        assert_eq!(jump_target([10, 70, 10], [20, 70, 10], world_with(floating)), None, "nothing to stand on");
    }

    #[test]
    fn nowhere_to_hop_is_said_without_pressing_anything() {
        let attempt = JumpAttempt::no_step([10, 64, 10]);
        assert_eq!(attempt.kind, "no_step_candidate");
        assert!(!attempt.sent, "no key was pressed");
        assert!(!attempt.observed(), "nothing to report in `tried`");
        let mut tried = Vec::new();
        note_jump(&mut tried, &attempt);
        assert!(tried.is_empty(), "a step that was never taken must not appear in tried: {tried:?}");
        let said = attempt.describe();
        assert!(said.contains("nowhere to hop"), "{said}");
        assert_eq!(attempt.to_json()["kind"], "no_step_candidate");
    }

    // ---- t89: the jump record must outlive the progress reset ----

    /// The record of a hop that worked: 64.00 -> 65.252, the t88 shape (`rise_ground=+1.252`).
    fn a_hop_that_worked() -> JumpRecord {
        JumpRecord::new(Some([12, 65, 4]), JumpAttempt::from_jump(&sample_jump(true, 64.0, 65.252, None)))
    }

    #[test]
    fn a_jump_that_worked_outlives_the_progress_reset() {
        // A successful hop covers √2 blocks, i.e. more than WALK_PROGRESS, so it used to clear its own record on
        // the next check. `arrived: true` must still say a jump went out and rose a block (t89).
        let arrived = attach_jump(json!({"arrived": true}), Some(&a_hop_that_worked()));
        let jump = &arrived["jump_last"];
        assert_eq!(jump["kind"], json!("jump_ok"), "a hop that ended a block higher must read as jump_ok: {arrived}");
        assert_eq!(jump["target"], json!([12, 65, 4]));
        assert_eq!(jump["y_ground"], json!(64.0));
        assert_eq!(jump["gained"], json!(1.0), "gained is one block, and `y_ground` still carries the float feet: {arrived}");
        assert_eq!(jump["from_ground_block"], json!(64));
        assert_eq!(jump["to_ground_block"], json!(65));
    }

    #[test]
    fn a_result_without_a_jump_grows_no_jump_last_key() {
        // No walk, no record: the field must not appear empty-shaped, which would read as "it jumped and said nothing".
        let plain = attach_jump(json!({"arrived": true}), None);
        assert!(plain.get("jump_last").is_none(), "no jump happened, so no jump_last: {plain}");
    }

    #[test]
    fn the_timeout_names_what_it_tried_and_what_the_jump_did() {
        // t88: 30 s of walking (six windows with `jump=1`) ended as `refused` / "not possible: timed out" with
        // `details: null`. The deadline path must now carry the steps, the last cell and the jump observation.
        let failure = walk_timeout_failure(30, vec!["jump"], Some([10, 64, 4]), Some([10, 64, 9]), Some(&a_hop_that_worked()), false);
        assert_eq!(failure.reason, "timeout");
        let details = failure.details.expect("the deadline branch must carry details");
        assert_eq!(details["tried"], json!(["jump"]));
        assert_eq!(details["stuck_at"], json!([10, 64, 4]));
        assert_eq!(details["jump_last"]["kind"], json!("jump_ok"));
        assert!(failure.message.contains("30 s budget"), "the message must name the budget: {}", failure.message);
        assert!(failure.message.contains("jump went out"), "the message must say the jump went out: {}", failure.message);
        assert!(
            failure.hint.unwrap_or_default().contains("jump_ok"),
            "the hint must tell the model the jump was fine and the budget was not"
        );
    }

    #[test]
    fn a_timeout_without_a_jump_still_carries_the_steps_and_the_cell() {
        let failure = walk_timeout_failure(45, vec!["dig"], Some([7, 63, 7]), Some([9, 66, 7]), None, false);
        let details = failure.details.expect("details");
        assert_eq!(details["tried"], json!(["dig"]));
        assert_eq!(details["jump_last"], Value::Null);
        assert!(
            failure.hint.clone().unwrap_or_default().contains("时间预算"),
            "with no jump the hint must be about the budget: {:?}",
            failure.hint
        );
    }

    // ---- t105: the ceiling above the step ----

    /// A hop the primitive refused before pressing anything: the head cell above `target` is solid.
    fn no_headroom_jump(target: [i32; 3]) -> acacia_bot::movement::Jump {
        acacia_bot::movement::Jump {
            target: Some(target),
            y_ground: Some(64.0),
            sent: false,
            reason: Some("no_headroom"),
            y_before: 64.0,
            y_max: 64.0,
            y_after: 64.0,
            rose: false,
            pressed_ticks: 0,
            airborne_ticks: None,
        }
    }

    /// A hop the primitive refused because the same hop from the same cell already gained nothing.
    fn dead_hop_jump(target: [i32; 3]) -> acacia_bot::movement::Jump {
        acacia_bot::movement::Jump { reason: Some("hop_no_progress"), ..no_headroom_jump(target) }
    }

    const STEP: [i32; 3] = [150012, 65, 149828];
    const HEAD: [i32; 3] = [150012, 66, 149828];

    #[tokio::test]
    async fn t105_a_hop_with_no_headroom_digs_the_ceiling_then_asks_again() {
        // The reported defect: the body stood in a one-block hole, only the head cell was solid, and it jumped in
        // place for 15 s. With digging allowed the step digs *that cell* and asks once more.
        let mut body = FakeLeaper::scripted(vec![
            Ok(no_headroom_jump(STEP)),
            Ok(sample_jump(true, 64.0, 65.0, None)),
        ]);
        let attempt = jump_step(&mut body, STEP, true).await;
        assert_eq!(body.digs, vec![HEAD], "the blocking head cell must be the one dug");
        assert_eq!(body.calls.len(), 2, "one hop, the dig, one hop");
        assert_eq!(attempt.kind, "jump_ok", "after the dig the hop must be tried for real: {attempt:?}");
    }

    #[tokio::test]
    async fn t105_the_walks_digging_permission_reaches_the_hop() {
        // The whole point of `hop_toward_with`: a `move_to {allow_dig:true}` in a one-block hole must be able to widen
        // it. Before t105 the agent called `hop_toward` (= `allow_dig:false`), so the only answer it could ever get was
        // `no_headroom` — and the body jumped in place for the rest of the budget.
        let mut body = FakeLeaper::scripted(vec![Ok(no_headroom_jump(STEP))]);
        let _ = jump_step(&mut body, STEP, true).await;
        // Two calls: the refused one and the retry after the dig — both must carry the walk's permission, not `false`.
        assert!(!body.allow_digs.is_empty(), "the hop must have been called at all");
        assert!(body.allow_digs.iter().all(|allow| *allow), "every hop must see allow_dig=true: {:?}", body.allow_digs);
    }

    #[tokio::test]
    async fn t105_without_digging_permission_the_ceiling_is_reported_and_nothing_is_dug() {
        // `allow_dig:false` is a promise the walk keeps: report the cell, do not touch it, do not retry.
        let mut body = FakeLeaper::scripted(vec![Ok(no_headroom_jump(STEP))]);
        let attempt = jump_step(&mut body, STEP, false).await;
        assert!(body.digs.is_empty(), "a walk without digging permission must not dig");
        assert_eq!(body.calls.len(), 1, "and must not hop twice either");
        assert_eq!(attempt.kind, "no_headroom");
        assert_eq!(attempt.blocked, Some(HEAD), "the cell to dig must be named");
        assert!(!attempt.sent && attempt.pressed_ticks == 0, "nothing was pressed: {attempt:?}");
    }

    #[tokio::test]
    async fn t105_a_second_no_headroom_gives_up_instead_of_jumping_in_place() {
        // The dig went through and the cell is still solid (unbreakable, unloaded, refilled): stop hopping — the walk
        // moves on to dig/detour in its own order rather than pressing at a ceiling for the rest of the budget.
        let mut body = FakeLeaper::scripted(vec![Ok(no_headroom_jump(STEP)), Ok(no_headroom_jump(STEP))]);
        let attempt = jump_step(&mut body, STEP, true).await;
        assert_eq!(body.digs.len(), 1, "exactly one attempt at the ceiling, not a loop");
        assert_eq!(body.calls.len(), 2, "one retry, then no more");
        assert_eq!(attempt.kind, "no_headroom");
        assert!(jump_climbed(&attempt) == false, "a refused hop is not progress");
    }

    #[tokio::test]
    async fn t105_a_failed_dig_does_not_press_a_second_time() {
        let mut body = FakeLeaper::scripted(vec![Ok(no_headroom_jump(STEP))]);
        body.digs_ok = false;
        let attempt = jump_step(&mut body, STEP, true).await;
        assert_eq!(body.digs, vec![HEAD], "the dig was attempted");
        assert_eq!(body.calls.len(), 1, "and the hop was not repeated after a dig that failed");
        assert_eq!(attempt.kind, "no_headroom");
    }

    #[test]
    fn t105_the_refused_hops_never_enter_tried_and_never_gain() {
        // `tried` means "inputs this round really sent" (t82): a hop that was never pressed says nothing about the
        // body. Both t105 verdicts are `sent: false`, so neither may reach `tried` — and `hop_no_progress` is a
        // details kind, not a new `fail_reason`.
        for jump in [no_headroom_jump(STEP), dead_hop_jump(STEP)] {
            let attempt = JumpAttempt::from_jump(&jump);
            assert!(!attempt.observed(), "{} must not count as an attempt: {attempt:?}", attempt.kind);
            let mut tried = Vec::new();
            note_jump(&mut tried, &attempt);
            assert!(tried.is_empty(), "{} leaked into tried: {tried:?}", attempt.kind);
            assert!(!jump_climbed(&attempt), "{} is not progress", attempt.kind);
        }
    }

    #[test]
    fn t105_the_dead_hop_verdict_comes_from_the_primitive() {
        let refused = JumpAttempt::from_jump(&dead_hop_jump(STEP));
        assert_eq!(refused.kind, "hop_no_progress", "the primitive's reason is the verdict: {refused:?}");
        assert_eq!(refused.pressed_ticks, 0);
        assert!(refused.blocked.is_none(), "a refused hop has no ceiling to blame");
    }

    #[test]
    fn t105_the_hint_names_the_cell_and_tells_the_model_to_dig_it_first() {
        // Both failure shapes (`blocked_by_terrain`, `timeout`) must carry the same sentence, and it must name the
        // exact cell — "somewhere above you" is what left the model jumping in place.
        let attempt = JumpAttempt::from_jump(&no_headroom_jump(STEP));
        let record = JumpRecord::new(Some(STEP), attempt);
        let hint = blocked_by_terrain_hint(false, Some(&record));
        assert!(hint.contains("[150012, 66, 149828]"), "the hint must name the head cell: {hint}");
        assert!(hint.contains("dig"), "and say to dig it: {hint}");
        assert!(hint.contains("别在原地连跳"), "and warn against hopping in place: {hint}");
        assert_eq!(no_headroom_hint(HEAD), hint, "one sentence for one situation, in both failure shapes");

        // A plain blocked walk keeps its old advice; the ceiling wording must not leak into it.
        assert!(!blocked_by_terrain_hint(false, None).contains("头顶"));
    }

    #[test]
    fn t105_the_jump_details_carry_the_cell_and_the_pressed_ticks() {
        let record = JumpRecord::new(Some(STEP), JumpAttempt::from_jump(&no_headroom_jump(STEP)));
        let json = record.to_json();
        assert_eq!(json["kind"], json!("no_headroom"));
        assert_eq!(json["blocked"], json!(HEAD), "details.jump_last.blocked must name the cell");
        assert_eq!(json["pressed_ticks"], json!(0), "and how many ticks the key was held");
        assert_eq!(json["sent"], json!(false));
        // A real hop carries its own count, so 0 can be read as "nothing was pressed" rather than "not recorded".
        let real = JumpRecord::new(Some(STEP), JumpAttempt::from_jump(&sample_jump(true, 64.0, 65.0, None)));
        assert_eq!(real.to_json()["pressed_ticks"], json!(6));
    }
}

#[cfg(test)]
mod plan_tests {
    //! t120: a refused search hands its numbers over as `details.plan`, never as prose.
    //!
    //! t117 put `[plan {..}]` at the end of the message so a human could grep it. A tool layer that cuts that text
    //! back out is the same mistake as reading the wire id out of `no_local_change`: the numbers are one field away,
    //! and the two ways of spelling them drift apart the first time somebody rewords a sentence. These tests pin the
    //! structured path and, just as important, the case where there is nothing to say.
    use super::*;

    fn refused_search() -> PlanStats {
        PlanStats {
            nodes_expanded: 42,
            max_nodes: 200_000,
            budget_exhausted: false,
            target_region_reached: false,
            closest_short: Some(6.0),
            ..PlanStats::default()
        }
    }

    fn exhausted_search() -> PlanStats {
        PlanStats {
            nodes_expanded: 199_998,
            max_nodes: 200_000,
            budget_exhausted: true,
            target_region_reached: false,
            closest_short: None,
            ..PlanStats::default()
        }
    }

    #[test]
    fn a_refused_search_travels_as_numbers_in_details_plan() {
        let failure = with_plan(Failure::new("no_path", "no path to the goal"), Some(refused_search()));
        let details = failure.details.expect("details must exist");
        let plan = &details["plan"];
        assert_eq!(plan["nodes_expanded"], json!(42), "the count the search actually ran: {plan}");
        assert_eq!(plan["max_nodes"], json!(200_000), "the budget it ran under: {plan}");
        assert_eq!(plan["budget_exhausted"], json!(false), "it ran out of places, not budget: {plan}");
        assert_eq!(plan["target_region_reached"], json!(false), "it never came near the goal: {plan}");
        assert_eq!(plan["closest_short"], json!(6.0), "how far short the closest node was: {plan}");
    }

    #[test]
    fn the_two_kinds_of_refusal_are_told_apart_by_the_payload() {
        // The whole point of the numbers: "walled off" and "cut short" ask for opposite next moves.
        let walled = with_plan(Failure::new("no_path", "no path"), Some(refused_search())).details.unwrap()["plan"].clone();
        let cut = with_plan(Failure::new("no_path", "no path"), Some(exhausted_search())).details.unwrap()["plan"].clone();
        assert_eq!(walled["budget_exhausted"], json!(false));
        assert_eq!(cut["budget_exhausted"], json!(true), "a search stopped at max_nodes must say so: {cut}");
        assert_eq!(cut["closest_short"], Value::Null, "a goal with no neighbourhood has nothing to be short of: {cut}");
    }

    #[test]
    fn a_walk_the_planner_never_refused_carries_no_plan_at_all() {
        // `None` is not "zero nodes": it is "this failure did not come from a refused search" (a timeout, a ceiling
        // refusal, a body that did not follow). Writing the previous walk's numbers here would make the caller act
        // on a budget that never ran out.
        let failure = with_plan(Failure::new("timeout", "the walk ran out of its 30 s budget"), None);
        let details = failure.details.unwrap_or(Value::Null);
        assert!(details.get("plan").is_none() || details["plan"].is_null(), "no plan may be invented: {details}");
    }

    #[test]
    fn the_plan_sits_beside_what_the_walk_already_said() {
        // `tried` / `stuck_at` / `jump_last` must survive the merge: one failure, one object, every arm present.
        let walk = walk_details(vec!["jump"], Some([10, 64, 10]), Some([12, 65, 10]), false, None);
        let failure = with_plan(Failure::new("blocked_by_terrain", "stood still").with_details(walk), Some(refused_search()));
        let details = failure.details.expect("details");
        assert_eq!(details["tried"], json!(["jump"]), "the steps survive: {details}");
        assert_eq!(details["stuck_at"], json!([10, 64, 10]));
        assert_eq!(details["plan"]["nodes_expanded"], json!(42), "and the numbers join them: {details}");
    }

    #[test]
    fn a_failure_without_details_still_gets_one() {
        // `no path` used to answer `details: null`; adding the plan must not resurrect that shape.
        let failure = with_plan(Failure::new("refused", "the shell task dropped the reply"), Some(exhausted_search()));
        assert!(failure.details.as_ref().is_some_and(|d| d["plan"].is_object()));
    }

    #[test]
    fn the_message_is_left_alone() {
        // The prose keeps the `[plan {..}]` tail a human greps; the tool must not rewrite it into JSON.
        let text = "no path to [12, 65, 10] [plan {\"nodes_expanded\":42}]";
        let failure = with_plan(Failure::new("no_path", text), Some(refused_search()));
        assert_eq!(failure.message, text, "the caller's message is not ours to reformat");
    }
}

#[cfg(test)]
mod place_tests {
    //! t99/F3: the place that echoed nothing is not automatically a refusal.
    //!
    //! t98 placed at a cell that stayed air, floor solid, `desync=0.0`, zero confirmations, and the result said
    //! "the server refused it (anticheat, claim or game rule); stop trying" — three explanations the run itself had
    //! already ruled out. The hint is now a sequence, and the sequence is what this test pins.
    use super::*;

    #[test]
    fn an_echo_less_place_asks_for_a_read_back_before_a_verdict() {
        let hint = place_failure_hint("no_effect: the server sent no update for the cell")
            .expect("a cell that never echoed must get the ordered hint");
        let (first, second, third) = (
            hint.find('①').expect("step one"),
            hint.find('②').expect("step two"),
            hint.find('③').expect("step three"),
        );
        assert!(first < second && second < third, "the steps must be ordered: {hint}");
        assert!(hint[first..second].contains("block"), "step one is a read-back of the cell: {hint}");
        assert!(hint[second..third].contains("相邻格"), "step two is a different cell: {hint}");
        assert!(!hint.contains("stop trying"), "nothing here justifies giving up yet: {hint}");
    }

    #[test]
    fn an_echo_less_confirmation_from_the_bot_layer_reads_the_same() {
        // `interact/confirm.rs` words the same outcome as `no local change`; both spellings take the ordered hint.
        assert!(place_failure_hint("no local change: the block at [1, 64, 2] is still air").is_some());
    }

    #[test]
    fn a_refusal_that_says_why_keeps_the_generic_hint() {
        // A refusal the server explained is not a jitter: the table's `rejected` hint stays.
        assert!(place_failure_hint("placement is not allowed in this claim").is_none());
        assert!(place_failure_hint("tile.bed.occupied").is_none());
    }

}

#[cfg(test)]
mod sleep_tests {
    //! t89/F4: `sleep` must say what happened when the body did not end up asleep, and a "too far" refusal must be
    //! read as the server's stale position rather than as a walk the model still owes (t88: refused twice with the
    //! bed right there at desync 4.19, asleep 2.4 s after the drift hit 0).
    use super::*;

    #[test]
    fn sleep_always_answers_woke_reason() {
        let asleep = sleep_result(true, Some([4, 64, 9]), [4, 64, 10]);
        assert_eq!(asleep["slept"], json!(true));
        assert_eq!(asleep["bed_pos"], json!([4, 64, 9]));
        assert!(asleep["woke_reason"].is_null(), "asleep: nothing woke it: {asleep}");

        let awake = sleep_result(false, Some([4, 64, 9]), [4, 64, 10]);
        assert_eq!(awake["slept"], json!(false));
        let reason = awake["woke_reason"].as_str().expect("not asleep: the reason must be there, not null");
        assert!(reason.contains("never confirmed sleep"), "the reason must be honest about what is known: {reason}");
    }

    #[test]
    fn a_too_far_bed_is_read_as_desync() {
        assert!(bed_too_far("tile.bed.toofar"));
        assert!(bed_too_far("bed is too far away"));
        assert!(!bed_too_far("tile.bed.occupied"), "an occupied bed is not a position problem");

        let failure = bed_refusal("tile.bed.toofar".to_owned());
        assert_eq!(failure.reason, "rejected");
        assert!(failure.message.contains("desync 4.19"), "the message must cite the t88 measurement: {}", failure.message);
        let hint = failure.hint.unwrap_or_default();
        assert!(hint.contains("位置不同步"), "the hint must point at desync: {hint}");
        assert!(hint.contains("status"), "the hint must say to look at status first: {hint}");
    }

    #[test]
    fn a_plain_refusal_keeps_the_generic_hint() {
        let failure = bed_refusal("tile.bed.occupied".to_owned());
        assert_eq!(failure.reason, "rejected");
        let hint = failure.hint.unwrap_or_default();
        assert!(!hint.contains("位置不同步"), "only the too-far refusal may claim desync: {hint}");
    }
}

#[cfg(test)]
mod sync_tests {
    //! t94: `place`/`dig` must be self-evidencing.
    //!
    //! t91's verdict ("the same shape refused at t88 and succeeded at t91") could only be reached by hand-inserting
    //! a `status` before and after the action; with any other call in between, the desync and the confirmation count
    //! were gone. Both outcomes now carry the snapshot, built by the one function `status` also reads.
    use super::*;

    fn the_t91_snapshot() -> Value {
        // desync 4.16 blocks, the server never having confirmed a position: exactly what t91 had to reconstruct.
        sync_snapshot(Some(4.161), 0, None, false)
    }

    #[test]
    fn the_snapshot_carries_what_status_shows() {
        let snapshot = the_t91_snapshot();
        // `round_hundredth` keeps `f32` (as `status` does), so the literal is the f32 one: the two paths must encode
        // the number identically, not merely be equal as reals.
        assert_eq!(snapshot["position_desync_blocks"], json!(4.16_f32), "the same blocks `status.position_desync` rounds to");
        assert_eq!(
            snapshot["server_reference"],
            json!({"confirmations": 0, "confirmed_ticks": null, "fresh": false}),
            "status's position_confirmations / server_confirmed_ticks / server_reference_fresh, folded"
        );

        let confirmed = sync_snapshot(None, 3, Some(2), true);
        assert!(confirmed["position_desync_blocks"].is_null(), "no drift measured: null, not zero");
        assert_eq!(confirmed["server_reference"], json!({"confirmations": 3, "confirmed_ticks": 2, "fresh": true}));
    }

    #[test]
    fn a_success_body_gains_the_snapshot_without_losing_what_it_said() {
        let body = attach_sync(json!({"placed": true, "against": [1, 2, 3]}), &the_t91_snapshot());
        assert_eq!(body["placed"], json!(true), "the tool's own fields stay");
        assert_eq!(body["against"], json!([1, 2, 3]));
        assert_eq!(body["position_desync_blocks"], json!(4.16_f32));
        assert_eq!(body["server_reference"]["confirmations"], json!(0));
    }

    #[test]
    fn a_failure_keeps_its_own_details_and_gains_the_snapshot() {
        let failure = Failure::new("no_item", "nothing held: hold a block first").with_details(json!({"held": null}));
        let failure = attach_sync_failure(failure, &the_t91_snapshot());
        let details = failure.details.expect("details must survive");
        assert_eq!(details["held"], Value::Null, "the tool's own detail stays");
        assert_eq!(details["position_desync_blocks"], json!(4.16_f32));
        assert_eq!(details["server_reference"], json!({"confirmations": 0, "confirmed_ticks": null, "fresh": false}));
        assert_eq!(failure.reason, "no_item", "the reason is untouched");
    }

    #[test]
    fn a_failure_with_no_details_still_gets_them() {
        let failure = attach_sync_failure(Failure::new("rejected", "the server said no"), &the_t91_snapshot());
        let details = failure.details.expect("details must be created");
        assert_eq!(details["position_desync_blocks"], json!(4.16_f32));
    }

    #[test]
    fn synced_covers_both_outcomes() {
        // The dispatch wrapper is what `place`/`dig` really go through: success and failure must both be stamped.
        let ok = synced(Ok(json!({"broke": true, "pos": [1, 64, 2]})), &the_t91_snapshot()).expect("ok");
        assert_eq!(ok["broke"], json!(true));
        assert_eq!(ok["position_desync_blocks"], json!(4.16_f32));

        let err = synced(Err(Failure::new("out_of_reach", "too far")), &the_t91_snapshot()).expect_err("err");
        assert_eq!(err.reason, "out_of_reach");
        assert_eq!(err.details.expect("details")["server_reference"]["fresh"], json!(false));
    }
}

#[cfg(test)]
mod plan_trace_tests {
    use std::cell::{Cell, RefCell};

    use acacia_bot::pathfind::PlanStats;

    use super::{PlanSource, trace_plan};

    fn stats_of(nodes: usize, cost: f32) -> PlanStats {
        PlanStats { nodes_expanded: nodes, max_nodes: 20_000, cost, ..PlanStats::default() }
    }

    /// The bot's plan slot, as the trace sees it: `take` empties it, exactly like `Bot::take_plan_stats` (t150).
    struct FakePlans {
        slot: RefCell<Option<PlanStats>>,
        takes: Cell<usize>,
    }

    impl FakePlans {
        fn holding(stats: PlanStats) -> Self {
            Self { slot: RefCell::new(Some(stats)), takes: Cell::new(0) }
        }

        fn empty() -> Self {
            Self { slot: RefCell::new(None), takes: Cell::new(0) }
        }

        fn put(&self, stats: PlanStats) {
            *self.slot.borrow_mut() = Some(stats);
        }

        fn takes(&self) -> usize {
            self.takes.get()
        }
    }

    impl PlanSource for FakePlans {
        fn take_plan(&self) -> Option<PlanStats> {
            self.takes.set(self.takes.get() + 1);
            self.slot.borrow_mut().take()
        }
    }

    /// Runs one operation's trace and returns what it printed.
    fn collect(source: &FakePlans, on: bool) -> Vec<String> {
        let mut lines = Vec::new();
        trace_plan(source, on, &mut |line| lines.push(line.to_owned()));
        lines
    }

    /// The default must stay silent *and* still drain: a stale report must not survive into the next
    /// operation just because this run never asked for the line (t149).
    #[test]
    fn the_switch_off_prints_nothing_but_still_takes_the_report() {
        let plans = FakePlans::holding(stats_of(320, 12.5));
        assert!(collect(&plans, false).is_empty(), "a silent default is the whole contract");
        assert_eq!(plans.takes(), 1, "the report must still be drained");
        assert!(plans.slot.borrow().is_none(), "drained means empty");
    }

    /// Exactly one line, and the fields in the order the log reader expects.
    #[test]
    fn the_switch_on_prints_exactly_one_line_in_a_fixed_order() {
        let plans = FakePlans::holding(stats_of(320, 12.5));
        let lines = collect(&plans, true);
        assert_eq!(lines, vec!["[plan] nodes=320 cost=12.50".to_owned()]);
    }

    /// The line's shape, from the other side: three fields, one space each, no trailing space (t155).
    ///
    /// A reader diffs these lines between two walks, so a doubled space or a trailing one turns a `diff` into
    /// noise that looks like a behaviour change.
    #[test]
    fn the_line_is_three_fields_with_single_spaces_and_nothing_after_the_cost() {
        let plans = FakePlans::holding(stats_of(320, 12.5));
        let line = collect(&plans, true).remove(0);
        assert_eq!(line, "[plan] nodes=320 cost=12.50", "the exact line, byte for byte");
        assert_eq!(line.trim_end(), line, "no trailing space: {line:?}");
        assert_eq!(line.trim_start(), line, "no leading space: {line:?}");
        assert!(!line.contains("  "), "one space between fields, not two: {line:?}");
        assert_eq!(line.split(' ').count(), 3, "tag + two fields, nothing more: {line:?}");
    }

    /// The three D* Lite words are gone from the line, not merely unused (t155).
    ///
    /// Someone reading a real-server log greps for `engine` or `fallback`; after t154 there is no second
    /// planner and no fallback, so a line that still carried the words would be a claim the code cannot back.
    #[test]
    fn the_line_no_longer_names_an_engine_a_fallback_or_a_reuse() {
        for (nodes, cost) in [(320usize, 12.5f32), (5, 14.13), (49_960, 0.0)] {
            let plans = FakePlans::holding(stats_of(nodes, cost));
            let line = collect(&plans, true).remove(0);
            for gone in ["engine", "fallback", "reused", "dstar", "astar"] {
                assert!(!line.contains(gone), "{line} must not mention {gone:?} any more (t155)");
            }
        }
    }

    /// `nodes` before `cost`: the order is part of the format, not an accident of the format string (t155).
    #[test]
    fn nodes_comes_before_cost() {
        let plans = FakePlans::holding(stats_of(320, 12.5));
        let line = collect(&plans, true).remove(0);
        let nodes = line.find("nodes=").expect("the line names the node count");
        let cost = line.find("cost=").expect("the line names the cost");
        assert!(nodes < cost, "nodes must come before cost: {line}");
        assert_eq!(&line[..nodes], "[plan] ", "the tag opens the line: {line}");
    }

    /// Two decimals, always — including the values that round (a reader must not see `12.5` one walk and
    /// `12.50` the next, or `12` when the price is a fraction) (t155).
    #[test]
    fn cost_always_prints_two_decimals() {
        for (cost, want) in [(12.4999f32, "12.50"), (12.5, "12.50"), (0.0, "0.00"), (123.456, "123.46"), (14.13, "14.13")] {
            let plans = FakePlans::holding(stats_of(320, cost));
            let line = collect(&plans, true).remove(0);
            assert_eq!(line, format!("[plan] nodes=320 cost={want}"), "cost {cost} must print as {want}");
        }
    }

    /// The shell's own default is silence; `main` copies `config.agent.plan_trace` into this flag (t149/t155).
    #[test]
    fn the_shell_defaults_to_silence() {
        assert!(!super::ShellConfig::default().plan_trace, "an unasked-for trace must stay quiet");
    }

    /// A walk that never searched has nothing to say — the line is about a search, not about a tool.
    #[test]
    fn a_walk_without_a_search_prints_nothing_even_when_on() {
        let plans = FakePlans::empty();
        assert!(collect(&plans, true).is_empty());
        assert_eq!(plans.takes(), 1, "the slot is checked once per operation");
    }

    /// The line is numbers, not a verdict — the reader must not think the walk went wrong (t149).
    #[test]
    fn the_line_carries_numbers_and_not_a_judgement() {
        let plans = FakePlans::holding(stats_of(41, 3.25));
        let line = collect(&plans, true).remove(0);
        assert_eq!(line, "[plan] nodes=41 cost=3.25", "the line carries the numbers, not a judgement");
        for word in ["error", "fail", "warn", "refus", "retry"] {
            assert!(!line.to_ascii_lowercase().contains(word), "{line} must not read as a complaint ({word})");
        }
    }

    /// Two walks in a row are two reports: the first take empties the slot, and the second search's cost is
    /// its own — the P2 case, where reading `last` would hand the second walk the first walk's price.
    #[test]
    fn each_search_is_taken_once_and_reports_its_own_cost() {
        let plans = FakePlans::holding(stats_of(120, 12.5));
        let first = collect(&plans, true).remove(0);
        assert!(first.contains("cost=12.50"), "{first}");

        // Nothing new searched yet: no second line, and no repeat of the first one.
        assert!(collect(&plans, true).is_empty(), "a drained slot cannot be read twice");

        plans.put(stats_of(260, 5.0));
        let second = collect(&plans, true).remove(0);
        assert!(second.contains("cost=5.00"), "{second}");
        assert_ne!(first, second, "two searches must not report the same numbers");
        assert_eq!(plans.takes(), 3);
    }
}

#[cfg(test)]
mod heal_tests {
    //! t97: the heal that undid itself.
    //!
    //! t95 climbed one block (feet 70 -> 71, `on_ground` steady for about four seconds) and then the very next step
    //! — a `detour` that only knows the goal's direction — walked the body back down to `y=70.5625`, ending the walk
    //! where it started. Two rules come out of it, both pinned here without a server: a heal that lifted the body
    //! ends the round instead of handing over to the detour, and the detour itself runs once.
    use super::*;

    fn hop(sent: bool, y_before: f32, y_after: f32) -> JumpAttempt {
        JumpAttempt::from_jump(&acacia_bot::movement::Jump {
            target: None,
            y_ground: None,
            sent,
            reason: None,
            y_before,
            y_max: y_before.max(y_after),
            y_after,
            rose: y_after >= y_before + 0.9,
            pressed_ticks: if sent { 6 } else { 0 },
            airborne_ticks: if sent { Some(4) } else { None },
        })
    }

    #[test]
    fn a_hop_that_ends_a_block_higher_solved_the_round() {
        // The t95 shape: the server's standing face is 70.5625, so climbing one block is +1.0 by the feet.
        assert!(jump_climbed(&hop(true, 70.5625, 71.5625)), "70.5625 -> 71.5625 is a climbed step");
        assert!(jump_climbed(&hop(true, 64.0, 65.0)));
        // A little over a block still counts (landing above the step's top face).
        assert!(jump_climbed(&hop(true, 64.0, 65.252)));
        // Sent, but the feet ended where they started: the press only bounced on the spot.
        assert!(!jump_climbed(&hop(true, 64.0, 64.0)));
        // Never sent: nothing was solved however close the target was.
        assert!(!jump_climbed(&hop(false, 64.0, 64.0)));
    }

    #[test]
    fn a_heal_that_worked_is_not_followed_by_a_detour() {
        // The t95 round end to end, through the same bookkeeping the walk uses: the hop runs and lifts the body.
        let mut heals = Heals::default();
        heals.started("jump");
        assert!(jump_climbed(&hop(true, 70.5625, 71.5625)), "the hop that climbed in t95");
        heals.replanned();
        // Back to the pathfinder from the new height: the next step is the jump again, never the detour that carried
        // the body back down.
        assert_eq!(heals.next(false), Some("jump"));
        assert_ne!(heals.next(false), Some("detour"));
        assert!(heals.healed, "`healed_once` is set for the rest of the walk");
        assert_eq!(heals.attempts(), 0, "the round is cleared, so `tried` cannot name the step that worked");
    }

    #[test]
    fn a_heal_that_did_nothing_hands_over_to_the_detour() {
        let mut heals = Heals::default();
        heals.started("jump");
        // Nothing lifted, so nothing is cleared: the walk moves on to the next step — the detour, last in the list.
        assert!(!jump_climbed(&hop(true, 64.0, 64.0)), "a press that bounced on the spot solved nothing");
        assert_eq!(heals.next(false), Some("detour"));
        assert!(!heals.healed, "a failed heal is not a heal");
        heals.started("detour");
        assert_eq!(heals.next(false), None, "and that is the one detour");
    }

    #[test]
    fn a_heal_that_did_nothing_moves_on_to_the_detour() {
        // Without `allow_dig` the jump is followed by the detour directly; with it, digging comes first.
        assert_eq!(next_step(&heal_steps(false), &["jump"]), Some("detour"));
        assert_eq!(next_step(&heal_steps(false), &[]), Some("jump"));
        assert_eq!(next_step(&heal_steps(true), &["jump", "dig"]), Some("detour"));
        // The step index follows what was *attempted*, so a jump that could not even be sent still advances (t82).
        assert_eq!(next_step(&heal_steps(false), &["jump"]), Some("detour"));
    }

    #[test]
    fn the_walk_spends_one_detour_and_then_gives_up() {
        // A second detour in the same round used to be reachable (the index saturated on "detour") and the walk kept
        // circling until the deadline. Now it stops and reports `no progress`, which the model can act on.
        assert_eq!(next_step(&heal_steps(false), &["jump", "detour"]), None);
        assert_eq!(next_step(&heal_steps(true), &["jump", "dig", "detour"]), None);
    }

    #[test]
    fn the_timeout_says_a_heal_already_worked() {
        let failure = walk_timeout_failure(30, vec!["detour"], Some([10, 64, 4]), Some([10, 66, 4]), None, true);
        assert_eq!(failure.details.expect("details")["healed_once"], json!(true));
        let plain = walk_timeout_failure(30, vec!["jump"], Some([10, 64, 4]), Some([10, 66, 4]), None, false);
        assert_eq!(plain.details.expect("details")["healed_once"], json!(false));
    }

    /// The dig step changes the map, not the body: the round is over (the next step would be judged against a world
    /// that no longer exists) but nothing claims the body was lifted (t144).
    #[test]
    fn digging_hands_the_walk_back_without_claiming_the_body_was_lifted() {
        let mut heals = Heals::default();
        heals.started("jump");
        heals.started("dig");
        assert_eq!(heals.attempts(), 2);
        heals.world_changed();
        assert_eq!(heals.attempts(), 0, "the round starts over on the new geometry");
        assert!(!heals.healed, "digging a hole is not a heal that lifted the body");
        assert_eq!(heals.next(true), Some("jump"), "a later stall may ask for a hop again, against the new shape");
    }

    /// The two successes stay apart: one lifted the body, the other only opened a hole.
    #[test]
    fn a_hop_that_lifted_the_body_and_a_dug_hole_are_two_different_successes() {
        let mut lifted = Heals::default();
        lifted.started("jump");
        lifted.replanned();
        let mut dug = Heals::default();
        dug.started("dig");
        dug.world_changed();
        assert_eq!(lifted.attempts(), dug.attempts(), "both end the round");
        assert_ne!(lifted.healed, dug.healed, "only one of them lifted the body");
    }
}

#[cfg(test)]
mod trade_tests {
    //! t83: the model picked offer #0 (32 rotten_flesh) while 64 emeralds sat in the bag and offer #1
    //! was one emerald away. The tool returned ten prices and no affordability answer, so these tests
    //! pin the answer: `payable`, `missing`, `payable_offers`, and a `missing_offer_cost` message that
    //! names the offer and the alternatives.
    use super::*;
    use acacia_bot::state::{TradeItem, TradeOffer};

    fn item(name: &str, count: u16) -> TradeItem {
        TradeItem { name: name.to_owned(), count, metadata: 0 }
    }

    fn offer(buy_a: (&str, u16), buy_b: Option<(&str, u16)>, sell: (&str, u16)) -> TradeOffer {
        TradeOffer {
            buy_a: item(buy_a.0, buy_a.1),
            buy_b: buy_b.map(|(name, count)| item(name, count)),
            sell: item(sell.0, sell.1),
            uses: 0,
            max_uses: 12,
            tier: 0,
            recipe_network_id: 0,
        }
    }

    /// The run's table: #0 is the rotten-flesh trade the model chose, #1 the emerald one it could do.
    fn the_ten_offers() -> Vec<TradeOffer> {
        vec![
            offer(("minecraft:rotten_flesh", 32), None, ("minecraft:emerald", 1)),
            offer(("minecraft:emerald", 1), None, ("minecraft:redstone", 1)),
            offer(("minecraft:wheat", 20), None, ("minecraft:emerald", 1)),
            offer(("minecraft:paper", 24), None, ("minecraft:emerald", 1)),
            offer(("minecraft:coal", 15), None, ("minecraft:emerald", 1)),
            offer(("minecraft:emerald", 2), None, ("minecraft:bread", 1)),
            offer(("minecraft:iron_ingot", 4), None, ("minecraft:emerald", 1)),
            offer(("minecraft:stick", 32), None, ("minecraft:emerald", 1)),
            offer(("minecraft:glass", 9), None, ("minecraft:emerald", 1)),
            offer(("minecraft:emerald", 3), None, ("minecraft:redstone", 4)),
        ]
    }

    fn bag(entries: &[(&str, u32)]) -> BTreeMap<String, u32> {
        entries.iter().map(|(name, count)| ((*name).to_owned(), *count)).collect()
    }

    #[test]
    fn only_the_offer_the_bag_covers_is_payable() {
        // 64 emeralds after the chest run; everything else is at zero.
        let have = bag(&[("minecraft:emerald", 64)]);
        let payable = payable_indices(&the_ten_offers(), &have, 1);
        assert_eq!(payable, vec![1, 5, 9], "the three offers priced in emeralds are the payable ones");
        assert!(payable.contains(&1), "#1 (1 emerald -> 1 redstone) is the one the run could have done");
        assert!(!payable.contains(&0), "#0 needs 32 rotten_flesh and the bag has none");
    }

    #[test]
    fn first_payable_picks_the_emerald_offer_not_the_rotten_flesh_one() {
        let have = bag(&[("minecraft:emerald", 64)]);
        let chosen = payable_indices(&the_ten_offers(), &have, 1).first().copied();
        assert_eq!(chosen, Some(1), "the first payable offer, not offer 0");
    }

    #[test]
    fn the_message_names_the_offer_the_shortfall_and_the_alternatives() {
        let have = bag(&[("minecraft:emerald", 64)]);
        let failure = unpayable_failure(Some(0), &the_ten_offers(), &have, 1);
        assert_eq!(failure.reason, "missing_offer_cost");
        assert_eq!(
            failure.message,
            "offer #0 needs 32 rotten_flesh, you have 0; payable offers: #1 (1 emerald -> 1 redstone), \
             #5 (2 emerald -> 1 bread), #9 (3 emerald -> 4 redstone)",
            "the old message named neither the offer nor the offers that could trade"
        );
        let hint = failure.hint.unwrap_or_default();
        assert!(hint.contains("first_payable"), "hint points at the default that avoids the mistake: {hint}");
        let details = failure.details.unwrap_or(Value::Null);
        assert_eq!(details["offer_index"], json!(0));
        assert_eq!(details["missing"], json!([{"item": "minecraft:rotten_flesh", "have": 0, "need": 32}]));
        assert_eq!(details["payable_offers"], json!([1, 5, 9]));
    }

    #[test]
    fn nothing_payable_names_the_closest_offer_and_says_so() {
        // Twelve wheat: nothing trades, but the 20-wheat offer is 8 short while the rotten-flesh one
        // is 32 short, so the wheat offer is the one worth naming.
        let offers = vec![
            offer(("minecraft:rotten_flesh", 32), None, ("minecraft:emerald", 1)),
            offer(("minecraft:wheat", 20), None, ("minecraft:emerald", 1)),
        ];
        let have = bag(&[("minecraft:wheat", 12)]);
        let failure = unpayable_failure(None, &offers, &have, 1);
        assert_eq!(failure.reason, "missing_offer_cost");
        assert_eq!(
            failure.message,
            "closest offer #1 needs 20 wheat, you have 12; no offer is payable with this inventory"
        );
        let details = failure.details.unwrap_or(Value::Null);
        assert_eq!(details["offer_index"], json!(1));
        assert_eq!(details["missing"], json!([{"item": "minecraft:wheat", "have": 12, "need": 20}]));
        assert_eq!(details["payable_offers"], json!([]));
        assert!(failure.hint.unwrap_or_default().contains("affordable"));
    }

    #[test]
    fn buy_b_and_times_are_part_of_the_price() {
        // A two-item price is two shortfalls, and `times` multiplies both sides.
        let offers = vec![offer(("minecraft:emerald", 1), Some(("minecraft:book", 1)), ("minecraft:enchanted_book", 1))];
        let have = bag(&[("minecraft:emerald", 2)]);
        assert!(missing_parts(&offers[0], &have, 1).len() == 1, "one emerald is enough, the book is missing");
        let three = missing_parts(&offers[0], &have, 3);
        assert_eq!(three.len(), 2, "three trades need three emeralds and three books: {three:?}");
        assert_eq!(three.iter().find(|(name, _, _)| name.ends_with("emerald")).map(|(_, have, need)| (*have, *need)), Some((2, 3)));
        assert_eq!(three.iter().find(|(name, _, _)| name.ends_with("book")).map(|(_, have, need)| (*have, *need)), Some((0, 3)));
    }

    #[test]
    fn a_repeated_item_on_both_sides_is_summed() {
        // The same item charged twice must not be paid twice from a single stack.
        let offers = vec![offer(("minecraft:emerald", 2), Some(("minecraft:emerald", 3)), ("minecraft:redstone", 1))];
        let cost = offer_cost(&offers[0], 1);
        assert_eq!(cost, vec![("minecraft:emerald".to_owned(), 5)]);
        assert_eq!(payable_indices(&offers, &bag(&[("minecraft:emerald", 4)]), 1), Vec::<usize>::new());
        assert_eq!(payable_indices(&offers, &bag(&[("minecraft:emerald", 5)]), 1), vec![0]);
    }

    #[test]
    fn the_offer_row_carries_payable_and_missing() {
        let have = bag(&[("minecraft:emerald", 64)]);
        let rotten = trade_offer_json(&the_ten_offers()[0], 0, &have, 1);
        assert_eq!(rotten["payable"], json!(false));
        assert_eq!(rotten["missing"], json!([{"item": "minecraft:rotten_flesh", "have": 0, "need": 32}]));
        let emerald = trade_offer_json(&the_ten_offers()[1], 1, &have, 1);
        assert_eq!(emerald["payable"], json!(true));
        assert_eq!(emerald["missing"], json!([]));
        assert_eq!(emerald["buy_a"], json!({"item": "minecraft:emerald", "count": 1}));
    }

    #[test]
    fn a_step_one_block_up_with_a_solid_ceiling_is_refused_before_the_walk_starts() {
        let feet = [150026, 64, 149828];
        let step = [150026, 65, 149828];
        let head = [150026, 66, 149828];
        // t107: `move_to{at:[..,65,..]}` with a solid cell at y=66 — the body is two blocks tall, so no walk, no jump.
        assert_eq!(headroom_blocked(feet, step, |pos| pos == head), Some(head));
        // Level ground: nothing to climb, nothing to refuse.
        assert_eq!(headroom_blocked(feet, [150027, 64, 149828], |pos| pos == head), None);
        // A step whose head cell is free is exactly what the heal is for.
        assert_eq!(headroom_blocked(feet, step, |_| false), None);
        // A wall two blocks up is not this rule's business: the pathfinder has to route around it.
        assert_eq!(headroom_blocked(feet, [150026, 66, 149828], |_| true), None);
    }

    #[test]
    fn a_refused_walk_says_what_it_tried_and_what_the_last_jump_did() {
        // t108 P1: `{reason:"refused"}` with `details: null` after 36 s (t107) left nothing to read.
        let empty = walk_details(Vec::new(), Some([150026, 64, 149828]), Some([150026, 65, 149828]), false, None);
        assert_eq!(empty["tried"], json!([]));
        assert_eq!(empty["stuck_at"], json!([150026, 64, 149828]));
        assert!(empty["jump_last"].is_null(), "no hop, no claim");

        // The t107 shape: a press went out and came down on the block it started from.
        let landed_back = acacia_bot::movement::Jump {
            target: None,
            y_ground: None,
            sent: true,
            reason: None,
            y_before: 64.0,
            y_max: 64.0,
            y_after: 64.0,
            rose: false,
            pressed_ticks: 6,
            airborne_ticks: Some(4),
        };
        let jump = JumpRecord::new(Some([150027, 65, 149828]), JumpAttempt::from_jump(&landed_back));
        let seen = walk_details(vec!["jump"], None, Some([150026, 65, 149828]), true, Some(&jump));
        assert_eq!(seen["tried"], json!(["jump"]));
        assert_eq!(seen["healed_once"], json!(true));
        assert_eq!(seen["jump_last"]["kind"], json!("jump_landed_back"));
        assert_eq!(seen["jump_last"]["target"], json!([150027, 65, 149828]));
    }

    #[test]
    fn a_refused_step_under_a_ceiling_carries_the_jump_that_was_never_sent() {
        // t109 B1, verbatim: `move_to{at:[150028,4,149832], exact:true, allow_dig:false}` at 0.07 s with the body on
        // [150028,3,149831] and `dirt` on [150028,5,149832] came back `{reason:"refused"}` and **no** `details`, so
        // the one thing the run proved (nothing was pressed, because the ceiling stops a two-block body) was lost.
        let feet = [150028, 3, 149831];
        let at = [150028, 4, 149832];
        let head = [150028, 5, 149832];
        let refusal = Failure::new(
            "refused",
            "not possible: cannot reach Block([150028, 4, 149832]) from [150028.64, 3.0, 149831.58]: every reachable cell searched",
        );
        let details = enrich_move_details(None, feet, Some(at), true, |pos| pos == head);
        assert_eq!(details["blocked_at"], json!(head));
        assert_eq!(details["feet"], json!(feet));
        assert_eq!(details["tried"], json!([]));
        assert_eq!(details["stuck_at"], json!(feet));
        let jump = &details["jump_last"];
        assert_eq!(jump["kind"], json!("no_headroom"));
        assert_eq!(jump["sent"], json!(false));
        assert_eq!(jump["pressed_ticks"], json!(0), "no key may be pressed to produce this record");
        assert_eq!(jump["why"], json!("no_room_above_launch"));
        assert_eq!(jump["blocked"], json!(head));
        assert_eq!(jump["target"], json!(at));
        // and it survives the trip the model reads it on
        let json = Failure { details: Some(details), ..refusal }.to_json();
        assert_eq!(json["details"]["jump_last"]["kind"], json!("no_headroom"));
        assert_eq!(json["reason"], json!("refused"));
    }

    #[test]
    fn an_open_step_is_not_a_ceiling_and_a_walk_keeps_its_own_record() {
        // Free ceiling: nothing to say about the head, but the fields the verifier reads are still there.
        let details = enrich_move_details(None, [150028, 3, 149831], Some([150028, 4, 149832]), true, |_| false);
        assert!(details["jump_last"].is_null(), "{details}");
        assert_eq!(details["stuck_at"], json!([150028, 3, 149831]));

        // A goal that is not a step (`Near`, level, two up) is not this rule's business.
        for at in [[150028, 3, 149832], [150028, 5, 149832], [150028, 6, 149832]] {
            let details = enrich_move_details(None, [150028, 3, 149831], Some(at), true, |_| true);
            assert!(details["jump_last"].is_null(), "{at:?} is not a one-block step");
        }
        // `exact:false` is a `Near` box: standing beside the step is allowed, so no headroom verdict.
        assert!(step_headroom([150028, 3, 149831], Some([150028, 4, 149832]), false, |_| true).is_none());

        // A walk that already recorded something (t107: pressed and landed back) keeps it, untouched by the step rule.
        let walked = walk_details(vec!["jump"], None, Some([150026, 65, 149828]), true, None);
        let kept = enrich_move_details(Some(walked), [150026, 64, 149828], Some([150026, 65, 149828]), true, |_| false);
        assert_eq!(kept["tried"], json!(["jump"]));
        assert_eq!(kept["healed_once"], json!(true));
        assert_eq!(kept["stuck_at"], json!([150026, 64, 149828]));
    }

    #[test]
    fn a_place_refusal_names_the_support_cell_and_the_cell_the_block_went_into() {
        // t107 F3: the model read `nothing to place against at [150030, 2, 149829]` as "the wall build failed".
        let message = no_support_message([150030, 2, 149829], [150030, 3, 149829]);
        assert!(message.contains("[150030, 2, 149829]"), "{message}");
        assert!(message.contains("target [150030, 3, 149829]"), "{message}");
        assert!(no_support_hint([150030, 2, 149829]).contains("[150030, 2, 149829]"));
    }
}

/// t112: the record a walk reports when the *body* jumped, not the heal path.
///
/// Nine runs reported "no `jump_ok` sample" while the relay saw `jump=1` and the feet rose a block, because the
/// pathfinder's own `Ascend` never writes `walk.jump_last` -- only `jump_step` does. The bot now records every jump
/// where the key becomes a tick, and this pins the other half: such a record becomes the walk's `jump_last` with the
/// same fields a heal-made one has, so the model and the evidence read the same either way.
#[cfg(test)]
mod body_jump_tests {
    use super::*;

    fn a_bodys_step_up() -> acacia_bot::movement::Jump {
        // The t112 numbers: standing at 70.5625, a one-block step, landing at 71.0 (a 0.4375 float rise).
        acacia_bot::movement::Jump {
            sent: true,
            reason: None,
            target: None,
            y_before: 70.5625,
            y_ground: Some(70.0),
            y_max: 71.2,
            y_after: 71.0,
            rose: true,
            pressed_ticks: 2,
            airborne_ticks: Some(11),
        }
    }

    #[test]
    fn a_jump_the_body_made_becomes_a_jump_record() {
        let jump = a_bodys_step_up();
        let record = JumpRecord::from_body(&jump);
        let value = attach_jump(json!({"arrived": true}), Some(&record));
        let last = &value["jump_last"];
        assert_eq!(last["kind"], json!("jump_ok"), "a step climbed is a jump that worked: {last}");
        assert_eq!(last["gained"], json!(1.0), "in blocks, not the 0.4375 float rise");
        assert_eq!(last["from_ground_block"], json!(70));
        assert_eq!(last["to_ground_block"], json!(71));
        assert_eq!(last["sent"], json!(true));
        assert_eq!(last["target"], json!(null));
        assert_eq!(last["y_ground"], json!(70.0));
    }

    #[test]
    fn a_body_jump_that_came_back_down_is_not_reported_as_a_climb() {
        let jump = acacia_bot::movement::Jump {
            sent: true,
            reason: None,
            target: Some([150027, 6, 149833]),
            y_before: 70.0,
            y_ground: Some(70.0),
            y_max: 71.25,
            y_after: 70.0,
            rose: false,
            pressed_ticks: 1,
            airborne_ticks: Some(18),
        };
        let record = JumpRecord::from_body(&jump);
        let value = attach_jump(json!({"arrived": false}), Some(&record));
        assert_eq!(value["jump_last"]["kind"], json!("jump_landed_back"));
        assert_eq!(value["jump_last"]["gained"], json!(0.0));
        assert_eq!(value["jump_last"]["target"], json!([150027, 6, 149833]));
    }
}

/// t113: `allow_dig` was invisible to the pre-flight, so a step under a ceiling was refused even when the caller had
/// explicitly authorised digging -- `move_to{at:[150026,7,149834], allow_dig:true}` on the live server answered the
/// exact same `cannot stand on ... dig that one first` and dug nothing. These pin the permission split, the dig call
/// itself, and the wording of the refusal the server has already seen.
#[cfg(test)]
mod ceiling_authorization_tests {
    use super::*;

    const FEET: [i32; 3] = [150026, 6, 149834];
    const AT: [i32; 3] = [150026, 7, 149834];
    const HEAD: [i32; 3] = [150026, 8, 149834];

    #[test]
    fn the_same_ceiling_is_work_when_digging_is_allowed() {
        assert_eq!(headroom_step(true, Some(HEAD)), HeadroomStep::DigFirst { head: HEAD });
        assert_eq!(headroom_step(false, Some(HEAD)), HeadroomStep::Refuse { head: HEAD });
        // No ceiling over the step (or not a step): nothing to decide, path as usual either way.
        assert_eq!(headroom_step(true, None), HeadroomStep::Walk);
        assert_eq!(headroom_step(false, None), HeadroomStep::Walk);
    }

    #[tokio::test]
    async fn an_authorised_step_digs_the_ceiling_and_only_then_returns_ok() {
        // `move_to` passes `Bot::break_block` here; the recorder stands in for it, so the order the code promises
        // ("dig that cell, and only then walk") is observable: one call, on that cell, before anything else.
        let calls = std::cell::RefCell::new(Vec::new());
        let dug = dig_the_ceiling(HEAD, |cell| {
            calls.borrow_mut().push(cell);
            async { Ok(()) }
        })
        .await;
        assert_eq!(dug, Ok(()));
        assert_eq!(*calls.borrow(), vec![HEAD], "exactly the cell over the step, once");
    }

    #[tokio::test]
    async fn a_dig_that_cannot_be_done_comes_back_as_its_own_reason() {
        let dug = dig_the_ceiling(HEAD, |_| async { Err("not visible from here".to_string()) }).await;
        assert_eq!(dug, Err("not visible from here".to_string()), "the dig error is handed on, not flattened");
    }

    #[test]
    fn a_refusal_without_permission_is_word_for_word_what_the_server_saw() {
        let failure = ceiling_failure_with(FEET, AT, HEAD, |pos| pos == HEAD, None);
        assert_eq!(failure.reason, "blocked_by_terrain");
        assert_eq!(
            failure.message,
            "cannot stand on [150026, 7, 149834]: [150026, 8, 149834] is in the way — dig that one first"
        );
        let hint = failure.hint.clone().unwrap_or_default();
        assert_eq!(hint, no_headroom_hint(HEAD), "digging was not allowed, so the model has to be told to dig");
        let details = failure.details.expect("every failure carries details");
        assert_eq!(details["blocked_at"], json!(HEAD));
        assert_eq!(details["jump_last"]["kind"], json!("no_headroom"));
        assert_eq!(details["jump_last"]["sent"], json!(false));
        assert_eq!(details["jump_last"]["pressed_ticks"], json!(0));
        assert_eq!(details["step_head"]["cell"], json!(HEAD), "the self-report that separates 'no view' from 'solid'");
        assert_eq!(details["step_head"]["solid"], json!(true));
        assert!(details.get("ceiling_dig").is_none(), "nothing was dug, so nothing failed: {details}");
    }

    #[test]
    fn a_dig_that_failed_is_reported_not_swallowed() {
        let failure = ceiling_failure_with(FEET, AT, HEAD, |pos| pos == HEAD, Some("the server refused the break".into()));
        // Same message as the un-authorised case: the model should recognise the situation, then read the hint.
        assert_eq!(
            failure.message,
            "cannot stand on [150026, 7, 149834]: [150026, 8, 149834] is in the way — dig that one first"
        );
        let hint = failure.hint.clone().unwrap_or_default();
        assert_ne!(hint, no_headroom_hint(HEAD), "we were allowed to dig and tried: do not repeat the instruction as if nobody did");
        assert!(hint.contains("the server refused the break") && hint.contains("允许挖路了"), "{hint}");
        let details = failure.details.expect("details");
        assert_eq!(details["ceiling_dig"]["cell"], json!(HEAD));
        assert_eq!(details["ceiling_dig"]["failed"], json!("the server refused the break"));
        assert_eq!(details["step_head"]["solid"], json!(true));
    }
}

#[cfg(test)]
mod eat_tests {
    use serde_json::json;

    use super::not_hungry_hint;

    /// Exactly what `action_failure` hands back for any `refused` action when a free cell exists:
    /// the generic hint that told the model to move (t116).
    fn refused_with_the_free_cell_hint(message: &str) -> super::Failure {
        super::Failure::new("refused", message).with_hint("换个位置：[10,64,10] 是同层空位、下方有支撑")
    }

    /// t116: `eat {}` came back "not hungry" with a hint that pushed the model to move. A full food
    /// bar has nothing to do with where the bot stands, so no hint may say otherwise.
    #[test]
    fn the_not_hungry_hint_talks_about_food_and_never_about_moving() {
        let hint = not_hungry_hint(20.0, 0.0);
        assert!(hint.contains("food=20.0") && hint.contains("saturation=0.0"), "the numbers must be the live ones: {hint}");
        assert!(hint.contains("还不饿"), "{hint}");
        assert!(hint.contains("挖矿") && hint.contains("留"), "say what to do instead: {hint}");
        for wrong in ["换个位置", "换一格", "换一个位置", "挪一格", "move", "another spot", "different spot"] {
            assert!(!hint.contains(wrong), "the hint must not send the model to move ({wrong}): {hint}");
        }
    }

    /// Half a bar is still "not hungry" for the bot, but the numbers have to follow the state, not a
    /// template: a hint that always says 20.0/0.0 is the same lie as a hard-coded message.
    #[test]
    fn the_numbers_come_from_the_state_not_from_the_text() {
        let hint = not_hungry_hint(18.5, 2.25);
        assert!(hint.contains("food=18.5") && hint.contains("saturation=2.2"), "{hint}");
        assert_ne!(hint, not_hungry_hint(20.0, 0.0));
    }

    /// The wiring, not just the wording: the exact failure acacia-bot builds for a full stomach
    /// (`survival/consume.rs`: "not hungry enough to eat …", classified `refused`) is the one that
    /// must come back with the food hint and the numbers in `details`.
    #[test]
    fn the_not_hungry_refusal_is_the_one_that_gets_rewritten() {
        let failure = super::eat_failure_with(
            20.0,
            0.0,
            refused_with_the_free_cell_hint("not possible: not hungry enough to eat minecraft:apple"),
        );
        let hint = failure.hint.clone().unwrap_or_default();
        assert!(hint.contains("food=20.0") && hint.contains("saturation=0.0"), "{hint}");
        assert!(!hint.contains("换个位置") && !hint.contains("换一格"), "{hint}");
        let details = failure.details.expect("details");
        assert_eq!(details["food"], json!(20.0));
        assert_eq!(details["saturation"], json!(0.0));
    }

    /// Anything else a `refused` eat can mean keeps the hint it came with: this fix is about the
    /// stomach, not about silencing every refusal.
    #[test]
    fn other_refusals_keep_their_hint() {
        let failure = super::eat_failure_with(20.0, 0.0, refused_with_the_free_cell_hint("not possible: the window is taken"));
        assert!(failure.hint.is_none() || !failure.hint.clone().unwrap_or_default().contains("还不饿"));
        let missing = super::eat_failure_with(20.0, 0.0, super::Failure::new("no_item", "no minecraft:apple in the inventory"));
        assert_eq!(missing.reason, "no_item");
    }
}

/// The shape of an `equip` answer (t128/t134): which request shape moved the item, what the server said
/// about the shapes that did not, and - on failure - the protocol's name for the refusal. Pure, so it can
/// be pinned without a connected bot.
#[cfg(test)]
mod equip_tests {
    use super::{EquipAttempt, EquipReport, EquipSlot, equip_failure, equip_json};
    use crate::tools::Failure;

    fn refused_rung() -> EquipAttempt {
        EquipAttempt {
            action: "swap",
            status: Some("55 CannotPlaceItem".to_string()),
            message: "rejected by the server: item stack request -5 failed with status 55 - CannotPlaceItem".to_string(),
        }
    }

    /// What the bot leaves behind when the whole ladder was refused.
    fn refused() -> EquipReport {
        EquipReport { action_used: "swap", status_translated: Some("55 CannotPlaceItem".to_string()), attempts: vec![refused_rung()] }
    }

    fn worked(action_used: &'static str) -> EquipReport {
        EquipReport { action_used, status_translated: None, attempts: Vec::new() }
    }

    /// The common case: the exchange worked at once. Say which shape, and say nothing about attempts -
    /// an empty list reads as "tried nothing", the opposite of "worked first time".
    #[test]
    fn a_first_try_swap_names_the_shape_and_omits_attempts() {
        let answer = equip_json("minecraft:diamond_helmet", EquipSlot::Head, &worked("swap"));
        assert_eq!(answer["action_used"], "swap");
        assert_eq!(answer["slot"], "head");
        assert_eq!(answer["equipped"], "minecraft:diamond_helmet");
        assert!(answer.get("attempts").is_none(), "a clean first try must not carry an attempts key: {answer}");
    }

    /// The second shape can be the one that works; the answer has to say so, or a reader cannot tell
    /// a click-move from an exchange-move in a real report.
    #[test]
    fn a_clicks_win_says_clicks() {
        let answer = equip_json("minecraft:shield", EquipSlot::Offhand, &worked("clicks"));
        assert_eq!(answer["action_used"], "clicks");
        assert_eq!(answer["slot"], "offhand");
    }

    /// Nothing moved because the item was already where it belonged.
    #[test]
    fn an_already_there_move_says_none() {
        assert_eq!(equip_json("minecraft:diamond_boots", EquipSlot::Feet, &worked("none"))["action_used"], "none");
    }

    /// When the first shape was refused and the next one worked, the refusal is kept with its protocol
    /// name - not a copy of the message text.
    #[test]
    fn a_refused_rung_is_listed_with_its_status() {
        let report = EquipReport { action_used: "clicks", status_translated: None, attempts: vec![refused_rung()] };
        let answer = equip_json("minecraft:shield", EquipSlot::Offhand, &report);
        assert_eq!(answer["action_used"], "clicks");
        let attempts = answer["attempts"].as_array().expect("attempts present when a shape was refused");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0]["action"], "swap");
        assert_eq!(attempts[0]["status_translated"], "55 CannotPlaceItem");
        assert!(attempts[0]["message"].as_str().unwrap().contains("CannotPlaceItem"));
    }

    /// A successful equip has no refusal of its own. The key stays (the shape is stable) but its value
    /// is `null` - never a guess like "unknown" that would look like an answer.
    #[test]
    fn success_never_invents_a_status() {
        let answer = equip_json("minecraft:iron_pickaxe", EquipSlot::Hand, &worked("swap"));
        assert!(answer["status_translated"].is_null());
        assert_eq!(answer["status_translated"], serde_json::Value::Null);
    }

    /// The failing ladder's code reaches the answer from the report, in the place a caller looks for it.
    #[test]
    fn a_failed_equip_carries_the_protocol_code() {
        let failure = equip_failure(
            Failure::new("refused", "the server refused the item move"),
            "minecraft:shield",
            EquipSlot::Offhand,
            Some(&refused()),
        );
        let answer = failure.to_json();
        assert_eq!(answer["ok"], false);
        assert_eq!(answer["reason"], "refused");
        let details = &answer["details"];
        assert_eq!(details["equipped"], "minecraft:shield");
        assert_eq!(details["slot"], "offhand");
        assert_eq!(details["action_used"], "swap");
        assert_eq!(details["status_translated"], "55 CannotPlaceItem");
        assert_eq!(details["attempts"].as_array().expect("the refused rung is listed").len(), 1);
    }

    /// The code is read from the report, not scraped out of the message: a report without a status must
    /// not yield one, even when the message carries the very text a parser would have matched.
    #[test]
    fn the_code_never_comes_from_the_message_text() {
        let text = "rejected by the server: item stack request -5 failed with status 55 - CannotPlaceItem (tried: swap -> 55 CannotPlaceItem)";
        let mut report = refused();
        report.status_translated = None;
        report.attempts = Vec::new();
        let answer =
            equip_failure(Failure::new("refused", text), "minecraft:shield", EquipSlot::Offhand, Some(&report)).to_json();
        assert!(
            answer["details"]["status_translated"].is_null(),
            "no report status means no code, however loudly the message says it: {answer}"
        );
        assert!(answer["details"].get("attempts").is_none(), "nothing was tried, so nothing is listed: {answer}");
    }

    /// `take_equip_report()` clears as it reads (t129). Mirror that here: the second failure of a run has
    /// no report of its own, so it must come back without codes rather than quoting the first one.
    #[test]
    fn a_drained_report_is_not_reused_by_the_next_call() {
        let mut kept: Option<EquipReport> = Some(refused());
        let first = equip_failure(Failure::new("refused", "first"), "minecraft:shield", EquipSlot::Offhand, kept.take().as_ref())
            .to_json();
        assert_eq!(first["details"]["status_translated"], "55 CannotPlaceItem");
        let second = equip_failure(Failure::new("refused", "second"), "minecraft:shield", EquipSlot::Offhand, kept.take().as_ref())
            .to_json();
        assert!(second["details"]["status_translated"].is_null(), "a drained report must not resurface: {second}");
        assert_eq!(kept, None, "reading the report is what empties it");
    }
}

/// t130: a finished walk that moved nothing is not an arrival. These pin the rule and the answer it
/// produces, so the 46 zero-move `arrived:true` answers of t127 cannot come back.
#[cfg(test)]
mod moved_zero_tests {
    use super::{WalkEnd, moved_zero_failure, walk_end};
    use std::time::Duration;

    const HERE: [i32; 3] = [150012, 64, 149826];

    /// The reported case: the caller asked for the cell next door. Standing still must not be reported as
    /// an arrival, even though the 1.5-block box around `at` already contains where the bot stands.
    #[test]
    fn the_cell_next_door_is_not_reached_by_standing_still() {
        let next_door = [HERE[0] + 1, HERE[1], HERE[2]];
        assert_eq!(walk_end(Some(HERE), Some(HERE), Some(next_door), false), WalkEnd::StoodStill);
    }

    /// Two blocks away, and the mid-range goal (up to 8 blocks) the channel-digging runs used: the same
    /// silence — nothing moved, so nothing was walked.
    #[test]
    fn a_two_block_and_an_eight_block_goal_are_not_reached_by_standing_still() {
        let two = [HERE[0] + 2, HERE[1], HERE[2]];
        let eight = [HERE[0] + 8, HERE[1], HERE[2] - 3];
        assert_eq!(walk_end(Some(HERE), Some(HERE), Some(two), false), WalkEnd::StoodStill);
        assert_eq!(walk_end(Some(HERE), Some(HERE), Some(eight), false), WalkEnd::StoodStill);
    }

    /// A body that did move is an arrival as before — the rule only speaks about zero movement.
    #[test]
    fn a_walk_that_moved_the_body_is_still_an_arrival() {
        let two = [HERE[0] + 2, HERE[1], HERE[2]];
        assert_eq!(walk_end(Some(HERE), Some([HERE[0] + 1, HERE[1], HERE[2]]), Some(two), false), WalkEnd::Arrived);
    }

    /// Standing *on* the asked-for cell is the one honest zero-move arrival: there was nowhere to go.
    #[test]
    fn standing_on_the_asked_for_cell_is_already_there() {
        assert_eq!(walk_end(Some(HERE), Some(HERE), Some(HERE), false), WalkEnd::AlreadyThere);
    }

    /// `exact:true` is judged by `block_position == at` inside its own branch; this rule must not touch it.
    #[test]
    fn the_exact_path_keeps_its_own_verdict() {
        let eight = [HERE[0] + 8, HERE[1], HERE[2]];
        assert_eq!(walk_end(Some(HERE), Some(HERE), Some(eight), true), WalkEnd::Arrived);
    }

    /// An unreadable position is not evidence: never accuse a walk of standing still when the cell is unknown.
    #[test]
    fn an_unknown_position_is_not_accused_of_standing_still() {
        assert_eq!(walk_end(None, Some(HERE), Some([HERE[0] + 2, HERE[1], HERE[2]]), false), WalkEnd::Arrived);
        assert_eq!(walk_end(Some(HERE), None, Some([HERE[0] + 2, HERE[1], HERE[2]]), false), WalkEnd::Arrived);
    }

    /// The answer a caller and a run report actually read: `moved_zero: true`, the cell it never left, and
    /// a message that says "nothing was walked" instead of anything that could be read as arrival.
    #[test]
    fn the_answer_says_moved_zero_and_never_claims_arrival() {
        let next_door = [HERE[0] + 1, HERE[1], HERE[2]];
        let failure = moved_zero_failure(Some(HERE), Some(next_door), 30, Duration::from_secs_f32(0.0004));
        assert_eq!(failure.reason, "timeout");
        assert!(!failure.message.contains("arrived"), "the message must not use the word arrival as a fact: {}", failure.message);
        assert!(failure.message.contains("nothing was walked"));
        assert!(failure.message.contains("not an arrival"));
        let details = failure.details.expect("details carry the verdict");
        assert_eq!(details["moved_zero"], true);
        assert_eq!(details["start_block"], serde_json::json!(HERE));
        assert_eq!(details["at"], serde_json::json!(next_door));
        assert!(details.get("arrived").is_none(), "a failed walk must not carry an arrived key: {details}");
        assert!(failure.hint.unwrap_or_default().contains("一步没走成"));
    }

    /// Sub-block movement needs no float threshold: a 0.3-block shuffle that stays inside one cell reaches
    /// `walk_end` as `start == end` and is caught by the rule we already have, so there is nothing for a
    /// `0.05`-style magic number to improve. Only a shuffle that crosses a cell boundary shows up as another
    /// cell, and that is not a false arrival — `walk_end` is consulted on the `Ok` branch only, which means the
    /// body already sat inside the `Near` 1.5 box, and the cell next door is what that box asked for.
    ///
    /// Keep the criterion discrete on purpose: "same cell, yes or no" can be disproved by evidence, while a
    /// float threshold has to be tuned, and the failures this file chases were all of the "looks right but
    /// cannot be explained" kind.
    #[test]
    fn a_sub_block_wobble_is_judged_by_whole_cells_and_needs_no_float_threshold() {
        // `block_position()` hands over whole cells, so this is the shape `walk_end` really sees for a body
        // shuffling 100.98 → 100.99: the same cell on both ends.
        let cell_before = 100.98f32.floor() as i32;
        let cell_after = 100.99f32.floor() as i32;
        assert_eq!(cell_before, cell_after, "a wobble inside one cell must not change the whole-cell reading");
        let start = [cell_before, 64, 149826];
        let goal_two_away = [cell_before + 2, 64, start[2]];
        assert_eq!(
            walk_end(Some(start), Some([cell_after, 64, start[2]]), Some(goal_two_away), false),
            WalkEnd::StoodStill,
            "0.01 blocks of shuffling is still \"never left the cell\", and the answer must say so"
        );

        // 100.98 → 101.02 is genuinely another cell. The walk only ends `Ok` when the goal box already holds
        // the body, so this is an arrival — reported as one, without a float comparison anywhere.
        let crossed = 101.02f32.floor() as i32;
        assert_eq!(crossed, cell_before + 1, "crossing the boundary has to be visible as a different cell");
        assert_eq!(
            walk_end(Some(start), Some([crossed, 64, start[2]]), Some(goal_two_away), false),
            WalkEnd::Arrived,
            "a body that really moved into the next cell arrived; suppressing that would be the opposite lie"
        );
    }
}

#[cfg(test)]
mod put_json_tests {
    use super::*;

    /// t132 F1: asked 2, the container gained 61 — the report must carry 61 as `count` and 2 as
    /// `asked`, never the request value dressed up as a fact.
    #[test]
    fn the_reported_count_is_what_the_container_gained() {
        let out = put_moved_json("minecraft:cobblestone", 61, 2, 3, None);
        assert_eq!(out["count"], json!(61));
        assert_eq!(out["asked"], json!(2));
        assert_eq!(out["container_slot"], json!(3));
        assert_eq!(out["moved"], json!("minecraft:cobblestone"));
    }

    #[test]
    fn a_short_source_says_so_and_a_clean_put_leaves_no_note_key() {
        let short = put_moved_json("minecraft:dirt", 3, 9, 1, Some("only what there was was sent"));
        assert_eq!(short["note"], json!("only what there was was sent"));
        let clean = put_moved_json("minecraft:dirt", 2, 2, 1, None);
        assert!(clean.get("note").is_none(), "an empty note would read as a problem: {clean}");
    }
}

#[cfg(test)]
mod close_tests {
    use super::*;

    /// t137 F2: `close` with nothing open must read as "closed", not as a failed close.
    #[test]
    fn closing_what_is_already_closed_is_not_a_failure() {
        let out = already_closed_json();
        assert_eq!(out["closed"], json!(true));
        assert_eq!(out["detail"], json!("already_closed"));
        assert!(out["hint"].as_str().is_some_and(|hint| hint.contains("keep_open")), "{out}");
    }
}

#[cfg(test)]
mod dig_tests {
    use super::*;

    /// The three walk endings are three different reasons, and none of them is a synonym of another.
    #[test]
    fn the_three_walk_endings_are_three_reasons() {
        let stops = [
            DigStop::NoPath { stuck_at: None, note: "the search found no way there" },
            DigStop::BlockedByTerrain { stuck_at: None },
            DigStop::OutOfReach { distance: 9.0, reach: 5.7, from: [0, 64, 0] },
        ];
        let reasons: Vec<&str> = stops.into_iter().map(|stop| dig_failure(stop).reason).collect();
        assert_eq!(reasons, vec!["no_path", "blocked_by_terrain", "out_of_reach"]);
    }

    /// "Walked as close as the route allowed and it is still too far" carries the distance and the reach,
    /// because those two numbers are what says whether to walk a little more or to give up on this block.
    #[test]
    fn a_walk_that_ends_short_reports_the_numbers() {
        let failure = dig_failure(DigStop::OutOfReach { distance: 6.4, reach: 5.7, from: [10, 64, 10] });
        assert_eq!(failure.reason, "out_of_reach");
        let details = failure.details.expect("details");
        assert_eq!(details["from"], json!([10, 64, 10]));
        assert!(failure.message.contains("6.40"), "the message repeats it: {}", failure.message);
        assert!(failure.hint.expect("a hint").contains("旁边"));
    }

    /// "No way there" names the cell it aimed at: the caller can pick another target or walk there itself.
    #[test]
    fn a_no_path_says_where_it_was_aiming() {
        let failure = dig_failure(DigStop::NoPath { stuck_at: Some([150026, 6, 149833]), note: "the search found no way there" });
        assert_eq!(failure.reason, "no_path");
        assert_eq!(failure.details.expect("details")["stuck_at"], json!([150026, 6, 149833]));
        assert!(failure.hint.expect("a hint").contains("move_to"));
    }

    /// A blocked walk is not a no-path: its hint is about clearing what is in the way.
    #[test]
    fn a_blocked_walk_does_not_read_like_a_missing_route() {
        let blocked = dig_failure(DigStop::BlockedByTerrain { stuck_at: Some([3, 64, 3]) });
        let no_path = dig_failure(DigStop::NoPath { stuck_at: Some([3, 64, 3]), note: "gone" });
        assert_ne!(blocked.hint, no_path.hint);
        assert!(blocked.hint.expect("a hint").contains("dig"));
    }
}

