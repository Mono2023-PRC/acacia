use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use acacia_client::proto::types::InputData;
use acacia_client::proto::RawPacket;
use acacia_client::{Client, ClientBuilder, ConnectError, DisconnectReason, Event, MemoryBlobStore, PacketFilter, PacketTraceSummary};
use crate::cadence::Ticker;
use crate::events::{BotEvent, EventSource, Events};
use crate::governor::Governor;
use crate::human::Human;
use crate::interact::closing::PendingClose;
use crate::lag::LagLine;
use crate::items::RequestIds;
use crate::movement::{Controls, Idle, Movement};
use crate::prediction_sync::PredictionSync;
use crate::reflex::Reflexes;
use crate::riding::Ride;
use crate::sleep::Bed;
use crate::subchunks::SubChunkRequester;
use crate::spawn::SpawnSequence;
use crate::state::{GameState, PlayerState};
use crate::survival::Survival;
use crate::trace::{self, Recorder};
use crate::world::WorldTracker;
use crate::BotConfig;

mod dimension;
mod tick;

/// One unit of work done by [`Bot::step`].
pub(crate) enum Step {
    Packet(RawPacket),
    Tick,
    /// Nothing for an action to react to (an event was queued).
    Idle,
    Disconnected(DisconnectReason),
}

/// A connected client plus the game state tracked from its packets.
pub struct Bot {
    pub(crate) client: Client,
    pub(crate) state: GameState,
    pub(crate) subscribe: PacketFilter,
    pub(crate) world: Option<WorldTracker>,
    pub(crate) movement: Option<Movement>,
    /// Bots without physics: standing-still input and the loading-screen sequence.
    pub(crate) idle: Option<(Idle, SpawnSequence)>,
    /// Bots without physics: vanilla's sub-chunk requests (physics bots request through `world`).
    subchunks: Option<SubChunkRequester>,
    sync: PredictionSync,
    ticker: Ticker,
    pub(crate) auto_respawn: bool,
    /// Ticks until the scheduled respawn (respawn.rs).
    pub(crate) respawn_in: Option<u32>,
    recorder: Option<Recorder>,
    events: EventSource,
    pub(crate) human: Human,
    pub(crate) survival: Survival,
    /// Events not yet returned by [`Bot::next`] (queued while stepping or while an action waited).
    pub(crate) pending: VecDeque<BotEvent>,
    /// Flags for the next `PlayerAuthInput` (item use, gliding, dismount sneak).
    pub(crate) queued_flags: Vec<InputData>,
    pub(crate) ride: Ride,
    pub(crate) bed: Bed,
    pub(crate) request_ids: RequestIds,
    /// How the last [`Bot::equip_reporting`] went, until a caller collects it: the failing shapes
    /// too, which a bare [`ActionError`] can only describe in prose.
    ///
    /// One slot, drained on read — so it answers "*this* operation's report?" only if the caller
    /// drains it before the operation it wants a report for. [`Bot::equip_reporting`] empties it as
    /// it starts for that reason; a caller that reads at other times must call
    /// [`Bot::take_equip_report`] first, or it may be reading the previous equip's report.
    pub(crate) last_equip_report: Option<crate::survival::EquipReport>,
    pub(crate) reflexes: Reflexes,
    pub(crate) mining: Option<crate::interact::Mining>,
    /// A dimension change in progress.
    travel: Option<dimension::Travel>,
    /// The counts of the last fight, kept even when it ended in an error: the tool layer reports them
    /// instead of scraping the message (combat/mod.rs).
    pub(crate) last_fight: Option<crate::combat::FightReport>,
    /// What the last search did, refused **or not** (t117, widened in t150): the tool layer reads it through
    /// [`Bot::last_plan_stats`] / [`Bot::take_plan_stats`] instead of parsing the message. Shared with the pilot,
    /// which is the only thing that knows. The slot is per operation — [`Bot::goto_with`] clears it on entry — so
    /// `None` means "no search has run since this walk started", never "not searched".
    pub(crate) plan_stats: std::sync::Arc<std::sync::Mutex<Option<crate::pathfind::PlanStats>>>,
    /// The D* Lite table (t145), next to `plan_stats` for the same reason: the pilot owns the searches, and only the
    /// bot can hold state that has to outlive one of them. Dropped by the engine itself when it stops applying.
    pub(crate) dstar: std::sync::Arc<std::sync::Mutex<Option<crate::pathfind::DStarState>>>,
    /// Physics bots: the simulation has started once, and the spawn packets went out with it.
    spawned: bool,
    strict: bool,
    /// A client container close still settling (interact/closing.rs).
    pub(crate) closing: Option<PendingClose>,
    closed: Option<DisconnectReason>,
    pub(crate) governor: Governor,
    pub(crate) act_timing: crate::act_timing::ActTiming,
    /// Ticks in a row the predicted body has been more than a block from the server's position (desync.rs).
    pub(crate) desync_ticks: u32,
    pub(crate) in_flight: crate::interact::pipeline::Pipeline<crate::interact::pipeline::StackUndo>,
    lag: Option<LagLine>,
    pub(crate) cancel: crate::cancel::CancelState,
}

