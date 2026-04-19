use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use engram::anchor::fingerprint_token_hashes;
use engram::config::{
    EffectiveWatchConfig, EffectiveWatchSource, ensure_user_config,
    load_effective_config_with_override,
};
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
use notify::event::{ModifyKind, RenameMode};
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

const MAX_QUERY_WINDOW_ANCHORS: usize = 16;
const DEFAULT_WINDOW_BEFORE_RATIO_NUM: usize = 3;
const DEFAULT_WINDOW_BEFORE_RATIO_DEN: usize = 4;
const SAFE_RESULT_SESSION_THRESHOLD: usize = 25;

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
    Watch(WatchArgs),
    Fingerprint,
    Record(RecordArgs),
    Explain(ExplainArgs),
    Grep(GrepArgs),
    Peek(PeekArgs),
    Rate(RateArgs),
    Tapes,
    Show(ShowArgs),
    Gc,
}

#[derive(Args, Debug, Default)]
struct IngestArgs {
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
}

#[derive(Args, Debug, Default)]
struct WatchArgs {
    #[arg(long)]
    config: Option<PathBuf>,
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
    #[arg(long, hide = true)]
    anchor: bool,
    #[arg(long)]
    grep_filter: Option<String>,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long, default_value_t = 0.5)]
    min_confidence: f32,
    #[arg(long, default_value_t = 0)]
    offset: usize,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    count: bool,
    #[arg(long, default_value_t = 50, hide = true)]
    max_fanout: usize,
    #[arg(long, default_value_t = 500, hide = true)]
    max_edges: usize,
    #[arg(long, default_value_t = 10, hide = true)]
    depth: usize,
    #[arg(long, hide = true)]
    include_deleted: bool,
    #[arg(long, hide = true)]
    forensics: bool,
    #[arg(long, hide = true)]
    pretty: bool,
}

#[derive(Args, Debug)]
struct GrepArgs {
    pattern: String,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long, default_value_t = 0)]
    offset: usize,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    count: bool,
}

#[derive(Args, Debug)]
struct PeekArgs {
    session_id: String,
    #[arg(long)]
    start: Option<usize>,
    #[arg(long)]
    lines: Option<usize>,
    #[arg(long)]
    before: Option<usize>,
    #[arg(long)]
    after: Option<usize>,
    #[arg(long)]
    grep_filter: Option<String>,
}

#[derive(Args, Debug)]
struct RateArgs {
    result_id: String,
    #[arg(long)]
    outcome: RateOutcome,
    #[arg(long)]
    note: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
enum RateOutcome {
    FoundAnswer,
    PartiallyHelped,
    Noise,
    Misleading,
    NotUsed,
}

impl RateOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::FoundAnswer => "found_answer",
            Self::PartiallyHelped => "partially_helped",
            Self::Noise => "noise",
            Self::Misleading => "misleading",
            Self::NotUsed => "not_used",
        }
    }
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
    tape_lookup_dirs: Vec<PathBuf>,
    additional_stores: Vec<PathBuf>,
    explain_default_limit: usize,
    peek_default_lines: usize,
    peek_default_before: usize,
    peek_default_after: usize,
    peek_grep_context: usize,
    metrics_enabled: bool,
    metrics_log: PathBuf,
    watch: Option<EffectiveWatchConfig>,
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
            let payload = error_payload(&err);
            eprintln!("{payload}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), CliError> {
    if maybe_print_spec_help()? {
        return Ok(());
    }
    let cli = Cli::parse();
    let cwd = std::env::current_dir().map_err(|err| CliError::io("cwd_error", err))?;
    let paths = repo_paths(&cwd)?;
    match cli.command {
        Command::Init => cmd_init(&paths),
        Command::Ingest(args) => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_ingest(&cwd, &paths, &context, args)
        }
        Command::Watch(args) => cmd_watch(&cwd, args),
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
        Command::Grep(args) => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_grep(&paths, &context, args)
        }
        Command::Peek(args) => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_peek(&paths, &context, args)
        }
        Command::Rate(args) => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_rate(&paths, &context, args)
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

fn error_payload(err: &CliError) -> Value {
    match err.code {
        "session_not_found" => json!({
            "error": "session_not_found",
            "session_id": err.message,
        }),
        "no_results" => json!({
            "error": "no_results",
            "query": err.message,
        }),
        "invalid_span" => json!({
            "error": "invalid_span",
            "detail": err.message,
        }),
        _ => json!({
            "error": {
                "code": err.code,
                "message": err.message,
            }
        }),
    }
}

const HELP_ENGRAM: &str = r#"Engram indexes agent conversations that produced your code.

Results are organized as provenance chains: the root is WHY
(product decisions, design rationale), descendants are HOW
(specs, implementation). Use explain to find chains, peek to
read them.

COMMANDS:
  explain    Find provenance for code (by fingerprint)
  grep       Find provenance for a term (by text search)
  peek       Read content from a provenance session
  rate       Record whether a returned result was useful
  ingest     Import transcripts into the index
  watch      Continuously watch for new transcripts

Run engram <command> --help for details.
"#;

const HELP_EXPLAIN: &str = r#"Find the conversations that produced this code.

Returns the root of each provenance chain — the highest-level
context explaining WHY this code exists. Results include chain
metadata (children, depth) so you can walk down to HOW with peek.
Returns metadata only. Use peek <session_id> to read content.

USAGE:
  engram explain <file>:<start>-<end>   Provenance for a code span
  engram explain <file>                 Provenance for an entire file  
  engram explain "<string>"             Provenance for arbitrary text

OPTIONS:
  --grep-filter <pattern>   Only results whose content matches (grep syntax)
  --limit N                 Max results [default: 10]
  --offset N                Skip first N results (pagination)
  --min-confidence N        Only results above this match quality (0.0-1.0)
  --since <date>            Only sessions after this date
  --until <date>            Only sessions before this date
  --count                   Show counts only, no content (token budgeting)

EXAMPLES:
  engram explain src/server.ts:40-78
  engram explain src/server.ts:40-78 --grep-filter "retry"
  engram explain src/server.ts --since 2026-03-01 --limit 5
"#;

const HELP_GREP: &str = r#"Search all provenance sessions for a term.

