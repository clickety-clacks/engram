use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::process::ExitCode;

use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use engram::anchor::fingerprint_text;
use engram::config::{ensure_user_config, load_effective_config};
use engram::index::lineage::{
    Cardinality, EvidenceFragmentRef, EvidenceKind, LINK_THRESHOLD_DEFAULT, LocationDelta,
    StoredEdgeClass,
};
use engram::index::{DispatchDirection, DispatchLink, DispatchLinkRow, EdgeRow, SqliteIndex};
use engram::query::explain::{
    ExplainTraversal, PrettyConfidenceTier, explain_by_anchor, pretty_tier,
};
use engram::store::atomic::atomic_write;
use engram::tape::adapter::{
    AdapterId, adapter_registry, convert_with_adapter, discover_sessions_with_adapter,
};
use engram::tape::compress::{compress_jsonl, decompress_jsonl};
use engram::tape::event::{TapeEventAt, TapeEventData, parse_jsonl_events};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

const TAPE_SUFFIX: &str = ".jsonl.zst";
const TRANSCRIPT_WINDOW_RADIUS: usize = 2;
const CURSOR_GUARD_WINDOW: usize = 512;

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
    Ingest(IngestArgs),
    Fingerprint,
    Record(RecordArgs),
    Explain(ExplainArgs),
    Tapes,
    Show(ShowArgs),
    Gc,
}

#[derive(Args, Debug, Default)]
struct IngestArgs {
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
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
    target: Option<String>,
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
    tapes: PathBuf,
    objects: PathBuf,
    cursors: PathBuf,
}

#[derive(Debug, Clone)]
struct RuntimeContext {
    config_path: PathBuf,
    db_path: PathBuf,
    tapes_dir: PathBuf,
    additional_stores: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IngestCursorGuard {
    offset: u64,
    len: u32,
    hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IngestFileState {
    byte_cursor: u64,
    cursor_guard: IngestCursorGuard,
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
    let paths = repo_paths(&cwd)?;
    match cli.command {
        Command::Init => cmd_init(&paths),
        Command::Ingest(args) => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_ingest(&cwd, &paths, &context, args)
        }
        Command::Fingerprint => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_fingerprint(&paths, &context)
        }
        Command::Record(args) => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_record(&cwd, &paths, &context, args)
        }
        Command::Explain(args) => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_explain(&cwd, &paths, &context, args)
        }
        Command::Tapes => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_tapes(&paths, &context)
        }
        Command::Show(args) => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_show(&paths, &context, args)
        }
        Command::Gc => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_gc(&paths, &context)
        }
    }
}

fn cmd_init(paths: &RepoPaths) -> Result<(), CliError> {
    let home = home_dir()?;
    ensure_user_config(&home).map_err(|err| CliError::new("config_error", err.to_string()))?;
    ensure_local_store(paths)?;
    let context = RuntimeContext {
        config_path: paths.root.join("config.yml"),
        db_path: paths.root.join("index.sqlite"),
        tapes_dir: paths.root.join("tapes"),
        additional_stores: Vec::new(),
    };
    print_context_conspicuity(&context);
    if context.config_path.exists() {
        return print_json(&json!({
            "status": "ok",
            "created": false,
            "message": "local workspace config already exists",
        }));
    }

    atomic_write(
        &context.config_path,
        b"db: .engram/index.sqlite\ntapes_dir: .engram/tapes\n",
    )
    .map_err(|err| CliError::io("write_error", err))?;
    print_json(&json!({
        "status": "ok",
        "created": true,
        "message": "created local workspace config at .engram/config.yml",
    }))
}

