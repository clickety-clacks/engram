use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::index::{DispatchDirection, DispatchLink, DispatchLinkRow, SqliteIndex};
use crate::store::tapes::{TapeRow, event_window, parse_jsonl_rows, read_tape_content, resolve_tape_path};
use crate::{CliError, RuntimeContext};

const TRANSCRIPT_WINDOW_RADIUS: usize = 2;

#[derive(Default)]
struct DispatchTapeCache {
    rows: HashMap<String, Vec<TapeRow>>,
    paths: HashMap<String, Option<PathBuf>>,
}

impl DispatchTapeCache {
    fn load<'a>(
        &'a mut self,
        context: &RuntimeContext,
        tape_id: &str,
    ) -> Result<&'a Vec<TapeRow>, CliError> {
        if !self.rows.contains_key(tape_id) {
            let path = resolve_tape_path(context, tape_id);
            self.paths.insert(tape_id.to_owned(), path.clone());
            let rows = if let Some(path) = path {
                parse_jsonl_rows(&read_tape_content(&path)?)?
            } else {
                Vec::new()
            };
            self.rows.insert(tape_id.to_owned(), rows);
        }
        Ok(self.rows.get(tape_id).expect("cache entry inserted"))
    }

    fn path(&self, tape_id: &str) -> Option<&Path> {
        self.paths.get(tape_id).and_then(Option::as_deref)
    }
}

