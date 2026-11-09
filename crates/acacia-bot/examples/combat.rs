//! Live check of the combat composition ([`Bot::defend`] / [`Bot::fight`]) on a server where the bot
//! is operator: summons a zombie, makes it chase, then reports the fight.
//!
//! `cargo run -p acacia-bot --example combat -- <server> <name>`
//!
//! The config matters: `BotConfig::default()` has `physics: false` (cannot walk, so cannot approach)
//! and `Trackers::entities == false` (the zombie is never tracked, and `fight` returns
//! `NotPossible("... is not tracked")`). It also has `events: NONE`, which is fine here; a dialogue bot
//! wants `Events::CHAT`.
use std::time::{Duration, Instant};

use acacia_bot::client::Client;
use acacia_bot::combat::CombatOpts;
use acacia_bot::state::Trackers;
use acacia_bot::{Bot, BotConfig};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    let mut args = std::env::args().skip(1);
    let server = args.next().unwrap_or_else(|| "127.0.0.1:19140".into());
    let name = args.next().unwrap_or_else(|| "Fighter".into());

    let mut bot = connect(&server, &name).await?;
    let [x, y, z] = bot.block_position().expect("movement started");
    println!("spawned as {} at {x},{y},{z}; activity {:?}", bot.client().display_name(), bot.activity());

    // A zombie beside the bot, hurt by the bot's own player so it turns hostile (a summoned mob
    // otherwise strolls). `peaceful` would remove it: normal difficulty, and mob spawning stays off.
    for cmd in ["/difficulty normal".to_string(), format!("/summon zombie {} {} {}", x + 4, y, z), "/damage @e[type=zombie] 1 entity_attack entity @s".into()] {
        bot.client().command(&cmd);
        bot.wait_ticks(2).await?;
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    let (id, distance) = loop {
        if let Some(found) = bot.nearest_enemy(16.0) {
            break found;
        }
        if Instant::now() > deadline {
            return Err("no hostile tracked; is Trackers::entities on?".into());
        }
        bot.wait_ticks(2).await?;
    };
    println!("target entity {id} at {distance:.1} blocks; hostile kinds in sight: {:?}", bot.enemies());

    let health = bot.state().player.health;
    let started = Instant::now();
    let outcome = bot.defend(16.0, &CombatOpts::default()).await;
    match outcome {
        Ok(Some(fought)) => {
            println!("OK   fight with {fought} ended in {:?}; health {health} -> {}", started.elapsed(), bot.state().player.health);
            println!("     target still tracked: {}; activity {:?}", bot.state().entities.get(fought).is_some(), bot.activity());
        }
        Ok(None) => println!("FAIL nothing hostile within 16 blocks"),
        Err(e) => println!("FAIL fight: {e}"),
    }
    // `stop_all` is the one call a task layer needs to hand the bot a new order.
    bot.stop_all();
    println!("after stop_all: activity {:?}, busy {}", bot.activity(), bot.is_busy());
    bot.disconnect().await;
    Ok(())
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
