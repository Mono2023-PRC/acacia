//! Configuration: `config.json` plus environment overrides, with the API key never echoed.
//!
//! Precedence (highest first): environment → `--config FILE` → `./config.json` → defaults.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{AgentError, Result};

/// The reply budget used when nothing else says otherwise.
///
/// It is also the ceiling's partner: prepaid proxies reserve credit proportional to `max_tokens`
/// (t116 measured ＄0.51–0.75 per call at 4096 against a ＄0.068 balance), so the value is tunable
/// through [`MAX_TOKENS_ENV`] and any unusable setting falls back here instead of failing to start.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// The environment variable that overrides [`LlmConfig::max_tokens`].
pub const MAX_TOKENS_ENV: &str = "ACACIA_LLM_MAX_TOKENS";

/// The environment variable that turns the pathfinding trace on (t149): every `move_to`, `dig` and `place`
/// that ran a search prints one `[plan] nodes=… cost=…` line to stderr (two fields since t155: master has
/// a single engine, so `engine`/`fallback`/`reused` stopped saying anything).
///
/// Off unless asked for: the line is diagnostic, `serve` shares stderr with its supervisor, and a silent
/// default is what keeps the existing output byte-for-byte unchanged.
pub const PLAN_TRACE_ENV: &str = "ACACIA_PLAN_TRACE";

/// The largest reply budget this client accepts. Above it the request is almost certainly a typo
/// (`40096`, or tokens where a count was meant), and a model that long is not a reply but a dump.
pub const MAX_TOKENS_CEILING: u32 = 8192;

/// The whole configuration. Every section has defaults, so an empty file is valid.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The Shuttler (or direct BDS) listener the bot dials.
    pub address: String,
    /// Player name used for the offline login.
    pub name: String,
    pub llm: LlmConfig,
    pub agent: AgentConfig,
    pub tools: ToolsConfig,
    pub chat: ChatConfig,
    pub safety: SafetyConfig,
    /// Where per-run JSONL traces are written.
    pub runs_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    /// A key pasted into the file. Prefer [`LlmConfig::api_key_env`]; kept for `serve` behind a manager.
    pub api_key: Option<String>,
    /// Name of the environment variable holding the key.
    pub api_key_env: String,
    /// Reply budget per request. Overridable at runtime by `ACACIA_LLM_MAX_TOKENS` (see
    /// [`parse_max_tokens`] for what counts as usable).
    pub max_tokens: u32,
    /// Which layer chose [`LlmConfig::max_tokens`] (t121): `Some("ACACIA_LLM_MAX_TOKENS")` when the
    /// variable supplied it, `None` when the file or the default did.
    ///
    /// It exists so a reply cut at this budget can name the knob that cut it, instead of telling the
    /// operator "your reply was too long" while the actual cause is a 256-token setting they forgot.
    /// Not part of the file format: a `config.json` written by an older version still loads, and one
    /// written by this version does not grow a key nobody sets by hand.
    #[serde(skip)]
    pub max_tokens_source: Option<String>,
    pub context_window: u64,
    pub timeout_s: u64,
    pub temperature: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub max_steps: u32,
    pub wall_secs: u64,
    pub compact_threshold: f32,
    pub compact_keep_recent: usize,
    pub max_total_tokens: u64,
    /// `brief` or `full`: the state message emitted after a tool result.
    pub post_action_observe: String,
    /// Automatic reconnect attempts after the connection drops (0 disables them).
    pub reconnects: u32,
    /// Print one `[plan] …` line to stderr per planning operation (t149). `ACACIA_PLAN_TRACE` overrides it.
    ///
    /// The plan a *successful* walk used is otherwise invisible: `Path.plan` only ever reached the tool result
    /// when the walk was refused, so "was this D* Lite, and did it fall back" could only be answered by
    /// reading the bot's source. This switch answers it from the log.
    pub plan_trace: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Tool families (`perceive`, `act`) or individual tool names.
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatConfig {
    pub enabled: bool,
    pub prefix: String,
    pub max_chars: usize,
    pub min_interval_ms: u64,
    pub auto_reply_cooldown_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    /// Ask `y/N` before a write that touches a protected region or a keep block (interactive only).
    pub confirm: bool,
    /// Allow those writes at all in `serve` mode, where nothing can ask.
    pub allow_protected: bool,
    /// Boxes `[x0,y0,z0,x1,y1,z1]` the bot must not dig or place in.
    pub protected_regions: Vec<[i32; 6]>,
    /// Blocks that are never dug without confirmation.
    pub keep_blocks: Vec<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com/v1".to_owned(),
            model: "deepseek-chat".to_owned(),
            api_key: None,
            api_key_env: "ACACIA_LLM_API_KEY".to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
            max_tokens_source: None,
            context_window: 128_000,
            timeout_s: 120,
            temperature: 0.2,
        }
    }
}

