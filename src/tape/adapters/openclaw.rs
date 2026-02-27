use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::{Value, json};

const DEFAULT_TS: &str = "1970-01-01T00:00:00Z";

pub fn openclaw_jsonl_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    let mut events = Vec::new();
    let mut first_ts = None::<String>;
    let mut session_id = None::<String>;
    let mut saw_json = false;

    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line)?;
        saw_json = true;
        extract_from_row(
            &row,
            &mut events,
            &mut first_ts,
            &mut session_id,
            input,
        );
    }

    if !saw_json {
        return to_jsonl(&with_meta(None, events));
    }

    to_jsonl(&with_meta(session_id.as_deref(), events))
}

fn with_meta(session_id: Option<&str>, events: Vec<Value>) -> Vec<Value> {
    let ts = events
        .iter()
        .find_map(|event| event.get("t").and_then(Value::as_str))
        .unwrap_or(DEFAULT_TS);

    let mut out = Vec::with_capacity(events.len() + 1);
    out.push(json!({
        "t": ts,
        "k": "meta",
        "source": source_block(session_id),
        "coverage.tool": "partial",
        "coverage.read": "none",
        "coverage.edit": "none"
    }));
    out.extend(events);
    out
}

fn extract_from_row(
    row: &Value,
    out: &mut Vec<Value>,
    first_ts: &mut Option<String>,
    session_id: &mut Option<String>,
    raw_input: &str,
) {
    let Some(obj) = row.as_object() else {
        return;
    };

    let event_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
    if event_type == "session" {
        if session_id.is_none() {
            *session_id = obj
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| {
                    obj.get("session")
                        .and_then(Value::as_object)
                        .and_then(|session| session.get("id"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                });
        }
        return;
    }

    if event_type != "message" {
        return;
    }

    let timestamp = extract_timestamp(row);
    if first_ts.is_none() {
        *first_ts = Some(timestamp.clone());
    }

    if session_id.is_none() {
        *session_id = obj
            .get("session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| obj.get("sessionId").and_then(Value::as_str).map(ToOwned::to_owned))
            .or_else(|| first_session_id_from_jsonl(raw_input));
    }

    let Some(message) = obj.get("message").and_then(Value::as_object) else {
        return;
    };
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    let content_blocks = message
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    match role {
        "user" => {
            let text = join_text_blocks(&content_blocks);
            if !text.is_empty() {
                out.push(json!({
                    "t": timestamp,
                    "k": "msg.in",
                    "source": source_block(session_id.as_deref()),
                    "role": "user",
                    "content": text,
                }));
            }
        }
        "assistant" => {
            let text = join_text_blocks(&content_blocks);
            if !text.is_empty() {
                out.push(json!({
                    "t": timestamp,
                    "k": "msg.out",
                    "source": source_block(session_id.as_deref()),
                    "role": "assistant",
                    "content": text,
                }));
            }
            for block in content_blocks {
                let Some(block_obj) = block.as_object() else {
                    continue;
                };
                if block_obj.get("type").and_then(Value::as_str) != Some("toolCall") {
                    continue;
                }
                let tool = block_obj
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let args_value = block_obj
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let args = serde_json::to_string(&args_value).unwrap_or_else(|_| "{}".to_string());

                let mut tool_event = serde_json::Map::new();
                tool_event.insert("t".to_string(), json!(timestamp));
                tool_event.insert("k".to_string(), json!("tool.call"));
                tool_event.insert("source".to_string(), source_block(session_id.as_deref()));
                tool_event.insert("tool".to_string(), json!(tool));
                tool_event.insert("args".to_string(), json!(args));
                if let Some(call_id) = block_obj.get("id").and_then(Value::as_str) {
                    tool_event.insert("call_id".to_string(), json!(call_id));
                }
                out.push(Value::Object(tool_event));

                if let Some(edit) =
                    code_edit_from_tool_call_arguments(&timestamp, session_id.as_deref(), &args_value)
                {
                    out.push(edit);
                }
            }
        }
        "toolResult" => {
            let text = join_text_blocks(&content_blocks);
            let is_error = message
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let tool = message
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("unknown");

            let mut result = serde_json::Map::new();
            result.insert("t".to_string(), json!(timestamp));
            result.insert("k".to_string(), json!("tool.result"));
            result.insert("source".to_string(), source_block(session_id.as_deref()));
            result.insert("tool".to_string(), json!(tool));
            result.insert("exit".to_string(), json!(if is_error { 1 } else { 0 }));
            result.insert(
                "stdout".to_string(),
                json!(if is_error { "" } else { text.as_str() }),
            );
            result.insert(
                "stderr".to_string(),
                json!(if is_error { text.as_str() } else { "" }),
            );
            if let Some(call_id) = message.get("toolCallId").and_then(Value::as_str) {
                result.insert("call_id".to_string(), json!(call_id));
            }
            out.push(Value::Object(result));
        }
        _ => {}
    }
}

fn join_text_blocks(content_blocks: &[Value]) -> String {
    let mut chunks = Vec::new();
    for block in content_blocks {
        let Some(block_obj) = block.as_object() else {
            continue;
        };
        if block_obj.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = block_obj.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            chunks.push(text.to_string());
        }
    }
    chunks.join("\n")
}

fn extract_timestamp(row: &Value) -> String {
    row.get("timestamp")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| row.get("t").and_then(Value::as_str).map(ToOwned::to_owned))
        .or_else(|| row.get("time").and_then(Value::as_str).map(ToOwned::to_owned))
        .or_else(|| {
            row.get("timestamp")
                .and_then(Value::as_i64)
                .and_then(timestamp_from_epoch_millis)
        })
        .unwrap_or_else(|| DEFAULT_TS.to_string())
}

