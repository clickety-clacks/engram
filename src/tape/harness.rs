use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn codex_jsonl_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    let mut out = Vec::new();
    let mut call_tools: HashMap<String, String> = HashMap::new();

    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line)?;
        let timestamp = row
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("1970-01-01T00:00:00Z");
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
                out.push(json!({
                    "t": timestamp,
                    "k": "meta",
                    "model": model,
                    "repo_head": repo_head
                }));
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
                        let args = payload
                            .and_then(|obj| obj.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if let Some(call_id) = payload
                            .and_then(|obj| obj.get("call_id"))
                            .and_then(Value::as_str)
                        {
                            call_tools.insert(call_id.to_string(), tool.to_string());
                        }
                        out.push(json!({
                            "t": timestamp,
                            "k": "tool.call",
                            "tool": tool,
                            "args": args
                        }));
                        if tool == "apply_patch" {
                            for file in extract_apply_patch_files(&args) {
                                out.push(json!({
                                    "t": timestamp,
                                    "k": "code.edit",
                                    "file": file
                                }));
                            }
                        }
                    }
                    "function_call_output" => {
                        let call_id = payload
                            .and_then(|obj| obj.get("call_id"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let output = payload
                            .and_then(|obj| obj.get("output"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let tool = call_tools
                            .get(call_id)
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_string());
                        out.push(json!({
                            "t": timestamp,
                            "k": "tool.result",
                            "tool": tool,
                            "exit": extract_exit_code(&output).unwrap_or(0),
                            "stdout": output,
                            "stderr": ""
                        }));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    to_jsonl(&out)
}

pub fn claude_jsonl_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    let mut out = Vec::new();
    let mut tool_by_id: HashMap<String, String> = HashMap::new();

    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line)?;
        let timestamp = row
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("1970-01-01T00:00:00Z");
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
                            "tool": tool,
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
                if let Some(blocks) = message.and_then(|obj| obj.get("content")).and_then(Value::as_array) {
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
                                        "role": role,
                                        "content": text
                                    }));
                                }
                            }
                            "tool_use" => {
                                let tool = block.get("name").and_then(Value::as_str).unwrap_or("unknown");
                                let tool_input = block.get("input").cloned().unwrap_or(Value::Null);
                                let tool_use_id = block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                tool_by_id.insert(tool_use_id, tool.to_string());

                                out.push(json!({
                                    "t": timestamp,
                                    "k": "tool.call",
                                    "tool": tool,
                                    "args": serde_json::to_string(&tool_input).unwrap_or_else(|_| "{}".to_string())
                                }));

                                match tool {
                                    "Read" => {
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
                                                .map(|n| start.saturating_add((n as u32).saturating_sub(1)))
                                                .unwrap_or(start);
                                            out.push(json!({
                                                "t": timestamp,
                                                "k": "code.read",
                                                "file": file,
                                                "range": [start, end]
                                            }));
                                        }
                                    }
                                    "Edit" => {
                                        if let Some(file) = tool_input
                                            .get("file_path")
                                            .and_then(Value::as_str)
                                            .map(ToOwned::to_owned)
                                        {
                                            out.push(json!({
                                                "t": timestamp,
                                                "k": "code.edit",
                                                "file": file,
                                                "before_hash": tool_input.get("old_string").and_then(Value::as_str).map(hash_text),
                                                "after_hash": tool_input.get("new_string").and_then(Value::as_str).map(hash_text)
                                            }));
                                        }
                                    }
                                    "Write" => {
                                        if let Some(file) = tool_input
                                            .get("file_path")
                                            .and_then(Value::as_str)
                                            .map(ToOwned::to_owned)
                                        {
                                            out.push(json!({
                                                "t": timestamp,
                                                "k": "code.edit",
                                                "file": file,
                                                "after_hash": tool_input.get("content").and_then(Value::as_str).map(hash_text)
                                            }));
                                        }
                                    }
                                    "MultiEdit" => {
                                        if let Some(file) = tool_input
                                            .get("file_path")
                                            .and_then(Value::as_str)
                                            .map(ToOwned::to_owned)
                                        {
                                            if let Some(edits) = tool_input.get("edits").and_then(Value::as_array) {
                                                for edit in edits {
                                                    out.push(json!({
                                                        "t": timestamp,
                                                        "k": "code.edit",
                                                        "file": file,
                                                        "before_hash": edit.get("old_string").and_then(Value::as_str).map(hash_text),
                                                        "after_hash": edit.get("new_string").and_then(Value::as_str).map(hash_text)
                                                    }));
                                                }
                                            }
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

fn extract_apply_patch_files(arguments: &str) -> Vec<String> {
    let patch_body = serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| value.get("patch").and_then(Value::as_str).map(ToOwned::to_owned))
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
        let input = r#"{"type":"assistant","timestamp":"2026-02-22T00:00:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/repo/src/lib.rs","offset":10,"limit":5}}]}}
{"type":"user","timestamp":"2026-02-22T00:00:01Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"10->line"}]}}
{"type":"assistant","timestamp":"2026-02-22T00:00:02Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Edit","input":{"file_path":"/repo/src/lib.rs","old_string":"a","new_string":"b"}}]}}
"#;

        let out = claude_jsonl_to_tape_jsonl(input).expect("adapter should parse");
        assert!(out.contains(r#""k":"tool.call""#), "out={out}");
        assert!(out.contains(r#""tool":"Read""#), "out={out}");
        assert!(
            out.contains(r#""k":"code.read""#)
                && out.contains(r#""file":"/repo/src/lib.rs""#)
                && out.contains(r#""range":[10,14]"#),
            "out={out}"
        );
        assert!(out.contains(r#""k":"tool.result""#), "out={out}");
        assert!(out.contains(r#""tool":"Edit""#), "out={out}");
        assert!(out.contains(r#""k":"code.edit""#), "out={out}");
    }
}