impl LlmConfig {
    /// Where the effective budget came from, phrased for a message (t121):
    /// `ACACIA_LLM_MAX_TOKENS=256`, `config.json max_tokens=256`, or `the default 4096`.
    ///
    /// The value is always the one actually sent, so a truncation message and the request body can
    /// never disagree about the number that cut the reply.
    pub fn max_tokens_origin(&self) -> String {
        match self.max_tokens_source.as_deref() {
            Some(variable) => format!("{variable}={}", self.max_tokens),
            None if self.max_tokens == DEFAULT_MAX_TOKENS => format!("the default {DEFAULT_MAX_TOKENS}"),
            None => format!("config.json max_tokens={}", self.max_tokens),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 80,
            wall_secs: 900,
            compact_threshold: 0.8,
            compact_keep_recent: 6,
            max_total_tokens: 1_500_000,
            post_action_observe: "brief".to_owned(),
            reconnects: 1,
            plan_trace: false,
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self { allow: vec!["perceive".to_owned(), "act".to_owned()], deny: vec!["chat".to_owned()] }
    }
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self { enabled: false, prefix: "[Acacia]".to_owned(), max_chars: 200, min_interval_ms: 3_000, auto_reply_cooldown_ms: 10_000 }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            address: "127.0.0.1:19132".to_owned(),
            name: "AIBot".to_owned(),
            llm: LlmConfig::default(),
            agent: AgentConfig::default(),
            tools: ToolsConfig::default(),
            chat: ChatConfig::default(),
            safety: SafetyConfig::default(),
            runs_dir: ".acacia-agent/runs".to_owned(),
        }
    }
}

