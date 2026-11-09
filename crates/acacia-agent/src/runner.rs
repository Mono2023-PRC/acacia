//! One run, wired up: provider + shell tools + registry + trace, shared by `chat` and `serve`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::json;

use crate::agent::{Agent, CancelFlag, Limits, Report, Trace};
use crate::config::Config;
use crate::error::AgentError;
use crate::llm::openai::OpenAiClient;
use crate::shell::{ShellHandle, ShellTools};
use crate::tools::Registry;

/// How `chat` and `agent/run` start a run.
pub struct RunOptions {
    pub limits: Limits,
    pub cancel: CancelFlag,
    pub verbose: bool,
}

impl RunOptions {
    pub fn from_config(config: &Config) -> Self {
        Self { limits: config.limits(), cancel: Arc::new(AtomicBool::new(false)), verbose: false }
    }
}

/// Builds the pieces and runs one task to completion.
pub async fn run_task(shell: &ShellHandle, config: &Config, task: &str, options: RunOptions) -> crate::Result<Report> {
    let api_key = config.api_key().ok_or_else(|| {
        AgentError::Config(
            "no API key: set ACACIA_LLM_API_KEY (or DEEPSEEK_API_KEY), or llm.api_key in config.json".to_owned(),
        )
    })?;
    let llm = OpenAiClient::new(&config.llm, api_key)?;
    let registry = Registry::new(&config.tools).with_run_allow(options.limits.allow.clone());
    let mut trace = Trace::open(
        Path::new(&config.runs_dir),
        task,
        json!({
            "model": config.llm.model,
            "address": config.address,
            "limits": {
                "max_steps": options.limits.max_steps,
                "wall_clock_s": options.limits.wall_clock.as_secs(),
                "max_total_tokens": options.limits.max_total_tokens,
            },
            "config": config.redacted(),
        }),
    )?;

    // A previous run (or a stray action) must not leak into this one: drop everything once.
    let stopped = shell.call(crate::tools::Call::Stop).await;
    trace.record(
        "run_start_stop",
        json!({
            "ok": stopped.is_ok(),
            "error": stopped.as_ref().err().map(|failure| failure.message.clone()),
        }),
    );

    if options.verbose {
        eprintln!(
            "[acacia-agent] run: model={} tools={} trace={}",
            config.llm.model,
            registry.specs().len(),
            trace.path().display()
        );
    }

    // Read before the move: `limits` is not `Copy` (t72 build fix; reported to its author).
    let max_steps = options.limits.max_steps;
    let mut agent = Agent {
        llm,
        tools: ShellTools::new(shell.clone()),
        registry,
        limits: options.limits,
        system: crate::agent::system_prompt(&config.chat),
        chat: config.chat.clone(),
        safety: config.safety.clone(),
        cancel: options.cancel,
    };
    let report = agent.run(task, &mut trace).await?;
    // Every run, not only `--verbose`: t69's "55 calls, 480k tokens, 40 minutes" was reconstructed by
    // counting the trace by hand, and a cost line that only appears under a flag does not get counted.
    eprintln!(
        "acacia-agent: stop={} {}",
        report.stop.as_str(),
        report.summary_line(max_steps)
    );
    Ok(report)
}
