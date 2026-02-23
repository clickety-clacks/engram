use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn gemini_json_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    let root: Value = serde_json::from_str(input)?;

    if let Some(rows) = root.as_array() {
        let first_timestamp = rows
            .first()
            .and_then(|row| row.get("timestamp"))
            .and_then(Value::as_str)
            .unwrap_or("1970-01-01T00:00:00Z");
        let session_id = rows
            .first()
            .and_then(|row| row.get("sessionId"))
            .and_then(Value::as_str);

        let mut out = vec![json!({
            "t": first_timestamp,
            "k": "meta",
            "source": source_block(session_id),
            "coverage.read": "none",
            "coverage.edit": "none",
            "coverage.tool": "none"
        })];
        for row in rows {
            let row_type = row.get("type").and_then(Value::as_str).unwrap_or("");
            let timestamp = row
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or(first_timestamp);
            let content = row
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if content.is_empty() {
                continue;
            }
            match row_type {
                "user" => out.push(json!({
                    "t": timestamp,
                    "k": "msg.in",
                    "source": source_block(session_id),
                    "role": "user",
                    "content": content
                })),
                "gemini" => out.push(json!({
                    "t": timestamp,
                    "k": "msg.out",
                    "source": source_block(session_id),
                    "role": "assistant",
                    "content": content
                })),
                _ => {}
            }
        }
        return to_jsonl(&out);
    }

    let session_id = root.get("sessionId").and_then(Value::as_str);
    let default_timestamp = root
        .get("startTime")
        .and_then(Value::as_str)
        .unwrap_or("1970-01-01T00:00:00Z");
    let mut model: Option<&str> = None;
    let mut out = Vec::new();

    let mut read_total = 0u32;
    let mut read_emitted = 0u32;
    let mut edit_total = 0u32;
    let mut edit_emitted = 0u32;

    out.push(json!({
        "t": default_timestamp,
        "k": "meta",
        "source": source_block(session_id),
        "coverage.tool": "full",
        // read_file lacks explicit span ranges; writes outside write_file may exist.
        "coverage.read": "partial",
        "coverage.edit": "partial"
    }));

    if let Some(messages) = root.get("messages").and_then(Value::as_array) {
        for message in messages {
            let message_type = message.get("type").and_then(Value::as_str).unwrap_or("");
            let timestamp = message
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or(default_timestamp);

            if model.is_none() && message_type == "gemini" {
                model = message.get("model").and_then(Value::as_str);
            }

            match message_type {
                "user" => {
                    let content = message
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !content.is_empty() {
                        out.push(json!({
                            "t": timestamp,
                            "k": "msg.in",
                            "source": source_block(session_id),
                            "role": "user",
                            "content": content
                        }));
                    }
                }
                "gemini" => {
                    let content = message
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !content.is_empty() {
                        out.push(json!({
                            "t": timestamp,
                            "k": "msg.out",
                            "source": source_block(session_id),
                            "role": "assistant",
                            "content": content
                        }));
                    }

                    let Some(tool_calls) = message.get("toolCalls").and_then(Value::as_array)
                    else {
                        continue;
                    };

                    for tool_call in tool_calls {
                        let tool_timestamp = tool_call
                            .get("timestamp")
                            .and_then(Value::as_str)
                            .unwrap_or(timestamp);
                        let tool = tool_call
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let call_id = tool_call.get("id").and_then(Value::as_str);
                        let args = tool_call.get("args").cloned().unwrap_or_else(|| json!({}));
                        let args_json =
                            serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
                        out.push(json!({
                            "t": tool_timestamp,
                            "k": "tool.call",
                            "source": source_block(session_id),
                            "tool": tool,
                            "call_id": call_id,
                            "args": args_json
                        }));

                        if tool.eq_ignore_ascii_case("read_file") {
                            read_total = read_total.saturating_add(1);
                            if let Some(file) = args.get("file_path").and_then(Value::as_str) {
                                out.push(json!({
                                    "t": tool_timestamp,
                                    "k": "code.read",
                                    "source": source_block(session_id),
                                    "file": file,
                                    "range": [1, 1],
                                    "range_basis": "line"
                                }));
                                read_emitted = read_emitted.saturating_add(1);
                            }
                        }

                        if tool.eq_ignore_ascii_case("write_file") {
                            edit_total = edit_total.saturating_add(1);
                            if let Some(file) = args.get("file_path").and_then(Value::as_str) {
                                out.push(json!({
                                    "t": tool_timestamp,
                                    "k": "code.edit",
                                    "source": source_block(session_id),
                                    "file": file,
                                    "after_hash": args.get("content").and_then(Value::as_str).map(hash_text)
                                }));
                                edit_emitted = edit_emitted.saturating_add(1);
                            }
                        }

                        let (stdout, stderr, exit) = extract_gemini_tool_result(tool_call);
                        out.push(json!({
                            "t": tool_timestamp,
                            "k": "tool.result",
                            "source": source_block(session_id),
                            "tool": tool,
                            "call_id": call_id,
                            "exit": exit,
                            "stdout": stdout,
                            "stderr": stderr
                        }));
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(meta) = out.first_mut().and_then(Value::as_object_mut) {
        meta.insert(
            "coverage.read".to_string(),
            json!(coverage_grade(read_total, read_emitted)),
        );
        meta.insert(
            "coverage.edit".to_string(),
            json!(coverage_grade(edit_total, edit_emitted)),
        );
        if let Some(model) = model {
            meta.insert("model".to_string(), json!(model));
        }
    }

    to_jsonl(&out)
}

fn extract_gemini_tool_result(tool_call: &Value) -> (String, String, i32) {
    let status = tool_call
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit = if status == "success" { 0 } else { 1 };

    if let Some(results) = tool_call.get("result").and_then(Value::as_array) {
        for result in results {
            let Some(response) = result
                .get("functionResponse")
                .and_then(|item| item.get("response"))
                .and_then(Value::as_object)
            else {
                continue;
            };
            if let Some(error) = response.get("error").and_then(Value::as_str) {
                stderr = error.to_string();
                exit = 1;
            } else if let Some(output) = response.get("output").and_then(Value::as_str) {
                stdout = output.to_string();
            } else if let Ok(serialized) = serde_json::to_string(response) {
                stdout = serialized;
            }
        }
    }

    if stdout.is_empty() && stderr.is_empty() {
        if let Some(display) = tool_call.get("resultDisplay").and_then(Value::as_str) {
            stdout = display.to_string();
        }
    }

    (stdout, stderr, exit)
}

fn source_block(session_id: Option<&str>) -> Value {
    match session_id {
        Some(session_id) => json!({
            "harness": "gemini-cli",
            "session_id": session_id
        }),
        None => json!({
            "harness": "gemini-cli"
        }),
    }
}

fn coverage_grade(total: u32, emitted: u32) -> &'static str {
    if total == 0 || emitted == total {
        "full"
    } else {
        "partial"
    }
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::gemini_json_to_tape_jsonl;

    #[test]
    fn gemini_adapter_emits_tool_pairs_and_structured_read_edit() {
        let input = include_str!("../../../tests/fixtures/gemini/session_with_tools.json");
        let out = gemini_json_to_tape_jsonl(input).expect("adapter should parse");
        let events: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON event"))
            .collect();

        let meta = events
            .iter()
            .find(|event| event.get("k").and_then(Value::as_str) == Some("meta"))
            .expect("meta event");
        assert_eq!(meta["source"]["harness"], "gemini-cli");
        assert_eq!(meta["source"]["session_id"], "session-gemini-1");
        assert_eq!(meta["coverage.tool"], "full");
        assert_eq!(meta["coverage.read"], "full");
        assert_eq!(meta["coverage.edit"], "full");
        assert_eq!(meta["model"], "gemini-2.5-flash");

        let read = events
            .iter()
            .find(|event| event.get("k").and_then(Value::as_str) == Some("code.read"))
            .expect("code.read event");
        assert_eq!(read["file"], "/repo/README.md");
        assert_eq!(read["range"], json!([1, 1]));

        let edit = events
            .iter()
            .find(|event| {
                event.get("k").and_then(Value::as_str) == Some("code.edit")
                    && event.get("file").and_then(Value::as_str) == Some("/repo/notes.txt")
            })
            .expect("code.edit event");
        assert!(edit.get("after_hash").is_some());

        let tool_result = events
            .iter()
            .find(|event| {
                event.get("k").and_then(Value::as_str) == Some("tool.result")
                    && event.get("tool").and_then(Value::as_str) == Some("run_shell_command")
            })
            .expect("run_shell_command result");
        assert_eq!(tool_result["exit"], 1);
        assert_ne!(
            tool_result["stderr"].as_str().unwrap_or_default(),
            "",
            "events={events:?}"
        );
    }

    #[test]
    fn gemini_logs_adapter_emits_message_only_tape_with_none_coverage() {
        let input = include_str!("../../../tests/fixtures/gemini/logs.json");
        let out = gemini_json_to_tape_jsonl(input).expect("adapter should parse");
        let events: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON event"))
            .collect();

        assert_eq!(events[0]["k"], "meta");
        assert_eq!(events[0]["coverage.tool"], "none");
        assert_eq!(events[0]["coverage.read"], "none");
        assert_eq!(events[0]["coverage.edit"], "none");
        assert_eq!(events[1]["k"], "msg.in");
        assert_eq!(events[2]["k"], "msg.out");
    }
}
