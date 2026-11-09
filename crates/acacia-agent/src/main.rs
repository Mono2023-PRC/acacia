//! `acacia-agent` CLI: `serve` (stdio JSON-RPC), `chat` (one task), `tools` (the registry),
//! `runs` (recent traces).
//!
//! The repo's other CLIs hand-roll their argument parsing (`tools/netem`), so this one does too.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use acacia_agent::agent::Limits;
use acacia_agent::config::Config;
use acacia_agent::runner::{self, RunOptions};
use acacia_agent::shell::{self, ShellConfig};
use acacia_agent::tools::Registry;

const USAGE: &str = "\
acacia-agent — an LLM-driven survival agent on acacia

USAGE:
  acacia-agent serve [--address HOST:PORT] [--name NAME] [--config FILE]
  acacia-agent chat \"<task>\" [--address HOST:PORT] [--name NAME] [--config FILE]
                              [--max-steps N] [--wall-secs N] [--allow a,b,c]
                              [--verbose] [--no-connect]
  acacia-agent tools [--config FILE]        print the tool registry as JSON
  acacia-agent runs [--dir DIR] [--last N]  list recent traces

Global flags may come before or after the command (`acacia-agent --allow perceive,act serve`).
The API key comes from ACACIA_LLM_API_KEY (or DEEPSEEK_API_KEY), or llm.api_key in config.json.
`serve` speaks line-delimited JSON-RPC 2.0 on stdin/stdout (logs on stderr) and connects at startup;
`chat` runs one task and prints the final answer.
";

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("acacia-agent: cannot start the runtime: {error}");
            return std::process::ExitCode::from(1);
        }
    };
    let local = tokio::task::LocalSet::new();
    let code = local.block_on(&runtime, run(argv));
    std::process::ExitCode::from(code as u8)
}

#[derive(Default)]
struct Flags {
    values: HashMap<String, String>,
    switches: HashSet<String>,
    positional: Vec<String>,
}

impl Flags {
    fn parse(argv: &[String]) -> Self {
        let mut flags = Flags::default();
        let mut index = 0;
        while index < argv.len() {
            let argument = &argv[index];
            if let Some(body) = argument.strip_prefix("--") {
                if let Some((key, value)) = body.split_once('=') {
                    flags.values.insert(key.to_owned(), value.to_owned());
                } else if index + 1 < argv.len() && !argv[index + 1].starts_with("--") {
                    // `--allow status,observe` style. A task that starts with a dash would need `=`.
                    flags.values.insert(body.to_owned(), argv[index + 1].clone());
                    index += 1;
                } else {
                    flags.switches.insert(body.to_owned());
                }
            } else {
                flags.positional.push(argument.clone());
            }
            index += 1;
        }
        flags
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    fn number<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.value(key)?.parse().ok()
    }

    fn has(&self, key: &str) -> bool {
        self.switches.contains(key)
    }
}

const COMMANDS: [&str; 4] = ["serve", "chat", "tools", "runs"];

async fn run(argv: Vec<String>) -> i32 {
    if argv.iter().any(|argument| argument == "--help" || argument == "-h" || argument == "help") {
        print!("{USAGE}");
        return 0;
    }
    // The subcommand may come after global flags: `acacia-agent --allow perceive,act serve ...`
    // is what a driver that appends its own flags produces (`run_caps.py --backend agent`), so the
    // first *known* command wins and everything else is parsed as that command's flags.
    let Some(index) = argv.iter().position(|argument| COMMANDS.contains(&argument.as_str())) else {
        match argv.first() {
            Some(other) => {
                eprintln!("acacia-agent: unknown command {other:?}\n");
                eprint!("{USAGE}");
            }
            None => eprint!("{USAGE}"),
        }
        return 2;
    };
    let command = argv[index].clone();
    let mut rest: Vec<String> = Vec::with_capacity(argv.len().saturating_sub(1));
    rest.extend_from_slice(&argv[..index]);
    rest.extend_from_slice(&argv[index + 1..]);
    match command.as_str() {
        "serve" => serve(Flags::parse(&rest)).await,
        "chat" => chat(Flags::parse(&rest)).await,
        "tools" => tools(Flags::parse(&rest)),
        "runs" => runs(Flags::parse(&rest)),
        other => {
            eprintln!("acacia-agent: unknown command {other:?}\n");
            eprint!("{USAGE}");
            2
        }
    }
}

/// `"perceive,act,chat"` to a list.
fn flag_list(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_owned).collect()
}

fn load_config(flags: &Flags) -> Result<Config, i32> {
    let path = flags.value("config").map(PathBuf::from);
    Config::load(path.as_deref()).map_err(|error| {
        eprintln!("acacia-agent: {error}");
        2
    })
}