impl Bot {
    /// Connects and spawns. Tracked packets are added to `builder`'s subscription automatically.
    pub async fn connect(builder: ClientBuilder, mut config: BotConfig) -> Result<Bot, ConnectError> {
        config.trackers.block_entities = config.trackers.block_entities.resolve(config.physics);
        let events = EventSource::new(config.events, config.chat_patterns);
        let mut tracked: PacketFilter = config.trackers.packet_ids().into_iter().chain(events.packet_ids()).collect();
        tracked = PredictionSync::PACKETS.iter().chain(crate::sleep::PACKETS).chain(dimension::PACKETS).fold(tracked, |f, &id| f.with(id));
        tracked = WorldTracker::PACKETS.iter().fold(tracked, |f, &id| f.with(id));
        let mode = if config.physics { Movement::PACKETS } else { SubChunkRequester::PACKETS };
        tracked = mode.iter().fold(tracked, |f, &id| f.with(id));
        if config.physics && config.record.is_some() {
            tracked = trace::ANALYSIS_PACKETS.iter().fold(tracked, |f, &id| f.with(id));
        }
        let server = builder.server().to_owned();
        // Physics reads terrain from blobs; a trace must hold every blob it uses, so it gets a fresh store.
        let mut builder = builder.keep_blob_payloads(config.physics).mouse_input(config.mouse_input);
        if config.physics && config.record.is_some() {
            builder = builder.blob_store(Arc::new(MemoryBlobStore::with_payloads()));
        }
        // The bot sends SetLocalPlayerAsInitialized itself, when it leaves the loading screen (spawn.rs).
        let client = builder.filter(tracked.union(&config.subscribe)).initialize_on_spawn(false).strict(config.strict).connect().await?;
        let state = GameState::new(config.trackers, client.runtime_entity_id());
        let recorder = config.record.as_deref().filter(|_| config.physics).and_then(|path| {
            Recorder::create(path).inspect_err(|e| tracing::warn!(error = %e, "cannot record trace")).ok()
        });
        let mut world = WorldTracker::new(server, config.shared_worlds);
        if let Some(store) = client.blob_store() {
            world.set_blob_store(store.clone());
        }
        let (world, movement, idle) = if config.physics {
            let mut movement = Movement::new();
            movement.mouse_input = config.mouse_input;
            movement.rotation_noise = Some(crate::movement::RotationNoise::new(crate::cadence::seed()));
            (world, Some(movement), None)
        } else {
            (world.nearby(), None, Some((Idle::default(), SpawnSequence::default())))
        };
        Ok(Bot {
            client,
            state,
            subscribe: config.subscribe,
            world: Some(world),
            movement,
            subchunks: idle.is_some().then(SubChunkRequester::default),
            idle,
            sync: PredictionSync::default(),
            ticker: Ticker::new(),
            auto_respawn: config.auto_respawn,
            respawn_in: None,
            recorder,
            events,
            human: Human::default(),
            survival: Survival::new(config.auto_eat),
            pending: VecDeque::new(),
            queued_flags: Vec::new(),
            ride: Ride::default(),
            bed: Bed::default(),
            request_ids: RequestIds::default(),
            last_equip_report: None,
            reflexes: Reflexes::default(),
            mining: None,
            travel: None,
            spawned: false,
            strict: config.strict,
            closing: None,
            closed: None,
            governor: Governor::default(),
            act_timing: Default::default(),
            desync_ticks: 0,
            last_fight: None,
            plan_stats: Default::default(),
            dstar: Default::default(),
            in_flight: Default::default(),
            lag: config.lag.map(LagLine::new),
            cancel: Default::default(),
        })
    }

