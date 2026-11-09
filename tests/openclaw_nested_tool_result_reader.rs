//! OpenClaw tool-result messages can hold their output in a nested
//! `toolResult` part rather than a `text` part. The reader takes that output
//! as it takes a `text` part's.

use engram::tape::adapters::openclaw_jsonl_to_tape_jsonl;
use serde_json::{Value, json};

const SESSION: &str = "oc-nested-reader";

fn header() -> Value {
    json!({"type": "session", "id": SESSION, "timestamp": "2026-03-01T02:00:00Z"})
}

fn row(id: &str, second: u32, message: Value) -> Value {
    json!({
        "type": "message",
        "id": id,
        "timestamp": format!("2026-03-01T02:00:{second:02}Z"),
        "message": message,
    })
}

fn call(id: &str, second: u32, call_id: &str, tool: &str, arguments: Value) -> Value {
    row(
        id,
        second,
        json!({
            "role": "assistant",
            "content": [{"type": "toolCall", "id": call_id, "name": tool, "arguments": arguments}],
        }),
    )
}

fn result(id: &str, second: u32, call_id: &str, tool: &str, error: bool, content: Value) -> Value {
    row(
        id,
        second,
        json!({
            "role": "toolResult",
            "toolCallId": call_id,
            "toolName": tool,
            "isError": error,
            "content": content,
        }),
    )
}

/// Output the way OpenClaw writes it in a nested part.
fn nested(call_id: &str, output: &str) -> Value {
    json!([{"type": "toolResult", "toolCallId": call_id, "content": output, "text": output}])
}

fn text_part(output: &str) -> Value {
    json!([{"type": "text", "text": output}])
}

/// Events of the tape the reader makes from `rows`, without its meta row.
fn events(rows: &[Value]) -> Vec<Value> {
    let input: String = rows.iter().map(|row| format!("{row}\n")).collect();
    let tape = openclaw_jsonl_to_tape_jsonl(&input).expect("tape");
    tape.lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event"))
        .filter(|event| event["k"] != "meta")
        .collect()
}

fn of_kind<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|event| event["k"] == kind).collect()
}

fn only_result(events: &[Value]) -> &Value {
    let results = of_kind(events, "tool.result");
    assert_eq!(results.len(), 1, "{events:?}");
    results[0]
}

#[test]
fn nested_output_is_read() {
    let rows = [
        header(),
        row(
            "m1",
            1,
            json!({
                "role": "toolResult",
                "toolCallId": "c1",
                "toolName": "bash",
                "isError": false,
                "content": [{"type": "toolResult", "toolCallId": "c1", "content": "hello\n", "text": "hello\n"}],
            }),
        ),
    ];
    let events = events(&rows);
    let result = only_result(&events);
    assert_eq!(result["call_id"], "c1");
    assert_eq!(result["stdout"], "hello\n");
    assert_eq!(result["stderr"], "");
    assert_eq!(result["exit"], 0);
}

#[test]
fn output_parts_join_in_order_without_empty_pieces() {
    let rows = [
        header(),
        result(
            "m1",
            1,
            "c1",
            "bash",
            false,
            json!([
                {"type": "text", "text": "a"},
                {"type": "toolResult", "content": "b"},
                {"type": "image"},
                {"type": "toolResult", "content": ""},
            ]),
        ),
    ];
    assert_eq!(only_result(&events(&rows))["stdout"], "a\nb");

    // Only tool-result messages read nested parts; user and assistant text
    // joins as before.
    let rows = [
        header(),
        row(
            "m1",
            1,
            json!({"role": "user", "content": [{"type": "toolResult", "content": "u"}]}),
        ),
        row(
            "m2",
            2,
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "said"},
                {"type": "toolResult", "content": "hidden"},
            ]}),
        ),
    ];
    let events = events(&rows);
    assert!(of_kind(&events, "msg.in").is_empty(), "{events:?}");
    let replies = of_kind(&events, "msg.out");
    assert_eq!(replies.len(), 1, "{events:?}");
    assert_eq!(replies[0]["content"], "said");
}