Unlike explain (which matches by code fingerprint), grep searches
for literal text across all indexed conversations.

USAGE:
  engram grep <pattern>

OPTIONS:
  --limit N       Max results [default: 10]
  --offset N      Skip first N results
  --since <date>  Only sessions after this date
  --until <date>  Only sessions before this date
  --count         Show counts only, no content

EXAMPLES:
  engram grep "maxMessageBytes"
  engram grep "retry logic" --since 2026-03-01
"#;

const HELP_PEEK: &str = r#"Read content from a provenance session.

Use explain or grep to find sessions, then peek to read them.
By default returns a window around the anchor point (where the
session connects to its parent chain). Use --start/--lines for
absolute positioning.

USAGE:
  engram peek <session_id>

OPTIONS:
  --start N                 Read from this line number
  --lines N                 Number of lines to return [default: 30]
  --before N                Lines before the anchor point [default: 30]
  --after N                 Lines after the anchor point [default: 10]
  --grep-filter <pattern>   Find lines matching this term within the session

EXAMPLES:
  engram peek af156abd
  engram peek af156abd --start 421 --lines 30
  engram peek af156abd --grep-filter "NO_REPLY"
"#;

const HELP_RATE: &str = r#"Record usefulness feedback for a prior query result.

USAGE:
  engram rate <result_id> --outcome <class> [--note "..."]

OUTCOMES:
  found_answer
  partially_helped
  noise
  misleading
  not_used

EXAMPLES:
  engram rate result_abc123 --outcome found_answer
  engram rate result_abc123 --outcome misleading --note "sent me to the wrong session"
"#;

fn maybe_print_spec_help() -> Result<bool, CliError> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let help_flag = |value: &str| value == "--help" || value == "-h";

    if args.len() == 1 && help_flag(&args[0]) {
        print!("{HELP_ENGRAM}");
        return Ok(true);
    }

    if args.len() == 2 && help_flag(&args[1]) {
        match args[0].as_str() {
            "explain" => {
                print!("{HELP_EXPLAIN}");
                return Ok(true);
            }
            "grep" => {
                print!("{HELP_GREP}");
                return Ok(true);
            }
            "peek" => {
                print!("{HELP_PEEK}");
                return Ok(true);
            }
            "rate" => {
                print!("{HELP_RATE}");
                return Ok(true);
            }
            _ => {}
        }
    }

    Ok(false)
}

