use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::structured::{bounded_shell_read, is_absolute_path, parse_patch, patch_is_complete};

const CODEX_COVERAGE_TOOL: &str = "full";
const CODEX_COVERAGE_READ: &str = "partial";
const CODEX_COVERAGE_EDIT: &str = "partial";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct CodexState {
    calls: HashMap<String, CodexCall>,
    session_id: Option<String>,
    session_cwd: Option<String>,
    #[serde(default)]
    native_items: VecDeque<(String, Vec<u8>)>,
    #[serde(default)]
    native_calls: HashMap<String, HashSet<usize>>,
    #[serde(default)]
    fallback_edits: VecDeque<(String, Vec<String>)>,
    #[serde(default)]
    native_partial: bool,
}

pub fn codex_jsonl_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    codex_jsonl_incremental(input, &mut CodexState::default())
}

pub(crate) fn codex_jsonl_incremental(
    input: &str,
    state: &mut CodexState,
) -> Result<String, serde_json::Error> {
    let mut out = Vec::new();
    let mut calls = std::mem::take(&mut state.calls);
    let mut session_id = state.session_id.clone();
    let mut first_timestamp: Option<String> = None;
    let mut emitted_meta = false;
    let mut session_cwd = state.session_cwd.clone();
    let filechange_ids = input
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| {
            row["type"] == "event_msg"
                && row["payload"]["type"] == "item_completed"
                && row["payload"]["item"]["type"] == "FileChange"
        })
        .filter_map(|row| row["payload"]["item"]["id"].as_str().map(str::to_owned))
        .collect::<HashSet<_>>();

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
                            session_cwd.as_deref(),
                            payload
                                .and_then(|obj| {
                                    obj.get("internal_chat_message_metadata_passthrough")
                                })
                                .and_then(|value| value.get("turn_id"))
                                .and_then(Value::as_str),
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
                            session_cwd.as_deref(),
                            payload
                                .and_then(|obj| {
                                    obj.get("internal_chat_message_metadata_passthrough")
                                })
                                .and_then(|value| value.get("turn_id"))
                                .and_then(Value::as_str),
                        );
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let call_id = payload
                            .and_then(|obj| obj.get("call_id"))
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .map(ToOwned::to_owned);
                        let raw_output = payload
                            .and_then(|obj| obj.get("output"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        let output = content_text(&raw_output);
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
                        if !raw_output.is_string() {
                            result_event.insert("raw_output".to_string(), raw_output.clone());
                        }
                        let exit = match context.as_ref().map(|context| context.tool.as_str()) {
                            Some("exec") => context.as_ref().and_then(|call| {
                                let calls = nested_calls(&call.args)?;
                                nested_results(&raw_output, &calls).map(|_| 0)
                            }),
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
                        let native_indices = call_id
                            .as_ref()
                            .and_then(|id| state.native_calls.remove(id))
                            .unwrap_or_default();
                        if exit == Some(0)
                            && let Some(context) = context
                        {
                            if context.tool == "exec" {
                                if let Some(nested) = nested_calls(&context.args)
                                    && let Some(results) = nested_results(&raw_output, &nested)
                                {
                                    for (nested_index, (call, result)) in
                                        nested.iter().zip(results).enumerate()
                                    {
                                        let stdout = result["output"].as_str().unwrap_or_default();
                                        if native_indices.contains(&nested_index)
                                            && call.tool == "apply_patch"
                                        {
                                            continue;
                                        }
                                        let before = out.len();
                                        emit_structured_after_result(
                                            &mut out,
                                            timestamp,
                                            session_id.as_deref(),
                                            session_cwd.as_deref(),
                                            call,
                                            stdout,
                                        );
                                        remember_fallback_edits(
                                            state,
                                            context.turn_id.as_deref(),
                                            &out[before..],
                                        );
                                    }
                                }
                            } else {
                                let before = out.len();
                                if native_indices.contains(&0) && context.tool == "apply_patch" {
                                    continue;
                                }
                                emit_structured_after_result(
                                    &mut out,
                                    timestamp,
                                    session_id.as_deref(),
                                    session_cwd.as_deref(),
                                    &context,
                                    command_stdout(&output),
                                );
                                remember_fallback_edits(
                                    state,
                                    context.turn_id.as_deref(),
                                    &out[before..],
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            "event_msg" => {
                let payload = &row["payload"];
                if payload["type"] == "turn_context" {
                    if let Some(cwd) = payload["cwd"].as_str() {
                        session_cwd = Some(cwd.to_owned());
                    }
                }
                if payload["type"] == "item_completed" && payload["item"]["type"] == "FileChange" {
                    let item = &payload["item"];
                    let Some(id) = item["id"].as_str().filter(|id| !id.is_empty()) else {
                        state.native_partial = true;
                        continue;
                    };
                    emit_native_file_changes(
                        &mut out,
                        state,
                        &calls,
                        timestamp,
                        session_id.as_deref(),
                        item,
                        id,
                        payload["turn_id"].as_str(),
                    );
                }
                if payload["type"] == "patch_apply_end" {
                    let Some(id) = payload["call_id"].as_str().filter(|id| !id.is_empty()) else {
                        state.native_partial = true;
                        continue;
                    };
                    // When both native forms are in one input chunk, defer to the
                    // completed FileChange representation even if patch_apply_end
                    // appeared first in source order.
                    if filechange_ids.contains(id) {
                        continue;
                    }
                    let success = payload["success"].as_bool() == Some(true)
                        && payload["status"].as_str() == Some("completed");
                    let item = json!({
                        "status": payload["status"].as_str().unwrap_or_default(),
                        "success": success,
                        "stdout": payload["stdout"].as_str().unwrap_or_default(),
                        "stderr": payload["stderr"].as_str().unwrap_or_default(),
                        "changes": payload["changes"].clone()
                    });
                    emit_native_file_changes(
                        &mut out,
                        state,
                        &calls,
                        timestamp,
                        session_id.as_deref(),
                        &item,
                        id,
                        payload["turn_id"].as_str(),
                    );
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
    let (read_coverage, edit_coverage) = codex_coverage(&out, state.native_partial);
    for meta in out
        .iter_mut()
        .filter_map(Value::as_object_mut)
        .filter(|event| event.get("k").and_then(Value::as_str) == Some("meta"))
    {
        meta.insert("coverage.read".to_string(), json!(read_coverage));
        meta.insert("coverage.edit".to_string(), json!(edit_coverage));
    }

    state.calls = calls;
    state.session_id = session_id;
    state.session_cwd = session_cwd;
    to_jsonl(&out)
}

fn codex_coverage(events: &[Value], native_partial: bool) -> (&'static str, &'static str) {
    let mut read_partial = false;
    let mut edit_partial = native_partial;
    for (index, call) in events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["k"] == "tool.call")
    {
        let tool = call["tool"].as_str().unwrap_or("");
        if tool == "exec" {
            // No generalized JS interpretation: unknown nested operations cannot
            // justify full read/edit coverage, even when the outer script exits.
            read_partial = true;
            let paired = events.iter().skip(index + 1).find(|event| {
                event["k"] == "tool.result" && event.get("call_id") == call.get("call_id")
            });
            if nested_calls(call["args"].as_str().unwrap_or_default())
                .is_none_or(|calls| calls.iter().any(|call| call.tool == "exec_command"))
                || !paired.is_some_and(|event| event["exit"] == 0)
            {
                edit_partial = true;
            }
            continue;
        }
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
        let native_emitted = events
            .iter()
            .skip(index + 1)
            .take(result_index.saturating_sub(index + 1))
            .any(|event| {
                event["k"] == expected_kind && event.get("call_id") == call.get("call_id")
            });
        let emitted = events
            .iter()
            .skip(result_index + 1)
            .take_while(|event| !matches!(event["k"].as_str(), Some("tool.call" | "tool.result")))
            .find(|event| event["k"] == expected_kind);
        if emitted.is_none() && !native_emitted {
            if tool == "exec_command" {
                read_partial = true;
                edit_partial = true;
            } else {
                edit_partial = true;
            }
        } else if tool == "exec_command"
            && emitted
                .and_then(|event| event["file"].as_str())
                .is_some_and(|file| !is_absolute_path(file))
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

fn edit_paths(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event["k"] == "code.edit")
        .filter_map(|event| event["file"].as_str().map(str::to_owned))
        .collect()
}

fn remember_fallback_edits(state: &mut CodexState, turn_id: Option<&str>, events: &[Value]) {
    if let Some(turn_id) = turn_id {
        let paths = edit_paths(events);
        if !paths.is_empty() {
            state.fallback_edits.push_back((turn_id.to_owned(), paths));
            while state.fallback_edits.len() > 64 {
                state.fallback_edits.pop_front();
            }
        }
    }
}

fn native_change_digest(item: &Value) -> Vec<u8> {
    let status = item["status"].as_str().unwrap_or_default();
    let successful = item["success"].as_bool().unwrap_or(true)
        && status == "completed"
        && item["stderr"].as_str().is_some_and(str::is_empty)
        && item["stdout"]
            .as_str()
            .is_some_and(|text| text.starts_with("Success."));
    Sha256::digest(
        json!({"successful": successful, "changes": item["changes"]})
            .to_string()
            .as_bytes(),
    )
    .to_vec()
}

fn native_change_paths(item: &Value) -> Vec<String> {
    item["changes"]
        .as_object()
        .into_iter()
        .flat_map(|changes| changes.keys())
        .filter(|path| is_absolute_path(path))
        .cloned()
        .collect()
}

fn emit_native_file_changes(
    out: &mut Vec<Value>,
    state: &mut CodexState,
    calls: &HashMap<String, CodexCall>,
    timestamp: &str,
    session_id: Option<&str>,
    item: &Value,
    id: &str,
    turn_id: Option<&str>,
) {
    let digest = native_change_digest(item);
    if let Some((_, previous)) = state.native_items.iter().find(|(seen, _)| seen == id) {
        if previous != &digest {
            state.native_partial = true;
        }
        return;
    }

    match native_file_changes(item, timestamp, session_id) {
        Ok((mut edits, partial)) => {
            state.native_partial |= partial;
            state.native_items.push_back((id.to_owned(), digest));
            if state.native_items.len() > 512 {
                state.native_items.pop_front();
            }
            let paths = edit_paths(&edits);
            if let Some((call_id, index)) =
                match_native_call(calls, &state.native_calls, turn_id, &paths)
            {
                state
                    .native_calls
                    .entry(call_id.clone())
                    .or_default()
                    .insert(index);
                for edit in &mut edits {
                    edit["call_id"] = json!(call_id);
                }
            } else if turn_id.is_some() && !paths.is_empty() {
                // An unrelated equal-text edit must never be silently discarded.
                state.native_partial = true;
            }
            if let Some(turn_id) = turn_id {
                if state.fallback_edits.iter().any(|(seen_turn, seen_paths)| {
                    seen_turn == turn_id && paths.iter().any(|path| seen_paths.contains(path))
                }) {
                    state.native_partial = true;
                }
            }
            out.extend(edits);
        }
        Err(()) => {
            state.native_partial = true;
            // A failed or malformed structured operation must not turn its
            // request text into a guessed successful patch. Bind the recorded
            // operation where its absolute change paths permit that proof.
            let paths = native_change_paths(item);
            if let Some((call_id, index)) =
                match_native_call(calls, &state.native_calls, turn_id, &paths)
            {
                state.native_calls.entry(call_id).or_default().insert(index);
            }
        }
    }
}

fn match_native_call(
    calls: &HashMap<String, CodexCall>,
    used: &HashMap<String, HashSet<usize>>,
    turn_id: Option<&str>,
    paths: &[String],
) -> Option<(String, usize)> {
    let turn_id = turn_id?;
    if paths.is_empty() {
        return None;
    }
    let mut matching = Vec::new();
    for (call_id, call) in calls {
        if call.turn_id.as_deref() != Some(turn_id) {
            continue;
        }
        let operations = if call.tool == "exec" {
            let Some(operations) = nested_calls_for_native_binding(&call.args) else {
                continue;
            };
            operations
        } else {
            vec![call.clone()]
        };
        for (index, operation) in operations.iter().enumerate() {
            if operation.tool != "apply_patch"
                || used.get(call_id).is_some_and(|set| set.contains(&index))
            {
                continue;
            }
            let patch_paths =
                declared_patch_paths(&patch_body(&operation.args), call.cwd.as_deref());
            if paths
                .iter()
                .all(|path| patch_paths.contains(&PathBuf::from(path)))
            {
                matching.push((call_id.clone(), index));
            }
        }
    }
    (matching.len() == 1).then(|| matching.remove(0))
}

/// Header paths describe the operation even when a path-only delete has no
/// content for the legacy patch parser to emit. They are used only to bind a
/// completed native item to its recorded call, never as edit evidence.
fn declared_patch_paths(patch: &str, cwd: Option<&str>) -> Vec<PathBuf> {
    if !patch_has_envelope(patch) {
        return Vec::new();
    }
    patch
        .lines()
        .filter_map(|line| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
        })
        .filter_map(|raw| {
            let raw = raw.trim();
            if raw.is_empty() {
                return None;
            }
            if is_absolute_path(raw) {
                return Some(PathBuf::from(raw));
            }
            let cwd = cwd?;
            let base = Path::new(cwd);
            base.is_absolute().then(|| base.join(raw))
        })
        .collect()
}

fn patch_has_envelope(patch: &str) -> bool {
    patch.trim_start().starts_with("*** Begin Patch\n")
        && patch.trim_end().ends_with("*** End Patch")
}

fn patch_allowed(patch: &str, native_binding: bool) -> bool {
    if native_binding {
        patch_has_envelope(patch)
            && patch.lines().any(|line| {
                line.starts_with("*** Add File: ")
                    || line.starts_with("*** Update File: ")
                    || line.starts_with("*** Delete File: ")
            })
    } else {
        patch_is_complete(patch) && !parse_patch(patch).is_empty()
    }
}

fn native_file_changes(
    item: &Value,
    timestamp: &str,
    session_id: Option<&str>,
) -> Result<(Vec<Value>, bool), ()> {
    if item["status"] != "completed"
        || item
            .get("success")
            .is_some_and(|success| success.as_bool() != Some(true))
        || !item["stderr"].as_str().is_some_and(str::is_empty)
        || !item["stdout"]
            .as_str()
            .is_some_and(|text| text.starts_with("Success."))
    {
        return Err(());
    }
    let changes = item["changes"]
        .as_object()
        .filter(|changes| !changes.is_empty())
        .ok_or(())?;
    let mut events = Vec::new();
    let mut partial = false;
    for (path, change) in changes {
        if !is_absolute_path(path) {
            partial = true;
            continue;
        }
        let mut event = |file: &str,
                         before: Option<&str>,
                         after: Option<&str>,
                         before_range: Option<[u32; 2]>,
                         after_range: Option<[u32; 2]>| {
            let mut value = json!({"t":timestamp,"k":"code.edit","source":codex_source(session_id),"file":file});
            if let Some(before) = before {
                value["before_text"] = json!(before);
            }
            if let Some(after) = after {
                value["after_text"] = json!(after);
            }
            if let Some(range) = before_range {
                value["before_range"] = json!(range);
            }
            if let Some(range) = after_range {
                value["after_range"] = json!(range);
            }
            events.push(value);
        };
        match change["type"].as_str() {
            Some("add") => {
                if let Some(content) = change["content"].as_str() {
                    event(path, None, Some(content), None, None);
                } else {
                    partial = true;
                }
            }
            Some("update") if change["move_path"].is_null() => {
                if let Some(hunks) = change["unified_diff"].as_str().and_then(parse_native_hunks) {
                    for hunk in hunks {
                        event(
                            path,
                            Some(&hunk.before),
                            Some(&hunk.after),
                            hunk.before_range,
                            hunk.after_range,
                        );
                    }
                } else {
                    partial = true;
                }
            }
            Some("delete") => {
                if let Some(content) = change["content"].as_str() {
                    event(path, Some(content), None, None, None);
                } else if let Some(hunks) =
                    change["unified_diff"].as_str().and_then(parse_native_hunks)
                {
                    for hunk in hunks {
                        event(path, Some(&hunk.before), None, hunk.before_range, None);
                    }
                } else {
                    event(path, None, None, None, None);
                    partial = true;
                }
            }
            Some("move" | "update") => {
                if let Some(destination) = change["move_path"]
                    .as_str()
                    .filter(|path| is_absolute_path(path))
                {
                    if let Some(hunks) =
                        change["unified_diff"].as_str().and_then(parse_native_hunks)
                    {
                        for hunk in hunks {
                            event(path, Some(&hunk.before), None, hunk.before_range, None);
                            event(destination, None, Some(&hunk.after), None, hunk.after_range);
                        }
                    } else if let Some(content) = change["content"].as_str() {
                        event(path, None, None, None, None);
                        event(destination, None, Some(content), None, None);
                        partial = true;
                    } else {
                        event(path, None, None, None, None);
                        event(destination, None, None, None, None);
                        partial = true;
                    }
                } else {
                    partial = true;
                }
            }
            _ => partial = true,
        }
    }
    if events.is_empty() {
        Err(())
    } else {
        Ok((events, partial))
    }
}

struct NativeHunk {
    before_range: Option<[u32; 2]>,
    after_range: Option<[u32; 2]>,
    before: String,
    after: String,
}

fn parse_native_hunks(diff: &str) -> Option<Vec<NativeHunk>> {
    fn range(raw: &str) -> Option<(Option<[u32; 2]>, u32)> {
        let (start, count) = raw.split_once(',').unwrap_or((raw, "1"));
        let start = start.parse::<u32>().ok()?;
        let count = count.parse::<u32>().ok()?;
        let span = (count > 0)
            .then(|| start.checked_add(count - 1).map(|end| [start, end]))
            .flatten();
        Some((span, count))
    }
    let mut hunks = Vec::<NativeHunk>::new();
    let mut expected = (0u32, 0u32);
    let mut observed = (0u32, 0u32);
    let mut last = ' ';
    for line in diff.split_inclusive('\n') {
        let line = line.strip_suffix('\n').unwrap_or(line);
        if let Some(header) = line.strip_prefix("@@ ") {
            if !hunks.is_empty() && observed != expected {
                return None;
            }
            let mut parts = header.split_whitespace();
            let before = parts.next()?.strip_prefix('-')?;
            let after = parts.next()?.strip_prefix('+')?;
            let (before_range, before_count) = range(before)?;
            let (after_range, after_count) = range(after)?;
            hunks.push(NativeHunk {
                before_range,
                after_range,
                before: String::new(),
                after: String::new(),
            });
            expected = (before_count, after_count);
            observed = (0, 0);
            continue;
        }
        let Some(hunk) = hunks.last_mut() else {
            continue;
        };
        if line == "\\ No newline at end of file" {
            if matches!(last, ' ' | '-') {
                hunk.before.pop();
            }
            if matches!(last, ' ' | '+') {
                hunk.after.pop();
            }
            continue;
        }
        let Some(kind) = line.get(..1) else {
            return None;
        };
        let text = &line[1..];
        match kind {
            " " => {
                hunk.before.push_str(text);
                hunk.before.push('\n');
                hunk.after.push_str(text);
                hunk.after.push('\n');
                observed.0 += 1;
                observed.1 += 1;
                last = ' ';
            }
            "-" => {
                hunk.before.push_str(text);
                hunk.before.push('\n');
                observed.0 += 1;
                last = '-';
            }
            "+" => {
                hunk.after.push_str(text);
                hunk.after.push('\n');
                observed.1 += 1;
                last = '+';
            }
            _ => return None,
        }
    }
    (!hunks.is_empty() && observed == expected).then_some(hunks)
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
    cwd: Option<&str>,
    turn_id: Option<&str>,
) {
    if let Some(call_id) = call_id {
        calls.insert(
            call_id.to_string(),
            CodexCall {
                tool: tool.to_string(),
                args: args.to_string(),
                cwd: cwd.map(str::to_owned),
                turn_id: turn_id.map(str::to_owned),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexCall {
    tool: String,
    args: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
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

/// Exact straight-line wrapper only. Never evaluate JavaScript, infer execution
/// from mentioned source, or associate ambiguous result blocks with operations.
fn nested_calls(code: &str) -> Option<Vec<CodexCall>> {
    nested_calls_with_mode(code, false)
}

/// Native success can bind to an exact patch call whose declared paths are
/// known even when the legacy content parser cannot describe every edit.
fn nested_calls_for_native_binding(code: &str) -> Option<Vec<CodexCall>> {
    nested_calls_with_mode(code, true)
}

fn nested_calls_with_mode(code: &str, native_binding: bool) -> Option<Vec<CodexCall>> {
    let mut rest = code.trim();
    if rest.starts_with("// @exec:") {
        rest = rest.split_once('\n')?.1.trim_start();
    }
    if rest.starts_with("const ") {
        return assigned_apply_patch_result_call_with_mode(rest, native_binding)
            .or_else(|| assigned_apply_patch_call_with_mode(rest, native_binding))
            .or_else(|| bound_patch_literal_call_with_mode(rest, native_binding));
    }
    let mut calls = Vec::new();
    while !rest.is_empty() {
        rest = rest.strip_prefix("text(await tools.")?;
        let (tool, arguments) = rest.split_once('(')?;
        let (args, consumed) = match tool {
            "apply_patch" => {
                let mut values =
                    serde_json::Deserializer::from_str(arguments).into_iter::<String>();
                let patch = values.next()?.ok()?;
                if !patch_allowed(&patch, native_binding) {
                    return None;
                }
                (patch, values.byte_offset())
            }
            "exec_command" => literal_command_arguments(arguments)?,
            _ => return None,
        };
        rest = arguments[consumed..]
            .trim_start()
            .strip_prefix("));")?
            .trim_start();
        calls.push(CodexCall {
            tool: tool.into(),
            args,
            cwd: None,
            turn_id: None,
        });
    }
    (!calls.is_empty()).then_some(calls)
}

/// Accept the recorded wrapper that binds the apply_patch result before
/// printing it. This is syntax-only recognition of one literal patch and one
/// straight-line call; it never evaluates JavaScript.
fn assigned_apply_patch_result_call_with_mode(
    code: &str,
    native_binding: bool,
) -> Option<Vec<CodexCall>> {
    let rest = code.strip_prefix("const ")?;
    let (patch_binding, rest) = rest.split_once('=')?;
    let patch_binding = patch_binding.trim();
    if !is_ascii_identifier(patch_binding) {
        return None;
    }

    let literal = rest.trim_start();
    let mut values = serde_json::Deserializer::from_str(literal).into_iter::<String>();
    let patch = values.next()?.ok()?;
    if !patch_allowed(&patch, native_binding) {
        return None;
    }
    let rest = literal[values.byte_offset()..]
        .trim_start()
        .strip_prefix(';')?
        .trim_start()
        .strip_prefix("const ")?;
    let (result_binding, rest) = rest.split_once('=')?;
    let result_binding = result_binding.trim();
    if !is_ascii_identifier(result_binding) {
        return None;
    }
    let rest = rest.trim_start().strip_prefix("await tools.apply_patch(")?;
    let (argument, rest) = rest.split_once(");")?;
    if argument.trim() != patch_binding {
        return None;
    }
    let rest = rest.trim_start().strip_prefix("text(")?;
    let (printed, rest) = rest.split_once(");")?;
    if printed.trim() != result_binding || !rest.trim().is_empty() {
        return None;
    }

    Some(vec![CodexCall {
        tool: "apply_patch".into(),
        args: patch,
        cwd: None,
        turn_id: None,
    }])
}

/// The recorded code-mode form may name a literal patch before passing it to
/// apply_patch. Accept only this single, straight-line binding and call.
fn bound_patch_literal_call_with_mode(code: &str, native_binding: bool) -> Option<Vec<CodexCall>> {
    let rest = code.strip_prefix("const ")?;
    let (binding, rest) = rest.split_once('=')?;
    let binding = binding.trim();
    if !is_ascii_identifier(binding) {
        return None;
    }

    let literal = rest.trim_start();
    let mut values = serde_json::Deserializer::from_str(literal).into_iter::<String>();
    let patch = values.next()?.ok()?;
    if !patch_allowed(&patch, native_binding) {
        return None;
    }
    let rest = literal[values.byte_offset()..]
        .trim_start()
        .strip_prefix(';')?
        .trim_start()
        .strip_prefix("text(await tools.apply_patch(")?;
    let (argument, rest) = rest.split_once(')')?;
    if argument.trim() != binding || rest.trim_start().strip_prefix(");")?.trim().len() != 0 {
        return None;
    }

    Some(vec![CodexCall {
        tool: "apply_patch".into(),
        args: patch,
        cwd: None,
        turn_id: None,
    }])
}

/// Accept the recorded exec wrapper only when it binds one literal patch and
/// echoes that same result. This parses syntax; it never evaluates JavaScript.
fn assigned_apply_patch_call_with_mode(code: &str, native_binding: bool) -> Option<Vec<CodexCall>> {
    let rest = code.strip_prefix("const ")?;
    let (binding, rest) = rest.split_once('=')?;
    let binding = binding.trim();
    if !is_ascii_identifier(binding) {
        return None;
    }

    let arguments = rest.trim_start().strip_prefix("await tools.apply_patch(")?;
    let mut values = serde_json::Deserializer::from_str(arguments).into_iter::<String>();
    let patch = values.next()?.ok()?;
    if !patch_allowed(&patch, native_binding) {
        return None;
    }
    let rest = arguments[values.byte_offset()..]
        .trim_start()
        .strip_prefix(");")?
        .trim_start()
        .strip_prefix("text(")?;
    let (printed, rest) = rest.split_once(')')?;
    if printed.trim() != binding || rest.trim_start().strip_prefix(';')?.trim().len() != 0 {
        return None;
    }

    Some(vec![CodexCall {
        tool: "apply_patch".into(),
        args: patch,
        cwd: None,
        turn_id: None,
    }])
}

fn is_ascii_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c == '$' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

// JSON literal values and literal property names only; handles the recorded
// {cmd:"...",workdir:"..."} spelling, not expressions, spreads or JS strings.
fn literal_command_arguments(input: &str) -> Option<(String, usize)> {
    let mut rest = input.trim_start().strip_prefix('{')?.trim_start();
    let mut object = serde_json::Map::new();
    loop {
        if let Some(tail) = rest.strip_prefix('}') {
            if !object.get("cmd")?.is_string() {
                return None;
            }
            return Some((Value::Object(object).to_string(), input.len() - tail.len()));
        }
        let (key, tail) = if rest.starts_with('"') {
            let mut values = serde_json::Deserializer::from_str(rest).into_iter::<String>();
            let key = values.next()?.ok()?;
            (key, &rest[values.byte_offset()..])
        } else {
            let end = rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_')?;
            (rest[..end].to_string(), &rest[end..])
        };
        if !matches!(
            key.as_str(),
            "cmd"
                | "workdir"
                | "max_output_tokens"
                | "yield_time_ms"
                | "tty"
                | "login"
                | "sandbox_permissions"
                | "justification"
                | "prefix_rule"
        ) {
            return None;
        }
        rest = tail.trim_start().strip_prefix(':')?.trim_start();
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        let value = values.next()?.ok()?;
        if object.insert(key, value).is_some() {
            return None;
        }
        rest = rest[values.byte_offset()..].trim_start();
        if let Some(tail) = rest.strip_prefix(',') {
            rest = tail.trim_start();
        } else if !rest.starts_with('}') {
            return None;
        }
    }
}

/// Extract the JSON-literal arguments from the recorded assigned exec wrapper.
/// This recognizes syntax only; it never evaluates the tool-call source.
pub(crate) fn assigned_exec_command_arguments(code: &str) -> Option<String> {
    let rest = code.trim_start().strip_prefix("const ")?;
    let (binding, rest) = rest.split_once('=')?;
    let binding = binding.trim();
    if !is_ascii_identifier(binding) {
        return None;
    }

    let call_arguments = rest
        .trim_start()
        .strip_prefix("await tools.exec_command(")?;
    let (arguments, consumed) = literal_command_arguments(call_arguments)?;
    let rest = call_arguments[consumed..]
        .trim_start()
        .strip_prefix(");")?
        .trim_start();
    let rest = rest
        .strip_prefix("text(")?
        .strip_prefix(binding)?
        .strip_prefix(".output)")?
        .trim_start();
    rest.strip_prefix(';')?
        .trim()
        .is_empty()
        .then_some(arguments)
}

fn nested_results(output: &Value, calls: &[CodexCall]) -> Option<Vec<Value>> {
    let blocks = output.as_array()?;
    if blocks.len() != calls.len() + 1 || !blocks.iter().all(|b| b["type"] == "input_text") {
        return None;
    }
    let header = blocks[0]["text"].as_str()?;
    if !header.starts_with("Script completed\nWall time ") || !header.ends_with("\nOutput:\n") {
        return None;
    }
    calls
        .iter()
        .zip(&blocks[1..])
        .map(|(call, block)| {
            let value: Value = serde_json::from_str(block["text"].as_str()?).ok()?;
            match call.tool.as_str() {
                "apply_patch" if value == json!({}) => Some(value),
                "exec_command" if value["exit_code"] == 0 && value["output"].is_string() => {
                    Some(value)
                }
                _ => None,
            }
        })
        .collect()
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
    stdout: &str,
) {
    // Callers decode transport envelopes once; nested JSON output is already text.
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
            stdout,
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

    use super::{
        CodexState, assigned_exec_command_arguments, codex_jsonl_incremental,
        codex_jsonl_to_tape_jsonl, native_file_changes, nested_calls,
    };

    #[test]
    fn codex_filechange_add_precedes_paired_exec_fallback_even_across_append() {
        let patch =
            "*** Begin Patch\n*** Add File: /tmp/e2/fixture.txt\n+receiver edit\n*** End Patch\n";
        let code = format!(
            "const patch = {};\ntext(await tools.apply_patch(patch));",
            serde_json::to_string(patch).unwrap()
        );
        let call = serde_json::json!({"timestamp":"2026-09-29T03:12:09.460Z","type":"response_item","payload":{
            "type":"custom_tool_call","name":"exec","call_id":"call_E2","input":code,
            "internal_chat_message_metadata_passthrough":{"turn_id":"turn_E2"}
        }});
        let native = serde_json::json!({"timestamp":"2026-09-29T03:12:09.580Z","type":"event_msg","payload":{
            "type":"item_completed","turn_id":"turn_E2","item":{
                "type":"FileChange","id":"exec_E2","status":"completed",
                "changes":{"/tmp/e2/fixture.txt":{"type":"add","content":"receiver edit\n"}},
                "stdout":"Success. Updated the following files:\nA /tmp/e2/fixture.txt\n","stderr":""
            }
        }});
        let output = serde_json::json!({"timestamp":"2026-09-29T03:12:09.693Z","type":"response_item","payload":{
            "type":"custom_tool_call_output","call_id":"call_E2","output":[
                {"type":"input_text","text":"Script completed\nWall time 0.1s\nOutput:\n"},
                {"type":"input_text","text":"{}"}
            ]
        }});
        let mut state = CodexState::default();
        let first = codex_jsonl_incremental(&format!("{call}\n{native}\n"), &mut state).unwrap();
        let second = codex_jsonl_incremental(&format!("{native}\n{output}\n"), &mut state).unwrap();
        let events = format!("{first}{second}");
        let edits = events
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["k"] == "code.edit")
            .collect::<Vec<_>>();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["file"], "/tmp/e2/fixture.txt");
        assert_eq!(edits[0]["after_text"], "receiver edit\n");
    }

    #[test]
    fn codex_filechange_after_unbound_fallback_is_retained_as_partial() {
        let patch = "*** Begin Patch\n*** Add File: /tmp/late.txt\n+same edit\n*** End Patch\n";
        let call = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call","call_id":"call_late","name":"exec",
            "internal_chat_message_metadata_passthrough":{"turn_id":"turn_late"},
            "input":format!("text(await tools.apply_patch({}));", serde_json::to_string(patch).unwrap())
        }});
        let output = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call_output","call_id":"call_late","output":[
                {"type":"input_text","text":"Script completed\nWall time 0.1s\nOutput:\n"},
                {"type":"input_text","text":"{}"}
            ]
        }});
        let native = serde_json::json!({"type":"event_msg","payload":{
            "type":"item_completed","turn_id":"turn_late","item":{"type":"FileChange","id":"exec_late","status":"completed",
                "changes":{"/tmp/late.txt":{"type":"add","content":"same edit\n"}},
                "stdout":"Success. Updated the following files:\nA /tmp/late.txt\n","stderr":""}
        }});
        let mut state = CodexState::default();
        let first = codex_jsonl_incremental(&format!("{call}\n{output}\n"), &mut state).unwrap();
        let second = codex_jsonl_incremental(&format!("{native}\n"), &mut state).unwrap();
        let events = format!("{first}{second}");
        assert_eq!(
            events
                .lines()
                .filter(|line| line.contains("\"k\":\"code.edit\""))
                .count(),
            2
        );
        assert!(events.contains("\"coverage.edit\":\"partial\""));
    }

    #[test]
    fn codex_filechange_update_uses_turn_and_path_not_text_equality() {
        let patch =
            "*** Begin Patch\n*** Update File: /tmp/update.txt\n@@\n-old\n+new\n*** End Patch\n";
        let call = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call","call_id":"call_update","name":"exec",
            "input":format!("text(await tools.apply_patch({}));", serde_json::to_string(patch).unwrap()),
            "internal_chat_message_metadata_passthrough":{"turn_id":"turn_update"}
        }});
        let native = serde_json::json!({"type":"event_msg","payload":{
            "type":"item_completed","turn_id":"turn_update","item":{
                "type":"FileChange","id":"exec_update","status":"completed",
                "changes":{"/tmp/update.txt":{"type":"update","unified_diff":"@@ -1,2 +1,2 @@\n context\n-old\n+new\n"}},
                "stdout":"Success. Updated the following files:\nM /tmp/update.txt\n","stderr":""
            }
        }});
        let output = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call_output","call_id":"call_update","output":[
                {"type":"input_text","text":"Script completed\nWall time 0.1s\nOutput:\n"},
                {"type":"input_text","text":"{}"}
            ]
        }});
        let out =
            codex_jsonl_to_tape_jsonl(&[call, native, output].map(|v| v.to_string()).join("\n"))
                .unwrap();
        let edits = out
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["k"] == "code.edit")
            .collect::<Vec<_>>();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["before_text"], "context\nold\n");
        assert_eq!(edits[0]["call_id"], "call_update");
    }

    #[test]
    fn codex_filechange_only_suppresses_its_bound_nested_patch() {
        let first = "*** Begin Patch\n*** Add File: /tmp/first.txt\n+one\n*** End Patch\n";
        let second = "*** Begin Patch\n*** Add File: /tmp/second.txt\n+two\n*** End Patch\n";
        let code = format!(
            "text(await tools.apply_patch({})); text(await tools.apply_patch({}));",
            serde_json::to_string(first).unwrap(),
            serde_json::to_string(second).unwrap()
        );
        let call = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call","call_id":"call_multi","name":"exec","input":code,
            "internal_chat_message_metadata_passthrough":{"turn_id":"turn_multi"}
        }});
        let native = serde_json::json!({"type":"event_msg","payload":{
            "type":"item_completed","turn_id":"turn_multi","item":{
                "type":"FileChange","id":"exec_first","status":"completed",
                "changes":{"/tmp/first.txt":{"type":"add","content":"one\n"}},
                "stdout":"Success. Updated the following files:\nA /tmp/first.txt\n","stderr":""
            }
        }});
        let output = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call_output","call_id":"call_multi","output":[
                {"type":"input_text","text":"Script completed\nWall time 0.1s\nOutput:\n"},
                {"type":"input_text","text":"{}"},
                {"type":"input_text","text":"{}"}
            ]
        }});
        let out =
            codex_jsonl_to_tape_jsonl(&[call, native, output].map(|v| v.to_string()).join("\n"))
                .unwrap();
        let edits = out
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["k"] == "code.edit")
            .collect::<Vec<_>>();
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().any(|event| event["file"] == "/tmp/first.txt"));
        assert!(edits.iter().any(|event| event["file"] == "/tmp/second.txt"));
    }

    #[test]
    fn codex_filechange_relative_patch_binds_against_recorded_cwd() {
        let patch = "*** Begin Patch\n*** Add File: relative.txt\n+new\n*** End Patch\n";
        let cwd = if cfg!(windows) {
            r"C:\tmp\work"
        } else {
            "/tmp/work"
        };
        let native_path = if cfg!(windows) {
            r"C:\tmp\work\relative.txt"
        } else {
            "/tmp/work/relative.txt"
        };
        let meta = serde_json::json!({"type":"session_meta","payload":{"id":"session_relative","cwd":cwd}});
        let call = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call","call_id":"call_relative","name":"exec",
            "input":format!("text(await tools.apply_patch({}));", serde_json::to_string(patch).unwrap()),
            "internal_chat_message_metadata_passthrough":{"turn_id":"turn_relative"}
        }});
        let native = serde_json::json!({"type":"event_msg","payload":{
            "type":"item_completed","turn_id":"turn_relative","item":{
                "type":"FileChange","id":"exec_relative","status":"completed",
                "changes":{(native_path):{"type":"add","content":"new\n"}},
                "stdout":"Success. Updated the following files:\nA relative.txt\n","stderr":""
            }
        }});
        let output = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call_output","call_id":"call_relative","output":[
                {"type":"input_text","text":"Script completed\nWall time 0.1s\nOutput:\n"},
                {"type":"input_text","text":"{}"}
            ]
        }});
        let out = codex_jsonl_to_tape_jsonl(
            &[meta, call, native, output]
                .map(|v| v.to_string())
                .join("\n"),
        )
        .unwrap();
        let edits = out
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["k"] == "code.edit")
            .collect::<Vec<_>>();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["file"], native_path);
        assert_eq!(edits[0]["call_id"], "call_relative");
    }

    #[test]
    fn codex_filechange_path_only_delete_still_binds_its_patch() {
        let patch = "*** Begin Patch\n*** Update File: /tmp/edit.txt\n@@\n-old\n+new\n*** Delete File: /tmp/remove.txt\n*** End Patch\n";
        assert_eq!(super::declared_patch_paths(patch, None).len(), 2);
        let call = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call","call_id":"call_delete","name":"exec",
            "input":format!("text(await tools.apply_patch({}));", serde_json::to_string(patch).unwrap()),
            "internal_chat_message_metadata_passthrough":{"turn_id":"turn_delete"}
        }});
        let native = serde_json::json!({"type":"event_msg","payload":{
            "type":"item_completed","turn_id":"turn_delete","item":{
                "type":"FileChange","id":"exec_delete","status":"completed",
                "changes":{
                    "/tmp/edit.txt":{"type":"update","unified_diff":"@@ -1 +1 @@\n-old\n+new\n"},
                    "/tmp/remove.txt":{"type":"delete"}
                },
                "stdout":"Success. Updated the following files:\nM /tmp/edit.txt\nD /tmp/remove.txt\n","stderr":""
            }
        }});
        let output = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call_output","call_id":"call_delete","output":[
                {"type":"input_text","text":"Script completed\nWall time 0.1s\nOutput:\n"},
                {"type":"input_text","text":"{}"}
            ]
        }});
        let out =
            codex_jsonl_to_tape_jsonl(&[call, native, output].map(|v| v.to_string()).join("\n"))
                .unwrap();
        let edits = out
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["k"] == "code.edit")
            .collect::<Vec<_>>();
        assert_eq!(edits.len(), 2);
        assert!(
            edits.iter().all(|event| event["call_id"] == "call_delete"),
            "{edits:?}"
        );
        assert!(edits.iter().any(|event| event["file"] == "/tmp/remove.txt"));
    }

    #[test]
    fn codex_filechange_preserves_empty_add_and_multiple_hunks() {
        let item = serde_json::json!({
            "type":"FileChange","id":"hunks","status":"completed","stdout":"Success. Updated the following files:\n","stderr":"",
            "changes":{
                "/tmp/empty.txt":{"type":"add","content":""},
                "/tmp/hunks.txt":{"type":"update","unified_diff":"@@ -1 +1 @@\n-old\n+new\n@@ -8 +8 @@\n-last\n+next\n"}
            }
        });
        let (events, partial) = native_file_changes(&item, "", None).unwrap();
        assert!(!partial);
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .any(|e| e["file"] == "/tmp/empty.txt" && e["after_text"] == "")
        );
        assert!(events.iter().any(
            |e| e["file"] == "/tmp/hunks.txt" && e["before_range"] == serde_json::json!([8, 8])
        ));
    }

    #[test]
    fn codex_filechange_update_delete_and_move_keep_only_observed_fragments() {
        let item = serde_json::json!({
            "type":"FileChange","id":"multi","status":"completed","stdout":"Success. Updated the following files:\n", "stderr":"",
            "changes":{
                "/tmp/update.txt":{"type":"update","unified_diff":"@@ -2,2 +2,2 @@\n context\n-old\n+new\n"},
                "/tmp/delete.txt":{"type":"delete","content":"removed\n"},
                "/tmp/old.txt":{"type":"update","move_path":"/tmp/new.txt","unified_diff":"@@ -1 +1 @@\n-before\n+after\n"},
                "/tmp/path-only.txt":{"type":"delete"}
            }
        });
        let (events, partial) = native_file_changes(&item, "2026-09-29T00:00:00Z", None).unwrap();
        assert!(partial); // The path-only delete cannot supply content anchors.
        assert!(events.iter().any(|e| e["file"] == "/tmp/update.txt"
            && e["before_text"] == "context\nold\n"
            && e["after_text"] == "context\nnew\n"
            && e["before_range"] == serde_json::json!([2, 3])));
        assert!(events.iter().any(|e| e["file"] == "/tmp/delete.txt"
            && e["before_text"] == "removed\n"
            && e.get("after_text").is_none()));
        assert!(events.iter().any(|e| e["file"] == "/tmp/old.txt"
            && e["before_text"] == "before\n"
            && e.get("after_text").is_none()));
        assert!(events.iter().any(|e| e["file"] == "/tmp/new.txt"
            && e["after_text"] == "after\n"
            && e.get("before_text").is_none()));
        assert!(
            events
                .iter()
                .any(|e| e["file"] == "/tmp/path-only.txt" && e.get("before_text").is_none())
        );
    }

    #[test]
    fn codex_filechange_rejects_failure_and_malformed_success() {
        for item in [
            serde_json::json!({"type":"FileChange","id":"a","status":"failed","stdout":"Success.","stderr":"","changes":{"/tmp/a":{"type":"add","content":"x"}}}),
            serde_json::json!({"type":"FileChange","id":"a","status":"completed","stdout":"Success.","stderr":"conflict","changes":{"/tmp/a":{"type":"add","content":"x"}}}),
            serde_json::json!({"type":"FileChange","id":"a","status":"completed","stdout":"not success","stderr":"","changes":{"/tmp/a":{"type":"add","content":"x"}}}),
            serde_json::json!({"type":"FileChange","id":"a","status":"completed","stdout":"Success.","stderr":"","changes":{"/tmp/a":{"type":"update","unified_diff":"bad"}}}),
        ] {
            assert!(native_file_changes(&item, "", None).is_err());
        }
    }

    #[test]
    fn codex_filechange_failure_cannot_be_overridden_by_successful_wrapper_guess() {
        let patch = "*** Begin Patch\n*** Add File: /tmp/no-edit.txt\n+wrong\n*** End Patch\n";
        let call = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call","call_id":"call_fail","name":"exec",
            "internal_chat_message_metadata_passthrough":{"turn_id":"turn_fail"},
            "input":format!("text(await tools.apply_patch({}));", serde_json::to_string(patch).unwrap())
        }});
        let failed = serde_json::json!({"type":"event_msg","payload":{
            "type":"item_completed","turn_id":"turn_fail","item":{"type":"FileChange","id":"exec_fail","status":"failed",
                "changes":{"/tmp/no-edit.txt":{"type":"add","content":"wrong\n"}},
                "stdout":"Failed to apply patch","stderr":"conflict"}
        }});
        let output = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call_output","call_id":"call_fail","output":[
                {"type":"input_text","text":"Script completed\nWall time 0.1s\nOutput:\n"},
                {"type":"input_text","text":"{}"}
            ]
        }});
        let input = [call, failed, output].map(|row| row.to_string()).join("\n");
        let out = codex_jsonl_to_tape_jsonl(&input).unwrap();
        let events = out
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(events.iter().all(|event| event["k"] != "code.edit"));
        assert_eq!(events[0]["coverage.edit"], "partial");
    }

    #[test]
    fn assigned_exec_argument_projection_accepts_only_literal_wrapper() {
        let code = r#"const r = await tools.exec_command({cmd:"echo marker",workdir:"/tmp"}); text(r.output);"#;
        let arguments = assigned_exec_command_arguments(code).expect("literal exec wrapper");
        let arguments: Value = serde_json::from_str(&arguments).expect("normalized arguments");
        assert_eq!(arguments["cmd"], "echo marker");
        assert_eq!(arguments["workdir"], "/tmp");

        assert!(
            assigned_exec_command_arguments(
                "const r = await tools.exec_command({cmd:command}); text(r.output);"
            )
            .is_none()
        );
        assert!(
            assigned_exec_command_arguments(
                r#"const r = await tools.exec_command({cmd:"echo marker"}); text(other.output);"#
            )
            .is_none()
        );
    }

    #[test]
    fn codex_adapter_emits_patch_from_recorded_assigned_exec_wrapper() {
        let input = r#"{"timestamp":"2026-08-22T09:17:27.500Z","type":"session_meta","payload":{"id":"01a028d3-ebd5-7f83-ac3c-8084d8fd686a","model_provider":"openai"}}
{"timestamp":"2026-09-28T09:17:27.523Z","type":"response_item","payload":{"type":"custom_tool_call","id":"ctc_05c582b826bb8eb6016aba30a59fd087d288859b23ca842429","status":"completed","call_id":"call_DRmC7XHFZMYY0GQh8AjnGuOi","name":"exec","input":"const r = await tools.apply_patch(\"*** Begin Patch\\n*** Add File: /Users/mike/.tightbeam/work/d009fb9f2357/e1-lineage-fixture-1e5d708e/scratch/e1-lineage-1e5d708e.txt\\n+e1-lineage-1e5d708e-8e69-469a-99f3-e5b95a13f7ef\\n*** End Patch\");\ntext(r);\n"}}
{"timestamp":"2026-09-28T09:17:27.611Z","type":"response_item","payload":{"type":"custom_tool_call_output","id":"ctco_01a0e74e-0ebb-7271-9450-e31838c1b72c","call_id":"call_DRmC7XHFZMYY0GQh8AjnGuOi","output":[{"type":"input_text","text":"Script completed\nWall time 0.0s\nOutput:\n"},{"type":"input_text","text":"{}"}]}}
"#;

        let out = codex_jsonl_to_tape_jsonl(input).expect("adapter should parse");
        let events: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON event"))
            .collect();

        let meta = events.iter().find(|event| event["k"] == "meta").unwrap();
        assert_eq!(meta["coverage.edit"], "full");
        let edit = events
            .iter()
            .find(|event| event["k"] == "code.edit")
            .unwrap();
        assert_eq!(
            edit["file"],
            "/Users/mike/.tightbeam/work/d009fb9f2357/e1-lineage-fixture-1e5d708e/scratch/e1-lineage-1e5d708e.txt"
        );
        assert_eq!(
            edit["after_text"],
            "e1-lineage-1e5d708e-8e69-469a-99f3-e5b95a13f7ef\n"
        );
        assert_eq!(meta["source"], serde_json::json!({"harness": "codex-cli"}));
        assert_eq!(edit["source"], serde_json::json!({"harness": "codex-cli"}));
        for raw in events
            .iter()
            .filter(|event| matches!(event["k"].as_str(), Some("tool.call" | "tool.result")))
        {
            assert_eq!(raw["source"], serde_json::json!({"harness": "codex-cli"}));
        }
    }

    #[test]
    fn codex_adapter_refuses_nonliteral_or_mismatched_assigned_patch_wrapper() {
        let input = r#"{"timestamp":"2026-09-28T09:17:27.523Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"call_unresolved","name":"exec","input":"const r = await tools.apply_patch(patch); text(r);"}}
{"timestamp":"2026-09-28T09:17:27.611Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_unresolved","output":[{"type":"input_text","text":"Script completed\nWall time 0.0s\nOutput:\n"},{"type":"input_text","text":"{}"}]}}
{"timestamp":"2026-09-28T09:18:27.523Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"call_mismatched","name":"exec","input":"const r = await tools.apply_patch(\"*** Begin Patch\\n*** Add File: file.txt\\n+line\\n*** End Patch\"); text(other);"}}
{"timestamp":"2026-09-28T09:18:27.611Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_mismatched","output":[{"type":"input_text","text":"Script completed\nWall time 0.0s\nOutput:\n"},{"type":"input_text","text":"{}"}]}}"#;

        let out = codex_jsonl_to_tape_jsonl(input).expect("adapter should parse");
        let events: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON event"))
            .collect();

        assert_eq!(events[0]["coverage.edit"], "partial");
        assert!(events.iter().all(|event| event["k"] != "code.edit"));
    }

    #[test]
    fn codex_adapter_recognizes_bound_literal_patch_in_exec() {
        let patch =
            "*** Begin Patch\n*** Add File: /tmp/e2/fixture.txt\n+receiver edit\n*** End Patch\n";
        let code = format!(
            "const patch = {}; text(await tools.apply_patch(patch));",
            serde_json::to_string(patch).unwrap()
        );
        let input = [
            serde_json::json!({"type":"response_item","payload":{
                "type":"custom_tool_call","name":"exec","call_id":"edit","input":code
            }}),
            serde_json::json!({"type":"response_item","payload":{
                "type":"custom_tool_call_output","call_id":"edit","output":[
                    {"type":"input_text","text":"Script completed\nWall time 0.1s\nOutput:\n"},
                    {"type":"input_text","text":"{}"}
                ]
            }}),
        ]
        .map(|value| value.to_string())
        .join("\n");
        let out = codex_jsonl_to_tape_jsonl(&input).unwrap();
        let events: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events[0]["coverage.edit"], "full");
        let edit = events
            .iter()
            .find(|event| event["k"] == "code.edit")
            .unwrap();
        assert_eq!(edit["file"], "/tmp/e2/fixture.txt");
        assert_eq!(edit["after_text"], "receiver edit\n");

        for rejected in [
            format!(
                "const other = {}; text(await tools.apply_patch(patch));",
                serde_json::to_string(patch).unwrap()
            ),
            format!(
                "const patch = {}; text(await tools.apply_patch(other));",
                serde_json::to_string(patch).unwrap()
            ),
            format!(
                "const patch = {}; extra(); text(await tools.apply_patch(patch));",
                serde_json::to_string(patch).unwrap()
            ),
            "const patch = `${value}`; text(await tools.apply_patch(patch));".into(),
        ] {
            assert!(nested_calls(&rejected).is_none(), "accepted {rejected}");
        }
    }

    #[test]
    fn codex_adapter_refuses_ambiguous_assigned_patch_result() {
        let patch = "*** Begin Patch\n*** Add File: file.txt\n+line\n*** End Patch\n";
        let input = format!(
            "const r = await tools.apply_patch({});\ntext(r);\n",
            serde_json::to_string(patch).unwrap()
        );
        let raw = [
            serde_json::json!({
                "type":"response_item",
                "payload":{"type":"custom_tool_call","name":"exec","call_id":"p","input":input}
            }),
            serde_json::json!({
                "type":"response_item",
                "payload":{"type":"custom_tool_call_output","call_id":"p","output":[
                    {"type":"input_text","text":"Script completed\nWall Time 0.0s\nOutput:\n"},
                    {"type":"input_text","text":"{}"},
                    {"type":"input_text","text":"{}"}
                ]}
            }),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");

        let out = codex_jsonl_to_tape_jsonl(&raw).expect("adapter should parse");
        let events: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON event"))
            .collect();

        assert_eq!(events[0]["coverage.edit"], "partial");
        assert!(events.iter().all(|event| event["k"] != "code.edit"));
    }

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

    #[test]
    fn codex_real_patch_apply_end_add_emits_structured_edit() {
        let patch = "*** Begin Patch\n*** Add File: /Users/mike/.tightbeam/work/d009fb9f2357/e3-three-host-endpoint-20260929.txt\n+e3-three-host-endpoint-20260929\n*** End Patch";
        let call = serde_json::json!({
            "timestamp":"2026-09-29T08:44:22.604Z",
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "call_id":"call_hY8QXqvjPUQJpqBNHfOT8CHx",
                "name":"exec",
                "input":format!(
                    "const patch = {};\nconst result = await tools.apply_patch(patch);\ntext(result);\n",
                    serde_json::to_string(patch).unwrap()
                ),
                "internal_chat_message_metadata_passthrough":{"turn_id":"turn_e3"}
            }
        });
        let native = serde_json::json!({
            "timestamp":"2026-09-29T08:44:22.645Z",
            "type":"event_msg",
            "payload":{
                "type":"patch_apply_end",
                "call_id":"exec-2b58799a-5969-491a-af28-b95497286561",
                "turn_id":"turn_e3",
                "stdout":"Success. Updated the following files:\nA /Users/mike/.tightbeam/work/d009fb9f2357/e3-three-host-endpoint-20260929.txt\n",
                "stderr":"",
                "success":true,
                "changes":{
                    "/Users/mike/.tightbeam/work/d009fb9f2357/e3-three-host-endpoint-20260929.txt":{
                        "type":"add",
                        "content":"e3-three-host-endpoint-20260929\n"
                    }
                },
                "status":"completed"
            }
        });
        let output = serde_json::json!({
            "timestamp":"2026-09-29T08:44:22.688Z",
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":"call_hY8QXqvjPUQJpqBNHfOT8CHx",
                "output":[
                    {"type":"input_text","text":"Script completed\nWall time 0.0s\nOutput:\n"},
                    {"type":"input_text","text":"{}"}
                ]
            }
        });
        let input = [call, native, output]
            .into_iter()
            .map(|row| row.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let out = codex_jsonl_to_tape_jsonl(&input).expect("adapter should parse");
        let edits = out
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["k"] == "code.edit")
            .collect::<Vec<_>>();

        assert_eq!(edits.len(), 1, "events={out}");
        assert_eq!(
            edits[0]["file"],
            "/Users/mike/.tightbeam/work/d009fb9f2357/e3-three-host-endpoint-20260929.txt"
        );
        assert_eq!(edits[0]["after_text"], "e3-three-host-endpoint-20260929\n");
        assert_eq!(edits[0]["call_id"], "call_hY8QXqvjPUQJpqBNHfOT8CHx");
    }

    #[test]
    fn codex_noncompleted_patch_apply_end_blocks_wrapper_edit_guess() {
        let patch = "*** Begin Patch\n*** Add File: /tmp/failed-native.txt\n+wrong\n*** End Patch";
        let call = serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "call_id":"call_failed_native",
                "name":"exec",
                "input":format!(
                    "const patch = {};\nconst result = await tools.apply_patch(patch);\ntext(result);",
                    serde_json::to_string(patch).unwrap()
                ),
                "internal_chat_message_metadata_passthrough":{"turn_id":"turn_failed_native"}
            }
        });
        let failed = serde_json::json!({
            "type":"event_msg",
            "payload":{
                "type":"patch_apply_end",
                "call_id":"exec-failed-native",
                "turn_id":"turn_failed_native",
                "stdout":"Success. Updated the following files:\nA /tmp/failed-native.txt\n",
                "stderr":"",
                "success":true,
                "changes":{
                    "/tmp/failed-native.txt":{"type":"add","content":"wrong\n"}
                },
                "status":"canceled"
            }
        });
        let output = serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":"call_failed_native",
                "output":[
                    {"type":"input_text","text":"Script completed\nWall time 0.0s\nOutput:\n"},
                    {"type":"input_text","text":"{}"}
                ]
            }
        });
        let input = [call, failed, output].map(|row| row.to_string()).join("\n");
        let out = codex_jsonl_to_tape_jsonl(&input).expect("adapter should parse");
        let events = out
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();

        assert!(events.iter().all(|event| event["k"] != "code.edit"));
        assert_eq!(events[0]["coverage.edit"], "partial");
    }

    #[test]
    fn codex_filechange_has_priority_over_same_identity_patch_apply_end_and_duplicates() {
        let patch =
            "*** Begin Patch\n*** Add File: /tmp/authoritative.txt\n+wrapper\n*** End Patch";
        let call = serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "call_id":"call_authoritative",
                "name":"exec",
                "input":format!(
                    "const patch = {};\nconst result = await tools.apply_patch(patch);\ntext(result);",
                    serde_json::to_string(patch).unwrap()
                ),
                "internal_chat_message_metadata_passthrough":{"turn_id":"turn_authoritative"}
            }
        });
        let patch_event = serde_json::json!({
            "type":"event_msg",
            "payload":{
                "type":"patch_apply_end",
                "call_id":"exec-authoritative",
                "turn_id":"turn_authoritative",
                "stdout":"Success. Updated the following files:\nA /tmp/authoritative.txt\n",
                "stderr":"",
                "success":true,
                "changes":{
                    "/tmp/authoritative.txt":{"type":"add","content":"wrapper\n"}
                },
                "status":"completed"
            }
        });
        let filechange = serde_json::json!({
            "type":"event_msg",
            "payload":{
                "type":"item_completed",
                "turn_id":"turn_authoritative",
                "item":{
                    "type":"FileChange",
                    "id":"exec-authoritative",
                    "status":"completed",
                    "changes":{
                        "/tmp/authoritative.txt":{"type":"add","content":"authoritative\n"}
                    },
                    "stdout":"Success. Updated the following files:\nA /tmp/authoritative.txt\n",
                    "stderr":""
                }
            }
        });
        let duplicate = filechange.clone();
        let output = serde_json::json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":"call_authoritative",
                "output":[
                    {"type":"input_text","text":"Script completed\nWall time 0.0s\nOutput:\n"},
                    {"type":"input_text","text":"{}"}
                ]
            }
        });
        let input = [call, patch_event, filechange, duplicate, output]
            .into_iter()
            .map(|row| row.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let out = codex_jsonl_to_tape_jsonl(&input).expect("adapter should parse");
        let edits = out
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["k"] == "code.edit")
            .collect::<Vec<_>>();

        assert_eq!(edits.len(), 1, "events={out}");
        assert_eq!(edits[0]["after_text"], "authoritative\n");
        assert_eq!(edits[0]["call_id"], "call_authoritative");
    }
}
