use serde_json::{Value, json};

const CURSOR_COVERAGE_TOOL: &str = "full";
const CURSOR_COVERAGE_READ: &str = "partial";
const CURSOR_COVERAGE_EDIT: &str = "partial";

pub fn cursor_jsonl_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    let mut out = Vec::new();
    let mut session_id: Option<String> = None;
    let mut first_timestamp: Option<String> = None;
    let mut emitted_meta = false;

    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line)?;
        if session_id.is_none() {
            session_id = row
                .get("session_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }

        if first_timestamp.is_none() {
            first_timestamp = row
                .get("timestamp")
                .or_else(|| row.get("t"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }

        let timestamp = row
            .get("timestamp")
            .or_else(|| row.get("t"))
            .and_then(Value::as_str)
            .or(first_timestamp.as_deref())
            .unwrap_or("1970-01-01T00:00:00Z");

        let row_type = row.get("type").and_then(Value::as_str).unwrap_or("");
        match row_type {
            "system" => {
                if row.get("subtype").and_then(Value::as_str) == Some("init") {
                    out.push(json!({
                        "t": timestamp,
                        "k": "meta",
                        "source": cursor_source(session_id.as_deref()),
                        "model": row.get("model").and_then(Value::as_str),
                        "coverage.tool": CURSOR_COVERAGE_TOOL,
                        "coverage.read": CURSOR_COVERAGE_READ,
                        "coverage.edit": CURSOR_COVERAGE_EDIT
                    }));
                    emitted_meta = true;
                }
            }
            "user" | "assistant" => {
                let message = row.get("message").and_then(Value::as_object);
                let role = message
                    .and_then(|obj| obj.get("role"))
                    .and_then(Value::as_str)
                    .unwrap_or(if row_type == "assistant" {
                        "assistant"
                    } else {
                        "user"
                    });
                let content = message
                    .and_then(|obj| obj.get("content"))
                    .map(content_text)
                    .unwrap_or_default();
                if !content.is_empty() {
                    out.push(json!({
                        "t": timestamp,
                        "k": if role == "assistant" { "msg.out" } else { "msg.in" },
                        "source": cursor_source(session_id.as_deref()),
                        "role": role,
                        "content": content
                    }));
                }
            }
            "tool_call" => {
                let call_id = row
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let tool = cursor_tool_name(&row);
                let subtype = row.get("subtype").and_then(Value::as_str).unwrap_or("");
                match subtype {
                    "started" => {
                        let args = cursor_tool_args(&row);
                        let mut call = serde_json::Map::new();
                        call.insert("t".to_string(), json!(timestamp));
                        call.insert("k".to_string(), json!("tool.call"));
                        call.insert("source".to_string(), cursor_source(session_id.as_deref()));
                        call.insert("tool".to_string(), json!(tool));
                        call.insert("args".to_string(), json!(args));
                        if let Some(call_id) = &call_id {
                            call.insert("call_id".to_string(), json!(call_id));
                        }
                        out.push(Value::Object(call));
                    }
                    "completed" => {
                        let mut result = serde_json::Map::new();
                        result.insert("t".to_string(), json!(timestamp));
                        result.insert("k".to_string(), json!("tool.result"));
                        result.insert("source".to_string(), cursor_source(session_id.as_deref()));
                        result.insert("tool".to_string(), json!(tool));
                        result.insert("exit".to_string(), json!(cursor_tool_exit_code(&row)));
                        result.insert(
                            "stdout".to_string(),
                            json!(cursor_tool_stdout(&row).unwrap_or_default()),
                        );
                        result.insert("stderr".to_string(), json!(cursor_tool_stderr(&row)));
                        if let Some(call_id) = &call_id {
                            result.insert("call_id".to_string(), json!(call_id));
                        }
                        out.push(Value::Object(result));

                        if let Some(file) = cursor_write_edit_path(&row) {
                            out.push(json!({
                                "t": timestamp,
                                "k": "code.edit",
                                "source": cursor_source(session_id.as_deref()),
                                "file": file
                            }));
                        }
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
                "source": cursor_source(session_id.as_deref()),
                "coverage.tool": CURSOR_COVERAGE_TOOL,
                "coverage.read": CURSOR_COVERAGE_READ,
                "coverage.edit": CURSOR_COVERAGE_EDIT
            }),
        );
    }

    to_jsonl(&out)
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

fn cursor_source(session_id: Option<&str>) -> Value {
    match session_id {
        Some(session_id) => json!({
            "harness": "cursor",
            "session_id": session_id
        }),
        None => json!({
            "harness": "cursor"
        }),
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

fn cursor_tool_name(row: &Value) -> String {
    let Some(tool_call) = row.get("tool_call").and_then(Value::as_object) else {
        return "unknown".to_string();
    };

    if tool_call.contains_key("readToolCall") {
        return "readToolCall".to_string();
    }
    if tool_call.contains_key("writeToolCall") {
        return "writeToolCall".to_string();
    }
    if let Some(name) = tool_call
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
    {
        return name.to_string();
    }
    "unknown".to_string()
}

fn cursor_tool_args(row: &Value) -> String {
    let args = row
        .get("tool_call")
        .and_then(Value::as_object)
        .and_then(|tool_call| {
            tool_call
                .get("readToolCall")
                .and_then(|read| read.get("args"))
                .or_else(|| {
                    tool_call
                        .get("writeToolCall")
                        .and_then(|write| write.get("args"))
                })
                .or_else(|| {
                    tool_call
                        .get("function")
                        .and_then(|function| function.get("arguments"))
                })
        })
        .cloned()
        .unwrap_or_else(|| json!({}));
    serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string())
}

fn cursor_tool_stdout(row: &Value) -> Option<String> {
    let tool_call = row.get("tool_call")?.as_object()?;
    if let Some(content) = tool_call
        .get("readToolCall")
        .and_then(|read| read.get("result"))
        .and_then(|result| result.get("success"))
        .and_then(|success| success.get("content"))
        .and_then(Value::as_str)
    {
        return Some(content.to_string());
    }

    if let Some(success) = tool_call
        .get("writeToolCall")
        .and_then(|write| write.get("result"))
        .and_then(|result| result.get("success"))
    {
        return serde_json::to_string(success).ok();
    }

    if let Some(success) = tool_call
        .get("function")
        .and_then(|function| function.get("result"))
        .and_then(|result| result.get("success"))
    {
        return serde_json::to_string(success).ok();
    }

    None
}

fn cursor_tool_stderr(row: &Value) -> String {
    let Some(tool_call) = row.get("tool_call").and_then(Value::as_object) else {
        return String::new();
    };
    if let Some(error) = tool_call
        .get("readToolCall")
        .and_then(|read| read.get("result"))
        .and_then(|result| result.get("error"))
    {
        return serde_json::to_string(error).unwrap_or_else(|_| String::new());
    }
    if let Some(error) = tool_call
        .get("writeToolCall")
        .and_then(|write| write.get("result"))
        .and_then(|result| result.get("error"))
    {
        return serde_json::to_string(error).unwrap_or_else(|_| String::new());
    }
    if let Some(error) = tool_call
        .get("function")
        .and_then(|function| function.get("result"))
        .and_then(|result| result.get("error"))
    {
        return serde_json::to_string(error).unwrap_or_else(|_| String::new());
    }
    String::new()
}

fn cursor_tool_exit_code(row: &Value) -> i64 {
    let Some(tool_call) = row.get("tool_call").and_then(Value::as_object) else {
        return 0;
    };
    let has_error = tool_call
        .get("readToolCall")
        .and_then(|read| read.get("result"))
        .and_then(|result| result.get("error"))
        .is_some()
        || tool_call
            .get("writeToolCall")
            .and_then(|write| write.get("result"))
            .and_then(|result| result.get("error"))
            .is_some()
        || tool_call
            .get("function")
            .and_then(|function| function.get("result"))
            .and_then(|result| result.get("error"))
            .is_some();
    if has_error { 1 } else { 0 }
}

fn cursor_write_edit_path(row: &Value) -> Option<String> {
    let tool_call = row.get("tool_call").and_then(Value::as_object)?;
    let write = tool_call.get("writeToolCall")?;

    write
        .get("result")
        .and_then(|result| result.get("success"))
        .and_then(|success| success.get("path"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            write
                .get("args")
                .and_then(|args| args.get("path"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::cursor_jsonl_to_tape_jsonl;

    #[test]
    fn cursor_adapter_emits_tool_pairs_and_write_based_code_edit() {
        let input = include_str!("../../../tests/fixtures/cursor/supported_paths.jsonl");
        let out = cursor_jsonl_to_tape_jsonl(input).expect("adapter should parse");
        let events: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON event"))
            .collect();

        let meta = events
            .iter()
            .find(|event| event.get("k").and_then(Value::as_str) == Some("meta"))
            .expect("meta event");
        assert_eq!(meta["source"]["harness"], "cursor");
        assert_eq!(
            meta["source"]["session_id"],
            "c6b62c6f-7ead-4fd6-9922-e952131177ff"
        );
        assert_eq!(meta["coverage.tool"], "full");
        assert_eq!(meta["coverage.read"], "partial");
        assert_eq!(meta["coverage.edit"], "partial");

        let read_call = events
            .iter()
            .find(|event| {
                event.get("k").and_then(Value::as_str) == Some("tool.call")
                    && event.get("tool").and_then(Value::as_str) == Some("readToolCall")
            })
            .expect("read tool.call");
        assert_eq!(read_call["call_id"], "toolu_vrtx_01NnjaR886UcE8whekg2MGJd");

        let write_result = events
            .iter()
            .find(|event| {
                event.get("k").and_then(Value::as_str) == Some("tool.result")
                    && event.get("tool").and_then(Value::as_str) == Some("writeToolCall")
            })
            .expect("write tool.result");
        assert_eq!(write_result["exit"], 0);

        let edit = events
            .iter()
            .find(|event| event.get("k").and_then(Value::as_str) == Some("code.edit"))
            .expect("code.edit event");
        assert_eq!(edit["file"], "/Users/user/project/summary.txt");
    }
}
