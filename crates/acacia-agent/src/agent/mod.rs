//! The agent: the step machine, its memory, compaction, guardrails and trace.

pub mod compact;
pub mod guard;
pub mod memory;
pub mod step;
pub mod trace;

pub use step::{system_prompt, Agent, CancelFlag, Limits, Report, Stop};
pub use trace::Trace;
