use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use engram::access::client::{
    DEFAULT_DECOMPRESSED_BYTES_PER_TAPE, DEFAULT_READ_FILE_COMPRESSED_BYTES, PeerFailure,
    PeerRequest, PeerResponse, RemoteOwner, decode_base64_chunk,
};
use engram::config::{
    EffectiveWatchSource, Topology, TopologyPeer, ensure_user_config, load_effective_config_read_only,
    load_effective_config_with_override, load_frozen_stores, load_topology,
};
use engram::dispatch::{
    collect_dispatch_upstream_sessions, extract_dispatch_links_from_transcript,
};
#[cfg(test)]
use engram::index::DispatchDirection;
use engram::index::{ReaderMode, SqliteIndex};
use engram::index::lineage::LINK_THRESHOLD_DEFAULT;
use engram::ingest::{extract_meta, git_head, now_iso8601, record_transcript, run_ingest};
use engram::query::explain::ExplainTraversal;
#[cfg(test)]
use engram::query::format::MAX_QUERY_WINDOW_ANCHORS;
use engram::query::format::{
    DateFilter, ExplainTarget, GrepRank, annotate_chain_fields, apply_session_truncation,
    build_chain_metadata, build_session_windows, classify_explain_target, collect_anchor_scores,
    collect_grep_matches, collect_touch_evidence, compact_event, compare_explain_sessions,
    compare_grep_sessions, default_peek_anchor_line, derive_anchor_candidates, edge_to_json,
    emit_query_result, explain_across_indexes, extract_latest_timestamp_from_rows,
    format_sessions_for_agent, open_query_indexes, print_pretty_explain, read_file_span_variants,
    session_matches_date_filter,
};
use engram::store::atomic::atomic_write;
use engram::store::tapes::{
    parse_jsonl_rows, print_json, read_tape_content, resolve_tape_path, tape_id_from_path,
    tape_lookup_dirs,
};
use engram::tape::compress::{decompress_jsonl, decompress_jsonl_with_limit};
use engram::tape::event::parse_jsonl_events;
use engram::{CliError, RepoPaths, RuntimeContext, ensure_db_parent, home_dir, path_string};
use notify::event::{ModifyKind, RenameMode};
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_CONCURRENT_PEERS: usize = 4;
const PEER_CONNECT_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const PEER_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const PEER_QUERY_TIMEOUT: Duration = Duration::from_secs(120);
const PEER_QUERY_TERMINAL_RUNNING: u8 = 0;
const PEER_QUERY_TERMINAL_CANCELLED: u8 = 1;
const PEER_QUERY_TERMINAL_COMMITTED: u8 = 2;

struct PeerRoundJob {
    machine: String,
    owner: RemoteOwner,
    exports: Vec<String>,
    requests: Vec<PeerRequest>,
}

struct PeerRoundResult {
    machine: String,
    owner: Option<RemoteOwner>,
    exports: Vec<String>,
    outcomes: Vec<Result<PeerResponse, PeerFailure>>,
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
    Tapes,
    Show(ShowArgs),
    Topology(TopologyArgs),
    Gc,
    #[command(hide = true)]
    PeerServe(PeerServeArgs),
}

#[derive(Args, Debug)]
struct TopologyArgs {
    #[command(subcommand)]
    command: TopologyCommand,
}

#[derive(Subcommand, Debug)]
enum TopologyCommand {
    Status(TopologyStatusArgs),
}

#[derive(Args, Debug)]
struct TopologyStatusArgs {
    #[arg(long, value_name = "PEER")]
    peers: Option<String>,
    #[arg(long)]
    check_exports: bool,
}

#[derive(Args, Debug)]
struct PeerServeArgs {
    #[arg(long, required = true)]
    stdio: bool,
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
    #[arg(long, value_name = "MACHINE/EXPORT")]
    store: Option<String>,
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
    #[arg(long, value_name = "PEER")]
    peers: Option<String>,
    #[arg(long)]
    require_complete: bool,
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
    #[arg(long, value_name = "MACHINE/EXPORT")]
    store: Option<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if err.report_error {
                let payload = error_payload(&err);
                eprintln!("{payload}");
            }
            err.exit_code
                .map(ExitCode::from)
                .unwrap_or(ExitCode::FAILURE)
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
            let context = resolve_query_runtime_context(&cwd)?;
            cmd_explain(&cwd, &paths, &context, args)
        }
        Command::Grep(args) => {
            let context = resolve_query_runtime_context(&cwd)?;
            cmd_grep(&paths, &context, args)
        }
        Command::Peek(args) => {
            let context = resolve_query_runtime_context(&cwd)?;
            cmd_peek(&paths, &context, args)
        }
        Command::Tapes => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_tapes(&paths, &context)
        }
        Command::Show(args) => {
            let context = if args.store.is_some() {
                resolve_query_runtime_context(&cwd)?
            } else {
                resolve_runtime_context(&cwd)?
            };
            cmd_show(&paths, &context, args)
        }
        Command::Topology(args) => match args.command {
            TopologyCommand::Status(args) => cmd_topology_status(args),
        },
        Command::Gc => {
            let context = resolve_runtime_context(&cwd)?;
            cmd_gc(&paths, &context)
        }
        Command::PeerServe(args) => {
            if args.stdio {
                let home = home_dir()?;
                engram::access::peer::serve_stdio(&home)
                    .map_err(|message| CliError::new("peer_serve", message))
            } else {
                Err(CliError::new("peer_serve", "peer-serve requires --stdio"))
            }
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
  topology   Inspect peer handshakes and declared tape exports
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

const HELP_GC: &str = r#"Deprecated. Reports the tape store; deletes nothing, ever.

Tapes are immutable and permanent — never a GC target.
The blob store this command was designed for does not exist.
The derived index is a disposable cache with an explicit
lifecycle instead of garbage collection:

  rebuild   re-ingest all tapes into a staging index
            (isolated config; never rebuild in place)
  validate  PRAGMA quick_check + sanity queries on staging
  swap      atomic rename of staging over live
  retire    keep the old index as a rollback artifact;
            delete it only as a deliberate operator action

See docs/index-lifecycle.md in the Engram repo for the runbook.

OUTPUT:
  JSON on stdout: {"status":"ok","deprecated":true,
  "deleted_tape_ids":[],"deleted_count":0,"kept_count":N}

This command is retained for compatibility; it exits 0 on
success and always reports deleted_count 0.
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
            "gc" => {
                print!("{HELP_GC}");
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
        frozen_stores: Vec::new(),
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
    let frozen_stores = resolved_frozen_store_paths(home, &config.additional_stores)?;
    let context = RuntimeContext {
        config_path: config.path,
        db_path: config.db,
        tapes_dir: config.tapes_dir,
        frozen_stores,
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
    let index = SqliteIndex::open_writer(&path_string(&context.db_path))?;

    let mut scanned = 0usize;
    let mut fingerprinted = 0usize;
    let mut skipped_existing = 0usize;
    let mut failures = Vec::new();

    let entries = fs::read_dir(&paths.tapes).map_err(|err| CliError::io("read_dir_error", err))?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| CliError::io("read_dir_error", err))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(tape_id) = tape_id_from_path(&path) else {
            continue;
        };
        candidates.push((tape_id, path));
    }
    sort_fingerprint_candidates(&mut candidates);

    for (tape_id, path) in candidates {
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

fn sort_fingerprint_candidates(candidates: &mut [(String, PathBuf)]) {
    candidates.sort_by(|(left_id, left_path), (right_id, right_path)| {
        left_id
            .cmp(right_id)
            .then_with(|| left_path.cmp(right_path))
    });
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
    if let Some(store) = args.store.as_deref() {
        return cmd_show_remote(context, &args.tape_id, args.raw, store);
    }
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

fn cmd_show_remote(
    context: &RuntimeContext,
    tape_id: &str,
    raw: bool,
    store_ref: &str,
) -> Result<(), CliError> {
    let (cancelled, terminal_state) = peer_cancellation_flag()?;
    let result = cmd_show_remote_inner(
        context,
        tape_id,
        raw,
        store_ref,
        &cancelled,
        &terminal_state,
    );
    finish_peer_query(result, &terminal_state, "remote show")
}

fn cmd_show_remote_inner(
    context: &RuntimeContext,
    tape_id: &str,
    raw: bool,
    store_ref: &str,
    cancelled: &Arc<AtomicBool>,
    terminal_state: &AtomicU8,
) -> Result<(), CliError> {
    let query_deadline = Instant::now() + PEER_QUERY_TIMEOUT;
    print_context_conspicuity(context);
    let (machine, export, mut owner) =
        connect_remote_store(store_ref, "remote show", query_deadline, cancelled)?;

    let locate = one_peer_response(
        &mut owner,
        PeerRequest::new(
            "locate_tapes",
            vec![export.to_string()],
            json!({"tape_ids":[tape_id]}),
        ),
        "locate_tapes",
        query_deadline,
        cancelled,
    )?;
    if locate.data.len() != 1
        || locate.data[0].get("tape_id").and_then(Value::as_str) != Some(tape_id)
    {
        return Err(CliError::new(
            "protocol_error",
            "peer returned an invalid locate_tapes result",
        ));
    }
    let located = &locate.data[0];
    let Some(file) = located.get("file").filter(|file| !file.is_null()) else {
        return Err(CliError::new(
            "tape_not_found",
            format!("tape `{tape_id}` is not present at `{store_ref}`"),
        ));
    };
    let file_machine = file
        .get("machine")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::new("protocol_error", "peer file address has no machine"))?;
    let file_path = file
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::new("protocol_error", "peer file address has no path"))?;
    let file_kind = file
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::new("protocol_error", "peer file address has no kind"))?;
    if file_machine != machine || file_kind != "tape" || !Path::new(file_path).is_absolute() {
        return Err(CliError::new(
            "protocol_error",
            "peer returned an invalid remote tape address",
        ));
    }
    let file_bytes = located
        .get("size_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| CliError::new("protocol_error", "peer file result has no size"))?;
    let compressed_limit = owner
        .limits
        .get("read_file_compressed_bytes")
        .copied()
        .unwrap_or(DEFAULT_READ_FILE_COMPRESSED_BYTES)
        .min(DEFAULT_READ_FILE_COMPRESSED_BYTES);
    if file_bytes > compressed_limit {
        return Err(CliError::new(
            "budget_exceeded",
            format!("remote tape is {file_bytes} compressed bytes; limit is {compressed_limit}"),
        ));
    }

    let read = one_peer_response(
        &mut owner,
        PeerRequest::new(
            "read_file",
            vec![export.to_string()],
            json!({
                "address": file,
                "max_bytes": compressed_limit,
            }),
        ),
        "read_file",
        query_deadline,
        cancelled,
    )?;
    let capacity = usize::try_from(file_bytes).map_err(|_| {
        CliError::new(
            "budget_exceeded",
            "remote tape size does not fit caller address space",
        )
    })?;
    let mut compressed = Vec::with_capacity(capacity);
    for chunk in &read.data {
        if chunk.get("tape_id").and_then(Value::as_str) != Some(tape_id) {
            return Err(CliError::new(
                "protocol_error",
                "read_file returned a chunk for a different tape",
            ));
        }
        let offset = chunk
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| CliError::new("protocol_error", "read_file chunk has no offset"))?;
        if offset != compressed.len() as u64 {
            return Err(CliError::new(
                "incomplete_stream",
                "remote tape stream has a gap or overlapping chunk",
            ));
        }
        let encoded = chunk
            .get("bytes_b64")
            .and_then(Value::as_str)
            .ok_or_else(|| CliError::new("protocol_error", "read_file chunk has no bytes"))?;
        let bytes = decode_base64_chunk(encoded)
            .map_err(|message| CliError::new("protocol_error", message))?;
        if compressed.len().saturating_add(bytes.len()) > capacity {
            return Err(CliError::new(
                "incomplete_stream",
                "remote tape stream exceeds the located file size",
            ));
        }
        compressed.extend_from_slice(&bytes);
    }
    if compressed.len() != capacity
        || read.stats.get("complete").and_then(Value::as_bool) != Some(true)
        || read.stats.get("bytes").and_then(Value::as_u64) != Some(file_bytes)
        || read.stats.get("tape_id").and_then(Value::as_str) != Some(tape_id)
    {
        return Err(CliError::new(
            "incomplete_stream",
            "peer did not complete the remote tape stream",
        ));
    }
    let decompressed_limit = owner
        .limits
        .get("decompressed_bytes_per_tape")
        .copied()
        .unwrap_or(DEFAULT_DECOMPRESSED_BYTES_PER_TAPE)
        .min(DEFAULT_DECOMPRESSED_BYTES_PER_TAPE);
    let content = match decompress_jsonl_with_limit(&compressed, decompressed_limit) {
        Ok(content) => content,
        Err(error) if error.to_string().starts_with("decompressed tape exceeds ") => {
            return Err(CliError::new("budget_exceeded", error.to_string()));
        }
        Err(error) => return Err(CliError::io("decompress_error", error)),
    };
    let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
    let id_verified = is_sha256_tape_id(tape_id);
    if id_verified && tape_id.to_ascii_lowercase() != digest {
        return Err(CliError::new(
            "id_mismatch",
            format!("remote tape `{tape_id}` content hashes to `{digest}`"),
        ));
    }
    if raw {
        commit_peer_query_terminal(terminal_state, "remote show")?;
        print!("{content}");
        return Ok(());
    }
    let events = parse_jsonl_events(&content)?;
    let rows = parse_jsonl_rows(&content)?;
    let compacted = rows
        .iter()
        .map(|row| compact_event(row.offset, &row.value))
        .collect::<Vec<_>>();
    let payload = json!({
        "tape_id": tape_id,
        "path": null,
        "location": {
            "machine": machine,
            "store": store_ref,
            "path": file_path,
        },
        "digest": digest,
        "id_verified": id_verified,
        "event_count": events.len(),
        "meta": extract_meta(&events),
        "events": compacted,
    });
    commit_peer_query_terminal(terminal_state, "remote show")?;
    print_json(&payload)
}

