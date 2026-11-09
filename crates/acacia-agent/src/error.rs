//! Errors that end a run, and the exit code each maps to.

use std::fmt;

/// Why a run could not start or had to stop early.
///
/// A tool failure is **not** an `AgentError`: it is a tool result the model reads and reacts to
/// ([`crate::tools::Failure`]). Only failures of the machinery around the tools land here.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("configuration: {0}")]
    Config(String),
    #[error("the bot is not connected: {0}")]
    NotConnected(String),
    #[error("the bot connection ended: {0}")]
    Bot(String),
    #[error("llm request failed: {0}")]
    Llm(String),
    #[error("the run was cancelled")]
    Cancelled,
    #[error("budget exhausted: {0}")]
    Budget(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl AgentError {
    /// Process exit code: `0` finished, `2` usage/config, `3` bot or LLM unreachable, `4` cancelled,
    /// `5` budget exhausted (see `docs/agent-design.md`, "Transport").
    pub fn exit_code(&self) -> i32 {
        match self {
            AgentError::Config(_) => 2,
            AgentError::NotConnected(_) | AgentError::Bot(_) | AgentError::Llm(_) => 3,
            AgentError::Cancelled => 4,
            AgentError::Budget(_) => 5,
            AgentError::Protocol(_) | AgentError::Io(_) | AgentError::Json(_) => 1,
        }
    }
}

impl fmt::Display for ExitCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A process exit code as an error, so `main` can return it without an unwrap.
#[derive(Debug, Clone, Copy)]
pub struct ExitCode(pub i32);

impl std::error::Error for ExitCode {}

impl ExitCode {
    pub const OK: ExitCode = ExitCode(0);
    pub const USAGE: ExitCode = ExitCode(2);
}

impl From<AgentError> for ExitCode {
    fn from(error: AgentError) -> Self {
        ExitCode(error.exit_code())
    }
}

/// The result type used across the crate.
pub type Result<T, E = AgentError> = std::result::Result<T, E>;
