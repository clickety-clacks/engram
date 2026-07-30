use std::fs;

use engram::tape::harness::opencode_json_to_tape_jsonl;
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
fn opencode_export_maps_to_success_gated_events_with_full_coverage() {
    let input = load_fixture("tests/fixtures/opencode/session_export.json");
    let output = opencode_json_to_tape_jsonl(&input).expect("adapter should parse fixture");
    let events = parse_output_events(&output);

    assert!(events.len() >= 9, "events={events:?}");

    let meta = &events[0];
    assert_eq!(meta["k"], "meta");
    assert_eq!(meta["coverage.tool"], "full");
    assert_eq!(meta["coverage.read"], "full");
    assert_eq!(meta["coverage.edit"], "full");
    assert_eq!(meta["source"]["harness"], "opencode");
    assert_eq!(meta["source"]["session_id"], "ses_open_1");

    assert!(
        events
            .iter()
            .any(|event| event["k"] == "msg.in" && event["content"] == "Please read src/lib.rs"),
        "events={events:?}"
    );

    assert!(
        events
            .iter()
            .any(|event| event["k"] == "msg.out" && event["content"] == "Running tools"),
        "events={events:?}"
    );

    assert!(
        events.iter().any(|event| event["k"] == "tool.call"
            && event["tool"] == "read"
            && event["call_id"] == "call_read_1"),
        "events={events:?}"
    );

    assert!(
        events.iter().any(|event| event["k"] == "code.read"
            && event["file"] == "src/lib.rs"
            && event["range"] == serde_json::json!([1, 1])),
        "events={events:?}"
    );

    assert!(
        events
            .iter()
            .any(|event| event["k"] == "code.edit" && event["file"] == "src/lib.rs"),
        "events={events:?}"
    );

    assert!(
        events.iter().all(|event| {
            event["k"] != "code.edit"
                || !matches!(event["file"].as_str(), Some("src/main.rs" | "src/new.rs"))
        }),
        "events={events:?}"
    );

    assert!(
        events.iter().any(|event| event["k"] == "tool.result"
            && event["tool"] == "patch"
            && event["call_id"] == "call_patch_1"
            && event["exit"] == 1),
        "events={events:?}"
    );
}
