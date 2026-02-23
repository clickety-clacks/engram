use std::fs;

use engram::tape::adapter::{AdapterId, CoverageGrade, run_conformance};
use engram::tape::harness::cursor_jsonl_to_tape_jsonl;
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
fn cursor_supported_paths_emit_deterministic_events_and_partial_coverage_meta() {
    let input = load_fixture("tests/fixtures/cursor/supported_paths.jsonl");
    let output = cursor_jsonl_to_tape_jsonl(&input).expect("adapter should parse fixture");
    let events = parse_output_events(&output);

    let meta = &events[0];
    assert_eq!(meta["k"], "meta");
    assert_eq!(meta["source"]["harness"], "cursor");
    assert_eq!(
        meta["source"]["session_id"],
        "c6b62c6f-7ead-4fd6-9922-e952131177ff"
    );
    assert_eq!(meta["coverage.tool"], "full");
    assert_eq!(meta["coverage.read"], "partial");
    assert_eq!(meta["coverage.edit"], "partial");

    assert!(
        events
            .iter()
            .any(|event| event["k"] == "tool.call" && event["tool"] == "readToolCall"),
        "events={events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["k"] == "tool.result" && event["tool"] == "writeToolCall"),
        "events={events:?}"
    );

    let edit = events
        .iter()
        .find(|event| event["k"] == "code.edit")
        .expect("expected code.edit from writeToolCall result");
    assert_eq!(edit["file"], "/Users/user/project/summary.txt");

    assert!(
        events.iter().all(|event| event["k"] != "code.read"),
        "events={events:?}"
    );
}

#[test]
fn cursor_conformance_reports_full_tool_and_partial_read_edit() {
    let input = load_fixture("tests/fixtures/cursor/supported_paths.jsonl");
    let report = run_conformance(AdapterId::Cursor, &input).expect("cursor conformance");

    assert!(report.issues.is_empty(), "issues={:?}", report.issues);
    assert_eq!(report.coverage.tool, CoverageGrade::Full);
    assert_eq!(report.coverage.read, CoverageGrade::Partial);
    assert_eq!(report.coverage.edit, CoverageGrade::Partial);
}
