//! The doubles `cargo test` uses to prove the loop without a key or a server.
//!
//! - [`ScriptedLlm`] is an in-process provider: a queue of canned replies plus the requests it saw.
//! - [`MockServer`] is a ~100-line tokio HTTP server answering `/chat/completions` from a script, so
//!   the acceptance test exercises the real [`OpenAiClient`] (request shape, `tool_calls` parsing)
//!   end to end.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{ChatReply, ChatRequest, Llm, LlmError, ToolCall};

/// A provider that answers from a script.
pub struct ScriptedLlm {
    replies: Mutex<VecDeque<ChatReply>>,
    requests: Mutex<Vec<ChatRequest>>,
    budget: Option<(u32, String)>,
}

impl ScriptedLlm {
    pub fn new(replies: Vec<ChatReply>) -> Self {
        Self { replies: Mutex::new(VecDeque::from(replies)), requests: Mutex::new(Vec::new()), budget: None }
    }

    /// Claims a reply budget, so a test can prove what a truncated reply says about it (t121).
    pub fn with_budget(mut self, max_tokens: u32, origin: impl Into<String>) -> Self {
        self.budget = Some((max_tokens, origin.into()));
        self
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Llm for ScriptedLlm {
    fn max_tokens_budget(&self) -> Option<(u32, String)> {
        self.budget.clone()
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatReply, LlmError> {
        self.requests.lock().expect("requests lock").push(request);
        self.replies
            .lock()
            .expect("replies lock")
            .pop_front()
            .ok_or_else(|| LlmError::Protocol("the script has no more replies".to_owned()))
    }
}

/// A reply that asks for one tool call.
pub fn tool_reply(id: &str, name: &str, arguments: Value) -> ChatReply {
    ChatReply {
        content: None,
        tool_calls: vec![ToolCall::new(id, name, arguments.to_string())],
        usage: super::Usage { prompt_tokens: 100, completion_tokens: 20 },
        finish_reason: Some("tool_calls".to_owned()),
    }
}

/// A reply that asks for several tool calls in one step.
pub fn tools_reply(calls: &[(&str, &str, Value)]) -> ChatReply {
    ChatReply {
        content: None,
        tool_calls: calls
            .iter()
            .map(|(id, name, arguments)| ToolCall::new(*id, *name, arguments.to_string()))
            .collect(),
        usage: super::Usage { prompt_tokens: 120, completion_tokens: 30 },
        finish_reason: Some("tool_calls".to_owned()),
    }
}

/// A reply the provider cut at `max_tokens`: half a tool call and `finish_reason:"length"` (t121).
///
/// The arguments are deliberately *not* valid JSON: a truncated call must never be executed on the
/// strength of a best-effort parse, so the test passes the fragment the provider would really leave.
pub fn truncated_reply(id: &str, name: &str, partial_arguments: &str) -> ChatReply {
    ChatReply {
        content: None,
        tool_calls: vec![ToolCall::new(id, name, partial_arguments)],
        usage: super::Usage { prompt_tokens: 100, completion_tokens: 256 },
        finish_reason: Some("length".to_owned()),
    }
}

/// A final answer.
pub fn answer_reply(text: &str) -> ChatReply {
    ChatReply {
        content: Some(text.to_owned()),
        tool_calls: Vec::new(),
        usage: super::Usage { prompt_tokens: 150, completion_tokens: 12 },
        finish_reason: Some("stop".to_owned()),
    }
}

/// The wire body a real provider would send for `reply`.
pub fn wire_body(reply: &ChatReply) -> Value {
    let mut message = json!({"role": "assistant", "content": reply.content});
    if !reply.tool_calls.is_empty() {
        message["tool_calls"] = json!(reply
            .tool_calls
            .iter()
            .map(|call| json!({
                "id": call.id,
                "type": "function",
                "function": {"name": call.name, "arguments": call.arguments},
            }))
            .collect::<Vec<_>>());
    }
    json!({
        "id": "mock",
        "object": "chat.completion",
        "model": "mock",
        "choices": [{"index": 0, "message": message, "finish_reason": reply.finish_reason}],
        "usage": {"prompt_tokens": reply.usage.prompt_tokens, "completion_tokens": reply.usage.completion_tokens},
    })
}

/// An OpenAI-compatible server with a scripted sequence of replies.
pub struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    /// Binds `127.0.0.1:0` and answers the next scripted reply per request. Run inside a tokio runtime.
    pub async fn start(replies: Vec<Value>) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let queue = Arc::new(Mutex::new(VecDeque::from(replies)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handle = tokio::spawn(serve(listener, queue, requests.clone()));
        Ok(Self { base_url: format!("http://{address}/v1"), requests, handle })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(
    listener: TcpListener,
    queue: Arc<Mutex<VecDeque<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
) {
    loop {
        let Ok((stream, _peer)) = listener.accept().await else { return };
        let queue = queue.clone();
        let requests = requests.clone();
        tokio::spawn(async move {
            let _ = answer(stream, queue, requests).await;
        });
    }
}

async fn answer(
    mut stream: tokio::net::TcpStream,
    queue: Arc<Mutex<VecDeque<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 1 << 20 {
            return Ok(());
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buffer.len() < header_end + content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    if let Ok(body) = std::str::from_utf8(&buffer[header_end..]) {
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            requests.lock().expect("requests lock").push(value);
        }
    }

    let reply = queue.lock().expect("queue lock").pop_front();
    let (status, body) = match reply {
        Some(body) => ("200 OK".to_owned(), body.to_string()),
        None => (
            "500 Internal Server Error".to_owned(),
            json!({"error": {"message": "the mock script has no more replies"}}).to_string(),
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}
