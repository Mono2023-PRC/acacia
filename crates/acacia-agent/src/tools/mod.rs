//! The tool registry: what the model may call, and the one gate every call goes through.
//!
//! A name that is not in the catalogue, or that this run's whitelist denies, answers
//! `unknown_tool` — exactly the same refusal whether the tool does not exist or is merely forbidden,
//! so the model cannot map the server's permissions by probing.

pub mod args;
pub mod result;
pub mod spec;

use std::collections::HashSet;

use serde_json::Value;

pub use result::Failure;
pub use spec::{spec, Call, EquipSlot, CraftMode, ContainerOp, OfferChoice, Table, ToolSpec, SPECS};

use crate::config::ToolsConfig;

/// Where a validated call goes. The shell implements it against the bot; tests implement it with a
/// script, so the whole loop is exercised without a server.
pub trait ToolBox {
    fn call(&mut self, call: Call) -> impl std::future::Future<Output = Result<Value, Failure>>;
    /// The state message for the next step (rendered perception; delta form when possible).
    fn state(&mut self) -> impl std::future::Future<Output = Value>;
    /// Tries to bring the game connection back after a drop. `false` when it cannot (or when the
    /// toolbox has no connection at all, as in tests).
    fn reconnect(&mut self) -> impl std::future::Future<Output = bool> {
        async { false }
    }
}

/// The catalogue filtered for one process (config) and one run (`--allow`).
#[derive(Debug, Clone)]
pub struct Registry {
    allowed: HashSet<&'static str>,
    run_allow: Option<HashSet<String>>,
}

impl Registry {
    /// Applies `tools.allow` / `tools.deny`. `deny` wins over `allow`; `"all"` means every family.
    pub fn new(config: &ToolsConfig) -> Self {
        let mut allowed = HashSet::new();
        for entry in SPECS {
            let family = entry.table.as_str();
            let named = |list: &[String]| {
                list.iter().any(|item| item == "all" || item == family || item == entry.name)
            };
            if named(&config.allow) && !named(&config.deny) {
                allowed.insert(entry.name);
            }
        }
        Registry { allowed, run_allow: None }
    }

    /// Narrows the catalogue further for one run (`RunLimits.allow`).
    pub fn with_run_allow(mut self, allow: Option<Vec<String>>) -> Self {
        self.run_allow = allow.map(|names| names.into_iter().collect());
        self
    }

    pub fn allows(&self, name: &str) -> bool {
        self.allowed.contains(name)
            && self.run_allow.as_ref().is_none_or(|allow| allow.contains(name) || allow.contains("all"))
    }

    /// The specs `tools/list` returns, in catalogue order.
    pub fn specs(&self) -> Vec<&'static ToolSpec> {
        SPECS.iter().filter(|entry| self.allows(entry.name)).collect()
    }

    /// The `tools` array of a chat-completions request.
    pub fn tools(&self) -> Vec<Value> {
        self.specs()
            .into_iter()
            .map(|entry| {
                serde_json::json!({
                    "type": "function",
                    "function": {"name": entry.name, "description": entry.summary, "parameters": spec::schema(entry.name)},
                })
            })
            .collect()
    }

    /// The `tools/list` result.
    pub fn list(&self) -> Value {
        serde_json::json!({"tools": self.specs().into_iter().map(|spec| spec.json()).collect::<Vec<_>>()})
    }

    /// Validates a call: catalogue → whitelist → arguments.
    pub fn resolve(&self, name: &str, arguments: Option<&Value>) -> Result<Call, Failure> {
        let Some(entry) = spec(name) else { return Err(Failure::unknown_tool(name)) };
        if !self.allows(entry.name) {
            return Err(Failure::unknown_tool(name));
        }
        spec::parse(name, arguments)
    }
}