    /// Why the connection ended, once it has.
    pub fn disconnect_reason(&self) -> Option<&DisconnectReason> {
        self.closed.as_ref()
    }

    /// Labels the recorded trace from here on (no-op when not recording).
    pub fn trace_mark(&mut self, label: &str) {
        if let Some(r) = &mut self.recorder {
            r.write(&trace::Event::Mark(label.to_owned()));
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Recent inbound packets (filtered ones too) and the server's silence, for stall diagnostics.
    pub fn packet_trace(&self) -> PacketTraceSummary {
        self.client.packet_trace()
    }

    pub fn state(&self) -> &GameState {
        &self.state
    }

    pub fn world(&self) -> Option<&WorldTracker> {
        self.world.as_ref()
    }

    /// Movement intent (physics bots only); applied from the next tick on.
    pub fn controls(&mut self) -> Option<&mut Controls> {
        self.movement.as_mut().map(|m| &mut m.controls)
    }

    /// How the last [`Bot::equip_reporting`] (or [`Bot::equip`]) went, cleared as it is read.
    ///
    /// A failed equip still leaves its report here: `ActionError` can only say what went wrong in
    /// prose, while a caller that answers refusals programmatically needs the status code and which
    /// request shapes were already tried.
    ///
    /// The slot is single and drained on read: drain it *before* an equip whose report you want, or
    /// you may collect the previous one's (an equip also empties it as it starts, which covers the
    /// call-then-read order but not reads that happen between unrelated operations).
    pub fn take_equip_report(&mut self) -> Option<crate::survival::EquipReport> {
        self.last_equip_report.take()
    }

    pub fn movement(&self) -> Option<&Movement> {
        self.movement.as_ref()
    }

    /// The last jump the body made, whoever pressed the key: the pathfinder's `steer`/`maneuver`/`shortcut`/`leap`,
    /// the bot's own [`Self::hop_toward`], or a replay (t112).
    ///
    /// `move_to` used to report jumps only from the heal path, so a walk that climbed a step by the pathfinder's own
    /// `Ascend` came back with no record at all, although the server saw `jump=1` and the feet rose a block.
    pub fn last_jump(&self) -> Option<&crate::movement::Jump> {
        self.movement.as_ref().and_then(|m| m.last_jump())
    }

    /// Jumps finished so far; compare it before and after an action to know whether [`Self::last_jump`] is fresh.
    pub fn jump_count(&self) -> Option<u64> {
        self.movement.as_ref().map(|m| m.jump_count())
    }

    /// Closes the connection and waits (up to 2 s) for the disconnect to be sent, so the server
    /// frees the session at once instead of timing it out ("already logged in" on a quick relog).
    pub async fn disconnect(&mut self) {
        self.client.close();
        let closed = async {
            while let Some(event) = self.next().await {
                if matches!(event, BotEvent::Disconnected(_)) {
                    break;
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), closed).await;
    }

    /// Processes incoming packets and ticks until an event (typed or a subscribed packet) is ready or
    /// the connection ends. Must be polled continuously for state, input and movement to run.
    pub async fn next(&mut self) -> Option<BotEvent> {
        self.idle_again();
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            self.survival.idle = true;
            match self.step().await? {
                Step::Packet(packet) => self.keep_for_caller(packet),
                Step::Tick if self.events.wants(Events::TICKS) => return Some(BotEvent::Tick),
                Step::Tick | Step::Idle => {}
                Step::Disconnected(reason) => return Some(BotEvent::Disconnected(reason)),
            }
        }
    }

    /// Handles one incoming packet or movement tick, updating state. `None` once the connection is gone.
    pub(crate) async fn step(&mut self) -> Option<Step> {
        if let Some(world) = &mut self.world {
            for request in world.outgoing.drain(..) {
                self.client.send(&request);
            }
            let rejected = world.rejected.drain(..).filter(|_| self.strict);
            self.pending.extend(rejected.map(BotEvent::Violation));
        }
        if let Some(packet) = self.lag.as_mut().and_then(|l| l.pop_due(tokio::time::Instant::now())) {
            self.apply(&packet);
            return Some(Step::Packet(packet));
        }
        // A queued event with no tick due skips the select!, whose tick timer was registered and cleared per packet.
        if self.lag.is_none()
            && !self.ticker.due()
            && let Some(event) = self.client.try_recv()
        {
            return self.on_event(event);
        }
        let lagged = self.lag.as_ref().and_then(LagLine::next_at);
        tokio::select! {
            event = self.client.recv() => self.on_event(event?),
            // In an async block: select! builds a disabled branch's future too, and a bare sleep looks up the timer.
            _ = async { tokio::time::sleep_until(lagged.unwrap_or_else(tokio::time::Instant::now)).await }, if lagged.is_some() => Some(Step::Idle),
            ticks = self.ticker.wait() => {
                (0..ticks).for_each(|_| self.on_tick());
                Some(Step::Tick)
            }
        }
    }

    fn on_event(&mut self, event: Event) -> Option<Step> {
        match event {
            Event::Packet(packet) if self.lag.is_some() => {
                self.lag.as_mut()?.push(tokio::time::Instant::now(), packet);
                Some(Step::Idle)
            }
            Event::Packet(packet) => {
                self.apply(&packet);
                Some(Step::Packet(packet))
            }
            Event::Violation(violation) => {
                self.pending.push_back(BotEvent::Violation(violation));
                Some(Step::Idle)
            }
            Event::Disconnected(reason) => {
                self.closed = Some(reason.clone());
                Some(Step::Disconnected(reason))
            }
        }
    }

    fn apply(&mut self, packet: &RawPacket) {
        if let Some(r) = &mut self.recorder
            && (PlayerState::PACKETS.contains(&packet.id)
                || WorldTracker::PACKETS.contains(&packet.id)
                || Movement::PACKETS.contains(&packet.id)
                || trace::ANALYSIS_PACKETS.contains(&packet.id) && trace::about(packet, self.state.me().runtime_entity_id))
        {
            r.write(&trace::Event::Packet(packet.clone()));
        }
        let was_alive = self.state.player.alive;
        let mut result = self.state.apply(packet);
        let me = self.state.me();
        if let Some(world) = &mut self.world {
            result = result.and(world.apply(packet));
        }
        if let Some(movement) = &mut self.movement {
            result = result.and(movement.apply(packet, &me));
        }
        if let Err(e) = result {
            tracing::debug!(id = packet.id, error = %e, "tracker failed to decode packet");
        }
        self.on_container_close(packet);
        self.sync.apply(packet, self.state.player.runtime_entity_id);
        if let Some(subchunks) = &mut self.subchunks {
            subchunks.apply(packet);
        }
        self.on_travel_packet(packet);
        self.auto_respawn(packet, was_alive);
        self.reflex_packet(packet);
        self.on_fetch_packet(packet);
        self.events.on_packet(packet, &mut self.state, &mut self.pending);
    }
}
