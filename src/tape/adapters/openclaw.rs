use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const DEFAULT_TS: &str = "1970-01-01T00:00:00Z";

pub fn openclaw_jsonl_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    let mut events = Vec::new();
    let mut first_ts = None::<String>;
    let mut saw_json = false;

    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<Value>(line) {
            Ok(row) => {
                saw_json = true;
                extract_from_row(&row, &mut events, &mut first_ts);
            }
            Err(_) => {
                let timestamp = first_ts.clone().unwrap_or_else(|| DEFAULT_TS.to_string());
                events.push(json!({
                    "t": timestamp,
                    "k": "msg.out",
                    "source": source_block(None),
                    "role": "assistant",
                    "content": line.trim(),
                }));
            }
        }
    }

    if !saw_json {
        return to_jsonl(&with_meta(None, events));
    }

    let session_id = first_session_id_from_jsonl(input);
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

fn extract_from_row(row: &Value, out: &mut Vec<Value>, first_ts: &mut Option<String>) {
    let Some(obj) = row.as_object() else {
        return;
    };

    let timestamp = extract_timestamp(row);
    if first_ts.is_none() {
        *first_ts = Some(timestamp.clone());
    }
    let session_id = obj
        .get("session_id")
        .and_then(Value::as_str)
        .or_else(|| obj.get("sessionId").and_then(Value::as_str));

    if let Some(text) = extract_text(obj) {
        let role = extract_role(obj);
        out.push(json!({
            "t": timestamp,
            "k": if role == "assistant" { "msg.out" } else { "msg.in" },
            "source": source_block(session_id),
            "role": role,
            "content": text,
        }));
    }

    if let Some(tool_call) = extract_tool_call(obj, &timestamp, session_id) {
        out.push(tool_call);
    }
    if let Some(tool_result) = extract_tool_result(obj, &timestamp, session_id) {
        out.push(tool_result);
    }
    if let Some(code_read) = extract_code_read(obj, &timestamp, session_id) {
        out.push(code_read);
    }
    if let Some(code_edit) = extract_code_edit(obj, &timestamp, session_id) {
        out.push(code_edit);
    }
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

fn extract_text(obj: &serde_json::Map<String, Value>) -> Option<String> {
    obj.get("content")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| obj.get("message").and_then(Value::as_str).map(ToOwned::to_owned))
        .or_else(|| obj.get("text").and_then(Value::as_str).map(ToOwned::to_owned))
        .filter(|text| !text.is_empty())
}

fn extract_role(obj: &serde_json::Map<String, Value>) -> &'static str {
    match obj.get("role").and_then(Value::as_str).unwrap_or("assistant") {
        "user" => "user",
        _ => "assistant",
    }
}

fn extract_tool_call(
    obj: &serde_json::Map<String, Value>,
    timestamp: &str,
    session_id: Option<&str>,
) -> Option<Value> {
    let event_kind = event_kind(obj);
    let has_call_shape =
        event_kind == "tool.call" || event_kind == "tool_call" || obj.get("args").is_some();
    if !has_call_shape {
        return None;
    }

    let tool = obj
        .get("tool")
        .and_then(Value::as_str)
        .or_else(|| obj.get("name").and_then(Value::as_str))
        .unwrap_or("unknown");
    let args_value = obj
        .get("args")
        .cloned()
        .or_else(|| obj.get("input").cloned())
        .unwrap_or_else(|| json!({}));
    let args = serde_json::to_string(&args_value).unwrap_or_else(|_| "{}".to_string());
    let mut event = serde_json::Map::new();
    event.insert("t".to_string(), json!(timestamp));
    event.insert("k".to_string(), json!("tool.call"));
    event.insert("source".to_string(), source_block(session_id));
    event.insert("tool".to_string(), json!(tool));
    event.insert("args".to_string(), json!(args));
    if let Some(call_id) = obj
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| obj.get("id").and_then(Value::as_str))
    {
        event.insert("call_id".to_string(), json!(call_id));
    }
    Some(Value::Object(event))
}

fn extract_tool_result(
    obj: &serde_json::Map<String, Value>,
    timestamp: &str,
    session_id: Option<&str>,
) -> Option<Value> {
    let event_kind = event_kind(obj);
    let has_result_shape = event_kind == "tool.result"
        || event_kind == "tool_result"
        || obj.get("stdout").is_some()
        || obj.get("stderr").is_some()
        || obj.get("result").is_some()
        || obj.get("exit").is_some();
    if !has_result_shape {
        return None;
    }

    let tool = obj
        .get("tool")
        .and_then(Value::as_str)
        .or_else(|| obj.get("name").and_then(Value::as_str))
        .unwrap_or("unknown");
    let stdout = obj
        .get("stdout")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| obj.get("result").and_then(Value::as_str).map(ToOwned::to_owned))
        .unwrap_or_default();
    let stderr = obj
        .get("stderr")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_default();
    let exit = obj.get("exit").and_then(Value::as_i64).unwrap_or(0);

    let mut event = serde_json::Map::new();
    event.insert("t".to_string(), json!(timestamp));
    event.insert("k".to_string(), json!("tool.result"));
    event.insert("source".to_string(), source_block(session_id));
    event.insert("tool".to_string(), json!(tool));
    event.insert("stdout".to_string(), json!(stdout));
    event.insert("stderr".to_string(), json!(stderr));
    event.insert("exit".to_string(), json!(exit));
    if let Some(call_id) = obj
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| obj.get("id").and_then(Value::as_str))
    {
        event.insert("call_id".to_string(), json!(call_id));
    }

    Some(Value::Object(event))
}

