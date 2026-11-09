//! Live check of the container bulk moves: [`Bot::find_container`], [`Bot::deposit_all`],
//! [`Bot::withdraw`]. Places a chest, gives dirt, deposits everything and pulls it back out.
//!
//! `cargo run -p acacia-bot --example container_ops -- <server> <name>`
//!
//! Operator needed for `/setblock`, `/clear` and `/give`. The config must simulate movement
//! (`physics: true`): opening a container clicks a block face, which needs the terrain and reach.
//! Entity tracking is not required (a chest is a block, not an entity).
use acacia_bot::client::Client;
use acacia_bot::state::Trackers;
use acacia_bot::{Bot, BotConfig};

const ITEM: &str = "minecraft:dirt";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    let mut args = std::env::args().skip(1);
    let server = args.next().unwrap_or_else(|| "127.0.0.1:19140".into());
    let name = args.next().unwrap_or_else(|| "Storer".into());

    let mut bot = connect(&server, &name).await?;
    let [x, y, z] = bot.block_position().expect("movement started");
    let chest = [x + 2, y, z];
    println!("spawned as {} at {x},{y},{z}; activity {:?}", bot.client().display_name(), bot.activity());

    for cmd in [
        "/clear @s".to_string(),
        format!("/setblock {} {} {} chest", chest[0], chest[1], chest[2]),
        format!("/give @s {} 16", ITEM.trim_start_matches("minecraft:")),
    ] {
        bot.client().command(&cmd);
        bot.wait_ticks(4).await?;
    }
    bot.wait_ticks(10).await?;

    // The scan should find the chest we just placed.
    match bot.find_container(6) {
        Some((pos, block)) => println!("nearest container: {block} at {pos:?}"),
        None => println!("WARN no container in 6 blocks (world not loaded yet?)"),
    }
    let held = bot.state().inventory.count_of(item_id(&bot, ITEM).expect("dirt id"));
    println!("holding {held} dirt");

    let started = std::time::Instant::now();
    match bot.deposit_all(chest).await {
        Ok(moved) => {
            let left = bot.state().inventory.count_of(item_id(&bot, ITEM).expect("dirt id"));
            println!("OK   deposit_all moved {moved} stacks in {:?}; dirt left {left}", started.elapsed());
            println!("     activity while the chest is open: {:?}", bot.activity());
        }
        Err(e) => println!("FAIL deposit_all: {e}"),
    }

    let started = std::time::Instant::now();
    match bot.withdraw(chest, ITEM, 16).await {
        Ok(got) => println!("OK   withdraw got {got} {ITEM} in {:?}", started.elapsed()),
        Err(e) => println!("FAIL withdraw: {e}"),
    }
    if bot.state().containers.open.is_some() {
        match bot.close_container().await {
            Ok(()) => println!("chest closed; activity {:?}", bot.activity()),
            Err(e) => println!("FAIL close_container: {e}"),
        }
    }
    bot.stop_all();
    bot.disconnect().await;
    Ok(())
}

fn item_id(bot: &Bot, name: &str) -> Option<i32> {
    bot.state().items.id(name)
}

async fn connect(server: &str, name: &str) -> Result<Bot, Box<dyn std::error::Error>> {
    let builder = Client::builder(server).offline(name);
    let config = BotConfig { physics: true, trackers: Trackers::default(), ..BotConfig::default() };
    let mut bot = Bot::connect(builder, config).await?;
    while bot.movement().is_none_or(|m| !m.is_started()) {
        bot.wait_ticks(1).await?;
    }
    bot.wait_ticks(20).await?;
    Ok(bot)
}
