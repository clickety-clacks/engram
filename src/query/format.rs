use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::{Map, Value, json};

use crate::anchor::fingerprint_token_hashes;
use crate::dispatch::message_turn_to_event_offset;
use crate::index::lineage::{
    Cardinality, EvidenceFragmentRef, EvidenceKind, LocationDelta, StoredEdgeClass,
};
use crate::index::{DispatchDirection, EdgeRow, ReaderMode, ReaderOpenError, SqliteIndex};
use crate::query::explain::{
    ExplainResult, ExplainTraversal, PrettyConfidenceTier, explain_across_indexes_by_anchor,
    pretty_tier,
};
use crate::query::file_identity::{FileIdentityRelation, FileIdentityResolver};
pub use crate::store::tapes::{TapeRow, parse_jsonl_rows, print_json};
use crate::store::tapes::{event_window, read_tape_content, resolve_tape_path, tape_id_from_path};
use crate::tape::grep::{
    grep_line_matches as decoded_grep_line_matches, scan_grep_reader, scan_parallel_in_order,
};
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
        indexes.push(open_query_index(&context.db_path, ReaderMode::Live)?);
    }
    for store in &context.additional_stores {
        if store.exists() {
            let mode = if context.frozen_stores.contains(store) {
                ReaderMode::Frozen
            } else {
                ReaderMode::Live
            };
            indexes.push(open_query_index(store, mode)?);
        }
    }
    if indexes.is_empty() {
        return Err(rusqlite::Error::InvalidPath(context.db_path.clone()).into());
    }
    Ok(indexes)
}

