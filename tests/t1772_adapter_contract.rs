use engram::tape::adapters::{
    claude_jsonl_to_tape_jsonl, codex_jsonl_to_tape_jsonl, cursor_jsonl_to_tape_jsonl,
    gemini_json_to_tape_jsonl, openclaw_jsonl_to_tape_jsonl, opencode_json_to_tape_jsonl,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn events(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("normalized event"))
        .collect()
}

fn structured(rows: &[Value]) -> Vec<&Value> {
    rows.iter()
        .filter(|row| matches!(row["k"].as_str(), Some("code.read" | "code.edit")))
        .collect()
}

fn raw_ids<'a>(rows: &'a [Value], kind: &str) -> Vec<&'a str> {
    rows.iter()
        .filter(|row| row["k"] == kind)
        .filter_map(|row| row["call_id"].as_str())
        .collect()
}

#[test]
fn p1_raw_fixture_hashes_are_frozen() {
    let manifest: Value =
        serde_json::from_str(include_str!("fixtures/t1772/sha256-manifest.json")).unwrap();
    for entry in manifest["files"].as_array().unwrap() {
        let path = env!("CARGO_MANIFEST_DIR").to_string() + "/" + entry["path"].as_str().unwrap();
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes.len() as u64, entry["bytes"].as_u64().unwrap());
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), entry["sha256"]);
    }
}

fn assert_deterministic(convert: impl Fn(&str) -> Result<String, serde_json::Error>, raw: &str) {
    assert_eq!(convert(raw).unwrap(), convert(raw).unwrap());
}

#[test]
fn claude_edits_wait_for_paired_success_and_preserve_multiedit_order() {
    assert_deterministic(
        claude_jsonl_to_tape_jsonl,
        include_str!("fixtures/t1772/claude.jsonl"),
    );
    let rows =
        events(&claude_jsonl_to_tape_jsonl(include_str!("fixtures/t1772/claude.jsonl")).unwrap());
    let edits = structured(&rows);
    assert_eq!(rows[0]["coverage.read"], "partial");
    assert_eq!(rows[0]["coverage.edit"], "full");
    assert_eq!(edits.len(), 5);
    assert_eq!(edits[1]["before_text"], "old");
    assert_eq!(edits[2]["after_text"], "written");
    assert_eq!(edits[3]["before_text"], "one");
    assert_eq!(edits[4]["before_text"], "three");
    assert_eq!(edits[0]["file"], "src/read.rs");
    assert_eq!(edits[0]["range"], serde_json::json!([2, 3]));
    assert_eq!(edits[0]["text"], "line two\nline three\n");
    assert_eq!(
        raw_ids(&rows, "tool.call"),
        [
            "read-ok",
            "edit-ok",
            "write-ok",
            "write-fail",
            "multi-ok",
            "read-missing"
        ]
    );
    assert_eq!(
        raw_ids(&rows, "tool.result"),
        ["read-ok", "edit-ok", "write-ok", "write-fail", "multi-ok"]
    );
    for edit in edits {
        let index = rows.iter().position(|row| row == edit).unwrap();
        let last_raw = rows[..index]
            .iter()
            .rev()
            .find(|row| matches!(row["k"].as_str(), Some("tool.call" | "tool.result")))
            .unwrap();
        assert_eq!(last_raw["k"], "tool.result");
    }
}

