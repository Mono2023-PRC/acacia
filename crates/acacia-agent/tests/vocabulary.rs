//! The failure vocabulary is written in three places — `tools/result.rs::REASONS`, the `HINTS` table that
//! travels with every failure, and the table in `docs/agent-design.md` — and they have drifted twice
//! (`no_such_entity` and `refused` were emitted for a while without being listed; `trade_failed` and
//! `no_entity` were listed twice in the document). This file pins all three in both directions, and its
//! failures name the file that is out of step rather than saying "assertion failed".

use acacia_agent::agent::{Report, Stop};
use acacia_agent::llm::Usage;
use acacia_agent::tools::Failure;
use acacia_agent::tools::result::{HINTS, REASONS, hint_for};

const RESULT_RS: &str = "crates/acacia-agent/src/tools/result.rs";
const DESIGN_MD: &str = "docs/agent-design.md";
const DESIGN: &str = include_str!("../../../docs/agent-design.md");

/// The rows of the `| reason | … |` table, as the doc writes them: `` | `name` | … ``.
fn documented_reasons() -> Vec<String> {
    let mut out = Vec::new();
    let mut in_table = false;
    for line in DESIGN.lines() {
        if line.starts_with("| reason |") {
            in_table = true;
            continue;
        }
        if in_table {
            if line.starts_with("|---") || line.starts_with("| ---") {
                continue; // the header's separator row
            }
            if !line.starts_with("| `") {
                break;
            }
            let rest = line.trim_start_matches("| `");
            let (name, _) = rest.split_once('`').expect("a backticked reason in the first column");
            out.push(name.to_owned());
        }
    }
    out
}

#[test]
fn the_document_reason_table_was_found_at_all() {
    // Guards the parser above: a renamed column would silently make every other check pass on an empty list.
    let rows = documented_reasons();
    assert!(
        rows.len() >= 20,
        "parsed only {} rows out of {DESIGN_MD}: is the `| reason | …` table still there?",
        rows.len()
    );
    assert_eq!(rows.first().map(String::as_str), Some("no_block"), "the table must open with `no_block`");
}

#[test]
fn every_reason_has_a_hint() {
    let missing: Vec<String> = REASONS
        .iter()
        .filter(|reason| hint_for(reason).is_none())
        .map(|reason| {
            format!("  `{reason}`: no entry in HINTS ({RESULT_RS}) — the model gets the reason with no way out")
        })
        .collect();
    assert!(missing.is_empty(), "{} reason(s) without a hint:\n{}", missing.len(), missing.join("\n"));
}

#[test]
fn no_hint_is_empty() {
    let empty: Vec<String> = HINTS
        .iter()
        .filter(|(_, hint)| hint.trim().is_empty())
        .map(|(reason, _)| format!("  `{reason}`: the hint in HINTS ({RESULT_RS}) is blank"))
        .collect();
    assert!(empty.is_empty(), "{} blank hint(s):\n{}", empty.len(), empty.join("\n"));
}

#[test]
fn no_hint_is_an_orphan() {
    // The reverse direction: a hint for a reason nobody can emit is dead text, and it hides the day the
    // reason gets renamed. `HINTS` is a table (not a `match`) precisely so this can be checked.
    let orphans: Vec<String> = HINTS
        .iter()
        .filter(|(reason, _)| !REASONS.contains(reason))
        .map(|(reason, _)| format!("  `{reason}`: has a hint in HINTS but is missing from REASONS ({RESULT_RS})"))
        .collect();
    assert!(orphans.is_empty(), "{} orphan hint(s):\n{}", orphans.len(), orphans.join("\n"));
}

#[test]
fn the_hint_table_and_the_reason_list_have_the_same_size() {
    assert_eq!(
        HINTS.len(),
        REASONS.len(),
        "HINTS has {} entries and REASONS has {} ({}): every reason needs exactly one hint",
        HINTS.len(),
        REASONS.len(),
        RESULT_RS
    );
}

#[test]
fn every_reason_has_a_row_in_the_design_document() {
    let documented = documented_reasons();
    let missing: Vec<String> = REASONS
        .iter()
        .filter(|reason| !documented.iter().any(|row| row == *reason))
        .map(|reason| format!("  `{reason}`: no row in the reason table of {DESIGN_MD}"))
        .collect();
    assert!(missing.is_empty(), "{} reason(s) undocumented:\n{}", missing.len(), missing.join("\n"));
}

#[test]
fn the_design_document_lists_each_reason_once_and_nothing_else() {
    let documented = documented_reasons();
    let mut problems: Vec<String> = Vec::new();
    for reason in REASONS {
        let count = documented.iter().filter(|row| *row == reason).count();
        if count != 1 {
            problems.push(format!("  `{reason}`: appears {count} time(s) in {DESIGN_MD} (want 1)"));
        }
    }
    for row in &documented {
        if !REASONS.contains(&row.as_str()) {
            problems.push(format!("  `{row}`: a row in {DESIGN_MD} that REASONS ({RESULT_RS}) does not know"));
        }
    }
    assert!(problems.is_empty(), "the reason table drifted:\n{}", problems.join("\n"));
}

#[test]
fn a_failure_never_reaches_the_model_without_a_message() {
    // The t69 shape: `{"ok":false,"reason":"no_block","message":""}` eleven times in a row. The model
    // could see that the pickup failed and nothing about why, so it retried blind.
    let built = Failure::new("no_block", "   ").to_json();
    let message = built["message"].as_str().expect("a message field");
    assert!(!message.trim().is_empty(), "an empty message survived Failure::new: {built}");
    assert!(message.contains("no_block"), "the stand-in message must name the reason: {built}");
    assert!(built["hint"].is_string(), "a normalised failure still carries its hint: {built}");

    // A struct literal bypasses `new`, so `to_json` guards it too.
    let literal = Failure { reason: "timeout", message: String::new(), hint: None, details: None }.to_json();
    let message = literal["message"].as_str().expect("a message field");
    assert!(!message.trim().is_empty(), "an empty message survived to_json: {literal}");
    assert!(message.contains("timeout"), "the stand-in message must name the reason: {literal}");
}

#[test]
fn the_cost_line_names_every_field_it_promised() {
    // t69's "55 calls, 480k tokens, how long did it take?" was answered by counting the trace by hand.
    let report = Report {
        answer: "done".to_owned(),
        steps: 7,
        tool_calls: 12,
        compactions: 2,
        usage: Usage { prompt_tokens: 300_000, completion_tokens: 12_345 },
        stop: Stop::Answer,
        trace: std::path::PathBuf::from("target/test-runs/x/trace.jsonl"),
        elapsed: std::time::Duration::from_millis(12_345),
    };
    let line = report.summary_line(80);
    for field in ["steps=7/80", "tool_calls=12", "tokens=312345", "compactions=2", "elapsed=12.3s", "prompt 300000"] {
        assert!(line.contains(field), "the cost line is missing {field:?}: {line}");
    }
}
