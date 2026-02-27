use std::collections::HashMap;

use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::anchor::fingerprint_text;

const DEFAULT_TS: &str = "1970-01-01T00:00:00Z";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    Read,
    Edit,
    Other,
}

#[derive(Debug, Clone)]
struct ToolCallContext {
    kind: ToolKind,
    file: Option<String>,
    range: [u32; 2],
    before_hash: Option<String>,
    after_hash: Option<String>,
}

pub fn openclaw_jsonl_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    let mut events = Vec::new();
    let mut first_ts = None::<String>;
    let mut session_id = None::<String>;
    let mut saw_json = false;
    let mut tool_contexts = HashMap::<String, ToolCallContext>::new();

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
            &mut tool_contexts,
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
        "coverage.read": "partial",
        "coverage.edit": "partial"
    }));
    out.extend(events);
    out
}

fn extract_from_row(
    row: &Value,
    out: &mut Vec<Value>,
    first_ts: &mut Option<String>,
    session_id: &mut Option<String>,
    tool_contexts: &mut HashMap<String, ToolCallContext>,
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
                    obj.get("session_id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .or_else(|| obj.get("sessionId").and_then(Value::as_str).map(ToOwned::to_owned));
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
            .or_else(|| obj.get("sessionId").and_then(Value::as_str).map(ToOwned::to_owned));
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
        "user" => emit_user_message(out, &timestamp, session_id.as_deref(), &content_blocks),
        "assistant" => emit_assistant_message(
            out,
            &timestamp,
            session_id.as_deref(),
            &content_blocks,
            tool_contexts,
        ),
        "toolResult" => emit_tool_result_message(
            out,
            &timestamp,
            session_id.as_deref(),
            message,
            &content_blocks,
            tool_contexts,
        ),
        _ => {}
    }
}

fn emit_user_message(
    out: &mut Vec<Value>,
    timestamp: &str,
    session_id: Option<&str>,
    content_blocks: &[Value],
) {
    let text = join_text_blocks(content_blocks);
    if text.is_empty() {
        return;
    }
    out.push(json!({
        "t": timestamp,
        "k": "msg.in",
        "source": source_block(session_id),
        "role": "user",
        "content": text,
    }));
}

