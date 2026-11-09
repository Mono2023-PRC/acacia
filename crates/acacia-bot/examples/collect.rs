//! Live check of [`Bot::collect`] / [`Bot::collect_all`]: drops part of a stack, walks to the drop and
//! waits until the server hands it back over, checking the inventory count returns.
//!
//! `cargo run -p acacia-bot --example collect -- <server> <name>`
//!
//! Operator needed only for `/clear` and `/give`. The config must track entities
//! (`Trackers { entities: true, .. }`, off by default) and simulate movement (`physics: true`): a drop
//! is an entity, and an untracked drop can be neither seen nor walked to.
use std::time::{Duration, Instant};

use acacia_bot::client::Client;
use acacia_bot::items::SlotRef;
use acacia_bot::state::Trackers;
use acacia_bot::{Bot, BotConfig};

const ITEM: &str = "minecraft:apple";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    let mut args = std::env::args().skip(1);
    let server = args.next().unwrap_or_else(|| "127.0.0.1:19140".into());
    let name = args.next().unwrap_or_else(|| "Collector".into());

    let mut bot = connect(&server, &name).await?;
    let [x, y, z] = bot.block_position().expect("movement started");
    println!("spawned as {} at {x},{y},{z}; activity {:?}", bot.client().display_name(), bot.activity());

    for cmd in ["/clear @s".to_string(), format!("/give @s {} 5", ITEM.trim_start_matches("minecraft:"))] {
        bot.client().command(&cmd);
        bot.wait_ticks(5).await?;
    }
    bot.wait_ticks(10).await?;
    let count = bot.state().inventory.count_of(item_id(&bot, ITEM).expect("the item registry knows apples"));
    println!("holding {count} apples");

    // Throw two on the ground (`drop_item` sends the throw; the drop appears as an entity).
    let slot = hotbar_of(&bot, ITEM).ok_or("no apple in the hotbar: did /give work (operator)?")?;
    bot.select_hotbar(slot)?;
    bot.drop_item(SlotRef::Main(slot), 2).await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let drop = loop {
        if let Some(id) = bot.nearest_dropped_item(8.0, Some(ITEM)) {
            break id;
        }
        if Instant::now() > deadline {
            return Err("the drop never showed up (Trackers::entities off, or the throw failed)".into());
        }
        bot.wait_ticks(2).await?;
    };
    println!("drop {drop} in sight at {:?}; activity {:?}", bot.state().entities.get(drop).map(|e| e.feet()), bot.activity());

    let before = bot.state().inventory.count_of(item_id(&bot, ITEM).expect("apple id"));
    let started = Instant::now();
    match bot.collect(drop).await {
        Ok(()) => {
            let now = bot.state().inventory.count_of(item_id(&bot, ITEM).expect("apple id"));
            println!("OK   collected drop {drop} in {:?}: apples {before} -> {now}", started.elapsed());
        }
        Err(e) => println!("FAIL collect {drop}: {e}"),
    }

    // A second pass over everything nearby proves the radius scan (no drop left: 0).
    match bot.collect_all(8.0).await {
        Ok(n) => println!("collect_all(8) picked up {n} more"),
        Err(e) => println!("FAIL collect_all: {e}"),
    }
    bot.stop_all();
    bot.disconnect().await;
    Ok(())
}

fn item_id(bot: &Bot, name: &str) -> Option<i32> {
    bot.state().items.id(name)
}

fn hotbar_of(bot: &Bot, name: &str) -> Option<u8> {
    let id = item_id(bot, name)?;
    (0..9u8).find(|&slot| bot.state().inventory.main[usize::from(slot)].network_id == id)
}

async fn connect(server: &str, name: &str) -> Result<Bot, Box<dyn std::error::Error>> {
    let builder = Client::builder(server).offline(name);
    let config = BotConfig {
        physics: true,
        trackers: Trackers { entities: true, ..Trackers::default() },
        ..BotConfig::default()
    };
    let mut bot = Bot::connect(builder, config).await?;
    while bot.movement().is_none_or(|m| !m.is_started()) {
        bot.wait_ticks(1).await?;
    }
    bot.wait_ticks(20).await?;
    Ok(bot)
}