impl Config {
    /// Reads `explicit` (if any) or `./config.json` (if present) over the defaults, then applies the
    /// environment overrides. A missing optional file is not an error.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        // The one place the real process environment enters: everything below goes through `env`.
        Self::load_with(explicit, &|name| std::env::var_os(name))
    }

    /// [`Config::load`] with the environment passed in, so a test can drive the real loader (file →
    /// environment → defaults) without mutating the process environment other tests share.
    fn load_with(explicit: Option<&Path>, env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<Self> {
        let mut config = Config::default();
        let file = match explicit {
            Some(path) => Some(path.to_path_buf()),
            None => {
                let default = Path::new("config.json");
                if default.exists() {
                    Some(default.to_path_buf())
                } else {
                    None
                }
            }
        };
        if let Some(path) = file {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| AgentError::Config(format!("cannot read {}: {e}", path.display())))?;
            config = serde_json::from_str(&text)
                .map_err(|e| AgentError::Config(format!("{} is not valid JSON: {e}", path.display())))?;
        }

        if let Some(value) = env("ACACIA_LLM_BASE_URL").and_then(|value| value.into_string().ok()) {
            config.llm.base_url = value;
        }
        if let Some(value) = env("ACACIA_LLM_MODEL").and_then(|value| value.into_string().ok()) {
            config.llm.model = value;
        }
        if let Some(value) = env("ACACIA_LLM_API_KEY_ENV").and_then(|value| value.into_string().ok()) {
            config.llm.api_key_env = value;
        }
        // `var_os`, not `var`: a value that is not valid UTF-8 is still "set but unusable", and
        // silently keeping the default would look like the variable was ignored.
        let raw_max_tokens = env(MAX_TOKENS_ENV);
        let (max_tokens, warning) = max_tokens_from_env_value(raw_max_tokens.as_deref());
        config.llm.max_tokens = max_tokens;
        // A value that was set but unusable is *not* a source: what the client will send is the
        // default, and the message about a truncated reply has to name the default, not a knob the
        // operator already saw rejected on stderr.
        config.llm.max_tokens_source =
            match (&raw_max_tokens, &warning) {
                (Some(_), None) => Some(MAX_TOKENS_ENV.to_owned()),
                _ => None,
            };
        if let Some(warning) = warning {
            eprintln!("{warning}");
        }
        if let Some(value) = env("ACACIA_AGENT_RUNS_DIR").and_then(|value| value.into_string().ok()) {
            config.runs_dir = value;
        }
        // t149: the trace is off unless asked for. A value that was set but unusable falls back to the
        // default *and* says so once, because "I set the variable and got nothing" is the one failure mode
        // this switch cannot afford.
        let raw_plan_trace = env(PLAN_TRACE_ENV);
        let (plan_trace, warning) = plan_trace_from_env_value(raw_plan_trace.as_deref());
        config.agent.plan_trace = plan_trace;
        if let Some(warning) = warning {
            eprintln!("{warning}");
        }
        if let Some(value) = env("ACACIA_ADDRESS").and_then(|value| value.into_string().ok()) {
            config.address = value;
        }
        if let Some(value) = env("ACACIA_NAME").and_then(|value| value.into_string().ok()) {
            config.name = value;
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.llm.base_url.trim().is_empty() {
            return Err(AgentError::Config("llm.base_url is empty".to_owned()));
        }
        if self.llm.model.trim().is_empty() {
            return Err(AgentError::Config("llm.model is empty".to_owned()));
        }
        if !(0.0..=1.0).contains(&self.llm.temperature) {
            return Err(AgentError::Config("llm.temperature must be between 0 and 1".to_owned()));
        }
        if !(0.0..=1.0).contains(&self.agent.compact_threshold) {
            return Err(AgentError::Config("agent.compact_threshold must be between 0 and 1".to_owned()));
        }
        if self.agent.compact_keep_recent == 0 {
            return Err(AgentError::Config("agent.compact_keep_recent must be at least 1".to_owned()));
        }
        Ok(())
    }

    /// The key: the named environment variable, then `DEEPSEEK_API_KEY`, then the file value.
    pub fn api_key(&self) -> Option<String> {
        for name in [self.llm.api_key_env.as_str(), "DEEPSEEK_API_KEY"] {
            if let Ok(value) = std::env::var(name) {
                let value = value.trim().to_owned();
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
        self.llm.api_key.clone().filter(|key| !key.trim().is_empty())
    }

    /// The configuration with the key masked: this is what gets logged and traced.
    pub fn redacted(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        if let Some(llm) = value.get_mut("llm").and_then(Value::as_object_mut) {
            if llm.contains_key("api_key") {
                llm.insert("api_key".to_owned(), json!("<redacted>"));
            }
        }
        value
    }

    /// The per-run limits derived from the configuration.
    pub fn limits(&self) -> crate::agent::Limits {
        crate::agent::Limits {
            max_steps: self.agent.max_steps.clamp(1, 200),
            wall_clock: Duration::from_secs(self.agent.wall_secs.max(1)),
            max_total_tokens: self.agent.max_total_tokens,
            compact_at: (f64::from(self.agent.compact_threshold) * self.llm.context_window as f64) as u64,
            keep_recent: self.agent.compact_keep_recent,
            allow: None,
            max_reconnects: self.agent.reconnects,
            legacy_history: false,
        }
    }
}

/// Why a reply budget cannot be used, or the value itself.
///
/// The window is `1..=MAX_TOKENS_CEILING`: 0 leaves no room for a reply at all (the provider
/// either refuses or truncates to nothing), and a huge number is how a prepaid proxy turns a small
/// balance into a `403 insufficient_user_quota` (t116 saw ＄0.51–0.75 reserved per call at 4096).
pub fn parse_max_tokens(raw: &str) -> std::result::Result<u32, String> {
    let trimmed = raw.trim();
    let value: u32 = trimmed
        .parse()
        .map_err(|_| format!("{trimmed:?} is not a whole number"))?;
    if value == 0 {
        return Err("0 leaves no room for a reply (the smallest usable budget is 1)".to_owned());
    }
    if value > MAX_TOKENS_CEILING {
        return Err(format!("{value} is above the {MAX_TOKENS_CEILING} ceiling this client allows"));
    }
    Ok(value)
}

/// A switch written by hand or by a runner script: `1`/`true`/`yes`/`on` and `0`/`false`/`no`/`off`.
///
/// Case and surrounding blanks do not matter; anything else is an error naming what was accepted, because a
/// typo has to be visible (`ACACIA_PLAN_TRACE=ture` silently meaning "off" is the same silence as not setting
/// it at all).
pub fn parse_switch(raw: &str) -> std::result::Result<bool, String> {
    let trimmed = raw.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("{trimmed:?} is not a switch (use 1/true/yes/on or 0/false/no/off)")),
    }
}