fn connect_remote_store(
    store_ref: &str,
    command: &str,
    query_deadline: Instant,
    cancelled: &Arc<AtomicBool>,
) -> Result<(String, String, RemoteOwner), CliError> {
    let (machine, export) = store_ref
        .split_once('/')
        .filter(|(machine, export)| {
            !machine.is_empty() && !export.is_empty() && !export.contains('/')
        })
        .ok_or_else(|| {
            CliError::new(
                "invalid_store",
                "remote store must be written as <machine>/<export>",
            )
        })?;
    let home = home_dir()?;
    let topology = load_topology(&home)
        .map_err(|error| CliError::new("config_error", error.to_string()))?
        .ok_or_else(|| {
            CliError::new(
                "topology_missing",
                format!("{command} requires ~/.engram/topology.yml"),
            )
        })?;
    if machine == topology.self_label {
        return Err(CliError::new(
            "store_selection_unsupported",
            "--store currently selects a configured remote machine/export",
        ));
    }
    eprintln!("topology: ~/.engram/topology.yml peers={machine}");
    let mut peer: TopologyPeer = topology.peers.get(machine).cloned().ok_or_else(|| {
        CliError::new(
            "peer_not_configured",
            format!("peer `{machine}` is not configured in ~/.engram/topology.yml"),
        )
    })?;
    if !peer.exports.iter().any(|candidate| candidate == export) {
        return Err(CliError::new(
            "store_not_configured",
            format!("export `{store_ref}` is not selected in the peer topology"),
        ));
    }
    peer.exports = vec![export.to_string()];

    let timeout =
        PEER_CONNECT_OPEN_TIMEOUT.min(query_deadline.saturating_duration_since(Instant::now()));
    let owner =
        RemoteOwner::connect_cancellable(machine, &topology.self_label, &peer, timeout, cancelled)
            .map_err(peer_failure_to_cli)?;
    owner
        .exports
        .get(export)
        .ok_or_else(|| CliError::new("protocol_error", "peer omitted the selected export"))?
        .as_ref()
        .map_err(|failure| peer_failure_to_cli(failure.clone()))?;
    Ok((machine.to_string(), export.to_string(), owner))
}

fn one_peer_response(
    owner: &mut RemoteOwner,
    request: PeerRequest,
    operation: &str,
    query_deadline: Instant,
    cancelled: &Arc<AtomicBool>,
) -> Result<engram::access::client::PeerResponse, CliError> {
    let timeout = peer_operation_timeout(owner, query_deadline);
    owner
        .round_cancellable(&[request], timeout, cancelled)
        .pop()
        .ok_or_else(|| {
            CliError::new(
                "protocol_error",
                format!("peer returned no result for {operation}"),
            )
        })?
        .map_err(peer_failure_to_cli)
}

fn peer_failure_to_cli(failure: PeerFailure) -> CliError {
    let code = match failure.code.as_str() {
        "unavailable" => "unavailable",
        "cancelled" => "cancelled",
        "timeout" => "timeout",
        "no_results" => "no_results",
        "invalid_request" => "invalid_request",
        "incompatible" => "incompatible",
        "incompatible_semantics" => "incompatible_semantics",
        "label_mismatch" => "label_mismatch",
        "reader_unavailable" => "reader_unavailable",
        "tape_unavailable" => "tape_unavailable",
        "invalid_tape" => "invalid_tape",
        "tape_changed" => "tape_changed",
        "over_limit" | "budget_exceeded" => "budget_exceeded",
        "id_mismatch" => "id_mismatch",
        "protocol_error" => "protocol_error",
        "protocol_mismatch" => "protocol_mismatch",
        _ => "peer_error",
    };
    CliError::new(code, format!("{}: {}", failure.code, failure.message))
}

fn is_sha256_tape_id(tape_id: &str) -> bool {
    tape_id.len() == 64 && tape_id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn cmd_gc(paths: &RepoPaths, context: &RuntimeContext) -> Result<(), CliError> {
    ensure_local_store(paths)?;
    print_context_conspicuity(context);
    eprintln!(
        "deprecation: engram gc is deprecated and permanently non-destructive; tapes are immutable and the derived index is maintained by explicit rebuild/validate/swap/retire (see `engram gc --help`)"
    );
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
        "deprecated": true,
        "deleted_tape_ids": [],
        "deleted_count": 0,
        "kept_count": kept,
    }))
}

fn cmd_topology_status(args: TopologyStatusArgs) -> Result<(), CliError> {
    let home = home_dir()?;
    let topology = load_topology(&home)
        .map_err(|error| CliError::new("config_error", error.to_string()))?
        .ok_or_else(|| {
            CliError::new(
                "topology_missing",
                "topology status requires ~/.engram/topology.yml",
            )
        })?;
    let selected = match args.peers.as_deref() {
        Some(selection) => select_peers(selection, &topology.peers)?,
        None => topology.peers.keys().cloned().collect(),
    };
    if !selected.is_empty() {
        eprintln!(
            "topology: ~/.engram/topology.yml peers={}",
            selected.join(",")
        );
    }

    let query_timeout = Duration::from_millis(
        topology
            .limits
            .get("total_query_deadline_ms")
            .copied()
            .unwrap_or(PEER_QUERY_TIMEOUT.as_millis() as u64)
            .min(PEER_QUERY_TIMEOUT.as_millis() as u64),
    );
    let query_deadline = Instant::now() + query_timeout;
    let (cancelled, terminal_state) = if selected.is_empty() {
        (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU8::new(PEER_QUERY_TERMINAL_COMMITTED)),
        )
    } else {
        peer_cancellation_flag()?
    };
    let mut connections = connect_topology_status_peers(
        &selected,
        &topology,
        query_deadline,
        &cancelled,
    );

    let mut peer_rows = Vec::with_capacity(topology.peers.len());
    let mut has_failures = false;
    for (machine, peer) in &topology.peers {
        if selected.contains(machine) {
            let result = connections.remove(machine).unwrap_or_else(|| {
                Err(PeerFailure {
                    code: "unavailable".into(),
                    message: "peer connection did not produce a result".into(),
                })
            });
            let (row, failed) = topology_peer_status_row(machine, result);
            has_failures |= failed;
            peer_rows.push(row);
        } else {
            let stores = if peer.exports.is_empty() {
                vec![format!("{machine}/*")]
            } else {
                peer.exports
                    .iter()
                    .map(|export| format!("{machine}/{export}"))
                    .collect()
            };
            peer_rows.push(json!({
                "machine": machine,
                "status": "not_selected",
                "stores": stores,
            }));
        }
    }

    let local_exports = if args.check_exports {
        let rows = check_local_export_coverage(&topology);
        has_failures |= rows
            .iter()
            .any(|row| row.get("status").and_then(Value::as_str) != Some("ok"));
        rows
    } else {
        Vec::new()
    };

    finish_peer_query(Ok(()), &terminal_state, "topology status")?;
    print_json(&json!({
        "status": if has_failures { "partial" } else { "ok" },
        "self": topology.self_label,
        "selected_peers": selected,
        "peers": peer_rows,
        "check_exports": args.check_exports,
        "local_exports": local_exports,
        "watcher_caught_up": "unknown",
    }))
}

