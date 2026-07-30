use std::collections::HashMap;

use serde_json::{Value, json};

use super::structured::{bounded_shell_read, parse_patch, patch_is_complete};

const CODEX_COVERAGE_TOOL: &str = "full";
const CODEX_COVERAGE_READ: &str = "partial";
const CODEX_COVERAGE_EDIT: &str = "partial";

pub fn codex_jsonl_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    let mut out = Vec::new();
    let mut calls: HashMap<String, CodexCall> = HashMap::new();
    let mut session_id: Option<String> = None;
    let mut first_timestamp: Option<String> = None;
    let mut emitted_meta = false;
    let mut session_cwd = None::<String>;

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
                session_cwd = payload
                    .and_then(|obj| obj.get("cwd"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
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
                out.push(codex_meta_event(
                    timestamp,
                    session_id.as_deref(),
                    model,
                    repo_head,
                ));
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
                            .filter(|id| !id.is_empty())
                            .map(ToOwned::to_owned);
                        let args = payload
                            .and_then(|obj| obj.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        emit_tool_call(
                            &mut out,
                            &mut calls,
                            timestamp,
                            session_id.as_deref(),
                            tool,
                            call_id.as_deref(),
                            &args,
                        );
                    }
                    "custom_tool_call" => {
                        let tool = payload
                            .and_then(|obj| obj.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let call_id = payload
                            .and_then(|obj| obj.get("call_id"))
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .map(ToOwned::to_owned);
                        let args = payload
                            .and_then(|obj| obj.get("input"))
                            .map(value_to_argument_string)
                            .unwrap_or_default();
                        emit_tool_call(
                            &mut out,
                            &mut calls,
                            timestamp,
                            session_id.as_deref(),
                            tool,
                            call_id.as_deref(),
                            &args,
                        );
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let call_id = payload
                            .and_then(|obj| obj.get("call_id"))
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .map(ToOwned::to_owned);
                        let output = payload
                            .and_then(|obj| obj.get("output"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let context = call_id.as_ref().and_then(|id| calls.remove(id));
                        let tool = context
                            .as_ref()
                            .map(|context| context.tool.clone())
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
                        let exit = match context.as_ref().map(|context| context.tool.as_str()) {
                            Some("apply_patch") if payload_type == "custom_tool_call_output" => {
                                custom_tool_exit_code(&output)
                            }
                            Some("exec_command") if payload_type == "function_call_output" => {
                                extract_exit_code(&output)
                            }
                            _ => None,
                        };
                        if let Some(exit) = exit {
                            result_event.insert("exit".to_string(), json!(exit));
                        }
                        result_event.insert("stdout".to_string(), json!(output));
                        result_event.insert("stderr".to_string(), json!(""));
                        out.push(Value::Object(result_event));
                        if exit == Some(0)
                            && let Some(context) = context
                        {
                            emit_structured_after_result(
                                &mut out,
                                timestamp,
                                session_id.as_deref(),
                                session_cwd.as_deref(),
                                &context,
                                &output,
                            );
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
            codex_meta_event(
                first_timestamp.as_deref().unwrap_or("1970-01-01T00:00:00Z"),
                session_id.as_deref(),
                None,
                None,
            ),
        );
    }
    let (read_coverage, edit_coverage) = codex_coverage(&out);
    for meta in out
        .iter_mut()
        .filter_map(Value::as_object_mut)
        .filter(|event| event.get("k").and_then(Value::as_str) == Some("meta"))
    {
        meta.insert("coverage.read".to_string(), json!(read_coverage));
        meta.insert("coverage.edit".to_string(), json!(edit_coverage));
    }

    to_jsonl(&out)
}

fn codex_coverage(events: &[Value]) -> (&'static str, &'static str) {
    let mut read_partial = false;
    let mut edit_partial = false;
    for (index, call) in events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["k"] == "tool.call")
    {
        let tool = call["tool"].as_str().unwrap_or("");
        if !matches!(tool, "exec_command" | "apply_patch") {
            continue;
        }
        let result = events
            .iter()
            .enumerate()
            .skip(index + 1)
            .find(|(_, event)| {
                event["k"] == "tool.result" && event.get("call_id") == call.get("call_id")
            });
        let Some((result_index, result)) = result else {
            if tool == "exec_command" {
                read_partial = true;
                edit_partial = true;
            } else {
                edit_partial = true
            }
            continue;
        };
        let Some(exit) = result.get("exit").and_then(Value::as_i64) else {
            if tool == "exec_command" {
                read_partial = true;
                edit_partial = true;
            } else {
                edit_partial = true
            }
            continue;
        };
        if exit != 0 {
            continue;
        }
        if tool == "apply_patch"
            && !call["args"]
                .as_str()
                .is_some_and(|args| patch_is_complete(&patch_body(args)))
        {
            edit_partial = true;
            continue;
        }
        let expected_kind = if tool == "exec_command" {
            "code.read"
        } else {
            "code.edit"
        };
        let emitted = events
            .iter()
            .skip(result_index + 1)
            .take_while(|event| !matches!(event["k"].as_str(), Some("tool.call" | "tool.result")))
            .find(|event| event["k"] == expected_kind);
        if emitted.is_none() {
            if tool == "exec_command" {
                read_partial = true;
                edit_partial = true;
            } else {
                edit_partial = true;
            }
        } else if tool == "exec_command"
            && emitted
                .and_then(|event| event["file"].as_str())
                .is_some_and(|file| !std::path::Path::new(file).is_absolute())
        {
            read_partial = true;
            edit_partial = true;
        }
    }
    (
        if read_partial { "partial" } else { "full" },
        if edit_partial { "partial" } else { "full" },
    )
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

fn codex_meta_event(
    timestamp: &str,
    session_id: Option<&str>,
    model: Option<String>,
    repo_head: Option<String>,
) -> Value {
    let mut event = serde_json::Map::new();
    event.insert("t".to_string(), json!(timestamp));
    event.insert("k".to_string(), json!("meta"));
    event.insert("source".to_string(), codex_source(session_id));
    event.insert("coverage.tool".to_string(), json!(CODEX_COVERAGE_TOOL));
    event.insert("coverage.read".to_string(), json!(CODEX_COVERAGE_READ));
    event.insert("coverage.edit".to_string(), json!(CODEX_COVERAGE_EDIT));
    if model.is_some() {
        event.insert("model".to_string(), json!(model));
    }
    if repo_head.is_some() {
        event.insert("repo_head".to_string(), json!(repo_head));
    }
    Value::Object(event)
}

fn emit_tool_call(
    out: &mut Vec<Value>,
    calls: &mut HashMap<String, CodexCall>,
    timestamp: &str,
    session_id: Option<&str>,
    tool: &str,
    call_id: Option<&str>,
    args: &str,
) {
    if let Some(call_id) = call_id {
        calls.insert(
            call_id.to_string(),
            CodexCall {
                tool: tool.to_string(),
                args: args.to_string(),
            },
        );
    }

    let mut call_event = serde_json::Map::new();
    call_event.insert("t".to_string(), json!(timestamp));
    call_event.insert("k".to_string(), json!("tool.call"));
    call_event.insert("source".to_string(), codex_source(session_id));
    call_event.insert("tool".to_string(), json!(tool));
    call_event.insert("args".to_string(), json!(args));
    if let Some(call_id) = call_id {
        call_event.insert("call_id".to_string(), json!(call_id));
    }
    out.push(Value::Object(call_event));
}

fn value_to_argument_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

#[derive(Debug)]
struct CodexCall {
    tool: String,
    args: String,
}

fn patch_body(arguments: &str) -> String {
    let patch_body = serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("patch")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| arguments.to_string());
    patch_body
}

fn custom_tool_exit_code(output: &str) -> Option<i64> {
    serde_json::from_str::<Value>(output)
        .ok()?
        .get("metadata")?
        .get("exit_code")?
        .as_i64()
}

fn command_stdout(output: &str) -> &str {
    output
        .split_once("Final output:\n")
        .map(|(_, stdout)| stdout)
        .or_else(|| output.split_once("Output:\n").map(|(_, stdout)| stdout))
        .unwrap_or("")
}

fn emit_structured_after_result(
    out: &mut Vec<Value>,
    timestamp: &str,
    session_id: Option<&str>,
    cwd: Option<&str>,
    context: &CodexCall,
    output: &str,
) {
    if context.tool == "apply_patch" {
        for edit in parse_patch(&patch_body(&context.args)) {
            let mut event = serde_json::Map::new();
            event.insert("t".to_string(), json!(timestamp));
            event.insert("k".to_string(), json!("code.edit"));
            event.insert("source".to_string(), codex_source(session_id));
            event.insert("file".to_string(), json!(edit.file));
            if let Some(before) = edit.before_text {
                event.insert("before_text".to_string(), json!(before));
            }
            if let Some(after) = edit.after_text {
                event.insert("after_text".to_string(), json!(after));
            }
            out.push(Value::Object(event));
        }
    } else if context.tool == "exec_command"
        && let Ok(args) = serde_json::from_str::<Value>(&context.args)
        && let Some(command) = args.get("cmd").and_then(Value::as_str)
        && let Some(read) = bounded_shell_read(
            command,
            command_stdout(output),
            args.get("workdir").and_then(Value::as_str),
            cwd,
        )
    {
        out.push(json!({
            "t": timestamp,
            "k": "code.read",
            "source": codex_source(session_id),
            "file": read.file,
            "range": read.range,
            "text": read.text,
            "range_basis": "line"
        }));
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

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::codex_jsonl_to_tape_jsonl;

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
        assert!(!out.contains(r#""k":"code.edit""#), "out={out}");
    }

    #[test]
    fn codex_adapter_does_not_emit_code_edit_without_patch_file_headers() {
        let input = r#"{"timestamp":"2026-02-22T00:00:00Z","type":"session_meta","payload":{"model_provider":"openai"}}
{"timestamp":"2026-02-22T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"apply_patch","call_id":"call_1","arguments":"not a patch body"}}
{"timestamp":"2026-02-22T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"Done."}}"#;

        let out = codex_jsonl_to_tape_jsonl(input).expect("adapter should parse");
        let events: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON event"))
            .collect();

        assert_eq!(events[0]["k"], "meta");
        assert_eq!(events[0]["coverage.read"], "full");
        assert_eq!(events[0]["coverage.edit"], "partial");
        assert!(
            events.iter().all(|event| event["k"] != "code.edit"),
            "events={events:?}"
        );
    }

    #[test]
    fn codex_adapter_does_not_emit_custom_patch_without_paired_result() {
        let input = r#"{"timestamp":"2025-11-03T20:59:25.465Z","type":"session_meta","payload":{"id":"019a4b84-7c94-7783-a08b-fb4674e68b65","model_provider":"openai"}}
{"timestamp":"2025-11-03T20:59:25.465Z","type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"call_patch","name":"apply_patch","input":"*** Begin Patch\n*** Update File: Helm/Features/Chat/Components/ChatScrollContent.swift\n@@\n-import SwiftUI\n-import Foundation\n+import SwiftUI\n+import Foundation\n+import OSLog\n@@\n-struct ChatScrollContent: View {\n+struct ChatScrollContent: View {\n     let messageIDs: [UUID]\n     let screenGeometry: GeometryProxy\n     let screenHeight: CGFloat\n*** End Patch"}} "#;

        let out = codex_jsonl_to_tape_jsonl(input).expect("adapter should parse");
        let events: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON event"))
            .collect();

        assert!(events.iter().all(|event| event["k"] != "code.edit"));
    }
}
