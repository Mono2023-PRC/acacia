//! Confirmation against a toy server with injected lag: actions reach it after `up` ticks (or never: lost),
//! its block changes reach the bot after `down` ticks (in order, with spikes for resent datagrams).

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use acacia_world::BlockRegistry;

use super::{give_back, unpredict, Confirm, Expect, Failure, Verdict};
use crate::cadence::splitmix;
use crate::governor::Governor;
use crate::state::ItemStack;

struct Net {
    up: u64,
    down: u64,
    /// Chance an action never reaches the server.
    lost: f32,
    /// Chance an echo is held back `spike` more ticks (and everything behind it).
    late: f32,
    spike: u64,
    /// The server refuses every action.
    refuse: bool,
}

struct World {
    registry: std::sync::Arc<BlockRegistry>,
    server: HashMap<i32, u32>,
    client: HashMap<i32, u32>,
    to_server: VecDeque<(u64, i32, u32)>,
    to_client: VecDeque<(u64, i32, u32)>,
    rng: u64,
    stone: u32,
    air: u32,
}

impl World {
    fn new() -> Self {
        let registry = BlockRegistry::vanilla_arc();
        let stone = registry.states_of("minecraft:stone").next().expect("stone").0;
        let air = registry.states_of("minecraft:air").next().expect("air").0;
        let row: HashMap<i32, u32> = (0..20).map(|x| (x, if x % 2 == 0 { stone } else { air })).collect();
        Self { registry, server: row.clone(), client: row, to_server: VecDeque::new(), to_client: VecDeque::new(), rng: 7, stone, air }
    }

    fn chance(&mut self, p: f32) -> bool {
        ((splitmix(&mut self.rng) >> 40) as f32 / (1u64 << 24) as f32) < p
    }

    fn send(&mut self, net: &Net, now: u64, x: i32, want: u32) {
        if !self.chance(net.lost) {
            self.to_server.push_back((now + net.up, x, want));
        }
    }

    /// One tick: the server applies arrived actions (an unchanged cell is echoed too: a refusal), echoes arrive.
    fn tick(&mut self, net: &Net, now: u64) -> Vec<i32> {
        while self.to_server.front().is_some_and(|(at, ..)| *at <= now) {
            let (_, x, want) = self.to_server.pop_front().expect("due");
            if !net.refuse {
                self.server.insert(x, want);
            }
            let mut at = now + net.down + if self.chance(net.late) { net.spike } else { 0 };
            at = at.max(self.to_client.back().map_or(0, |(t, ..)| *t));
            self.to_client.push_back((at, x, self.server[&x]));
        }
        let mut echoed = Vec::new();
        while self.to_client.front().is_some_and(|(at, ..)| *at <= now) {
            let (_, x, id) = self.to_client.pop_front().expect("due");
            self.client.insert(x, id);
            echoed.push(x);
        }
        echoed
    }

    fn cell(&self, x: i32) -> Option<(u32, &acacia_world::BlockState)> {
        let id = self.client[&x];
        self.registry.get(id).map(|s| (id, s))
    }
}

struct Outcome {
    verdicts: Vec<Verdict>,
    max_tries: u32,
    ticks: u64,
}

/// Flips every cell of the row (breaks the stone, places stone in the gaps) one action at a time, as
/// `break_block` / `place_block` do.
fn flip_row(w: &mut World, net: &Net, g: &mut Governor) -> Outcome {
    let mut now = 0;
    let mut out = Outcome { verdicts: Vec::new(), max_tries: 0, ticks: 0 };
    for x in 0..20 {
        let (expect, want) = if w.client[&x] == w.air { (Expect::Solid, w.stone) } else { (Expect::Gone, w.air) };
        // Placements are predicted, as `place_block` does: the bot's world shows the block at once.
        let place = expect == Expect::Solid;
        let mut c = Confirm::new(expect, Some(w.client[&x]), now, g.timeout_ticks(), 3);
        if place {
            c = c.predicted();
        }
        let old = w.client[&x];
        let send = |w: &mut World, now| {
            w.send(net, now, x, want);
            if place {
                w.client.insert(x, want);
            }
        };
        send(w, now);
        let sent = now;
        let verdict = loop {
            now += 1;
            g.tick(None);
            let echoed = w.tick(net, now).contains(&x);
            let echo_id = echoed.then(|| w.client[&x]);
            let v = c.check(now, w.cell(x), echo_id);
            if place && !matches!(v, Verdict::Wait | Verdict::Done) && c.unechoed() {
                w.client.insert(x, old);
            }
            match v {
                Verdict::Wait => {}
                Verdict::Retry => {
                    c.retried(now, g.timeout_ticks());
                    send(w, now);
                }
                v => break v,
            }
        };
        if verdict == Verdict::Done && c.tries() == 1 {
            g.sample(Duration::from_millis(50 * (now - sent)));
        }
        out.max_tries = out.max_tries.max(c.tries());
        out.verdicts.push(verdict);
    }
    // Let late echoes land before comparing worlds.
    for _ in 0..200 {
        now += 1;
        w.tick(net, now);
    }
    out.ticks = now;
    out
}