fn cmd_record(
    cwd: &Path,
    paths: &RepoPaths,
    context: &RuntimeContext,
    args: RecordArgs,
) -> Result<(), CliError> {
    if args.stdin && !args.command.is_empty() {
        return Err(CliError::new(
            "invalid_record_args",
            "use either `engram record --stdin` or `engram record <command>`",
        ));
    }

    ensure_local_store(paths)?;
    print_context_conspicuity(context);
    if args.stdin {
        let mut stdin_buf = String::new();
        io::stdin()
            .read_to_string(&mut stdin_buf)
            .map_err(|err| CliError::io("stdin_error", err))?;
        return record_transcript(
            paths,
            &context.db_path,
            &stdin_buf,
            json!({ "mode": "stdin" }),
            None,
        );
    }

    if args.command.is_empty() {
        return Err(CliError::new(
            "missing_record_command",
            "expected command args or --stdin",
        ));
    }

    let transcript = capture_command_tape(cwd, &args.command)?;
    record_transcript(
        paths,
        &context.db_path,
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

fn cmd_ingest(
    cwd: &Path,
    paths: &RepoPaths,
    context: &RuntimeContext,
    args: IngestArgs,
) -> Result<(), CliError> {
    ensure_local_store(paths)?;
    print_context_conspicuity(context);
    fs::create_dir_all(&context.tapes_dir).map_err(|err| CliError::io("mkdir_error", err))?;
    let (mut candidates, mut failures) = discover_ingest_candidates(cwd, &args.paths)?;
    let home = home_dir()?;
    if args.paths.is_empty() {
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
    let index = SqliteIndex::open(&path_string(&context.db_path))?;

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
            if metadata.len() < prev.byte_cursor {
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

        let adapter = if let Some(adapter) = adapter_hint {
            if convert_with_adapter(adapter, ingest_input).is_ok() {
                Some(adapter)
            } else {
                None
            }
        } else {
            None
        };
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

fn discover_local_transcript_candidates(cwd: &Path) -> Result<Vec<PathBuf>, CliError> {
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

fn detect_adapter_for_input(path: &Path, input: &str) -> Option<AdapterId> {
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

fn cmd_fingerprint(paths: &RepoPaths, context: &RuntimeContext) -> Result<(), CliError> {
    ensure_local_store(paths)?;
    print_context_conspicuity(context);
    ensure_db_parent(&context.db_path)?;
    let index = SqliteIndex::open(&path_string(&context.db_path))?;

    let mut scanned = 0usize;
    let mut fingerprinted = 0usize;
    let mut skipped_existing = 0usize;
    let mut failures = Vec::new();

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
        scanned += 1;
        if index.has_tape(&tape_id)? {
            skipped_existing += 1;
            continue;
        }

        let content = match read_tape_content(&path) {
            Ok(value) => value,
            Err(err) => {
                failures.push(json!({
                    "path": path,
                    "error": err.message,
                }));
                continue;
            }
        };
        let events = match parse_jsonl_events(&content) {
            Ok(value) => value,
            Err(err) => {
                failures.push(json!({
                    "path": path,
                    "error": err.to_string(),
                }));
                continue;
            }
        };
        let dispatch_links = extract_dispatch_links_from_transcript(&content);
        index.ingest_tape_events_with_dispatch(
            &tape_id,
            &events,
            &dispatch_links,
            LINK_THRESHOLD_DEFAULT,
        )?;
        fingerprinted += 1;
    }

    print_json(&json!({
        "status": if failures.is_empty() { "ok" } else { "partial" },
        "scanned_tapes": scanned,
        "fingerprinted_tapes": fingerprinted,
        "skipped_existing_tapes": skipped_existing,
        "failure_count": failures.len(),
        "failures": failures,
    }))
}

fn discover_ingest_candidates(
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
                "error": "path is outside local ingest scope",
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

fn adapter_id_from_name(raw: &str) -> Option<AdapterId> {
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

fn cursor_state_path(paths: &RepoPaths, abs_path: &Path) -> PathBuf {
    let key = sha256_hex(&path_string(abs_path));
    paths.cursors.join(format!("{key}.json"))
}

fn load_ingest_state_for_path(
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

fn save_ingest_state_for_path(
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

fn build_cursor_guard(path: &Path, byte_cursor: u64) -> Result<IngestCursorGuard, CliError> {
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

fn ingest_cursor_guard_matches(
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

fn complete_ingest_prefix_len(path: &Path, bytes: &[u8]) -> usize {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());
    if matches!(extension.as_deref(), Some("json")) {
        return bytes.len();
    }
    complete_jsonl_prefix_len(bytes)
}

fn complete_jsonl_prefix_len(bytes: &[u8]) -> usize {
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

fn record_transcript(
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
    let index = SqliteIndex::open(&path_string(db_path))?;
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

fn cmd_tapes(paths: &RepoPaths, context: &RuntimeContext) -> Result<(), CliError> {
    ensure_local_store(paths)?;
    print_context_conspicuity(context);
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

fn cmd_show(paths: &RepoPaths, context: &RuntimeContext, args: ShowArgs) -> Result<(), CliError> {
    ensure_local_store(paths)?;
    print_context_conspicuity(context);
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

fn cmd_gc(paths: &RepoPaths, context: &RuntimeContext) -> Result<(), CliError> {
    ensure_local_store(paths)?;
    print_context_conspicuity(context);
    ensure_db_parent(&context.db_path)?;
    let index = SqliteIndex::open(&path_string(&context.db_path))?;
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

fn cmd_explain(
    cwd: &Path,
    paths: &RepoPaths,
    context: &RuntimeContext,
    args: ExplainArgs,
) -> Result<(), CliError> {
    ensure_local_store(paths)?;
    print_context_conspicuity(context);
    ensure_db_parent(&context.db_path)?;
    let mut indexes = Vec::new();
    indexes.push(SqliteIndex::open(&path_string(&context.db_path))?);
    for store in &context.additional_stores {
        if store.exists() {
            indexes.push(SqliteIndex::open(&path_string(store))?);
        }
    }
    let anchors = resolve_explain_anchors(cwd, &args)?;
    let target = args
        .target
        .clone()
        .ok_or_else(|| CliError::new("invalid_explain_target", "target is required"))?;
    let traversal = ExplainTraversal {
        min_confidence: args.min_confidence,
        max_fanout: args.max_fanout,
        max_edges: args.max_edges,
        max_depth: args.depth,
    };
    let result = explain_across_indexes(&indexes, &anchors, traversal, args.forensics)?;

    let touches = collect_touch_evidence(&indexes, &result.direct, &result.touched_anchors)?;
    let mut sessions = build_session_windows(&paths, touches)?;
    let (dispatch_lineage, dispatch_sessions) =
        collect_dispatch_upstream_sessions(paths, &indexes[0], &sessions)?;
    sessions.extend(dispatch_sessions);

    let mut tombstones = Vec::new();
    if args.include_deleted {
        for anchor in &result.touched_anchors {
            for index in &indexes {
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
    }

    if args.pretty {
        print_pretty_explain(&target, &result.lineage, &sessions, &tombstones);
        return Ok(());
    }

    let lineage = result.lineage.iter().map(edge_to_json).collect::<Vec<_>>();

    print_json(&json!({
        "query": {
            "target": target,
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
        "dispatch_lineage": dispatch_lineage,
        "tombstones": tombstones,
        "stores_queried": indexes.len(),
    }))
}

fn collect_dispatch_upstream_sessions(
    paths: &RepoPaths,
    index: &SqliteIndex,
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
                message_turn_before_offset(paths, &mut rows_cache, tape_id, edit_offset)?;

            let mut current_tape = tape_id.to_string();
            let mut current_turn = edit_turn;
            let mut visited = HashSet::new();
            while let Some(received) =
                index.latest_received_dispatch_before_turn(&current_tape, current_turn)?
            {
                let Some(parent) = index.sent_dispatch_for_uuid(&received.uuid)? else {
                    break;
                };
                let hop_key = format!(
                    "{}:{}:{}:{}",
                    current_tape, current_turn, received.uuid, parent.tape_id
                );
                if !visited.insert(hop_key) {
                    break;
                }

                chain.push(json!({
                    "session": current_tape,
                    "edit_turn_index": current_turn,
                    "received_uuid": received.uuid,
                    "received_turn_index": received.first_turn_index,
                    "parent_session": parent.tape_id,
                    "parent_sent_turn_index": parent.first_turn_index,
                }));

                if seen_tapes.insert(parent.tape_id.clone())
                    && let Some(extra) = build_dispatch_session(paths, &mut rows_cache, &parent)?
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

fn build_dispatch_session(
    paths: &RepoPaths,
    rows_cache: &mut HashMap<String, Vec<TapeRow>>,
    link: &DispatchLinkRow,
) -> Result<Option<Value>, CliError> {
    let rows = load_tape_rows_cached(paths, rows_cache, &link.tape_id)?;
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

fn collect_touch_evidence(
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

fn explain_across_indexes(
    indexes: &[SqliteIndex],
    anchors: &[String],
    traversal: ExplainTraversal,
    include_forensics: bool,
) -> Result<engram::query::explain::ExplainResult, CliError> {
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

    for index in indexes {
        let result = explain_by_anchor(index, anchors, traversal, include_forensics)?;
        for fragment in result.direct {
            let key = touch_key(&fragment);
            if seen_direct.insert(key) {
                direct.push(fragment);
            }
        }
        for edge in result.lineage {
            let key = format!(
                "{}:{}:{:.6}:{}:{}:{}:{}",
                edge.from_anchor,
                edge.to_anchor,
                edge.confidence,
                location_delta_name(edge.location_delta),
                cardinality_name(edge.cardinality),
                edge.agent_link,
                edge.note.clone().unwrap_or_default()
            );
            if seen_lineage.insert(key) {
                lineage.push(edge);
            }
        }
        for anchor in result.touched_anchors {
            if seen_anchors.insert(anchor.clone()) {
                touched_anchors.push(anchor);
            }
        }
    }

    Ok(engram::query::explain::ExplainResult {
        direct,
        lineage,
        touched_anchors,
    })
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
        let windows = if tape_path.exists() {
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
            "tape_present_locally": tape_path.exists(),
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
        let target = args.target.clone().ok_or_else(|| {
            CliError::new(
                "invalid_explain_target",
                "target is required for --anchor mode",
            )
        })?;
        return Ok(vec![target]);
    }

    let target = args
        .target
        .as_deref()
        .ok_or_else(|| CliError::new("invalid_explain_target", "target is required"))?;
    let (file, start, end) = parse_file_range_target(target)?;
    let file_path = cwd.join(file);
    let span_text = read_file_span(&file_path, start, end)?;
    Ok(derive_anchor_candidates(&span_text))
}

fn derive_anchor_candidates(span_text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    let fingerprint = fingerprint_text(span_text).fingerprint;
    if seen.insert(fingerprint.clone()) {
        out.push(fingerprint);
    }

    let mut hasher = Sha256::new();
    hasher.update(span_text.as_bytes());
    let digest = hasher.finalize();
    let mut sha = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut sha, "{byte:02x}");
    }
    if seen.insert(sha.clone()) {
        out.push(sha);
    }

    out
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

fn load_tape_rows_cached<'a>(
    paths: &RepoPaths,
    cache: &'a mut HashMap<String, Vec<TapeRow>>,
    tape_id: &str,
) -> Result<&'a Vec<TapeRow>, CliError> {
    if !cache.contains_key(tape_id) {
        let tape_path = tape_path_for_id(paths, tape_id);
        if !tape_path.exists() {
            cache.insert(tape_id.to_string(), Vec::new());
        } else {
            let content = read_tape_content(&tape_path)?;
            cache.insert(tape_id.to_string(), parse_jsonl_rows(&content)?);
        }
    }
    Ok(cache.get(tape_id).expect("cache entry inserted"))
}

fn message_turn_before_offset(
    paths: &RepoPaths,
    cache: &mut HashMap<String, Vec<TapeRow>>,
    tape_id: &str,
    event_offset: u64,
) -> Result<i64, CliError> {
    let rows = load_tape_rows_cached(paths, cache, tape_id)?;
    let turn = rows
        .iter()
        .filter(|row| row.offset < event_offset && is_message_row(&row.value))
        .count() as i64;
    Ok(turn)
}

fn message_turn_to_event_offset(rows: &[TapeRow], turn_index: i64) -> Option<u64> {
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

fn is_message_row(value: &Value) -> bool {
    matches!(
        value.get("k").and_then(Value::as_str),
        Some("msg.in" | "msg.out")
    )
}

fn dispatch_direction_name(direction: DispatchDirection) -> &'static str {
    match direction {
        DispatchDirection::Received => "received",
        DispatchDirection::Sent => "sent",
    }
}

fn extract_dispatch_links_from_transcript(transcript: &str) -> Vec<DispatchLink> {
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

fn extract_message_objects<'a>(row: &'a Value) -> Vec<&'a Value> {
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

fn extract_dispatch_direction_by_uuid(message: &Value) -> HashMap<String, DispatchDirection> {
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

fn collect_dispatch_uuids_on_message_surface(message: &Value, out: &mut HashSet<String>) {
    if let Some(content) = message.get("content") {
        collect_dispatch_uuids_from_surface_content(content, out);
    }
    if let Some(text) = message.get("text").and_then(Value::as_str) {
        for uuid in extract_dispatch_uuids_from_text(text) {
            out.insert(uuid);
        }
    }
}

fn collect_dispatch_uuids_from_surface_content(content: &Value, out: &mut HashSet<String>) {
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

fn collect_dispatch_uuids_anywhere(value: &Value, out: &mut HashSet<String>) {
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

fn extract_dispatch_uuids_from_text(text: &str) -> Vec<String> {
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

fn is_uuid_format(raw: &str) -> bool {
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

fn repo_paths(cwd: &Path) -> Result<RepoPaths, CliError> {
    let root = cwd.join(".engram");
    Ok(RepoPaths {
        tapes: root.join("tapes"),
        objects: root.join("objects"),
        cursors: root.join("cursors"),
        root,
    })
}

fn resolve_runtime_context(cwd: &Path) -> Result<RuntimeContext, CliError> {
    let home = home_dir()?;
    let config = load_effective_config(cwd, &home)
        .map_err(|err| CliError::new("config_error", err.to_string()))?;
    Ok(RuntimeContext {
        config_path: config.path,
        db_path: config.db,
        tapes_dir: config.tapes_dir,
        additional_stores: config.additional_stores,
    })
}

fn ensure_local_store(paths: &RepoPaths) -> Result<(), CliError> {
    fs::create_dir_all(&paths.root).map_err(|err| CliError::io("mkdir_error", err))?;
    fs::create_dir_all(&paths.tapes).map_err(|err| CliError::io("mkdir_error", err))?;
    fs::create_dir_all(&paths.objects).map_err(|err| CliError::io("mkdir_error", err))?;
    fs::create_dir_all(&paths.cursors).map_err(|err| CliError::io("mkdir_error", err))?;
    Ok(())
}

fn ensure_db_parent(db_path: &Path) -> Result<(), CliError> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).map_err(|err| CliError::io("mkdir_error", err))?;
    }
    Ok(())
}

fn print_context_conspicuity(context: &RuntimeContext) {
    eprintln!("config: {}", context.config_path.display());
    eprintln!("db: {}", context.db_path.display());
}

fn home_dir() -> Result<PathBuf, CliError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| CliError::new("home_error", "HOME environment variable is not set"))
}

fn tape_path_for_id(paths: &RepoPaths, tape_id: &str) -> PathBuf {
    paths.tapes.join(format!("{tape_id}{TAPE_SUFFIX}"))
}

fn tape_path_for_tapes_dir(tapes_dir: &Path, tape_id: &str) -> PathBuf {
    tapes_dir.join(format!("{tape_id}{TAPE_SUFFIX}"))
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
    sha256_hex_bytes(input.as_bytes())
}

fn sha256_hex_bytes(input: &[u8]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_extraction_handles_same_uuid_in_surface_and_nested_locations() {
        let uuid = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
        let transcript = format!(
            concat!(
                "{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"<engram-src id=\\\"{0}\\\"/> do task\"}}]}}}}\n",
                "{{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"id\":\"call_1\",\"name\":\"exec\",\"arguments\":{{\"cmd\":\"echo <engram-src id=\\\"{0}\\\"/>\"}}}}]}}}}\n"
            ),
            uuid
        );

        let links = extract_dispatch_links_from_transcript(&transcript);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].uuid, uuid);
        assert_eq!(links[0].first_turn_index, 0);
        assert_eq!(links[0].direction, DispatchDirection::Received);
    }

    #[test]
    fn dispatch_extraction_classifies_nested_uuid_as_sent() {
        let uuid = "18d3ce5f-50f5-4c4e-94b7-c58f91dbf6be";
        let transcript = format!(
            "{{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"id\":\"call_1\",\"name\":\"exec\",\"arguments\":{{\"cmd\":\"tmux send-keys \\\"<engram-src id=\\\\\\\"{uuid}\\\\\\\"/>\\\"\"}}}}]}}}}"
        );
        let links = extract_dispatch_links_from_transcript(&transcript);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].uuid, uuid);
        assert_eq!(links[0].direction, DispatchDirection::Sent);
    }
}
