//! `acacia-agent`: an LLM-driven survival agent on top of [`acacia_bot`].
//!
//! Three layers, each usable on its own:
//! - **tools** — a registry with hand-written JSON Schemas, typed argument reads, and a failure
//!   taxonomy the model can reason about (`no_item`, `out_of_reach`, …);
//! - **perceive + shell** — one task owns the `Bot`, keeps a structured, token-bounded snapshot, and
//!   executes validated calls (the `acacia-netease` ownership contract, without the MCP bridge);
//! - **agent** — the step machine over any OpenAI-compatible provider: tool calling, budgets,
//!   compaction, memory, guardrails, and a per-run JSONL trace.
//!
//! The design is written down in `docs/agent-design.md`.

pub mod agent;
pub mod config;
pub mod error;
pub mod llm;
pub mod perceive;
pub mod rpc;
pub mod runner;
pub mod shell;
pub mod tools;

pub use agent::{Agent, CancelFlag, Limits, Report, Stop, Trace};
pub use config::Config;
pub use error::{AgentError, Result};
pub use shell::{ShellHandle, ShellState};
