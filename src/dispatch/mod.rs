use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::index::{DispatchDirection, DispatchLink, DispatchLinkRow, SqliteIndex};
use crate::store::tapes::{TapeRow, event_window, load_tape_rows_cached};
use crate::{CliError, RuntimeContext};

const TRANSCRIPT_WINDOW_RADIUS: usize = 2;

pub fn collect_dispatch_upstream_sessions(
    context: &RuntimeContext,
    indexes: &[SqliteIndex],
    sessions: &[Value],
) -> Result<(Vec<Value>, Vec<Value>), CliError> {
    let mut chain = Vec::new();
    let mut extras = Vec::new();
    let mut seen_tapes = sessions
        .iter()
        .filter_map(|session| session.get("tape_id").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect::<HashSet<_>>();
    let mut rows_cache = HashMap::<String, Vec<TapeRow>>::new();
    let mut seen_hops = HashSet::new();

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

            let mut current_tape = tape_id.to_string();
            let mut current_turn = edit_turn;
            let mut visited = HashSet::new();
            while let Some(received) =
                latest_received_dispatch_before_turn(indexes, &current_tape, current_turn)?
            {
                let Some(parent) = sent_dispatch_for_uuid(indexes, &received.uuid)? else {
                    break;
                };
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
                    chain.push(json!({
                        "session": current_tape,
                        "edit_turn_index": current_turn,
                        "received_uuid": received.uuid,
                        "received_turn_index": received.first_turn_index,
                        "parent_session": parent.tape_id,
                        "parent_sent_turn_index": parent.first_turn_index,
                    }));
                }

                if seen_tapes.insert(parent.tape_id.clone())
                    && let Some(extra) = build_dispatch_session(context, &mut rows_cache, &parent)?
                {
                    extras.push(extra);
                }

                current_tape = parent.tape_id;
                current_turn = parent.first_turn_index;
            }
        }
    }

    Ok((chain, extras))
}

fn latest_received_dispatch_before_turn(
    indexes: &[SqliteIndex],
    tape_id: &str,
    turn_index: i64,
) -> Result<Option<DispatchLink>, CliError> {
    let mut candidates = Vec::new();
    for index in indexes {
        if let Some(link) = index.latest_received_dispatch_before_turn(tape_id, turn_index)? {
            candidates.push(link);
        }
    }
    candidates.sort_by(|left, right| {
        right
            .first_turn_index
            .cmp(&left.first_turn_index)
            .then_with(|| left.uuid.cmp(&right.uuid))
    });
    Ok(candidates.into_iter().next())
}

fn sent_dispatch_for_uuid(
    indexes: &[SqliteIndex],
    uuid: &str,
) -> Result<Option<DispatchLinkRow>, CliError> {
    let mut candidates = Vec::new();
    for index in indexes {
        if let Some(link) = index.sent_dispatch_for_uuid(uuid)? {
            candidates.push(link);
        }
    }
    candidates.sort_by(|left, right| {
        right
            .first_turn_index
            .cmp(&left.first_turn_index)
            .then_with(|| left.tape_id.cmp(&right.tape_id))
    });
    Ok(candidates.into_iter().next())
}

pub(crate) fn build_dispatch_session(
    context: &RuntimeContext,
    rows_cache: &mut HashMap<String, Vec<TapeRow>>,
    link: &DispatchLinkRow,
) -> Result<Option<Value>, CliError> {
    let rows = load_tape_rows_cached(context, rows_cache, &link.tape_id)?;
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

pub(crate) fn message_turn_before_offset(
    context: &RuntimeContext,
    cache: &mut HashMap<String, Vec<TapeRow>>,
    tape_id: &str,
    event_offset: u64,
) -> Result<i64, CliError> {
    let rows = load_tape_rows_cached(context, cache, tape_id)?;
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
    let mut first_by_uuid = HashMap::<String, (i64, DispatchDirection)>::new();

    for line in transcript.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        for message in extract_message_objects(&row) {
            let dispatch_in_message = extract_dispatch_direction_by_uuid(message);
            for (uuid, direction) in dispatch_in_message {
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
            turn_index += 1;
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

pub(crate) fn extract_message_objects<'a>(row: &'a Value) -> Vec<&'a Value> {
    let mut out = Vec::new();
    let Some(obj) = row.as_object() else {
        return out;
    };

    if obj.get("type").and_then(Value::as_str) == Some("message")
        && let Some(message) = obj.get("message")
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