pub fn collect_dispatch_upstream_sessions(
    context: &RuntimeContext,
    indexes: &[SqliteIndex],
    sessions: &[Value],
) -> Result<(Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>), CliError> {
    let mut chain = Vec::new();
    let mut extras = Vec::new();
    let mut unresolved = Vec::new();
    let mut ambiguous = Vec::new();
    let mut seen_tapes = sessions
        .iter()
        .filter_map(|session| session.get("tape_id").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect::<HashSet<_>>();
    let mut rows_cache = DispatchTapeCache::default();
    let mut seen_hops = HashSet::new();
    let mut seen_unresolved = HashSet::new();
    let mut seen_ambiguous = HashSet::new();
    let mut recovery = crate::ingest::recovery::QueryRecovery::default();

    for session in sessions {
        let Some(tape_id) = session.get("tape_id").and_then(Value::as_str) else {
            continue;
        };
        let touches = session
            .get("touches")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for touch in touches {
            if touch.get("kind").and_then(Value::as_str) != Some("edit") {
                continue;
            }
            let Some(edit_offset) = touch.get("event_offset").and_then(Value::as_u64) else {
                continue;
            };
            let edit_turn =
                message_turn_before_offset(context, &mut rows_cache, tape_id, edit_offset)?;

            let recovered = recovery.lookup(context, tape_id)?;
            let mut first_hop = true;
            let (mut current_tape, mut current_turn) = if let Some(recovered) = recovered {
                let point = recovered
                    .recovered
                    .points
                    .iter()
                    .find(|p| p.old_offset == edit_offset)
                    .ok_or_else(|| {
                        CliError::new("native_recovery_error", "edit offset missing from recovery")
                    })?;
                (recovered.context_tape.clone(), point.turn)
            } else {
                (tape_id.to_string(), edit_turn)
            };
            let mut visited = HashSet::new();
            while let Some((received, received_tape, received_start)) = latest_received_in_history(
                context,
                &mut rows_cache,
                indexes,
                &current_tape,
                current_turn,
            )? {
                let candidates = sent_dispatch_candidates(
                    context,
                    &mut rows_cache,
                    indexes,
                    &mut recovery,
                    &received.uuid,
                )?;
                let parent = match candidates.as_slice() {
                    [] => {
                        let key = (received_tape.clone(), received.uuid.clone());
                        if seen_unresolved.insert(key) {
                            unresolved.push(json!({
                                "reason": "no_sender_observed",
                                "uuid": received.uuid,
                            }));
                        }
                        break;
                    }
                    [parent] => parent,
                    _ => {
                        let key = (received_tape.clone(), received.uuid.clone());
                        if seen_ambiguous.insert(key) {
                            let candidates = candidates
                                .iter()
                                .map(|(candidate, start)| {
                                    let location = rows_cache
                                        .path(&candidate.tape_id)
                                        .map(|path| path.display().to_string());
                                    json!({
                                        "session": candidate.tape_id,
                                        "sent_turn_index": *start + candidate.first_turn_index,
                                        "location": location,
                                    })
                                })
                                .collect::<Vec<_>>();
                            ambiguous.push(json!({
                                "received_uuid": received.uuid,
                                "received_session": received_tape,
                                "candidates": candidates,
                            }));
                        }
                        break;
                    }
                };
                let (parent, _) = parent;
                let hop_key = (
                    current_tape.clone(),
                    current_turn,
                    received.uuid.clone(),
                    received.first_turn_index,
                    parent.tape_id.clone(),
                    parent.first_turn_index,
                );
                if !visited.insert(hop_key.clone()) {
                    break;
                }

                if seen_hops.insert(hop_key) {
                    let current_start = message_turn_start(rows_cache.load(context, &current_tape)?);
                    let parent_start =
                        message_turn_start(rows_cache.load(context, &parent.tape_id)?);
                    let mut hop = json!({
                        "session": current_tape,
                        "edit_turn_index": current_start + current_turn,
                        "received_uuid": received.uuid,
                        "received_turn_index": received_start + received.first_turn_index,
                        "parent_session": parent.tape_id,
                        "parent_sent_turn_index": parent_start + parent.first_turn_index,
                    });
                    if first_hop && current_tape != tape_id {
                        hop["edit_session"] = json!(tape_id);
                        hop["edit_event_offset"] = json!(edit_offset);
                    }
                    if received_tape != current_tape {
                        hop["received_session"] = json!(received_tape);
                    }
                    chain.push(hop);
                }

                if seen_tapes.insert(parent.tape_id.clone())
                    && let Some(extra) = build_dispatch_session(context, &mut rows_cache, &parent)?
                {
                    extras.push(extra);
                }

                first_hop = false;
                current_tape = parent.tape_id.clone();
                current_turn = parent.first_turn_index;
            }
        }
    }

    Ok((chain, extras, unresolved, ambiguous))
}

fn message_turn_start(rows: &[TapeRow]) -> i64 {
    rows.iter()
        .find(|row| row.value["k"] == "meta")
        .and_then(|row| row.value["ingest_continuation"]["message_turn_start"].as_i64())
        .unwrap_or(0)
}

/// Merge the first occurrence of each UUID across the immutable predecessor
/// chain before selecting a parent. A later append must not renew a marker.
/// Keep both directions: an earlier sent occurrence suppresses a later receive.
fn first_dispatches_in_history(
    context: &RuntimeContext,
    cache: &mut DispatchTapeCache,
    indexes: &[SqliteIndex],
    tape_id: &str,
) -> Result<
    (
        HashMap<String, (DispatchLink, String, i64)>,
        HashSet<String>,
    ),
    CliError,
> {
    let mut tape = tape_id.to_string();
    let mut first = HashMap::<String, (DispatchLink, String, i64)>::new();
    let mut visited = HashSet::new();
    while visited.insert(tape.clone()) {
        let rows = cache.load(context, &tape)?;
        let start = message_turn_start(rows);
        for index in indexes {
            for link in index.dispatch_links_for_tape(&tape)? {
                let replace = match first.get(&link.uuid) {
                    None => true,
                    Some((seen, _, seen_start)) => {
                        let turn = start + link.first_turn_index;
                        let seen_turn = seen_start + seen.first_turn_index;
                        turn < seen_turn
                            || (turn == seen_turn
                                && (link.direction == DispatchDirection::Received
                                    || seen.direction == DispatchDirection::Sent))
                    }
                };
                if replace {
                    first.insert(link.uuid.clone(), (link, tape.clone(), start));
                }
            }
        }
        let previous = rows
            .iter()
            .find(|row| row.value["k"] == "meta")
            .and_then(|row| row.value["ingest_continuation"]["previous_tape_id"].as_str())
            .map(ToOwned::to_owned);
        let Some(previous) = previous else { break };
        tape = previous;
    }
    Ok((first, visited))
}

fn latest_received_in_history(
    context: &RuntimeContext,
    cache: &mut DispatchTapeCache,
    indexes: &[SqliteIndex],
    tape_id: &str,
    turn: i64,
) -> Result<Option<(DispatchLink, String, i64)>, CliError> {
    let cutoff = message_turn_start(cache.load(context, tape_id)?) + turn;
    let mut candidates: Vec<_> = first_dispatches_in_history(context, cache, indexes, tape_id)?
        .0
        .into_values()
        .filter(|(link, _, start)| {
            link.direction == DispatchDirection::Received && start + link.first_turn_index < cutoff
        })
        .collect();
    candidates.sort_by(|(left, _, ls), (right, _, rs)| {
        (rs + right.first_turn_index)
            .cmp(&(ls + left.first_turn_index))
            .then_with(|| left.uuid.cmp(&right.uuid))
    });
    Ok(candidates.into_iter().next())
}

fn sent_dispatch_candidates(
    context: &RuntimeContext,
    cache: &mut DispatchTapeCache,
    indexes: &[SqliteIndex],
    recovery: &mut crate::ingest::recovery::QueryRecovery,
    uuid: &str,
) -> Result<Vec<(DispatchLinkRow, i64)>, CliError> {
    // Examine the first occurrence in each continuation, including received
    // rows, so a same-turn receive can suppress an older sender. Independent
    // roots remain separate candidates; their turn ordinals are incomparable.
    let mut histories = Vec::new();
    let mut seen = HashSet::new();
    for index in indexes {
        for row in index.dispatch_links_for_uuid(uuid)? {
            // Old per-segment rows can have the wrong first direction. Resolve
            // their bound recovery context before considering any sender.
            let tape_id = recovery
                .lookup(context, &row.tape_id)?
                .map(|r| r.context_tape.clone())
                .unwrap_or(row.tape_id);
            if seen.insert(tape_id.clone()) {
                let (mut first, tapes) =
                    first_dispatches_in_history(context, cache, indexes, &tape_id)?;
                histories.push((tape_id, first.remove(uuid), tapes));
            }
        }
    }
    let mut candidates = Vec::new();
    for (tip, first, _) in &histories {
        if histories
            .iter()
            .any(|(other, _, tapes)| other != tip && tapes.contains(tip))
        {
            continue;
        }
        if let Some((link, tape_id, start)) = first
            && link.direction == DispatchDirection::Sent
        {
            candidates.push((
                DispatchLinkRow {
                    tape_id: tape_id.clone(),
                    uuid: link.uuid.clone(),
                    first_turn_index: link.first_turn_index,
                    direction: link.direction,
                },
                *start,
            ));
        }
    }
    candidates.sort_by(|(left, _), (right, _)| left.tape_id.cmp(&right.tape_id));
    Ok(candidates)
}

fn build_dispatch_session(
    context: &RuntimeContext,
    rows_cache: &mut DispatchTapeCache,
    link: &DispatchLinkRow,
) -> Result<Option<Value>, CliError> {
    let rows = rows_cache.load(context, &link.tape_id)?;
    let anchor_offset = message_turn_to_event_offset(rows, link.first_turn_index)
        .or_else(|| rows.last().map(|row| row.offset))
        .unwrap_or(0);
    let windows = event_window(rows, anchor_offset, TRANSCRIPT_WINDOW_RADIUS)
        .into_iter()
        .collect::<Vec<_>>();
    Ok(Some(json!({
        "tape_id": link.tape_id,
        "touch_count": 0,
        "latest_touch_timestamp": "",
        "touches": [],
        "windows": windows,
        "dispatch": {
            "uuid": link.uuid,
            "direction": dispatch_direction_name(link.direction),
            "first_turn_index": link.first_turn_index,
        }
    })))
}

fn message_turn_before_offset(
    context: &RuntimeContext,
    cache: &mut DispatchTapeCache,
    tape_id: &str,
    event_offset: u64,
) -> Result<i64, CliError> {
    let rows = cache.load(context, tape_id)?;
    let turn = rows
        .iter()
        .filter(|row| row.offset < event_offset && is_message_row(&row.value))
        .count() as i64;
    Ok(turn)
}

pub(crate) fn message_turn_to_event_offset(rows: &[TapeRow], turn_index: i64) -> Option<u64> {
    if turn_index < 0 {
        return None;
    }
    let mut current = 0_i64;
    for row in rows {
        if is_message_row(&row.value) {
            if current == turn_index {
                return Some(row.offset);
            }
            current += 1;
        }
    }
    None
}

pub(crate) fn is_message_row(value: &Value) -> bool {
    matches!(
        value.get("k").and_then(Value::as_str),
        Some("msg.in" | "msg.out")
    )
}

pub(crate) fn dispatch_direction_name(direction: DispatchDirection) -> &'static str {
    match direction {
        DispatchDirection::Received => "received",
        DispatchDirection::Sent => "sent",
    }
}

