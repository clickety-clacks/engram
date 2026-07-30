use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use chrono::Utc;
use serde_json::{Map, Value, json};

use crate::anchor::fingerprint_token_hashes;
use crate::dispatch::message_turn_to_event_offset;
use crate::index::lineage::{
    Cardinality, EvidenceFragmentRef, EvidenceKind, LocationDelta, StoredEdgeClass,
};
use crate::index::{DispatchDirection, EdgeRow, SqliteIndex};
use crate::query::explain::{
    ExplainResult, ExplainTraversal, PrettyConfidenceTier, explain_across_indexes_by_anchor,
    pretty_tier,
};
pub use crate::store::tapes::{TapeRow, parse_jsonl_rows, print_json};
use crate::store::tapes::{event_window, read_tape_content, resolve_tape_path, tape_id_from_path};
use crate::{CliError, RuntimeContext, path_string};

pub const MAX_QUERY_WINDOW_ANCHORS: usize = 16;
const DEFAULT_WINDOW_BEFORE_RATIO_NUM: usize = 3;
const DEFAULT_WINDOW_BEFORE_RATIO_DEN: usize = 4;
const SAFE_RESULT_SESSION_THRESHOLD: usize = 25;
const TRANSCRIPT_WINDOW_RADIUS: usize = 2;

#[derive(Debug, Clone)]
pub enum ExplainTarget {
    FileRange { file: String, start: u32, end: u32 },
    FileWhole { file: String },
    Literal(String),
}

pub fn open_query_indexes(context: &RuntimeContext) -> Result<Vec<SqliteIndex>, CliError> {
    let mut indexes = Vec::new();
    if context.db_path.exists() {
        indexes.push(SqliteIndex::open_reader(&path_string(&context.db_path))?);
    }
    for store in &context.additional_stores {
        if store.exists() {
            indexes.push(SqliteIndex::open_reader(&path_string(store))?);
        }
    }
    if indexes.is_empty() {
        return Err(rusqlite::Error::InvalidPath(context.db_path.clone()).into());
    }
    Ok(indexes)
}

pub fn classify_explain_target(
    cwd: &Path,
    _context: &RuntimeContext,
    _indexes: &[SqliteIndex],
    target: &str,
    anchor_mode: bool,
) -> Result<ExplainTarget, CliError> {
    if anchor_mode {
        return Ok(ExplainTarget::Literal(target.to_string()));
    }

    if has_span_shape(target) {
        let (file, start, end) = parse_file_range_target(target)?;
        if cwd.join(file).exists() {
            return Ok(ExplainTarget::FileRange {
                file: file.to_string(),
                start,
                end,
            });
        }
    }

    if cwd.join(target).is_file() {
        return Ok(ExplainTarget::FileWhole {
            file: target.to_string(),
        });
    }

    Ok(ExplainTarget::Literal(target.to_string()))
}

pub(crate) fn has_span_shape(target: &str) -> bool {
    target
        .rsplit_once(':')
        .is_some_and(|(_, rhs)| rhs.contains('-'))
}

pub fn collect_anchor_scores(
    indexes: &[SqliteIndex],
    anchors: &[String],
) -> Result<HashMap<String, f32>, CliError> {
    if anchors.is_empty() {
        return Ok(HashMap::new());
    }

    let mut by_tape: HashMap<String, HashSet<String>> = HashMap::new();
    for anchor in anchors {
        for index in indexes {
            for fragment in index.evidence_for_anchor(anchor)? {
                by_tape
                    .entry(fragment.tape_id)
                    .or_default()
                    .insert(anchor.clone());
            }
        }
    }

    let denom = anchors.len() as f32;
    let mut out = HashMap::new();
    for (tape_id, hits) in by_tape {
        out.insert(tape_id, hits.len() as f32 / denom);
    }
    Ok(out)
}

