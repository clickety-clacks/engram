//! One-time, exact legacy native chronology recovery. Original tapes and evidence
//! stay untouched; immutable context tapes and small locator files bind old offsets.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::store::atomic::atomic_write;
use crate::store::tapes::{read_tape_content, tape_id_from_path, tape_path_for_tapes_dir};
use crate::{CliError, RuntimeContext};

use super::tape_id_for_contents;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Point {
    pub old_offset: u64,
    pub source_offset: u64,
    pub turn: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct RecoveredTape {
    pub tape_id: String,
    pub points: Vec<Point>,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Locator {
    pub context_tape: String,
    pub recovered: RecoveredTape,
}

fn error(message: impl Into<String>) -> CliError {
    CliError::new("native_recovery_error", message.into())
}

const MAX_RECOVERY_LOCATOR_BYTES: u64 = 1024 * 1024;

fn valid_tape_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn read_locator(path: &Path) -> Result<Locator, CliError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| CliError::io("native_recovery_error", e))?;
    if !metadata.file_type().is_file() {
        return Err(error("recovery locator is not a regular file"));
    }
    if metadata.len() > MAX_RECOVERY_LOCATOR_BYTES {
        return Err(error("recovery locator exceeds the 1 MiB limit"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|e| CliError::io("native_recovery_error", e))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_RECOVERY_LOCATOR_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| CliError::io("native_recovery_error", e))?;
    if bytes.len() as u64 > MAX_RECOVERY_LOCATOR_BYTES {
        return Err(error("recovery locator exceeds the 1 MiB limit"));
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| error(format!("recovery locator is invalid: {e}")))
}

fn directory(context: &RuntimeContext) -> PathBuf {
    context.tapes_dir.join("native-upgrade-v1")
}
fn rows(content: &str) -> Result<Vec<Value>, CliError> {
    content
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}
fn key(row: &Value) -> String {
    let mut row = row.clone();
    // Stateless suffix conversion could omit session identity and the paired
    // result's tool name. All timestamp, call ID, content and code fields remain.
    if let Some(source) = row["source"].as_object_mut() {
        source.remove("session_id");
    }
    if row["k"] == "tool.result" {
        row.as_object_mut().unwrap().remove("tool");
    }
    tape_id_for_contents(&row.to_string())
}
// Omitted legacy identity/tool metadata may be filled by full-prefix conversion;
// explicitly recorded values must agree before an event is a matching candidate.
fn compatible(old: &Value, full: &Value) -> bool {
    if old["k"] == "tool.result" && old.get("exit") != full.get("exit") {
        // A stateless Codex suffix cannot derive exec success without its call.
        // Only this absent annotation may be filled; recorded exits still bind.
        if !(old.get("exit").is_none()
            && old["tool"] == "unknown"
            && full["source"]["harness"] == "codex-cli"
            && full["tool"] == "exec"
            && full["exit"] == 0)
        {
            return false;
        }
    }
    if let (Some(a), Some(b)) = (
        old["source"]["session_id"].as_str(),
        full["source"]["session_id"].as_str(),
    ) && a != b
    {
        return false;
    }
    !(old["k"] == "tool.result"
        && old["tool"].as_str().is_some_and(|tool| tool != "unknown")
        && old["tool"] != full["tool"])
}

fn verified_rows(context: &RuntimeContext, id: &str) -> Result<Vec<Value>, CliError> {
    let text = read_tape_content(&tape_path_for_tapes_dir(&context.tapes_dir, id))?;
    if tape_id_for_contents(&text) != id {
        return Err(error(format!("legacy tape hash mismatch: {id}")));
    }
    rows(&text)
}

/// Freeze an inventory of pre-upgrade native tapes once per store, not once per
/// cursor or poll. Only one event hash per tape is retained in this small catalog.
fn catalog(context: &RuntimeContext) -> Result<BTreeMap<String, Vec<String>>, CliError> {
    let dir = directory(context);
    let path = dir.join("catalog.json");
    if path.exists() {
        return Ok(serde_json::from_slice(
            &fs::read(path).map_err(|e| CliError::io("read_error", e))?,
        )?);
    }
    let mut catalog = BTreeMap::<String, Vec<String>>::new();
    for entry in fs::read_dir(&context.tapes_dir).map_err(|e| CliError::io("read_error", e))? {
        let path = entry.map_err(|e| CliError::io("read_error", e))?.path();
        let Some(id) = tape_id_from_path(&path) else {
            continue;
        };
        let rows = verified_rows(context, &id)?;
        let Some(meta) = rows.iter().find(|r| r["k"] == "meta") else {
            continue;
        };
        if meta["ingest_context_only"] == true || !meta["ingest_continuation"].is_null() {
            continue;
        }
        if !matches!(
            meta["source"]["harness"].as_str(),
            Some("codex-cli" | "claude-code")
        ) {
            continue;
        }
        if let Some(first) = rows.iter().find(|r| r["k"] != "meta") {
            catalog.entry(key(first)).or_default().push(id);
        }
    }
    for ids in catalog.values_mut() {
        ids.sort();
    }
    fs::create_dir_all(dir).map_err(|e| CliError::io("mkdir_error", e))?;
    atomic_write(&path, &serde_json::to_vec(&catalog)?)
        .map_err(|e| CliError::io("write_error", e))?;
    Ok(catalog)
}

/// Bind the entire legacy event sequence uniquely within the retained raw prefix.
/// A missing/ambiguous current segment is a visible error, never a guess.
pub(super) fn plan(
    context: &RuntimeContext,
    current: &str,
    full: &str,
    projection: &mut [Value],
) -> Result<Vec<RecoveredTape>, CliError> {
    let full = rows(full)?;
    let mut positions = BTreeMap::<String, Vec<(u64, i64)>>::new();
    let mut turn = 0;
    for (offset, row) in full.iter().enumerate() {
        if row["k"] == "meta" {
            continue;
        }
        positions
            .entry(key(row))
            .or_default()
            .push((offset as u64, turn));
        if row["k"] == "tool.result"
            && row["source"]["harness"] == "codex-cli"
            && row["tool"] == "exec"
            && row["exit"] == 0
        {
            // Preserve existing catalog keys. Add the exact legacy stateless
            // form only: raw output, timestamp, call ID and chronology still bind.
            // compatible() requires unknown tool + absent exit for this alias.
            let mut legacy = row.clone();
            legacy.as_object_mut().unwrap().remove("exit");
            positions
                .entry(key(&legacy))
                .or_default()
                .push((offset as u64, turn));
        }
        if crate::dispatch::is_message_row(row) {
            turn += 1;
        }
    }
    let mut candidates = BTreeSet::from([current.to_string()]);
    for (first, ids) in catalog(context)? {
        if positions.contains_key(&first) {
            candidates.extend(ids);
        }
    }
    let mut recovered = Vec::new();
    'candidate: for id in candidates {
        let old = verified_rows(context, &id)?;
        let events: Vec<_> = old
            .iter()
            .enumerate()
            .filter(|(_, r)| r["k"] != "meta")
            .collect();
        if events.iter().any(|(_, r)| !positions.contains_key(&key(r))) {
            if id == current {
                return Err(error(format!(
                    "current legacy segment {id} does not exactly match committed source prefix"
                )));
            }
            continue;
        }
        let matches: Vec<_> = events
            .iter()
            .map(|(_, row)| positions[&key(row)].as_slice())
            .collect();
        for ((_, row), matches) in events.iter().zip(&matches) {
            if !matches
                .iter()
                .any(|(offset, _)| compatible(row, &full[*offset as usize]))
            {
                // The catalog's first-event key intentionally omits session/tool
                // metadata; an unrelated catalog tape is not this source's history.
                // The cursor's own tape, however, must never be skipped.
                if id != current {
                    continue 'candidate;
                }
                return Err(error(format!(
                    "legacy session/tool mismatch in current segment {id}"
                )));
            }
        }
        // Earliest and latest monotone embeddings bound every valid alignment.
        // They agree at every event iff the complete sequence binds uniquely.
        // Repeated rows are valid when their surrounding chronology fixes them;
        // choosing the first duplicate alone would silently invent offsets.
        let mut earliest = Vec::with_capacity(events.len());
        let mut previous = None;
        for ((_, row), matches) in events.iter().zip(&matches) {
            let begin =
                matches.partition_point(|(offset, _)| previous.is_some_and(|p| *offset <= p));
            let Some(&(offset, turn)) = matches[begin..]
                .iter()
                .find(|(offset, _)| compatible(row, &full[*offset as usize]))
            else {
                return Err(error(format!("legacy chronology mismatch in {id}")));
            };
            earliest.push((offset, turn));
            previous = Some(offset);
        }
        let mut next = full.len() as u64;
        for (((old_offset, row), matches), &(earliest_offset, _)) in
            events.iter().zip(&matches).zip(&earliest).rev()
        {
            let end = matches.partition_point(|(offset, _)| *offset < next);
            let Some(&(offset, _)) = matches[..end]
                .iter()
                .rfind(|(offset, _)| compatible(row, &full[*offset as usize]))
            else {
                return Err(error(format!("legacy chronology mismatch in {id}")));
            };
            if offset != earliest_offset {
                return Err(error(format!(
                    "ambiguous legacy event in {id} at offset {old_offset}"
                )));
            }
            next = offset;
        }
        let points = events
            .iter()
            .zip(earliest)
            .filter(|((_, row), _)| row["k"] == "code.edit")
            .map(|((old_offset, _), (source_offset, turn))| Point {
                old_offset: *old_offset as u64,
                source_offset,
                turn,
            })
            .collect();
        recovered.push(RecoveredTape {
            tape_id: id,
            points,
        });
    }
    if let Some(meta) = projection.iter_mut().find(|r| r["k"] == "meta") {
        meta["native_recovery_v1"] = json!(recovered);
    }
    Ok(recovered)
}