pub fn extract_dispatch_links_from_transcript(transcript: &str) -> Vec<DispatchLink> {
    let mut turn_index = 0_i64;
    let mut last_message_timestamp = None::<String>;
    let mut first_by_uuid = HashMap::<String, (i64, DispatchDirection)>::new();

    for line in transcript.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row["k"] == "meta" {
            last_message_timestamp = row["ingest_continuation"]["last_message_timestamp"]
                .as_str()
                .map(ToOwned::to_owned);
        }
        if row["type"] == "response_item"
            && matches!(
                row["payload"]["type"].as_str(),
                Some("custom_tool_call" | "function_call")
            )
        {
            let mut uuids = HashSet::new();
            for key in ["input", "arguments"] {
                collect_dispatch_uuids_anywhere(&row["payload"][key], &mut uuids);
            }
            for uuid in uuids {
                record_first_dispatch(
                    &mut first_by_uuid,
                    uuid,
                    turn_index,
                    DispatchDirection::Sent,
                );
            }
        }
        if row.get("k").and_then(Value::as_str) == Some("tool.call") {
            let mut dispatch_uuids = HashSet::new();
            if let Some(args) = row.get("args") {
                collect_dispatch_uuids_anywhere(args, &mut dispatch_uuids);
            }
            let timestamp = row.get("t").and_then(Value::as_str).unwrap_or("");
            let dispatch_turn = if last_message_timestamp.as_deref() == Some(timestamp) {
                turn_index.saturating_sub(1)
            } else {
                turn_index
            };
            for uuid in dispatch_uuids {
                record_first_dispatch(
                    &mut first_by_uuid,
                    uuid,
                    dispatch_turn,
                    DispatchDirection::Sent,
                );
            }
        }
        for message in extract_message_objects(&row) {
            // Native Claude user envelopes also carry tool results. Their
            // quoted output is neither a received message nor a sender call.
            let mut native_user;
            let message = if row["type"] == "user" {
                native_user = message.clone();
                if let Some(blocks) = native_user["content"].as_array_mut() {
                    blocks.retain(|block| block["type"] != "tool_result");
                }
                &native_user
            } else {
                message
            };
            let dispatch_in_message = extract_dispatch_direction_by_uuid(message);
            for (uuid, direction) in dispatch_in_message {
                record_first_dispatch(&mut first_by_uuid, uuid, turn_index, direction);
            }
            turn_index += 1;
        }
        if is_message_row(&row) {
            last_message_timestamp = Some(
                row.get("t")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        }
    }

    let mut out = first_by_uuid
        .into_iter()
        .map(|(uuid, (first_turn_index, direction))| DispatchLink {
            uuid,
            first_turn_index,
            direction,
        })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| {
        a.first_turn_index
            .cmp(&b.first_turn_index)
            .then_with(|| a.uuid.cmp(&b.uuid))
    });
    out
}

