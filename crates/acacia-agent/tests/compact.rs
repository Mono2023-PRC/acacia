//! Compaction and token-estimation tests: the cut must never split an assistant tool call from the
//! tool messages that answer it (a provider rejects such a history outright).

use acacia_agent::agent::compact::{estimate_messages, estimate_tokens, safe_cut, transcript};
use acacia_agent::llm::{Message, Role, ToolCall};

fn conversation() -> Vec<Message> {
    vec![
        Message::system("system prompt"),
        Message::user("collect 8 oak logs"),
        Message::assistant(None, vec![ToolCall::new("c1", "observe", "{\"radius\":8}")]),
        Message::tool("c1", "{\"ok\":true,\"data\":{}}"),
        Message::user("state 1"),
        Message::assistant(None, vec![ToolCall::new("c2", "find", "{\"block\":\"minecraft:oak_log\"}")]),
        Message::tool("c2", "{\"ok\":true,\"data\":{}}"),
        Message::user("state 2"),
        Message::assistant(Some("done".to_owned()), Vec::new()),
    ]
}

#[test]
fn cjk_counts_one_token_and_ascii_a_quarter() {
    assert_eq!(estimate_tokens("abcd"), 1);
    assert_eq!(estimate_tokens("你好"), 2);
    assert_eq!(estimate_tokens(""), 0);
}

#[test]
fn the_estimate_grows_with_the_conversation() {
    let messages = conversation();
    let full = estimate_messages(&messages);
    let head = estimate_messages(&messages[..2]);
    assert!(full > head);
}

#[test]
fn a_cut_never_separates_a_call_from_its_results() {
    let messages = conversation();
    for keep in 1..messages.len() {
        let cut = safe_cut(&messages, keep);
        assert!(cut >= 1);
        assert!(cut <= messages.len());
        if cut == messages.len() {
            continue;
        }
        // Cutting right after an assistant message with an unanswered call would break the history.
        if let Some(last) = messages.get(cut - 1) {
            let pending: Vec<&str> = last.tool_calls.iter().map(|call| call.id.as_str()).collect();
            for id in pending {
                assert!(
                    messages[cut..].iter().all(|message| message.tool_call_id.as_deref() != Some(id)),
                    "cut at {cut} (keep {keep}) separated call {id} from its result"
                );
            }
        }
        // A tool result whose call was cut away would be orphaned.
        if messages[cut].role == Role::Tool {
            panic!("cut at {cut} (keep {keep}) orphaned a tool result");
        }
    }
}

#[test]
fn the_system_prompt_always_survives() {
    for keep in 1..conversation().len() {
        let messages = conversation();
        let cut = safe_cut(&messages, keep);
        assert_eq!(messages[0].role, Role::System);
        assert!(cut >= 1, "the system prompt is at index 0 and is never cut");
    }
}

#[test]
fn the_transcript_keeps_tool_requests_and_truncates_long_results() {
    let messages = vec![
        Message::system("system"),
        Message::user("goal"),
        Message::assistant(None, vec![ToolCall::new("c1", "observe", "{\"radius\":8}")]),
        Message::tool("c1", &"x".repeat(5000)),
    ];
    let text = transcript(&messages, messages.len());
    assert!(text.contains("tool request: observe"));
    assert!(text.contains("tool result: "));
    assert!(text.len() < 2000, "long results are truncated, got {} chars", text.len());
}