fn ghosts(w: &World) -> Vec<i32> {
    (0..20).filter(|x| w.client[x] != w.server[x]).collect()
}

#[test]
fn lagging_echoes_confirm_without_ghosts() {
    let net = Net { up: 4, down: 4, lost: 0.0, late: 0.0, spike: 0, refuse: false };
    let (mut w, mut g) = (World::new(), Governor::default());
    let r = flip_row(&mut w, &net, &mut g);
    assert!(r.verdicts.iter().all(|v| *v == Verdict::Done), "{:?}", r.verdicts);
    assert_eq!(r.max_tries, 1, "an 8-tick echo fits the timeout: no retries");
    assert!(ghosts(&w).is_empty(), "client and server differ at {:?}", ghosts(&w));
    let rtt = g.rtt().expect("measured").as_millis();
    assert!((400..=450).contains(&rtt), "echo round-trip {rtt} ms");
}

#[test]
fn lost_actions_and_late_echoes_retry_without_ghosts_or_loops() {
    let net = Net { up: 3, down: 3, lost: 0.15, late: 0.1, spike: 16, refuse: false };
    let (mut w, mut g) = (World::new(), Governor::default());
    let r = flip_row(&mut w, &net, &mut g);
    assert!(r.verdicts.iter().all(|v| *v == Verdict::Done), "{:?}", r.verdicts);
    assert!(r.max_tries > 1, "some action was lost and sent again");
    assert!(r.max_tries <= 3, "no more than 3 tries: {}", r.max_tries);
    assert!(ghosts(&w).is_empty(), "client and server differ at {:?}", ghosts(&w));
    assert!(r.ticks < 20 * 60 + 200, "{} ticks", r.ticks);
}

#[test]
fn refused_actions_fail_after_three_tries_and_leave_the_world_as_the_server_has_it() {
    let net = Net { up: 2, down: 2, lost: 0.0, late: 0.0, spike: 0, refuse: true };
    let (mut w, mut g) = (World::new(), Governor::default());
    let r = flip_row(&mut w, &net, &mut g);
    assert!(r.verdicts.iter().all(|v| matches!(v, Verdict::Failed(_))), "{:?}", r.verdicts);
    assert_eq!(r.max_tries, 3);
    assert!(ghosts(&w).is_empty());
    // A refusal is believed only at the deadline: an echo carrying the old block may be an older update
    // (NetEase resyncs whole sections), so each try is waited out (12 ticks) instead of ending at its echo.
    assert!(r.ticks < 20 * 3 * 14 + 200, "{} ticks", r.ticks);
}

#[test]
fn a_cell_changed_otherwise_fails_at_once() {
    let w = World::new();
    let mut c = Confirm::new(Expect::Gone, Some(w.stone), 0, 12, 3);
    let dirt = w.registry.states_of("minecraft:dirt").next().expect("dirt");
    assert_eq!(c.check(1, Some(dirt), None), Verdict::Wait, "before the deadline anything may still come");
    assert!(matches!(c.check(12, Some(dirt), None), Verdict::Failed(Failure::Other { name }) if name.contains("dirt")));
}

#[test]
fn a_predicted_block_counts_only_once_the_server_echoes_it() {
    let w = World::new();
    let stone = w.registry.states_of("minecraft:stone").next().expect("stone");
    let mut c = Confirm::new(Expect::Solid, Some(w.air), 0, 12, 3).predicted();
    assert_eq!(c.check(1, Some(stone), None), Verdict::Wait, "our own prediction is no confirmation");
    assert_eq!(c.check(12, Some(stone), None), Verdict::Retry, "no echo by the deadline: as unchanged");
    c.retried(12, 12);
    assert_eq!(c.check(15, Some(stone), Some(stone.0)), Verdict::Done);
}

