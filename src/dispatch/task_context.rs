//! Conservative projection of retained Tightbeam operations into compact task
//! identity rows. This only recognizes literal, single-command executions and
//! pairs them with their recorded successful result; it never evaluates shell
//! or JavaScript and never treats prose mentions as relationships.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::index::{SqliteIndex, TaskContextEvent, TaskContextKind, TaskIdKind};
use crate::query::format::{DateFilter, EventTimeDecision};
use crate::store::tapes::{parse_jsonl_rows, read_tape_content, resolve_tape_path};
use crate::tape::adapters::codex::literal_exec_command_arguments;
use crate::{CliError, RuntimeContext};

const MAX_TASK_CONTEXT_EVENTS: usize = 64;
const MAX_TASK_ANCESTRY_DEPTH: usize = 4;
const MAX_TASK_ANCESTRY_BRANCHES: usize = 4;
const TASK_CONTEXT_MESSAGE_COUNT: usize = 2;
const TASK_CONTEXT_MESSAGE_CHARS: usize = 480;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Operation {
    kind: OperationKind,
    work_item_id: Option<String>,
    expected_recipient: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationKind {
    WorkItemCreate,
    AssignmentCreate,
    AssignmentDispatch,
}

/// Extract task-operation evidence from an already normalized transcript.
/// The event offsets match `parse_jsonl_events`: zero-based nonblank source
/// line indices, including gaps left by blank lines.
pub fn extract_task_context_events(normalized: &str) -> Vec<TaskContextEvent> {
    let rows = normalized
        .lines()
        .enumerate()
        .filter_map(|(offset, line)| {
            serde_json::from_str::<Value>(line)
                .ok()
                .map(|row| (offset as u64, row))
        })
        .collect::<Vec<_>>();
    let mut out = Vec::new();

    for (call_index, (call_offset, call)) in rows.iter().enumerate() {
        if call["k"] == "msg.in" {
            if let Some(event) = assignment_receipt_event(*call_offset, call) {
                out.push(event);
            }
            continue;
        }
        if call["k"] != "tool.call" {
            continue;
        }
        let Some(call_id) = nonempty_string(&call["call_id"]) else {
            continue;
        };
        let Some(operation) = operation_from_call(call) else {
            continue;
        };
        let Some((result_offset, result)) = rows
            .iter()
            .skip(call_index + 1)
            .find(|(_, row)| row["k"] == "tool.result" && row["call_id"].as_str() == Some(call_id))
        else {
            continue;
        };
        let Some(stdout) = successful_command_stdout(call, result, &operation) else {
            continue;
        };
        let Some(result_timestamp) = nonempty_string(&result["t"]) else {
            continue;
        };
        let Some(result_json) = serde_json::from_str::<Value>(stdout.trim()).ok() else {
            continue;
        };
        if let Some(event) = event_from_result(
            call,
            &operation,
            &result_json,
            *call_offset,
            Some(*result_offset),
            result_timestamp,
            call_id,
        ) {
            out.push(event);
        }
    }

    out.sort_by(|left, right| {
        left.event_offset
            .cmp(&right.event_offset)
            .then_with(|| left.event_identity.cmp(&right.event_identity))
    });
    out
}

fn operation_from_call(call: &Value) -> Option<Operation> {
    let args = call["args"].as_str()?;
    let tool = call["tool"].as_str()?;
    let command = if tool == "exec" {
        let json_args = literal_exec_command_arguments(args)?;
        serde_json::from_str::<Value>(&json_args).ok()?["cmd"]
            .as_str()?
            .to_owned()
    } else if tool == "exec_command" {
        serde_json::from_str::<Value>(args).ok()?["cmd"]
            .as_str()?
            .to_owned()
    } else {
        return None;
    };
    parse_operation_command(&command)
}

fn parse_operation_command(command: &str) -> Option<Operation> {
    let words = split_literal_shell_words(command)?;
    if words.first().map(String::as_str) != Some("tightbeam") {
        return None;
    }
    let kind = match words.get(1)?.as_str() {
        "work-item-create" => OperationKind::WorkItemCreate,
        "assign" => OperationKind::AssignmentCreate,
        "dispatch" => OperationKind::AssignmentDispatch,
        _ => return None,
    };

    let allowed = match kind {
        OperationKind::WorkItemCreate => &[
            "--title",
            "--spec-ref",
            "--spec-sha256",
            "--key",
            "--as",
            "--as-user",
        ][..],
        OperationKind::AssignmentCreate => &[
            "--subject",
            "--session",
            "--role",
            "--key",
            "--work-item",
            "--reviews",
            "--effect-kind",
            "--files",
            "--as",
            "--as-user",
        ][..],
        OperationKind::AssignmentDispatch => &[
            "--to",
            "--holder",
            "--subject",
            "--brief",
            "--work-item",
            "--effect-kind",
            "--workdir-root",
            "--key",
            "--as",
            "--as-user",
        ][..],
    };
    let mut work_item_id = None;
    let mut expected_recipient = None;
    let mut seen = std::collections::HashSet::new();
    let mut index = 2;
    while index < words.len() {
        let option = words[index].as_str();
        if !allowed.contains(&option) || !seen.insert(option) {
            return None;
        }
        let value = words.get(index + 1)?;
        if value.starts_with('-') && option != "--title" && option != "--subject" {
            return None;
        }
        match option {
            "--work-item" => {
                if !valid_typed_id(value, "wi_") {
                    return None;
                }
                work_item_id = Some(value.clone());
            }
            "--to" | "--holder" | "--session" => expected_recipient = Some(value.clone()),
            _ => {}
        }
        index += 2;
    }
    Some(Operation {
        kind,
        work_item_id,
        expected_recipient,
    })
}

/// A deliberately small shell lexer. Shell expansion and command composition
/// are rejected; quoting only groups literal argument bytes.
fn split_literal_shell_words(input: &str) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        Single,
        Double,
    }
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    let mut word = String::new();
    let mut words = Vec::new();
    for ch in input.chars() {
        if escaped {
            word.push(ch);
            started = true;
            escaped = false;
            continue;
        }
        match quote {
            Some(Quote::Single) => {
                if ch == '\'' {
                    quote = None;
                } else {
                    word.push(ch);
                }
            }
            Some(Quote::Double) => match ch {
                '"' => quote = None,
                '$' | '`' => return None,
                '\\' => escaped = true,
                _ => word.push(ch),
            },
            None => match ch {
                '\'' => {
                    quote = Some(Quote::Single);
                    started = true;
                }
                '"' => {
                    quote = Some(Quote::Double);
                    started = true;
                }
                '\\' => {
                    escaped = true;
                    started = true;
                }
                '$' | '`' | ';' | '|' | '&' | '<' | '>' | '\n' | '\r' | '#' => return None,
                ch if ch.is_whitespace() => {
                    if started {
                        words.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                _ => {
                    word.push(ch);
                    started = true;
                }
            },
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

fn successful_command_stdout(
    call: &Value,
    result: &Value,
    operation: &Operation,
) -> Option<String> {
    let tool = call["tool"].as_str()?;
    if tool == "exec_command" {
        if result["exit"].as_i64()? != 0 {
            return None;
        }
        return result["stdout"].as_str().map(str::to_owned);
    }
    if tool != "exec" {
        return None;
    }
    let blocks = result["raw_output"].as_array()?;
    if blocks.len() != 2 || blocks[0]["type"] != "input_text" || blocks[1]["type"] != "input_text" {
        return None;
    }
    let header = blocks[0]["text"].as_str()?;
    if !header.starts_with("Script completed\nWall time ") || !header.ends_with("\nOutput:\n") {
        return None;
    }
    let returned_text = blocks[1]["text"].as_str()?;
    let nested: Value = serde_json::from_str(returned_text).ok()?;
    if nested["exit_code"].as_i64() == Some(0) {
        return nested["output"].as_str().map(str::to_owned);
    }
    task_result_is_explicit_success(operation.kind, &nested).then(|| returned_text.to_owned())
}

fn task_result_is_explicit_success(operation: OperationKind, result: &Value) -> bool {
    if result["ruminationRequired"] == true || result["success"] == false {
        return false;
    }
    match operation {
        OperationKind::WorkItemCreate => {
            let item = result.get("workItem").or_else(|| result.get("item"));
            item.and_then(|item| nonempty_string(&item["id"]))
                .is_some_and(|id| valid_typed_id(id, "wi_"))
                && (result["success"] == true || item.is_some_and(|item| item["state"] == "open"))
        }
        OperationKind::AssignmentCreate | OperationKind::AssignmentDispatch => {
            let assignment = result.get("assignment").unwrap_or(result);
            nonempty_string(&assignment["id"]).is_some_and(|id| valid_typed_id(id, "asg_"))
                && (result["success"] == true || assignment["state"] == "open")
        }
    }
}

fn event_from_result(
    call: &Value,
    operation: &Operation,
    result: &Value,
    event_offset: u64,
    result_offset: Option<u64>,
    result_timestamp: &str,
    event_identity: &str,
) -> Option<TaskContextEvent> {
    let source_session = call["source"]["session_id"].as_str().map(str::to_owned);
    let event_timestamp = nonempty_string(&call["t"])?.to_owned();
    match operation.kind {
        OperationKind::WorkItemCreate => {
            if !task_result_is_explicit_success(operation.kind, result) {
                return None;
            }
            let item = result.get("workItem").or_else(|| result.get("item"))?;
            let work_item_id = nonempty_string(&item["id"])?;
            if !valid_typed_id(work_item_id, "wi_") {
                return None;
            }
            Some(TaskContextEvent {
                event_identity: event_identity.to_owned(),
                event_offset,
                result_offset,
                kind: TaskContextKind::WorkItemCreate,
                task_id_kind: TaskIdKind::WorkItem,
                task_id: work_item_id.to_owned(),
                work_item_id: Some(work_item_id.to_owned()),
                actor_session: source_session,
                recipient_session: None,
                timestamp: result_timestamp.to_owned(),
                event_timestamp,
            })
        }
        OperationKind::AssignmentCreate | OperationKind::AssignmentDispatch => {
            if !task_result_is_explicit_success(operation.kind, result) {
                return None;
            }
            let assignment = result.get("assignment").unwrap_or(result);
            let assignment_id = nonempty_string(&assignment["id"])?;
            if !valid_typed_id(assignment_id, "asg_") {
                return None;
            }
            let returned_work_item = nonempty_string(&assignment["workItemId"]);
            if let (Some(expected), Some(actual)) =
                (operation.work_item_id.as_deref(), returned_work_item)
                && expected != actual
            {
                return None;
            }
            let work_item_id = returned_work_item
                .or(operation.work_item_id.as_deref())
                .map(str::to_owned);
            let opened_by = nonempty_string(&assignment["openedBySession"]);
            let actor_session = opened_by.map(str::to_owned).or(source_session);
            let recipient_session = nonempty_string(&assignment["holderKey"]).map(str::to_owned);
            if let (Some(expected), Some(actual)) = (
                operation.expected_recipient.as_deref(),
                recipient_session.as_deref(),
            ) && expected != actual
            {
                return None;
            }
            let kind = match operation.kind {
                OperationKind::AssignmentCreate => TaskContextKind::AssignmentCreate,
                OperationKind::AssignmentDispatch => TaskContextKind::AssignmentDispatch,
                OperationKind::WorkItemCreate => unreachable!(),
            };
            Some(TaskContextEvent {
                event_identity: event_identity.to_owned(),
                event_offset,
                result_offset,
                kind,
                task_id_kind: TaskIdKind::Assignment,
                task_id: assignment_id.to_owned(),
                work_item_id,
                actor_session,
                recipient_session,
                timestamp: result_timestamp.to_owned(),
                event_timestamp,
            })
        }
    }
}

fn assignment_receipt_event(event_offset: u64, row: &Value) -> Option<TaskContextEvent> {
    let content = row["content"].as_str()?;
    let mut lines = content.lines();
    let sender_line = lines.next()?.trim();
    if !sender_line.starts_with("[from ") || !sender_line.ends_with(']') {
        return None;
    }
    let marker = "[assignment: ";
    let mut assignment_id = None;
    if let Some((_, after_sender)) = sender_line.split_once(']') {
        let after_sender = after_sender.trim();
        if let Some(value) = after_sender
            .strip_prefix(marker)
            .and_then(|value| value.strip_suffix(']'))
        {
            assignment_id = Some(value.trim());
        }
    }
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !line.starts_with('[') || !line.ends_with(']') {
            break;
        }
        if let Some(value) = line
            .strip_prefix(marker)
            .and_then(|value| value.strip_suffix(']'))
        {
            if assignment_id.is_some() {
                return None;
            }
            assignment_id = Some(value.trim());
        }
    }
    let assignment_id = assignment_id?;
    if !valid_typed_id(assignment_id, "asg_") {
        return None;
    }
    let recipient_session = nonempty_string(&row["source"]["session_id"])?.to_owned();
    let timestamp = nonempty_string(&row["t"])?.to_owned();
    Some(TaskContextEvent {
        event_identity: format!("assignment-envelope:{event_offset}"),
        event_offset,
        result_offset: None,
        kind: TaskContextKind::AssignmentReceipt,
        task_id_kind: TaskIdKind::Assignment,
        task_id: assignment_id.to_owned(),
        work_item_id: None,
        actor_session: None,
        recipient_session: Some(recipient_session),
        timestamp: timestamp.clone(),
        event_timestamp: timestamp,
    })
}

fn nonempty_string(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn valid_typed_id(value: &str, prefix: &str) -> bool {
    let Some(uuid) = value.strip_prefix(prefix) else {
        return false;
    };
    uuid.len() == 36
        && uuid.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

#[derive(Debug, Clone)]
struct StoredTaskEvent {
    store_index: usize,
    tape_id: String,
    event: TaskContextEvent,
}

/// Attach bounded, cited task ancestry to edit occurrences already selected by
/// explain. The index relation supplies candidate IDs; every relationship is
/// rechecked against the exact receipt/result offsets and event-time filter.
pub fn attach_explain_task_ancestry(
    context: &RuntimeContext,
    indexes: &[SqliteIndex],
    sessions: &mut [Value],
    date_filter: &DateFilter,
    requested_depth: usize,
    machine_label: &str,
) -> Result<(), CliError> {
    let stores = selected_store_paths(context);
    let depth_limit = requested_depth.min(MAX_TASK_ANCESTRY_DEPTH);
    for session in sessions {
        let Some(tape_id) = session["tape_id"].as_str().map(str::to_owned) else {
            continue;
        };
        let touches = session["touches"].as_array().cloned().unwrap_or_default();
        let mut ancestry = Vec::new();
        let eligible_edits = touches
            .iter()
            .filter(|touch| touch["kind"] == "edit")
            .filter(|touch| {
                touch["timestamp"].as_str().is_some_and(|timestamp| {
                    date_filter.event_time(Some(timestamp)) == EventTimeDecision::Included
                })
            })
            .collect::<Vec<_>>();
        for touch in eligible_edits.iter().take(8) {
            let Some(edit_offset) = touch["event_offset"].as_u64() else {
                continue;
            };
            let timestamp = touch["timestamp"].as_str().unwrap_or_default();
            if date_filter.event_time(Some(timestamp)) != EventTimeDecision::Included {
                continue;
            }
            let edit_ancestry = build_edit_task_ancestry(
                context,
                indexes,
                &stores,
                machine_label,
                &tape_id,
                edit_offset,
                timestamp,
                date_filter,
                depth_limit,
            )?;
            if edit_ancestry["status"] != "no_assignment_receipt" {
                ancestry.push(edit_ancestry);
            }
        }
        if !ancestry.is_empty() {
            session["task_ancestry"] = Value::Array(ancestry);
        }
        if eligible_edits.len() > 8 {
            session["task_ancestry_truncated"] = json!({
                "reason":"edit_ancestry_display_limit",
                "displayed":8,
                "eligible_edit_count_observed":eligible_edits.len(),
            });
        }
    }
    Ok(())
}

fn selected_store_paths(context: &RuntimeContext) -> Vec<PathBuf> {
    let mut stores = Vec::new();
    if context.db_path.exists() {
        stores.push(context.db_path.clone());
    }
    stores.extend(
        context
            .additional_stores
            .iter()
            .filter(|path| path.exists())
            .cloned(),
    );
    stores
}

fn task_events_for_id(
    indexes: &[SqliteIndex],
    kind: TaskIdKind,
    id: &str,
) -> Result<Vec<StoredTaskEvent>, CliError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (store_index, index) in indexes.iter().enumerate() {
        for (tape_id, event) in
            index.task_context_events_for_id(kind, id, MAX_TASK_CONTEXT_EVENTS)?
        {
            let key = (
                tape_id.clone(),
                event.event_identity.clone(),
                task_context_kind_name(event.kind),
            );
            if seen.insert(key) {
                out.push(StoredTaskEvent {
                    store_index,
                    tape_id,
                    event,
                });
            }
        }
    }
    out.sort_by(|left, right| {
        left.event
            .timestamp
            .cmp(&right.event.timestamp)
            .then_with(|| left.tape_id.cmp(&right.tape_id))
            .then_with(|| left.event.event_offset.cmp(&right.event.event_offset))
    });
    Ok(out)
}

fn task_receipts_before(
    indexes: &[SqliteIndex],
    tape_id: &str,
    offset: u64,
) -> Result<Vec<StoredTaskEvent>, CliError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (store_index, index) in indexes.iter().enumerate() {
        for event in index.task_context_events_for_tape_before(
            tape_id,
            offset,
            TaskContextKind::AssignmentReceipt,
            MAX_TASK_CONTEXT_EVENTS,
        )? {
            let key = (tape_id.to_owned(), event.event_identity.clone());
            if seen.insert(key) {
                out.push(StoredTaskEvent {
                    store_index,
                    tape_id: tape_id.to_owned(),
                    event,
                });
            }
        }
    }
    out.sort_by(|left, right| {
        right
            .event
            .event_offset
            .cmp(&left.event.event_offset)
            .then_with(|| left.event.event_identity.cmp(&right.event.event_identity))
    });
    Ok(out)
}

fn build_edit_task_ancestry(
    context: &RuntimeContext,
    indexes: &[SqliteIndex],
    stores: &[PathBuf],
    machine_label: &str,
    edit_tape: &str,
    edit_offset: u64,
    edit_timestamp: &str,
    date_filter: &DateFilter,
    depth_limit: usize,
) -> Result<Value, CliError> {
    let edit_store_index = indexes
        .iter()
        .position(|index| index.has_tape(edit_tape).unwrap_or(false))
        .unwrap_or(0);
    let mut result = json!({
        "edit": {
            "source": source_address(
                context,
                stores,
                machine_label,
                edit_store_index,
                edit_tape,
                edit_offset,
                edit_timestamp,
            ),
            "event_offset": edit_offset,
            "timestamp": edit_timestamp,
        },
        "steps": [],
        "status": "no_assignment_receipt",
        "guidance": ["No delivered assignment envelope was found before this edit. The available history may be incomplete."],
    });
    let mut receipts = task_receipts_before(indexes, edit_tape, edit_offset)?
        .into_iter()
        .filter(|receipt| receipt.event.task_id_kind == TaskIdKind::Assignment)
        .filter(|receipt| {
            date_filter.event_time(Some(&receipt.event.timestamp)) == EventTimeDecision::Included
        })
        .collect::<Vec<_>>();
    if receipts.is_empty() {
        return Ok(result);
    }
    if receipts.len() > 1 {
        let observed_receipt_count = receipts.len();
        let receipt_query_limit_reached = observed_receipt_count == MAX_TASK_CONTEXT_EVENTS;
        let alternatives_truncated =
            observed_receipt_count > MAX_TASK_ANCESTRY_BRANCHES || receipt_query_limit_reached;
        receipts.truncate(MAX_TASK_ANCESTRY_BRANCHES);
        result["status"] = json!("multiple_assignment_receipts");
        result["alternatives_truncated"] = json!(alternatives_truncated);
        result["alternatives_observed"] = json!(observed_receipt_count);
        if alternatives_truncated {
            result["truncation_reason"] = json!(if receipt_query_limit_reached {
                "receipt_query_limit_reached; additional alternatives may be omitted"
            } else {
                "receipt_branch_display_limit"
            });
        }
        let mut alternatives = Vec::new();
        for receipt in &receipts {
            let mut alternative = json!({
                "assignment_id": receipt.event.task_id,
                "receipt": task_event_citation(
                    context,
                    stores,
                    machine_label,
                    receipt,
                    receipt.event.event_offset,
                    "delivered_assignment_envelope",
                ),
            });
            let operations = preceding_assignment_operations(indexes, receipt, date_filter)?;
            match operations.as_slice() {
                [operation] => {
                    alternative["operation_link"] = assignment_operation_step(
                        context,
                        stores,
                        machine_label,
                        operation,
                        receipt,
                        date_filter,
                    )?;
                }
                [] => {
                    alternative["operation_link_status"] =
                        json!("no_preceding_operation_for_exact_assignment_id");
                }
                _ => {
                    alternative["operation_link_status"] =
                        json!("multiple_preceding_operations_for_exact_assignment_id");
                    alternative["operation_link_alternatives"] = Value::Array(
                        operations
                            .iter()
                            .take(MAX_TASK_ANCESTRY_BRANCHES)
                            .map(|operation| {
                                assignment_operation_step(
                                    context,
                                    stores,
                                    machine_label,
                                    operation,
                                    receipt,
                                    date_filter,
                                )
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    );
                }
            }
            alternatives.push(alternative);
        }
        result["receipt_alternatives"] = Value::Array(alternatives);
        result["guidance"] = json!([
            "More than one delivered assignment envelope precedes this edit. Exact assignment IDs are joined to their recorded operation results where available, but the edit's task remains ambiguous; inspect each cited handoff."
        ]);
        return Ok(result);
    }

    let mut current_receipt = receipts.remove(0);
    let mut seen_assignments = HashSet::new();
    let mut steps = Vec::new();
    let mut status = "earliest_observed_handoff";
    let mut chain_finished = false;

    if depth_limit == 0 {
        result["status"] = json!("depth_limit");
        result["receipt"] = task_event_citation(
            context,
            stores,
            machine_label,
            &current_receipt,
            current_receipt.event.event_offset,
            "delivered_assignment_envelope",
        );
        result["guidance"] = json!([
            "The task chain is available, but the requested traversal depth is zero. Earlier or missing context may remain."
        ]);
        return Ok(result);
    }

    loop {
        let assignment_id = current_receipt.event.task_id.clone();
        if !seen_assignments.insert(assignment_id.clone()) {
            status = "cycle_detected";
            steps.push(json!({
                "relationship": "cycle_guard",
                "assignment_id": assignment_id,
                "receipt": task_event_citation(
                    context,
                    stores,
                    machine_label,
                    &current_receipt,
                    current_receipt.event.event_offset,
                    "delivered_assignment_envelope",
                ),
            }));
            break;
        }

        let mut operations =
            preceding_assignment_operations(indexes, &current_receipt, date_filter)?;

        if operations.is_empty() {
            status = "no_recorded_assignment_operation";
            steps.push(json!({
                "relationship": "assignment_receipt",
                "assignment_id": assignment_id,
                "receipt": task_event_citation(
                    context,
                    stores,
                    machine_label,
                    &current_receipt,
                    current_receipt.event.event_offset,
                    "delivered_assignment_envelope",
                ),
                "reason": "No preceding successful assign or dispatch result for this exact assignment ID is indexed in the selected stores.",
            }));
            break;
        }
        if operations.len() > 1 {
            let observed_operation_count = operations.len();
            let operation_query_limit_reached = observed_operation_count == MAX_TASK_CONTEXT_EVENTS;
            let alternatives_truncated = observed_operation_count > MAX_TASK_ANCESTRY_BRANCHES
                || operation_query_limit_reached;
            operations.truncate(MAX_TASK_ANCESTRY_BRANCHES);
            status = "multiple_assignment_operations";
            steps.push(json!({
                "relationship": "assignment_operation_alternatives",
                "assignment_id": assignment_id,
                "receipt": task_event_citation(
                    context,
                    stores,
                    machine_label,
                    &current_receipt,
                    current_receipt.event.event_offset,
                    "delivered_assignment_envelope",
                ),
                "alternatives": operations.iter().map(|candidate| assignment_operation_step(
                    context,
                    stores,
                    machine_label,
                    candidate,
                    &current_receipt,
                    date_filter,
                )).collect::<Result<Vec<_>, CliError>>()?,
                "alternatives_observed": observed_operation_count,
                "alternatives_truncated": alternatives_truncated,
                "truncation_reason": if alternatives_truncated {
                    Some(if operation_query_limit_reached {
                        "operation_query_limit_reached; additional alternatives may be omitted"
                    } else {
                        "operation_branch_display_limit"
                    })
                } else {
                    None
                },
            }));
            break;
        }

        let operation = operations.remove(0);
        steps.push(assignment_operation_step(
            context,
            stores,
            machine_label,
            &operation,
            &current_receipt,
            date_filter,
        )?);
        if steps.len() >= depth_limit {
            status = "depth_limit";
            break;
        }

        let preceding =
            task_receipts_before(indexes, &operation.tape_id, operation.event.event_offset)?
                .into_iter()
                .filter(|receipt| {
                    date_filter.event_time(Some(&receipt.event.timestamp))
                        == EventTimeDecision::Included
                })
                .collect::<Vec<_>>();
        if preceding.len() == 1 {
            current_receipt = preceding.into_iter().next().expect("one receipt");
            continue;
        }
        if preceding.len() > 1 {
            let observed_parent_count = preceding.len();
            let parent_query_limit_reached = observed_parent_count == MAX_TASK_CONTEXT_EVENTS;
            let alternatives_truncated =
                observed_parent_count > MAX_TASK_ANCESTRY_BRANCHES || parent_query_limit_reached;
            status = "multiple_parent_assignments";
            let candidates = preceding
                .into_iter()
                .take(MAX_TASK_ANCESTRY_BRANCHES)
                .map(|receipt| {
                    task_event_citation(
                        context,
                        stores,
                        machine_label,
                        &receipt,
                        receipt.event.event_offset,
                        "delivered_assignment_envelope",
                    )
                })
                .collect::<Vec<_>>();
            if let Some(last) = steps.last_mut() {
                last["parent_assignment_alternatives"] = json!(candidates);
                last["parent_assignment_alternatives_observed"] = json!(observed_parent_count);
                last["parent_assignment_alternatives_truncated"] = json!(alternatives_truncated);
                if alternatives_truncated {
                    last["parent_assignment_truncation_reason"] =
                        json!(if parent_query_limit_reached {
                            "receipt_query_limit_reached; additional alternatives may be omitted"
                        } else {
                            "parent_branch_display_limit"
                        });
                }
            }
            break;
        }

        if let Some(work_item_id) = operation.event.work_item_id.as_deref() {
            let mut creations = task_events_for_id(indexes, TaskIdKind::WorkItem, work_item_id)?
                .into_iter()
                .filter(|candidate| candidate.event.kind == TaskContextKind::WorkItemCreate)
                .filter(|candidate| {
                    date_filter.event_time(Some(&candidate.event.timestamp))
                        == EventTimeDecision::Included
                })
                .filter(|candidate| {
                    task_event_precedes(
                        &candidate.tape_id,
                        &candidate.event,
                        &operation.tape_id,
                        &operation.event,
                    ) == Some(true)
                })
                .collect::<Vec<_>>();
            if creations.len() > 1 {
                let observed_creation_count = creations.len();
                let creation_query_limit_reached =
                    observed_creation_count == MAX_TASK_CONTEXT_EVENTS;
                let alternatives_truncated = observed_creation_count > MAX_TASK_ANCESTRY_BRANCHES
                    || creation_query_limit_reached;
                status = "multiple_work_item_creations";
                creations.truncate(MAX_TASK_ANCESTRY_BRANCHES);
                steps.push(json!({
                    "relationship": "work_item_creation_alternatives",
                    "work_item_id": work_item_id,
                    "alternatives_observed": observed_creation_count,
                    "alternatives_truncated": alternatives_truncated,
                    "truncation_reason": if alternatives_truncated {
                        Some(if creation_query_limit_reached {
                            "creation_query_limit_reached; additional alternatives may be omitted"
                        } else {
                            "creation_branch_display_limit"
                        })
                    } else {
                        None
                    },
                    "alternatives": creations.iter().map(|creation| task_event_citation(
                        context,
                        stores,
                        machine_label,
                        creation,
                        creation.event.event_offset,
                        "work_item_creation_result",
                    )).collect::<Vec<_>>(),
                }));
                break;
            }
            if let Some(creation) = creations.pop() {
                steps.push(work_item_creation_step(
                    context,
                    stores,
                    machine_label,
                    &creation,
                    work_item_id,
                    date_filter,
                )?);
                let creation_receipts =
                    task_receipts_before(indexes, &creation.tape_id, creation.event.event_offset)?
                        .into_iter()
                        .filter(|receipt| {
                            date_filter.event_time(Some(&receipt.event.timestamp))
                                == EventTimeDecision::Included
                        })
                        .collect::<Vec<_>>();
                if creation_receipts.len() == 1 {
                    current_receipt = creation_receipts.into_iter().next().expect("one receipt");
                    continue;
                }
                if creation_receipts.len() > 1 {
                    status = "multiple_parent_assignments";
                    if let Some(last) = steps.last_mut() {
                        last["parent_assignment_alternatives"] = json!(
                            creation_receipts
                                .into_iter()
                                .take(MAX_TASK_ANCESTRY_BRANCHES)
                                .map(|receipt| task_event_citation(
                                    context,
                                    stores,
                                    machine_label,
                                    &receipt,
                                    receipt.event.event_offset,
                                    "delivered_assignment_envelope",
                                ))
                                .collect::<Vec<_>>()
                        );
                    }
                    break;
                }
            }
        }
        chain_finished = true;
        break;
    }

    if steps.len() >= depth_limit && !chain_finished && status == "earliest_observed_handoff" {
        status = "depth_limit";
    }
    result["steps"] = Value::Array(steps);
    result["status"] = json!(status);
    result["guidance"] = json!([match status {
        "earliest_observed_handoff" =>
            "This is the earliest handoff found in the available history. The reason may be in the human discussion above it; earlier or missing history may remain.",
        "depth_limit" =>
            "This chain stops at the requested traversal limit. Earlier or missing context may remain; the passages are context, not an established reason.",
        "cycle_detected" =>
            "A repeated assignment ID stopped this chain. Inspect the cited events; no intent or reason is inferred.",
        "multiple_assignment_operations"
        | "multiple_parent_assignments"
        | "multiple_work_item_creations" =>
            "More than one supported parent is recorded. The alternatives are shown separately; no single cause or ruling is selected.",
        _ =>
            "The linked task has no preceding supported operation in the selected history. The available context may be incomplete.",
    }]);
    Ok(result)
}

fn assignment_operation_step(
    context: &RuntimeContext,
    stores: &[PathBuf],
    machine_label: &str,
    operation: &StoredTaskEvent,
    receipt: &StoredTaskEvent,
    date_filter: &DateFilter,
) -> Result<Value, CliError> {
    let request = task_event_citation(
        context,
        stores,
        machine_label,
        operation,
        operation.event.event_offset,
        match operation.event.kind {
            TaskContextKind::AssignmentCreate => "assignment_create_request",
            _ => "assignment_dispatch_request",
        },
    );
    let returned = task_event_citation(
        context,
        stores,
        machine_label,
        operation,
        operation
            .event
            .result_offset
            .unwrap_or(operation.event.event_offset),
        "successful_assignment_result",
    );
    let receipt = task_event_citation(
        context,
        stores,
        machine_label,
        receipt,
        receipt.event.event_offset,
        "delivered_assignment_envelope",
    );
    Ok(json!({
        "relationship": if operation.event.kind == TaskContextKind::AssignmentDispatch { "dispatched_assignment" } else { "created_assignment" },
        "assignment_id": operation.event.task_id,
        "work_item_id": operation.event.work_item_id,
        "actor_session": operation.event.actor_session,
        "actor_session_identity_domain": operation.event.actor_session.as_deref().map(session_identity_domain),
        "recipient_session": operation.event.recipient_session,
        "recipient_session_identity_domain": "tightbeam_session_key",
        "binding_basis": "The exact assignment ID joins the successful operation result to the delivered envelope. The operation's Tightbeam holder key and the envelope's native transcript session ID are different identifier domains; equality is not asserted.",
        "event_identity": operation.event.event_identity,
        "event_time": operation.event.timestamp,
        "edge_evidence": {
            "recipient_envelope": receipt,
            "operation_request": request,
            "operation_result": returned,
        },
        "context": preceding_context(
            context,
            stores,
            machine_label,
            operation.store_index,
            &operation.tape_id,
            operation.event.event_offset,
            date_filter,
        )?,
    }))
}

fn preceding_assignment_operations(
    indexes: &[SqliteIndex],
    receipt: &StoredTaskEvent,
    date_filter: &DateFilter,
) -> Result<Vec<StoredTaskEvent>, CliError> {
    let mut operations =
        task_events_for_id(indexes, TaskIdKind::Assignment, &receipt.event.task_id)?
            .into_iter()
            .filter(|candidate| {
                matches!(
                    candidate.event.kind,
                    TaskContextKind::AssignmentCreate | TaskContextKind::AssignmentDispatch
                )
            })
            .filter(|candidate| {
                date_filter.event_time(Some(&candidate.event.timestamp))
                    == EventTimeDecision::Included
            })
            .filter(|candidate| {
                task_event_precedes(
                    &candidate.tape_id,
                    &candidate.event,
                    &receipt.tape_id,
                    &receipt.event,
                ) == Some(true)
            })
            .collect::<Vec<_>>();
    operations.sort_by(|left, right| {
        left.event
            .timestamp
            .cmp(&right.event.timestamp)
            .then_with(|| left.tape_id.cmp(&right.tape_id))
            .then_with(|| left.event.event_offset.cmp(&right.event.event_offset))
    });
    Ok(operations)
}

fn work_item_creation_step(
    context: &RuntimeContext,
    stores: &[PathBuf],
    machine_label: &str,
    creation: &StoredTaskEvent,
    work_item_id: &str,
    date_filter: &DateFilter,
) -> Result<Value, CliError> {
    Ok(json!({
        "relationship": "work_item_created",
        "work_item_id": work_item_id,
        "event_identity": creation.event.event_identity,
        "event_time": creation.event.timestamp,
        "edge_evidence": {
            "operation_request": task_event_citation(
                context,
                stores,
                machine_label,
                creation,
                creation.event.event_offset,
                "work_item_create_request",
            ),
            "operation_result": task_event_citation(
                context,
                stores,
                machine_label,
                creation,
                creation.event.result_offset.unwrap_or(creation.event.event_offset),
                "work_item_creation_result",
            ),
        },
        "context": preceding_context(
            context,
            stores,
            machine_label,
            creation.store_index,
            &creation.tape_id,
            creation.event.event_offset,
            date_filter,
        )?,
    }))
}

fn task_event_citation(
    context: &RuntimeContext,
    stores: &[PathBuf],
    machine_label: &str,
    event: &StoredTaskEvent,
    offset: u64,
    event_type: &str,
) -> Value {
    let (timestamp, timestamp_basis) = if offset == event.event.event_offset {
        (
            event.event.event_timestamp.as_str(),
            "indexed_source_event_timestamp",
        )
    } else if event.event.result_offset == Some(offset) {
        (event.event.timestamp.as_str(), "indexed_result_timestamp")
    } else {
        (
            event.event.timestamp.as_str(),
            "indexed_event_time_fallback",
        )
    };
    let mut source = source_address(
        context,
        stores,
        machine_label,
        event.store_index,
        &event.tape_id,
        offset,
        &timestamp,
    );
    source["timestamp_basis"] = json!(timestamp_basis);
    json!({
        "type": event_type,
        "event_identity": event.event.event_identity,
        "source": source,
        "observed_actor_session": event.event.actor_session.as_ref().map(|id| json!({
            "id": id,
            "identity_domain": session_identity_domain(id),
        })),
        "observed_recipient_session": event.event.recipient_session.as_ref().map(|id| json!({
            "id": id,
            "identity_domain": if event.event.kind == TaskContextKind::AssignmentReceipt {
                "native_transcript_session_id"
            } else {
                "tightbeam_session_key"
            },
        })),
    })
}

fn source_address(
    context: &RuntimeContext,
    stores: &[PathBuf],
    machine_label: &str,
    store_index: usize,
    tape_id: &str,
    event_offset: u64,
    timestamp: &str,
) -> Value {
    let selected_store_path = stores.get(store_index).cloned();
    let store_path = selected_store_path
        .as_ref()
        .map(|path| path.display().to_string())
        .or_else(|| {
            resolve_tape_path(context, tape_id).map(|tape| {
                tape.parent()
                    .and_then(|dir| dir.parent())
                    .map(|root| root.join("index.sqlite"))
                    .unwrap_or_default()
                    .display()
                    .to_string()
            })
        });
    let source_machine = if selected_store_path.as_ref() == Some(&context.db_path) {
        machine_label
    } else {
        "machine_identity_unavailable_for_selected_store"
    };
    json!({
        "machine": source_machine,
        "store": store_path,
        "tape_id": tape_id,
        "event_offset": event_offset,
        "timestamp": timestamp,
    })
}

fn preceding_context(
    context: &RuntimeContext,
    stores: &[PathBuf],
    machine_label: &str,
    store_index: usize,
    tape_id: &str,
    before_offset: u64,
    date_filter: &DateFilter,
) -> Result<Value, CliError> {
    let Some(path) = resolve_tape_path_for_store(context, stores, store_index, tape_id) else {
        return Ok(
            json!({"status":"unavailable","reason":"source_tape_unavailable","passages":[]}),
        );
    };
    let raw = read_tape_content(&path)?;
    let rows = parse_jsonl_rows(&raw)?;
    let mut selected = Vec::new();
    let mut unknown_time = false;
    for row in rows.iter().filter(|row| row.offset < before_offset).rev() {
        if !matches!(row.value["k"].as_str(), Some("msg.in" | "msg.out")) {
            continue;
        }
        match date_filter.event_time(row.value["t"].as_str()) {
            EventTimeDecision::Included => {}
            EventTimeDecision::Excluded => continue,
            EventTimeDecision::Unknown => {
                unknown_time = true;
                continue;
            }
        }
        selected.push(context_passage(
            context,
            stores,
            machine_label,
            store_index,
            tape_id,
            row.offset,
            &row.value,
        ));
        if selected.len() >= TASK_CONTEXT_MESSAGE_COUNT {
            break;
        }
    }
    selected.reverse();
    let status = if selected.is_empty() {
        if unknown_time && date_filter.is_bounded() {
            "preceding_context_has_unknown_time"
        } else {
            "no_preceding_message_found"
        }
    } else {
        "available"
    };
    Ok(json!({"status":status,"passages":selected}))
}

fn resolve_tape_path_for_store(
    context: &RuntimeContext,
    stores: &[PathBuf],
    store_index: usize,
    tape_id: &str,
) -> Option<PathBuf> {
    let store = stores.get(store_index)?;
    let tapes_dir = if store == &context.db_path {
        context.tapes_dir.clone()
    } else {
        store.parent()?.join("tapes")
    };
    let path = crate::store::tapes::tape_path_for_tapes_dir(&tapes_dir, tape_id);
    path.exists().then_some(path)
}

fn context_passage(
    context: &RuntimeContext,
    stores: &[PathBuf],
    machine_label: &str,
    store_index: usize,
    tape_id: &str,
    offset: u64,
    row: &Value,
) -> Value {
    let text = row["content"].as_str().unwrap_or_default();
    let truncated = text.chars().count() > TASK_CONTEXT_MESSAGE_CHARS;
    let snippet = text
        .chars()
        .take(TASK_CONTEXT_MESSAGE_CHARS)
        .collect::<String>();
    let speaker = if row["k"] == "msg.out" {
        "agent_message"
    } else if text.starts_with("[from agent:") {
        "agent_delivery_rendered_as_user_role"
    } else if text.starts_with("[from user:") {
        "user_delivery_label_human_origin_unverified"
    } else {
        "incoming_origin_unverified"
    };
    let trimmed = text.trim_start();
    let quoted_or_relayed = if trimmed.starts_with('>') {
        "quoted_text"
    } else if text.contains("<engram-src") || text.starts_with("From Claude") {
        "relayed_or_quoted_origin_unverified"
    } else {
        "not_classified"
    };
    json!({
        "speaker":speaker,
        "quoted_or_relayed":quoted_or_relayed,
        "role":row["k"],
        "text":snippet,
        "truncated":truncated,
        "source": source_address(
            context,
            stores,
            machine_label,
            store_index,
            tape_id,
            offset,
            row["t"].as_str().unwrap_or_default(),
        ),
    })
}

fn task_event_precedes(
    candidate_tape: &str,
    candidate: &TaskContextEvent,
    later_tape: &str,
    later: &TaskContextEvent,
) -> Option<bool> {
    if candidate_tape == later_tape {
        let candidate_result = candidate.result_offset.unwrap_or(candidate.event_offset);
        return Some(candidate_result < later.event_offset);
    }
    let candidate_time = chrono::DateTime::parse_from_rfc3339(&candidate.timestamp).ok()?;
    let later_time = chrono::DateTime::parse_from_rfc3339(&later.timestamp).ok()?;
    Some(candidate_time < later_time)
}

fn task_context_kind_name(kind: TaskContextKind) -> &'static str {
    match kind {
        TaskContextKind::WorkItemCreate => "work_item_create",
        TaskContextKind::AssignmentCreate => "assignment_create",
        TaskContextKind::AssignmentDispatch => "assignment_dispatch",
        TaskContextKind::AssignmentReceipt => "assignment_receipt",
    }
}

fn session_identity_domain(session: &str) -> &'static str {
    if session.starts_with("agent:") {
        "tightbeam_session_key"
    } else {
        "native_or_unverified_session_identifier"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ReaderMode, SqliteIndex};
    use crate::tape::compress::compress_jsonl;
    use crate::tape::event::parse_jsonl_events;
    use crate::{RuntimeContext, config::EffectiveWatchConfig};
    use serde_json::json;
    use std::fs;

    const ASSIGNMENT: &str = "asg_12345678-1234-1234-1234-1234567890ab";
    const PARENT_ASSIGNMENT: &str = "asg_12345678-1234-1234-1234-1234567890ac";
    const SIBLING_ASSIGNMENT: &str = "asg_12345678-1234-1234-1234-1234567890ad";
    const WORK_ITEM: &str = "wi_abcdef01-2345-6789-abcd-ef0123456789";
    const SENDER: &str = "agent:main:clawline:mike:main s_sender01";
    const RECIPIENT: &str = "agent:coder:engram s_recipient01";

    fn wrapped_call(command: &str, call_id: &str, timestamp: &str) -> Value {
        json!({
            "t": timestamp,
            "k": "tool.call",
            "tool": "exec",
            "call_id": call_id,
            "source": {"harness":"codex-cli", "session_id":SENDER},
            "args": format!("text(await tools.exec_command({{cmd:{}}}));", serde_json::to_string(command).unwrap())
        })
    }

    fn wrapped_result(call_id: &str, stdout: &str, exit_code: i64) -> Value {
        json!({
            "t":"2026-10-03T12:00:01Z",
            "k":"tool.result",
            "tool":"exec",
            "call_id":call_id,
            "stdout":stdout,
            "raw_output":[
                {"type":"input_text","text":"Script completed\nWall time 0.1s\nOutput:\n"},
                {"type":"input_text","text":serde_json::json!({"exit_code":exit_code,"output":stdout}).to_string()}
            ]
        })
    }

    fn jsonl(rows: impl IntoIterator<Item = Value>) -> String {
        rows.into_iter()
            .map(|row| row.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn runtime_context(root: &std::path::Path) -> RuntimeContext {
        let db_path = root.join("index.sqlite");
        let tapes_dir = root.join("tapes");
        fs::create_dir_all(&tapes_dir).unwrap();
        RuntimeContext {
            config_path: root.join("config.yml"),
            db_path,
            tapes_dir: tapes_dir.clone(),
            frozen_stores: Vec::new(),
            tape_lookup_dirs: vec![tapes_dir],
            additional_stores: Vec::new(),
            explain_default_limit: 10,
            peek_default_lines: 8,
            peek_default_before: 2,
            peek_default_after: 2,
            peek_grep_context: 2,
            metrics_enabled: false,
            metrics_log: root.join("metrics.jsonl"),
            watch: None::<EffectiveWatchConfig>,
        }
    }

    fn write_fixture_tape(context: &RuntimeContext, tape_id: &str, normalized: &str) {
        let path = context.tapes_dir.join(format!("{tape_id}.jsonl.zst"));
        fs::write(path, compress_jsonl(normalized).unwrap()).unwrap();
    }

    #[test]
    fn pairs_literal_dispatch_with_successful_returned_assignment_and_receiver() {
        let call_id = "call_dispatch_01";
        let output = json!({
            "id": ASSIGNMENT,
            "workItemId": WORK_ITEM,
            "openedBySession": SENDER,
            "holderKey": RECIPIENT,
            "state":"open"
        })
        .to_string();
        let normalized = jsonl([
            wrapped_call(
                &format!(
                    "tightbeam dispatch --to '{RECIPIENT}' --work-item {WORK_ITEM} --subject 'ID-heavy title' --brief 'bounded brief'"
                ),
                call_id,
                "2026-10-03T12:00:00Z",
            ),
            wrapped_result(call_id, &output, 0),
        ]);

        let events = extract_task_context_events(&normalized);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.event_identity, call_id);
        assert_eq!(event.event_offset, 0);
        assert_eq!(event.result_offset, Some(1));
        assert_eq!(event.kind, TaskContextKind::AssignmentDispatch);
        assert_eq!(event.task_id_kind, TaskIdKind::Assignment);
        assert_eq!(event.task_id, ASSIGNMENT);
        assert_eq!(event.work_item_id.as_deref(), Some(WORK_ITEM));
        assert_eq!(event.actor_session.as_deref(), Some(SENDER));
        assert_eq!(event.recipient_session.as_deref(), Some(RECIPIENT));
    }

    #[test]
    fn pairs_retained_direct_success_result_block_without_inventing_exit_status() {
        let call_id = "call_dispatch_direct_result";
        let call = wrapped_call(
            &format!(
                "tightbeam dispatch --to '{RECIPIENT}' --work-item {WORK_ITEM} --subject 'Retained result' --brief 'Use returned state.'"
            ),
            call_id,
            "2026-10-03T12:00:00Z",
        );
        let returned = json!({
            "assignment": {
                "id": ASSIGNMENT,
                "workItemId": WORK_ITEM,
                "openedBySession": SENDER,
                "holderKey": RECIPIENT,
                "state": "open"
            },
            "attest": null,
            "referents": []
        });
        let direct_result = json!({
            "t":"2026-10-03T12:00:01Z",
            "k":"tool.result",
            "tool":"exec",
            "call_id":call_id,
            "raw_output":[
                {"type":"input_text","text":"Script completed\nWall time 0.1 seconds\nOutput:\n"},
                {"type":"input_text","text":returned.to_string()}
            ]
        });
        let events = extract_task_context_events(&jsonl([call, direct_result]));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].task_id, ASSIGNMENT);
        assert_eq!(events[0].work_item_id.as_deref(), Some(WORK_ITEM));
        assert_eq!(events[0].recipient_session.as_deref(), Some(RECIPIENT));

        let failed_result = json!({
            "t":"2026-10-03T12:00:02Z",
            "k":"tool.result",
            "tool":"exec",
            "call_id":"call_dispatch_failed_direct",
            "raw_output":[
                {"type":"input_text","text":"Script completed\nWall time 0.1 seconds\nOutput:\n"},
                {"type":"input_text","text":json!({"message":"denied","ruminationRequired":true,"workItemId":WORK_ITEM}).to_string()}
            ]
        });
        let failed_call = wrapped_call(
            &format!(
                "tightbeam dispatch --to '{RECIPIENT}' --work-item {WORK_ITEM} --subject 'Retained result' --brief 'Use returned state.'"
            ),
            "call_dispatch_failed_direct",
            "2026-10-03T12:00:01Z",
        );
        assert!(extract_task_context_events(&jsonl([failed_call, failed_result])).is_empty());
    }

    #[test]
    fn rejects_mentions_failed_dispatches_bad_pairing_and_false_work_item_binding() {
        let command = format!(
            "tightbeam dispatch --to '{RECIPIENT}' --work-item {WORK_ITEM} --subject 'mentions {ASSIGNMENT}'"
        );
        let false_pair = wrapped_call(&command, "call_mismatch", "2026-10-03T12:00:00Z");
        let actual_result = wrapped_result(
            "some_other_call",
            &json!({"id":ASSIGNMENT,"workItemId":WORK_ITEM,"openedBySession":SENDER,"holderKey":RECIPIENT}).to_string(),
            0,
        );
        let failed = wrapped_call(&command, "call_failed", "2026-10-03T12:00:02Z");
        let failed_result = wrapped_result(
            "call_failed",
            &json!({"ruminationRequired":true,"workItemId":WORK_ITEM}).to_string(),
            1,
        );
        let mismatch = wrapped_call(
            &format!(
                "tightbeam dispatch --to '{RECIPIENT}' --work-item {WORK_ITEM} --subject 'work'"
            ),
            "call_wrong_work_item",
            "2026-10-03T12:00:03Z",
        );
        let mismatch_result = wrapped_result(
            "call_wrong_work_item",
            &json!({
                "id":ASSIGNMENT,
                "workItemId":"wi_00000000-0000-0000-0000-000000000000",
                "openedBySession":SENDER,
                "holderKey":RECIPIENT
            })
            .to_string(),
            0,
        );
        let mention = json!({"t":"2026-10-03T12:00:04Z","k":"msg.in","source":{"session_id":RECIPIENT},"content":format!("Please inspect {ASSIGNMENT} and {WORK_ITEM}.")});
        let fake_command = wrapped_call(
            &format!("echo 'tightbeam dispatch --work-item {WORK_ITEM} --to {RECIPIENT}'"),
            "call_echo",
            "2026-10-03T12:00:05Z",
        );
        let fake_result = wrapped_result("call_echo", &json!({"id":ASSIGNMENT}).to_string(), 0);

        let normalized = jsonl([
            false_pair,
            actual_result,
            failed,
            failed_result,
            mismatch,
            mismatch_result,
            mention,
            fake_command,
            fake_result,
        ]);
        assert!(extract_task_context_events(&normalized).is_empty());
    }

    #[test]
    fn accepts_only_a_delivered_assignment_envelope_not_a_quote_or_output() {
        let delivered = json!({
            "t":"2026-10-03T12:00:00Z",
            "k":"msg.in",
            "source":{"session_id":RECIPIENT},
            "content":format!("[from agent:main]\n\n[assignment: {ASSIGNMENT}]\n\nContinue the work.")
        });
        let quoted = json!({
            "t":"2026-10-03T12:00:01Z",
            "k":"msg.in",
            "source":{"session_id":RECIPIENT},
            "content":format!("Earlier note:\n[from agent:main]\n\n[assignment: {ASSIGNMENT}] old message")
        });
        let assistant_quote = json!({
            "t":"2026-10-03T12:00:02Z",
            "k":"msg.out",
            "source":{"session_id":RECIPIENT},
            "content":format!("[from agent:main]\n\n[assignment: {ASSIGNMENT}]")
        });
        let events = extract_task_context_events(&jsonl([delivered, quoted, assistant_quote]));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_offset, 0);
        assert_eq!(events[0].kind, TaskContextKind::AssignmentReceipt);
        assert_eq!(events[0].task_id, ASSIGNMENT);
        assert_eq!(events[0].recipient_session.as_deref(), Some(RECIPIENT));
    }

    #[test]
    fn shell_parser_rejects_composition_and_dynamic_expansion() {
        assert!(
            parse_operation_command(
                "tightbeam dispatch --work-item wi_abcdef01-2345-6789-abcd-ef0123456789"
            )
            .is_some()
        );
        assert!(parse_operation_command("tightbeam dispatch --work-item wi_abcdef01-2345-6789-abcd-ef0123456789 && echo done").is_none());
        assert!(parse_operation_command("tightbeam dispatch --work-item $WORK_ITEM").is_none());
        assert!(
            parse_operation_command(
                "sh -c 'tightbeam dispatch --work-item wi_abcdef01-2345-6789-abcd-ef0123456789'"
            )
            .is_none()
        );
        assert!(parse_operation_command("tightbeam dispatch --work-item wi_abcdef01-2345-6789-abcd-ef0123456789 --work-item wi_abcdef01-2345-6789-abcd-ef0123456789").is_none());
    }

    #[test]
    fn retained_edit_joins_two_dispatch_hops_and_work_item_creation_with_cited_context() {
        let temp = tempfile::tempdir().unwrap();
        let context = runtime_context(temp.path());
        let root_tape = "root-tape";
        let middle_tape = "middle-tape";
        let receiver_tape = "receiver-tape";
        let mut work_item_call = wrapped_call(
            "tightbeam work-item-create --title 'Retain verified edit context'",
            "call_create_work_item",
            "2026-10-03T12:00:01Z",
        );
        work_item_call["source"]["session_id"] = json!("codex-native-owner-session");
        let work_item_result = wrapped_result(
            "call_create_work_item",
            &json!({"workItem":{"id":WORK_ITEM,"state":"open"}}).to_string(),
            0,
        );
        let mut parent_dispatch_call = wrapped_call(
            &format!(
                "tightbeam dispatch --to '{SENDER}' --work-item {WORK_ITEM} --subject 'Root handoff' --brief 'Preserve the human request.'"
            ),
            "call_dispatch_parent",
            "2026-10-03T12:00:03Z",
        );
        parent_dispatch_call["source"]["session_id"] = json!("codex-native-owner-session");
        let mut parent_dispatch_result = wrapped_result(
            "call_dispatch_parent",
            &json!({
                "id":PARENT_ASSIGNMENT,
                "workItemId":WORK_ITEM,
                "openedBySession":"agent:owner s_owner01",
                "holderKey":SENDER,
                "state":"open"
            })
            .to_string(),
            0,
        );
        parent_dispatch_result["t"] = json!("2026-10-03T12:00:04Z");
        let sibling_call = wrapped_call(
            &format!(
                "tightbeam dispatch --to 'agent:reviewer s_review01' --work-item {WORK_ITEM} --subject 'Sibling review' --brief 'Inspect independently.'"
            ),
            "call_dispatch_sibling",
            "2026-10-03T12:00:04Z",
        );
        let sibling_result = wrapped_result(
            "call_dispatch_sibling",
            &json!({
                "id":SIBLING_ASSIGNMENT,
                "workItemId":WORK_ITEM,
                "openedBySession":"agent:owner s_owner01",
                "holderKey":"agent:reviewer s_review01",
                "state":"open"
            })
            .to_string(),
            0,
        );
        let root_raw = jsonl([
            json!({
                "t":"2026-10-03T12:00:00Z",
                "k":"msg.in",
                "source":{"session_id":"codex-native-owner-session"},
                "content":"[from user:mike]\n\nThe upstream edit needs its real dispatch context. Preserve the exact task and recipient identity."
            }),
            work_item_call,
            work_item_result,
            parent_dispatch_call,
            parent_dispatch_result,
            sibling_call,
            sibling_result,
        ]);
        let mut child_dispatch_call = wrapped_call(
            &format!(
                "tightbeam dispatch --to '{RECIPIENT}' --work-item {WORK_ITEM} --subject 'Retained edit' --brief 'Use the verified upstream context.'"
            ),
            "call_dispatch_edit",
            "2026-10-03T12:00:07Z",
        );
        child_dispatch_call["source"]["session_id"] = json!("codex-native-middle-session");
        let mut child_dispatch_result = wrapped_result(
            "call_dispatch_edit",
            &json!({
                "id":ASSIGNMENT,
                "workItemId":WORK_ITEM,
                "openedBySession":SENDER,
                "holderKey":RECIPIENT,
                "state":"open"
            })
            .to_string(),
            0,
        );
        child_dispatch_result["t"] = json!("2026-10-03T12:00:08Z");
        let middle_raw = jsonl([
            json!({
                "t":"2026-10-03T12:00:05Z",
                "k":"msg.in",
                "source":{"session_id":"codex-native-middle-session"},
                "content":format!("[from agent:owner]\n\n[assignment: {PARENT_ASSIGNMENT}]\n\nContinue the verified work.")
            }),
            json!({
                "t":"2026-10-03T12:00:06Z",
                "k":"msg.in",
                "source":{"session_id":"codex-native-middle-session"},
                "content":"[from user:mike]\n\nKeep the child dispatch tied to the same requested change."
            }),
            child_dispatch_call,
            child_dispatch_result,
        ]);
        let receiver_raw = jsonl([
            json!({
                "t":"2026-10-03T12:00:09Z",
                "k":"msg.in",
                "source":{"session_id":"codex-native-recipient-session"},
                "content":format!("[from agent:main]\n\n[assignment: {ASSIGNMENT}]\n\nImplement the retained edit with the cited context.")
            }),
            json!({
                "t":"2026-10-03T12:00:10Z",
                "k":"code.edit",
                "file":"scripts/verify_mix.sh",
                "before_range":[19,25],
                "after_range":[19,25],
                "before_text":"verify the mixed changes",
                "after_text":"verify the mixed changes and retain dispatch context"
            }),
        ]);
        write_fixture_tape(&context, root_tape, &root_raw);
        write_fixture_tape(&context, middle_tape, &middle_raw);
        write_fixture_tape(&context, receiver_tape, &receiver_raw);

        let root_rows = parse_jsonl_events(&root_raw).unwrap();
        let root_context = extract_task_context_events(&root_raw);
        let middle_rows = parse_jsonl_events(&middle_raw).unwrap();
        let middle_context = extract_task_context_events(&middle_raw);
        let receiver_rows = parse_jsonl_events(&receiver_raw).unwrap();
        let receiver_context = extract_task_context_events(&receiver_raw);
        assert_eq!(
            root_context.len(),
            3,
            "creation, parent dispatch, and same-work-item sibling are retained"
        );
        assert_eq!(
            middle_context.len(),
            2,
            "parent receipt and child dispatch are retained"
        );
        assert_eq!(
            receiver_context.len(),
            1,
            "only the delivered envelope is a receipt"
        );
        {
            let writer = SqliteIndex::open_owner_writer(
                context.db_path.to_str().expect("fixture database path"),
            )
            .unwrap();
            writer
                .ingest_tape_events_with_context(
                    root_tape,
                    &root_rows,
                    &[],
                    &root_context,
                    crate::index::lineage::LINK_THRESHOLD_DEFAULT,
                )
                .unwrap();
            writer
                .ingest_tape_events_with_context(
                    middle_tape,
                    &middle_rows,
                    &[],
                    &middle_context,
                    crate::index::lineage::LINK_THRESHOLD_DEFAULT,
                )
                .unwrap();
            writer
                .ingest_tape_events_with_context(
                    receiver_tape,
                    &receiver_rows,
                    &[],
                    &receiver_context,
                    crate::index::lineage::LINK_THRESHOLD_DEFAULT,
                )
                .unwrap();
        }
        let index = SqliteIndex::open_reader_mode(
            context.db_path.to_str().expect("fixture database path"),
            ReaderMode::Live,
        )
        .unwrap();
        let mut sessions = vec![json!({
            "tape_id":receiver_tape,
            "touches":[{
                "kind":"edit",
                "event_offset":1,
                "timestamp":"2026-10-03T12:00:10Z"
            }]
        })];
        attach_explain_task_ancestry(
            &context,
            &[index],
            &mut sessions,
            &DateFilter::parse(None, None).unwrap(),
            4,
            "gibson",
        )
        .unwrap();

        let ancestry = &sessions[0]["task_ancestry"][0];
        assert_eq!(ancestry["status"], "earliest_observed_handoff");
        assert_eq!(ancestry["edit"]["source"]["tape_id"], receiver_tape);
        assert_eq!(ancestry["edit"]["source"]["event_offset"], 1);
        assert_eq!(ancestry["steps"].as_array().unwrap().len(), 3);
        let dispatch = &ancestry["steps"][0];
        assert_eq!(dispatch["relationship"], "dispatched_assignment");
        assert_eq!(dispatch["assignment_id"], ASSIGNMENT);
        assert_eq!(dispatch["work_item_id"], WORK_ITEM);
        assert_eq!(
            dispatch["edge_evidence"]["operation_request"]["source"]["tape_id"],
            middle_tape
        );
        assert_eq!(
            dispatch["edge_evidence"]["operation_request"]["source"]["event_offset"],
            2
        );
        assert_eq!(
            dispatch["edge_evidence"]["operation_request"]["source"]["timestamp"],
            "2026-10-03T12:00:07Z"
        );
        assert_eq!(
            dispatch["edge_evidence"]["operation_request"]["source"]["timestamp_basis"],
            "indexed_source_event_timestamp"
        );
        assert_eq!(
            dispatch["edge_evidence"]["operation_result"]["source"]["event_offset"],
            3
        );
        assert_eq!(
            dispatch["edge_evidence"]["operation_result"]["source"]["timestamp"],
            "2026-10-03T12:00:08Z"
        );
        assert_eq!(
            dispatch["edge_evidence"]["recipient_envelope"]["source"]["tape_id"],
            receiver_tape
        );
        assert_eq!(
            dispatch["edge_evidence"]["recipient_envelope"]["source"]["event_offset"],
            0
        );
        assert_eq!(
            dispatch["recipient_session_identity_domain"],
            "tightbeam_session_key"
        );
        assert_eq!(
            dispatch["edge_evidence"]["recipient_envelope"]["observed_recipient_session"]["identity_domain"],
            "native_transcript_session_id"
        );
        assert_eq!(
            dispatch["edge_evidence"]["recipient_envelope"]["observed_recipient_session"]["id"],
            "codex-native-recipient-session"
        );
        assert_eq!(
            dispatch["context"]["passages"][0]["source"]["event_offset"],
            0
        );
        assert_eq!(
            dispatch["context"]["passages"][0]["speaker"],
            "agent_delivery_rendered_as_user_role"
        );
        assert!(
            dispatch["context"]["passages"][0]["text"]
                .as_str()
                .unwrap()
                .contains(PARENT_ASSIGNMENT)
        );
        assert_eq!(
            dispatch["context"]["passages"][1]["speaker"],
            "user_delivery_label_human_origin_unverified"
        );
        assert!(
            dispatch["context"]["passages"][1]["text"]
                .as_str()
                .unwrap()
                .contains("child dispatch")
        );
        assert!(
            ancestry["guidance"][0]
                .as_str()
                .unwrap()
                .contains("reason may be")
        );
        assert_eq!(
            ancestry["steps"][1]["relationship"],
            "dispatched_assignment"
        );
        assert_eq!(ancestry["steps"][1]["assignment_id"], PARENT_ASSIGNMENT);
        assert_eq!(
            ancestry["steps"][1]["edge_evidence"]["recipient_envelope"]["observed_recipient_session"]
                ["id"],
            "codex-native-middle-session"
        );
        assert_eq!(
            ancestry["steps"][1]["edge_evidence"]["recipient_envelope"]["source"]["tape_id"],
            middle_tape
        );
        assert_eq!(
            ancestry["steps"][1]["edge_evidence"]["operation_request"]["source"]["tape_id"],
            root_tape
        );
        assert_eq!(ancestry["steps"][2]["relationship"], "work_item_created");
        assert_eq!(ancestry["steps"][2]["work_item_id"], WORK_ITEM);
        assert_eq!(
            ancestry["steps"][2]["edge_evidence"]["operation_result"]["source"]["event_offset"],
            2
        );
        assert_eq!(
            ancestry["steps"][2]["context"]["passages"][0]["speaker"],
            "user_delivery_label_human_origin_unverified"
        );
    }

    #[test]
    fn multiple_exact_receipts_keep_distinct_dispatch_edges_without_crosswalking_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let context = runtime_context(temp.path());
        let first_tape = "dispatch-first";
        let second_tape = "dispatch-second";
        let receiver_tape = "dispatch-recipient";
        let recipient_key = "agent:coder:engram s_recipient01";

        let mut first_call = wrapped_call(
            &format!(
                "tightbeam dispatch --to '{recipient_key}' --work-item {WORK_ITEM} --subject 'First task' --brief 'Inspect first branch.'"
            ),
            "call_first_dispatch",
            "2026-10-03T12:00:00Z",
        );
        first_call["source"]["session_id"] = json!("native-first-dispatch-session");
        let mut first_result = wrapped_result(
            "call_first_dispatch",
            &json!({
                "id":ASSIGNMENT,
                "workItemId":WORK_ITEM,
                "openedBySession":"agent:owner s_owner01",
                "holderKey":recipient_key,
                "state":"open"
            })
            .to_string(),
            0,
        );
        first_result["t"] = json!("2026-10-03T12:00:01Z");
        let mut second_call = wrapped_call(
            &format!(
                "tightbeam dispatch --to '{recipient_key}' --work-item {WORK_ITEM} --subject 'Second task' --brief 'Inspect the second branch.'"
            ),
            "call_second_dispatch",
            "2026-10-03T12:00:02Z",
        );
        second_call["source"]["session_id"] = json!("native-second-dispatch-session");
        let mut second_result = wrapped_result(
            "call_second_dispatch",
            &json!({
                "id":SIBLING_ASSIGNMENT,
                "workItemId":WORK_ITEM,
                "openedBySession":"agent:owner s_owner01",
                "holderKey":recipient_key,
                "state":"open"
            })
            .to_string(),
            0,
        );
        second_result["t"] = json!("2026-10-03T12:00:03Z");
        let first_raw = jsonl([first_call, first_result]);
        let second_raw = jsonl([second_call, second_result]);
        let control_assignment_ids = [
            "asg_12345678-1234-1234-1234-1234567890b1",
            "asg_12345678-1234-1234-1234-1234567890b2",
            "asg_12345678-1234-1234-1234-1234567890b3",
        ];
        let mut receiver_rows = control_assignment_ids
            .iter()
            .enumerate()
            .map(|(index, assignment_id)| {
                json!({
                    "t":format!("2026-10-03T12:00:0{}Z", index + 1),
                    "k":"msg.in",
                    "source":{"session_id":"native-recipient-session"},
                    "content":format!("[from agent:main]\n[assignment: {assignment_id}]\nUnlinked control delivery.")
                })
            })
            .collect::<Vec<_>>();
        receiver_rows.extend([
            json!({
                "t":"2026-10-03T12:00:04Z",
                "k":"msg.in",
                "source":{"session_id":"native-recipient-session"},
                "content":format!("[from agent:main]\n[assignment: {ASSIGNMENT}]\nFirst delivery.")
            }),
            json!({
                "t":"2026-10-03T12:00:05Z",
                "k":"msg.in",
                "source":{"session_id":"native-recipient-session"},
                "content":format!("[from agent:main]\n[assignment: {SIBLING_ASSIGNMENT}]\nSecond delivery.")
            }),
            json!({
                "t":"2026-10-03T12:00:06Z",
                "k":"code.edit",
                "file":"src/lib.rs",
                "before_range":[4,4],
                "after_range":[4,5],
                "before_text":"old",
                "after_text":"new"
            }),
        ]);
        let receiver_raw = jsonl(receiver_rows);
        for (tape, raw) in [
            (first_tape, first_raw.as_str()),
            (second_tape, second_raw.as_str()),
            (receiver_tape, receiver_raw.as_str()),
        ] {
            write_fixture_tape(&context, tape, raw);
        }
        {
            let writer = SqliteIndex::open_owner_writer(
                context.db_path.to_str().expect("fixture database path"),
            )
            .unwrap();
            for (tape, raw) in [
                (first_tape, first_raw.as_str()),
                (second_tape, second_raw.as_str()),
                (receiver_tape, receiver_raw.as_str()),
            ] {
                writer
                    .ingest_tape_events_with_context(
                        tape,
                        &parse_jsonl_events(raw).unwrap(),
                        &[],
                        &extract_task_context_events(raw),
                        crate::index::lineage::LINK_THRESHOLD_DEFAULT,
                    )
                    .unwrap();
            }
        }
        let index = SqliteIndex::open_reader_mode(
            context.db_path.to_str().expect("fixture database path"),
            ReaderMode::Live,
        )
        .unwrap();
        let mut sessions = vec![json!({
            "tape_id":receiver_tape,
            "touches":[{"kind":"edit","event_offset":5,"timestamp":"2026-10-03T12:00:06Z"}]
        })];
        attach_explain_task_ancestry(
            &context,
            &[index],
            &mut sessions,
            &DateFilter::parse(None, None).unwrap(),
            3,
            "gibson",
        )
        .unwrap();

        let ancestry = &sessions[0]["task_ancestry"][0];
        assert_eq!(ancestry["status"], "multiple_assignment_receipts");
        assert_eq!(ancestry["alternatives_observed"], 5);
        assert_eq!(ancestry["alternatives_truncated"], true);
        assert_eq!(
            ancestry["truncation_reason"],
            "receipt_branch_display_limit"
        );
        let alternatives = ancestry["receipt_alternatives"].as_array().unwrap();
        assert_eq!(alternatives.len(), 4);
        assert_eq!(alternatives[0]["assignment_id"], SIBLING_ASSIGNMENT);
        assert_eq!(alternatives[1]["assignment_id"], ASSIGNMENT);
        let mut linked_count = 0;
        for alternative in alternatives {
            let Some(operation) = alternative.get("operation_link") else {
                assert_eq!(
                    alternative["operation_link_status"],
                    "no_preceding_operation_for_exact_assignment_id"
                );
                continue;
            };
            linked_count += 1;
            assert_eq!(operation["relationship"], "dispatched_assignment");
            assert_eq!(
                operation["binding_basis"],
                "The exact assignment ID joins the successful operation result to the delivered envelope. The operation's Tightbeam holder key and the envelope's native transcript session ID are different identifier domains; equality is not asserted."
            );
            assert_eq!(
                operation["edge_evidence"]["recipient_envelope"]["observed_recipient_session"]["identity_domain"],
                "native_transcript_session_id"
            );
            assert_ne!(
                operation["recipient_session"],
                operation["edge_evidence"]["recipient_envelope"]["observed_recipient_session"]["id"]
            );
        }
        assert_eq!(linked_count, 2);
    }

    #[test]
    fn an_id_mention_before_an_edit_does_not_create_task_ancestry() {
        let temp = tempfile::tempdir().unwrap();
        let context = runtime_context(temp.path());
        let tape_id = "mention-only-tape";
        let raw = jsonl([
            json!({
                "t":"2026-10-03T12:00:00Z",
                "k":"msg.in",
                "source":{"session_id":RECIPIENT},
                "content":format!("Please look at assignment {ASSIGNMENT} for work item {WORK_ITEM}.")
            }),
            json!({
                "t":"2026-10-03T12:00:01Z",
                "k":"code.edit",
                "file":"scripts/verify_mix.sh",
                "before_range":[19,25],
                "after_range":[19,25],
                "before_text":"verify the mixed changes",
                "after_text":"verify the mixed changes and retain context"
            }),
        ]);
        write_fixture_tape(&context, tape_id, &raw);
        let rows = parse_jsonl_events(&raw).unwrap();
        let task_events = extract_task_context_events(&raw);
        assert!(task_events.is_empty());
        {
            let writer = SqliteIndex::open_owner_writer(
                context.db_path.to_str().expect("fixture database path"),
            )
            .unwrap();
            writer
                .ingest_tape_events_with_context(
                    tape_id,
                    &rows,
                    &[],
                    &task_events,
                    crate::index::lineage::LINK_THRESHOLD_DEFAULT,
                )
                .unwrap();
        }
        let index = SqliteIndex::open_reader_mode(
            context.db_path.to_str().expect("fixture database path"),
            ReaderMode::Live,
        )
        .unwrap();
        let mut sessions = vec![json!({
            "tape_id":tape_id,
            "touches":[{"kind":"edit","event_offset":1,"timestamp":"2026-10-03T12:00:01Z"}]
        })];
        attach_explain_task_ancestry(
            &context,
            &[index],
            &mut sessions,
            &DateFilter::parse(None, None).unwrap(),
            4,
            "gibson",
        )
        .unwrap();
        assert!(
            sessions[0].get("task_ancestry").is_none(),
            "mention-only identifiers do not add an empty ancestry result"
        );
    }
}
