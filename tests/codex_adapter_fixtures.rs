use std::fs;

use engram::tape::harness::codex_jsonl_to_tape_jsonl;
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
fn codex_supported_paths_emit_deterministic_events_and_partial_coverage_meta() {
    let input = load_fixture("tests/fixtures/codex/supported_paths.jsonl");
    let output = codex_jsonl_to_tape_jsonl(&input).expect("adapter should parse fixture");
    let events = parse_output_events(&output);

    assert_eq!(events.len(), 7, "events={events:?}");

    let meta = &events[0];
    assert_eq!(meta["k"], "meta");
    assert_eq!(meta["coverage.tool"], "full");
    assert_eq!(meta["coverage.read"], "partial");
    assert_eq!(meta["coverage.edit"], "partial");
    assert_eq!(meta["source"]["harness"], "codex-cli");
    assert_eq!(meta["source"]["session_id"], "sess_123");

    assert_eq!(events[1]["k"], "tool.call");
    assert_eq!(events[1]["tool"], "exec_command");
    assert_eq!(events[1]["call_id"], "call_1");

    assert_eq!(events[2]["k"], "tool.result");
    assert_eq!(events[2]["tool"], "exec_command");
    assert_eq!(events[2]["call_id"], "call_1");
    assert_eq!(events[2]["exit"], 7);

    assert_eq!(events[3]["k"], "tool.call");
    assert_eq!(events[3]["tool"], "apply_patch");
    assert_eq!(events[3]["call_id"], "call_2");

    assert_eq!(events[4]["k"], "code.edit");
    assert_eq!(events[4]["file"], "src/main.rs");
    assert_eq!(events[5]["k"], "code.edit");
    assert_eq!(events[5]["file"], "src/new.rs");

    assert_eq!(events[6]["k"], "tool.result");
    assert_eq!(events[6]["tool"], "apply_patch");
    assert_eq!(events[6]["call_id"], "call_2");
    assert!(events[6].get("exit").is_none(), "events={events:?}");

    assert!(
        events
            .iter()
            .all(|event| event["source"]["harness"] == "codex-cli"),
        "events={events:?}"
    );
    assert!(
        events.iter().all(|event| event["k"] != "code.read"),
        "events={events:?}"
    );
}

#[test]
fn codex_unsupported_shell_read_and_edit_stay_unemitted() {
    let input = load_fixture("tests/fixtures/codex/unsupported_paths.jsonl");
    let output = codex_jsonl_to_tape_jsonl(&input).expect("adapter should parse fixture");
    let events = parse_output_events(&output);

    assert_eq!(events[0]["k"], "meta");
    assert_eq!(events[0]["coverage.read"], "partial");
    assert_eq!(events[0]["coverage.edit"], "partial");

    assert!(
        events.iter().all(|event| event["k"] != "code.read"),
        "events={events:?}"
    );
    assert!(
        events.iter().all(|event| event["k"] != "code.edit"),
        "events={events:?}"
    );

    let tool_calls = events
        .iter()
        .filter(|event| event["k"] == "tool.call")
        .count();
    let tool_results = events
        .iter()
        .filter(|event| event["k"] == "tool.result")
        .count();
    assert_eq!(tool_calls, 2, "events={events:?}");
    assert_eq!(tool_results, 2, "events={events:?}");
}