fn open_query_index(path: &Path, mode: ReaderMode) -> Result<SqliteIndex, CliError> {
    let mode_label = match mode {
        ReaderMode::Live => "live",
        ReaderMode::Frozen => "frozen",
    };
    let guidance = match mode {
        ReaderMode::Live => {
            "a managed live store must complete its owner's normal initialization or recovery before it is considered ready. This query remains read-only and does not initialize or repair the store. If it is expected to be ready, ask the store owner to check readiness and readability; declare only a stable captured copy in ~/.engram/topology.yml under frozen_stores"
        }
        ReaderMode::Frozen => {
            "verify the declared frozen copy exists, is readable, and has the expected schema"
        }
    };
    match SqliteIndex::open_reader_mode_detailed(&path_string(path), mode) {
        Ok(index) => Ok(index),
        Err(ReaderOpenError::SchemaVersion { expected, actual }) => Err(CliError::new(
            "reader_unavailable",
            format!(
                "store `{}` has schema version {actual}; this Engram reader requires version {expected}. The query remains read-only and does not migrate the store.",
                path.display()
            ),
        )),
        Err(ReaderOpenError::Sqlite(error)) => Err(CliError::new(
            "reader_unavailable",
            format!(
                "store `{}` could not be opened in {mode_label} read-only mode: {error}. Guidance: {guidance}.",
                path.display()
            ),
        )),
    }
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
        let (file, _) = target.rsplit_once(':').expect("span shape has colon");
        // Punctuation alone does not make literal source text a file range.
        // Validate the suffix only when its prefix names an actual file.
        if cwd.join(file).is_file() {
            let (_, start, end) = parse_file_range_target(target)?;
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
    let work = prepare_grep_scan(context, indexes)?;
    let result = run_grep_scan(work, pattern)?;
    Ok((result.raw_sessions, result.ranks))
}

#[derive(Debug)]
struct GrepScanTask {
    tape_id: String,
    path: PathBuf,
}

#[derive(Debug)]
pub struct GrepScanWork {
    tasks: Vec<GrepScanTask>,
}

#[derive(Debug)]
pub struct GrepScanOutput {
    pub raw_sessions: Vec<Value>,
    pub ranks: HashMap<String, GrepRank>,
    pub workers: usize,
    pub scanned_tapes: usize,
}

pub fn prepare_grep_scan(
    context: &RuntimeContext,
    indexes: &[SqliteIndex],
) -> Result<GrepScanWork, CliError> {
    let tape_ids = referenced_grep_tape_ids(indexes)?;
    prepare_grep_scan_with_tape_ids(context.tape_lookup_dirs.clone(), tape_ids)
}

pub fn referenced_grep_tape_ids(indexes: &[SqliteIndex]) -> Result<Vec<String>, CliError> {
    let mut tape_ids = HashSet::new();
    for index in indexes {
        tape_ids.extend(index.referenced_tape_ids()?);
    }
    let mut tape_ids = tape_ids.into_iter().collect::<Vec<_>>();
    tape_ids.sort();
    Ok(tape_ids)
}

pub fn prepare_grep_scan_with_tape_ids(
    tape_lookup_dirs: Vec<PathBuf>,
    tape_ids: Vec<String>,
) -> Result<GrepScanWork, CliError> {
    let mut tape_ids = tape_ids.into_iter().collect::<HashSet<_>>();
    let mut paths = HashMap::new();
    for dir in &tape_lookup_dirs {
        if !dir.exists() {
            continue;
        }
        let entries = fs::read_dir(dir).map_err(|err| CliError::io("read_dir_error", err))?;
        for entry in entries {
            let entry = entry.map_err(|err| CliError::io("read_dir_error", err))?;
            let path = entry.path();
            let Some(tape_id) = tape_id_from_path(&path) else {
                continue;
            };
            tape_ids.insert(tape_id.clone());
            if !paths.contains_key(&tape_id) && path.exists() {
                paths.insert(tape_id, path);
            }
        }
    }

    let mut tape_ids = tape_ids.into_iter().collect::<Vec<_>>();
    tape_ids.sort();
    let tasks = tape_ids
        .into_iter()
        .filter_map(|tape_id| {
            paths
                .get(&tape_id)
                .cloned()
                .map(|path| GrepScanTask { tape_id, path })
        })
        .collect();
    Ok(GrepScanWork { tasks })
}

pub fn run_grep_scan(work: GrepScanWork, pattern: &str) -> Result<GrepScanOutput, CliError> {
    let scanned_tapes = work.tasks.len();
    let mut raw_sessions = Vec::new();
    let mut ranks = HashMap::new();
    let workers = scan_parallel_in_order(
        &work.tasks,
        |task| {
            let file = File::open(&task.path).map_err(|error| CliError::io("read_error", error))?;
            scan_grep_reader(file, None, pattern)
                .map_err(|error| CliError::new(error.code, error.message))
        },
        |result| match result {
            Ok(summary) => summary.reorder_weight(),
            Err(error) => (
                std::mem::size_of::<CliError>().saturating_add(error.message.capacity()),
                0,
            ),
        },
        |task, result| -> Result<(), CliError> {
            let summary = result?;
            if summary.match_count == 0 {
                return Ok(());
            }
            let anchor_offset = summary.anchor_line.saturating_sub(1) as u64;
            raw_sessions.push(json!({
                "tape_id": task.tape_id,
                "tape_present_locally": true,
                "touch_count": summary.match_count,
                "latest_touch_timestamp": summary.timestamp,
                "touches": [],
                "windows": [{"touch_offset": anchor_offset}],
                "grep_scan_prepared": true,
                "grep_pattern": pattern,
                "grep_total_lines": summary.total_lines,
                "grep_files_touched": summary.files_touched,
                "grep_anchor_line": summary.anchor_line,
            }));
            ranks.insert(
                task.tape_id.clone(),
                GrepRank {
                    provenance_match_count: summary.provenance_match_count,
                    match_count: summary.match_count,
                    provenance_event_count: summary.provenance_event_count,
                },
            );
            Ok(())
        },
    )?;
    Ok(GrepScanOutput {
        raw_sessions,
        ranks,
        workers,
        scanned_tapes,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GrepRank {
    pub provenance_match_count: usize,
    pub match_count: usize,
    pub provenance_event_count: usize,
}

pub fn grep_line_matches(line: &str, pattern: &str) -> Result<bool, CliError> {
    decoded_grep_line_matches(line, pattern)
        .map_err(|error| CliError::new(error.code, error.message))
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

    b_score
        .total_cmp(&a_score)
        .then_with(|| b_touch_count.cmp(&a_touch_count))
        .then_with(|| b_ts.cmp(a_ts))
        .then_with(|| a_depth.cmp(&b_depth))
        .then_with(|| a_session_id.cmp(b_session_id))
}

pub fn compare_explain_sessions_with_span_priority(
    a: &Value,
    b: &Value,
    exact_edit_sessions: &HashSet<String>,
) -> std::cmp::Ordering {
    let a_session_id = a.get("session_id").and_then(Value::as_str).unwrap_or("");
    let b_session_id = b.get("session_id").and_then(Value::as_str).unwrap_or("");
    exact_edit_sessions
        .contains(b_session_id)
        .cmp(&exact_edit_sessions.contains(a_session_id))
        .then_with(|| compare_explain_sessions(a, b))
}

pub fn structured_edit_overlaps_span(event: &Value, file: &str, start: u32, end: u32) -> bool {
    if event.get("k").and_then(Value::as_str) != Some("code.edit")
        || event.get("file").and_then(Value::as_str) != Some(file)
    {
        return false;
    }

    ["before_range", "after_range"].into_iter().any(|key| {
        let Some(range) = event.get(key).and_then(Value::as_array) else {
            return false;
        };
        let (Some(edit_start), Some(edit_end)) = (
            range.first().and_then(Value::as_u64),
            range.get(1).and_then(Value::as_u64),
        ) else {
            return false;
        };
        edit_start > 0
            && edit_end >= edit_start
            && edit_start <= u64::from(end)
            && u64::from(start) <= edit_end
    })
}

pub fn exact_span_edit_sessions(
    raw_sessions: &[Value],
    file: &str,
    start: u32,
    end: u32,
    cwd: &Path,
) -> HashSet<String> {
    let mut identity_resolver = FileIdentityResolver::default();
    let target_identity = identity_resolver.identity_for_query_path(cwd, file);
    raw_sessions
        .iter()
        .filter_map(|session| {
            let tape_id = session.get("tape_id")?.as_str()?;
            let source_repo_head = session.get("repo_head").and_then(Value::as_str);
            let edit_touches = session
                .get("touches")?
                .as_array()?
                .iter()
                .filter(|touch| touch.get("kind").and_then(Value::as_str) == Some("edit"))
                .filter_map(|touch| {
                    let evidence_file = touch.get("file_path")?.as_str()?;
                    let event_offset = touch.get("event_offset")?.as_u64()?;
                    let relation = identity_resolver.relation(cwd, file, evidence_file);
                    let same_path = match relation {
                        FileIdentityRelation::SamePhysicalPath => match (
                            source_repo_head,
                            target_identity
                                .as_ref()
                                .and_then(|identity| identity.head()),
                        ) {
                            (Some(source), Some(target)) => source == target,
                            _ => true,
                        },
                        FileIdentityRelation::SameRepositoryFile => matches!(
                            (
                                source_repo_head,
                                target_identity.as_ref().and_then(|identity| identity.head()),
                            ),
                            (Some(source), Some(target)) if source == target
                        ),
                        _ => false,
                    };
                    same_path.then(|| (event_offset, evidence_file.to_owned()))
                })
                .collect::<Vec<_>>();
            if edit_touches.is_empty() {
                return None;
            }

            let matches = session
                .get("windows")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|window| window.get("events").and_then(Value::as_array))
                .flatten()
                .any(|entry| {
                    let Some(offset) = entry.get("offset").and_then(Value::as_u64) else {
                        return false;
                    };
                    let Some((_, evidence_file)) = edit_touches
                        .iter()
                        .find(|(touch_offset, _)| *touch_offset == offset)
                    else {
                        return false;
                    };
                    let Some(event) = entry.get("event") else {
                        return false;
                    };
                    structured_edit_overlaps_span(event, evidence_file, start, end)
                });
            matches.then(|| tape_id.to_string())
        })
        .collect()
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

        let grep_prepared = raw
            .get("grep_scan_prepared")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let tape_path = if grep_prepared {
            None
        } else {
            resolve_tape_path(context, session_id)
        };
        let (rows, raw_text, total_lines) = if grep_prepared {
            let total_lines = raw
                .get("grep_total_lines")
                .and_then(Value::as_u64)
                .and_then(|lines| usize::try_from(lines).ok())
                .unwrap_or_default();
            (Vec::new(), String::new(), total_lines)
        } else if let Some(path) = tape_path.as_ref() {
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

        if let Some(pattern) = grep {
            let matched = if grep_prepared {
                raw.get("grep_pattern").and_then(Value::as_str) == Some(pattern)
            } else {
                let mut matched = false;
                for text in &window_texts {
                    if decoded_grep_line_matches(text, pattern)
                        .map_err(|error| CliError::new(error.code, error.message))?
                    {
                        matched = true;
                        break;
                    }
                }
                matched
            };
            if !matched {
                continue;
            }
        }

        let mut files_touched = if grep_prepared {
            raw.get("grep_files_touched")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<HashSet<_>>()
        } else {
            raw.get("touches")
                .and_then(Value::as_array)
                .map(|touches| {
                    touches
                        .iter()
                        .filter_map(|touch| touch.get("file_path").and_then(Value::as_str))
                        .filter(|file| !file.is_empty())
                        .map(ToOwned::to_owned)
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default()
        };
        if files_touched.is_empty() && !grep_prepared {
            for file in collect_files_touched_from_rows(&rows) {
                files_touched.insert(file);
            }
        }
        let mut files_touched = files_touched.into_iter().collect::<Vec<_>>();
        files_touched.sort();

        let (refs_up, refs_down) = if grep_prepared {
            (0, 0)
        } else {
            dispatch_ref_counts(indexes, session_id)?
        };
        let timestamp = raw
            .get("latest_touch_timestamp")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                if grep_prepared {
                    String::new()
                } else {
                    extract_latest_timestamp_from_rows(&rows)
                }
            });
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
            "repo_head": raw.get("repo_head").cloned().unwrap_or(Value::Null),
            "files_touched": files_touched,
            "touches": touches,
        }));
    }

    Ok(out)
}