/// [`PLAN_TRACE_ENV`] as a switch: the default is off, and an unusable value keeps it off with one warning.
///
/// Split from [`Config::load_with`] so the three cases (unset, usable, unusable) can be tested without
/// touching the process environment.
pub fn plan_trace_from_env_value(raw: Option<&std::ffi::OsStr>) -> (bool, Option<String>) {
    match raw {
        None => (false, None),
        Some(value) => match value.to_str().map(parse_switch) {
            Some(Ok(on)) => (on, None),
            Some(Err(why)) => (
                false,
                Some(format!("{PLAN_TRACE_ENV}: {why} — the plan trace stays off (it is off unless asked for)")),
            ),
            // Not valid UTF-8: still "set but unusable", and silently ignoring it would look like it worked.
            None => (
                false,
                Some(format!("{PLAN_TRACE_ENV}: the value is not valid UTF-8 — the plan trace stays off")),
            ),
        },
    }
}

/// The budget to use for an environment value, and the warning to print when it was unusable.
///
/// `None` means the variable was not set: that is the ordinary case and says nothing. A set value
/// that cannot be used falls back to [`DEFAULT_MAX_TOKENS`] *with* a warning — a silent fallback
/// would leave the operator believing the variable took effect, which is exactly the kind of "the
/// tool said nothing, so it must have worked" trap the agent layer keeps paying for.
fn max_tokens_from_env_value(raw: Option<&std::ffi::OsStr>) -> (u32, Option<String>) {
    let Some(raw) = raw else {
        return (DEFAULT_MAX_TOKENS, None);
    };
    match raw.to_str() {
        Some(text) => match parse_max_tokens(text) {
            Ok(value) => (value, None),
            Err(why) => (
                DEFAULT_MAX_TOKENS,
                Some(format!("{MAX_TOKENS_ENV}={text:?} ignored: {why}; using {DEFAULT_MAX_TOKENS}")),
            ),
        },
        None => (
            DEFAULT_MAX_TOKENS,
            Some(format!("{MAX_TOKENS_ENV}={raw:?} ignored: not valid UTF-8; using {DEFAULT_MAX_TOKENS}")),
        ),
    }
}

#[cfg(test)]
mod max_tokens_tests {
    use std::ffi::OsStr;

    use super::{Config, DEFAULT_MAX_TOKENS, LlmConfig, MAX_TOKENS_CEILING, MAX_TOKENS_ENV, max_tokens_from_env_value, parse_max_tokens};

    /// Nobody said anything: the default stands, and nothing is printed.
    #[test]
    fn unset_keeps_the_default_and_says_nothing() {
        assert_eq!(max_tokens_from_env_value(None), (DEFAULT_MAX_TOKENS, None));
        assert_eq!(DEFAULT_MAX_TOKENS, 4096, "the default is part of the contract: sessions were tuned against it");
    }