fn topology_peer_status_row(
    machine: &str,
    result: Result<RemoteOwner, PeerFailure>,
) -> (Value, bool) {
    let owner = match result {
        Ok(owner) => owner,
        Err(error) => {
            return (
                json!({
                    "machine": machine,
                    "status": peer_failure_status(&error.code),
                    "error": {"code": error.code, "message": error.message},
                }),
                true,
            );
        }
    };

    let mut failed = false;
    let exports = owner
        .exports
        .iter()
        .map(|(name, result)| match result {
            Ok(export) => json!({
                "store": format!("{machine}/{name}"),
                "status": "ok",
                "db": export.db,
                "tape_dirs": export.tape_dirs,
                "reader_mode": export.reader_mode,
                "snapshot_at": export.snapshot_at,
            }),
            Err(error) => {
                failed = true;
                json!({
                    "store": format!("{machine}/{name}"),
                    "status": peer_failure_status(&error.code),
                    "error": {"code": error.code, "message": error.message},
                })
            }
        })
        .collect::<Vec<_>>();
    (
        json!({
            "machine": machine,
            "status": if failed { "partial" } else { "ok" },
            "handshake": {
                "self": owner.machine,
                "build": owner.build,
                "protocol": owner.protocol,
                "schema": owner.schema,
                "query_semantics": owner.query_semantics,
                "limits": owner.limits,
            },
            "exports": exports,
        }),
        failed,
    )
}

fn check_local_export_coverage(topology: &Topology) -> Vec<Value> {
    topology
        .exports
        .iter()
        .map(|(name, export)| {
            match local_export_tape_coverage(export) {
                Ok((indexed, missing)) => json!({
                    "store": format!("{}/{name}", topology.self_label),
                    "status": "ok",
                    "db": export.db,
                    "tape_dirs": export.tape_dirs,
                    "indexed_tape_count": indexed,
                    "indexed_tapes_without_file": missing,
                }),
                Err((code, message)) => json!({
                    "store": format!("{}/{name}", topology.self_label),
                    "status": if code == "incompatible" { "incompatible" } else { "unavailable" },
                    "db": export.db,
                    "tape_dirs": export.tape_dirs,
                    "error": {"code": code, "message": message},
                }),
            }
        })
        .collect()
}

fn local_export_tape_coverage(
    export: &engram::config::TopologyExport,
) -> Result<(usize, usize), (String, String)> {
    let db = export.db.to_string_lossy();
    let index = SqliteIndex::open_reader_mode(&db, ReaderMode::Live).map_err(|error| {
        let (code, message) = if matches!(error, rusqlite::Error::InvalidQuery) {
            (
                "incompatible".to_string(),
                format!("export schema is not supported: {error}"),
            )
        } else {
            (
                "reader_unavailable".to_string(),
                format!("cannot open Live reader for {}: {error}", export.db.display()),
            )
        };
        (code, message)
    })?;
    index.pin_snapshot().map_err(|error| {
        (
            "reader_unavailable".to_string(),
            format!("cannot pin Live reader snapshot for {}: {error}", export.db.display()),
        )
    })?;
    let tape_ids = index.tape_ids().map_err(|error| {
        (
            "reader_unavailable".to_string(),
            format!("cannot enumerate indexed tapes in {}: {error}", export.db.display()),
        )
    })?;
    let mut missing = 0usize;
    for tape_id in &tape_ids {
        if !valid_tape_filename_segment(tape_id) {
            return Err((
                "invalid_tape_id".to_string(),
                format!("index contains a tape ID that cannot be checked safely: {tape_id:?}"),
            ));
        }
        let filename = format!("{tape_id}.jsonl.zst");
        let mut found = false;
        for directory in &export.tape_dirs {
            match fs::symlink_metadata(directory.join(&filename)) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    found = true;
                    break;
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err((
                        "tape_inventory_error".to_string(),
                        format!("cannot inspect tape {tape_id} in {}: {error}", directory.display()),
                    ));
                }
            }
        }
        if !found {
            missing += 1;
        }
    }
    Ok((tape_ids.len(), missing))
}