pub fn collect_grep_matches(
    context: &RuntimeContext,
    indexes: &[SqliteIndex],
    pattern: &str,
) -> Result<(Vec<Value>, HashMap<String, GrepRank>), CliError> {
    let mut tape_ids = HashSet::new();
    for index in indexes {
        for tape_id in index.referenced_tape_ids()? {
            tape_ids.insert(tape_id);
        }
    }
    for dir in &context.tape_lookup_dirs {
        if !dir.exists() {
            continue;
        }
        let entries = fs::read_dir(dir).map_err(|err| CliError::io("read_dir_error", err))?;
        for entry in entries {
            let entry = entry.map_err(|err| CliError::io("read_dir_error", err))?;
            if let Some(tape_id) = tape_id_from_path(&entry.path()) {
                tape_ids.insert(tape_id);
            }
        }
    }

    let mut raw_sessions = Vec::new();
    let mut rank_by_session = HashMap::new();

    for tape_id in tape_ids {
        let Some(path) = resolve_tape_path(context, &tape_id) else {
            continue;
        };
        let content = read_tape_content(&path)?;
        let lines = content.lines().collect::<Vec<_>>();
        let rows = parse_jsonl_rows(&content)?;
        let provenance_offsets = rows
            .iter()
            .filter(|row| is_provenance_row(&row.value))
            .map(|row| row.offset)
            .collect::<HashSet<_>>();
        let provenance_event_count = provenance_offsets.len();

        let mut first_match = None;
        let mut first_provenance_match = None;
        let mut match_count = 0usize;
        let mut provenance_match_count = 0usize;
        for (idx, line) in lines.iter().enumerate() {
            if line.contains(pattern) {
                match_count += 1;
                if first_match.is_none() {
                    first_match = Some(idx as u64);
                }
                let offset = idx as u64;
                if provenance_offsets.contains(&offset) {
                    provenance_match_count += 1;
                    if first_provenance_match.is_none() {
                        first_provenance_match = Some(offset);
                    }
                }
            }
        }
        let Some(first_match) = first_match else {
            continue;
        };

        let anchor_offset = first_provenance_match.unwrap_or(first_match);
        let windows = event_window(&rows, anchor_offset, TRANSCRIPT_WINDOW_RADIUS)
            .into_iter()
            .collect::<Vec<_>>();
        raw_sessions.push(json!({
            "tape_id": tape_id,
            "tape_present_locally": true,
            "touch_count": match_count,
            "latest_touch_timestamp": extract_latest_timestamp_from_rows(&rows),
            "touches": [],
            "windows": windows,
        }));
        rank_by_session.insert(
            raw_sessions
                .last()
                .and_then(|v| v.get("tape_id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            GrepRank {
                provenance_match_count,
                match_count,
                provenance_event_count,
            },
        );
    }

    Ok((raw_sessions, rank_by_session))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GrepRank {
    provenance_match_count: usize,
    pub match_count: usize,
    provenance_event_count: usize,
}

pub fn compare_grep_sessions(
    a: &Value,
    b: &Value,
    rank_by_session: &HashMap<String, GrepRank>,
) -> std::cmp::Ordering {
    let a_session_id = a.get("session_id").and_then(Value::as_str).unwrap_or("");
    let b_session_id = b.get("session_id").and_then(Value::as_str).unwrap_or("");
    let a_rank = rank_by_session
        .get(a_session_id)
        .copied()
        .unwrap_or_default();
    let b_rank = rank_by_session
        .get(b_session_id)
        .copied()
        .unwrap_or_default();
    let a_ts = a.get("timestamp").and_then(Value::as_str).unwrap_or("");
    let b_ts = b.get("timestamp").and_then(Value::as_str).unwrap_or("");

    b_rank
        .provenance_match_count
        .cmp(&a_rank.provenance_match_count)
        .then_with(|| b_rank.match_count.cmp(&a_rank.match_count))
        .then_with(|| {
            b_rank
                .provenance_event_count
                .cmp(&a_rank.provenance_event_count)
        })
        .then_with(|| b_ts.cmp(a_ts))
        .then_with(|| a_session_id.cmp(b_session_id))
}

pub fn compare_explain_sessions(a: &Value, b: &Value) -> std::cmp::Ordering {
    let a_touch_count = a
        .get("touches")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let b_touch_count = b
        .get("touches")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let a_ts = a.get("timestamp").and_then(Value::as_str).unwrap_or("");
    let b_ts = b.get("timestamp").and_then(Value::as_str).unwrap_or("");
    let a_depth = a.get("depth").and_then(Value::as_u64).unwrap_or(0);
    let b_depth = b.get("depth").and_then(Value::as_u64).unwrap_or(0);
    let a_score = a.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
    let b_score = b.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
    let a_session_id = a.get("session_id").and_then(Value::as_str).unwrap_or("");
    let b_session_id = b.get("session_id").and_then(Value::as_str).unwrap_or("");

    b_touch_count
        .cmp(&a_touch_count)
        .then_with(|| b_ts.cmp(a_ts))
        .then_with(|| a_depth.cmp(&b_depth))
        .then_with(|| {
            b_score
                .partial_cmp(&a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| a_session_id.cmp(b_session_id))
}

pub(crate) fn is_provenance_row(value: &Value) -> bool {
    matches!(
        value.get("k").and_then(Value::as_str),
        Some("code.edit" | "code.read" | "span.link")
    )
}

pub fn format_sessions_for_agent(
    context: &RuntimeContext,
    indexes: &[SqliteIndex],
    raw_sessions: Vec<Value>,
    score_by_session: &HashMap<String, f32>,
    grep: Option<&str>,
) -> Result<Vec<Value>, CliError> {
    let mut out = Vec::new();
    let line_count = context.peek_default_lines.max(1);

    for raw in raw_sessions {
        let Some(session_id) = raw.get("tape_id").and_then(Value::as_str) else {
            continue;
        };

        let tape_path = resolve_tape_path(context, session_id);
        let (rows, raw_text, total_lines) = if let Some(path) = tape_path.as_ref() {
            let content = read_tape_content(path)?;
            let rows = parse_jsonl_rows(&content)?;
            let total = content.lines().count();
            (rows, content, total)
        } else {
            (Vec::new(), String::new(), 0usize)
        };

        let content_lines = raw_text.lines().collect::<Vec<_>>();
        let anchor_line = raw
            .get("windows")
            .and_then(Value::as_array)
            .and_then(|windows| windows.first())
            .and_then(|window| window.get("touch_offset"))
            .and_then(Value::as_u64)
            .map(|offset| offset as usize + 1)
            .unwrap_or(1);

        let default_before =
            line_count * DEFAULT_WINDOW_BEFORE_RATIO_NUM / DEFAULT_WINDOW_BEFORE_RATIO_DEN;
        let window_start = anchor_line.saturating_sub(default_before).max(1);
        let window_end = if total_lines == 0 {
            0
        } else {
            usize::min(
                total_lines,
                window_start.saturating_add(line_count).saturating_sub(1),
            )
        };

        let window_texts = if total_lines == 0 || window_end == 0 {
            Vec::new()
        } else {
            ((window_start - 1)..window_end)
                .map(|idx| content_lines.get(idx).copied().unwrap_or_default())
                .collect::<Vec<_>>()
        };

        if let Some(pattern) = grep
            && !window_texts.iter().any(|text| text.contains(pattern))
        {
            continue;
        }

        let mut files_touched = raw
            .get("touches")
            .and_then(Value::as_array)
            .map(|touches| {
                touches
                    .iter()
                    .filter_map(|touch| touch.get("file_path").and_then(Value::as_str))
                    .filter(|file| !file.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        if files_touched.is_empty() {
            for file in collect_files_touched_from_rows(&rows) {
                files_touched.insert(file);
            }
        }
        let mut files_touched = files_touched.into_iter().collect::<Vec<_>>();
        files_touched.sort();

        let (refs_up, refs_down) = dispatch_ref_counts(indexes, session_id)?;
        let timestamp = raw
            .get("latest_touch_timestamp")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| extract_latest_timestamp_from_rows(&rows));
        let touches = raw.get("touches").cloned().unwrap_or_else(|| json!([]));

        out.push(json!({
            "session_id": session_id,
            "timestamp": timestamp,
            "window_start": window_start,
            "window_end": window_end,
            "total_lines": total_lines,
            "confidence": score_by_session.get(session_id).copied().unwrap_or(0.0),
            "refs_up": refs_up,
            "refs_down": refs_down,
            "files_touched": files_touched,
            "touches": touches,
        }));
    }

    Ok(out)
}

pub(crate) fn dispatch_ref_counts(
    indexes: &[SqliteIndex],
    tape_id: &str,
) -> Result<(usize, usize), CliError> {
    let mut up = 0usize;
    let mut down = 0usize;
    let mut seen = HashSet::new();
    for index in indexes {
        for link in index.dispatch_links_for_tape(tape_id)? {
            let received = matches!(link.direction, DispatchDirection::Received);
            if !seen.insert((link.uuid, received)) {
                continue;
            }
            if received {
                up += 1;
            } else {
                down += 1;
            }
        }
    }
    Ok((up, down))
}

pub fn extract_latest_timestamp_from_rows(rows: &[TapeRow]) -> String {
    rows.iter()
        .filter_map(|row| row.value.get("t").and_then(Value::as_str))
        .max()
        .unwrap_or("")
        .to_string()
}

pub(crate) fn collect_files_touched_from_rows(rows: &[TapeRow]) -> Vec<String> {
    let mut files = HashSet::new();
    for row in rows {
        if let Some(file) = row.value.get("file").and_then(Value::as_str) {
            files.insert(file.to_string());
        }
        if let Some(file) = row.value.get("from_file").and_then(Value::as_str) {
            files.insert(file.to_string());
        }
        if let Some(file) = row.value.get("to_file").and_then(Value::as_str) {
            files.insert(file.to_string());
        }
    }
    let mut out = files.into_iter().collect::<Vec<_>>();
    out.sort();
    out
}

pub fn apply_session_truncation(
    sessions: Vec<Value>,
    limit: Option<usize>,
    offset: usize,
    default_limit: usize,
) -> (Vec<Value>, usize, usize, Value, bool) {
    let total = sessions.len();
    let start = usize::min(offset, total);
    let remaining = total.saturating_sub(start);
    let max_return = usize::min(
        limit.unwrap_or(default_limit),
        SAFE_RESULT_SESSION_THRESHOLD,
    );
    let returned_count = usize::min(remaining, max_return);

    let mut timestamps = sessions
        .iter()
        .filter_map(|session| session.get("timestamp").and_then(Value::as_str))
        .filter(|timestamp| !timestamp.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    timestamps.sort();

    let time_range = if timestamps.is_empty() {
        json!({"start": Value::Null, "end": Value::Null})
    } else {
        json!({
            "start": timestamps.first().cloned().unwrap_or_default(),
            "end": timestamps.last().cloned().unwrap_or_default(),
        })
    };

    let truncated = start > 0 || start.saturating_add(returned_count) < total;
    let sessions = sessions
        .into_iter()
        .skip(start)
        .take(returned_count)
        .collect::<Vec<_>>();

    (sessions, returned_count, total, time_range, truncated)
}

#[derive(Debug, Clone)]
pub struct DateFilter {
    since: Option<chrono::DateTime<Utc>>,
    until: Option<chrono::DateTime<Utc>>,
}

impl DateFilter {
    pub fn parse(since: Option<&str>, until: Option<&str>) -> Result<Self, CliError> {
        Ok(Self {
            since: parse_date_bound(since, DateBound::Since)?,
            until: parse_date_bound(until, DateBound::Until)?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum DateBound {
    Since,
    Until,
}

pub(crate) fn parse_date_bound(
    raw: Option<&str>,
    bound: DateBound,
) -> Result<Option<chrono::DateTime<Utc>>, CliError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    if let Ok(value) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(value.with_timezone(&Utc)));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        let dt = match bound {
            DateBound::Since => date.and_hms_opt(0, 0, 0),
            DateBound::Until => date.and_hms_opt(23, 59, 59),
        }
        .ok_or_else(|| CliError::new("invalid_date", raw.to_string()))?;
        return Ok(Some(chrono::DateTime::<Utc>::from_naive_utc_and_offset(
            dt, Utc,
        )));
    }
    Err(CliError::new(
        "invalid_date",
        format!("invalid date format `{raw}`"),
    ))
}

pub fn session_matches_date_filter(session: &Value, filter: &DateFilter) -> bool {
    let Some(raw_ts) = session.get("timestamp").and_then(Value::as_str) else {
        return true;
    };
    if raw_ts.is_empty() {
        return true;
    }
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(raw_ts) else {
        return true;
    };
    let ts = ts.with_timezone(&Utc);
    if let Some(since) = filter.since
        && ts < since
    {
        return false;
    }
    if let Some(until) = filter.until
        && ts > until
    {
        return false;
    }
    true
}

pub fn annotate_chain_fields(sessions: &mut [Value], dispatch_lineage: &[Value]) {
    let ids = sessions
        .iter()
        .filter_map(|session| session.get("session_id").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect::<HashSet<_>>();

    let mut parent_of = HashMap::<String, String>::new();
    let mut children_of = HashMap::<String, Vec<String>>::new();
    for link in dispatch_lineage {
        let Some(child) = link.get("session").and_then(Value::as_str) else {
            continue;
        };
        let Some(parent) = link.get("parent_session").and_then(Value::as_str) else {
            continue;
        };
        if !ids.contains(child) || !ids.contains(parent) {
            continue;
        }
        parent_of.insert(child.to_string(), parent.to_string());
        children_of
            .entry(parent.to_string())
            .or_default()
            .push(child.to_string());
    }
    for children in children_of.values_mut() {
        children.sort();
    }

    let mut root_for = HashMap::<String, String>::new();
    for id in &ids {
        let mut current = id.clone();
        while let Some(parent) = parent_of.get(&current) {
            current = parent.clone();
        }
        root_for.insert(id.clone(), current);
    }
    let mut chain_len = HashMap::<String, usize>::new();
    for root in root_for.values() {
        *chain_len.entry(root.clone()).or_insert(0) += 1;
    }

    for session in sessions {
        let Some(id) = session
            .get("session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        let mut depth = 0usize;
        let mut current = id.clone();
        while let Some(parent) = parent_of.get(&current) {
            depth += 1;
            current = parent.clone();
        }
        let parent = parent_of.get(&id).cloned();
        let children = children_of.get(&id).cloned().unwrap_or_default();
        let root = root_for.get(&id).cloned().unwrap_or_else(|| id.clone());
        let length = chain_len.get(&root).copied().unwrap_or(1);

        if let Some(obj) = session.as_object_mut() {
            obj.insert("depth".to_string(), json!(depth));
            obj.insert(
                "parent".to_string(),
                parent.map(Value::from).unwrap_or(Value::Null),
            );
            obj.insert("children".to_string(), json!(children));
            obj.insert("chain_length".to_string(), json!(length));
        }
    }
}

pub fn build_chain_metadata(sessions: &[Value]) -> Vec<Value> {
    let mut parent_of = HashMap::<String, String>::new();
    for session in sessions {
        if let (Some(id), Some(parent)) = (
            session.get("session_id").and_then(Value::as_str),
            session.get("parent").and_then(Value::as_str),
        ) {
            parent_of.insert(id.to_string(), parent.to_string());
        }
    }
    let mut by_root = HashMap::<String, Vec<Value>>::new();
    let mut root_order = Vec::<String>::new();
    for session in sessions {
        let Some(id) = session.get("session_id").and_then(Value::as_str) else {
            continue;
        };
        let mut root = id.to_string();
        while let Some(parent) = parent_of.get(&root) {
            root = parent.clone();
        }
        if !root_order.iter().any(|value| value == &root) {
            root_order.push(root.clone());
        }
        by_root.entry(root).or_default().push(json!({
            "session_id": id,
            "depth": session.get("depth").cloned().unwrap_or_else(|| json!(0)),
            "parent": session.get("parent").cloned().unwrap_or(Value::Null),
            "children": session.get("children").cloned().unwrap_or_else(|| json!([])),
        }));
    }
    let mut out = Vec::new();
    for root in root_order {
        let mut descendants = by_root.remove(&root).unwrap_or_default();
        descendants.sort_by(|a, b| {
            let ad = a.get("depth").and_then(Value::as_u64).unwrap_or(0);
            let bd = b.get("depth").and_then(Value::as_u64).unwrap_or(0);
            ad.cmp(&bd)
        });
        out.push(json!({
            "root_session_id": root,
            "descendants": descendants,
        }));
    }
    out
}

pub fn default_peek_anchor_line(
    indexes: &[SqliteIndex],
    session_id: &str,
    rows: &[TapeRow],
) -> usize {
    let mut received_links = Vec::new();
    for index in indexes {
        if let Ok(links) = index.dispatch_links_for_tape(session_id) {
            received_links.extend(
                links
                    .into_iter()
                    .filter(|link| matches!(link.direction, DispatchDirection::Received)),
            );
        }
    }
    received_links.sort_by(|left, right| {
        left.first_turn_index
            .cmp(&right.first_turn_index)
            .then_with(|| left.uuid.cmp(&right.uuid))
    });
    if let Some(received) = received_links.into_iter().next() {
        if let Some(offset) = message_turn_to_event_offset(rows, received.first_turn_index)
            && let Some(pos) = rows.iter().position(|row| row.offset == offset)
        {
            return pos + 1;
        }
    }
    if rows.is_empty() { 1 } else { 1 }
}

pub fn collect_touch_evidence(
    indexes: &[SqliteIndex],
    direct: &[EvidenceFragmentRef],
    touched_anchors: &[String],
) -> Result<Vec<EvidenceFragmentRef>, CliError> {
    let mut dedup = HashSet::new();
    let mut out = Vec::new();

    for fragment in direct {
        let key = touch_key(fragment);
        if dedup.insert(key) {
            out.push(fragment.clone());
        }
    }

    for anchor in touched_anchors {
        for index in indexes {
            for fragment in index.evidence_for_anchor(anchor)? {
                let key = touch_key(&fragment);
                if dedup.insert(key) {
                    out.push(fragment);
                }
            }
        }
    }

    Ok(out)
}

pub fn explain_across_indexes(
    indexes: &[SqliteIndex],
    anchors: &[String],
    traversal: ExplainTraversal,
    include_forensics: bool,
) -> Result<ExplainResult, CliError> {
    let mut direct = Vec::new();
    let mut lineage = Vec::new();
    let mut touched_anchors = Vec::new();

    let mut seen_direct = HashSet::new();
    let mut seen_lineage = HashSet::new();
    let mut seen_anchors = HashSet::new();

    for anchor in anchors {
        if seen_anchors.insert(anchor.clone()) {
            touched_anchors.push(anchor.clone());
        }
    }

    let result = explain_across_indexes_by_anchor(indexes, anchors, traversal, include_forensics)?;
    for fragment in result.direct {
        let key = touch_key(&fragment);
        if seen_direct.insert(key) {
            direct.push(fragment);
        }
    }
    for edge in result.lineage {
        let key = crate::index::semantic_edge_key(&edge);
        if seen_lineage.insert(key) {
            lineage.push(edge);
        }
    }
    for anchor in result.touched_anchors {
        if seen_anchors.insert(anchor.clone()) {
            touched_anchors.push(anchor);
        }
    }
    direct.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.tape_id.cmp(&b.tape_id))
            .then_with(|| a.event_offset.cmp(&b.event_offset))
    });

    Ok(ExplainResult {
        direct,
        lineage,
        touched_anchors,
    })
}

pub(crate) fn touch_key(fragment: &EvidenceFragmentRef) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        fragment.tape_id,
        fragment.event_offset,
        evidence_kind_name(fragment.kind),
        fragment.file_path,
        fragment.timestamp
    )
}

pub fn build_session_windows(
    context: &RuntimeContext,
    touches: Vec<EvidenceFragmentRef>,
) -> Result<Vec<Value>, CliError> {
    let mut by_tape: HashMap<String, Vec<EvidenceFragmentRef>> = HashMap::new();
    for touch in touches {
        by_tape
            .entry(touch.tape_id.clone())
            .or_default()
            .push(touch);
    }

    let mut sessions = Vec::new();
    for (tape_id, mut tape_touches) in by_tape {
        tape_touches.sort_by_key(|t| t.event_offset);
        let tape_path = resolve_tape_path(context, &tape_id);
        let windows = if let Some(tape_path) = tape_path.as_ref() {
            let content = read_tape_content(&tape_path)?;
            let rows = parse_jsonl_rows(&content)?;
            tape_touches
                .iter()
                .filter_map(|touch| {
                    event_window(&rows, touch.event_offset, TRANSCRIPT_WINDOW_RADIUS)
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let latest_touch_timestamp = tape_touches
            .iter()
            .map(|touch| touch.timestamp.as_str())
            .max()
            .unwrap_or("")
            .to_string();

        let touches_json = tape_touches
            .iter()
            .map(|touch| {
                json!({
                    "event_offset": touch.event_offset,
                    "kind": evidence_kind_name(touch.kind),
                    "file_path": touch.file_path,
                    "timestamp": touch.timestamp,
                })
            })
            .collect::<Vec<_>>();

        sessions.push(json!({
            "tape_id": tape_id,
            "tape_present_locally": tape_path.is_some(),
            "touch_count": tape_touches.len(),
            "latest_touch_timestamp": latest_touch_timestamp,
            "touches": touches_json,
            "windows": windows,
        }));
    }

    sessions.sort_by(|a, b| {
        let a_touch_count = a.get("touch_count").and_then(Value::as_u64).unwrap_or(0);
        let b_touch_count = b.get("touch_count").and_then(Value::as_u64).unwrap_or(0);
        let a_latest = a
            .get("latest_touch_timestamp")
            .and_then(Value::as_str)
            .unwrap_or("");
        let b_latest = b
            .get("latest_touch_timestamp")
            .and_then(Value::as_str)
            .unwrap_or("");
        b_touch_count
            .cmp(&a_touch_count)
            .then_with(|| b_latest.cmp(a_latest))
    });

    Ok(sessions)
}

pub fn print_pretty_explain(
    target: &str,
    lineage: &[EdgeRow],
    sessions: &[Value],
    tombstones: &[Value],
) {
    println!("target: {target}");
    println!("sessions: {}", sessions.len());
    for session in sessions {
        let tape_id = session.get("tape_id").and_then(Value::as_str).unwrap_or("");
        let touch_count = session
            .get("touch_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!("- tape={} touches={}", tape_id, touch_count);
    }

    println!("lineage:");
    for edge in lineage {
        let tier = pretty_tier(
            edge.confidence,
            matches!(edge.location_delta, LocationDelta::Moved),
            edge.stored_class == StoredEdgeClass::LocationOnly,
        );
        println!(
            "- {} -> {} conf={:.2} tier={} agent_link={}",
            edge.from_anchor,
            edge.to_anchor,
            edge.confidence,
            pretty_tier_name(tier),
            edge.agent_link
        );
    }

    if !tombstones.is_empty() {
        println!("tombstones:");
        for tombstone in tombstones {
            println!("- {tombstone}");
        }
    }
}

pub fn derive_anchor_candidates(span_texts: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for span_text in span_texts {
        for token in fingerprint_token_hashes(span_text) {
            if seen.insert(token.clone()) {
                out.push(token);
            }
        }
    }

    sample_anchor_candidates(out, MAX_QUERY_WINDOW_ANCHORS)
}

pub(crate) fn sample_anchor_candidates(anchors: Vec<String>, max_anchors: usize) -> Vec<String> {
    if anchors.len() <= max_anchors || max_anchors == 0 {
        return anchors;
    }

    let last = anchors.len() - 1;
    let mut out = Vec::with_capacity(max_anchors);
    let mut seen = HashSet::new();

    for slot in 0..max_anchors {
        let idx = slot * last / (max_anchors - 1);
        let anchor = anchors[idx].clone();
        if seen.insert(anchor.clone()) {
            out.push(anchor);
        }
    }

    out
}

pub(crate) fn parse_file_range_target(target: &str) -> Result<(&str, u32, u32), CliError> {
    let (file, range) = target
        .rsplit_once(':')
        .ok_or_else(|| CliError::new("invalid_span", "expected <file>:<start>-<end>"))?;
    let (start_raw, end_raw) = range
        .split_once('-')
        .ok_or_else(|| CliError::new("invalid_span", "expected <file>:<start>-<end>"))?;

    let start: u32 = start_raw
        .parse()
        .map_err(|_| CliError::new("invalid_span", "start line must be an integer"))?;
    let end: u32 = end_raw
        .parse()
        .map_err(|_| CliError::new("invalid_span", "end line must be an integer"))?;
    if start == 0 || end == 0 || end < start {
        return Err(CliError::new(
            "invalid_span",
            "line range must be 1-based and end must be >= start",
        ));
    }

    Ok((file, start, end))
}

pub fn read_file_span_variants(path: &Path, start: u32, end: u32) -> Result<Vec<String>, CliError> {
    let content = fs::read_to_string(path).map_err(|err| CliError::io("read_span_error", err))?;
    let start_idx = start as usize - 1;
    let end_idx = end as usize - 1;
    let lines = content.lines().collect::<Vec<_>>();

    if end_idx >= lines.len() {
        return Err(CliError::new(
            "invalid_span",
            format!(
                "requested range {}-{} exceeds file length {}",
                start,
                end,
                lines.len()
            ),
        ));
    }

    let normalized = lines[start_idx..=end_idx].join("\n");
    let raw_lines = content.split_inclusive('\n').collect::<Vec<_>>();
    let raw = raw_lines
        .get(start_idx..=end_idx)
        .map(|slice| slice.concat());

    let mut variants = vec![normalized];
    if let Some(raw) = raw
        && variants.last().is_none_or(|existing| existing != &raw)
    {
        variants.push(raw);
    }

    Ok(variants)
}

pub fn compact_event(offset: u64, event: &Value) -> Value {
    let mut obj = Map::new();
    obj.insert("offset".to_string(), json!(offset));
    for key in [
        "t",
        "k",
        "role",
        "tool",
        "file",
        "range",
        "before_range",
        "after_range",
        "before_hash",
        "after_hash",
        "from_file",
        "from_range",
        "to_file",
        "to_range",
        "note",
        "exit",
    ] {
        if let Some(value) = event.get(key) {
            obj.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(obj)
}

pub fn edge_to_json(edge: &EdgeRow) -> Value {
    json!({
        "from_anchor": edge.from_anchor,
        "to_anchor": edge.to_anchor,
        "confidence": edge.confidence,
        "location_delta": location_delta_name(edge.location_delta),
        "cardinality": cardinality_name(edge.cardinality),
        "agent_link": edge.agent_link,
        "note": edge.note,
        "stored_class": stored_class_name(edge.stored_class),
    })
}

pub fn emit_query_result(_command: &str, payload: Value) -> Result<(), CliError> {
    print_json(&payload)
}

pub(crate) fn evidence_kind_name(kind: EvidenceKind) -> &'static str {
    match kind {
        EvidenceKind::Edit => "edit",
        EvidenceKind::Read => "read",
    }
}

pub(crate) fn stored_class_name(class: StoredEdgeClass) -> &'static str {
    match class {
        StoredEdgeClass::Lineage => "lineage",
        StoredEdgeClass::LocationOnly => "location_only",
    }
}

pub(crate) fn location_delta_name(delta: LocationDelta) -> &'static str {
    match delta {
        LocationDelta::Same => "same",
        LocationDelta::Adjacent => "adjacent",
        LocationDelta::Moved => "moved",
        LocationDelta::Absent => "absent",
    }
}

pub(crate) fn cardinality_name(cardinality: Cardinality) -> &'static str {
    match cardinality {
        Cardinality::OneToOne => "1:1",
        Cardinality::OneToMany => "1:N",
        Cardinality::ManyToOne => "N:1",
    }
}

pub(crate) fn pretty_tier_name(tier: PrettyConfidenceTier) -> &'static str {
    match tier {
        PrettyConfidenceTier::Edit => "edit",
        PrettyConfidenceTier::Move => "move",
        PrettyConfidenceTier::Related => "related",
        PrettyConfidenceTier::Hidden => "hidden",
        PrettyConfidenceTier::ForensicsOnly => "forensics_only",
    }
}
