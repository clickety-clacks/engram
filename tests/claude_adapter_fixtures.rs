use std::fs;

use engram::tape::adapter::{AdapterId, CoverageGrade, run_conformance};
use engram::tape::harness::claude_jsonl_to_tape_jsonl;
use serde_json::Value;

fn load_fixture(path: &str) -> String {
    fs::read_to_string(path).expect("fixture should load")
}

fn parse_output_events(output: &str) -> Vec<Value> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("event should parse"))
        .collect()
}

#[test]
fn claude_multiedit_fixture_emits_expanded_edits_and_full_coverage() {
    let input = load_fixture("tests/fixtures/claude_adapter_multiedit_input.jsonl");
    let output = claude_jsonl_to_tape_jsonl(&input).expect("adapter should parse fixture");
    let events = parse_output_events(&output);

    let meta = &events[0];
    assert_eq!(meta["k"], "meta");
    assert_eq!(meta["coverage.tool"], "full");
    assert_eq!(meta["coverage.read"], "full");
    assert_eq!(meta["coverage.edit"], "full");
    assert_eq!(meta["source"]["harness"], "claude-code");
    assert_eq!(meta["source"]["session_id"], "session-claude-3");

    let edit_events: Vec<&Value> = events
        .iter()
        .filter(|event| event.get("k").and_then(Value::as_str) == Some("code.edit"))
        .collect();
    assert_eq!(edit_events.len(), 2, "events={events:?}");
    assert!(
        edit_events
            .iter()
            .all(|event| event.get("before_hash").is_some() && event.get("after_hash").is_some()),
        "events={events:?}"
    );

    let result = events
        .iter()
        .find(|event| event.get("k").and_then(Value::as_str) == Some("tool.result"))
        .expect("tool.result event");
    assert_eq!(result["tool"], "MultiEdit");
    assert_eq!(result["call_id"], "toolu_multi_1");
}

#[test]
fn claude_missing_session_fixture_omits_source_session_id() {
    let input = load_fixture("tests/fixtures/claude_adapter_no_session_input.jsonl");
    let output = claude_jsonl_to_tape_jsonl(&input).expect("adapter should parse fixture");
    let events = parse_output_events(&output);

    for event in events {
        assert_eq!(event["source"]["harness"], "claude-code");
        assert!(
            event["source"].get("session_id").is_none(),
            "event should omit source.session_id when unavailable: {event:?}"
        );
    }
}

#[test]
fn claude_partial_fixture_reports_partial_edit_and_read_coverage_in_conformance() {
    let input = load_fixture("tests/fixtures/claude_adapter_partial_input.jsonl");
    let report = run_conformance(AdapterId::ClaudeCode, &input).expect("conformance should run");

    assert!(report.issues.is_empty(), "issues={:?}", report.issues);
    assert_eq!(report.coverage.tool, CoverageGrade::Full);
    assert_eq!(report.coverage.read, CoverageGrade::Partial);
    assert_eq!(report.coverage.edit, CoverageGrade::Partial);
}
