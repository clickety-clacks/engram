use std::collections::HashMap;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn claude_jsonl_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    let mut out = Vec::new();
    let mut tool_by_id: HashMap<String, String> = HashMap::new();
    let mut session_id: Option<String> = None;
    let mut first_timestamp: Option<String> = None;

    let mut read_total = 0u32;
    let mut read_emitted = 0u32;
    let mut edit_total = 0u32;
    let mut edit_emitted = 0u32;

    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line)?;
        let timestamp = row
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("1970-01-01T00:00:00Z");
        if first_timestamp.is_none() {
            first_timestamp = Some(timestamp.to_string());
        }
        if session_id.is_none() {
            session_id = claude_session_id(&row);
        }
        let row_type = row.get("type").and_then(Value::as_str).unwrap_or("");

        match row_type {
            "user" => {
                let message = row.get("message").and_then(Value::as_object);
                let role = message
                    .and_then(|obj| obj.get("role"))
                    .and_then(Value::as_str)
                    .unwrap_or("user");
                let content = message.and_then(|obj| obj.get("content"));
                if let Some(text) = content.and_then(Value::as_str) {
                    out.push(json!({
                        "t": timestamp,
                        "k": "msg.in",
                        "source": source_block("claude-code", session_id.as_deref()),
                        "role": role,
                        "content": text
                    }));
                }
                if let Some(blocks) = content.and_then(Value::as_array) {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                            continue;
                        }
                        let tool_use_id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let tool = tool_by_id
                            .get(&tool_use_id)
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_string());
                        out.push(json!({
                            "t": timestamp,
                            "k": "tool.result",
                            "source": source_block("claude-code", session_id.as_deref()),
                            "tool": tool,
                            "call_id": if tool_use_id.is_empty() { Value::Null } else { Value::String(tool_use_id) },
                            "exit": if block.get("is_error").and_then(Value::as_bool) == Some(true) { 1 } else { 0 },
                            "stdout": content_text(block.get("content").unwrap_or(&Value::Null)),
                            "stderr": ""
                        }));
                    }
                }
            }
            "assistant" => {
                let message = row.get("message").and_then(Value::as_object);
                let role = message
                    .and_then(|obj| obj.get("role"))
                    .and_then(Value::as_str)
                    .unwrap_or("assistant");
                if let Some(blocks) = message
                    .and_then(|obj| obj.get("content"))
                    .and_then(Value::as_array)
                {
                    for block in blocks {
                        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                        match block_type {
                            "text" => {
                                let text = block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                if !text.is_empty() {
                                    out.push(json!({
                                        "t": timestamp,
                                        "k": "msg.out",
                                        "source": source_block("claude-code", session_id.as_deref()),
                                        "role": role,
                                        "content": text
                                    }));
                                }
                            }
                            "tool_use" => {
                                let tool = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                let tool_input = block.get("input").cloned().unwrap_or(Value::Null);
                                let tool_use_id = block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                tool_by_id.insert(tool_use_id.clone(), tool.to_string());

                                out.push(json!({
                                    "t": timestamp,
                                    "k": "tool.call",
                                    "source": source_block("claude-code", session_id.as_deref()),
                                    "tool": tool,
                                    "call_id": if tool_use_id.is_empty() { Value::Null } else { Value::String(tool_use_id.clone()) },
                                    "args": serde_json::to_string(&tool_input).unwrap_or_else(|_| "{}".to_string())
                                }));

                                match tool {
                                    "Read" => {
                                        read_total = read_total.saturating_add(1);
                                        if let Some(file) = tool_input
                                            .get("file_path")
                                            .and_then(Value::as_str)
                                            .map(ToOwned::to_owned)
                                        {
                                            let start = tool_input
                                                .get("offset")
                                                .and_then(Value::as_u64)
                                                .map(|n| n as u32)
                                                .unwrap_or(1)
                                                .max(1);
                                            let end = tool_input
                                                .get("limit")
                                                .and_then(Value::as_u64)
                                                .map(|n| {
                                                    start.saturating_add(
                                                        (n as u32).saturating_sub(1),
                                                    )
                                                })
                                                .unwrap_or(start);
                                            out.push(json!({
                                                "t": timestamp,
                                                "k": "code.read",
                                                "source": source_block("claude-code", session_id.as_deref()),
                                                "file": file,
                                                "range": [start, end],
                                                "range_basis": "line"
                                            }));
                                            read_emitted = read_emitted.saturating_add(1);
                                        }
                                    }
                                    "Edit" => {
                                        edit_total = edit_total.saturating_add(1);
                                        if let Some(file) = tool_input
                                            .get("file_path")
                                            .and_then(Value::as_str)
                                            .map(ToOwned::to_owned)
                                        {
                                            out.push(json!({
                                                "t": timestamp,
                                                "k": "code.edit",
                                                "source": source_block("claude-code", session_id.as_deref()),
                                                "file": file,
                                                "before_hash": tool_input.get("old_string").and_then(Value::as_str).map(hash_text),
                                                "after_hash": tool_input.get("new_string").and_then(Value::as_str).map(hash_text)
                                            }));
                                            edit_emitted = edit_emitted.saturating_add(1);
                                        }
                                    }
                                    "Write" => {
                                        edit_total = edit_total.saturating_add(1);
                                        if let Some(file) = tool_input
                                            .get("file_path")
                                            .and_then(Value::as_str)
                                            .map(ToOwned::to_owned)
                                        {
                                            out.push(json!({
                                                "t": timestamp,
                                                "k": "code.edit",
                                                "source": source_block("claude-code", session_id.as_deref()),
                                                "file": file,
                                                "after_hash": tool_input.get("content").and_then(Value::as_str).map(hash_text)
                                            }));
                                            edit_emitted = edit_emitted.saturating_add(1);
                                        }
                                    }
                                    "MultiEdit" => {
                                        if let Some(file) = tool_input
                                            .get("file_path")
                                            .and_then(Value::as_str)
                                            .map(ToOwned::to_owned)
                                        {
                                            if let Some(edits) =
                                                tool_input.get("edits").and_then(Value::as_array)
                                            {
                                                edit_total =
                                                    edit_total.saturating_add(edits.len() as u32);
                                                if edits.is_empty() {
                                                    continue;
                                                }
                                                for edit in edits {
                                                    out.push(json!({
                                                        "t": timestamp,
                                                        "k": "code.edit",
                                                        "source": source_block("claude-code", session_id.as_deref()),
                                                        "file": file,
                                                        "before_hash": edit.get("old_string").and_then(Value::as_str).map(hash_text),
                                                        "after_hash": edit.get("new_string").and_then(Value::as_str).map(hash_text)
                                                    }));
                                                    edit_emitted = edit_emitted.saturating_add(1);
                                                }
                                            } else {
                                                edit_total = edit_total.saturating_add(1);
                                            }
                                        } else {
                                            edit_total = edit_total.saturating_add(1);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    out.insert(
        0,
        json!({
            "t": first_timestamp.unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string()),
            "k": "meta",
            "source": source_block("claude-code", session_id.as_deref()),
            "coverage.read": coverage_grade(read_total, read_emitted),
            "coverage.edit": coverage_grade(edit_total, edit_emitted),
            "coverage.tool": "full"
        }),
    );

    to_jsonl(&out)
}

pub fn opencode_json_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    super::adapters::opencode::opencode_json_to_tape_jsonl(input)
}

fn source_block(harness: &str, session_id: Option<&str>) -> Value {
    match session_id {
        Some(session_id) => json!({
            "harness": harness,
            "session_id": session_id
        }),
        None => json!({
            "harness": harness
        }),
    }
}

fn coverage_grade(total: u32, emitted: u32) -> &'static str {
    // total == 0 means no structured events of this type were seen; all zero
    // of them were captured, so coverage is vacuously full.
    if total == 0 || emitted == total {
        "full"
    } else {
        "partial"
    }
}

fn claude_session_id(row: &Value) -> Option<String> {
    row.get("session_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            row.get("sessionId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
}

fn to_jsonl(events: &[Value]) -> Result<String, serde_json::Error> {
    let mut out = String::new();
    for event in events {
        out.push_str(&serde_json::to_string(event)?);
        out.push('\n');
    }
    Ok(out)
}

fn hash_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            let mut chunks = Vec::new();
            for item in items {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    chunks.push(text.to_string());
                }
                if let Some(text) = item.get("input_text").and_then(Value::as_str) {
                    chunks.push(text.to_string());
                }
                if let Some(text) = item.get("output_text").and_then(Value::as_str) {
                    chunks.push(text.to_string());
                }
            }
            chunks.join("\n")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{claude_jsonl_to_tape_jsonl, opencode_json_to_tape_jsonl};

    #[test]
    fn claude_adapter_emits_read_edit_and_tool_pairs() {
        let input = include_str!("../../tests/fixtures/claude_adapter_input.jsonl");

        let out = claude_jsonl_to_tape_jsonl(input).expect("adapter should parse");
        let events: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON event"))
            .collect();

        let meta = events
            .iter()
            .find(|event| event.get("k").and_then(Value::as_str) == Some("meta"))
            .expect("meta event");
        assert_eq!(meta["coverage.tool"], "full");
        assert_eq!(meta["coverage.read"], "full");
        assert_eq!(meta["coverage.edit"], "full");
        assert_eq!(meta["source"]["harness"], "claude-code");
        assert_eq!(meta["source"]["session_id"], "session-claude-1");

        let read_call = events
            .iter()
            .find(|event| {
                event.get("k").and_then(Value::as_str) == Some("tool.call")
                    && event.get("tool").and_then(Value::as_str) == Some("Read")
            })
            .expect("read call event");
        assert_eq!(read_call["call_id"], "toolu_read_1");

        let read = events
            .iter()
            .find(|event| event.get("k").and_then(Value::as_str) == Some("code.read"))
            .expect("code.read event");
        assert_eq!(read["file"], "/repo/src/lib.rs");
        assert_eq!(read["range"], json!([10, 14]));
        assert_eq!(read["range_basis"], "line");
        assert_eq!(read["source"]["harness"], "claude-code");

        let edit = events
            .iter()
            .find(|event| {
                event.get("k").and_then(Value::as_str) == Some("code.edit")
                    && event.get("file").and_then(Value::as_str) == Some("/repo/src/lib.rs")
            })
            .expect("code.edit event");
        assert_eq!(edit["source"]["harness"], "claude-code");

        let result = events
            .iter()
            .find(|event| event.get("k").and_then(Value::as_str) == Some("tool.result"))
            .expect("tool.result event");
        assert_eq!(result["call_id"], "toolu_read_1");
        assert_eq!(result["tool"], "Read");
        assert_eq!(result["source"]["session_id"], "session-claude-1");
    }

    #[test]
    fn claude_adapter_marks_partial_when_structured_fields_missing() {
        let input = include_str!("../../tests/fixtures/claude_adapter_partial_input.jsonl");
        let out = claude_jsonl_to_tape_jsonl(input).expect("adapter should parse");
        let events: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON event"))
            .collect();
        let meta = events
            .iter()
            .find(|event| event.get("k").and_then(Value::as_str) == Some("meta"))
            .expect("meta event");
        assert_eq!(meta["coverage.tool"], "full");
        assert_eq!(meta["coverage.read"], "partial");
        assert_eq!(meta["coverage.edit"], "partial");
    }

    #[test]
    fn opencode_adapter_emits_tool_pairs_and_structured_read_edit() {
        let input = r#"{
  "info": {"id": "ses_abc", "time": {"created": 1735689600000}},
  "messages": [{
    "info": {"id": "msg_1", "role": "assistant", "time": {"created": 1735689601000}},
    "parts": [
      {"id":"part_1","type":"text","text":"working"},
      {"id":"part_2","type":"tool","callID":"call_read","tool":"read","state":{"status":"completed","input":{"filePath":"src/lib.rs","offset":0,"limit":3},"output":"ok"}},
      {"id":"part_3","type":"tool","callID":"call_edit","tool":"edit","state":{"status":"completed","input":{"filePath":"src/lib.rs","oldString":"a","newString":"b"},"output":"done"}}
    ]
  }]
}"#;
        let out = opencode_json_to_tape_jsonl(input).expect("adapter should parse");
        assert!(out.contains(r#""k":"meta""#), "out={out}");
        assert!(
            out.contains(r#""source":{"harness":"opencode","session_id":"ses_abc"}"#),
            "out={out}"
        );
        assert!(out.contains(r#""coverage.tool":"full""#), "out={out}");
        assert!(out.contains(r#""coverage.read":"partial""#), "out={out}");
        assert!(out.contains(r#""coverage.edit":"partial""#), "out={out}");
        assert!(out.contains(r#""k":"msg.out""#), "out={out}");
        assert!(out.contains(r#""k":"tool.call""#), "out={out}");
        assert!(out.contains(r#""k":"tool.result""#), "out={out}");
        assert!(out.contains(r#""k":"code.read""#), "out={out}");
        assert!(out.contains(r#""file":"src/lib.rs""#), "out={out}");
        assert!(out.contains(r#""k":"code.edit""#), "out={out}");
    }
}
