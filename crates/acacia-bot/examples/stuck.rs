//! Live check of the "dug itself into a hole" path: dig three blocks straight down (the block under the feet
//! each time), print the height and `on_ground` after every break, then climb back out with
//! [`Bot::climb_out`], printing the same after each staircase step.
//!
//! `cargo run -p acacia-bot --example stuck -- <server> <name>`
//!
//! Needs a physics bot (`BotConfig::physics`) and a world where digging down three blocks is allowed. The
//! bot ends up where it started (surface) or reports the height it is stuck at — that report is the bug
//! signature the t19 run could not give: `dig` said "broke", yet y never moved.
use acacia_bot::client::Client;
use acacia_bot::state::Trackers;
use acacia_bot::{Bot, BotConfig};

const DEPTH: u32 = 3;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    let mut args = std::env::args().skip(1);
    let server = args.next().unwrap_or_else(|| "127.0.0.1:19140".into());
    let name = args.next().unwrap_or_else(|| "StuckTester".into());

    let mut bot = connect(&server, &name).await?;
    // Standing on air (a spawn over a drop, a bot that fell through) makes every dig `nothing to break`:
    // wait for solid ground first instead of burning a five-run experiment.
    for _ in 0..40 {
        let feet = bot.block_position().expect("movement started");
        if bot.is_solid([feet[0], feet[1] - 1, feet[2]]) {
            break;
        }
        bot.wait_ticks(5).await?;
    }
    let start = bot.block_position().expect("movement started");
    if !bot.is_solid([start[0], start[1] - 1, start[2]]) {
        println!("!! standing on air at {start:?}: walk onto solid ground first (nothing under the feet to dig)");
        return Ok(());
    }
    report(&bot, "spawned", start);
    println!("in_hole {} (false on open ground)", bot.in_hole());

    // Straight down: the cell under the feet each time. A break that reports success *must* be followed by a
    // fall of one block; y standing still is the t19 bug.
    for i in 1..=DEPTH {
        let feet = bot.block_position().expect("movement started");
        let target = [feet[0], feet[1] - 1, feet[2]];
        let result = bot.break_block(target).await;
        let now = bot.block_position().expect("movement started");
        report(&bot, &format!("dig {i} at {target:?}: {result:?}"), now);
        if result.is_ok() && now[1] == feet[1] {
            println!("  !! broke the support but y did not move ({}) — this is the stuck signature", feet[1]);
        }
    }
    println!("in_hole after digging: {} (true when the shaft is 1x1)", bot.in_hole());

    // Climb back to the height the run started at — "no longer boxed in" is not the same as "back on top"
    // (t29: three runs each climbed one block of a three-block shaft).
    let climbing = bot.climb_out_to(start[1] as f32, acacia_bot::escape::DEFAULT_STEPS).await;
    let after = bot.block_position().expect("movement started");
    report(&bot, &format!("climb_out: {climbing:?}"), after);
    println!(
        "surface was y={}, now y={} ({} back, drop from y={} was {})",
        start[1],
        after[1],
        after[1] >= start[1],
        start[1],
        start[1] - after[1]
    );
    println!("in_hole at the end: {}", bot.in_hole());
    Ok(())
}

fn report(bot: &Bot, what: &str, at: [i32; 3]) {
    let on_ground = bot.movement().is_some_and(|m| m.on_ground());
    println!("{what}: feet {at:?} y={:.2} on_ground={on_ground}", bot.feet_y().unwrap_or(f32::NAN));
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