fn extract_code_read(
    obj: &serde_json::Map<String, Value>,
    timestamp: &str,
    session_id: Option<&str>,
) -> Option<Value> {
    let kind = event_kind(obj);
    let explicit_read = kind == "code.read" || kind == "read" || kind == "file.read";
    if !explicit_read {
        return None;
    }
    let file = obj
        .get("file")
        .and_then(Value::as_str)
        .or_else(|| obj.get("path").and_then(Value::as_str))?;
    let range = extract_range(obj).unwrap_or([1, 1]);
    Some(json!({
        "t": timestamp,
        "k": "code.read",
        "source": source_block(session_id),
        "file": file,
        "range": range,
    }))
}

fn extract_code_edit(
    obj: &serde_json::Map<String, Value>,
    timestamp: &str,
    session_id: Option<&str>,
) -> Option<Value> {
    let kind = event_kind(obj);
    let explicit_edit = kind == "code.edit"
        || kind == "edit"
        || kind == "write"
        || kind == "file.write"
        || kind == "patch";
    if !explicit_edit {
        return None;
    }

    let file = obj
        .get("file")
        .and_then(Value::as_str)
        .or_else(|| obj.get("path").and_then(Value::as_str))
        .or_else(|| obj.get("file_path").and_then(Value::as_str))
        .unwrap_or("unknown");

    let before_hash = obj
        .get("before")
        .and_then(Value::as_str)
        .map(hash_text)
        .or_else(|| obj.get("before_hash").and_then(Value::as_str).map(ToOwned::to_owned));
    let after_hash = obj
        .get("after")
        .and_then(Value::as_str)
        .map(hash_text)
        .or_else(|| obj.get("after_hash").and_then(Value::as_str).map(ToOwned::to_owned));

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
    if let Some(similarity) = obj.get("similarity").and_then(Value::as_f64) {
        event.insert("similarity".to_string(), json!(similarity as f32));
    }
    if let Some(range) = extract_range(obj) {
        event.insert("before_range".to_string(), json!(range));
        event.insert("after_range".to_string(), json!(range));
    }
    Some(Value::Object(event))
}

fn extract_range(obj: &serde_json::Map<String, Value>) -> Option<[u32; 2]> {
    if let Some(raw) = obj.get("range").and_then(Value::as_array) {
        if raw.len() == 2 {
            let start = raw[0].as_u64()? as u32;
            let end = raw[1].as_u64()? as u32;
            return Some([start, end]);
        }
    }
    let start = obj.get("start").and_then(Value::as_u64).map(|n| n as u32)?;
    let end = obj.get("end").and_then(Value::as_u64).map(|n| n as u32)?;
    Some([start, end])
}

fn event_kind(obj: &serde_json::Map<String, Value>) -> &str {
    obj.get("k")
        .and_then(Value::as_str)
        .or_else(|| obj.get("type").and_then(Value::as_str))
        .or_else(|| obj.get("event").and_then(Value::as_str))
        .unwrap_or("")
}

fn first_session_id_from_jsonl(input: &str) -> Option<String> {
    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line).ok()?;
        if let Some(session_id) = row.get("session_id").and_then(Value::as_str) {
            return Some(session_id.to_string());
        }
        if let Some(session_id) = row.get("sessionId").and_then(Value::as_str) {
            return Some(session_id.to_string());
        }
    }
    None
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
    use super::openclaw_jsonl_to_tape_jsonl;

    #[test]
    fn openclaw_adapter_emits_messages_tools_and_edits() {
        let input = include_str!("../../../tests/fixtures/openclaw/session_log.jsonl");
        let out = openclaw_jsonl_to_tape_jsonl(input).expect("adapter should parse");
        assert!(out.contains(r#""k":"meta""#));
        assert!(out.contains(r#""source":{"harness":"openclaw","session_id":"oc-123"}"#));
        assert!(out.contains(r#""k":"msg.in""#));
        assert!(out.contains(r#""k":"msg.out""#));
        assert!(out.contains(r#""k":"tool.call""#));
        assert!(out.contains(r#""k":"tool.result""#));
        assert!(out.contains(r#""k":"code.read""#));
        assert!(out.contains(r#""k":"code.edit""#));
    }
}