#[test]
fn claude_shell_read_uses_recorded_cwd_and_preserves_raw_pair() {
    let raw = r#"{"timestamp":"2026-07-29T00:00:00Z","type":"assistant","sessionId":"claude-cwd","cwd":"/repo","message":{"role":"assistant","model":"claude-fable-5","content":[{"type":"tool_use","id":"bash-relative","name":"Bash","input":{"command":"cat src/a.rs"}}]}}
{"timestamp":"2026-07-29T00:00:01Z","type":"user","sessionId":"claude-cwd","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"bash-relative","content":"one\ntwo\n"}]}}"#;
    let rows = events(&claude_jsonl_to_tape_jsonl(raw).unwrap());

    assert_eq!(rows[0]["coverage.read"], "full", "{rows:?}");
    assert_eq!(rows[0]["coverage.edit"], "full", "{rows:?}");
    assert_eq!(raw_ids(&rows, "tool.call"), ["bash-relative"]);
    assert_eq!(raw_ids(&rows, "tool.result"), ["bash-relative"]);

    let read = rows.iter().find(|row| row["k"] == "code.read").unwrap();
    assert_eq!(read["file"], "/repo/src/a.rs");
    assert_eq!(read["text"], "one\ntwo\n");
    assert_eq!(read["range"], serde_json::json!([1, 2]));
    assert_eq!(read["range_basis"], "line");
}

#[test]
fn codex_shell_read_and_patch_are_success_gated_and_ordered() {
    assert_deterministic(
        codex_jsonl_to_tape_jsonl,
        include_str!("fixtures/t1772/codex.jsonl"),
    );
    let rows =
        events(&codex_jsonl_to_tape_jsonl(include_str!("fixtures/t1772/codex.jsonl")).unwrap());
    let structured = structured(&rows);
    assert_eq!(rows[0]["coverage.read"], "partial");
    assert_eq!(rows[0]["coverage.edit"], "partial");
    assert_eq!(structured.len(), 11, "{structured:?}");
    assert_eq!(structured[0]["k"], "code.read");
    assert_eq!(structured[0]["file"], "/repo/src/a file.rs");
    assert_eq!(structured[2]["file"], "/repo/pkg/src/a file.rs");
    assert_eq!(structured[2]["range"], serde_json::json!([2, 3]));
    assert_eq!(structured[2]["text"], "two\nthree\n");
    assert_eq!(structured[6]["file"], "src/add.rs");
    assert_eq!(structured[7]["file"], "src/delete.rs");
    assert_eq!(structured[8]["file"], "src/a.rs");
    assert_eq!(structured[9]["file"], "src/a.rs");
    assert_eq!(structured[10]["file"], "src/b.rs");
    assert_eq!(structured[0]["range"], serde_json::json!([1, 2]));
    assert_eq!(structured[1]["range"], serde_json::json!([2, 2]));
    assert_eq!(structured[3]["range"], serde_json::json!([1, 2]));
    assert_eq!(structured[4]["range"], serde_json::json!([1, 2]));
    assert_eq!(structured[5]["range"], serde_json::json!([3, 4]));
    assert_eq!(structured[6]["after_text"], "added\n");
    assert_eq!(structured[7]["before_text"], "deleted\n");
    assert_eq!(structured[9]["before_text"], "before\n");
    assert!(structured[9].get("after_text").is_none());
    assert!(structured[10].get("before_text").is_none());
    assert_eq!(structured[10]["after_text"], "after\n");
    assert_eq!(raw_ids(&rows, "tool.call").len(), 18);
    assert_eq!(raw_ids(&rows, "tool.result").len(), 17);
}

#[test]
fn codex_preserves_preexisting_raw_call_result_fields_and_retains_custom_results() {
    let rows =
        events(&codex_jsonl_to_tape_jsonl(include_str!("fixtures/t1772/codex.jsonl")).unwrap());
    let raw = rows
        .iter()
        .filter(|row| matches!(row["k"].as_str(), Some("tool.call" | "tool.result")))
        .collect::<Vec<_>>();

    assert!(
        raw.iter().all(|row| row["source"]
            == serde_json::json!({
                "harness": "codex-cli"
            })),
        "session_meta payload.id must not alter pre-existing raw fields: {raw:?}"
    );
    assert_eq!(raw_ids(&rows, "tool.call").len(), 18);
    assert_eq!(raw_ids(&rows, "tool.result").len(), 17);
    for retained in ["patch-ok", "patch-path-only", "patch-failed"] {
        assert!(
            rows.iter()
                .any(|row| row["k"] == "tool.result" && row["call_id"] == retained),
            "custom_tool_call_output must be retained: {retained}"
        );
    }
}

#[test]
fn codex_all_success_accounts_immediately_inserted_structured_events_as_full() {
    let raw = r#"{"timestamp":"2026-07-29T00:00:00Z","type":"session_meta","payload":{"cwd":"/repo"}}
{"timestamp":"2026-07-29T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"read","arguments":"{\"cmd\":\"cat src/a.rs\"}"}}
{"timestamp":"2026-07-29T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"read","output":"Process exited with code 0\nFinal output:\nhello\n"}}
{"timestamp":"2026-07-29T00:00:03Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"edit","input":"*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n*** End Patch\n"}}
{"timestamp":"2026-07-29T00:00:04Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"edit","output":"{\"metadata\":{\"exit_code\":0}}"}}"#;
    let rows = events(&codex_jsonl_to_tape_jsonl(raw).unwrap());
    assert_eq!(rows[0]["coverage.read"], "full", "{rows:?}");
    assert_eq!(rows[0]["coverage.edit"], "full", "{rows:?}");
    assert_eq!(structured(&rows).len(), 2, "{rows:?}");
}

#[test]
fn native_adapters_emit_only_conclusive_success_with_required_material() {
    let cases: [(
        fn(&str) -> Result<String, serde_json::Error>,
        &str,
        usize,
        &str,
        &str,
    ); 4] = [
        (
            opencode_json_to_tape_jsonl,
            include_str!("fixtures/t1772/opencode.json"),
            4,
            "full",
            "partial",
        ),
        (
            gemini_json_to_tape_jsonl,
            include_str!("fixtures/t1772/gemini.json"),
            3,
            "full",
            "partial",
        ),
        (
            cursor_jsonl_to_tape_jsonl,
            include_str!("fixtures/t1772/cursor.jsonl"),
            2,
            "full",
            "partial",
        ),
        (
            openclaw_jsonl_to_tape_jsonl,
            include_str!("fixtures/t1772/openclaw.jsonl"),
            4,
            "full",
            "partial",
        ),
    ];
    for (convert, raw, expected, read_coverage, edit_coverage) in cases {
        assert_deterministic(convert, raw);
        let rows = events(&convert(raw).unwrap());
        let structured = structured(&rows);
        assert_eq!(structured.len(), expected, "{rows:?}");
        assert_eq!(rows[0]["coverage.read"], read_coverage);
        assert_eq!(rows[0]["coverage.edit"], edit_coverage);
        let index = rows.iter().position(|row| row == structured[0]).unwrap();
        assert_eq!(rows[index - 1]["k"], "tool.result");
        for event in structured {
            assert!(event["file"].as_str().is_some());
            assert!(
                event
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
                    || (event["source"]["harness"] == "gemini-cli"
                        && event["k"] == "code.read"
                        && event["text"] == ""
                        && event["range"] == serde_json::json!([1, 1])
                        && event["range_basis"] == "line")
                    || event.get("before_text").and_then(Value::as_str).is_some()
                    || event.get("after_text").and_then(Value::as_str).is_some()
            );
        }
    }
}

#[test]
fn gemini_empty_native_read_is_conclusive_but_missing_output_and_empty_shell_are_not() {
    let fixture_rows =
        events(&gemini_json_to_tape_jsonl(include_str!("fixtures/t1772/gemini.json")).unwrap());
    assert_eq!(fixture_rows.len(), 14, "{fixture_rows:?}");
    let empty_result_index = fixture_rows
        .iter()
        .position(|row| row["k"] == "tool.result" && row["call_id"] == "read-empty")
        .unwrap();
    assert_eq!(fixture_rows[empty_result_index + 1]["k"], "code.read");
    assert_eq!(fixture_rows[empty_result_index + 1]["file"], "src/empty.rs");
    assert_eq!(
        fixture_rows[empty_result_index + 1]["range"],
        serde_json::json!([1, 1])
    );
    assert_eq!(fixture_rows[empty_result_index + 1]["text"], "");
    assert_eq!(fixture_rows[empty_result_index + 1]["range_basis"], "line");

    let empty_native = r#"{"sessionId":"empty-native","messages":[{"type":"gemini","toolCalls":[{"id":"empty-native","name":"read_file","args":{"file_path":"src/empty.rs"},"status":"success","result":[{"functionResponse":{"response":{"output":""}}}]}]}]}"#;
    let rows = events(&gemini_json_to_tape_jsonl(empty_native).unwrap());
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(rows[0]["coverage.read"], "full");
    assert_eq!(rows[1]["k"], "tool.call");
    assert_eq!(rows[2]["k"], "tool.result");
    assert_eq!(rows[3]["k"], "code.read");
    assert_eq!(rows[3]["file"], "src/empty.rs");
    assert_eq!(rows[3]["range"], serde_json::json!([1, 1]));
    assert_eq!(rows[3]["text"], "");
    assert_eq!(rows[3]["range_basis"], "line");

    let missing_output = r#"{"sessionId":"missing-output","messages":[{"type":"gemini","toolCalls":[{"id":"missing-output","name":"read_file","args":{"file_path":"src/unknown.rs"},"status":"success","result":[{"functionResponse":{"response":{}}}]}]}]}"#;
    let rows = events(&gemini_json_to_tape_jsonl(missing_output).unwrap());
    assert_eq!(rows[0]["coverage.read"], "partial", "{rows:?}");
    assert!(structured(&rows).is_empty(), "{rows:?}");

    let empty_shell = r#"{"sessionId":"empty-shell","messages":[{"type":"gemini","toolCalls":[{"id":"empty-shell","name":"run_shell_command","args":{"command":"cat src/empty.rs"},"status":"success","result":[{"functionResponse":{"response":{"output":""}}}]}]}]}"#;
    let rows = events(&gemini_json_to_tape_jsonl(empty_shell).unwrap());
    assert_eq!(rows[0]["coverage.read"], "partial", "{rows:?}");
    assert_eq!(rows[0]["coverage.edit"], "partial", "{rows:?}");
    assert!(structured(&rows).is_empty(), "{rows:?}");
}

#[test]
fn gemini_message_only_fallback_has_none_coverage() {
    let rows =
        events(&gemini_json_to_tape_jsonl(include_str!("fixtures/gemini/logs.json")).unwrap());
    assert_eq!(rows[0]["coverage.read"], "none");
    assert_eq!(rows[0]["coverage.edit"], "none");
    assert_eq!(rows[0]["coverage.tool"], "none");
    assert!(structured(&rows).is_empty());
}

#[test]
fn opencode_completed_with_error_is_explicit_failure_and_raw_only() {
    let raw = r#"{"info":{"id":"conflict"},"messages":[{"info":{"role":"assistant"},"parts":[{"type":"tool","callID":"conflict","tool":"write","state":{"status":"completed","input":{"filePath":"src/a.rs","content":"bad"},"output":"done","error":"failed"}}]}]}"#;
    let rows = events(&opencode_json_to_tape_jsonl(raw).unwrap());
    assert_eq!(raw_ids(&rows, "tool.call"), ["conflict"]);
    assert_eq!(raw_ids(&rows, "tool.result"), ["conflict"]);
    assert_eq!(
        rows.iter().find(|row| row["k"] == "tool.result").unwrap()["exit"],
        1
    );
    assert!(structured(&rows).is_empty(), "{rows:?}");
    assert_eq!(rows[0]["coverage.edit"], "full");
}

#[test]
fn gemini_success_with_response_error_is_explicit_failure_and_raw_only() {
    let raw = r#"{"sessionId":"conflict","messages":[{"type":"gemini","toolCalls":[{"id":"conflict","name":"write_file","args":{"file_path":"src/a.rs","content":"bad"},"status":"success","result":[{"functionResponse":{"response":{"error":{"code":"EFAIL","message":"failed"}}}}]}]}]}"#;
    let rows = events(&gemini_json_to_tape_jsonl(raw).unwrap());
    assert_eq!(raw_ids(&rows, "tool.call"), ["conflict"]);
    assert_eq!(raw_ids(&rows, "tool.result"), ["conflict"]);
    let result = rows.iter().find(|row| row["k"] == "tool.result").unwrap();
    assert_eq!(result["exit"], 1);
    assert_eq!(result["stderr"], r#"{"code":"EFAIL","message":"failed"}"#);
    assert!(structured(&rows).is_empty(), "{rows:?}");
    assert_eq!(rows[0]["coverage.edit"], "full");
}

#[test]
fn cursor_success_with_error_is_explicit_failure_and_raw_only() {
    let raw = r#"{"type":"tool_call","subtype":"started","call_id":"conflict","tool_call":{"writeToolCall":{"args":{"path":"src/a.rs","content":"bad"}}}}
{"type":"tool_call","subtype":"completed","call_id":"conflict","tool_call":{"writeToolCall":{"result":{"success":{"path":"src/a.rs"},"error":{"message":"failed"}}}}}"#;
    let rows = events(&cursor_jsonl_to_tape_jsonl(raw).unwrap());
    assert_eq!(raw_ids(&rows, "tool.call"), ["conflict"]);
    assert_eq!(raw_ids(&rows, "tool.result"), ["conflict"]);
    assert_eq!(
        rows.iter().find(|row| row["k"] == "tool.result").unwrap()["exit"],
        1
    );
    assert!(structured(&rows).is_empty(), "{rows:?}");
    assert_eq!(rows[0]["coverage.edit"], "full");
}

#[test]
fn cursor_function_shell_errors_are_explicit_failures_and_raw_only() {
    for completion in [
        r#"{"error":{"message":"failed"}}"#,
        r#"{"success":{"content":"must not become evidence\n"},"error":{"message":"failed"}}"#,
    ] {
        let raw = r#"{"type":"tool_call","subtype":"started","call_id":"conflict","tool_call":{"function":{"name":"shell","arguments":{"command":"cat /repo/a.rs"}}}}
{"type":"tool_call","subtype":"completed","call_id":"conflict","tool_call":{"function":{"name":"shell","result":RESULT}}}"#
            .replace("RESULT", completion);
        let rows = events(&cursor_jsonl_to_tape_jsonl(&raw).unwrap());
        assert_eq!(raw_ids(&rows, "tool.call"), ["conflict"]);
        assert_eq!(raw_ids(&rows, "tool.result"), ["conflict"]);
        assert_eq!(
            rows.iter().find(|row| row["k"] == "tool.result").unwrap()["exit"],
            1
        );
        assert!(structured(&rows).is_empty(), "{rows:?}");
        assert_eq!(rows[0]["coverage.read"], "full");
        assert_eq!(rows[0]["coverage.edit"], "full");
    }
}

#[test]
fn cursor_null_errors_do_not_override_native_or_function_success() {
    let raw = r#"{"type":"tool_call","subtype":"started","call_id":"native","tool_call":{"writeToolCall":{"args":{"path":"src/a.rs","content":"ok"}}}}
{"type":"tool_call","subtype":"completed","call_id":"native","tool_call":{"writeToolCall":{"result":{"success":{"path":"src/a.rs"},"error":null}}}}
{"type":"tool_call","subtype":"started","call_id":"function","tool_call":{"function":{"name":"shell","arguments":{"command":"cat /repo/a.rs"}}}}
{"type":"tool_call","subtype":"completed","call_id":"function","tool_call":{"function":{"name":"shell","result":{"success":{"content":"one\ntwo\n"},"error":null}}}}"#;
    let rows = events(&cursor_jsonl_to_tape_jsonl(raw).unwrap());
    assert_eq!(structured(&rows).len(), 2, "{rows:?}");
    assert_eq!(raw_ids(&rows, "tool.call"), ["native", "function"]);
    assert_eq!(raw_ids(&rows, "tool.result"), ["native", "function"]);
    assert!(
        rows.iter()
            .filter(|row| row["k"] == "tool.result")
            .all(|row| row["exit"] == 0),
        "{rows:?}"
    );
    let read = rows.iter().find(|row| row["k"] == "code.read").unwrap();
    assert_eq!(read["text"], "one\ntwo\n");
    assert_eq!(read["range"], serde_json::json!([1, 2]));
    let function_result = rows
        .iter()
        .find(|row| row["k"] == "tool.result" && row["call_id"] == "function")
        .unwrap();
    assert_eq!(function_result["stdout"], r#"{"content":"one\ntwo\n"}"#);
    assert_eq!(rows[0]["coverage.read"], "full");
    assert_eq!(rows[0]["coverage.edit"], "full");
}

#[test]
fn null_errors_do_not_override_opencode_or_gemini_success() {
    let opencode = r#"{"info":{"id":"null"},"messages":[{"info":{"role":"assistant"},"parts":[{"type":"tool","callID":"null","tool":"write","state":{"status":"completed","input":{"filePath":"src/a.rs","content":"ok"},"output":"done","error":null}}]}]}"#;
    let opencode_rows = events(&opencode_json_to_tape_jsonl(opencode).unwrap());
    assert_eq!(structured(&opencode_rows).len(), 1, "{opencode_rows:?}");
    assert_eq!(
        opencode_rows
            .iter()
            .find(|row| row["k"] == "tool.result")
            .unwrap()["exit"],
        0
    );

    let gemini = r#"{"sessionId":"null","messages":[{"type":"gemini","toolCalls":[{"id":"null","name":"write_file","args":{"file_path":"src/a.rs","content":"ok"},"status":"success","result":[{"functionResponse":{"response":{"error":null,"output":"done"}}}]}]}]}"#;
    let gemini_rows = events(&gemini_json_to_tape_jsonl(gemini).unwrap());
    assert_eq!(structured(&gemini_rows).len(), 1, "{gemini_rows:?}");
    assert_eq!(
        gemini_rows
            .iter()
            .find(|row| row["k"] == "tool.result")
            .unwrap()["exit"],
        0
    );
}

#[test]
fn openclaw_successful_empty_read_is_raw_only_and_partial() {
    let raw = r#"{"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":"empty","name":"read","arguments":{"path":"src/empty.rs"}}]}}
{"type":"message","message":{"role":"toolResult","toolCallId":"empty","toolName":"read","content":[],"isError":false}}"#;
    let rows = events(&openclaw_jsonl_to_tape_jsonl(raw).unwrap());
    assert_eq!(raw_ids(&rows, "tool.call"), ["empty"]);
    assert_eq!(raw_ids(&rows, "tool.result"), ["empty"]);
    assert_eq!(
        rows.iter().find(|row| row["k"] == "tool.result").unwrap()["exit"],
        0
    );
    assert!(structured(&rows).is_empty(), "{rows:?}");
    assert_eq!(rows[0]["coverage.read"], "partial");
}