fn record_first_dispatch(
    first_by_uuid: &mut HashMap<String, (i64, DispatchDirection)>,
    uuid: String,
    turn_index: i64,
    direction: DispatchDirection,
) {
    match first_by_uuid.get(&uuid).copied() {
        None => {
            first_by_uuid.insert(uuid, (turn_index, direction));
        }
        Some((seen_turn, seen_dir)) => {
            let should_replace = turn_index < seen_turn
                || (turn_index == seen_turn
                    && seen_dir == DispatchDirection::Sent
                    && direction == DispatchDirection::Received);
            if should_replace {
                first_by_uuid.insert(uuid, (turn_index, direction));
            }
        }
    }
}

pub(crate) fn extract_message_objects<'a>(row: &'a Value) -> Vec<&'a Value> {
    let mut out = Vec::new();
    let Some(obj) = row.as_object() else {
        return out;
    };

    if matches!(
        obj.get("type").and_then(Value::as_str),
        Some("message" | "assistant" | "user")
    ) && let Some(message) = obj.get("message")
    {
        out.push(message);
    }

    if obj.get("type").and_then(Value::as_str) == Some("response_item")
        && let Some(payload) = obj.get("payload")
        && payload.get("type").and_then(Value::as_str) == Some("message")
    {
        out.push(payload);
    }

    let is_normalized_message = matches!(
        obj.get("k").and_then(Value::as_str),
        Some("msg.in" | "msg.out")
    );
    let has_role = obj.get("role").and_then(Value::as_str).is_some();
    let has_content = obj.get("content").is_some();
    if is_normalized_message || (has_role && has_content) {
        out.push(row);
    }

    out
}

