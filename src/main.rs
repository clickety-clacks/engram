use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::process::ExitCode;

use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use engram::anchor::fingerprint_text;
use engram::config::{
    AdapterChoice, default_global_config_yaml, default_repo_config_yaml, expand_tilde,
    load_effective_config,
};
use engram::index::lineage::{
    Cardinality, EvidenceFragmentRef, EvidenceKind, LINK_THRESHOLD_DEFAULT, LocationDelta,
    StoredEdgeClass,
};
use engram::index::{EdgeRow, SqliteIndex};
use engram::query::explain::{
    ExplainTraversal, PrettyConfidenceTier, explain_by_anchor, pretty_tier,
};
use engram::tape::adapter::{AdapterId, convert_with_adapter};
use engram::tape::compress::{compress_jsonl, decompress_jsonl};
use engram::tape::event::{TapeEventAt, TapeEventData, parse_jsonl_events};
use glob::glob;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

const TAPE_SUFFIX: &str = ".jsonl.zst";
const TRANSCRIPT_WINDOW_RADIUS: usize = 2;
const CURSOR_STATE_FILE: &str = "ingest-state.json";

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
    #[arg(long, global = true)]
    global: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Init,
    Ingest,
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
    cache_root: PathBuf,
    cursors: PathBuf,
    repo_config: PathBuf,
    user_config: PathBuf,
    mode: StorageMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageMode {
    RepoLocal,
    Global,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct IngestState {
    files: HashMap<String, IngestFileState>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IngestFileState {
    input_hash: String,
    adapter: String,
    tape_id: String,
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
    let paths = repo_paths(&cwd, cli.global)?;
    match cli.command {
        Command::Init => cmd_init(&paths),
        Command::Ingest => cmd_ingest(&cwd, &paths),
        Command::Record(args) => cmd_record(&cwd, &paths, args),
        Command::Explain(args) => cmd_explain(&cwd, &paths, args),
        Command::Tapes => cmd_tapes(&paths),
        Command::Show(args) => cmd_show(&paths, args),
        Command::Gc => cmd_gc(&paths),
    }
}

fn cmd_init(paths: &RepoPaths) -> Result<(), CliError> {
    fs::create_dir_all(&paths.tapes).map_err(|err| CliError::io("mkdir_error", err))?;
    fs::create_dir_all(&paths.objects).map_err(|err| CliError::io("mkdir_error", err))?;
    fs::create_dir_all(&paths.cursors).map_err(|err| CliError::io("mkdir_error", err))?;
    let _ = SqliteIndex::open(&path_string(&paths.index))?;
    write_default_config(paths)?;

    print_json(&json!({
        "status": "ok",
        "engram_dir": paths.root,
        "cache_dir": paths.cache_root,
        "index": paths.index,
        "mode": match paths.mode {
            StorageMode::RepoLocal => "repo",
            StorageMode::Global => "global",
        },
    }))
}

fn cmd_record(cwd: &Path, paths: &RepoPaths, args: RecordArgs) -> Result<(), CliError> {
    if args.stdin && !args.command.is_empty() {
        return Err(CliError::new(
            "invalid_record_args",
            "use either `engram record --stdin` or `engram record <command>`",
        ));
    }

    require_initialized_paths(paths)?;
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

fn cmd_ingest(cwd: &Path, paths: &RepoPaths) -> Result<(), CliError> {
    require_initialized_paths(paths)?;
    let home = home_dir()?;
    let config = load_effective_config(Some(&paths.repo_config), Some(&paths.user_config))
        .map_err(|err| CliError::new("config_error", err.to_string()))?;
    if config.sources.is_empty() {
        return Err(CliError::new(
            "missing_sources",
            "no ingest sources configured; add sources in .engram/config.yml or ~/.engram/config.yml",
        ));
    }

    let candidates = resolve_source_files(cwd, &home, &config.sources, &config.exclude)?;
    let mut state = load_ingest_state(paths)?;
    let index = SqliteIndex::open(&path_string(&paths.index))?;

    let mut scanned = 0usize;
    let mut imported = 0usize;
    let mut skipped_unchanged = 0usize;
    let mut skipped_existing_tape = 0usize;
    let mut failures = Vec::new();

    for candidate in candidates {
        scanned += 1;
        let input = match fs::read_to_string(&candidate.path) {
            Ok(content) => content,
            Err(err) => {
                failures.push(json!({
                    "path": candidate.path,
                    "error": err.to_string(),
                }));
                continue;
            }
        };
        let input_hash = sha256_hex(&input);
        let state_key = format!(
            "{}:{}",
            candidate.adapter.as_str(),
            candidate.path.to_string_lossy()
        );
        if let Some(prev) = state.files.get(&state_key) {
            if prev.input_hash == input_hash {
                skipped_unchanged += 1;
                continue;
            }
        }

        let normalized = match convert_with_adapter(candidate.adapter, &input) {
            Ok(output) => output,
            Err(err) => {
                failures.push(json!({
                    "path": candidate.path,
                    "adapter": candidate.adapter.as_str(),
                    "error": err.to_string(),
                }));
                continue;
            }
        };
        let events = match parse_jsonl_events(&normalized) {
            Ok(events) => events,
            Err(err) => {
                failures.push(json!({
                    "path": candidate.path,
                    "adapter": candidate.adapter.as_str(),
                    "error": err.to_string(),
                }));
                continue;
            }
        };

        let tape_id = tape_id_for_contents(&normalized);
        let tape_path = tape_path_for_id(paths, &tape_id);
        let tape_file_exists = tape_path.exists();
        if !tape_file_exists {
            let compressed =
                compress_jsonl(&normalized).map_err(|err| CliError::io("compress_error", err))?;
            fs::write(&tape_path, &compressed).map_err(|err| CliError::io("write_error", err))?;
        }

        let already_indexed = index.has_tape(&tape_id)?;
        if !already_indexed {
            index.ingest_tape_events(&tape_id, &events, LINK_THRESHOLD_DEFAULT)?;
            imported += 1;
        } else {
            skipped_existing_tape += 1;
        }

        state.files.insert(
            state_key,
            IngestFileState {
                input_hash,
                adapter: candidate.adapter.as_str().to_string(),
                tape_id,
            },
        );
    }

    save_ingest_state(paths, &state)?;

    print_json(&json!({
        "status": if failures.is_empty() { "ok" } else { "partial" },
        "scanned_inputs": scanned,
        "imported_tapes": imported,
        "skipped_unchanged": skipped_unchanged,
        "skipped_existing_tape": skipped_existing_tape,
        "failure_count": failures.len(),
        "failures": failures,
    }))
}

struct CapturedCommandTape {
    raw_jsonl: String,
    argv: Vec<String>,
    exit_code: i32,
    success: bool,
    stdout_bytes: usize,
    stderr_bytes: usize,
}

#[derive(Debug, Clone)]
struct IngestCandidate {
    path: PathBuf,
    adapter: AdapterId,
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

fn resolve_source_files(
    cwd: &Path,
    home: &Path,
    sources: &[engram::config::SourceSpec],
    exclude_patterns: &[String],
) -> Result<Vec<IngestCandidate>, CliError> {
    let mut out = Vec::new();
    let excludes = compile_excludes(cwd, home, exclude_patterns)?;

    for source in sources {
        let raw_path = source.path.trim();
        if raw_path.is_empty() {
            continue;
        }
        let expanded = expand_tilde(raw_path, home);
        let source_files = if looks_like_glob(raw_path) {
            glob_paths(&expanded)?
        } else if expanded.is_dir() {
            WalkDir::new(&expanded)
                .into_iter()
                .filter_map(Result::ok)
                .map(|entry| entry.path().to_path_buf())
                .filter(|path| path.is_file())
                .collect::<Vec<_>>()
        } else if expanded.is_file() {
            vec![expanded]
        } else {
            Vec::new()
        };

        for path in source_files {
            if is_excluded(&path, &excludes) {
                continue;
            }
            let adapter = match source.adapter {
                AdapterChoice::Auto => {
                    let input = fs::read_to_string(&path)
                        .map_err(|err| CliError::new("read_source_error", err.to_string()))?;
                    detect_adapter_for_input(&path, &input).ok_or_else(|| {
                        CliError::new(
                            "adapter_detection_error",
                            format!("unable to detect adapter for {}", path.display()),
                        )
                    })?
                }
                AdapterChoice::Codex => AdapterId::CodexCli,
                AdapterChoice::Claude => AdapterId::ClaudeCode,
                AdapterChoice::Cursor => AdapterId::Cursor,
                AdapterChoice::Gemini => AdapterId::GeminiCli,
                AdapterChoice::OpenCode => AdapterId::OpenCode,
                AdapterChoice::OpenClaw => AdapterId::OpenClaw,
            };
            out.push(IngestCandidate { path, adapter });
        }
    }

    out.sort_by(|a, b| {
        let a_key = format!("{}:{}", a.adapter.as_str(), a.path.to_string_lossy());
        let b_key = format!("{}:{}", b.adapter.as_str(), b.path.to_string_lossy());
        a_key.cmp(&b_key)
    });
    out.dedup_by(|a, b| a.path == b.path && a.adapter == b.adapter);
    Ok(out)
}

fn looks_like_glob(path: &str) -> bool {
    ['*', '?', '[', ']', '{', '}']
        .iter()
        .any(|ch| path.contains(*ch))
}

fn glob_paths(pattern: &Path) -> Result<Vec<PathBuf>, CliError> {
    let pattern_str = pattern.to_string_lossy();
    let mut out = Vec::new();
    let entries = glob(&pattern_str)
        .map_err(|err| CliError::new("glob_error", format!("{} ({pattern_str})", err.msg)))?;
    for entry in entries {
        match entry {
            Ok(path) if path.is_file() => out.push(path),
            Ok(_) => {}
            Err(err) => {
                return Err(CliError::new("glob_error", err.to_string()));
            }
        }
    }
    Ok(out)
}

fn compile_excludes(
    cwd: &Path,
    home: &Path,
    patterns: &[String],
) -> Result<Vec<glob::Pattern>, CliError> {
    let mut compiled = Vec::new();
    for pattern in patterns {
        let raw = pattern.trim();
        if raw.is_empty() {
            continue;
        }
        let expanded = expand_tilde(raw, home);
        let normalized = if expanded.is_absolute() {
            expanded.to_string_lossy().to_string()
        } else {
            cwd.join(expanded).to_string_lossy().to_string()
        };
        let compiled_pattern = glob::Pattern::new(&normalized)
            .map_err(|err| CliError::new("exclude_glob_error", err.to_string()))?;
        compiled.push(compiled_pattern);
    }
    Ok(compiled)
}

fn is_excluded(path: &Path, excludes: &[glob::Pattern]) -> bool {
    excludes.iter().any(|pattern| pattern.matches_path(path))
}

fn detect_adapter_for_input(path: &Path, input: &str) -> Option<AdapterId> {
    let lower_path = path.to_string_lossy().to_ascii_lowercase();
    let preferred = if lower_path.contains(".codex/sessions") || lower_path.ends_with("history.jsonl")
    {
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

    if let Some(adapter) = preferred {
        if convert_with_adapter(adapter, input).is_ok() {
            return Some(adapter);
        }
    }

    for adapter in [
        AdapterId::CodexCli,
        AdapterId::ClaudeCode,
        AdapterId::OpenCode,
        AdapterId::Cursor,
        AdapterId::GeminiCli,
        AdapterId::OpenClaw,
    ] {
        if convert_with_adapter(adapter, input).is_ok() {
            return Some(adapter);
        }
    }
    None
}

fn load_ingest_state(paths: &RepoPaths) -> Result<IngestState, CliError> {
    fs::create_dir_all(&paths.cursors).map_err(|err| CliError::io("mkdir_error", err))?;
    let state_path = paths.cursors.join(CURSOR_STATE_FILE);
    if !state_path.exists() {
        return Ok(IngestState::default());
    }
    let content = fs::read_to_string(&state_path).map_err(|err| CliError::io("read_error", err))?;
    serde_json::from_str::<IngestState>(&content)
        .map_err(|err| CliError::new("cursor_state_error", err.to_string()))
}

fn save_ingest_state(paths: &RepoPaths, state: &IngestState) -> Result<(), CliError> {
    fs::create_dir_all(&paths.cursors).map_err(|err| CliError::io("mkdir_error", err))?;
    let state_path = paths.cursors.join(CURSOR_STATE_FILE);
    let content = serde_json::to_string_pretty(state)
        .map_err(|err| CliError::new("cursor_state_error", err.to_string()))?;
    fs::write(state_path, content).map_err(|err| CliError::io("write_error", err))
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

fn cmd_tapes(paths: &RepoPaths) -> Result<(), CliError> {
    require_initialized_paths(paths)?;
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

fn cmd_show(paths: &RepoPaths, args: ShowArgs) -> Result<(), CliError> {
    require_initialized_paths(paths)?;
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

fn cmd_gc(paths: &RepoPaths) -> Result<(), CliError> {
    require_initialized_paths(paths)?;
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

fn cmd_explain(cwd: &Path, paths: &RepoPaths, args: ExplainArgs) -> Result<(), CliError> {
    require_initialized_paths(paths)?;
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

fn repo_paths(cwd: &Path, global: bool) -> Result<RepoPaths, CliError> {
    let home = home_dir()?;
    let (root, cache_root, mode) = if global {
        (
            home.join(".engram"),
            home.join(".engram-cache"),
            StorageMode::Global,
        )
    } else {
        (
            cwd.join(".engram"),
            cwd.join(".engram-cache"),
            StorageMode::RepoLocal,
        )
    };

    Ok(RepoPaths {
        index: root.join("index.sqlite"),
        tapes: root.join("tapes"),
        objects: root.join("objects"),
        cursors: cache_root.join("cursors"),
        repo_config: cwd.join(".engram").join("config.yml"),
        user_config: home.join(".engram").join("config.yml"),
        root,
        cache_root,
        mode,
    })
}

fn require_initialized_paths(paths: &RepoPaths) -> Result<(), CliError> {
    if !paths.root.exists() || !paths.index.exists() || !paths.tapes.exists() {
        return Err(CliError::new(
            "not_initialized",
            "repository is not initialized; run `engram init`",
        ));
    }
    Ok(())
}

fn write_default_config(paths: &RepoPaths) -> Result<(), CliError> {
    let config_path = match paths.mode {
        StorageMode::RepoLocal => &paths.repo_config,
        StorageMode::Global => &paths.user_config,
    };
    if config_path.exists() {
        return Ok(());
    }
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|err| CliError::io("mkdir_error", err))?;
    }
    let default = match paths.mode {
        StorageMode::RepoLocal => default_repo_config_yaml(),
        StorageMode::Global => default_global_config_yaml(),
    };
    fs::write(config_path, default).map_err(|err| CliError::io("write_error", err))
}

fn home_dir() -> Result<PathBuf, CliError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| CliError::new("home_error", "HOME environment variable is not set"))
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
    sha256_hex(input)
}

fn sha256_hex(input: &str) -> String {
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
