use std::collections::{HashMap, HashSet};

use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CURSOR_COVERAGE_TOOL: &str = "full";
const CURSOR_COVERAGE_READ: &str = "partial";
const CURSOR_COVERAGE_EDIT: &str = "partial";

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
                            for file in extract_opencode_patch_files(patch) {
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
                        "source": source_block("cursor", session_id.as_deref()),
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
                        "source": source_block("cursor", session_id.as_deref()),
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
                        call.insert(
                            "source".to_string(),
                            source_block("cursor", session_id.as_deref()),
                        );
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
                        result.insert(
                            "source".to_string(),
                            source_block("cursor", session_id.as_deref()),
                        );
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
                                "source": source_block("cursor", session_id.as_deref()),
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
                "source": source_block("cursor", session_id.as_deref()),
                "coverage.tool": CURSOR_COVERAGE_TOOL,
                "coverage.read": CURSOR_COVERAGE_READ,
                "coverage.edit": CURSOR_COVERAGE_EDIT
            }),
        );
    }

    to_jsonl(&out)
}

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
            "source": source_block("gemini-cli", session_id),
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
                    "source": source_block("gemini-cli", session_id),
                    "role": "user",
                    "content": content
                })),
                "gemini" => out.push(json!({
                    "t": timestamp,
                    "k": "msg.out",
                    "source": source_block("gemini-cli", session_id),
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
        "source": source_block("gemini-cli", session_id),
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
                            "source": source_block("gemini-cli", session_id),
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
                            "source": source_block("gemini-cli", session_id),
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
                            "source": source_block("gemini-cli", session_id),
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
                                    "source": source_block("gemini-cli", session_id),
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
                                    "source": source_block("gemini-cli", session_id),
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
                            "source": source_block("gemini-cli", session_id),
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

fn extract_opencode_patch_files(patch_text: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
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
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{
        claude_jsonl_to_tape_jsonl, cursor_jsonl_to_tape_jsonl, gemini_json_to_tape_jsonl,
        opencode_json_to_tape_jsonl,
    };

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

    #[test]
    fn cursor_adapter_emits_tool_pairs_and_write_based_code_edit() {
        let input = include_str!("../../tests/fixtures/cursor/supported_paths.jsonl");
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

    #[test]
    fn gemini_adapter_emits_tool_pairs_and_structured_read_edit() {
        let input = include_str!("../../tests/fixtures/gemini/session_with_tools.json");
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
        let input = include_str!("../../tests/fixtures/gemini/logs.json");
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