#[test]
fn nested_output_gives_the_same_events_as_a_text_part() {
    let cases = [
        (
            "read",
            call("m1", 1, "c1", "read", json!({"path": "src/a.rs"})),
            "fn main() {}",
            false,
        ),
        (
            "shell",
            call("m1", 1, "c1", "bash", json!({"command": "cat src/b.rs"})),
            "line one\nline two\n",
            false,
        ),
        (
            "edit",
            call(
                "m1",
                1,
                "c1",
                "edit",
                json!({"path": "src/c.rs", "old_string": "a", "new_string": "b"}),
            ),
            "Successfully replaced text in src/c.rs.",
            false,
        ),
        (
            "error",
            call("m1", 1, "c1", "read", json!({"path": "src/missing.rs"})),
            "ENOENT: no such file",
            true,
        ),
    ];
    for (name, tool_call, output, error) in cases {
        let tool = tool_call["message"]["content"][0]["name"]
            .as_str()
            .expect("tool name")
            .to_string();
        let with = |content: Value| {
            events(&[
                header(),
                tool_call.clone(),
                result("m2", 2, "c1", &tool, error, content),
            ])
        };
        let from_nested = with(nested("c1", output));
        assert_eq!(from_nested, with(text_part(output)), "{name}");

        let result = only_result(&from_nested);
        let reads = of_kind(&from_nested, "code.read");
        match name {
            "read" => {
                assert_eq!(result["stdout"], output);
                assert_eq!(reads.len(), 1, "{from_nested:?}");
                assert_eq!(reads[0]["file"], "src/a.rs");
                assert_eq!(reads[0]["text"], output);
            }
            "shell" => {
                assert_eq!(result["stdout"], output);
                assert_eq!(reads.len(), 1, "{from_nested:?}");
                assert_eq!(reads[0]["file"], "src/b.rs");
            }
            "edit" => {
                assert_eq!(result["stdout"], output);
                assert_eq!(
                    of_kind(&from_nested, "code.edit").len(),
                    1,
                    "{from_nested:?}"
                );
            }
            _ => {
                assert_eq!(result["stdout"], "");
                assert_eq!(result["stderr"], output);
                assert_eq!(result["exit"], 1);
                assert!(reads.is_empty(), "{from_nested:?}");
            }
        }
    }
}

#[test]
fn unseen_nested_shapes_add_no_output() {
    for content in [
        json!([{"type": "toolResult", "content": [{"type": "text", "text": "x"}], "text": "x"}]),
        json!([{"type": "toolResult", "content": 7, "text": "x"}]),
        json!([{"type": "toolResult", "text": "x", "id": "x", "name": "x"}]),
        json!([{"type": "image", "content": "x", "text": "x"}]),
    ] {
        let rows = [
            header(),
            result("m1", 1, "c1", "bash", false, content.clone()),
        ];
        assert_eq!(only_result(&events(&rows))["stdout"], "", "{content}");
    }
}

#[test]
fn output_read_in_chunks_matches_one_pass() {
    let rows = [
        header(),
        call("m1", 1, "c1", "bash", json!({"command": "ls"})),
        result("m2", 2, "c1", "bash", false, nested("c1", "a.txt\nb.txt\n")),
    ];
    let whole = events(&rows);
    let tail = events(&rows[2..]);
    assert_eq!(only_result(&tail)["stdout"], only_result(&whole)["stdout"]);
    assert_eq!(only_result(&whole)["stdout"], "a.txt\nb.txt\n");
}

/// The shape found in real session files: the nested part repeats the call
/// id and carries the output twice. Readers before this one wrote it empty.
#[test]
fn a_recorded_nested_result_is_not_empty() {
    let rows = [
        header(),
        call(
            "m1",
            1,
            "call_9",
            "exec",
            json!({"command": "git status --short"}),
        ),
        result(
            "m2",
            2,
            "call_9",
            "exec",
            false,
            json!([{
                "type": "toolResult",
                "toolCallId": "call_9",
                "content": " M src/lib.rs\n",
                "text": " M src/lib.rs\n",
            }]),
        ),
    ];
    let events = events(&rows);
    let result = only_result(&events);
    assert_eq!(result["call_id"], "call_9");
    assert_eq!(result["tool"], "exec");
    assert_eq!(result["stdout"], " M src/lib.rs\n");
    assert_eq!(result["source"]["session_id"], SESSION);
}
