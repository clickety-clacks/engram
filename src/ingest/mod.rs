use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::dispatch::extract_dispatch_links_from_transcript;
use crate::index::SqliteIndex;
use crate::index::lineage::LINK_THRESHOLD_DEFAULT;
use crate::store::atomic::atomic_write;
use crate::store::tapes::{print_json, tape_path_for_id, tape_path_for_tapes_dir};
use crate::tape::adapter::{
    AdapterId, adapter_claims_input, adapter_registry, convert_with_adapter,
    discover_sessions_with_adapter,
};
use crate::tape::compress::compress_jsonl;
use crate::tape::event::{TapeEventAt, TapeEventData, parse_jsonl_events};
use crate::{CliError, RepoPaths, RuntimeContext, ensure_db_parent, home_dir, path_string};

const CURSOR_GUARD_WINDOW: usize = 512;

pub fn run_ingest(
    cwd: &Path,
    paths: &RepoPaths,
    context: &RuntimeContext,
    raw_paths: &[PathBuf],
) -> Result<(), CliError> {
    fs::create_dir_all(&context.tapes_dir).map_err(|err| CliError::io("mkdir_error", err))?;
    let (mut candidates, mut failures) = discover_ingest_candidates(cwd, raw_paths)?;
    let home = home_dir()?;
    if raw_paths.is_empty() {
        for descriptor in adapter_registry() {
            // TODO: Merge/replace cwd scanning with adapter-driven session discovery
            // once harness adapters implement discover_sessions_for_repo.
            let discovered = discover_sessions_with_adapter(descriptor.id, cwd, &home);
            candidates.extend(discovered);
        }
    }
    candidates.sort();
    candidates.dedup();
    ensure_db_parent(&context.db_path)?;
    let index = SqliteIndex::open_writer(&path_string(&context.db_path))?;

    let mut scanned = 0usize;
    let mut imported = 0usize;
    let mut skipped_unchanged = 0usize;
    let mut skipped_existing_tape = 0usize;
    let mut skipped_non_transcript = 0usize;

    for path in candidates {
        scanned += 1;
        let abs_path = match fs::canonicalize(&path) {
            Ok(value) => value,
            Err(err) => {
                failures.push(json!({
                    "path": path_string(&path),
                    "error": err.to_string(),
                }));
                continue;
            }
        };
        let metadata = match fs::metadata(&abs_path) {
            Ok(value) => value,
            Err(err) => {
                failures.push(json!({
                    "path": path_string(&abs_path),
                    "error": err.to_string(),
                }));
                continue;
            }
        };

        let prior_state = match load_ingest_state_for_path(paths, &abs_path) {
            Ok(value) => value,
            Err(err) => {
                failures.push(json!({
                    "path": path_string(&abs_path),
                    "error": err.message,
                }));
                continue;
            }
        };

        let mut should_run_full = prior_state.is_none();
        let mut full_reason = None::<&str>;
        if let Some(prev) = prior_state.as_ref() {
            let prior_tape_path = tape_path_for_tapes_dir(&context.tapes_dir, &prev.tape_id);
            let prior_tape_missing = !prior_tape_path.exists();
            let prior_tape_unindexed = !index.has_tape(&prev.tape_id)?;
            if prior_tape_missing || prior_tape_unindexed {
                should_run_full = true;
                full_reason = Some(if prior_tape_missing {
                    "cursor_tape_missing"
                } else {
                    "cursor_tape_unindexed"
                });
            } else if metadata.len() < prev.byte_cursor {
                should_run_full = true;
                full_reason = Some("cursor_past_eof");
            } else {
                match ingest_cursor_guard_matches(&abs_path, &prev.cursor_guard, metadata.len()) {
                    Ok(false) => {
                        should_run_full = true;
                        full_reason = Some("guard_mismatch");
                    }
                    Ok(true) => {
                        if metadata.len() == prev.byte_cursor {
                            skipped_unchanged += 1;
                            continue;
                        }
                    }
                    Err(err) => {
                        failures.push(json!({
                            "path": path_string(&abs_path),
                            "error": err.message,
                        }));
                        continue;
                    }
                }
            }
        }

        let mut ingest_bytes = Vec::new();
        let mut adapter_hint = None;
        let mut next_cursor = 0u64;

        if !should_run_full {
            let prev = prior_state.as_ref().expect("known state");
            adapter_hint = adapter_id_from_name(&prev.adapter);
            let mut file = match File::open(&abs_path) {
                Ok(value) => value,
                Err(err) => {
                    failures.push(json!({
                        "path": path_string(&abs_path),
                        "error": err.to_string(),
                    }));
                    continue;
                }
            };
            if let Err(err) = file.seek(SeekFrom::Start(prev.byte_cursor)) {
                failures.push(json!({
                    "path": path_string(&abs_path),
                    "error": err.to_string(),
                }));
                continue;
            }
            if let Err(err) = file.read_to_end(&mut ingest_bytes) {
                failures.push(json!({
                    "path": path_string(&abs_path),
                    "error": err.to_string(),
                }));
                continue;
            }
            let complete = complete_ingest_prefix_len(&abs_path, &ingest_bytes);
            if complete == 0 {
                skipped_unchanged += 1;
                continue;
            }
            ingest_bytes.truncate(complete);
            next_cursor = prev.byte_cursor + complete as u64;
        }

        if should_run_full {
            let all_bytes = match fs::read(&abs_path) {
                Ok(value) => value,
                Err(err) => {
                    failures.push(json!({
                        "path": path_string(&abs_path),
                        "error": err.to_string(),
                    }));
                    continue;
                }
            };
            let complete = complete_ingest_prefix_len(&abs_path, &all_bytes);
            if complete == 0 {
                skipped_unchanged += 1;
                continue;
            }
            ingest_bytes = all_bytes[..complete].to_vec();
            next_cursor = complete as u64;
            if let Some(prev) = prior_state.as_ref() {
                adapter_hint = adapter_id_from_name(&prev.adapter);
            }
        }

        let ingest_input = match std::str::from_utf8(&ingest_bytes) {
            Ok(value) => value,
            Err(err) => {
                failures.push(json!({
                    "path": path_string(&abs_path),
                    "error": err.to_string(),
                }));
                continue;
            }
        };

        let adapter = adapter_hint.filter(|adapter| adapter_claims_input(*adapter, ingest_input));
        let adapter = if let Some(value) = adapter {
            value
        } else if let Some(value) = detect_adapter_for_input(&abs_path, ingest_input) {
            value
        } else if should_run_full {
            skipped_non_transcript += 1;
            continue;
        } else {
            should_run_full = true;
            full_reason = Some("adapter_parse_mismatch");
            let all_bytes = match fs::read(&abs_path) {
                Ok(value) => value,
                Err(err) => {
                    failures.push(json!({
                        "path": path_string(&abs_path),
                        "error": err.to_string(),
                    }));
                    continue;
                }
            };
            let complete = complete_ingest_prefix_len(&abs_path, &all_bytes);
            if complete == 0 {
                skipped_unchanged += 1;
                continue;
            }
            ingest_bytes = all_bytes[..complete].to_vec();
            next_cursor = complete as u64;
            let input = match std::str::from_utf8(&ingest_bytes) {
                Ok(value) => value,
                Err(err) => {
                    failures.push(json!({
                        "path": path_string(&abs_path),
                        "error": err.to_string(),
                    }));
                    continue;
                }
            };
            let Some(value) = detect_adapter_for_input(&abs_path, input) else {
                skipped_non_transcript += 1;
                continue;
            };
            value
        };

        let ingest_input = match std::str::from_utf8(&ingest_bytes) {
            Ok(value) => value,
            Err(err) => {
                failures.push(json!({
                    "path": path_string(&abs_path),
                    "error": err.to_string(),
                }));
                continue;
            }
        };

        let normalized = match convert_with_adapter(adapter, ingest_input) {
            Ok(output) => output,
            Err(err) => {
                failures.push(json!({
                    "path": path_string(&abs_path),
                    "adapter": adapter.as_str(),
                    "reason": full_reason,
                    "error": err.to_string(),
                }));
                continue;
            }
        };
        let events = match parse_jsonl_events(&normalized) {
            Ok(events) => events,
            Err(err) => {
                failures.push(json!({
                    "path": path_string(&abs_path),
                    "adapter": adapter.as_str(),
                    "reason": full_reason,
                    "error": err.to_string(),
                }));
                continue;
            }
        };
        if !events
            .iter()
            .any(|event| !matches!(event.event.data, TapeEventData::Meta(_)))
        {
            skipped_non_transcript += 1;
            continue;
        }
        let dispatch_links = extract_dispatch_links_from_transcript(ingest_input);

        let tape_id = tape_id_for_contents(&normalized);
        let tape_path = tape_path_for_tapes_dir(&context.tapes_dir, &tape_id);
        let tape_file_exists = tape_path.exists();
        if !tape_file_exists {
            let compressed =
                compress_jsonl(&normalized).map_err(|err| CliError::io("compress_error", err))?;
            atomic_write(&tape_path, &compressed)
                .map_err(|err| CliError::io("write_error", err))?;
        }

        let already_indexed = index.has_tape(&tape_id)?;
        if !already_indexed {
            index.ingest_tape_events_with_dispatch(
                &tape_id,
                &events,
                &dispatch_links,
                LINK_THRESHOLD_DEFAULT,
            )?;
            imported += 1;
        } else {
            skipped_existing_tape += 1;
        }

        let cursor_guard = match build_cursor_guard(&abs_path, next_cursor) {
            Ok(value) => value,
            Err(err) => {
                failures.push(json!({
                    "path": path_string(&abs_path),
                    "error": err.message,
                }));
                continue;
            }
        };
        let state = IngestFileState {
            byte_cursor: next_cursor,
            cursor_guard,
            adapter: adapter.as_str().to_string(),
            tape_id,
        };
        if let Err(err) = save_ingest_state_for_path(paths, &abs_path, &state) {
            failures.push(json!({
                "path": path_string(&abs_path),
                "error": err.message,
            }));
            continue;
        }
    }

    print_json(&json!({
        "status": if failures.is_empty() { "ok" } else { "partial" },
        "scanned_inputs": scanned,
        "imported_tapes": imported,
        "skipped_unchanged": skipped_unchanged,
        "skipped_existing_tape": skipped_existing_tape,
        "skipped_non_transcript": skipped_non_transcript,
        "failure_count": failures.len(),
        "failures": failures,
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IngestCursorGuard {
    pub offset: u64,
    pub len: u32,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IngestFileState {
    pub byte_cursor: u64,
    pub cursor_guard: IngestCursorGuard,
    pub adapter: String,
    pub tape_id: String,
}

pub(crate) fn discover_local_transcript_candidates(cwd: &Path) -> Result<Vec<PathBuf>, CliError> {
    let mut out = Vec::new();
    for entry in WalkDir::new(cwd).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if entry.file_type().is_dir() {
            continue;
        }
        if path.starts_with(cwd.join(".engram")) {
            continue;
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase());
        if matches!(extension.as_deref(), Some("json") | Some("jsonl")) {
            out.push(path.to_path_buf());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

pub(crate) fn detect_adapter_for_input(path: &Path, input: &str) -> Option<AdapterId> {
    let lower_path = path.to_string_lossy().to_ascii_lowercase();
    let preferred =
        if lower_path.contains(".codex/sessions") || lower_path.ends_with("history.jsonl") {
            Some(AdapterId::CodexCli)
        } else if lower_path.contains(".claude/projects") {
            Some(AdapterId::ClaudeCode)
        } else if lower_path.contains("opencode") {
            Some(AdapterId::OpenCode)
        } else if lower_path.contains("cursor") {
            Some(AdapterId::Cursor)
        } else if lower_path.contains("gemini") {
            Some(AdapterId::GeminiCli)
        } else if lower_path.contains(".openclaw") || lower_path.contains("openclaw") {
            Some(AdapterId::OpenClaw)
        } else {
            None
        };

    let mut candidates = Vec::new();
    if let Some(adapter) = preferred {
        candidates.push(adapter);
    }
    for adapter in [
        AdapterId::CodexCli,
        AdapterId::ClaudeCode,
        AdapterId::OpenCode,
        AdapterId::Cursor,
        AdapterId::GeminiCli,
        AdapterId::OpenClaw,
    ] {
        if !candidates.contains(&adapter) {
            candidates.push(adapter);
        }
    }

    for adapter in candidates {
        if adapter_claims_input(adapter, input) {
            return Some(adapter);
        }
    }
    None
}

pub(crate) fn discover_ingest_candidates(
    cwd: &Path,
    raw_paths: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<Value>), CliError> {
    if raw_paths.is_empty() {
        return Ok((discover_local_transcript_candidates(cwd)?, Vec::new()));
    }

    let scope_root = fs::canonicalize(cwd).map_err(|err| CliError::io("read_error", err))?;
    let mut failures = Vec::new();
    let mut candidates = Vec::new();
    for raw_path in raw_paths {
        let resolved = if raw_path.is_absolute() {
            raw_path.clone()
        } else {
            cwd.join(raw_path)
        };
        let canonical = match fs::canonicalize(&resolved) {
            Ok(value) => value,
            Err(err) => {
                failures.push(json!({
                    "path": path_string(&resolved),
                    "error": err.to_string(),
                }));
                continue;
            }
        };
        if !canonical.starts_with(&scope_root) {
            failures.push(json!({
                "path": path_string(&canonical),
                "error": "path is outside current working directory scope (run `engram ingest` from a parent directory, e.g. $HOME)",
            }));
            continue;
        }

        let metadata = match fs::metadata(&canonical) {
            Ok(value) => value,
            Err(err) => {
                failures.push(json!({
                    "path": path_string(&canonical),
                    "error": err.to_string(),
                }));
                continue;
            }
        };
        if metadata.is_dir() {
            for entry in WalkDir::new(&canonical).into_iter().filter_map(Result::ok) {
                if entry.file_type().is_dir() {
                    continue;
                }
                let entry_path = entry.path();
                if entry_path.starts_with(scope_root.join(".engram")) {
                    continue;
                }
                let extension = entry_path
                    .extension()
                    .and_then(|value| value.to_str())
                    .map(|value| value.to_ascii_lowercase());
                if matches!(extension.as_deref(), Some("json") | Some("jsonl")) {
                    candidates.push(entry_path.to_path_buf());
                }
            }
            continue;
        }

        let extension = canonical
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase());
        if !matches!(extension.as_deref(), Some("json") | Some("jsonl")) {
            failures.push(json!({
                "path": path_string(&canonical),
                "error": "path is not a .json/.jsonl transcript candidate",
            }));
            continue;
        }
        if canonical.starts_with(scope_root.join(".engram")) {
            failures.push(json!({
                "path": path_string(&canonical),
                "error": "path is inside .engram and outside local transcript scope",
            }));
            continue;
        }
        candidates.push(canonical);
    }

    candidates.sort();
    candidates.dedup();
    Ok((candidates, failures))
}

pub(crate) fn adapter_id_from_name(raw: &str) -> Option<AdapterId> {
    match raw {
        "claude-code" => Some(AdapterId::ClaudeCode),
        "codex-cli" => Some(AdapterId::CodexCli),
        "opencode" => Some(AdapterId::OpenCode),
        "gemini-cli" => Some(AdapterId::GeminiCli),
        "cursor" => Some(AdapterId::Cursor),
        "openclaw" => Some(AdapterId::OpenClaw),
        _ => None,
    }
}

pub(crate) fn cursor_state_path(paths: &RepoPaths, abs_path: &Path) -> PathBuf {
    let key = sha256_hex(&path_string(abs_path));
    paths.cursors.join(format!("{key}.json"))
}

pub(crate) fn load_ingest_state_for_path(
    paths: &RepoPaths,
    abs_path: &Path,
) -> Result<Option<IngestFileState>, CliError> {
    fs::create_dir_all(&paths.cursors).map_err(|err| CliError::io("mkdir_error", err))?;
    let state_path = cursor_state_path(paths, abs_path);
    if !state_path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&state_path).map_err(|err| CliError::io("read_error", err))?;
    let parsed = serde_json::from_str::<IngestFileState>(&content)
        .map_err(|err| CliError::new("cursor_state_error", err.to_string()))?;
    Ok(Some(parsed))
}

pub(crate) fn save_ingest_state_for_path(
    paths: &RepoPaths,
    abs_path: &Path,
    state: &IngestFileState,
) -> Result<(), CliError> {
    fs::create_dir_all(&paths.cursors).map_err(|err| CliError::io("mkdir_error", err))?;
    let state_path = cursor_state_path(paths, abs_path);
    let content = serde_json::to_string_pretty(state)
        .map_err(|err| CliError::new("cursor_state_error", err.to_string()))?;
    atomic_write(&state_path, content.as_bytes()).map_err(|err| CliError::io("write_error", err))
}

pub(crate) fn build_cursor_guard(
    path: &Path,
    byte_cursor: u64,
) -> Result<IngestCursorGuard, CliError> {
    let guard_len = usize::min(CURSOR_GUARD_WINDOW, byte_cursor as usize);
    let guard_offset = byte_cursor.saturating_sub(guard_len as u64);
    let mut bytes = vec![0u8; guard_len];
    if guard_len > 0 {
        let mut file = File::open(path).map_err(|err| CliError::io("read_error", err))?;
        file.seek(SeekFrom::Start(guard_offset))
            .map_err(|err| CliError::io("read_error", err))?;
        file.read_exact(&mut bytes)
            .map_err(|err| CliError::io("read_error", err))?;
    }
    Ok(IngestCursorGuard {
        offset: guard_offset,
        len: guard_len as u32,
        hash: sha256_hex_bytes(&bytes),
    })
}

pub(crate) fn ingest_cursor_guard_matches(
    path: &Path,
    guard: &IngestCursorGuard,
    file_len: u64,
) -> Result<bool, CliError> {
    let guard_end = guard.offset.saturating_add(guard.len as u64);
    if guard_end > file_len {
        return Ok(false);
    }
    let mut bytes = vec![0u8; guard.len as usize];
    if !bytes.is_empty() {
        let mut file = File::open(path).map_err(|err| CliError::io("read_error", err))?;
        file.seek(SeekFrom::Start(guard.offset))
            .map_err(|err| CliError::io("read_error", err))?;
        file.read_exact(&mut bytes)
            .map_err(|err| CliError::io("read_error", err))?;
    }
    Ok(sha256_hex_bytes(&bytes) == guard.hash)
}

pub(crate) fn complete_ingest_prefix_len(path: &Path, bytes: &[u8]) -> usize {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());
    if matches!(extension.as_deref(), Some("json")) {
        return bytes.len();
    }
    complete_jsonl_prefix_len(bytes)
}

pub(crate) fn complete_jsonl_prefix_len(bytes: &[u8]) -> usize {
    let mut offset = 0usize;
    let mut complete = 0usize;

    while offset < bytes.len() {
        if let Some(rel_newline) = bytes[offset..].iter().position(|value| *value == b'\n') {
            let line_end = offset + rel_newline + 1;
            let mut line = &bytes[offset..line_end];
            if let Some(stripped) = line.strip_suffix(b"\n") {
                line = stripped;
            }
            if let Some(stripped) = line.strip_suffix(b"\r") {
                line = stripped;
            }
            if line.is_empty() {
                complete = line_end;
                offset = line_end;
                continue;
            }
            if serde_json::from_slice::<Value>(line).is_ok() {
                complete = line_end;
                offset = line_end;
                continue;
            }
            break;
        }

        let mut line = &bytes[offset..];
        if let Some(stripped) = line.strip_suffix(b"\r") {
            line = stripped;
        }
        if line.is_empty() {
            break;
        }
        if serde_json::from_slice::<Value>(line).is_ok() {
            complete = bytes.len();
        }
        break;
    }

    complete
}

pub fn record_transcript(
    paths: &RepoPaths,
    db_path: &Path,
    transcript: &str,
    extra: Value,
    command_summary: Option<Value>,
) -> Result<(), CliError> {
    let events = parse_jsonl_events(transcript)?;
    let dispatch_links = extract_dispatch_links_from_transcript(transcript);
    let tape_id = tape_id_for_contents(transcript);
    let tape_path = tape_path_for_id(paths, &tape_id);
    let tape_file_exists = tape_path.exists();
    ensure_db_parent(db_path)?;
    let index = SqliteIndex::open_writer(&path_string(db_path))?;
    let already_indexed = index.has_tape(&tape_id)?;

    if !already_indexed {
        index.ingest_tape_events_with_dispatch(
            &tape_id,
            &events,
            &dispatch_links,
            LINK_THRESHOLD_DEFAULT,
        )?;
    }
    if !tape_file_exists {
        let compressed =
            compress_jsonl(transcript).map_err(|err| CliError::io("compress_error", err))?;
        atomic_write(&tape_path, &compressed).map_err(|err| CliError::io("write_error", err))?;
    }

    let compressed_len = fs::metadata(&tape_path)
        .map_err(|err| CliError::io("metadata_error", err))?
        .len();

    let mut payload = Map::new();
    payload.insert("status".to_string(), json!("ok"));
    payload.insert("tape_id".to_string(), json!(tape_id));
    payload.insert("path".to_string(), json!(tape_path));
    payload.insert("event_count".to_string(), json!(events.len()));
    payload.insert("uncompressed_bytes".to_string(), json!(transcript.len()));
    payload.insert("compressed_bytes".to_string(), json!(compressed_len));
    payload.insert(
        "already_exists".to_string(),
        json!(tape_file_exists && already_indexed),
    );
    payload.insert("already_indexed".to_string(), json!(already_indexed));
    payload.insert("tape_file_exists".to_string(), json!(tape_file_exists));
    payload.insert("meta".to_string(), json!(extract_meta(&events)));
    payload.insert("record".to_string(), extra);
    if let Some(command_summary) = command_summary {
        payload.insert("recorded_command".to_string(), command_summary);
    }

    print_json(&Value::Object(payload))
}

pub fn now_iso8601() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn git_head(cwd: &Path) -> Option<String> {
    let output = ProcessCommand::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8(output.stdout).ok()?;
    let trimmed = head.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn tape_id_for_contents(input: &str) -> String {
    sha256_hex(input)
}

pub(crate) fn sha256_hex(input: &str) -> String {
    sha256_hex_bytes(input.as_bytes())
}

pub(crate) fn sha256_hex_bytes(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub fn extract_meta(events: &[TapeEventAt]) -> Option<Value> {
    events.iter().find_map(|item| match &item.event.data {
        TapeEventData::Meta(meta) => Some(json!({
            "timestamp": item.event.timestamp,
            "model": meta.model,
            "repo_head": meta.repo_head,
            "label": meta.label,
            "coverage.read": meta.coverage_read,
            "coverage.edit": meta.coverage_edit,
            "coverage.tool": meta.coverage_tool,
        })),
        _ => None,
    })
}
