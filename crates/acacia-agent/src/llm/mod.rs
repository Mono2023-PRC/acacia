//! The LLM side: the chat-completions mapping, the provider trait, and the mock used by tests.
//!
//! The wire format is OpenAI's `/chat/completions` with function calling, which DeepSeek serves
//! directly (`base_url` `https://api.deepseek.com/v1`). A second provider is another [`Llm`]
//! implementation, not a branch in the loop.

pub mod mock;
pub mod openai;
pub mod prompt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// A function call the model asked for. `arguments` stays the raw JSON **string** the provider sent:
/// a truncated or malformed string becomes an `invalid_argument` tool result the model can fix,
/// instead of ending the run.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self { id: id.into(), name: name.into(), arguments: arguments.into() }
    }

    /// The arguments as JSON; empty means `{}`.
    pub fn arguments_json(&self) -> Result<Value, serde_json::Error> {
        if self.arguments.trim().is_empty() {
            return Ok(Value::Object(Default::default()));
        }
        serde_json::from_str(&self.arguments)
    }
}

/// One conversation turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self { role: Role::System, content: Some(text.into()), tool_calls: Vec::new(), tool_call_id: None }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: Some(text.into()), tool_calls: Vec::new(), tool_call_id: None }
    }

    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { role: Role::Assistant, content, tool_calls, tool_call_id: None }
    }

    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self { role: Role::Tool, content: Some(content.into()), tool_calls: Vec::new(), tool_call_id: Some(call_id.into()) }
    }

    /// The wire form of one message.
    pub fn to_wire(&self) -> Value {
        match self.role {
            Role::Assistant => {
                let mut out = json!({"role": "assistant", "content": self.content});
                if !self.tool_calls.is_empty() {
                    out["tool_calls"] = json!(self
                        .tool_calls
                        .iter()
                        .map(|call| json!({
                            "id": call.id,
                            "type": "function",
                            "function": {"name": call.name, "arguments": call.arguments},
                        }))
                        .collect::<Vec<_>>());
                }
                out
            }
            Role::Tool => json!({"role": "tool", "tool_call_id": self.tool_call_id, "content": self.content}),
            Role::System | Role::User => json!({"role": self.role.as_str(), "content": self.content}),
        }
    }
}

/// Token accounting, as the provider reports it.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// One request to the model.
#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    /// `tools` in the provider's function-calling shape.
    pub tools: Vec<Value>,
}

/// One model answer.
#[derive(Debug, Clone, Default)]
pub struct ChatReply {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: Option<String>,
}

/// Why a model call failed. Retries happen inside the client; what reaches the loop is final.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("llm transport: {0}")]
    Transport(String),
    #[error("llm http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("llm protocol: {0}")]
    Protocol(String),
    #[error("llm request timed out after {0}s")]
    Timeout(u64),
}

/// A provider. `async fn` in a trait keeps both implementations concrete (the loop is generic).
pub trait Llm {
    fn chat(&self, request: ChatRequest) -> impl std::future::Future<Output = Result<ChatReply, LlmError>>;

    /// The reply budget this provider sends and the layer that chose it (t121), phrased as
    /// `(256, "ACACIA_LLM_MAX_TOKENS=256")`.
    ///
    /// The loop needs it to explain a truncated reply: `finish_reason:"length"` says *that* the reply
    /// was cut, never *by which knob*. Leaving this `None` (the doubles, and any provider that does not
    /// know its own budget) is honest — the message then says only what it can prove.
    fn max_tokens_budget(&self) -> Option<(u32, String)> {
        None
    }
}