fn emit_assistant_message(
    out: &mut Vec<Value>,
    timestamp: &str,
    session_id: Option<&str>,
    content_blocks: &[Value],
    tool_contexts: &mut HashMap<String, ToolCallContext>,
) {
    let text = join_text_blocks(content_blocks);
    if !text.is_empty() {
        out.push(json!({
            "t": timestamp,
            "k": "msg.out",
            "source": source_block(session_id),
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

        let call_id = block_obj
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let tool_name = block_obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let args_value = block_obj
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let args = serde_json::to_string(&args_value).unwrap_or_else(|_| "{}".to_string());

        let mut tool_event = serde_json::Map::new();
        tool_event.insert("t".to_string(), json!(timestamp));
        tool_event.insert("k".to_string(), json!("tool.call"));
        tool_event.insert("source".to_string(), source_block(session_id));
        tool_event.insert("tool".to_string(), json!(tool_name));
        tool_event.insert("args".to_string(), json!(args));
        if let Some(call_id) = &call_id {
            tool_event.insert("call_id".to_string(), json!(call_id));
        }
        out.push(Value::Object(tool_event));

        if let Some(call_id) = call_id {
            tool_contexts.insert(call_id, tool_context_for(&tool_name, &args_value));
        }
    }
}

fn emit_tool_result_message(
    out: &mut Vec<Value>,
    timestamp: &str,
    session_id: Option<&str>,
    message: &serde_json::Map<String, Value>,
    content_blocks: &[Value],
    tool_contexts: &mut HashMap<String, ToolCallContext>,
) {
    let text = join_text_blocks(content_blocks);
    let tool_name = message
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let call_id = message
        .get("toolCallId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let is_error = message
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut result = serde_json::Map::new();
    result.insert("t".to_string(), json!(timestamp));
    result.insert("k".to_string(), json!("tool.result"));
    result.insert("source".to_string(), source_block(session_id));
    result.insert("tool".to_string(), json!(tool_name));
    result.insert("exit".to_string(), json!(if is_error { 1 } else { 0 }));
    result.insert(
        "stdout".to_string(),
        json!(if is_error { "" } else { text.as_str() }),
    );
    result.insert(
        "stderr".to_string(),
        json!(if is_error { text.as_str() } else { "" }),
    );
    if let Some(call_id) = &call_id {
        result.insert("call_id".to_string(), json!(call_id));
    }
    out.push(Value::Object(result));

    let context = call_id
        .as_ref()
        .and_then(|id| tool_contexts.remove(id))
        .unwrap_or_else(|| tool_context_for(tool_name, &json!({})));

    match context.kind {
        ToolKind::Read => {
            if let Some(file) = context.file {
                let mut event = serde_json::Map::new();
                event.insert("t".to_string(), json!(timestamp));
                event.insert("k".to_string(), json!("code.read"));
                event.insert("source".to_string(), source_block(session_id));
                event.insert("file".to_string(), json!(file));
                event.insert("range".to_string(), json!(context.range));
                if !text.is_empty() {
                    event.insert(
                        "anchor_hashes".to_string(),
                        json!([fingerprint_text(&text).fingerprint]),
                    );
                } else {
                    event.insert("anchor_hashes".to_string(), json!([]));
                }
                event.insert("range_basis".to_string(), json!("line"));
                out.push(Value::Object(event));
            }
        }
        ToolKind::Edit => {
            if let Some(file) = context.file {
                let before_hash = context.before_hash;
                let after_hash = context.after_hash.or_else(|| {
                    if text.is_empty() {
                        None
                    } else {
                        Some(hash_text(&text))
                    }
                });
                if before_hash.is_some() || after_hash.is_some() {
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
                    out.push(Value::Object(event));
                }
            }
        }
        ToolKind::Other => {}
    }
}

fn tool_context_for(tool_name: &str, args: &Value) -> ToolCallContext {
    let kind = classify_tool(tool_name);
    ToolCallContext {
        kind,
        file: extract_file_path(args),
        range: extract_line_range(args),
        before_hash: extract_before_hash(args),
        after_hash: extract_after_hash(args),
    }
}

fn classify_tool(tool_name: &str) -> ToolKind {
    let normalized = tool_name.to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "read" | "readfile" | "read_file" | "view" | "cat"
    ) {
        return ToolKind::Read;
    }
    if matches!(
        normalized.as_str(),
        "edit" | "write" | "multiedit" | "apply" | "apply_patch" | "patch" | "write_file"
    ) {
        return ToolKind::Edit;
    }
    ToolKind::Other
}

fn extract_file_path(args: &Value) -> Option<String> {
    let obj = args.as_object()?;
    for key in [
        "file",
        "file_path",
        "filePath",
        "path",
        "target_file",
        "filename",
    ] {
        if let Some(value) = obj.get(key).and_then(Value::as_str)
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }
    if let Some(input) = obj.get("input") {
        return extract_file_path(input);
    }
    None
}

fn extract_line_range(args: &Value) -> [u32; 2] {
    let Some(obj) = args.as_object() else {
        return [1, 1];
    };

    if let Some(range) = obj.get("range").and_then(Value::as_array)
        && range.len() == 2
    {
        let start = range[0].as_u64().unwrap_or(1) as u32;
        let end = range[1].as_u64().unwrap_or(start as u64) as u32;
        return [start.max(1), end.max(start.max(1))];
    }

    let start = obj
        .get("start")
        .and_then(Value::as_u64)
        .or_else(|| obj.get("offset").and_then(Value::as_u64).map(|n| n + 1))
        .unwrap_or(1) as u32;
    let end = obj
        .get("end")
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .or_else(|| {
            obj.get("limit")
                .and_then(Value::as_u64)
                .map(|n| start.saturating_add((n as u32).saturating_sub(1)))
        })
        .unwrap_or(start);
    [start.max(1), end.max(start.max(1))]
}

fn extract_before_hash(args: &Value) -> Option<String> {
    let obj = args.as_object()?;
    obj.get("before_hash")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            obj.get("old_string")
                .and_then(Value::as_str)
                .map(hash_text)
        })
        .or_else(|| obj.get("oldString").and_then(Value::as_str).map(hash_text))
        .or_else(|| obj.get("before").and_then(Value::as_str).map(hash_text))
}

fn extract_after_hash(args: &Value) -> Option<String> {
    let obj = args.as_object()?;
    obj.get("after_hash")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            obj.get("new_string")
                .and_then(Value::as_str)
                .map(hash_text)
        })
        .or_else(|| obj.get("newString").and_then(Value::as_str).map(hash_text))
        .or_else(|| obj.get("after").and_then(Value::as_str).map(hash_text))
        .or_else(|| obj.get("content").and_then(Value::as_str).map(hash_text))
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
        assert_eq!(rows[0]["coverage.read"], "partial");
        assert_eq!(rows[0]["coverage.edit"], "partial");
        assert_eq!(rows[0]["coverage.tool"], "partial");

        let kinds = rows
            .iter()
            .map(|row| row["k"].as_str().unwrap_or_default())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"msg.in"));
        assert!(kinds.contains(&"msg.out"));
        assert!(kinds.contains(&"tool.call"));
        assert!(kinds.contains(&"tool.result"));
        assert!(kinds.contains(&"code.read"));
        assert!(kinds.contains(&"code.edit"));

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
            .find(|row| row["k"] == "tool.call" && row["call_id"] == "call_abc")
            .expect("tool call");
        assert_eq!(tool_call["tool"], "Read");
        assert!(
            tool_call["args"]
                .as_str()
                .is_some_and(|v| v.contains("src/auth.rs"))
        );

        let tool_result = rows
            .iter()
            .find(|row| row["k"] == "tool.result" && row["call_id"] == "call_abc")
            .expect("tool result");
        assert_eq!(tool_result["tool"], "Read");
        assert_eq!(tool_result["exit"], 0);
        assert!(
            tool_result["stdout"]
                .as_str()
                .is_some_and(|v| v.contains("verify_token"))
        );

        let code_read = rows
            .iter()
            .find(|row| row["k"] == "code.read")
            .expect("code.read");
        assert_eq!(code_read["file"], "src/auth.rs");
        let anchors = code_read["anchor_hashes"].as_array().expect("anchor hashes");
        assert!(!anchors.is_empty());

        let code_edit = rows
            .iter()
            .find(|row| row["k"] == "code.edit")
            .expect("code.edit");
        assert_eq!(code_edit["file"], "src/auth.rs");
        assert!(code_edit.get("before_hash").is_some());
        assert!(code_edit.get("after_hash").is_some());
    }
}
