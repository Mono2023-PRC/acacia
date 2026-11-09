//! Live check of [`Bot::activity`] and [`Bot::stop_all`]: the one answer to "is the bot free?" and the
//! one call that ends whatever it is doing.
//!
//! `cargo run -p acacia-bot --example activity -- <server> <name>`
//!
//! The loop drives a [`Navigator`] by hand (one tick at a time), which is what a task layer does when
//! it wants to observe the bot between ticks: `Bot::goto` would own the bot for the whole walk and
//! nothing could read `activity()` in between.
use std::time::Duration;

use acacia_bot::client::Client;
use acacia_bot::pathfind::{Goal, GotoOpts, NavStatus, Navigator};
use acacia_bot::state::Trackers;
use acacia_bot::{ActionError, Bot, BotConfig};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    let mut args = std::env::args().skip(1);
    let server = args.next().unwrap_or_else(|| "127.0.0.1:19140".into());
    let name = args.next().unwrap_or_else(|| "Observer".into());

    let mut bot = connect(&server, &name).await?;
    let [x, y, z] = bot.block_position().expect("movement started");
    println!("spawned as {} at {x},{y},{z}", bot.client().display_name());
    println!("idle: activity {:?}, busy {}", bot.activity(), bot.is_busy());

    let goal = Goal::XZ(x + 8, z);
    let mut nav = Navigator::new(goal.clone(), GotoOpts::default());
    let mut walking = 0;
    for tick in 0..40 {
        match nav.tick(&mut bot).await {
            Ok(NavStatus::Arrived) => {
                println!("arrived after {tick} ticks; activity {:?}", bot.activity());
                break;
            }
            Ok(_) => {
                if bot.activity() == acacia_bot::Activity::Walking {
                    walking += 1;
                }
                if tick % 10 == 0 {
                    println!("tick {tick}: activity {:?}, {:?}", bot.activity(), nav.remaining());
                }
            }
            Err(e) => {
                println!("FAIL navigator: {e}");
                break;
            }
        }
    }
    println!("walking ticks observed: {walking} of 40");

    // Mid-walk: stop everything. The navigator's next tick reports Cancelled, the bot is free, and
    // nothing it held (keys, placements, a break) is left behind.
    bot.controls().expect("physics bot").forward = 1.0;
    println!("before stop_all: activity {:?}, busy {}", bot.activity(), bot.is_busy());
    bot.stop_all();
    println!("after stop_all:  activity {:?}, busy {}", bot.activity(), bot.is_busy());
    match nav.tick(&mut bot).await {
        Err(ActionError::Cancelled) => println!("navigator tick after stop_all: Cancelled (expected)"),
        Err(e) => println!("navigator tick after stop_all: {e}"),
        Ok(status) => println!("navigator resumed on its own: {status:?}"),
    }
    // The cancel is spent: a fresh action runs normally.
    let mut again = Navigator::new(goal, GotoOpts::default());
    for _ in 0..5 {
        match again.tick(&mut bot).await {
            Ok(_) => {}
            Err(e) => {
                println!("FAIL second navigator tick: {e}");
                break;
            }
        }
    }
    println!("second navigation started; activity {:?}", bot.activity());
    bot.stop_all();
    tokio::time::sleep(Duration::from_millis(100)).await;
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
