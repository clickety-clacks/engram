use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::config::EffectiveConfig;
use crate::tape::compress::decompress_jsonl;
use crate::{CliError, RepoPaths, RuntimeContext};

const TAPE_SUFFIX: &str = ".jsonl.zst";

pub(crate) fn tape_path_for_id(paths: &RepoPaths, tape_id: &str) -> PathBuf {
    paths.tapes.join(format!("{tape_id}{TAPE_SUFFIX}"))
}

pub(crate) fn tape_path_for_tapes_dir(tapes_dir: &Path, tape_id: &str) -> PathBuf {
    tapes_dir.join(format!("{tape_id}{TAPE_SUFFIX}"))
}

pub fn tape_lookup_dirs(cwd: &Path, home: &Path, config: &EffectiveConfig) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    push_tape_lookup_dir(&mut dirs, config.tapes_dir.clone());
    push_tape_lookup_dir(&mut dirs, cwd.join(".engram").join("tapes"));
    push_tape_lookup_dir(&mut dirs, home.join(".engram").join("tapes"));
    for store in &config.additional_stores {
        let store_tapes = store
            .parent()
            .map(|parent| parent.join("tapes"))
            .unwrap_or_else(|| PathBuf::from("tapes"));
        push_tape_lookup_dir(&mut dirs, store_tapes);
    }
    dirs
}

pub(crate) fn push_tape_lookup_dir(dirs: &mut Vec<PathBuf>, candidate: PathBuf) {
    if dirs.iter().all(|existing| existing != &candidate) {
        dirs.push(candidate);
    }
}

pub fn resolve_tape_path(context: &RuntimeContext, tape_id: &str) -> Option<PathBuf> {
    context
        .tape_lookup_dirs
        .iter()
        .map(|dir| tape_path_for_tapes_dir(dir, tape_id))
        .find(|path| path.exists())
}

pub fn tape_id_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    file_name.strip_suffix(TAPE_SUFFIX).map(ToOwned::to_owned)
}

pub fn read_tape_content(path: &Path) -> Result<String, CliError> {
    let bytes = fs::read(path).map_err(|err| CliError::io("read_error", err))?;
    decompress_jsonl(&bytes).map_err(|err| CliError::io("decompress_error", err))
}

#[derive(Debug, Clone)]
pub struct TapeRow {
    pub offset: u64,
    pub value: Value,
}

pub(crate) fn event_window(rows: &[TapeRow], target_offset: u64, radius: usize) -> Option<Value> {
    let pos = rows.iter().position(|row| row.offset == target_offset)?;
    let start = pos.saturating_sub(radius);
    let end = usize::min(rows.len().saturating_sub(1), pos + radius);
    let events = rows[start..=end]
        .iter()
        .map(|row| {
            json!({
                "offset": row.offset,
                "event": row.value,
            })
        })
        .collect::<Vec<_>>();

    Some(json!({
        "touch_offset": target_offset,
        "events": events,
    }))
}

pub fn parse_jsonl_rows(input: &str) -> Result<Vec<TapeRow>, CliError> {
    let mut rows = Vec::new();
    for (idx, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)?;
        rows.push(TapeRow {
            offset: idx as u64,
            value,
        });
    }
    Ok(rows)
}

pub(crate) fn load_tape_rows_cached<'a>(
    context: &RuntimeContext,
    cache: &'a mut HashMap<String, Vec<TapeRow>>,
    tape_id: &str,
) -> Result<&'a Vec<TapeRow>, CliError> {
    if !cache.contains_key(tape_id) {
        let Some(tape_path) = resolve_tape_path(context, tape_id) else {
            cache.insert(tape_id.to_string(), Vec::new());
            return Ok(cache.get(tape_id).expect("cache entry inserted"));
        };
        let content = read_tape_content(&tape_path)?;
        cache.insert(tape_id.to_string(), parse_jsonl_rows(&content)?);
    }
    Ok(cache.get(tape_id).expect("cache entry inserted"))
}

pub fn print_json(value: &Value) -> Result<(), CliError> {
    let rendered = serde_json::to_string(value)?;
    println!("{rendered}");
    Ok(())
}