fn cmd_init(paths: &RepoPaths) -> Result<(), CliError> {
    let home = home_dir()?;
    ensure_user_config(&home).map_err(|err| CliError::new("config_error", err.to_string()))?;
    ensure_local_store(paths)?;
    let local_tapes_dir = paths.root.join("tapes");
    let context = RuntimeContext {
        config_path: paths.root.join("config.yml"),
        db_path: paths.root.join("index.sqlite"),
        tapes_dir: local_tapes_dir.clone(),
        tape_lookup_dirs: vec![local_tapes_dir, home.join(".engram").join("tapes")],
        additional_stores: Vec::new(),
        explain_default_limit: 10,
        peek_default_lines: 40,
        peek_default_before: 30,
        peek_default_after: 10,
        peek_grep_context: 5,
        metrics_enabled: true,
        metrics_log: home.join(".engram").join("metrics.jsonl"),
        watch: None,
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

fn cmd_rate(paths: &RepoPaths, context: &RuntimeContext, args: RateArgs) -> Result<(), CliError> {
    ensure_local_store(paths)?;
    print_context_conspicuity(context);
    ensure_db_parent(&context.db_path)?;

    let index = SqliteIndex::open(&path_string(&context.db_path))?;
    if !index.query_result_exists(&args.result_id)? {
        return Err(CliError::new("unknown_result_id", args.result_id));
    }

    let rated_at = Utc::now().to_rfc3339();
    index.upsert_result_feedback(
        &args.result_id,
        args.outcome.as_str(),
        args.note.as_deref(),
        &rated_at,
    )?;

    print_json(&json!({
        "status": "ok",
        "result_id": args.result_id,
        "outcome": args.outcome.as_str(),
        "note": args.note,
        "rated_at": rated_at,
        "storage": "local_index",
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

#[derive(Debug, Clone)]
struct WatchSourceRuntime {
    source: EffectiveWatchSource,
    match_root: PathBuf,
    pattern: glob::Pattern,
    glob: Option<glob::Pattern>,
    debounce: Duration,
    ingest_timeout: Duration,
}

enum WatchIngestResult {
    Completed(Result<(), CliError>),
    TimedOut,
}

fn cmd_watch(cwd: &Path, args: WatchArgs) -> Result<(), CliError> {
    let home = home_dir()?;
    cmd_watch_with_home(cwd, args, &home)
}

fn cmd_watch_with_home(cwd: &Path, args: WatchArgs, home: &Path) -> Result<(), CliError> {
    let config_override = args.config.as_ref().map(|path| {
        if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        }
    });
    let config = load_effective_config_with_override(cwd, home, config_override.as_deref())
        .map_err(|err| CliError::new("config_error", err.to_string()))?;
    let tape_lookup_dirs = tape_lookup_dirs(cwd, home, &config);
    let context = RuntimeContext {
        config_path: config.path,
        db_path: config.db,
        tapes_dir: config.tapes_dir,
        tape_lookup_dirs,
        additional_stores: config.additional_stores,
        explain_default_limit: config.explain_default_limit,
        peek_default_lines: config.peek.default_lines,
        peek_default_before: config.peek.default_before,
        peek_default_after: config.peek.default_after,
        peek_grep_context: config.peek.grep_context,
        metrics_enabled: config.metrics.enabled,
        metrics_log: config.metrics.log,
        watch: config.watch,
    };
    print_context_conspicuity(&context);

    let watch_config = context
        .watch
        .clone()
        .ok_or_else(|| CliError::new("watch_config_error", "watch config missing in config.yml"))?;
    if watch_config.sources.is_empty() {
        return Err(CliError::new(
            "watch_config_error",
            "watch.sources must contain at least one source",
        ));
    }

    if let Some(parent) = watch_config.log.parent() {
        fs::create_dir_all(parent).map_err(|err| CliError::io("mkdir_error", err))?;
    }
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&watch_config.log)
        .map_err(|err| CliError::io("write_error", err))?;

    watch_log_line(
        &mut log,
        &format!("watch started sources={}", watch_config.sources.len()),
    )?;

    let mut runtimes = Vec::new();
    for source in watch_config.sources {
        let pattern = glob::Pattern::new(&source.pattern)
            .map_err(|err| CliError::new("watch_config_error", err.to_string()))?;
        let glob = source
            .glob
            .as_deref()
            .map(glob::Pattern::new)
            .transpose()
            .map_err(|err| CliError::new("watch_config_error", err.to_string()))?;
        if !source.path.is_dir() {
            watch_log_line(
                &mut log,
                &format!("watch source skipped missing_dir={}", source.path.display()),
            )?;
            continue;
        }
        if let Some(glob) = source.glob.as_deref() {
            watch_log_line(
                &mut log,
                &format!(
                    "watch source path={} pattern={} glob={} debounce={} timeout={}",
                    source.path.display(),
                    source.pattern,
                    glob,
                    watch_config.debounce_secs,
                    watch_config.ingest_timeout_secs
                ),
            )?;
        } else {
            watch_log_line(
                &mut log,
                &format!(
                    "watch source path={} pattern={} debounce={} timeout={}",
                    source.path.display(),
                    source.pattern,
                    watch_config.debounce_secs,
                    watch_config.ingest_timeout_secs
                ),
            )?;
        }
        let match_root = fs::canonicalize(&source.path).map_err(|err| {
            CliError::new(
                "watch_config_error",
                format!(
                    "failed to canonicalize watch source {}: {err}",
                    source.path.display()
                ),
            )
        })?;
        runtimes.push(WatchSourceRuntime {
            source,
            match_root,
            pattern,
            glob,
            debounce: Duration::from_secs(watch_config.debounce_secs),
            ingest_timeout: Duration::from_secs(watch_config.ingest_timeout_secs),
        });
    }
    if runtimes.is_empty() {
        return Err(CliError::new(
            "watch_config_error",
            "no watch sources available",
        ));
    }

    let (tx, rx) = mpsc::channel::<Result<Event, notify::Error>>();
    let mut watcher = RecommendedWatcher::new(
        move |result| {
            let _ = tx.send(result);
        },
        NotifyConfig::default(),
    )
    .map_err(|err| CliError::new("watch_error", err.to_string()))?;
    for runtime in &runtimes {
        watcher
            .watch(&runtime.source.path, RecursiveMode::Recursive)
            .map_err(|err| CliError::new("watch_error", err.to_string()))?;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stop_signal = stop.clone();
    ctrlc::set_handler(move || {
        stop_signal.store(true, Ordering::SeqCst);
    })
    .map_err(|err| CliError::new("watch_error", err.to_string()))?;

    let mut last_ingest = HashMap::<(usize, PathBuf), Instant>::new();
    while !stop.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Ok(event)) => {
                if !watch_event_kind_supported(&event.kind) {
                    continue;
                }
                for path in event.paths {
                    for (idx, runtime) in runtimes.iter().enumerate() {
                        if !watch_path_matches(runtime, &path) {
                            continue;
                        }
                        let key = (idx, path.clone());
                        if let Some(last) = last_ingest.get(&key)
                            && last.elapsed() < runtime.debounce
                        {
                            continue;
                        }
                        watch_log_line(&mut log, &format!("event path={}", path.display()))?;
                        std::thread::sleep(runtime.debounce);
                        match run_watch_ingest(runtime, &path, &context) {
                            WatchIngestResult::TimedOut => {
                                watch_log_line(
                                    &mut log,
                                    &format!("ingest timeout path={}", path.display()),
                                )?;
                            }
                            WatchIngestResult::Completed(Ok(())) => {
                                watch_log_line(
                                    &mut log,
                                    &format!("ingest ok path={}", path.display()),
                                )?;
                            }
                            WatchIngestResult::Completed(Err(err)) => {
                                watch_log_line(
                                    &mut log,
                                    &format!(
                                        "ingest failed path={} code={} message={}",
                                        path.display(),
                                        err.code,
                                        err.message
                                    ),
                                )?;
                            }
                        }
                        last_ingest.insert(key, Instant::now());
                    }
                }
            }
            Ok(Err(err)) => {
                watch_log_line(&mut log, &format!("watch error: {err}"))?;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    watch_log_line(&mut log, "watch stopped")?;
    log.flush()
        .map_err(|err| CliError::io("write_error", err))?;
    Ok(())
}

fn watch_event_kind_supported(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) => true,
        EventKind::Modify(ModifyKind::Name(mode)) => matches!(
            mode,
            RenameMode::Any | RenameMode::Both | RenameMode::To | RenameMode::From
        ),
        EventKind::Modify(_) => true,
        _ => false,
    }
}

fn watch_path_matches(runtime: &WatchSourceRuntime, path: &Path) -> bool {
    let Some(relative_path) = watch_path_relative_to_source(runtime, path) else {
        return false;
    };
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    if !runtime.pattern.matches(name) {
        return false;
    }
    let Some(glob) = runtime.glob.as_ref() else {
        return true;
    };
    glob.matches_path_with(&relative_path, watch_glob_match_options())
}

fn watch_glob_match_options() -> glob::MatchOptions {
    glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    }
}

fn watch_path_relative_to_source(runtime: &WatchSourceRuntime, path: &Path) -> Option<PathBuf> {
    if let Ok(relative_path) = path.strip_prefix(&runtime.source.path) {
        return Some(relative_path.to_path_buf());
    }
    if let Ok(relative_path) = path.strip_prefix(&runtime.match_root) {
        return Some(relative_path.to_path_buf());
    }
    if let Ok(canonical_path) = fs::canonicalize(path)
        && let Ok(relative_path) = canonical_path.strip_prefix(&runtime.match_root)
    {
        return Some(relative_path.to_path_buf());
    }
    None
}

fn run_watch_ingest(
    runtime: &WatchSourceRuntime,
    changed_path: &Path,
    context: &RuntimeContext,
) -> WatchIngestResult {
    let source_cwd = runtime.source.path.clone();
    let changed = changed_path.to_path_buf();
    let context = context.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = repo_paths(&source_cwd).and_then(|paths| {
            cmd_ingest(
                &source_cwd,
                &paths,
                &context,
                IngestArgs {
                    paths: vec![changed],
                },
            )
        });
        let _ = tx.send(result);
    });

    match rx.recv_timeout(runtime.ingest_timeout) {
        Ok(result) => WatchIngestResult::Completed(result),
        Err(mpsc::RecvTimeoutError::Timeout) => WatchIngestResult::TimedOut,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            WatchIngestResult::Completed(Err(CliError::new("watch_error", "ingest thread ended")))
        }
    }
}

