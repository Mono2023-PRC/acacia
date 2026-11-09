//! The OpenAI-compatible client (DeepSeek by default).
//!
//! Retries 408/429/5xx and transport errors three times with 1 s/4 s backoff, and honours
//! `Retry-After` in seconds. It does **not** decide what a cut reply means: `finish_reason` travels
//! on [`ChatReply`] so the loop can say "the reply was truncated at `max_tokens`" with the budget and
//! the knob that set it (t121), instead of the client retrying a call the model never finished.

use std::time::Duration;

use serde_json::{Value, json};

use super::{ChatReply, ChatRequest, Llm, LlmError, ToolCall, Usage};
use crate::config::LlmConfig;

pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
    max_tokens: u32,
    /// Where [`Self::max_tokens`] came from, already phrased for a message (t121).
    max_tokens_origin: String,
    temperature: f32,
    timeout: Duration,
    attempts: u32,
}

impl OpenAiClient {
    pub fn new(config: &LlmConfig, api_key: String) -> Result<Self, crate::error::AgentError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_s.max(1)))
            .build()
            .map_err(|error| crate::error::AgentError::Llm(format!("building the HTTP client failed: {error}")))?;
        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_owned(),
            model: config.model.clone(),
            api_key,
            max_tokens: config.max_tokens,
            max_tokens_origin: config.max_tokens_origin(),
            temperature: config.temperature,
            timeout: Duration::from_secs(config.timeout_s.max(1)),
            attempts: 3,
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn body(&self, request: &ChatRequest) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": request.messages.iter().map(super::Message::to_wire).collect::<Vec<_>>(),
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "stream": false,
        });
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.clone());
            body["tool_choice"] = json!("auto");
        }
        body
    }
}

impl Llm for OpenAiClient {
    fn max_tokens_budget(&self) -> Option<(u32, String)> {
        Some((self.max_tokens, self.max_tokens_origin.clone()))
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatReply, LlmError> {
        let body = self.body(&request);
        let mut last_error = LlmError::Transport("no attempt was made".to_owned());
        for attempt in 0..self.attempts {
            if attempt > 0 {
                let backoff = Duration::from_secs(if attempt == 1 { 1 } else { 4 });
                tokio::time::sleep(backoff).await;
            }
            let sent = self
                .http
                .post(self.endpoint())
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await;
            match sent {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok());
                    let text = match response.text().await {
                        Ok(text) => text,
                        Err(error) => {
                            last_error = LlmError::Transport(error.to_string());
                            continue;
                        }
                    };
                    if status.is_success() {
                        return parse_reply(&text);
                    }
                    let retryable = status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error();
                    last_error = LlmError::Http { status: status.as_u16(), body: truncate(&text, 400) };
                    if !retryable {
                        return Err(last_error);
                    }
                    if let Some(seconds) = retry_after {
                        tokio::time::sleep(Duration::from_secs(seconds.min(30))).await;
                    }
                }
                Err(error) => {
                    let timed_out = error.is_timeout();
                    last_error = if timed_out {
                        LlmError::Timeout(self.timeout.as_secs())
                    } else {
                        LlmError::Transport(error.to_string())
                    };
                }
            }
        }
        Err(last_error)
    }
}

fn parse_reply(text: &str) -> Result<ChatReply, LlmError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| LlmError::Protocol(format!("the response is not JSON: {error}; body: {}", truncate(text, 300))))?;
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| LlmError::Protocol("the response has no choices".to_owned()))?;
    let message = choice
        .get("message")
        .ok_or_else(|| LlmError::Protocol("the choice has no message".to_owned()))?;
    let content = message.get("content").and_then(Value::as_str).map(str::to_owned);
    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in calls.iter().enumerate() {
            let function = call.get("function").unwrap_or(&Value::Null);
            let name = function.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            // Some providers send `arguments` as an object; keep the string contract either way.
            let arguments = match function.get("arguments") {
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("call_{index}"));
            tool_calls.push(ToolCall::new(id, name, arguments));
        }
    }
    let usage = value
        .get("usage")
        .map(|usage| Usage {
            prompt_tokens: usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            completion_tokens: usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0),
        })
        .unwrap_or_default();
    let finish_reason = choice.get("finish_reason").and_then(Value::as_str).map(str::to_owned);
    Ok(ChatReply { content, tool_calls, usage, finish_reason })
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