pub fn dispatch_ref_counts(
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
        .collect::<BTreeSet<_>>();
    let mut edges = Vec::new();
    for link in dispatch_lineage {
        // Recovery keeps context provenance in `session`; the physical edit
        // remains the child displayed in the ordinary sessions/chain model.
        let Some(child) = link
            .get("edit_session")
            .or_else(|| link.get("session"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(parent) = link.get("parent_session").and_then(Value::as_str) else {
            continue;
        };
        edges.push((child.to_string(), parent.to_string()));
    }
    let graph = session_chain_graph(ids, edges);

    for session in sessions {
        let Some(id) = session
            .get("session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        let parents = graph.parents.get(&id).cloned().unwrap_or_default();
        let children = graph.children.get(&id).cloned().unwrap_or_default();
        let component = graph
            .node_component
            .get(&id)
            .map(|index| &graph.components[*index]);
        let depth = graph.depths.get(&id).copied().unwrap_or(0);
        let length = component.map_or(1, |component| component.ids.len());

        if let Some(obj) = session.as_object_mut() {
            obj.insert("depth".to_string(), json!(depth));
            obj.insert(
                "parent".to_string(),
                if parents.len() == 1 {
                    json!(parents.first().expect("one parent"))
                } else {
                    Value::Null
                },
            );
            if parents.len() > 1 {
                obj.insert("parents".to_string(), json!(parents));
            } else {
                obj.remove("parents");
            }
            obj.insert("children".to_string(), json!(children));
            obj.insert("chain_length".to_string(), json!(length));
        }
    }
}

pub fn build_chain_metadata(sessions: &[Value]) -> Vec<Value> {
    let ids = sessions
        .iter()
        .filter_map(|session| session.get("session_id").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    let mut edges = Vec::new();
    for session in sessions {
        let Some(id) = session.get("session_id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(parents) = session.get("parents").and_then(Value::as_array) {
            edges.extend(
                parents
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|parent| (id.to_string(), parent.to_string())),
            );
        } else if let Some(parent) = session.get("parent").and_then(Value::as_str) {
            edges.push((id.to_string(), parent.to_string()));
        }
    }
    let graph = session_chain_graph(ids, edges);
    if graph.components.iter().all(|component| !component.cycle)
        && graph.parents.values().all(|parents| parents.len() <= 1)
    {
        return build_single_parent_chain_metadata(sessions);
    }

    let mut out = Vec::new();
    for component in &graph.components {
        let mut descendants = component
            .ids
            .iter()
            .filter_map(|id| {
                let session = sessions.iter().find(|session| {
                    session.get("session_id").and_then(Value::as_str) == Some(id.as_str())
                })?;
                let parents = graph.parents.get(id).cloned().unwrap_or_default();
                let children = graph.children.get(id).cloned().unwrap_or_default();
                let mut row = json!({
                    "session_id": id,
                    "depth": session.get("depth").cloned().unwrap_or_else(|| {
                        json!(graph.depths.get(id).copied().unwrap_or(0))
                    }),
                    "parent": if parents.len() == 1 {
                        json!(parents.first().expect("one parent"))
                    } else {
                        Value::Null
                    },
                    "children": children,
                });
                if parents.len() > 1 {
                    row["parents"] = json!(parents);
                }
                // Retain any display-specific chain annotations already
                // computed for this session without trusting them for graph
                // construction.
                if session.get("cycle").and_then(Value::as_bool) == Some(true) {
                    row["cycle"] = json!(true);
                }
                Some(row)
            })
            .collect::<Vec<_>>();
        descendants.sort_by(|a, b| {
            let ad = a.get("depth").and_then(Value::as_u64).unwrap_or(0);
            let bd = b.get("depth").and_then(Value::as_u64).unwrap_or(0);
            ad.cmp(&bd).then_with(|| {
                a.get("session_id")
                    .and_then(Value::as_str)
                    .cmp(&b.get("session_id").and_then(Value::as_str))
            })
        });
        let mut chain = json!({"descendants": descendants});
        if component.roots.len() > 1 {
            chain["root_session_ids"] = json!(component.roots);
        } else {
            chain["root_session_id"] = json!(component.display_root);
        }
        if component.cycle {
            chain["cycle"] = json!(true);
        }
        out.push(chain);
    }
    out
}

fn build_single_parent_chain_metadata(sessions: &[Value]) -> Vec<Value> {
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
    root_order
        .into_iter()
        .map(|root| {
            let mut descendants = by_root.remove(&root).unwrap_or_default();
            descendants.sort_by(|a, b| {
                let ad = a.get("depth").and_then(Value::as_u64).unwrap_or(0);
                let bd = b.get("depth").and_then(Value::as_u64).unwrap_or(0);
                ad.cmp(&bd)
            });
            json!({
                "root_session_id": root,
                "descendants": descendants,
            })
        })
        .collect()
}

struct ChainComponent {
    ids: BTreeSet<String>,
    roots: Vec<String>,
    display_root: String,
    cycle: bool,
}

struct SessionChainGraph {
    parents: BTreeMap<String, BTreeSet<String>>,
    children: BTreeMap<String, BTreeSet<String>>,
    components: Vec<ChainComponent>,
    node_component: BTreeMap<String, usize>,
    depths: BTreeMap<String, usize>,
}

fn session_chain_graph(
    ids: BTreeSet<String>,
    edges: impl IntoIterator<Item = (String, String)>,
) -> SessionChainGraph {
    let mut parents = ids
        .iter()
        .cloned()
        .map(|id| (id, BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    let mut children = parents.clone();
    for (child, parent) in edges {
        if parents.contains_key(&child) && parents.contains_key(&parent) {
            parents
                .entry(child.clone())
                .or_default()
                .insert(parent.clone());
            children.entry(parent).or_default().insert(child);
        }
    }

    let mut unseen = ids;
    let mut components = Vec::new();
    let mut node_component = BTreeMap::new();
    let mut depths = BTreeMap::<String, usize>::new();
    while let Some(start) = unseen.iter().next().cloned() {
        unseen.remove(&start);
        let mut component_ids = BTreeSet::from([start.clone()]);
        let mut queue = VecDeque::from([start]);
        while let Some(id) = queue.pop_front() {
            let neighbors = parents
                .get(&id)
                .into_iter()
                .flatten()
                .chain(children.get(&id).into_iter().flatten());
            for neighbor in neighbors {
                if unseen.remove(neighbor) {
                    component_ids.insert(neighbor.clone());
                    queue.push_back(neighbor.clone());
                }
            }
        }

        let roots = component_ids
            .iter()
            .filter(|id| parents.get(*id).is_none_or(BTreeSet::is_empty))
            .cloned()
            .collect::<Vec<_>>();
        let cycle = roots.is_empty();
        let display_root = roots
            .first()
            .cloned()
            .or_else(|| component_ids.first().cloned())
            .expect("component is non-empty");
        let starts = if cycle {
            vec![display_root.clone()]
        } else {
            roots.clone()
        };
        let mut breadth = VecDeque::new();
        for root in starts {
            if depths.insert(root.clone(), 0).is_none() {
                breadth.push_back(root);
            }
        }
        while let Some(id) = breadth.pop_front() {
            let depth = depths[&id];
            for neighbor in parents
                .get(&id)
                .into_iter()
                .flatten()
                .chain(children.get(&id).into_iter().flatten())
            {
                if component_ids.contains(neighbor) && !depths.contains_key(neighbor) {
                    depths.insert(neighbor.clone(), depth.saturating_add(1));
                    breadth.push_back(neighbor.clone());
                }
            }
        }
        let index = components.len();
        for id in &component_ids {
            node_component.insert(id.clone(), index);
        }
        components.push(ChainComponent {
            ids: component_ids,
            roots,
            display_root,
            cycle,
        });
    }

    SessionChainGraph {
        parents,
        children,
        components,
        node_component,
        depths,
    }
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
        let (windows, repo_head) = if let Some(tape_path) = tape_path.as_ref() {
            let content = read_tape_content(&tape_path)?;
            let rows = parse_jsonl_rows(&content)?;
            let windows = tape_touches
                .iter()
                .filter_map(|touch| {
                    event_window(&rows, touch.event_offset, TRANSCRIPT_WINDOW_RADIUS)
                })
                .collect::<Vec<_>>();
            (windows, unique_tape_repo_head(&rows))
        } else {
            (Vec::new(), None)
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
            "repo_head": repo_head,
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

fn unique_tape_repo_head(rows: &[TapeRow]) -> Option<String> {
    let mut meta_rows = rows
        .iter()
        .filter(|row| row.value.get("k").and_then(Value::as_str) == Some("meta"));
    let first_meta = meta_rows.next()?;
    let repo_head = first_meta.value.get("repo_head").and_then(Value::as_str)?;
    if repo_head.len() > 128 {
        return None;
    }
    meta_rows
        .all(|row| {
            row.value
                .get("repo_head")
                .and_then(Value::as_str)
                .is_some_and(|other| other.len() <= 128 && other == repo_head)
        })
        .then(|| repo_head.to_owned())
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

#[cfg(test)]
mod chain_graph_tests {
    use super::*;

    #[test]
    fn query_schema_mismatch_is_not_reported_as_a_readonly_failure() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA user_version = 3").unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();

        let error = match open_query_index(&path, ReaderMode::Live) {
            Err(error) => error,
            Ok(_) => panic!("v3 index must not open as a v4 reader"),
        };

        assert_eq!(error.code, "reader_unavailable");
        assert!(error.message.contains("schema version 3"));
        assert!(error.message.contains("requires version 4"));
        assert!(error.message.contains("does not migrate"));
        assert!(!error.message.contains("Query is not read-only"));
        assert!(!error.message.contains("grant SQLite write access"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn chain_graph_retains_multiple_parents_and_reports_multiple_roots() {
        let mut sessions = vec![
            json!({"session_id": "a"}),
            json!({"session_id": "b"}),
            json!({"session_id": "c"}),
        ];
        let hops = vec![
            json!({"session": "c", "parent_session": "a"}),
            json!({"session": "c", "parent_session": "b"}),
        ];

        annotate_chain_fields(&mut sessions, &hops);

        let child = sessions
            .iter()
            .find(|session| session["session_id"] == "c")
            .expect("child session");
        assert_eq!(child["parent"], Value::Null);
        assert_eq!(child["parents"], json!(["a", "b"]));
        assert_eq!(child["depth"], 1);
        assert_eq!(child["chain_length"], 3);

        let chains = build_chain_metadata(&sessions);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0]["root_session_ids"], json!(["a", "b"]));
        assert!(chains[0].get("root_session_id").is_none());
        assert_eq!(
            chains[0]["descendants"]
                .as_array()
                .expect("component sessions")
                .iter()
                .map(|session| (
                    session["session_id"].as_str().unwrap(),
                    session["depth"].as_u64().unwrap(),
                ))
                .collect::<Vec<_>>(),
            vec![("a", 0), ("b", 0), ("c", 1)]
        );
    }

    #[test]
    fn single_parent_chains_keep_ranked_component_and_sibling_order() {
        let mut sessions = vec![
            json!({"session_id": "z-root"}),
            json!({"session_id": "z-child-ranked-first"}),
            json!({"session_id": "a-root"}),
            json!({"session_id": "z-child-ranked-second"}),
            json!({"session_id": "a-child"}),
        ];
        let hops = vec![
            json!({"session": "z-child-ranked-first", "parent_session": "z-root"}),
            json!({"session": "z-child-ranked-second", "parent_session": "z-root"}),
            json!({"session": "a-child", "parent_session": "a-root"}),
        ];
        annotate_chain_fields(&mut sessions, &hops);

        let chains = build_chain_metadata(&sessions);
        assert_eq!(
            chains
                .iter()
                .map(|chain| chain["root_session_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["z-root", "a-root"]
        );
        assert_eq!(
            chains[0]["descendants"]
                .as_array()
                .unwrap()
                .iter()
                .map(|session| session["session_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["z-root", "z-child-ranked-first", "z-child-ranked-second"]
        );
    }

    #[test]
    fn paged_chain_metadata_keeps_the_annotated_full_graph_depth() {
        let mut sessions = vec![
            json!({"session_id": "root"}),
            json!({"session_id": "child"}),
            json!({"session_id": "grandchild"}),
        ];
        let hops = vec![
            json!({"session": "child", "parent_session": "root"}),
            json!({"session": "grandchild", "parent_session": "child"}),
        ];
        annotate_chain_fields(&mut sessions, &hops);

        let control_chains = build_chain_metadata(&sessions);
        let control_grandchild = control_chains
            .iter()
            .flat_map(|chain| chain["descendants"].as_array().expect("descendants"))
            .find(|session| session["session_id"] == "grandchild")
            .expect("unpaginated grandchild");
        assert_eq!(control_grandchild["depth"], 2);
        assert_eq!(control_grandchild["parent"], "child");

        let (page, returned, total, _, truncated) =
            apply_session_truncation(sessions.clone(), Some(1), 2, 10);
        assert_eq!(returned, 1);
        assert_eq!(total, 3);
        assert!(truncated);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0]["session_id"], "grandchild");

        let chains = build_chain_metadata(&page);
        let page_grandchild = &chains[0]["descendants"][0];
        assert_eq!(page[0]["depth"], 2);
        assert_eq!(page[0]["depth"], control_grandchild["depth"]);
        assert_eq!(page[0]["parent"], "child");
        assert_eq!(page_grandchild["session_id"], "grandchild");
        assert_eq!(page_grandchild["depth"], page[0]["depth"]);
        assert_eq!(page_grandchild["depth"], control_grandchild["depth"]);
        assert_eq!(page_grandchild["parent"], "child");
        assert_eq!(page_grandchild["parent"], control_grandchild["parent"]);
    }

    #[test]
    fn chain_graph_formats_a_pure_cycle_with_a_stable_display_root() {
        let mut sessions = vec![json!({"session_id": "b"}), json!({"session_id": "a"})];
        let hops = vec![
            json!({"session": "a", "parent_session": "b"}),
            json!({"session": "b", "parent_session": "a"}),
        ];

        annotate_chain_fields(&mut sessions, &hops);
        let chains = build_chain_metadata(&sessions);

        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0]["root_session_id"], "a");
        assert_eq!(chains[0]["cycle"], true);
        assert_eq!(chains[0]["descendants"][0]["session_id"], "a");
        assert_eq!(chains[0]["descendants"][0]["depth"], 0);
        assert_eq!(chains[0]["descendants"][1]["session_id"], "b");
        assert_eq!(chains[0]["descendants"][1]["depth"], 1);
    }
}

#[cfg(test)]
mod file_identity_span_tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use serde_json::{Value, json};

    use super::exact_span_edit_sessions;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("git is available for worktree span tests");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git stdout is UTF-8")
            .trim()
            .to_string()
    }

    fn init_repo(root: &Path) -> String {
        fs::create_dir_all(root.join("scripts")).expect("scripts");
        fs::write(
            root.join("scripts/verify_mix.sh"),
            "line one\nline two\nline three\nline four\n",
        )
        .expect("verify_mix fixture");
        let output = Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(root)
            .output()
            .expect("git is available for worktree span tests");
        assert!(output.status.success(), "git init failed");
        let _ = git(root, &["config", "user.name", "Span Test"]);
        let _ = git(root, &["config", "user.email", "span-test@example.invalid"]);
        let _ = git(root, &["add", "scripts/verify_mix.sh"]);
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "commit.gpgsign=false", "commit", "-m", "initial"])
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()
            .expect("git is available for worktree span tests");
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        git(root, &["rev-parse", "HEAD"])
    }

    fn add_worktree(root: &Path, worktree: &Path) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["worktree", "add", "--quiet", "-b", "span-source"])
            .arg(worktree)
            .arg("HEAD")
            .output()
            .expect("git is available for worktree span tests");
        assert!(
            output.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn edit_session(tape_id: &str, repo_head: &str, file: &Path, start: u64, end: u64) -> Value {
        let file = file.to_string_lossy().to_string();
        json!({
            "tape_id": tape_id,
            "repo_head": repo_head,
            "touches": [{
                "kind": "edit",
                "file_path": file,
                "event_offset": 7,
            }],
            "windows": [{
                "events": [{
                    "offset": 7,
                    "event": {
                        "k": "code.edit",
                        "file": file,
                        "before_range": [start, end],
                        "after_range": [start, end],
                    }
                }]
            }]
        })
    }

    #[test]
    fn exact_span_priority_requires_same_worktree_revision_and_overlapping_range() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("repository");
        let worktree = temp.path().join("second-worktree");
        let target_head = init_repo(&root);
        add_worktree(&root, &worktree);

        let target = root.join("scripts/verify_mix.sh");
        let source = worktree.join("scripts/verify_mix.sh");
        let overlapping = edit_session("same-revision-overlap", &target_head, &source, 1, 1);
        assert_eq!(
            exact_span_edit_sessions(
                std::slice::from_ref(&overlapping),
                &target.to_string_lossy(),
                1,
                1,
                &root,
            ),
            ["same-revision-overlap".to_string()].into()
        );

        let non_overlapping =
            edit_session("same-revision-other-range", &target_head, &source, 3, 3);
        assert!(
            exact_span_edit_sessions(
                std::slice::from_ref(&non_overlapping),
                &target.to_string_lossy(),
                1,
                1,
                &root,
            )
            .is_empty()
        );

        fs::write(
            &source,
            "line one\nline two\nline three changed\nline four\n",
        )
        .expect("update second worktree");
        let _ = git(&worktree, &["add", "scripts/verify_mix.sh"]);
        let output = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["-c", "commit.gpgsign=false", "commit", "-m", "new revision"])
            .env("GIT_AUTHOR_DATE", "2026-01-02T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-02T00:00:00Z")
            .output()
            .expect("git is available for worktree span tests");
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let source_head = git(&worktree, &["rev-parse", "HEAD"]);
        assert_ne!(target_head, source_head);
        let different_revision = edit_session("different-revision", &source_head, &source, 1, 1);
        assert!(
            exact_span_edit_sessions(
                std::slice::from_ref(&different_revision),
                &target.to_string_lossy(),
                1,
                1,
                &root,
            )
            .is_empty()
        );
    }
}
