use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::{Value, json};

use super::structured::{bounded_shell_read, parse_patch, patch_is_complete};

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
    let mut read_total = 0u32;
    let mut read_accounted = 0u32;
    let mut edit_total = 0u32;
    let mut edit_accounted = 0u32;

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
                        let (read_claims, edit_claims) = match tool.to_ascii_lowercase().as_str() {
                            "read" => (1, 0),
                            "edit" | "write" | "patch" => (0, 1),
                            "bash" | "exec" | "shell" | "process" => (1, 1),
                            _ => (0, 0),
                        };
                        read_total = read_total.saturating_add(read_claims);
                        edit_total = edit_total.saturating_add(edit_claims);

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

                        if let Some(error) = state
                            .and_then(|obj| obj.get("error"))
                            .filter(|error| !error.is_null())
                        {
                            read_accounted = read_accounted.saturating_add(read_claims);
                            edit_accounted = edit_accounted.saturating_add(edit_claims);
                            let error =
                                error.as_str().map(ToOwned::to_owned).unwrap_or_else(|| {
                                    serde_json::to_string(error).unwrap_or_default()
                                });
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
                            continue;
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
                                    let (reads, edits) = emit_completed_structured(
                                        &mut out,
                                        &timestamp,
                                        session_id.as_deref(),
                                        &tool,
                                        &tool_input,
                                        &output,
                                        root.get("info")
                                            .and_then(|info| info.get("path"))
                                            .and_then(|path| path.get("cwd"))
                                            .and_then(Value::as_str),
                                    );
                                    read_accounted = read_accounted.saturating_add(reads);
                                    edit_accounted = edit_accounted.saturating_add(edits);
                                }
                                "error" => {
                                    read_accounted = read_accounted.saturating_add(read_claims);
                                    edit_accounted = edit_accounted.saturating_add(edit_claims);
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

    if let Some(meta) = out.first_mut().and_then(Value::as_object_mut) {
        meta.insert(
            "coverage.read".to_string(),
            json!(coverage(read_total, read_accounted)),
        );
        meta.insert(
            "coverage.edit".to_string(),
            json!(coverage(edit_total, edit_accounted)),
        );
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

fn emit_completed_structured(
    out: &mut Vec<Value>,
    timestamp: &str,
    session_id: Option<&str>,
    tool: &str,
    input: &Value,
    output: &str,
    cwd: Option<&str>,
) -> (u32, u32) {
    let source = || source_block("opencode", session_id);
    if tool.eq_ignore_ascii_case("read") {
        if output.is_empty() {
            return (0, 0);
        }
        if let Some(file) = input.get("filePath").and_then(Value::as_str) {
            let start = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as u32 + 1;
            let lines = output.lines().count() as u32;
            if lines > 0 {
                out.push(
                    json!({"t":timestamp,"k":"code.read","source":source(),"file":file,
                    "range":[start,start + lines - 1],"text":output,"range_basis":"line"}),
                );
                return (1, 0);
            }
        }
    } else if tool.eq_ignore_ascii_case("edit") {
        if let (Some(file), Some(before), Some(after)) = (
            input.get("filePath").and_then(Value::as_str),
            input.get("oldString").and_then(Value::as_str),
            input.get("newString").and_then(Value::as_str),
        ) {
            out.push(
                json!({"t":timestamp,"k":"code.edit","source":source(),"file":file,
                "before_text":before,"after_text":after}),
            );
            return (0, 1);
        }
    } else if tool.eq_ignore_ascii_case("write") {
        if let (Some(file), Some(after)) = (
            input.get("filePath").and_then(Value::as_str),
            input.get("content").and_then(Value::as_str),
        ) {
            out.push(
                json!({"t":timestamp,"k":"code.edit","source":source(),"file":file,
                "after_text":after}),
            );
            return (0, 1);
        }
    } else if tool.eq_ignore_ascii_case("patch") {
        if let Some(patch) = input.get("patchText").and_then(Value::as_str) {
            let complete = patch_is_complete(patch);
            let edits = parse_patch(patch);
            if edits.is_empty() {
                return (0, 0);
            }
            for edit in edits {
                let mut event =
                    json!({"t":timestamp,"k":"code.edit","source":source(),"file":edit.file});
                if let Some(before) = edit.before_text {
                    event["before_text"] = json!(before);
                }
                if let Some(after) = edit.after_text {
                    event["after_text"] = json!(after);
                }
                out.push(event);
            }
            return (0, u32::from(complete));
        }
    } else if matches!(
        tool.to_ascii_lowercase().as_str(),
        "bash" | "exec" | "shell" | "process"
    ) && let Some(command) = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(Value::as_str)
        && let Some(read) = bounded_shell_read(
            command,
            output,
            input.get("workdir").and_then(Value::as_str),
            cwd,
        )
    {
        let coverage_complete = read.coverage_complete;
        out.push(
            json!({"t":timestamp,"k":"code.read","source":source(),"file":read.file,
            "range":read.range,"text":read.text,"range_basis":"line"}),
        );
        return if coverage_complete { (1, 1) } else { (0, 0) };
    }
    (0, 0)
}

fn coverage(total: u32, accounted: u32) -> &'static str {
    if total == accounted {
        "full"
    } else {
        "partial"
    }
}

fn timestamp_from_millis(ms: i64) -> Option<String> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
}
