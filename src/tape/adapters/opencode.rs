use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn opencode_json_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    let root: Value = serde_json::from_str(input)?;
    let session_id = root
        .get("info")
        .and_then(|info| info.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let default_timestamp = root
        .get("info")
        .and_then(|info| info.get("time"))
        .and_then(|time| time.get("created"))
        .and_then(Value::as_i64)
        .and_then(timestamp_from_millis)
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());

    let mut out = Vec::new();

    out.push(json!({
        "t": default_timestamp,
        "k": "meta",
        "source": source_block("opencode", session_id.as_deref()),
        "coverage.tool": "full",
        // OpenCode also allows shell-based file reads/writes via bash-like tools,
        // which are not uniformly structured into span-level read/edit events.
        "coverage.read": "partial",
        "coverage.edit": "partial"
    }));

    if let Some(messages) = root.get("messages").and_then(Value::as_array) {
        for message in messages {
            let info = message.get("info").and_then(Value::as_object);
            let role = info
                .and_then(|obj| obj.get("role"))
                .and_then(Value::as_str)
                .unwrap_or("assistant");
            let timestamp = info
                .and_then(|obj| obj.get("time"))
                .and_then(Value::as_object)
                .and_then(|time| time.get("created"))
                .and_then(Value::as_i64)
                .and_then(timestamp_from_millis)
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());

            let Some(parts) = message.get("parts").and_then(Value::as_array) else {
                continue;
            };

            for part in parts {
                let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
                match part_type {
                    "text" => {
                        let text = part
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if text.is_empty() {
                            continue;
                        }
                        out.push(json!({
                            "t": timestamp,
                            "k": if role == "assistant" { "msg.out" } else { "msg.in" },
                            "source": source_block("opencode", session_id.as_deref()),
                            "role": role,
                            "content": text
                        }));
                    }
                    "tool" => {
                        let tool = part
                            .get("tool")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let call_id = part
                            .get("callID")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned);
                        let state = part.get("state").and_then(Value::as_object);
                        let tool_input = state
                            .and_then(|obj| obj.get("input"))
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        let args =
                            serde_json::to_string(&tool_input).unwrap_or_else(|_| "{}".to_string());

                        let mut call = serde_json::Map::new();
                        call.insert("t".to_string(), json!(timestamp));
                        call.insert("k".to_string(), json!("tool.call"));
                        call.insert(
                            "source".to_string(),
                            source_block("opencode", session_id.as_deref()),
                        );
                        call.insert("tool".to_string(), json!(tool));
                        call.insert("args".to_string(), json!(args));
                        if let Some(call_id) = &call_id {
                            call.insert("call_id".to_string(), json!(call_id));
                        }
                        out.push(Value::Object(call));

                        if tool.eq_ignore_ascii_case("read") {
                            if let Some(file) = tool_input
                                .get("filePath")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned)
                            {
                                let start_zero = tool_input
                                    .get("offset")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0);
                                let start = start_zero.saturating_add(1) as u32;
                                let end = tool_input
                                    .get("limit")
                                    .and_then(Value::as_u64)
                                    .map(|n| start.saturating_add((n as u32).saturating_sub(1)))
                                    .unwrap_or(start);
                                out.push(json!({
                                    "t": timestamp,
                                    "k": "code.read",
                                    "source": source_block("opencode", session_id.as_deref()),
                                    "file": file,
                                    "range": [start, end],
                                    "range_basis": "line"
                                }));
                            }
                        }

                        if tool.eq_ignore_ascii_case("edit") {
                            if let Some(file) = tool_input
                                .get("filePath")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned)
                            {
                                out.push(json!({
                                    "t": timestamp,
                                    "k": "code.edit",
                                    "source": source_block("opencode", session_id.as_deref()),
                                    "file": file,
                                    "before_hash": tool_input.get("oldString").and_then(Value::as_str).map(hash_text),
                                    "after_hash": tool_input.get("newString").and_then(Value::as_str).map(hash_text)
                                }));
                            }
                        }

                        if tool.eq_ignore_ascii_case("write") {
                            if let Some(file) = tool_input
                                .get("filePath")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned)
                            {
                                out.push(json!({
                                    "t": timestamp,
                                    "k": "code.edit",
                                    "source": source_block("opencode", session_id.as_deref()),
                                    "file": file,
                                    "after_hash": tool_input.get("content").and_then(Value::as_str).map(hash_text)
                                }));
                            }
                        }

                        if tool.eq_ignore_ascii_case("patch") {
                            let patch = tool_input
                                .get("patchText")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            for file in extract_patch_files(patch) {
                                out.push(json!({
                                    "t": timestamp,
                                    "k": "code.edit",
                                    "source": source_block("opencode", session_id.as_deref()),
                                    "file": file
                                }));
                            }
                        }

                        if let Some(status) = state
                            .and_then(|obj| obj.get("status"))
                            .and_then(Value::as_str)
                        {
                            match status {
                                "completed" => {
                                    let output = state
                                        .and_then(|obj| obj.get("output"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string();
                                    let mut result = serde_json::Map::new();
                                    result.insert("t".to_string(), json!(timestamp));
                                    result.insert("k".to_string(), json!("tool.result"));
                                    result.insert(
                                        "source".to_string(),
                                        source_block("opencode", session_id.as_deref()),
                                    );
                                    result.insert("tool".to_string(), json!(tool));
                                    result.insert("stdout".to_string(), json!(output));
                                    result.insert("stderr".to_string(), json!(""));
                                    result.insert("exit".to_string(), json!(0));
                                    if let Some(call_id) = &call_id {
                                        result.insert("call_id".to_string(), json!(call_id));
                                    }
                                    out.push(Value::Object(result));
                                }
                                "error" => {
                                    let error = state
                                        .and_then(|obj| obj.get("error"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string();
                                    let mut result = serde_json::Map::new();
                                    result.insert("t".to_string(), json!(timestamp));
                                    result.insert("k".to_string(), json!("tool.result"));
                                    result.insert(
                                        "source".to_string(),
                                        source_block("opencode", session_id.as_deref()),
                                    );
                                    result.insert("tool".to_string(), json!(tool));
                                    result.insert("stdout".to_string(), json!(""));
                                    result.insert("stderr".to_string(), json!(error));
                                    result.insert("exit".to_string(), json!(1));
                                    if let Some(call_id) = &call_id {
                                        result.insert("call_id".to_string(), json!(call_id));
                                    }
                                    out.push(Value::Object(result));
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    to_jsonl(&out)
}

fn to_jsonl(events: &[Value]) -> Result<String, serde_json::Error> {
    let mut out = String::new();
    for event in events {
        out.push_str(&serde_json::to_string(event)?);
        out.push('\n');
    }
    Ok(out)
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

fn extract_patch_files(patch_text: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in patch_text.lines() {
        let file = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "))
            .or_else(|| line.strip_prefix("+++ b/"))
            .or_else(|| line.strip_prefix("--- a/"));
        if let Some(path) = file.map(str::trim) {
            if path.is_empty() || path == "/dev/null" {
                continue;
            }
            if seen.insert(path.to_string()) {
                files.push(path.to_string());
            }
        }
    }
    files
}

fn timestamp_from_millis(ms: i64) -> Option<String> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
}