pub(super) fn publish(
    context: &RuntimeContext,
    context_tape: &str,
    recovered: &[RecoveredTape],
) -> Result<(), CliError> {
    let dir = directory(context);
    fs::create_dir_all(&dir).map_err(|e| CliError::io("mkdir_error", e))?;
    let mut pending = Vec::new();
    for recovered in recovered {
        let path = dir.join(format!("{}.json", recovered.tape_id));
        let bytes = serde_json::to_vec(&Locator {
            context_tape: context_tape.into(),
            recovered: recovered.clone(),
        })?;
        if path.exists() {
            if fs::read(&path).map_err(|e| CliError::io("read_error", e))? != bytes {
                return Err(error(format!(
                    "conflicting existing recovery locator: {}",
                    recovered.tape_id
                )));
            }
        } else {
            pending.push((path, bytes));
        }
    }
    // Check all conflicts before publishing any locator. The cursor is committed
    // only after every atomic locator write, so an interrupted attempt is retryable.
    for (path, bytes) in pending {
        atomic_write(&path, &bytes).map_err(|e| CliError::io("write_error", e))?;
    }
    Ok(())
}

/// A query verifies each immutable recovery context once, not once per touch.
#[derive(Default)]
pub(crate) struct QueryRecovery {
    locators: HashMap<String, Option<Locator>>,
    contexts: HashMap<String, Vec<RecoveredTape>>,
}
impl QueryRecovery {
    pub(crate) fn lookup(
        &mut self,
        context: &RuntimeContext,
        tape: &str,
    ) -> Result<Option<&Locator>, CliError> {
        self.lookup_with_reader(&context.tape_lookup_dirs, tape, read_tape_content)
    }