    /// A small budget is the whole point of the variable (a prepaid proxy reserves credit
    /// proportional to it), so a usable value must be taken as given.
    #[test]
    fn a_smaller_budget_is_taken_as_given() {
        assert_eq!(max_tokens_from_env_value(Some(OsStr::new("512"))), (512, None));
        assert_eq!(max_tokens_from_env_value(Some(OsStr::new(" 256 "))), (256, None), "surrounding space is not a typo");
        assert_eq!(max_tokens_from_env_value(Some(OsStr::new("1"))), (1, None));
        assert_eq!(max_tokens_from_env_value(Some(OsStr::new(&MAX_TOKENS_CEILING.to_string()))), (MAX_TOKENS_CEILING, None));
    }

    /// An unusable value must not take the process down: it warns, names the variable, and the bot
    /// still starts on the default budget.
    #[test]
    fn an_unusable_value_warns_and_falls_back_instead_of_panicking() {
        for raw in ["0", "abc", "8193", "40096", "-1", "512.5", ""] {
            let (tokens, warning) = max_tokens_from_env_value(Some(OsStr::new(raw)));
            assert_eq!(tokens, DEFAULT_MAX_TOKENS, "{raw:?} must fall back to the default");
            let warning = warning.unwrap_or_else(|| panic!("{raw:?} must come with a warning"));
            assert!(warning.contains(MAX_TOKENS_ENV), "the warning must name the variable it is about: {warning}");
            assert!(warning.contains(&DEFAULT_MAX_TOKENS.to_string()), "the warning must say what is used instead: {warning}");
        }
        assert!(max_tokens_from_env_value(Some(OsStr::new("8193"))).1.unwrap().contains(&MAX_TOKENS_CEILING.to_string()),
            "an over-ceiling value should say where the ceiling is");
    }

    /// The two boundaries, stated once each so a later "just raise it" edit has to argue with a test.
    #[test]
    fn the_window_is_one_to_the_ceiling() {
        assert!(parse_max_tokens("1").is_ok());
        assert!(parse_max_tokens("8192").is_ok());
        assert!(parse_max_tokens("0").is_err());
        assert!(parse_max_tokens("8193").is_err());
    }

    /// The wiring at the level that matters: the real loader, given a process environment, puts the
    /// value into `llm.max_tokens`. `Config::load` passes `std::env::var_os` as that environment, so
    /// this test covers `ACACIA_LLM_MAX_TOKENS=512 acacia-agent …` without mutating the shared process
    /// environment (which other tests read in parallel).
    #[test]
    fn the_loader_puts_the_environment_value_into_the_config() {
        let with = |pairs: &'static [(&'static str, &'static str)]| {
            let config = Config::load_with(None, &move |name| {
                pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| std::ffi::OsString::from(value))
            })
            .expect("the default config is valid");
            config.llm.max_tokens
        };
        assert_eq!(with(&[]), DEFAULT_MAX_TOKENS, "nothing set: the default stands");
        assert_eq!(with(&[(MAX_TOKENS_ENV, "512")]), 512);
        assert_eq!(with(&[(MAX_TOKENS_ENV, "abc")]), DEFAULT_MAX_TOKENS, "unusable values fall back, never panic");
        assert_eq!(with(&[(MAX_TOKENS_ENV, "8193")]), DEFAULT_MAX_TOKENS);
        assert_eq!(with(&[("ACACIA_LLM_MODEL", "deepseek-chat"), (MAX_TOKENS_ENV, "256")]), 256, "other overrides are independent");
    }

    /// Where the budget came from, which is what a truncation message quotes (t121). The three cases are
    /// distinct on purpose: `the default 4096` and `config.json max_tokens=4096` send the same number but
    /// tell the operator different things about where to change it.
    #[test]
    fn the_origin_names_the_layer_that_chose_the_budget() {
        let by_env = LlmConfig { max_tokens: 256, max_tokens_source: Some(MAX_TOKENS_ENV.to_owned()), ..LlmConfig::default() };
        assert_eq!(by_env.max_tokens_origin(), "ACACIA_LLM_MAX_TOKENS=256");

        let default = LlmConfig::default();
        assert_eq!(default.max_tokens_origin(), "the default 4096");

        let from_file = LlmConfig { max_tokens: 512, max_tokens_source: None, ..LlmConfig::default() };
        assert_eq!(from_file.max_tokens_origin(), "config.json max_tokens=512");
    }

    /// The loader has to set the source, not just the number: a message that says "max_tokens=256" without
    /// naming the variable leaves the operator grepping for a config file that never mentioned it.
    #[test]
    fn the_loader_remembers_where_the_value_came_from() {
        let load = |raw: Option<&'static str>| {
            Config::load_with(None, &move |name| {
                raw.filter(|_| name == MAX_TOKENS_ENV).map(std::ffi::OsString::from)
            })
            .expect("the default config is valid")
        };

        let from_env = load(Some("256"));
        assert_eq!(from_env.llm.max_tokens, 256);
        assert_eq!(from_env.llm.max_tokens_source.as_deref(), Some(MAX_TOKENS_ENV));
        assert_eq!(from_env.llm.max_tokens_origin(), "ACACIA_LLM_MAX_TOKENS=256");

        // Set but unusable: the client sends the default, so the origin must say "the default", not the
        // variable the operator already saw rejected on stderr.
        let unusable = load(Some("abc"));
        assert_eq!(unusable.llm.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(unusable.llm.max_tokens_source, None);
        assert_eq!(unusable.llm.max_tokens_origin(), "the default 4096");

        let unset = load(None);
        assert_eq!(unset.llm.max_tokens_source, None);
    }
}

