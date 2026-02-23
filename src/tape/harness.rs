use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn codex_jsonl_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    let mut out = Vec::new();
    let mut call_tools: HashMap<String, String> = HashMap::new();
    let mut session_id: Option<String> = None;
    let mut first_timestamp: Option<String> = None;
    let mut emitted_meta = false;

    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line)?;
        if session_id.is_none() {
            session_id = extract_codex_session_id(&row);
        }
        let timestamp = row
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("1970-01-01T00:00:00Z");
        if first_timestamp.is_none() {
            first_timestamp = Some(timestamp.to_string());
        }
        let row_type = row.get("type").and_then(Value::as_str).unwrap_or("");

        match row_type {
            "session_meta" => {
                let payload = row.get("payload").and_then(Value::as_object);
                let model = payload
                    .and_then(|obj| obj.get("model"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        payload
                            .and_then(|obj| obj.get("model_provider"))
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned)
                    });
                let repo_head = payload
                    .and_then(|obj| obj.get("git"))
                    .and_then(|git| git.get("commit_hash"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let mut event = serde_json::Map::new();
                event.insert("t".to_string(), json!(timestamp));
                event.insert("k".to_string(), json!("meta"));
                event.insert("source".to_string(), codex_source(session_id.as_deref()));
                event.insert("model".to_string(), json!(model));
                event.insert("repo_head".to_string(), json!(repo_head));
                event.insert("coverage.tool".to_string(), json!("full"));
                event.insert("coverage.read".to_string(), json!("partial"));
                event.insert("coverage.edit".to_string(), json!("partial"));
                out.push(Value::Object(event));
                emitted_meta = true;
            }
            "response_item" => {
                let payload = row.get("payload").and_then(Value::as_object);
                let payload_type = payload
                    .and_then(|obj| obj.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match payload_type {
                    "message" => {
                        let role = payload
                            .and_then(|obj| obj.get("role"))
                            .and_then(Value::as_str)
                            .unwrap_or("assistant");
                        let content = payload
                            .and_then(|obj| obj.get("content"))
                            .map(content_text)
                            .unwrap_or_default();
                        if !content.is_empty() {
                            out.push(json!({
                                "t": timestamp,
                                "k": if role == "assistant" { "msg.out" } else { "msg.in" },
                                "source": codex_source(session_id.as_deref()),
                                "role": role,
                                "content": content
                            }));
                        }
                    }
                    "function_call" => {
                        let tool = payload
                            .and_then(|obj| obj.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let call_id = payload
                            .and_then(|obj| obj.get("call_id"))
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned);
                        let args = payload
                            .and_then(|obj| obj.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if let Some(call_id) = &call_id {
                            call_tools.insert(call_id.clone(), tool.to_string());
                        }
                        let mut call_event = serde_json::Map::new();
                        call_event.insert("t".to_string(), json!(timestamp));
                        call_event.insert("k".to_string(), json!("tool.call"));
                        call_event
                            .insert("source".to_string(), codex_source(session_id.as_deref()));
                        call_event.insert("tool".to_string(), json!(tool));
                        call_event.insert("args".to_string(), json!(args));
                        if let Some(call_id) = &call_id {
                            call_event.insert("call_id".to_string(), json!(call_id));
                        }
                        out.push(Value::Object(call_event));
                        if tool == "apply_patch" {
                            for file in extract_apply_patch_files(&args) {
                                out.push(json!({
                                    "t": timestamp,
                                    "k": "code.edit",
                                    "source": codex_source(session_id.as_deref()),
                                    "file": file
                                }));
                            }
                        }
                    }
                    "function_call_output" => {
                        let call_id = payload
                            .and_then(|obj| obj.get("call_id"))
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned);
                        let output = payload
                            .and_then(|obj| obj.get("output"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let tool = call_id
                            .as_ref()
                            .and_then(|id| call_tools.get(id))
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_string());
                        let mut result_event = serde_json::Map::new();
                        result_event.insert("t".to_string(), json!(timestamp));
                        result_event.insert("k".to_string(), json!("tool.result"));
                        result_event
                            .insert("source".to_string(), codex_source(session_id.as_deref()));
                        result_event.insert("tool".to_string(), json!(tool));
                        if let Some(call_id) = &call_id {
                            result_event.insert("call_id".to_string(), json!(call_id));
                        }
                        if let Some(exit) = extract_exit_code(&output) {
                            result_event.insert("exit".to_string(), json!(exit));
                        }
                        result_event.insert("stdout".to_string(), json!(output));
                        result_event.insert("stderr".to_string(), json!(""));
                        out.push(Value::Object(result_event));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if !emitted_meta {
        out.insert(
            0,
            json!({
                "t": first_timestamp.unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string()),
                "k": "meta",
                "source": codex_source(session_id.as_deref()),
                "coverage.tool": "full",
                "coverage.read": "partial",
                "coverage.edit": "partial"
            }),
        );
    }

    to_jsonl(&out)
}

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

fn source_block(harness: &str, session_id: Option<&str>) -> Value {
    json!({
        "harness": harness,
        "session_id": session_id
    })
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

fn extract_exit_code(output: &str) -> Option<i64> {
    const PREFIX: &str = "Process exited with code ";
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix(PREFIX)
            .and_then(|raw| raw.parse::<i64>().ok())
    })
}

fn extract_codex_session_id(row: &Value) -> Option<String> {
    row.get("session_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            row.get("payload")
                .and_then(|payload| payload.get("session_id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .or_else(|| {
            row.get("payload")
                .and_then(|payload| payload.get("session"))
                .and_then(|session| session.get("id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
}

fn codex_source(session_id: Option<&str>) -> Value {
    match session_id {
        Some(session_id) => json!({
            "harness": "codex-cli",
            "session_id": session_id
        }),
        None => json!({
            "harness": "codex-cli"
        }),
    }
}

fn extract_apply_patch_files(arguments: &str) -> Vec<String> {
    let patch_body = serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("patch")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| arguments.to_string());

    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for line in patch_body.lines() {
        let file = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "));
        if let Some(path) = file.map(str::trim) {
            if !path.is_empty() && seen.insert(path.to_string()) {
                files.push(path.to_string());
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{claude_jsonl_to_tape_jsonl, codex_jsonl_to_tape_jsonl};

    #[test]
    fn codex_adapter_emits_tool_and_apply_patch_edit() {
        let input = r#"{"timestamp":"2026-02-22T00:00:00Z","type":"session_meta","payload":{"model_provider":"openai","git":{"commit_hash":"abc123"}}}
{"timestamp":"2026-02-22T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call_1","arguments":"{\"cmd\":\"echo hi\"}"}}
{"timestamp":"2026-02-22T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"Process exited with code 7\nOutput:\nboom"}}
{"timestamp":"2026-02-22T00:00:03Z","type":"response_item","payload":{"type":"function_call","name":"apply_patch","call_id":"call_2","arguments":"*** Begin Patch\n*** Update File: src/main.rs\n*** End Patch\n"}} "#;

        let out = codex_jsonl_to_tape_jsonl(input).expect("adapter should parse");
        assert!(out.contains(r#""k":"meta""#), "out={out}");
        assert!(out.contains(r#""k":"tool.call""#), "out={out}");
        assert!(out.contains(r#""tool":"exec_command""#), "out={out}");
        assert!(out.contains(r#""k":"tool.result""#), "out={out}");
        assert!(out.contains(r#""exit":7"#), "out={out}");
        assert!(out.contains(r#""k":"code.edit""#), "out={out}");
        assert!(out.contains(r#""file":"src/main.rs""#), "out={out}");
    }

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
}
