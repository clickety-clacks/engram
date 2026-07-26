use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use engram::config::{
    EffectiveWatchSource, ensure_user_config, load_effective_config_with_override,
};
use engram::dispatch::*;
#[cfg(test)]
use engram::index::DispatchDirection;
use engram::index::SqliteIndex;
use engram::index::lineage::LINK_THRESHOLD_DEFAULT;
use engram::ingest::*;
use engram::query::explain::ExplainTraversal;
use engram::query::format::*;
use engram::store::atomic::atomic_write;
use engram::store::tapes::*;
use engram::tape::compress::decompress_jsonl;
use engram::tape::event::parse_jsonl_events;
use engram::{CliError, RepoPaths, RuntimeContext, ensure_db_parent, home_dir, path_string};
use notify::event::{ModifyKind, RenameMode};
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde_json::{Value, json};

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
    run_ingest(cwd, paths, context, &args.paths)
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
    let mut kept = 0usize;

    let entries = fs::read_dir(&paths.tapes).map_err(|err| CliError::io("read_dir_error", err))?;
    for entry in entries {
        let entry = entry.map_err(|err| CliError::io("read_dir_error", err))?;
        let path = entry.path();
        let Some(_) = tape_id_from_path(&path) else {
            continue;
        };
        kept += 1;
    }

    print_json(&json!({
        "status": "ok",
        "deleted_tape_ids": [],
        "deleted_count": 0,
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
    let (raw_sessions, grep_rank_by_session) =
        collect_grep_matches(context, &indexes, &args.pattern)?;
    let score_by_session = grep_rank_by_session
        .iter()
        .map(|(session_id, rank)| (session_id.clone(), rank.match_count as f32))
        .collect::<HashMap<_, _>>();
    let date_filter = DateFilter::parse(args.since.as_deref(), args.until.as_deref())?;
    let mut sessions =
        format_sessions_for_agent(context, &indexes[0], raw_sessions, &score_by_session, None)?;
    sessions.retain(|session| session_matches_date_filter(session, &date_filter));
    sessions.sort_by(|a, b| compare_grep_sessions(a, b, &grep_rank_by_session));
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