fn first_session_id_from_jsonl(input: &str) -> Option<String> {
    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line).ok()?;
        if row.get("type").and_then(Value::as_str) == Some("session") {
            if let Some(id) = row.get("id").and_then(Value::as_str) {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn code_edit_from_tool_call_arguments(
    timestamp: &str,
    session_id: Option<&str>,
    args_value: &Value,
) -> Option<Value> {
    let args = args_value.as_object()?;
    let file = args
        .get("file")
        .and_then(Value::as_str)
        .or_else(|| args.get("file_path").and_then(Value::as_str))
        .or_else(|| args.get("path").and_then(Value::as_str))?;
    let before_hash = args
        .get("before_hash")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let after_hash = args
        .get("after_hash")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    if before_hash.is_none() && after_hash.is_none() {
        return None;
    }

    let mut event = serde_json::Map::new();
    event.insert("t".to_string(), json!(timestamp));
    event.insert("k".to_string(), json!("code.edit"));
    event.insert("source".to_string(), source_block(session_id));
    event.insert("file".to_string(), json!(file));
    if let Some(before_hash) = before_hash {
        event.insert("before_hash".to_string(), json!(before_hash));
    }
    if let Some(after_hash) = after_hash {
        event.insert("after_hash".to_string(), json!(after_hash));
    }
    Some(Value::Object(event))
}

fn source_block(session_id: Option<&str>) -> Value {
    match session_id {
        Some(session_id) => json!({
            "harness": "openclaw",
            "session_id": session_id
        }),
        None => json!({
            "harness": "openclaw"
        }),
    }
}

fn timestamp_from_epoch_millis(epoch_millis: i64) -> Option<String> {
    Utc.timestamp_millis_opt(epoch_millis)
        .single()
        .map(|ts| ts.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn to_jsonl(events: &[Value]) -> Result<String, serde_json::Error> {
    let mut out = String::new();
    for event in events {
        out.push_str(&serde_json::to_string(event)?);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::openclaw_jsonl_to_tape_jsonl;

    #[test]
    fn openclaw_adapter_parses_real_nested_message_format() {
        let input = include_str!("../../../tests/fixtures/openclaw/session_log.jsonl");
        let out = openclaw_jsonl_to_tape_jsonl(input).expect("adapter should parse");
        let rows = out
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("json row"))
            .collect::<Vec<_>>();

        assert_eq!(rows[0]["k"], "meta");
        assert_eq!(rows[0]["source"]["harness"], "openclaw");
        assert_eq!(rows[0]["source"]["session_id"], "oc-main-1");

        let kinds = rows
            .iter()
            .map(|row| row["k"].as_str().unwrap_or_default())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"msg.in"));
        assert!(kinds.contains(&"msg.out"));
        assert!(kinds.contains(&"tool.call"));
        assert!(kinds.contains(&"tool.result"));

        // Thinking blocks must not leak into msg.out content.
        let assistant = rows
            .iter()
            .find(|row| row["k"] == "msg.out")
            .expect("msg.out row");
        let text = assistant["content"].as_str().unwrap_or_default();
        assert!(
            !text.contains("internal reasoning"),
            "thinking block should be skipped"
        );

        let tool_call = rows
            .iter()
            .find(|row| row["k"] == "tool.call")
            .expect("tool call");
        assert_eq!(tool_call["call_id"], "call_abc");
        assert_eq!(tool_call["tool"], "Read");

        let tool_result = rows
            .iter()
            .find(|row| row["k"] == "tool.result")
            .expect("tool result");
        assert_eq!(tool_result["call_id"], "call_abc");
        assert_eq!(tool_result["tool"], "Read");
        assert_eq!(tool_result["exit"], 0);

        // Read call fixture has no before_hash/after_hash arguments, so no code.edit.
        assert!(
            rows.iter().all(|row| row["k"] != "code.edit"),
            "unexpected code.edit emission for call without hashes"
        );
    }

    #[test]
    fn openclaw_tool_call_with_hash_arguments_emits_code_edit() {
        let input = r#"{"type":"session","id":"oc-2","timestamp":"2026-02-26T00:00:00Z"}
{"type":"message","timestamp":"2026-02-26T00:00:01Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_2","name":"Apply","arguments":{"file":"src/lib.rs","after_hash":"abc"}}]}}"#;

        let out = openclaw_jsonl_to_tape_jsonl(input).expect("adapter parse");
        assert!(out.contains(r#""k":"tool.call""#));
        assert!(out.contains(r#""k":"code.edit""#));
        assert!(out.contains(r#""after_hash":"abc""#));
    }
}