fn watch_log_line(log: &mut File, message: &str) -> Result<(), CliError> {
    writeln!(log, "[{}] {}", now_iso8601(), message).map_err(|err| CliError::io("write_error", err))
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
    let Some(tape_path) = resolve_tape_path(context, &args.tape_id) else {
        return Err(CliError::new(
            "tape_not_found",
            format!("tape `{}` not found", args.tape_id),
        ));
    };

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

    let indexes = open_query_indexes(context)?;
    let target = args
        .target
        .clone()
        .ok_or_else(|| CliError::new("invalid_explain_target", "target is required"))?;
    let target_kind = classify_explain_target(cwd, context, &indexes, &target, args.anchor)?;

    let mut query_anchors = Vec::new();
    let mut raw_sessions: Vec<Value>;
    let mut dispatch_lineage = Vec::new();
    let mut lineage = Vec::new();
    let mut tombstones = Vec::new();
    let mut score_by_session = HashMap::new();
    let date_filter = DateFilter::parse(args.since.as_deref(), args.until.as_deref())?;

    match target_kind {
        ExplainTarget::FileRange { file, start, end } => {
            let span_texts = read_file_span_variants(&cwd.join(file), start, end)?;
            query_anchors = derive_anchor_candidates(&span_texts);
            let traversal = ExplainTraversal {
                min_confidence: args.min_confidence,
                max_fanout: args.max_fanout,
                max_edges: args.max_edges,
                max_depth: args.depth,
            };
            let result =
                explain_across_indexes(&indexes, &query_anchors, traversal, args.forensics)?;
            let touches =
                collect_touch_evidence(&indexes, &result.direct, &result.touched_anchors)?;
            raw_sessions = build_session_windows(context, touches)?;
            let (chain, dispatch_sessions) =
                collect_dispatch_upstream_sessions(context, &indexes[0], &raw_sessions)?;
            dispatch_lineage = chain;
            raw_sessions.extend(dispatch_sessions);
            lineage = result.lineage.iter().map(edge_to_json).collect::<Vec<_>>();
            score_by_session = collect_anchor_scores(&indexes, &query_anchors)?;

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
        }
        ExplainTarget::FileWhole { file } => {
            let full_text = fs::read_to_string(cwd.join(file))
                .map_err(|err| CliError::io("read_span_error", err))?;
            query_anchors = derive_anchor_candidates(&[full_text]);
            let traversal = ExplainTraversal {
                min_confidence: args.min_confidence,
                max_fanout: args.max_fanout,
                max_edges: args.max_edges,
                max_depth: args.depth,
            };
            let result =
                explain_across_indexes(&indexes, &query_anchors, traversal, args.forensics)?;
            let touches =
                collect_touch_evidence(&indexes, &result.direct, &result.touched_anchors)?;
            raw_sessions = build_session_windows(context, touches)?;
            let (chain, dispatch_sessions) =
                collect_dispatch_upstream_sessions(context, &indexes[0], &raw_sessions)?;
            dispatch_lineage = chain;
            raw_sessions.extend(dispatch_sessions);
            lineage = result.lineage.iter().map(edge_to_json).collect::<Vec<_>>();
            score_by_session = collect_anchor_scores(&indexes, &query_anchors)?;
        }
        ExplainTarget::Literal(text) => {
            query_anchors = if args.anchor {
                vec![text]
            } else {
                derive_anchor_candidates(&[text])
            };
            let traversal = ExplainTraversal {
                min_confidence: args.min_confidence,
                max_fanout: args.max_fanout,
                max_edges: args.max_edges,
                max_depth: args.depth,
            };
            let result =
                explain_across_indexes(&indexes, &query_anchors, traversal, args.forensics)?;
            let touches =
                collect_touch_evidence(&indexes, &result.direct, &result.touched_anchors)?;
            raw_sessions = build_session_windows(context, touches)?;
            let (chain, dispatch_sessions) =
                collect_dispatch_upstream_sessions(context, &indexes[0], &raw_sessions)?;
            dispatch_lineage = chain;
            raw_sessions.extend(dispatch_sessions);
            lineage = result.lineage.iter().map(edge_to_json).collect::<Vec<_>>();
            score_by_session = collect_anchor_scores(&indexes, &query_anchors)?;

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
        }
    }

    if args.pretty {
        print_pretty_explain(&target, &[], &raw_sessions, &tombstones);
        return Ok(());
    }

    let mut sessions = format_sessions_for_agent(
        context,
        &indexes[0],
        raw_sessions,
        &score_by_session,
        args.grep_filter.as_deref(),
    )?;
    sessions.retain(|session| session_matches_date_filter(session, &date_filter));
    annotate_chain_fields(&mut sessions, &dispatch_lineage);
    sessions.sort_by(|a, b| {
        let a_depth = a.get("depth").and_then(Value::as_u64).unwrap_or(0);
        let b_depth = b.get("depth").and_then(Value::as_u64).unwrap_or(0);
        let a_score = a.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
        let b_score = b.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
        let a_ts = a.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let b_ts = b.get("timestamp").and_then(Value::as_str).unwrap_or("");
        a_depth
            .cmp(&b_depth)
            .then_with(|| {
                b_score
                    .partial_cmp(&a_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b_ts.cmp(a_ts))
    });
    if sessions.is_empty() {
        return Err(CliError::new("no_results", target));
    }

    let (sessions, returned, total, time_range, truncated) = apply_session_truncation(
        sessions,
        args.limit,
        args.offset,
        context.explain_default_limit,
    );
    if sessions.is_empty() {
        return Err(CliError::new("no_results", target));
    }
    let chain_metadata = build_chain_metadata(&sessions);
    append_metrics(
        context,
        "explain",
        &target,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
    );

    emit_query_result(
        &indexes[0],
        "explain",
        json!({
        "query": {
            "command": "explain",
            "target": target,
            "anchors": query_anchors,
            "grep_filter": args.grep_filter,
            "limit": args.limit,
            "offset": args.offset,
            "min_confidence": args.min_confidence,
            "since": args.since,
            "until": args.until,
            "count": args.count,
            "max_fanout": args.max_fanout,
            "max_edges": args.max_edges,
            "depth": args.depth,
            "forensics": args.forensics,
            "include_deleted": args.include_deleted,
        },
        "sessions": sessions,
        "chains": chain_metadata,
        "lineage": lineage,
        "dispatch_lineage": dispatch_lineage,
        "tombstones": tombstones,
        "stores_queried": indexes.len(),
        "returned": returned,
        "total": total,
        "time_range": time_range,
        "truncated": truncated,
        }),
    )
}

fn cmd_grep(paths: &RepoPaths, context: &RuntimeContext, args: GrepArgs) -> Result<(), CliError> {
    ensure_local_store(paths)?;
    print_context_conspicuity(context);
    ensure_db_parent(&context.db_path)?;

    let indexes = open_query_indexes(context)?;
    let (raw_sessions, score_by_session) = collect_grep_matches(context, &indexes, &args.pattern)?;
    let date_filter = DateFilter::parse(args.since.as_deref(), args.until.as_deref())?;
    let mut sessions =
        format_sessions_for_agent(context, &indexes[0], raw_sessions, &score_by_session, None)?;
    sessions.retain(|session| session_matches_date_filter(session, &date_filter));
    sessions.sort_by(|a, b| {
        let a_ts = a.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let b_ts = b.get("timestamp").and_then(Value::as_str).unwrap_or("");
        b_ts.cmp(a_ts)
    });
    if sessions.is_empty() {
        return Err(CliError::new("no_results", args.pattern));
    }

    let (sessions, returned, total, time_range, truncated) = apply_session_truncation(
        sessions,
        args.limit,
        args.offset,
        context.explain_default_limit,
    );
    if sessions.is_empty() {
        return Err(CliError::new("no_results", args.pattern));
    }

    let metrics_sessions = if args.count { Vec::new() } else { sessions };
    append_metrics(
        context,
        "grep",
        &args.pattern,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
    );

    emit_query_result(
        &indexes[0],
        "grep",
        json!({
        "query": {
            "command": "grep",
            "pattern": args.pattern,
            "limit": args.limit,
            "offset": args.offset,
            "since": args.since,
            "until": args.until,
            "count": args.count,
        },
        "sessions": metrics_sessions,
        "lineage": [],
        "dispatch_lineage": [],
        "tombstones": [],
        "stores_queried": indexes.len(),
        "returned": returned,
        "total": total,
        "time_range": time_range,
        "truncated": truncated,
        }),
    )
}

fn cmd_peek(paths: &RepoPaths, context: &RuntimeContext, args: PeekArgs) -> Result<(), CliError> {
    ensure_local_store(paths)?;
    print_context_conspicuity(context);
    ensure_db_parent(&context.db_path)?;

    let indexes = open_query_indexes(context)?;
    let session_id = args.session_id;
    let Some(tape_path) = resolve_tape_path(context, &session_id) else {
        return Err(CliError::new("session_not_found", session_id));
    };
    let raw_text = read_tape_content(&tape_path)?;
    let rows = parse_jsonl_rows(&raw_text)?;
    let total_lines = raw_text.lines().count();
    let content_lines = raw_text.lines().collect::<Vec<_>>();
    let timestamp = extract_latest_timestamp_from_rows(&rows);
    let grep_context = context.peek_grep_context.max(1);

    let (window_start, window_end, content) = if let Some(pattern) = args.grep_filter.as_deref() {
        let mut hits = Vec::new();
        for (idx, line) in content_lines.iter().enumerate() {
            if line.contains(pattern) {
                hits.push(idx);
            }
        }
        if hits.is_empty() {
            return Err(CliError::new("no_results", pattern.to_string()));
        }
        let mut ranges = Vec::new();
        for idx in hits {
            let start = idx.saturating_sub(grep_context);
            let end = usize::min(total_lines.saturating_sub(1), idx + grep_context);
            ranges.push((start, end));
        }
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in ranges {
            if let Some(last) = merged.last_mut()
                && start <= last.1.saturating_add(1)
            {
                last.1 = usize::max(last.1, end);
                continue;
            }
            merged.push((start, end));
        }
        let mut out = Vec::new();
        let mut first = usize::MAX;
        let mut last = 0usize;
        for (start, end) in merged {
            first = usize::min(first, start);
            last = usize::max(last, end);
            for idx in start..=end {
                out.push(json!({
                    "line": idx + 1,
                    "text": content_lines.get(idx).copied().unwrap_or_default(),
                }));
            }
        }
        (first + 1, last + 1, out)
    } else {
        let anchor_line = default_peek_anchor_line(&indexes[0], &session_id, &rows);
        if let Some(start) = args.start {
            let line_count = args.lines.unwrap_or(context.peek_default_lines).max(1);
            let end = usize::min(
                total_lines,
                start.saturating_add(line_count).saturating_sub(1),
            );
            let content = if total_lines == 0 || end == 0 {
                Vec::new()
            } else {
                ((start.saturating_sub(1))..end)
                    .map(|idx| {
                        json!({
                            "line": idx + 1,
                            "text": content_lines.get(idx).copied().unwrap_or_default(),
                        })
                    })
                    .collect::<Vec<_>>()
            };
            (start, end, content)
        } else {
            let before = args.before.unwrap_or(context.peek_default_before);
            let after = args.after.unwrap_or(context.peek_default_after);
            let start = anchor_line.saturating_sub(before).max(1);
            let end = usize::min(total_lines, anchor_line.saturating_add(after));
            let content = if total_lines == 0 || end == 0 {
                Vec::new()
            } else {
                ((start - 1)..end)
                    .map(|idx| {
                        json!({
                            "line": idx + 1,
                            "text": content_lines.get(idx).copied().unwrap_or_default(),
                        })
                    })
                    .collect::<Vec<_>>()
            };
            (start, end, content)
        }
    };

    if content.is_empty() {
        return Err(CliError::new("no_results", session_id.clone()));
    }
    let window_lines = content.len();
    append_metrics(
        context,
        "peek",
        &session_id,
        Value::String(session_id.clone()),
        json!(window_start),
        json!(window_lines),
        json!(total_lines),
    );

    emit_query_result(
        &indexes[0],
        "peek",
        json!({
        "query": {
            "command": "peek",
            "session_id": session_id,
            "start": args.start,
            "lines": args.lines,
            "before": args.before,
            "after": args.after,
            "grep_filter": args.grep_filter,
        },
        "session": {
            "session_id": session_id,
            "timestamp": timestamp,
            "window_start": window_start,
            "window_end": window_end,
            "total_lines": total_lines,
            "content": content,
        }
        }),
    )
}

#[derive(Debug, Clone)]
enum ExplainTarget {
    FileRange { file: String, start: u32, end: u32 },
    FileWhole { file: String },
    Literal(String),
}

fn open_query_indexes(context: &RuntimeContext) -> Result<Vec<SqliteIndex>, CliError> {
    let mut indexes = Vec::new();
    indexes.push(SqliteIndex::open(&path_string(&context.db_path))?);
    for store in &context.additional_stores {
        if store.exists() {
            indexes.push(SqliteIndex::open(&path_string(store))?);
        }
    }
    Ok(indexes)
}

fn classify_explain_target(
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

fn has_span_shape(target: &str) -> bool {
    target
        .rsplit_once(':')
        .is_some_and(|(_, rhs)| rhs.contains('-'))
}

fn collect_anchor_scores(
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

fn collect_grep_matches(
    context: &RuntimeContext,
    indexes: &[SqliteIndex],
    pattern: &str,
) -> Result<(Vec<Value>, HashMap<String, f32>), CliError> {
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
    let mut score_by_session = HashMap::new();

    for tape_id in tape_ids {
        let Some(path) = resolve_tape_path(context, &tape_id) else {
            continue;
        };
        let content = read_tape_content(&path)?;
        let lines = content.lines().collect::<Vec<_>>();
        let mut first_match = None;
        let mut match_count = 0usize;
        for (idx, line) in lines.iter().enumerate() {
            if line.contains(pattern) {
                match_count += 1;
                if first_match.is_none() {
                    first_match = Some(idx as u64);
                }
            }
        }
        let Some(first_match) = first_match else {
            continue;
        };

        let rows = parse_jsonl_rows(&content)?;
        let windows = event_window(&rows, first_match, TRANSCRIPT_WINDOW_RADIUS)
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
        score_by_session.insert(
            raw_sessions
                .last()
                .and_then(|v| v.get("tape_id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            match_count as f32,
        );
    }

    Ok((raw_sessions, score_by_session))
}

fn format_sessions_for_agent(
    context: &RuntimeContext,
    primary_index: &SqliteIndex,
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

        let (refs_up, refs_down) = dispatch_ref_counts(primary_index, session_id)?;
        let timestamp = raw
            .get("latest_touch_timestamp")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| extract_latest_timestamp_from_rows(&rows));

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
        }));
    }

    Ok(out)
}

fn dispatch_ref_counts(index: &SqliteIndex, tape_id: &str) -> Result<(usize, usize), CliError> {
    let mut up = 0usize;
    let mut down = 0usize;
    for link in index.dispatch_links_for_tape(tape_id)? {
        match link.direction {
            DispatchDirection::Received => up += 1,
            DispatchDirection::Sent => down += 1,
        }
    }
    Ok((up, down))
}

fn extract_latest_timestamp_from_rows(rows: &[TapeRow]) -> String {
    rows.iter()
        .filter_map(|row| row.value.get("t").and_then(Value::as_str))
        .max()
        .unwrap_or("")
        .to_string()
}

fn collect_files_touched_from_rows(rows: &[TapeRow]) -> Vec<String> {
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

fn apply_session_truncation(
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
struct DateFilter {
    since: Option<chrono::DateTime<Utc>>,
    until: Option<chrono::DateTime<Utc>>,
}

impl DateFilter {
    fn parse(since: Option<&str>, until: Option<&str>) -> Result<Self, CliError> {
        Ok(Self {
            since: parse_date_bound(since, DateBound::Since)?,
            until: parse_date_bound(until, DateBound::Until)?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum DateBound {
    Since,
    Until,
}

fn parse_date_bound(
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

fn session_matches_date_filter(session: &Value, filter: &DateFilter) -> bool {
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

fn annotate_chain_fields(sessions: &mut [Value], dispatch_lineage: &[Value]) {
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

fn build_chain_metadata(sessions: &[Value]) -> Vec<Value> {
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

fn default_peek_anchor_line(index: &SqliteIndex, session_id: &str, rows: &[TapeRow]) -> usize {
    if let Ok(links) = index.dispatch_links_for_tape(session_id)
        && let Some(received) = links
            .into_iter()
            .filter(|link| matches!(link.direction, DispatchDirection::Received))
            .min_by_key(|link| link.first_turn_index)
    {
        if let Some(offset) = message_turn_to_event_offset(rows, received.first_turn_index)
            && let Some(pos) = rows.iter().position(|row| row.offset == offset)
        {
            return pos + 1;
        }
    }
    if rows.is_empty() { 1 } else { 1 }
}

fn collect_dispatch_upstream_sessions(
    context: &RuntimeContext,
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
                message_turn_before_offset(context, &mut rows_cache, tape_id, edit_offset)?;

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

fn build_dispatch_session(
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

fn derive_anchor_candidates(span_texts: &[String]) -> Vec<String> {
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

fn sample_anchor_candidates(anchors: Vec<String>, max_anchors: usize) -> Vec<String> {
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

fn parse_file_range_target(target: &str) -> Result<(&str, u32, u32), CliError> {
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

fn read_file_span_variants(path: &Path, start: u32, end: u32) -> Result<Vec<String>, CliError> {
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

fn message_turn_before_offset(
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
    resolve_runtime_context_with_override(cwd, None)
}

fn resolve_runtime_context_with_override(
    cwd: &Path,
    config_override: Option<&Path>,
) -> Result<RuntimeContext, CliError> {
    let home = home_dir()?;
    let config = load_effective_config_with_override(cwd, &home, config_override)
        .map_err(|err| CliError::new("config_error", err.to_string()))?;
    let tape_lookup_dirs = tape_lookup_dirs(cwd, &home, &config);
    Ok(RuntimeContext {
        config_path: config.path,
        db_path: config.db,
        tapes_dir: config.tapes_dir,
        tape_lookup_dirs,
        additional_stores: config.additional_stores,
        explain_default_limit: config.explain_default_limit,
        peek_default_lines: config.peek.default_lines,
        peek_default_before: config.peek.default_before,
        peek_default_after: config.peek.default_after,
        peek_grep_context: config.peek.grep_context,
        metrics_enabled: config.metrics.enabled,
        metrics_log: config.metrics.log,
        watch: config.watch,
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

fn append_metrics(
    context: &RuntimeContext,
    command: &str,
    target: &str,
    session_id: Value,
    window_start: Value,
    window_lines: Value,
    total_lines: Value,
) {
    if !context.metrics_enabled {
        return;
    }

    if let Some(parent) = context.metrics_log.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return;
    }

    let payload = json!({
        "ts": Utc::now().to_rfc3339(),
        "command": command,
        "target": target,
        "session_id": session_id,
        "window_start": window_start,
        "window_lines": window_lines,
        "total_lines": total_lines,
    });
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&context.metrics_log)
    else {
        return;
    };
    let _ = writeln!(file, "{payload}");
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

fn tape_lookup_dirs(
    cwd: &Path,
    home: &Path,
    config: &engram::config::EffectiveConfig,
) -> Vec<PathBuf> {
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

fn push_tape_lookup_dir(dirs: &mut Vec<PathBuf>, candidate: PathBuf) {
    if dirs.iter().all(|existing| existing != &candidate) {
        dirs.push(candidate);
    }
}

fn resolve_tape_path(context: &RuntimeContext, tape_id: &str) -> Option<PathBuf> {
    context
        .tape_lookup_dirs
        .iter()
        .map(|dir| tape_path_for_tapes_dir(dir, tape_id))
        .find(|path| path.exists())
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

fn emit_query_result(index: &SqliteIndex, command: &str, payload: Value) -> Result<(), CliError> {
    let payload_json = canonical_json_string(&payload)?;
    let result_id = format!(
        "result_{}",
        sha256_hex(&format!("{command}:{payload_json}"))
    );
    let created_at = Utc::now().to_rfc3339();
    index.record_query_result(&result_id, command, &payload_json, &created_at)?;

    let mut object = match payload {
        Value::Object(map) => map,
        _ => {
            return Err(CliError::new(
                "invalid_query_payload",
                format!("{command} payload must be a JSON object"),
            ));
        }
    };
    object.insert("result_id".to_string(), Value::String(result_id.clone()));
    object.insert(
        "rating_hint".to_string(),
        Value::String(format!(
            "Rate this result: engram rate {result_id} --outcome <found_answer|partially_helped|noise|misleading|not_used>"
        )),
    );
    print_json(&Value::Object(object))
}

fn canonical_json_string(value: &Value) -> Result<String, CliError> {
    let mut out = String::new();
    write_canonical_json(value, &mut out)?;
    Ok(out)
}

fn write_canonical_json(value: &Value, out: &mut String) -> Result<(), CliError> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            out.push_str(&serde_json::to_string(value)?);
        }
        Value::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (idx, key) in keys.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key)?);
                out.push(':');
                let value = map
                    .get(*key)
                    .expect("canonical json key collected from same map");
                write_canonical_json(value, out)?;
            }
            out.push('}');
        }
    }
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
    use notify::event::{CreateKind, RemoveKind};

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

    #[test]
    fn cmd_watch_errors_when_watch_config_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let cwd = home.join("workspace");
        fs::create_dir_all(&cwd).expect("workspace");

        let err = cmd_watch_with_home(&cwd, WatchArgs::default(), &home).expect_err("must fail");
        assert_eq!(err.code, "watch_config_error");
        assert!(
            err.message.contains("watch config missing in config.yml"),
            "unexpected message: {}",
            err.message
        );
    }

    #[test]
    fn cmd_watch_errors_when_watch_sources_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let cwd = home.join("workspace");
        fs::create_dir_all(&cwd).expect("workspace");
        let config_path = cwd.join(".engram/config.yml");
        fs::create_dir_all(config_path.parent().expect("parent")).expect("config dir");
        fs::write(&config_path, "db: ./index.sqlite\nwatch:\n  sources: []\n").expect("config");

        let err = cmd_watch_with_home(
            &cwd,
            WatchArgs {
                config: Some(config_path),
            },
            &home,
        )
        .expect_err("must fail");
        assert_eq!(err.code, "watch_config_error");
        assert!(
            err.message
                .contains("watch.sources must contain at least one source"),
            "unexpected message: {}",
            err.message
        );
    }

    #[test]
    fn watch_event_kind_supported_matrix() {
        assert!(watch_event_kind_supported(&EventKind::Create(
            CreateKind::Any
        )));
        assert!(watch_event_kind_supported(&EventKind::Modify(
            ModifyKind::Any
        )));
        assert!(watch_event_kind_supported(&EventKind::Modify(
            ModifyKind::Name(RenameMode::Any)
        )));
        assert!(!watch_event_kind_supported(&EventKind::Any));
        assert!(!watch_event_kind_supported(&EventKind::Remove(
            RemoveKind::Any
        )));
    }

    #[test]
    fn watch_path_matches_preserves_filename_pattern_without_glob() {
        let source_path = PathBuf::from("/tmp/source");
        let runtime = WatchSourceRuntime {
            source: EffectiveWatchSource {
                path: source_path.clone(),
                pattern: "*.jsonl".to_string(),
                glob: None,
            },
            match_root: source_path.clone(),
            pattern: glob::Pattern::new("*.jsonl").expect("pattern"),
            glob: None,
            debounce: Duration::from_secs(1),
            ingest_timeout: Duration::from_secs(1),
        };

        assert!(watch_path_matches(
            &runtime,
            &source_path.join("nested/session.jsonl")
        ));
        assert!(!watch_path_matches(
            &runtime,
            &source_path.join("nested/session.txt")
        ));
    }

    #[test]
    fn watch_path_matches_without_glob_accepts_canonical_event_path() {
        let source_path = PathBuf::from("/tmp/source");
        let match_root = PathBuf::from("/private/tmp/source");
        let runtime = WatchSourceRuntime {
            source: EffectiveWatchSource {
                path: source_path,
                pattern: "*.jsonl".to_string(),
                glob: None,
            },
            match_root: match_root.clone(),
            pattern: glob::Pattern::new("*.jsonl").expect("pattern"),
            glob: None,
            debounce: Duration::from_secs(1),
            ingest_timeout: Duration::from_secs(1),
        };

        assert!(watch_path_matches(
            &runtime,
            &match_root.join("nested/session.jsonl")
        ));
        assert!(!watch_path_matches(
            &runtime,
            &match_root.join("nested/session.txt")
        ));
    }

    #[test]
    fn watch_path_matches_optional_glob_against_relative_path() {
        let source_path = PathBuf::from("/tmp/source");
        let runtime = WatchSourceRuntime {
            source: EffectiveWatchSource {
                path: source_path.clone(),
                pattern: "*.jsonl".to_string(),
                glob: Some("accepted/**/*.jsonl".to_string()),
            },
            match_root: source_path.clone(),
            pattern: glob::Pattern::new("*.jsonl").expect("pattern"),
            glob: Some(glob::Pattern::new("accepted/**/*.jsonl").expect("glob")),
            debounce: Duration::from_secs(1),
            ingest_timeout: Duration::from_secs(1),
        };

        assert!(watch_path_matches(
            &runtime,
            &source_path.join("accepted/nested/session.jsonl")
        ));
        assert!(!watch_path_matches(
            &runtime,
            &source_path.join("ignored/nested/session.jsonl")
        ));
        assert!(!watch_path_matches(
            &runtime,
            &source_path.join("accepted/nested/session.txt")
        ));
    }

    #[test]
    fn watch_path_matches_glob_treats_separator_literally() {
        let source_path = PathBuf::from("/tmp/source");
        let runtime = WatchSourceRuntime {
            source: EffectiveWatchSource {
                path: source_path.clone(),
                pattern: "*.jsonl".to_string(),
                glob: Some("logs/*.jsonl".to_string()),
            },
            match_root: source_path.clone(),
            pattern: glob::Pattern::new("*.jsonl").expect("pattern"),
            glob: Some(glob::Pattern::new("logs/*.jsonl").expect("glob")),
            debounce: Duration::from_secs(1),
            ingest_timeout: Duration::from_secs(1),
        };

        assert!(watch_path_matches(
            &runtime,
            &source_path.join("logs/session.jsonl")
        ));
        assert!(!watch_path_matches(
            &runtime,
            &source_path.join("logs/nested/session.jsonl")
        ));
        assert!(!watch_path_matches(
            &runtime,
            &source_path.join("ignored/session.jsonl")
        ));
    }

    #[test]
    fn watch_path_matches_glob_double_star_allows_nested_paths() {
        let source_path = PathBuf::from("/tmp/source");
        let runtime = WatchSourceRuntime {
            source: EffectiveWatchSource {
                path: source_path.clone(),
                pattern: "*.jsonl".to_string(),
                glob: Some("logs/**/*.jsonl".to_string()),
            },
            match_root: source_path.clone(),
            pattern: glob::Pattern::new("*.jsonl").expect("pattern"),
            glob: Some(glob::Pattern::new("logs/**/*.jsonl").expect("glob")),
            debounce: Duration::from_secs(1),
            ingest_timeout: Duration::from_secs(1),
        };

        assert!(watch_path_matches(
            &runtime,
            &source_path.join("logs/nested/session.jsonl")
        ));
    }

    #[test]
    fn watch_path_matches_canonical_event_path_for_symlinked_source() {
        let source_path = PathBuf::from("/tmp/source");
        let match_root = PathBuf::from("/private/tmp/source");
        let runtime = WatchSourceRuntime {
            source: EffectiveWatchSource {
                path: source_path,
                pattern: "*.jsonl".to_string(),
                glob: Some("accepted/**/*.jsonl".to_string()),
            },
            match_root: match_root.clone(),
            pattern: glob::Pattern::new("*.jsonl").expect("pattern"),
            glob: Some(glob::Pattern::new("accepted/**/*.jsonl").expect("glob")),
            debounce: Duration::from_secs(1),
            ingest_timeout: Duration::from_secs(1),
        };

        assert!(watch_path_matches(
            &runtime,
            &match_root.join("accepted/nested/session.jsonl")
        ));
        assert!(!watch_path_matches(
            &runtime,
            &match_root.join("ignored/nested/session.jsonl")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn watch_path_matches_canonicalized_source_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real_source = dir.path().join("real-source");
        let linked_source = dir.path().join("linked-source");
        fs::create_dir_all(real_source.join("accepted/nested")).expect("real source");
        fs::write(real_source.join("accepted/session.jsonl"), "{}\n").expect("shallow file");
        fs::write(real_source.join("accepted/nested/session.jsonl"), "{}\n").expect("nested file");
        std::os::unix::fs::symlink(&real_source, &linked_source).expect("symlink source");
        let match_root = fs::canonicalize(&linked_source).expect("canonical source");
        let runtime = WatchSourceRuntime {
            source: EffectiveWatchSource {
                path: linked_source,
                pattern: "*.jsonl".to_string(),
                glob: Some("accepted/*.jsonl".to_string()),
            },
            match_root: match_root.clone(),
            pattern: glob::Pattern::new("*.jsonl").expect("pattern"),
            glob: Some(glob::Pattern::new("accepted/*.jsonl").expect("glob")),
            debounce: Duration::from_secs(1),
            ingest_timeout: Duration::from_secs(1),
        };

        assert!(watch_path_matches(
            &runtime,
            &real_source.join("accepted/session.jsonl")
        ));
        assert!(!watch_path_matches(
            &runtime,
            &real_source.join("accepted/nested/session.jsonl")
        ));
    }

    #[test]
    fn derive_anchor_candidates_caps_large_queries() {
        let text = (1..=1914)
            .map(|line| format!("fn line_{line}() {{ value_{line}(); }}\n"))
            .collect::<String>();

        let anchors = derive_anchor_candidates(&[text]);
        assert!(anchors.len() <= MAX_QUERY_WINDOW_ANCHORS);
    }
}