#[cfg(test)]
mod plan_trace_tests {
    use std::ffi::OsString;

    use super::{Config, PLAN_TRACE_ENV, parse_switch, plan_trace_from_env_value};

    /// Off unless asked for: the plan line is diagnostic, and a silent default is what keeps existing
    /// output byte-for-byte unchanged (t149).
    #[test]
    fn unset_means_off_and_nothing_is_printed() {
        assert_eq!(plan_trace_from_env_value(None), (false, None));
        assert!(!Config::default().agent.plan_trace, "the default must not trace");
    }

    /// Both spellings of both answers, with the blanks and case a hand-written env line has.
    #[test]
    fn every_spelling_of_on_and_off_is_read() {
        for raw in ["1", "true", "yes", "on", "TRUE", " On "] {
            assert_eq!(parse_switch(raw), Ok(true), "{raw:?} means on");
        }
        for raw in ["0", "false", "no", "off", "FALSE", " off\t"] {
            assert_eq!(parse_switch(raw), Ok(false), "{raw:?} means off");
        }
    }

    /// A typo must not read as "off": it warns once, names the variable, and keeps the default.
    #[test]
    fn a_typo_falls_back_to_off_with_one_warning_that_names_the_variable() {
        for raw in ["ture", "2", "", "on!", "开启"] {
            let (on, warning) = plan_trace_from_env_value(Some(std::ffi::OsStr::new(raw)));
            assert!(!on, "{raw:?} must not turn the trace on");
            let warning = warning.unwrap_or_else(|| panic!("{raw:?} must come with a warning"));
            assert!(warning.contains(PLAN_TRACE_ENV), "the warning must name the variable: {warning}");
            assert!(warning.contains("stays off"), "the warning must say what happened instead: {warning}");
        }
    }

    /// The variable beats the file, in both directions, through the real loader.
    #[test]
    fn the_loader_takes_the_variable_over_the_file() {
        let load = |raw: Option<&str>| {
            let env = move |name: &str| -> Option<OsString> {
                (name == PLAN_TRACE_ENV).then(|| OsString::from(raw.unwrap_or_default()))
            };
            Config::load_with(None, &env).expect("the default config is valid")
        };

        assert!(load(Some("1")).agent.plan_trace);
        assert!(!load(Some("0")).agent.plan_trace);
        // Set but unusable: the default (off) stands, so a run that asked wrongly is still silent.
        assert!(!load(Some("maybe")).agent.plan_trace);
        assert!(!load(None).agent.plan_trace);
    }
}