#[test]
fn a_refused_placement_gives_the_block_back() {
    let block = ItemStack { network_id: 5, count: 10, block_runtime_id: 3, ..ItemStack::default() };
    let left = ItemStack { count: 9, ..block.clone() };
    assert_eq!(unpredict(&left, &left, &block), Some(block.clone()), "slot still as predicted: undo");
    let server = ItemStack { count: 7, ..block.clone() };
    assert_eq!(unpredict(&server, &left, &block), None, "the server set the slot since: keep its stack");
}

#[test]
fn a_failed_pipelined_placement_gives_its_item_back_past_later_predictions() {
    let two = ItemStack { network_id: 5, count: 2, block_runtime_id: 3, stack_network_id: Some(9), ..ItemStack::default() };
    let one = ItemStack { count: 1, ..two.clone() };
    // Both placements predicted (2 -> 1 -> empty); the first then fails: one item back, not "no match".
    assert_eq!(give_back(&ItemStack::default(), &one, &two), Some(one.clone()));
    assert_eq!(give_back(&one, &ItemStack::default(), &one), Some(two.clone()), "the last one's back too");
    let other = ItemStack { network_id: 6, count: 4, ..ItemStack::default() };
    assert_eq!(give_back(&other, &one, &two), None, "another item there now: the server's update decides");
}

#[test]
fn a_prediction_only_counts_once_the_echo_lands() {
    let registry = BlockRegistry::vanilla_arc();
    let stone = registry.states_of("minecraft:stone").next().expect("stone");
    let air = registry.states_of("minecraft:air").next().expect("air");
    let mut c = Confirm::new(Expect::Solid, Some(air.0), 0, 20, 3).predicted();
    assert!(!c.satisfied(Some((stone.0, &stone.1)), false), "the bot's own prediction is not the server's word");
    assert!(c.satisfied(Some((stone.0, &stone.1)), true));
    // A retry re-arms the prediction: only the next echo counts again.
    c.retried(1, 20);
    assert!(!c.satisfied(Some((stone.0, &stone.1)), false));
    assert!(c.satisfied(Some((stone.0, &stone.1)), true));
}

#[test]
fn a_break_settles_on_air_without_waiting_for_an_echo() {
    let registry = BlockRegistry::vanilla_arc();
    let stone = registry.states_of("minecraft:stone").next().expect("stone");
    let air = registry.states_of("minecraft:air").next().expect("air");
    let c = Confirm::new(Expect::Gone, Some(stone.0), 0, 20, 3);
    assert!(c.satisfied(Some((air.0, &air.1)), false), "the cell turning to air is the server breaking it");
    assert!(!c.satisfied(Some((stone.0, &stone.1)), false));
}

#[test]
fn an_echo_showing_the_old_block_is_the_server_refusing() {
    let w = World::new();
    let mut c = Confirm::new(Expect::Gone, Some(w.stone), 0, 12, 1);
    assert_eq!(c.check(1, w.cell(0), Some(w.stone)), Verdict::Wait, "the echo may be an older update: wait it out");
    assert_eq!(c.check(12, w.cell(0), None), Verdict::Failed(Failure::ServerRejected), "the block is still there");
}

#[test]
fn an_echo_the_view_never_shows_is_a_stale_view_not_a_refusal() {
    let w = World::new();
    let mut c = Confirm::new(Expect::Gone, Some(w.stone), 0, 12, 2);
    // The server says air at tick 5, the view still shows stone: wait, the view may be a packet behind.
    assert_eq!(c.check(5, w.cell(0), Some(w.air)), Verdict::Wait);
    // One more try is allowed; the stale-view evidence stays with the confirmation.
    assert_eq!(c.check(12, w.cell(0), None), Verdict::Retry);
    c.retried(12, 12);
    // The deadline runs out with the view still on stone: resync the section, do not resend the break.
    assert_eq!(c.check(24, w.cell(0), None), Verdict::Failed(Failure::NoLocalChange));
    assert_eq!(c.failure(Some("minecraft:stone")).kind(), "no_local_change");
}

#[test]
fn nothing_at_all_is_no_effect() {
    let w = World::new();
    let mut c = Confirm::new(Expect::Gone, Some(w.stone), 0, 12, 1);
    assert_eq!(c.check(12, w.cell(0), None), Verdict::Failed(Failure::NoEffect));
    assert_eq!(c.failure(Some("minecraft:stone")).kind(), "no_effect");
    assert_eq!(c.failure(None).kind(), "unloaded");
}