fn valid_tape_filename_segment(tape_id: &str) -> bool {
    !tape_id.is_empty()
        && tape_id.len() <= 255
        && !tape_id.starts_with('.')
        && tape_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn cmd_explain(
    cwd: &Path,
    _paths: &RepoPaths,
    context: &RuntimeContext,
    args: ExplainArgs,
) -> Result<(), CliError> {
    print_context_conspicuity(context);

    let target = args
        .target
        .clone()
        .ok_or_else(|| CliError::new("invalid_explain_target", "target is required"))?;
    let target_kind = classify_explain_target(cwd, context, &[], &target, args.anchor)?;
    let indexes = open_query_indexes(context)?;

    let query_anchors;
    let mut raw_sessions: Vec<Value>;
    let dispatch_lineage;
    let lineage;
    let mut tombstones = Vec::new();
    let touched_anchors;
    let score_by_session;
    let mut proof_direct_touches = None;
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
            touched_anchors = result.touched_anchors.clone();
            let touches =
                collect_touch_evidence(&indexes, &result.direct, &result.touched_anchors)?;
            raw_sessions = build_session_windows(context, touches)?;
            let (chain, dispatch_sessions) =
                collect_dispatch_upstream_sessions(context, &indexes, &raw_sessions)?;
            dispatch_lineage = chain;
            raw_sessions.extend(dispatch_sessions);
            lineage = result.lineage.iter().map(edge_to_json).collect::<Vec<_>>();
            score_by_session = collect_anchor_scores(&indexes, &query_anchors)?;
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
            touched_anchors = result.touched_anchors.clone();
            let touches =
                collect_touch_evidence(&indexes, &result.direct, &result.touched_anchors)?;
            raw_sessions = build_session_windows(context, touches)?;
            let (chain, dispatch_sessions) =
                collect_dispatch_upstream_sessions(context, &indexes, &raw_sessions)?;
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
            if std::env::var("T1772_DIRECT_TOUCH_PROJECTION").as_deref() == Ok("1") {
                proof_direct_touches = Some(engram::proof::performance::direct_projection(
                    &result.direct,
                ));
            }
            touched_anchors = result.touched_anchors.clone();
            let touches =
                collect_touch_evidence(&indexes, &result.direct, &result.touched_anchors)?;
            raw_sessions = build_session_windows(context, touches)?;
            let (chain, dispatch_sessions) =
                collect_dispatch_upstream_sessions(context, &indexes, &raw_sessions)?;
            dispatch_lineage = chain;
            raw_sessions.extend(dispatch_sessions);
            lineage = result.lineage.iter().map(edge_to_json).collect::<Vec<_>>();
            score_by_session = collect_anchor_scores(&indexes, &query_anchors)?;
        }
    }

    if args.include_deleted {
        let mut tombstone_anchors = query_anchors.clone();
        for anchor in touched_anchors {
            if !tombstone_anchors.contains(&anchor) {
                tombstone_anchors.push(anchor);
            }
        }
        let mut seen_tombstones = std::collections::HashSet::new();
        for anchor in &tombstone_anchors {
            for index in &indexes {
                for tombstone in index.tombstones_for_anchor(anchor)? {
                    let key = (
                        tombstone.tape_id.clone(),
                        tombstone.event_offset,
                        tombstone.file_path.clone(),
                        tombstone.range_at_deletion.start,
                        tombstone.range_at_deletion.end,
                        tombstone.timestamp.clone(),
                    );
                    if !seen_tombstones.insert(key) {
                        continue;
                    }
                    tombstones.push(json!({
                        "anchor": tombstone.anchor_hashes.first().cloned().unwrap_or_default(),
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
        print_pretty_explain(&target, &[], &raw_sessions, &tombstones);
        return Ok(());
    }

    let mut sessions = format_sessions_for_agent(
        context,
        &indexes,
        raw_sessions,
        &score_by_session,
        args.grep_filter.as_deref(),
    )?;
    sessions.retain(|session| session_matches_date_filter(session, &date_filter));
    annotate_chain_fields(&mut sessions, &dispatch_lineage);
    sessions.sort_by(compare_explain_sessions);
    if sessions.is_empty() && tombstones.is_empty() && lineage.is_empty() {
        return Err(CliError::new("no_results", target));
    }

    let (sessions, returned, total, time_range, truncated) = apply_session_truncation(
        sessions,
        args.limit,
        args.offset,
        context.explain_default_limit,
    );
    if sessions.is_empty() && tombstones.is_empty() && lineage.is_empty() {
        return Err(CliError::new("no_results", target));
    }
    let chain_metadata = build_chain_metadata(&sessions);
    let mut payload = json!({
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
    });
    if let Some(touches) = proof_direct_touches {
        payload["t1772_direct_touches"] = touches;
    }
    emit_query_result("explain", payload)
}

fn cmd_grep(_paths: &RepoPaths, context: &RuntimeContext, args: GrepArgs) -> Result<(), CliError> {
    let query_deadline = Instant::now() + PEER_QUERY_TIMEOUT;
    let cancelled = Arc::new(AtomicBool::new(false));
    print_context_conspicuity(context);

    let indexes = if args.peers.is_some()
        && !context.db_path.exists()
        && context
            .additional_stores
            .iter()
            .all(|store| !store.exists())
    {
        Vec::new()
    } else {
        open_query_indexes(context)?
    };
    let (raw_sessions, grep_rank_by_session) =
        collect_grep_matches(context, &indexes, &args.pattern)?;
    let score_by_session = grep_rank_by_session
        .iter()
        .map(|(session_id, rank)| (session_id.clone(), rank.match_count as f32))
        .collect::<HashMap<_, _>>();
    let date_filter = DateFilter::parse(args.since.as_deref(), args.until.as_deref())?;
    let mut sessions =
        format_sessions_for_agent(context, &indexes, raw_sessions, &score_by_session, None)?;
    sessions.retain(|session| session_matches_date_filter(session, &date_filter));
    sessions.sort_by(|a, b| compare_grep_sessions(a, b, &grep_rank_by_session));

    if args.peers.is_some() {
        let signal_cancelled = Arc::clone(&cancelled);
        let terminal_state = Arc::new(AtomicU8::new(PEER_QUERY_TERMINAL_RUNNING));
        let signal_terminal_state = Arc::clone(&terminal_state);
        ctrlc::set_handler(move || {
            if signal_terminal_state
                .compare_exchange(
                    PEER_QUERY_TERMINAL_RUNNING,
                    PEER_QUERY_TERMINAL_CANCELLED,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                signal_cancelled.store(true, Ordering::SeqCst);
            }
        })
        .map_err(|error| CliError::new("signal_handler_error", error.to_string()))?;
        return cmd_grep_with_peer(
            context,
            indexes,
            sessions,
            grep_rank_by_session,
            args,
            query_deadline,
            cancelled,
            terminal_state,
        );
    }
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
    emit_query_result(
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

fn cmd_grep_with_peer(
    context: &RuntimeContext,
    indexes: Vec<SqliteIndex>,
    mut sessions: Vec<Value>,
    mut ranks: HashMap<String, GrepRank>,
    args: GrepArgs,
    query_deadline: Instant,
    cancelled: Arc<AtomicBool>,
    terminal_state: Arc<AtomicU8>,
) -> Result<(), CliError> {
    let home = home_dir()?;
    let topology = load_topology(&home)
        .map_err(|error| CliError::new("config_error", error.to_string()))?
        .ok_or_else(|| {
            CliError::new(
                "topology_missing",
                "grep --peers requires ~/.engram/topology.yml",
            )
        })?;
    let selection = args.peers.as_deref().unwrap_or_default();
    let selected_machines = select_peers(selection, &topology.peers)?;
    eprintln!(
        "topology: ~/.engram/topology.yml peers={}",
        selected_machines.join(",")
    );

    for session in &mut sessions {
        let session_id = session
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        session["tape_id"] = json!(session_id);
        session["location"] = local_grep_location(context, &topology.self_label, &session_id);
        session["locations"] = json!([session["location"].clone()]);
    }

    let page_limit = args.limit.unwrap_or(context.explain_default_limit).min(25);
    let k = args.offset.saturating_add(page_limit);
    let mut source_rows = local_grep_source_rows(context, &topology.self_label);
    let fallback_store = format!("{}/local-files", topology.self_label);
    if sessions
        .iter()
        .any(|session| session["location"]["store"].as_str() == Some(fallback_store.as_str()))
    {
        source_rows.push(json!({
            "store": fallback_store,
            "kind": "local_tapes",
            "status": "ok",
        }));
    }
    for (unselected, config) in &topology.peers {
        if selected_machines
            .iter()
            .any(|selected| selected == unselected)
        {
            continue;
        }
        if config.exports.is_empty() {
            source_rows.push(json!({
                "store": format!("{unselected}/*"),
                "status": "not_selected",
            }));
        } else {
            for export in &config.exports {
                source_rows.push(json!({
                    "store": format!("{unselected}/{export}"),
                    "status": "not_selected",
                }));
            }
        }
    }

    let mut identity_conflicts = Vec::new();
    let mut source_totals = vec![sessions.len()];
    let mut source_count_known = true;
    let mut source_time_ranges = vec![
        sessions
            .iter()
            .filter_map(|session| session.get("timestamp").and_then(Value::as_str))
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>(),
    ];
    let mut any_source_failure = false;
    // Grep aggregates are scoped to every selected source. A later metadata
    // failure must not erase a terminal-success scan, but any missing or
    // incomplete grep_scan keeps the matching scope unknown.
    let mut grep_scan_incomplete = false;
    let mut any_store_truncated = false;
    let mut peer_store_count = 0usize;
    let mut peer_owners = Vec::new();
    let mut peer_round_jobs = Vec::new();
    let mut connections = connect_peers_concurrently(
        &selected_machines,
        &topology.self_label,
        &topology.peers,
        query_deadline,
        &cancelled,
    );

    for machine in &selected_machines {
        let peer = topology
            .peers
            .get(machine)
            .expect("selected peers were resolved from topology")
            .clone();
        if peer.exports.is_empty() {
            any_source_failure = true;
            grep_scan_incomplete = true;
            source_rows.push(json!({
                "store": format!("{machine}/*"),
                "kind": "peer",
                "status": "failed",
                "phase": "open",
                "error": {
                    "code": "peer_exports_missing",
                    "message": format!("peer `{machine}` declares no queryable exports"),
                },
            }));
            continue;
        }

        let connection = connections.remove(machine).unwrap_or_else(|| {
            Err(PeerFailure {
                code: "unavailable".into(),
                message: "peer connection worker returned no result".into(),
            })
        });
        match connection {
            Err(failure) => {
                any_source_failure = true;
                grep_scan_incomplete = true;
                for export in &peer.exports {
                    source_rows.push(json!({
                        "store": format!("{machine}/{export}"),
                        "kind": "peer",
                        "status": peer_failure_status(&failure.code),
                        "phase": "open",
                        "error": {"code": failure.code, "message": failure.message},
                    }));
                }
            }
            Ok(owner) => {
                let mut active_exports = Vec::new();
                for export in &peer.exports {
                    match owner.exports.get(export) {
                        Some(Ok(opened)) => {
                            source_rows.push(json!({
                                "store": format!("{machine}/{export}"),
                                "db": opened.db,
                                "kind": "peer",
                                "status": "ok",
                                "snapshot_at": opened.snapshot_at,
                                "build": owner.build,
                                "semantics": owner.query_semantics,
                            }));
                            active_exports.push(export.clone());
                        }
                        Some(Err(failure)) => {
                            any_source_failure = true;
                            grep_scan_incomplete = true;
                            source_rows.push(json!({
                                "store": format!("{machine}/{export}"),
                                "kind": "peer",
                                "status": peer_failure_status(&failure.code),
                                "phase": "open",
                                "error": {"code": failure.code, "message": failure.message},
                            }));
                        }
                        None => {
                            any_source_failure = true;
                            grep_scan_incomplete = true;
                            source_rows.push(json!({
                            "store": format!("{machine}/{export}"),
                            "kind": "peer",
                            "status": "failed",
                            "phase": "open",
                            "error": {"code": "protocol_error", "message": "peer omitted configured export"},
                        }));
                        }
                    }
                }

                let grep_limit = owner.limits.get("grep_k").copied().unwrap_or(10_000);
                if k as u64 > grep_limit {
                    any_source_failure = true;
                    grep_scan_incomplete = true;
                    for export in &active_exports {
                        mark_source_phase(
                            &mut source_rows,
                            &format!("{machine}/{export}"),
                            "grep_scan",
                            "budget_exceeded",
                            &format!("grep page requires k={k}; peer limit is {grep_limit}"),
                        );
                    }
                    continue;
                }
                if active_exports.is_empty() {
                    continue;
                }
                let requests = active_exports
                    .iter()
                    .map(|export| {
                        PeerRequest::new(
                            "grep_scan",
                            vec![export.clone()],
                            json!({
                                "pattern": args.pattern,
                                "since": args.since,
                                "until": args.until,
                                "k": k,
                            }),
                        )
                    })
                    .collect::<Vec<_>>();
                peer_round_jobs.push(PeerRoundJob {
                    machine: machine.clone(),
                    owner,
                    exports: active_exports,
                    requests,
                });
            }
        }
    }

    for result in run_peer_rounds_concurrently(peer_round_jobs, query_deadline, &cancelled) {
        let PeerRoundResult {
            machine,
            owner,
            exports: active_exports,
            outcomes,
        } = result;
        let mut grep_succeeded = Vec::new();
        for (index, export) in active_exports.iter().enumerate() {
            let Some(outcome) = outcomes.get(index) else {
                any_source_failure = true;
                grep_scan_incomplete = true;
                mark_source_phase(
                    &mut source_rows,
                    &format!("{machine}/{export}"),
                    "grep_scan",
                    "protocol_error",
                    "peer returned no grep_scan outcome",
                );
                continue;
            };
            let response = match outcome {
                Ok(response) => response,
                Err(failure) => {
                    any_source_failure = true;
                    grep_scan_incomplete = true;
                    mark_source_phase(
                        &mut source_rows,
                        &format!("{machine}/{export}"),
                        "grep_scan",
                        &failure.code,
                        &failure.message,
                    );
                    continue;
                }
            };
            let store_name = format!("{machine}/{export}");
            let Some(total) = response
                .stats
                .get("total")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
            else {
                any_source_failure = true;
                grep_scan_incomplete = true;
                mark_source_phase(
                    &mut source_rows,
                    &store_name,
                    "grep_scan",
                    "protocol_error",
                    "grep_scan response has no valid total count",
                );
                continue;
            };
            let Some(store_truncated) = response.stats.get("truncated").and_then(Value::as_bool)
            else {
                any_source_failure = true;
                grep_scan_incomplete = true;
                mark_source_phase(
                    &mut source_rows,
                    &store_name,
                    "grep_scan",
                    "protocol_error",
                    "grep_scan response has no boolean truncated value",
                );
                continue;
            };
            let Some(store_time_range) = valid_peer_time_range(&response.stats) else {
                any_source_failure = true;
                grep_scan_incomplete = true;
                mark_source_phase(
                    &mut source_rows,
                    &store_name,
                    "grep_scan",
                    "protocol_error",
                    "grep_scan response has no valid time range",
                );
                continue;
            };
            let mut store_failures = Vec::new();
            let mut valid_response = true;
            let mut store_records = Vec::<(Value, GrepRank)>::new();
            for record in &response.data {
                match record.get("type").and_then(Value::as_str) {
                    Some("failure") => {
                        any_source_failure = true;
                        store_failures.push(record.clone());
                    }
                    Some("match") => {
                        match format_peer_grep_session(record, &machine, export, context) {
                            Ok((session, rank)) => store_records.push((session, rank)),
                            Err(error) => {
                                any_source_failure = true;
                                valid_response = false;
                                store_failures.push(json!({
                                    "error": {"code": error.code, "message": error.message},
                                }));
                            }
                        }
                    }
                    _ => {
                        any_source_failure = true;
                        valid_response = false;
                        store_failures.push(json!({
                            "error": {"code": "protocol_error", "message": "unknown grep_scan data record"},
                        }));
                    }
                }
            }
            source_totals.push(total);
            if let Some(source) = source_rows.iter_mut().find(|source| {
                source.get("store").and_then(Value::as_str) == Some(store_name.as_str())
            }) {
                source["grep_scan"] = json!({
                    "total": total,
                    "returned": store_records.len(),
                    "time_range": store_time_range,
                    "truncated": store_truncated,
                });
            }
            if !store_failures.is_empty() {
                mark_source_failures(&mut source_rows, &store_name, "grep_scan", &store_failures);
                source_count_known = false;
                grep_scan_incomplete = true;
            }
            source_time_ranges.push(peer_time_range(&response.stats));
            any_store_truncated |= store_truncated || total > k;
            if !valid_response {
                continue;
            }
            for (mut session, rank) in store_records {
                let tape_id = session["tape_id"].as_str().unwrap_or_default().to_string();
                session["locations"] = json!([session["location"].clone()]);
                merge_peer_grep_session(
                    &mut sessions,
                    &mut ranks,
                    &mut identity_conflicts,
                    session,
                    rank,
                    &tape_id,
                );
            }
            grep_succeeded.push(export.clone());
        }
        if let Some(owner) = owner {
            peer_store_count = peer_store_count.saturating_add(grep_succeeded.len());
            peer_owners.push((machine, owner, grep_succeeded));
        }
    }

    let mut page_ranked = sessions;
    page_ranked.sort_by(|a, b| compare_grep_sessions(a, b, &ranks));
    let start = args.offset.min(page_ranked.len());
    let page_len = page_limit.min(page_ranked.len().saturating_sub(start));
    if !args.count && page_len > 0 {
        let page_ids = page_ranked
            .iter()
            .skip(start)
            .take(page_len)
            .filter_map(|session| session.get("tape_id").and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let mut refs_by_tape =
            HashMap::<String, std::collections::HashSet<(String, String)>>::new();
        for session in page_ranked.iter().skip(start).take(page_len) {
            let Some(tape_id) = session.get("tape_id").and_then(Value::as_str) else {
                continue;
            };
            let refs = refs_by_tape.entry(tape_id.to_string()).or_default();
            for index in &indexes {
                for link in index.dispatch_links_for_tape(tape_id)? {
                    let direction =
                        if matches!(link.direction, engram::index::DispatchDirection::Received) {
                            "received"
                        } else {
                            "sent"
                        };
                    refs.insert((link.uuid, direction.to_string()));
                }
            }
        }
        let mut dispatch_jobs = Vec::new();
        for (machine, owner, grep_ok_exports) in peer_owners.drain(..) {
            if grep_ok_exports.is_empty() {
                continue;
            }
            let requests = grep_ok_exports
                .iter()
                .map(|export| {
                    PeerRequest::new(
                        "dispatch_rows",
                        vec![export.clone()],
                        json!({"by_tape": page_ids.clone()}),
                    )
                })
                .collect::<Vec<_>>();
            dispatch_jobs.push(PeerRoundJob {
                machine,
                owner,
                exports: grep_ok_exports,
                requests,
            });
        }
        for result in run_peer_rounds_concurrently(dispatch_jobs, query_deadline, &cancelled) {
            let PeerRoundResult {
                machine,
                owner: _owner,
                exports: grep_ok_exports,
                outcomes,
            } = result;
            for (index, export) in grep_ok_exports.iter().enumerate() {
                let store_name = format!("{machine}/{export}");
                match outcomes.get(index) {
                    Some(Ok(response)) => {
                        for row in &response.data {
                            let Some(tape_id) = row.get("tape_id").and_then(Value::as_str) else {
                                any_source_failure = true;
                                mark_source_phase(
                                    &mut source_rows,
                                    &store_name,
                                    "dispatch_rows",
                                    "protocol_error",
                                    "dispatch row has no tape_id",
                                );
                                break;
                            };
                            let Some(uuid) = row.get("uuid").and_then(Value::as_str) else {
                                any_source_failure = true;
                                mark_source_phase(
                                    &mut source_rows,
                                    &store_name,
                                    "dispatch_rows",
                                    "protocol_error",
                                    "dispatch row has no uuid",
                                );
                                break;
                            };
                            let Some(direction) = row.get("direction").and_then(Value::as_str)
                            else {
                                any_source_failure = true;
                                mark_source_phase(
                                    &mut source_rows,
                                    &store_name,
                                    "dispatch_rows",
                                    "protocol_error",
                                    "dispatch row has no direction",
                                );
                                break;
                            };
                            if !matches!(direction, "received" | "sent") {
                                any_source_failure = true;
                                mark_source_phase(
                                    &mut source_rows,
                                    &store_name,
                                    "dispatch_rows",
                                    "protocol_error",
                                    "dispatch row has an unknown direction",
                                );
                                break;
                            }
                            refs_by_tape
                                .entry(tape_id.to_string())
                                .or_default()
                                .insert((uuid.to_string(), direction.to_string()));
                        }
                    }
                    Some(Err(failure)) => {
                        any_source_failure = true;
                        mark_source_phase(
                            &mut source_rows,
                            &store_name,
                            "dispatch_rows",
                            &failure.code,
                            &failure.message,
                        );
                    }
                    None => {
                        any_source_failure = true;
                        mark_source_phase(
                            &mut source_rows,
                            &store_name,
                            "dispatch_rows",
                            "protocol_error",
                            "peer returned no dispatch_rows outcome",
                        );
                    }
                }
            }
        }
        for session in &mut page_ranked[start..start + page_len] {
            let tape_id = session["tape_id"].as_str().unwrap_or_default();
            if any_source_failure {
                session["refs_up"] = Value::Null;
                session["refs_down"] = Value::Null;
            } else if let Some(refs) = refs_by_tape.get(tape_id) {
                session["refs_up"] = json!(
                    refs.iter()
                        .filter(|(_, direction)| direction == "received")
                        .count()
                );
                session["refs_down"] = json!(
                    refs.iter()
                        .filter(|(_, direction)| direction == "sent")
                        .count()
                );
            }
        }
    }

    let any_selected_failure = any_source_failure
        || source_rows.iter().any(|source| {
            source
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| {
                    status == "unavailable"
                        || status == "incompatible"
                        || status == "label_mismatch"
                        || status == "failed"
                        || status == "partial"
                })
        });
    let coverage = if any_selected_failure {
        "partial"
    } else {
        "complete"
    };
    let total_exact = source_count_known
        && !grep_scan_incomplete
        && source_totals.iter().skip(1).all(|total| *total <= k);
    let exact_total = if total_exact {
        Some(page_ranked.len())
    } else {
        None
    };
    let min_total = source_totals
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
        .max(page_ranked.len());
    let max_total = if source_count_known && !grep_scan_incomplete {
        Some(
            source_totals
                .iter()
                .fold(0usize, |sum, value| sum.saturating_add(*value)),
        )
    } else {
        None
    };
    let total_bounds = if exact_total.is_none() {
        Some(json!({"min": min_total, "max": max_total}))
    } else {
        None
    };
    let time_range = if grep_scan_incomplete {
        Value::Null
    } else {
        merge_grep_time_ranges(&source_time_ranges)
    };
    let page = page_ranked
        .iter()
        .skip(start)
        .take(page_limit)
        .cloned()
        .collect::<Vec<_>>();
    let returned = page.len();
    let definitely_truncated =
        any_store_truncated || args.offset.saturating_add(returned) < page_ranked.len();
    let truncated = if definitely_truncated {
        json!(true)
    } else if grep_scan_incomplete {
        Value::Null
    } else if let Some(total) = exact_total {
        json!(args.offset.saturating_add(returned) < total)
    } else {
        // With all selected scans complete, and no positive tail proof above,
        // there is no remaining result after the requested page.
        json!(false)
    };
    let output_sessions = if args.count { Vec::new() } else { page };
    let mut payload = json!({
        "query": {
            "command": "grep",
            "pattern": args.pattern,
            "limit": args.limit,
            "offset": args.offset,
            "since": args.since,
            "until": args.until,
            "count": args.count,
            "peers": selection,
            "require_complete": args.require_complete,
        },
        "sessions": output_sessions,
        "lineage": [],
        "dispatch_lineage": [],
        "tombstones": [],
        "stores_queried": indexes.len() + peer_store_count,
        "returned": returned,
        "total": exact_total,
        "time_range": time_range,
        "truncated": truncated,
        "federation": {
            "self": topology.self_label,
            "coverage": coverage,
            "sources": source_rows.clone(),
            "identity_conflicts": identity_conflicts,
        },
    });
    if let Some(bounds) = total_bounds {
        payload["total_bounds"] = bounds;
    }

    let caller_cancelled = commit_grep_terminal(&terminal_state);
    if caller_cancelled {
        let incomplete_sources = source_rows
            .iter()
            .filter(|source| source.get("phase").is_some())
            .map(|source| {
                json!({
                    "store": source.get("store").cloned().unwrap_or(Value::Null),
                    "status": source.get("status").cloned().unwrap_or(Value::Null),
                    "phase": source.get("phase").cloned().unwrap_or(Value::Null),
                    "error": source.get("error").cloned().unwrap_or(Value::Null),
                })
            })
            .collect::<Vec<_>>();
        payload["cancellation"] = json!({
            "status": "cancelled",
            "source": "caller_sigint",
            "incomplete_sources": incomplete_sources,
        });
        emit_query_result("grep", payload)?;
        return Err(
            CliError::new("cancelled", "federated grep interrupted by caller SIGINT")
                .with_exit_code(130)
                .without_error_report(),
        );
    }
    if args.require_complete && any_selected_failure {
        return Err(CliError::new(
            "incomplete_coverage",
            "grep --require-complete rejected one or more failed peer sources",
        ));
    }
    if returned == 0 {
        if any_selected_failure {
            emit_query_result("grep", payload)?;
        }
        return Err(CliError::new("no_results", args.pattern));
    }
    emit_query_result("grep", payload)
}

fn commit_grep_terminal(terminal_state: &AtomicU8) -> bool {
    match terminal_state.compare_exchange(
        PEER_QUERY_TERMINAL_RUNNING,
        PEER_QUERY_TERMINAL_COMMITTED,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) => false,
        Err(PEER_QUERY_TERMINAL_CANCELLED) => {
            terminal_state.store(PEER_QUERY_TERMINAL_COMMITTED, Ordering::SeqCst);
            true
        }
        Err(PEER_QUERY_TERMINAL_COMMITTED) => false,
        Err(_) => false,
    }
}

fn select_peers(
    selection: &str,
    peers: &std::collections::BTreeMap<String, TopologyPeer>,
) -> Result<Vec<String>, CliError> {
    let selection = selection.trim();
    if selection == "all" {
        if peers.is_empty() {
            return Err(CliError::new(
                "peer_not_configured",
                "topology has no configured peers",
            ));
        }
        return Ok(peers.keys().cloned().collect());
    }

    let mut selected = std::collections::BTreeSet::new();
    for machine in selection.split(',').map(str::trim) {
        if machine.is_empty() {
            return Err(CliError::new(
                "invalid_peer_selection",
                "--peers must be `all` or a comma-separated list of configured peer labels",
            ));
        }
        if !peers.contains_key(machine) {
            return Err(CliError::new(
                "peer_not_configured",
                format!("peer `{machine}` is not configured in ~/.engram/topology.yml"),
            ));
        }
        if !selected.insert(machine.to_string()) {
            return Err(CliError::new(
                "invalid_peer_selection",
                format!("peer `{machine}` appears more than once in --peers"),
            ));
        }
    }

    Ok(selected.into_iter().collect())
}

fn connect_topology_status_peers(
    selected: &[String],
    topology: &Topology,
    deadline: Instant,
    cancelled: &Arc<AtomicBool>,
) -> HashMap<String, Result<RemoteOwner, PeerFailure>> {
    let configured_concurrency = topology
        .limits
        .get("concurrent_peer_connections")
        .copied()
        .unwrap_or(MAX_CONCURRENT_PEERS as u64)
        .clamp(1, MAX_CONCURRENT_PEERS as u64) as usize;
    let configured_connect_timeout = Duration::from_millis(
        topology
            .limits
            .get("connect_open_deadline_ms")
            .copied()
            .unwrap_or(PEER_CONNECT_OPEN_TIMEOUT.as_millis() as u64)
            .min(PEER_CONNECT_OPEN_TIMEOUT.as_millis() as u64),
    );
    let mut results = HashMap::with_capacity(selected.len());
    for batch in selected.chunks(configured_concurrency) {
        let batch_results = std::thread::scope(|scope| {
            let handles = batch
                .iter()
                .map(|machine| {
                    let machine = machine.clone();
                    let peer = topology.peers[&machine].clone();
                    let caller = topology.self_label.clone();
                    let timeout = configured_connect_timeout
                        .min(deadline.saturating_duration_since(Instant::now()));
                    let cancelled = Arc::clone(cancelled);
                    scope.spawn(move || {
                        let result = RemoteOwner::connect_cancellable(
                            &machine,
                            &caller,
                            &peer,
                            timeout,
                            &cancelled,
                        );
                        (machine, result)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .zip(batch)
                .map(|(handle, machine)| {
                    handle.join().unwrap_or_else(|_| {
                        (
                            machine.clone(),
                            Err(PeerFailure {
                                code: "unavailable".into(),
                                message: "peer status worker panicked".into(),
                            }),
                        )
                    })
                })
                .collect::<Vec<_>>()
        });
        results.extend(batch_results);
    }
    results
}

fn run_peer_rounds_concurrently(
    jobs: Vec<PeerRoundJob>,
    deadline: Instant,
    cancelled: &Arc<AtomicBool>,
) -> Vec<PeerRoundResult> {
    let mut pending = jobs.into_iter();
    let mut results = Vec::new();
    loop {
        let batch = pending
            .by_ref()
            .take(MAX_CONCURRENT_PEERS)
            .collect::<Vec<_>>();
        if batch.is_empty() {
            break;
        }

        let batch_results = std::thread::scope(|scope| {
            let handles = batch
                .into_iter()
                .map(|job| {
                    let machine = job.machine.clone();
                    let exports = job.exports.clone();
                    let requests_len = job.requests.len();
                    let cancelled = Arc::clone(cancelled);
                    let handle = scope.spawn(move || {
                        let PeerRoundJob {
                            mut owner,
                            requests,
                            ..
                        } = job;
                        let timeout = peer_operation_timeout(&owner, deadline);
                        let outcomes = owner.round_cancellable(&requests, timeout, &cancelled);
                        (owner, outcomes)
                    });
                    (machine, exports, requests_len, handle)
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(
                    |(machine, exports, requests_len, handle)| match handle.join() {
                        Ok((owner, outcomes)) => PeerRoundResult {
                            machine,
                            owner: Some(owner),
                            exports,
                            outcomes,
                        },
                        Err(_) => PeerRoundResult {
                            machine,
                            owner: None,
                            exports,
                            outcomes: (0..requests_len)
                                .map(|_| {
                                    Err(PeerFailure {
                                        code: "unavailable".into(),
                                        message: "peer request worker panicked".into(),
                                    })
                                })
                                .collect(),
                        },
                    },
                )
                .collect::<Vec<_>>()
        });
        results.extend(batch_results);
    }
    results
}

fn connect_peers_concurrently(
    selected: &[String],
    caller: &str,
    peers: &std::collections::BTreeMap<String, TopologyPeer>,
    deadline: Instant,
    cancelled: &Arc<AtomicBool>,
) -> HashMap<String, Result<RemoteOwner, PeerFailure>> {
    let jobs = selected
        .iter()
        .filter_map(|machine| {
            peers
                .get(machine)
                .filter(|peer| !peer.exports.is_empty())
                .map(|peer| (machine.clone(), peer.clone()))
        })
        .collect::<Vec<_>>();
    let mut connections = HashMap::with_capacity(jobs.len());

    for batch in jobs.chunks(MAX_CONCURRENT_PEERS) {
        let results = std::thread::scope(|scope| {
            let handles = batch
                .iter()
                .map(|(machine, peer)| {
                    let machine = machine.clone();
                    let peer = peer.clone();
                    let caller = caller.to_string();
                    let cancelled = Arc::clone(cancelled);
                    scope.spawn(move || {
                        let timeout = PEER_CONNECT_OPEN_TIMEOUT
                            .min(deadline.saturating_duration_since(Instant::now()));
                        let result = RemoteOwner::connect_cancellable(
                            &machine, &caller, &peer, timeout, &cancelled,
                        );
                        (machine, result)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .zip(batch)
                .map(|(handle, (machine, _))| {
                    handle.join().unwrap_or_else(|_| {
                        (
                            machine.clone(),
                            Err(PeerFailure {
                                code: "unavailable".into(),
                                message: "peer connection worker panicked".into(),
                            }),
                        )
                    })
                })
                .collect::<Vec<_>>()
        });
        connections.extend(results);
    }

    connections
}

fn local_grep_source_rows(context: &RuntimeContext, machine: &str) -> Vec<Value> {
    let mut sources = Vec::new();
    if context.db_path.exists() {
        sources.push(json!({
            "store": format!("{machine}/local:0"),
            "db": context.db_path,
            "kind": "primary",
            "status": "ok",
        }));
    }
    for (index, db) in context.additional_stores.iter().enumerate() {
        if !db.exists() {
            continue;
        }
        sources.push(json!({
            "store": format!("{machine}/local:{}", index + 1),
            "db": db,
            "kind": if context.frozen_stores.contains(db) { "frozen" } else { "local" },
            "status": "ok",
        }));
    }
    sources
}

fn local_grep_location(context: &RuntimeContext, machine: &str, tape_id: &str) -> Value {
    let Some(path) = resolve_tape_path(context, tape_id) else {
        return json!({"machine": machine, "store": format!("{machine}/local-files")});
    };
    if path.parent() == Some(context.tapes_dir.as_path()) {
        return json!({"machine": machine, "store": format!("{machine}/local:0")});
    }
    for (index, db) in context.additional_stores.iter().enumerate() {
        let Some(parent) = db.parent() else {
            continue;
        };
        let tapes_dir = parent.join("tapes");
        if path.parent() == Some(tapes_dir.as_path()) {
            return json!({
                "machine": machine,
                "store": format!("{machine}/local:{}", index + 1),
            });
        }
    }
    json!({"machine": machine, "store": format!("{machine}/local-files")})
}

fn peer_failure_status(code: &str) -> &'static str {
    match code {
        "incompatible" | "incompatible_semantics" => "incompatible",
        "label_mismatch" => "label_mismatch",
        "cancelled" => "failed",
        _ => "unavailable",
    }
}

fn peer_operation_timeout(owner: &RemoteOwner, query_deadline: Instant) -> Duration {
    let advertised_ms = owner
        .limits
        .get("request_timeout_ms")
        .copied()
        .unwrap_or(PEER_OPERATION_TIMEOUT.as_millis() as u64);
    Duration::from_millis(advertised_ms)
        .min(PEER_OPERATION_TIMEOUT)
        .min(query_deadline.saturating_duration_since(Instant::now()))
}

fn peer_cancellation_flag() -> Result<(Arc<AtomicBool>, Arc<AtomicU8>), CliError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal_cancelled = Arc::clone(&cancelled);
    let terminal_state = Arc::new(AtomicU8::new(PEER_QUERY_TERMINAL_RUNNING));
    let signal_terminal_state = Arc::clone(&terminal_state);
    ctrlc::set_handler(move || {
        if signal_terminal_state
            .compare_exchange(
                PEER_QUERY_TERMINAL_RUNNING,
                PEER_QUERY_TERMINAL_CANCELLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            signal_cancelled.store(true, Ordering::SeqCst);
        }
    })
    .map_err(|error| CliError::new("signal_handler_error", error.to_string()))?;
    Ok((cancelled, terminal_state))
}

fn caller_sigint_error(command: &str) -> CliError {
    CliError::new(
        "cancelled",
        format!("{command} interrupted by caller SIGINT"),
    )
    .with_exit_code(130)
}

fn commit_peer_query_terminal(terminal_state: &AtomicU8, command: &str) -> Result<(), CliError> {
    match terminal_state.compare_exchange(
        PEER_QUERY_TERMINAL_RUNNING,
        PEER_QUERY_TERMINAL_COMMITTED,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) | Err(PEER_QUERY_TERMINAL_COMMITTED) => Ok(()),
        Err(PEER_QUERY_TERMINAL_CANCELLED) => {
            terminal_state.store(PEER_QUERY_TERMINAL_COMMITTED, Ordering::SeqCst);
            Err(caller_sigint_error(command))
        }
        Err(_) => Ok(()),
    }
}

fn finish_peer_query(
    result: Result<(), CliError>,
    terminal_state: &AtomicU8,
    command: &str,
) -> Result<(), CliError> {
    match terminal_state.compare_exchange(
        PEER_QUERY_TERMINAL_RUNNING,
        PEER_QUERY_TERMINAL_COMMITTED,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) | Err(PEER_QUERY_TERMINAL_COMMITTED) => result,
        Err(PEER_QUERY_TERMINAL_CANCELLED) => {
            terminal_state.store(PEER_QUERY_TERMINAL_COMMITTED, Ordering::SeqCst);
            Err(caller_sigint_error(command))
        }
        Err(_) => result,
    }
}

fn mark_source_phase(sources: &mut [Value], store: &str, phase: &str, code: &str, message: &str) {
    if let Some(source) = sources
        .iter_mut()
        .find(|source| source.get("store").and_then(Value::as_str) == Some(store))
    {
        source["status"] = json!(if phase == "open" {
            peer_failure_status(code)
        } else {
            "failed"
        });
        source["phase"] = json!(phase);
        source["error"] = json!({"code": code, "message": message});
    }
}

fn mark_source_failures(sources: &mut [Value], store: &str, phase: &str, failures: &[Value]) {
    if let Some(source) = sources
        .iter_mut()
        .find(|source| source.get("store").and_then(Value::as_str) == Some(store))
    {
        source["status"] = json!("partial");
        source["phase"] = json!(phase);
        source["failures"] = json!(failures);
    }
}

fn format_peer_grep_session(
    record: &Value,
    machine: &str,
    export: &str,
    context: &RuntimeContext,
) -> Result<(Value, GrepRank), CliError> {
    let required_usize = |key: &str| -> Result<usize, CliError> {
        record
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| {
                CliError::new(
                    "protocol_error",
                    format!("grep_scan match has no valid {key}"),
                )
            })
    };
    let tape_id = record
        .get("tape_id")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::new("protocol_error", "grep_scan match has no tape_id"))?;
    let timestamp = record
        .get("timestamp")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::new("protocol_error", "grep_scan match has no timestamp"))?;
    let total_lines = required_usize("total_lines")?;
    let anchor_line = required_usize("anchor_line")?;
    let match_count = required_usize("match_count")?;
    let provenance_match_count = required_usize("provenance_match_count")?;
    let provenance_event_count = required_usize("provenance_event_count")?;
    let refs_up = required_usize("refs_up")?;
    let refs_down = required_usize("refs_down")?;
    let files_touched = record
        .get("files_touched")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            CliError::new(
                "protocol_error",
                "grep_scan match has no files_touched array",
            )
        })?;
    let line_count = context.peek_default_lines.max(1);
    let default_before = line_count * 3 / 4;
    let window_start = anchor_line.saturating_sub(default_before).max(1);
    let window_end = if total_lines == 0 {
        0
    } else {
        usize::min(
            total_lines,
            window_start.saturating_add(line_count).saturating_sub(1),
        )
    };
    let store = format!("{machine}/{export}");
    Ok((
        json!({
            "session_id": tape_id,
            "tape_id": tape_id,
            "timestamp": timestamp,
            "window_start": window_start,
            "window_end": window_end,
            "total_lines": total_lines,
            "confidence": match_count as f32,
            "refs_up": refs_up,
            "refs_down": refs_down,
            "files_touched": files_touched,
            "touches": [],
            "location": {"machine": machine, "store": store},
        }),
        GrepRank {
            provenance_match_count,
            match_count,
            provenance_event_count,
        },
    ))
}

fn merge_peer_grep_session(
    sessions: &mut Vec<Value>,
    ranks: &mut HashMap<String, GrepRank>,
    conflicts: &mut Vec<Value>,
    mut incoming: Value,
    rank: GrepRank,
    tape_id: &str,
) {
    let location = incoming["location"].clone();
    let store = location["store"].as_str().unwrap_or("peer").to_string();
    if let Some(existing_index) = sessions
        .iter()
        .position(|session| session.get("tape_id").and_then(Value::as_str) == Some(tape_id))
    {
        let existing_id = sessions[existing_index]["session_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let existing_rank = ranks.get(&existing_id).copied().unwrap_or_default();
        let same_timestamp = sessions[existing_index].get("timestamp") == incoming.get("timestamp");
        if existing_rank == rank && same_timestamp {
            let locations = sessions[existing_index]
                .get_mut("locations")
                .and_then(Value::as_array_mut)
                .expect("federated session locations initialized");
            locations.push(location);
            return;
        }
        let existing_location = sessions[existing_index]
            .get("location")
            .cloned()
            .unwrap_or_else(|| json!({"store":"local"}));
        let existing_store = existing_location["store"].as_str().unwrap_or("local");
        let qualified_existing = format!("{tape_id}@{existing_store}");
        if existing_id == tape_id {
            sessions[existing_index]["session_id"] = json!(qualified_existing);
            ranks.remove(&existing_id);
            ranks.insert(qualified_existing.clone(), existing_rank);
        }
        incoming["session_id"] = json!(format!("{tape_id}@{store}"));
        conflicts.push(json!({
            "tape_id": tape_id,
            "stores": [existing_store, store],
            "rank_keys": [
                {"provenance_match_count": existing_rank.provenance_match_count, "match_count": existing_rank.match_count, "provenance_event_count": existing_rank.provenance_event_count, "timestamp": sessions[existing_index]["timestamp"]},
                {"provenance_match_count": rank.provenance_match_count, "match_count": rank.match_count, "provenance_event_count": rank.provenance_event_count, "timestamp": incoming["timestamp"]},
            ],
        }));
    }
    let session_id = incoming["session_id"]
        .as_str()
        .unwrap_or(tape_id)
        .to_string();
    ranks.insert(session_id, rank);
    sessions.push(incoming);
}

fn peer_time_range(stats: &Value) -> Vec<String> {
    ["start", "end"]
        .iter()
        .filter_map(|key| stats.get("time_range")?.get(*key)?.as_str())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn valid_peer_time_range(stats: &Value) -> Option<Value> {
    let range = stats.get("time_range")?;
    let object = range.as_object()?;
    let start = object.get("start")?;
    let end = object.get("end")?;
    match (start, end) {
        (Value::Null, Value::Null) | (Value::String(_), Value::String(_)) => {}
        _ => return None,
    }
    Some(range.clone())
}

fn merge_grep_time_ranges(ranges: &[Vec<String>]) -> Value {
    let mut timestamps = ranges.iter().flatten().cloned().collect::<Vec<_>>();
    timestamps.sort();
    timestamps.dedup();
    if timestamps.is_empty() {
        json!({"start": Value::Null, "end": Value::Null})
    } else {
        json!({"start": timestamps.first(), "end": timestamps.last()})
    }
}

fn cmd_peek(_paths: &RepoPaths, context: &RuntimeContext, args: PeekArgs) -> Result<(), CliError> {
    print_context_conspicuity(context);
    if let Some(store_ref) = args.store.clone() {
        return cmd_peek_remote(context, args, &store_ref);
    }

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
        let anchor_line = default_peek_anchor_line(&indexes, &session_id, &rows);
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
    emit_query_result(
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

fn cmd_peek_remote(
    context: &RuntimeContext,
    args: PeekArgs,
    store_ref: &str,
) -> Result<(), CliError> {
    let (cancelled, terminal_state) = peer_cancellation_flag()?;
    let result = cmd_peek_remote_inner(context, args, store_ref, &cancelled, &terminal_state);
    finish_peer_query(result, &terminal_state, "remote peek")
}

fn cmd_peek_remote_inner(
    context: &RuntimeContext,
    args: PeekArgs,
    store_ref: &str,
    cancelled: &Arc<AtomicBool>,
    terminal_state: &AtomicU8,
) -> Result<(), CliError> {
    let query_deadline = Instant::now() + PEER_QUERY_TIMEOUT;
    let (machine, export, mut owner) =
        connect_remote_store(store_ref, "remote peek", query_deadline, cancelled)?;
    let session_id = args.session_id;
    if args.grep_filter.is_none() && args.start == Some(0) {
        return Err(CliError::new(
            "invalid_request",
            "--start is a 1-based line number",
        ));
    }

    let anchor_turn = if args.start.is_none() && args.grep_filter.is_none() {
        let rows = one_peer_response(
            &mut owner,
            PeerRequest::new(
                "dispatch_rows",
                vec![export.clone()],
                json!({"by_tape":[session_id]}),
            ),
            "dispatch_rows",
            query_deadline,
            cancelled,
        )?;
        let mut first_received: Option<(i64, String)> = None;
        for row in rows.data {
            if row.get("store").and_then(Value::as_str) != Some(store_ref)
                || row.get("tape_id").and_then(Value::as_str) != Some(session_id.as_str())
            {
                return Err(CliError::new(
                    "protocol_error",
                    "peer returned a dispatch row for a different store or tape",
                ));
            }
            let direction = row
                .get("direction")
                .and_then(Value::as_str)
                .ok_or_else(|| CliError::new("protocol_error", "dispatch row has no direction"))?;
            if !matches!(direction, "received" | "sent") {
                return Err(CliError::new(
                    "protocol_error",
                    "dispatch row has an invalid direction",
                ));
            }
            let turn = row
                .get("first_turn_index")
                .and_then(Value::as_i64)
                .ok_or_else(|| CliError::new("protocol_error", "dispatch row has no turn index"))?;
            let uuid = row
                .get("uuid")
                .and_then(Value::as_str)
                .ok_or_else(|| CliError::new("protocol_error", "dispatch row has no UUID"))?;
            if direction == "received"
                && first_received
                    .as_ref()
                    .is_none_or(|(best_turn, best_uuid)| {
                        turn < *best_turn || (turn == *best_turn && uuid < best_uuid.as_str())
                    })
            {
                first_received = Some((turn, uuid.to_string()));
            }
        }
        first_received.map(|(turn, _)| turn)
    } else {
        None
    };

    let mut request_args = serde_json::Map::new();
    request_args.insert("tape_id".into(), json!(session_id));
    if let Some(pattern) = args.grep_filter.as_deref() {
        request_args.insert("grep_filter".into(), json!(pattern));
        request_args.insert(
            "grep_context".into(),
            json!(context.peek_grep_context.max(1)),
        );
    } else if let Some(start) = args.start {
        request_args.insert("start".into(), json!(start));
        request_args.insert(
            "lines".into(),
            json!(args.lines.unwrap_or(context.peek_default_lines).max(1)),
        );
    } else {
        if let Some(turn) = anchor_turn {
            request_args.insert("anchor_turn".into(), json!(turn));
        }
        request_args.insert(
            "before".into(),
            json!(args.before.unwrap_or(context.peek_default_before)),
        );
        request_args.insert(
            "after".into(),
            json!(args.after.unwrap_or(context.peek_default_after)),
        );
    }

    let response = one_peer_response(
        &mut owner,
        PeerRequest::new("peek_lines", vec![export], Value::Object(request_args)),
        "peek_lines",
        query_deadline,
        cancelled,
    )
    .map_err(|error| {
        if error.code == "budget_exceeded" {
            CliError::new(
                "budget_exceeded",
                format!(
                    "{}; narrow --lines, the --before/--after window, or --grep-filter context",
                    error.message
                ),
            )
        } else {
            error
        }
    })?;

    if response.stats.get("tape_id").and_then(Value::as_str) != Some(session_id.as_str()) {
        return Err(CliError::new(
            "protocol_error",
            "peer returned peek statistics for a different tape",
        ));
    }
    let total_lines = response
        .stats
        .get("total_lines")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| CliError::new("protocol_error", "peek response has no total_lines"))?;
    let window_start = response
        .stats
        .get("window_start")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| CliError::new("protocol_error", "peek response has no window_start"))?;
    let window_end = response
        .stats
        .get("window_end")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| CliError::new("protocol_error", "peek response has no window_end"))?;
    let returned = response
        .stats
        .get("returned")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| CliError::new("protocol_error", "peek response has no returned count"))?;
    if response.data.len() != returned
        || window_start == 0
        || window_start > window_end
        || window_end > total_lines
    {
        return Err(CliError::new(
            "protocol_error",
            "peer returned inconsistent peek window statistics",
        ));
    }
    let timestamp = response
        .stats
        .get("timestamp")
        .cloned()
        .unwrap_or(Value::Null);
    if !timestamp.is_null() && !timestamp.is_string() {
        return Err(CliError::new(
            "protocol_error",
            "peek timestamp must be a string or null",
        ));
    }
    let mut previous_line = 0usize;
    let mut first_line = None;
    let mut content = Vec::with_capacity(response.data.len());
    for row in response.data {
        if row.get("tape_id").and_then(Value::as_str) != Some(session_id.as_str()) {
            return Err(CliError::new(
                "protocol_error",
                "peer returned a line for a different tape",
            ));
        }
        let line = row
            .get("line")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| CliError::new("protocol_error", "peek line has no valid number"))?;
        let text = row
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| CliError::new("protocol_error", "peek line has no text"))?;
        if line <= previous_line || line < window_start || line > window_end {
            return Err(CliError::new(
                "protocol_error",
                "peer returned unordered or out-of-range peek lines",
            ));
        }
        first_line.get_or_insert(line);
        previous_line = line;
        content.push(json!({"line":line,"text":text}));
    }
    if content.is_empty() {
        return Err(CliError::new("no_results", session_id));
    }
    if first_line != Some(window_start) || previous_line != window_end {
        return Err(CliError::new(
            "protocol_error",
            "peer returned peek lines that do not cover the stated window bounds",
        ));
    }

    let payload = json!({
        "query": {
            "command": "peek",
            "session_id": session_id,
            "start": args.start,
            "lines": args.lines,
            "before": args.before,
            "after": args.after,
            "grep_filter": args.grep_filter,
            "store": store_ref,
        },
        "session": {
            "session_id": session_id,
            "timestamp": timestamp,
            "window_start": window_start,
            "window_end": window_end,
            "total_lines": total_lines,
            "content": content,
            "location": {
                "machine": machine,
                "store": store_ref,
            },
        }
    });
    commit_peer_query_terminal(terminal_state, "remote peek")?;
    emit_query_result("peek", payload)
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

fn resolve_query_runtime_context(cwd: &Path) -> Result<RuntimeContext, CliError> {
    let home = home_dir()?;
    let config = load_effective_config_read_only(cwd, &home)
        .map_err(|err| CliError::new("config_error", err.to_string()))?;
    runtime_context_from_config(cwd, &home, config)
}

fn resolve_runtime_context_with_override(
    cwd: &Path,
    config_override: Option<&Path>,
) -> Result<RuntimeContext, CliError> {
    let home = home_dir()?;
    let config = load_effective_config_with_override(cwd, &home, config_override)
        .map_err(|err| CliError::new("config_error", err.to_string()))?;
    runtime_context_from_config(cwd, &home, config)
}

fn runtime_context_from_config(
    cwd: &Path,
    home: &Path,
    config: engram::config::EffectiveConfig,
) -> Result<RuntimeContext, CliError> {
    let tape_lookup_dirs = tape_lookup_dirs(cwd, &home, &config);
    let frozen_stores = resolved_frozen_store_paths(home, &config.additional_stores)?;
    Ok(RuntimeContext {
        config_path: config.path,
        db_path: config.db,
        tapes_dir: config.tapes_dir,
        frozen_stores,
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

fn resolved_frozen_store_paths(
    home: &Path,
    additional_stores: &[PathBuf],
) -> Result<Vec<PathBuf>, CliError> {
    let mut resolved = Vec::new();
    for frozen in load_frozen_stores(home)
        .map_err(|error| CliError::new("config_error", error.to_string()))?
    {
        if additional_stores.contains(&frozen.db) {
            if !resolved.contains(&frozen.db) {
                resolved.push(frozen.db);
            }
        } else {
            eprintln!(
                "warning: ignoring frozen store `{}` because it is not in additional_stores",
                frozen.db.display()
            );
        }
    }
    Ok(resolved)
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

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, RemoveKind};

    #[test]
    fn peer_selection_resolves_lists_and_all_deterministically() {
        let peer = || TopologyPeer {
            ssh: Some("host".into()),
            command: None,
            engram: "/usr/bin/engram".into(),
            exports: vec!["default".into()],
        };
        let peers = std::collections::BTreeMap::from([
            ("alpha".to_string(), peer()),
            ("beta".to_string(), peer()),
        ]);

        assert_eq!(
            select_peers("beta, alpha", &peers).expect("explicit peer list"),
            vec!["alpha".to_string(), "beta".to_string()]
        );
        assert_eq!(
            select_peers("all", &peers).expect("all peers"),
            vec!["alpha".to_string(), "beta".to_string()]
        );
        assert_eq!(
            select_peers("alpha,alpha", &peers)
                .expect_err("duplicate peers are ambiguous")
                .code,
            "invalid_peer_selection"
        );
        assert_eq!(
            select_peers("missing", &peers)
                .expect_err("unknown peers are rejected")
                .code,
            "peer_not_configured"
        );
    }

    #[test]
    fn fingerprint_candidates_sort_by_tape_id_then_path() {
        let expected = vec![
            ("a-tape".to_string(), PathBuf::from("/tapes/a-first")),
            ("a-tape".to_string(), PathBuf::from("/tapes/a-second")),
            ("b-tape".to_string(), PathBuf::from("/tapes/b")),
            ("c-tape".to_string(), PathBuf::from("/tapes/c")),
        ];
        let mut reversed = expected.iter().rev().cloned().collect::<Vec<_>>();

        sort_fingerprint_candidates(&mut reversed);

        assert_eq!(reversed, expected);
    }

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
    fn dispatch_extraction_records_normalized_tool_calls_at_current_message_turn() {
        let before = "00000000-0000-4000-8000-000000000001";
        let between = "00000000-0000-4000-8000-000000000002";
        let after = "00000000-0000-4000-8000-000000000003";
        let same_turn = "00000000-0000-4000-8000-000000000004";
        let earliest = "00000000-0000-4000-8000-000000000005";
        let ignored = "00000000-0000-4000-8000-000000000006";
        let ignored_non_args = "00000000-0000-4000-8000-000000000007";
        let transcript = [
            json!({
                "k": "tool.call",
                "source": format!("<engram-src id=\"{ignored_non_args}\"/>"),
                "args": {
                    "nested": {
                        "command": format!("<engram-src id=\"{before}\"/>"),
                        "repeat": format!("<engram-src id=\"{earliest}\"/>")
                    }
                }
            }),
            json!({
                "k": "msg.out",
                "t": "2026-07-29T11:00:00Z",
                "content": "first message"
            }),
            json!({
                "k": "tool.call",
                "t": "2026-07-29T11:00:01Z",
                "args": [format!("<engram-src id=\"{between}\"/>")]
            }),
            json!({
                "k": "msg.out",
                "t": "2026-07-29T11:00:02Z",
                "content": "second message"
            }),
            json!({
                "k": "tool.call",
                "t": "2026-07-29T11:00:03Z",
                "args": {
                    "command": format!("<engram-src id=\"{after}\"/>"),
                    "same_turn": format!("<engram-src id=\"{same_turn}\"/>")
                }
            }),
            json!({
                "k": "msg.in",
                "content": format!(
                    "<engram-src id=\"{same_turn}\"/> received at the same turn"
                )
            }),
            json!({
                "k": "msg.in",
                "content": format!("<engram-src id=\"{earliest}\"/> received later")
            }),
            json!({
                "k": "tool.result",
                "content": format!("<engram-src id=\"{ignored}\"/>")
            }),
        ]
        .into_iter()
        .map(|row| serde_json::to_string(&row).expect("serialize transcript row"))
        .collect::<Vec<_>>()
        .join("\n");

        let links = extract_dispatch_links_from_transcript(&transcript);
        let link = |uuid: &str| {
            links
                .iter()
                .find(|link| link.uuid == uuid)
                .unwrap_or_else(|| panic!("missing dispatch link {uuid}"))
        };

        assert_eq!(
            (link(before).first_turn_index, link(before).direction),
            (0, DispatchDirection::Sent)
        );
        assert_eq!(
            (link(between).first_turn_index, link(between).direction),
            (1, DispatchDirection::Sent)
        );
        assert_eq!(
            (link(after).first_turn_index, link(after).direction),
            (2, DispatchDirection::Sent)
        );
        assert_eq!(
            (link(same_turn).first_turn_index, link(same_turn).direction),
            (2, DispatchDirection::Received)
        );
        assert_eq!(
            (link(earliest).first_turn_index, link(earliest).direction),
            (0, DispatchDirection::Sent)
        );
        assert!(!links.iter().any(|link| link.uuid == ignored));
        assert!(!links.iter().any(|link| link.uuid == ignored_non_args));
    }

    #[test]
    fn dispatch_extraction_associates_tool_calls_with_matching_message_timestamps() {
        let initial = "10000000-0000-4000-8000-000000000001";
        let matching = "10000000-0000-4000-8000-000000000002";
        let differing = "10000000-0000-4000-8000-000000000003";
        let transcript = [
            json!({
                "k": "tool.call",
                "t": "2026-07-29T10:00:00Z",
                "args": {"marker": format!("<engram-src id=\"{initial}\"/>")}
            }),
            json!({
                "k": "msg.out",
                "t": "2026-07-29T10:00:01Z",
                "content": "first message"
            }),
            json!({
                "k": "tool.call",
                "t": "2026-07-29T10:00:01Z",
                "args": {"marker": format!("<engram-src id=\"{matching}\"/>")}
            }),
            json!({
                "k": "tool.call",
                "t": "2026-07-29T10:00:02Z",
                "args": {"marker": format!("<engram-src id=\"{differing}\"/>")}
            }),
        ]
        .into_iter()
        .map(|row| serde_json::to_string(&row).expect("serialize transcript row"))
        .collect::<Vec<_>>()
        .join("\n");

        let links = extract_dispatch_links_from_transcript(&transcript);
        let turn = |uuid: &str| {
            links
                .iter()
                .find(|link| link.uuid == uuid)
                .unwrap_or_else(|| panic!("missing dispatch link {uuid}"))
                .first_turn_index
        };

        assert_eq!(turn(initial), 0);
        assert_eq!(turn(matching), 0);
        assert_eq!(turn(differing), 1);
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