pub(crate) fn extract_dispatch_direction_by_uuid(
    message: &Value,
) -> HashMap<String, DispatchDirection> {
    let mut all = HashSet::new();
    collect_dispatch_uuids_anywhere(message, &mut all);
    if all.is_empty() {
        return HashMap::new();
    }

    let mut surface = HashSet::new();
    collect_dispatch_uuids_on_message_surface(message, &mut surface);

    let mut out = HashMap::new();
    for uuid in all {
        let direction = if surface.contains(&uuid) {
            DispatchDirection::Received
        } else {
            DispatchDirection::Sent
        };
        out.insert(uuid, direction);
    }
    out
}

pub(crate) fn collect_dispatch_uuids_on_message_surface(
    message: &Value,
    out: &mut HashSet<String>,
) {
    if let Some(content) = message.get("content") {
        collect_dispatch_uuids_from_surface_content(content, out);
    }
    if let Some(text) = message.get("text").and_then(Value::as_str) {
        for uuid in extract_dispatch_uuids_from_text(text) {
            out.insert(uuid);
        }
    }
}

pub(crate) fn collect_dispatch_uuids_from_surface_content(
    content: &Value,
    out: &mut HashSet<String>,
) {
    match content {
        Value::String(text) => {
            for uuid in extract_dispatch_uuids_from_text(text) {
                out.insert(uuid);
            }
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(text) => {
                        for uuid in extract_dispatch_uuids_from_text(text) {
                            out.insert(uuid);
                        }
                    }
                    Value::Object(obj) => {
                        for key in ["text", "input_text", "output_text"] {
                            if let Some(text) = obj.get(key).and_then(Value::as_str) {
                                for uuid in extract_dispatch_uuids_from_text(text) {
                                    out.insert(uuid);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn collect_dispatch_uuids_anywhere(value: &Value, out: &mut HashSet<String>) {
    match value {
        Value::String(text) => {
            for uuid in extract_dispatch_uuids_from_text(text) {
                out.insert(uuid);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_dispatch_uuids_anywhere(item, out);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_dispatch_uuids_anywhere(item, out);
            }
        }
        _ => {}
    }
}

pub(crate) fn extract_dispatch_uuids_from_text(text: &str) -> Vec<String> {
    const PREFIX: &str = "<engram-src id=\"";
    const SUFFIX: &str = "\"/>";
    let mut out = Vec::new();
    let normalized = text.replace("\\\"", "\"");
    let mut cursor = 0usize;
    while let Some(prefix_pos) = normalized[cursor..].find(PREFIX) {
        let start = cursor + prefix_pos + PREFIX.len();
        let Some(end_rel) = normalized[start..].find(SUFFIX) else {
            break;
        };
        let end = start + end_rel;
        let candidate = &normalized[start..end];
        if is_uuid_format(candidate) {
            out.push(candidate.to_string());
        }
        cursor = end + SUFFIX.len();
    }
    out
}

pub(crate) fn is_uuid_format(raw: &str) -> bool {
    if raw.len() != 36 {
        return false;
    }
    for (idx, ch) in raw.char_indices() {
        if [8, 13, 18, 23].contains(&idx) {
            if ch != '-' {
                return false;
            }
        } else if !ch.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}
