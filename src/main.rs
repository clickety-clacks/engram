use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::process::ExitCode;

use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use engram::anchor::fingerprint_text;
use engram::index::lineage::{
    Cardinality, EvidenceFragmentRef, EvidenceKind, LINK_THRESHOLD_DEFAULT, LocationDelta,
    StoredEdgeClass,
};
use engram::index::{EdgeRow, SqliteIndex};
use engram::query::explain::{
    ExplainTraversal, PrettyConfidenceTier, explain_by_anchor, pretty_tier,
};
use engram::tape::compress::{compress_jsonl, decompress_jsonl};
use engram::tape::event::{TapeEventAt, TapeEventData, parse_jsonl_events};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const TAPE_SUFFIX: &str = ".jsonl.zst";
const TRANSCRIPT_WINDOW_RADIUS: usize = 2;

#[derive(Debug)]
struct CliError {
    code: &'static str,
    message: String,
}

impl CliError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn io(code: &'static str, err: io::Error) -> Self {
        Self::new(code, err.to_string())
    }
}

impl From<rusqlite::Error> for CliError {
    fn from(value: rusqlite::Error) -> Self {
        Self::new("sqlite_error", value.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(value: serde_json::Error) -> Self {
        Self::new("json_error", value.to_string())
    }
}

#[derive(Parser, Debug)]
#[command(name = "engram")]
#[command(about = "A local-first causal index over code history")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Init,
    Record(RecordArgs),
    Explain(ExplainArgs),
    Tapes,
    Show(ShowArgs),
    Gc,
}

#[derive(Args, Debug)]
struct RecordArgs {
    #[arg(long)]
    stdin: bool,
    #[arg(trailing_var_arg = true)]
    command: Vec<String>,
}

#[derive(Args, Debug)]
struct ShowArgs {
    tape_id: String,
    #[arg(long)]
    raw: bool,
}

#[derive(Args, Debug)]
struct ExplainArgs {
    target: String,
    #[arg(long)]
    anchor: bool,
    #[arg(long, default_value_t = 0.50)]
    min_confidence: f32,
    #[arg(long, default_value_t = 50)]
    max_fanout: usize,
    #[arg(long, default_value_t = 500)]
    max_edges: usize,
    #[arg(long, default_value_t = 10)]
    depth: usize,
    #[arg(long)]
    include_deleted: bool,
    #[arg(long)]
    forensics: bool,
    #[arg(long)]
    pretty: bool,
}

#[derive(Debug, Clone)]
struct RepoPaths {
    root: PathBuf,
    index: PathBuf,
    tapes: PathBuf,
    objects: PathBuf,
}

#[derive(Debug, Clone)]
struct TapeRow {
    offset: u64,
    value: Value,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let payload = json!({
                "error": {
                    "code": err.code,
                    "message": err.message,
                }
            });
            eprintln!("{payload}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), CliError> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().map_err(|err| CliError::io("cwd_error", err))?;
    match cli.command {
        Command::Init => cmd_init(&cwd),
        Command::Record(args) => cmd_record(&cwd, args),
        Command::Explain(args) => cmd_explain(&cwd, args),
        Command::Tapes => cmd_tapes(&cwd),
        Command::Show(args) => cmd_show(&cwd, args),
        Command::Gc => cmd_gc(&cwd),
    }
}

fn cmd_init(cwd: &Path) -> Result<(), CliError> {
    let paths = repo_paths(cwd);
    fs::create_dir_all(&paths.tapes).map_err(|err| CliError::io("mkdir_error", err))?;
    fs::create_dir_all(&paths.objects).map_err(|err| CliError::io("mkdir_error", err))?;
    let _ = SqliteIndex::open(&path_string(&paths.index))?;

    print_json(&json!({
        "status": "ok",
        "engram_dir": paths.root,
        "index": paths.index,
    }))
}

fn cmd_record(cwd: &Path, args: RecordArgs) -> Result<(), CliError> {
    if args.stdin && !args.command.is_empty() {
        return Err(CliError::new(
            "invalid_record_args",
            "use either `engram record --stdin` or `engram record <command>`",
        ));
    }

    let paths = require_initialized_paths(cwd)?;
    if args.stdin {
        let mut stdin_buf = String::new();
        io::stdin()
            .read_to_string(&mut stdin_buf)
            .map_err(|err| CliError::io("stdin_error", err))?;
        return record_transcript(&paths, &stdin_buf, json!({ "mode": "stdin" }), None);
    }

    if args.command.is_empty() {
        return Err(CliError::new(
            "missing_record_command",
            "expected command args or --stdin",
        ));
    }

    let transcript = capture_command_tape(cwd, &args.command)?;
    record_transcript(
        &paths,
        &transcript.raw_jsonl,
        json!({
            "mode": "command",
            "command": args.command,
            "exit_code": transcript.exit_code,
            "success": transcript.success,
        }),
        Some(json!({
            "argv": transcript.argv,
            "exit": transcript.exit_code,
            "success": transcript.success,
            "stdout_bytes": transcript.stdout_bytes,
            "stderr_bytes": transcript.stderr_bytes,
        })),
    )
}

struct CapturedCommandTape {
    raw_jsonl: String,
    argv: Vec<String>,
    exit_code: i32,
    success: bool,
    stdout_bytes: usize,
    stderr_bytes: usize,
}

fn capture_command_tape(cwd: &Path, command: &[String]) -> Result<CapturedCommandTape, CliError> {
    let mut proc = ProcessCommand::new(&command[0]);
    if command.len() > 1 {
        proc.args(&command[1..]);
    }
    proc.current_dir(cwd);

    let started_at = now_iso8601();
    let output = proc
        .output()
        .map_err(|err| CliError::new("command_spawn_error", err.to_string()))?;
    let finished_at = now_iso8601();

    let exit_code = output.status.code().unwrap_or(-1);
    let success = output.status.success();
    let command_text = command.join(" ");
    let args_text = if command.len() > 1 {
        command[1..].join(" ")
    } else {
        String::new()
    };
    let cwd_text = cwd.to_string_lossy().into_owned();

    let mut lines = Vec::new();
    lines.push(json!({
        "t": started_at,
        "k": "meta",
        "model": "engram-cli",
        "repo_head": git_head(cwd),
        "label": "record-command",
    }));
    lines.push(json!({
        "t": started_at,
        "k": "tool.call",
        "tool": command_text,
        "args": args_text,
        "cwd": cwd_text,
    }));
    lines.push(json!({
        "t": finished_at,
        "k": "tool.result",
        "tool": command[0],
        "exit": exit_code,
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }));

    let raw_jsonl = lines
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n")
        + "\n";

    Ok(CapturedCommandTape {
        raw_jsonl,
        argv: command.to_vec(),
        exit_code,
        success,
        stdout_bytes: output.stdout.len(),
        stderr_bytes: output.stderr.len(),
    })
}

fn record_transcript(
    paths: &RepoPaths,
    transcript: &str,
    extra: Value,
    command_summary: Option<Value>,
) -> Result<(), CliError> {
    let events = parse_jsonl_events(transcript)?;
    let tape_id = tape_id_for_contents(transcript);
    let tape_path = tape_path_for_id(paths, &tape_id);
    let tape_file_exists = tape_path.exists();
    let index = SqliteIndex::open(&path_string(&paths.index))?;
    let already_indexed = index.has_tape(&tape_id)?;

    if !already_indexed {
        index.ingest_tape_events(&tape_id, &events, LINK_THRESHOLD_DEFAULT)?;
    }
    if !tape_file_exists {
        let compressed =
            compress_jsonl(transcript).map_err(|err| CliError::io("compress_error", err))?;
        fs::write(&tape_path, &compressed).map_err(|err| CliError::io("write_error", err))?;
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

fn now_iso8601() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn git_head(cwd: &Path) -> Option<String> {
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

fn cmd_tapes(cwd: &Path) -> Result<(), CliError> {
    let paths = require_initialized_paths(cwd)?;
    let mut tapes = Vec::new();

    let entries = fs::read_dir(&paths.tapes).map_err(|err| CliError::io("read_dir_error", err))?;
    for entry in entries {
        let entry = entry.map_err(|err| CliError::io("read_dir_error", err))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(tape_id) = tape_id_from_path(&path) else {
            continue;
        };

        let bytes = fs::read(&path).map_err(|err| CliError::io("read_error", err))?;
        let content =
            decompress_jsonl(&bytes).map_err(|err| CliError::io("decompress_error", err))?;
        let events = parse_jsonl_events(&content)?;
        let meta = extract_meta(&events);
        let timestamp = meta
            .as_ref()
            .and_then(|m| m.get("timestamp"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        tapes.push(json!({
            "tape_id": tape_id,
            "path": path,
            "compressed_bytes": bytes.len(),
            "event_count": events.len(),
            "timestamp": timestamp,
            "meta": meta,
        }));
    }

    tapes.sort_by(|a, b| {
        let a_count = a.get("event_count").and_then(Value::as_u64).unwrap_or(0);
        let b_count = b.get("event_count").and_then(Value::as_u64).unwrap_or(0);
        let a_ts = a.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let b_ts = b.get("timestamp").and_then(Value::as_str).unwrap_or("");
        b_ts.cmp(a_ts).then_with(|| b_count.cmp(&a_count))
    });

    print_json(&json!({ "tapes": tapes }))
}

fn cmd_show(cwd: &Path, args: ShowArgs) -> Result<(), CliError> {
    let paths = require_initialized_paths(cwd)?;
    let tape_path = tape_path_for_id(&paths, &args.tape_id);
    if !tape_path.exists() {
        return Err(CliError::new(
            "tape_not_found",
            format!("tape `{}` not found", args.tape_id),
        ));
    }

    let content = read_tape_content(&tape_path)?;
    if args.raw {
        print!("{content}");
        return Ok(());
    }

    let events = parse_jsonl_events(&content)?;
    let rows = parse_jsonl_rows(&content)?;
    let compacted = rows
        .iter()
        .map(|row| compact_event(row.offset, &row.value))
        .collect::<Vec<_>>();

    print_json(&json!({
        "tape_id": args.tape_id,
        "path": tape_path,
        "event_count": events.len(),
        "meta": extract_meta(&events),
        "events": compacted,
    }))
}

fn cmd_gc(cwd: &Path) -> Result<(), CliError> {
    let paths = require_initialized_paths(cwd)?;
    let index = SqliteIndex::open(&path_string(&paths.index))?;
    let referenced = index
        .referenced_tape_ids()?
        .into_iter()
        .collect::<HashSet<_>>();

    let mut deleted = Vec::new();
    let mut kept = 0usize;

    let entries = fs::read_dir(&paths.tapes).map_err(|err| CliError::io("read_dir_error", err))?;
    for entry in entries {
        let entry = entry.map_err(|err| CliError::io("read_dir_error", err))?;
        let path = entry.path();
        let Some(tape_id) = tape_id_from_path(&path) else {
            continue;
        };

        if referenced.contains(&tape_id) {
            kept += 1;
            continue;
        }

        fs::remove_file(&path).map_err(|err| CliError::io("remove_file_error", err))?;
        deleted.push(tape_id);
    }

    deleted.sort();
    print_json(&json!({
        "status": "ok",
        "deleted_tape_ids": deleted,
        "deleted_count": deleted.len(),
        "kept_count": kept,
    }))
}

fn cmd_explain(cwd: &Path, args: ExplainArgs) -> Result<(), CliError> {
    let paths = require_initialized_paths(cwd)?;
    let anchors = resolve_explain_anchors(cwd, &args)?;

    let index = SqliteIndex::open(&path_string(&paths.index))?;
    let traversal = ExplainTraversal {
        min_confidence: args.min_confidence,
        max_fanout: args.max_fanout,
        max_edges: args.max_edges,
        max_depth: args.depth,
    };
    let result = explain_by_anchor(&index, &anchors, traversal, args.forensics)?;

    let touches = collect_touch_evidence(&index, &result.direct, &result.touched_anchors)?;
    let sessions = build_session_windows(&paths, touches)?;

    let mut tombstones = Vec::new();
    if args.include_deleted {
        for anchor in &result.touched_anchors {
            for tombstone in index.tombstones_for_anchor(anchor)? {
                tombstones.push(json!({
                    "anchor": anchor,
                    "tape_id": tombstone.tape_id,
                    "event_offset": tombstone.event_offset,
                    "file_path": tombstone.file_path,
                    "range": {
                        "start": tombstone.range_at_deletion.start,
                        "end": tombstone.range_at_deletion.end
                    },
                    "timestamp": tombstone.timestamp,
                }));
            }
        }
    }

    if args.pretty {
        print_pretty_explain(&args.target, &result.lineage, &sessions, &tombstones);
        return Ok(());
    }

    let lineage = result.lineage.iter().map(edge_to_json).collect::<Vec<_>>();

    print_json(&json!({
        "query": {
            "target": args.target,
            "anchor_mode": args.anchor,
            "anchors": anchors,
            "min_confidence": args.min_confidence,
            "max_fanout": args.max_fanout,
            "max_edges": args.max_edges,
            "depth": args.depth,
            "forensics": args.forensics,
            "include_deleted": args.include_deleted,
        },
        "sessions": sessions,
        "lineage": lineage,
        "tombstones": tombstones,
    }))
}

fn collect_touch_evidence(
    index: &SqliteIndex,
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
        for fragment in index.evidence_for_anchor(anchor)? {
            let key = touch_key(&fragment);
            if dedup.insert(key) {
                out.push(fragment);
            }
        }
    }

    Ok(out)
}

fn touch_key(fragment: &EvidenceFragmentRef) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        fragment.tape_id,
        fragment.event_offset,
        evidence_kind_name(fragment.kind),
        fragment.file_path,
        fragment.timestamp
    )
}

fn build_session_windows(
    paths: &RepoPaths,
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
        let tape_path = tape_path_for_id(paths, &tape_id);
        if !tape_path.exists() {
            continue;
        }

        let content = read_tape_content(&tape_path)?;
        let rows = parse_jsonl_rows(&content)?;

        let windows = tape_touches
            .iter()
            .filter_map(|touch| event_window(&rows, touch.event_offset, TRANSCRIPT_WINDOW_RADIUS))
            .collect::<Vec<_>>();

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

fn event_window(rows: &[TapeRow], target_offset: u64, radius: usize) -> Option<Value> {
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

fn print_pretty_explain(
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

fn resolve_explain_anchors(cwd: &Path, args: &ExplainArgs) -> Result<Vec<String>, CliError> {
    if args.anchor {
        return Ok(vec![args.target.clone()]);
    }

    let (file, start, end) = parse_file_range_target(&args.target)?;
    let file_path = cwd.join(file);
    let span_text = read_file_span(&file_path, start, end)?;
    let anchor = fingerprint_text(&span_text);
    Ok(vec![anchor.fingerprint])
}

fn parse_file_range_target(target: &str) -> Result<(&str, u32, u32), CliError> {
    let (file, range) = target
        .rsplit_once(':')
        .ok_or_else(|| CliError::new("invalid_explain_target", "expected <file>:<start>-<end>"))?;
    let (start_raw, end_raw) = range
        .split_once('-')
        .ok_or_else(|| CliError::new("invalid_explain_target", "expected <file>:<start>-<end>"))?;

    let start: u32 = start_raw
        .parse()
        .map_err(|_| CliError::new("invalid_explain_target", "start line must be an integer"))?;
    let end: u32 = end_raw
        .parse()
        .map_err(|_| CliError::new("invalid_explain_target", "end line must be an integer"))?;
    if start == 0 || end == 0 || end < start {
        return Err(CliError::new(
            "invalid_explain_target",
            "line range must be 1-based and end must be >= start",
        ));
    }

    Ok((file, start, end))
}

fn read_file_span(path: &Path, start: u32, end: u32) -> Result<String, CliError> {
    let content = fs::read_to_string(path).map_err(|err| CliError::io("read_span_error", err))?;
    let lines = content.lines().collect::<Vec<_>>();
    let start_idx = start as usize - 1;
    let end_idx = end as usize - 1;

    if end_idx >= lines.len() {
        return Err(CliError::new(
            "span_out_of_bounds",
            format!(
                "requested range {}-{} exceeds file length {}",
                start,
                end,
                lines.len()
            ),
        ));
    }

    Ok(lines[start_idx..=end_idx].join("\n"))
}

fn parse_jsonl_rows(input: &str) -> Result<Vec<TapeRow>, CliError> {
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

fn compact_event(offset: u64, event: &Value) -> Value {
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

fn edge_to_json(edge: &EdgeRow) -> Value {
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

fn repo_paths(cwd: &Path) -> RepoPaths {
    let root = cwd.join(".engram");
    RepoPaths {
        index: root.join("index.sqlite"),
        tapes: root.join("tapes"),
        objects: root.join("objects"),
        root,
    }
}

fn require_initialized_paths(cwd: &Path) -> Result<RepoPaths, CliError> {
    let paths = repo_paths(cwd);
    if !paths.root.exists() || !paths.index.exists() || !paths.tapes.exists() {
        return Err(CliError::new(
            "not_initialized",
            "repository is not initialized; run `engram init`",
        ));
    }
    Ok(paths)
}

fn tape_path_for_id(paths: &RepoPaths, tape_id: &str) -> PathBuf {
    paths.tapes.join(format!("{tape_id}{TAPE_SUFFIX}"))
}

fn tape_id_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    file_name.strip_suffix(TAPE_SUFFIX).map(ToOwned::to_owned)
}

fn read_tape_content(path: &Path) -> Result<String, CliError> {
    let bytes = fs::read(path).map_err(|err| CliError::io("read_error", err))?;
    decompress_jsonl(&bytes).map_err(|err| CliError::io("decompress_error", err))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn print_json(value: &Value) -> Result<(), CliError> {
    let rendered = serde_json::to_string(value)?;
    println!("{rendered}");
    Ok(())
}

fn tape_id_for_contents(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn extract_meta(events: &[TapeEventAt]) -> Option<Value> {
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

fn evidence_kind_name(kind: EvidenceKind) -> &'static str {
    match kind {
        EvidenceKind::Edit => "edit",
        EvidenceKind::Read => "read",
        EvidenceKind::Tool => "tool",
        EvidenceKind::Message => "message",
    }
}

fn stored_class_name(class: StoredEdgeClass) -> &'static str {
    match class {
        StoredEdgeClass::Lineage => "lineage",
        StoredEdgeClass::LocationOnly => "location_only",
    }
}

fn location_delta_name(delta: LocationDelta) -> &'static str {
    match delta {
        LocationDelta::Same => "same",
        LocationDelta::Adjacent => "adjacent",
        LocationDelta::Moved => "moved",
        LocationDelta::Absent => "absent",
    }
}

fn cardinality_name(cardinality: Cardinality) -> &'static str {
    match cardinality {
        Cardinality::OneToOne => "1:1",
        Cardinality::OneToMany => "1:N",
        Cardinality::ManyToOne => "N:1",
    }
}

fn pretty_tier_name(tier: PrettyConfidenceTier) -> &'static str {
    match tier {
        PrettyConfidenceTier::Edit => "edit",
        PrettyConfidenceTier::Move => "move",
        PrettyConfidenceTier::Related => "related",
        PrettyConfidenceTier::Hidden => "hidden",
        PrettyConfidenceTier::ForensicsOnly => "forensics_only",
    }
}