async fn serve(flags: Flags) -> i32 {
    let mut config = match load_config(&flags) {
        Ok(config) => config,
        Err(code) => return code,
    };
    if let Some(address) = flags.value("address") {
        config.address = address.to_owned();
    }
    if let Some(name) = flags.value("name") {
        config.name = name.to_owned();
    }
    if let Some(allow) = flags.value("allow") {
        // An explicit list replaces the configured one; chat is only reachable this way.
        config.tools.allow = flag_list(allow);
        config.tools.deny.clear();
    }
    if let Some(deny) = flags.value("deny") {
        config.tools.deny = flag_list(deny);
    }
    let config = Arc::new(config);
    let shared = shell::shared();
    let shell_config = ShellConfig {
        observe_radius: 8,
        chat: config.chat.clone(),
        safety: config.safety.clone(),
        interactive: false,
        plan_trace: config.agent.plan_trace,
    };
    let handle = shell::spawn(None, shared, shell_config);
    // Connect in the background so the JSON-RPC loop answers `ping` during the handshake.
    let connect_config = config.clone();
    let connect_shell = handle.clone();
    tokio::task::spawn_local(async move {
        if let Err(failure) = connect_shell.connect(connect_config.address.clone(), connect_config.name.clone(), 60).await {
            eprintln!("[acacia-agent] connect failed: {}", failure.message);
        }
    });
    match acacia_agent::rpc::serve(handle, config).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("acacia-agent: {error}");
            error.exit_code()
        }
    }
}

async fn chat(flags: Flags) -> i32 {
    let Some(task) = flags.positional.first().cloned() else {
        eprintln!("acacia-agent: chat needs a task string, e.g. acacia-agent chat \"collect 8 oak logs\"\n");
        eprint!("{USAGE}");
        return 2;
    };
    let config = match load_config(&flags) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let mut limits = config.limits();
    if let Some(steps) = flags.number::<u32>("max-steps") {
        limits.max_steps = steps.clamp(1, 200);
    }
    if let Some(secs) = flags.number::<u64>("wall-secs") {
        limits.wall_clock = std::time::Duration::from_secs(secs.max(1));
    }
    if let Some(allow) = flags.value("allow") {
        limits.allow = Some(flag_list(allow));
    }
    let address = flags.value("address").unwrap_or(&config.address).to_owned();
    let name = flags.value("name").unwrap_or(&config.name).to_owned();

    let shared = shell::shared();
    let shell_config = ShellConfig {
        observe_radius: 8,
        chat: config.chat.clone(),
        safety: config.safety.clone(),
        interactive: true,
        plan_trace: config.agent.plan_trace,
    };
    let handle = shell::spawn(None, shared, shell_config);
    if !flags.has("no-connect") {
        if let Err(failure) = handle.connect(address, name, 60).await {
            eprintln!("acacia-agent: connect failed: {}", failure.message);
            return 3;
        }
    }

    let max_steps = limits.max_steps;
    let options = RunOptions { limits, cancel: Arc::new(AtomicBool::new(false)), verbose: flags.has("verbose") };
    match runner::run_task(&handle, &config, &task, options).await {
        Ok(report) => {
            println!("{}", report.answer);
            // One machine-readable line per run: steps / tool_calls / tokens / compactions / elapsed /
            // max_steps (t76). Before this, a run's cost had to be counted by hand off the trace.
            eprintln!("acacia-agent: stop={} {}", report.stop.as_str(), report.summary_line(max_steps));
            eprintln!("acacia-agent: trace={}", report.trace.display());
            match report.stop {
                acacia_agent::agent::Stop::Answer => 0,
                acacia_agent::agent::Stop::Cancelled => 4,
                acacia_agent::agent::Stop::Disconnected => 6,
                _ => 5,
            }
        }
        Err(error) => {
            eprintln!("acacia-agent: {error}");
            error.exit_code()
        }
    }
}

fn tools(flags: Flags) -> i32 {
    let config = match load_config(&flags) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let registry = Registry::new(&config.tools);
    println!("{}", serde_json::to_string_pretty(&registry.list()).unwrap_or_default());
    eprintln!(
        "acacia-agent: {} perception tools, {} action tools; system prompt {} chars; model {}",
        registry.specs().iter().filter(|spec| spec.table == acacia_agent::tools::Table::Perceive).count(),
        registry.specs().iter().filter(|spec| spec.table == acacia_agent::tools::Table::Act).count(),
        acacia_agent::agent::system_prompt(&config.chat).chars().count(),
        config.llm.model,
    );
    0
}

fn runs(flags: Flags) -> i32 {
    let config = match load_config(&flags) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let dir = flags.value("dir").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(&config.runs_dir));
    let last = flags.number::<usize>("last").unwrap_or(10);
    let read = match std::fs::read_dir(&dir) {
        Ok(read) => read,
        Err(error) => {
            eprintln!("acacia-agent: cannot list {}: {error}", dir.display());
            return 0;
        }
    };
    let mut entries: Vec<(std::time::SystemTime, PathBuf, u64)> = read
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "jsonl"))
        .map(|entry| {
            let meta = entry.metadata().ok();
            let modified = meta.as_ref().and_then(|meta| meta.modified().ok()).unwrap_or(std::time::UNIX_EPOCH);
            let size = meta.map(|meta| meta.len()).unwrap_or(0);
            (modified, entry.path(), size)
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    for (modified, path, size) in entries.into_iter().take(last) {
        let stamp = modified.duration_since(std::time::UNIX_EPOCH).map(|duration| duration.as_secs()).unwrap_or(0);
        println!("{stamp}\t{size}\t{}", path.display());
    }
    0
}

/// `Limits` is re-exported so a caller can build one without the config.
pub fn limits_from_config(config: &Config) -> Limits {
    config.limits()
}