    /// Resolve and verify one tape's recovery binding with an owner-selected
    /// bounded reader. The negative and positive locator result is memoized
    /// for this QueryRecovery invocation.
    pub(crate) fn lookup_with_reader(
        &mut self,
        tape_lookup_dirs: &[PathBuf],
        tape: &str,
        mut read_tape: impl FnMut(&Path) -> Result<String, CliError>,
    ) -> Result<Option<&Locator>, CliError> {
        if !self.locators.contains_key(tape) {
            let mut found = None;
            for dir in tape_lookup_dirs {
                let path = dir.join("native-upgrade-v1").join(format!("{tape}.json"));
                match fs::symlink_metadata(&path) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(CliError::io("native_recovery_error", error));
                    }
                }
                let locator = read_locator(&path)?;
                if locator.recovered.tape_id != tape {
                    return Err(error("recovery locator tape mismatch"));
                }
                if !valid_tape_id(&locator.context_tape) {
                    return Err(error("recovery locator context tape ID is invalid"));
                }
                if !self.contexts.contains_key(&locator.context_tape) {
                    let raw = read_tape(&tape_path_for_tapes_dir(dir, &locator.context_tape))
                        .map_err(|cause| {
                            error(format!(
                                "recovery context is unavailable: {}",
                                cause.message
                            ))
                        })?;
                    if tape_id_for_contents(&raw) != locator.context_tape {
                        return Err(error("recovery context hash mismatch"));
                    }
                    let stored = rows(&raw)
                        .map_err(|e| error(format!("recovery context JSON is invalid: {e}")))?
                        .into_iter()
                        .find(|r| r["k"] == "meta")
                        .ok_or_else(|| error("recovery context meta missing"))?;
                    let recoveries: Vec<RecoveredTape> =
                        serde_json::from_value(stored["native_recovery_v1"].clone())
                            .map_err(|e| error(format!("recovery context binding is invalid: {e}")))?;
                    self.contexts
                        .insert(locator.context_tape.clone(), recoveries);
                }
                if !self.contexts[&locator.context_tape].contains(&locator.recovered) {
                    return Err(error("recovery locator not bound by context"));
                }
                found = Some(locator);
                break;
            }
            self.locators.insert(tape.to_string(), found);
        }
        Ok(self.locators.get(tape).and_then(Option::as_ref))
    }
}
