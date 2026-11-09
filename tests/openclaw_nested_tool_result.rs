//! A store built by a reader that left nested OpenClaw tool-result output
//! empty gains that output once, as one added tape per file, and keeps every
//! existing tape, index row and cursor as it was.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use engram::RepoPaths;
use engram::ingest::has_pending_ingest;
use engram::tape::adapters::openclaw_jsonl_to_tape_jsonl;
use engram::tape::compress::decompress_jsonl;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SESSION: &str = "oc-nested-1";
const READ_OUT: &str = "fn quartzwren() {}";
const SHELL_OUT: &str = "lanternmoss shell line\n";
const ERROR_OUT: &str = "pewterfinch build failed";
const TAIL_OUT: &str = "copperlynx after the cursor\n";
const TEXT_OUT: &str = "brambleowl edited";
const PLAIN: &str = "thistlecrow plain question";
const REPLY: &str = "juniperfox assistant reply";
const NESTED_PHRASES: [&str; 4] = ["quartzwren", "lanternmoss", "pewterfinch", "copperlynx"];
/// Seconds of the rows whose output an older reader left empty.
const REPAIRED_SECONDS: [u32; 4] = [3, 5, 9, 12];

const NESTED_TYPE: &str = r#""type":"toolResult""#;
/// Same length as `NESTED_TYPE`, and a part type no reader takes output from.
const MASKED_TYPE: &str = r#""type":"tooLResult""#;

fn timestamp(second: u32) -> String {
    format!("2026-03-01T03:00:{second:02}Z")
}

fn row(id: &str, second: u32, message: Value) -> String {
    json!({"type": "message", "id": id, "timestamp": timestamp(second), "message": message})
        .to_string()
}

fn call(id: &str, second: u32, call_id: &str, tool: &str, arguments: Value) -> String {
    row(
        id,
        second,
        json!({
            "role": "assistant",
            "content": [{"type": "toolCall", "id": call_id, "name": tool, "arguments": arguments}],
        }),
    )
}

fn result(id: &str, second: u32, call_id: &str, tool: &str, error: bool, content: Value) -> String {
    row(
        id,
        second,
        json!({
            "role": "toolResult",
            "toolCallId": call_id,
            "toolName": tool,
            "isError": error,
            "content": content,
        }),
    )
}

fn nested(call_id: &str, output: &str) -> Value {
    json!([{"type": "toolResult", "toolCallId": call_id, "content": output, "text": output}])
}

fn long_reply(id: &str, second: u32) -> String {
    row(
        id,
        second,
        json!({"role": "assistant", "content": [
            {"type": "text", "text": format!("{REPLY} {}", "long ".repeat(200))},
        ]}),
    )
}

/// Lines of a session and whether each is a plain-string user message.
/// Both halves end with a long reply, so the cursor guard window of either
/// holds no row an older reader read differently.
fn session_lines() -> Vec<(String, bool)> {
    vec![
        (
            json!({"type": "session", "id": SESSION, "timestamp": timestamp(0)}).to_string(),
            false,
        ),
        (
            row("m1", 1, json!({"role": "user", "content": PLAIN})),
            true,
        ),
        (
            call("m2", 2, "c1", "read", json!({"path": "src/a.rs"})),
            false,
        ),
        (
            result("m3", 3, "c1", "read", false, nested("c1", READ_OUT)),
            false,
        ),
        (
            call("m4", 4, "c2", "bash", json!({"command": "cat src/b.rs"})),
            false,
        ),
        (
            result("m5", 5, "c2", "bash", false, nested("c2", SHELL_OUT)),
            false,
        ),
        (
            call(
                "m6",
                6,
                "c3",
                "edit",
                json!({"path": "src/c.rs", "old_string": "a", "new_string": "b"}),
            ),
            false,
        ),
        (
            result(
                "m7",
                7,
                "c3",
                "edit",
                false,
                json!([{"type": "text", "text": TEXT_OUT}]),
            ),
            false,
        ),
        (
            call("m8", 8, "c4", "bash", json!({"command": "make"})),
            false,
        ),
        (
            result("m9", 9, "c4", "bash", true, nested("c4", ERROR_OUT)),
            false,
        ),
        (long_reply("m10", 10), false),
        (
            call("m11", 11, "c5", "bash", json!({"command": "ls"})),
            false,
        ),
        (
            result("m12", 12, "c5", "bash", false, nested("c5", TAIL_OUT)),
            false,
        ),
        (long_reply("m13", 13), false),
    ]
}

/// Lines up to and including the first long reply.
const HALF: usize = 11;

#[derive(Clone, Copy)]
struct Mask {
    nested: bool,
    plain: bool,
}

const REAL: Mask = Mask {
    nested: false,
    plain: false,
};

/// A row no reader indexes, padded to `len` bytes.
fn ignored_row(len: usize) -> String {
    let empty = r#"{"type":"custom","pad":""}"#;
    format!(
        r#"{{"type":"custom","pad":"{}"}}"#,
        "x".repeat(len - empty.len())
    )
}

/// The session bytes, with what an older reader did not read masked by
/// same-length bytes that the current reader does not read either.
fn render(lines: &[(String, bool)], mask: Mask) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (line, plain) in lines {
        let line = if mask.plain && *plain {
            ignored_row(line.len())
        } else if mask.nested {
            line.replace(NESTED_TYPE, MASKED_TYPE)
        } else {
            line.clone()
        };
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

fn engram(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    fs::create_dir_all(home).expect("home dir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engram"));
    cmd.current_dir(cwd).args(args);
    if cfg!(windows) {
        cmd.env("USERPROFILE", home).env_remove("HOME");
    } else {
        cmd.env("HOME", home);
    }
    cmd.output().expect("engram runs")
}

fn engram_json(cwd: &Path, home: &Path, args: &[&str]) -> Value {
    let output = engram(cwd, home, args);
    assert!(
        output.status.success(),
        "engram {args:?} failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json stdout")
}

/// Number of indexed results holding `phrase`.
fn hits(repo: &Path, home: &Path, phrase: &str) -> u64 {
    let output = engram(repo, home, &["grep", phrase, "--count"]);
    if !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("no_results") {
        return 0;
    }
    let result: Value = serde_json::from_slice(&output.stdout).expect("json stdout");
    result["total"].as_u64().expect("total")
}

fn cursor_path(repo: &Path, source: &Path) -> PathBuf {
    let identity = fs::canonicalize(source).expect("canonical path");
    let key: String = Sha256::digest(identity.to_string_lossy().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    repo.join(".engram/cursors").join(format!("{key}.json"))
}

fn read_cursor(repo: &Path, source: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(cursor_path(repo, source)).expect("cursor"))
        .expect("cursor json")
}

/// Drop catch-up flags from the cursor, as a reader without them saved it.
fn forget(repo: &Path, source: &Path, flags: &[&str]) {
    let mut state = read_cursor(repo, source);
    for flag in flags {
        state.as_object_mut().expect("cursor").remove(*flag);
    }
    fs::write(cursor_path(repo, source), state.to_string()).expect("cursor write");
}

fn set_flag(repo: &Path, source: &Path, flag: &str) {
    let mut state = read_cursor(repo, source);
    state[flag] = json!(true);
    fs::write(cursor_path(repo, source), state.to_string()).expect("cursor write");
}

const NESTED_FLAG: &str = "openclaw_nested_tool_result";
const PLAIN_FLAG: &str = "openclaw_plain_string_user";
const NESTED_KEY: &str = "openclaw_nested_tool_result_catch_up";

/// Every tape file under `root`, by id.
fn tape_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut tapes = BTreeMap::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else if let Some(id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".jsonl.zst"))
            {
                tapes.insert(id.to_string(), fs::read(&path).expect("tape bytes"));
            }
        }
    }
    tapes
}

fn tape_events(bytes: &[u8]) -> Vec<Value> {
    decompress_jsonl(bytes)
        .expect("tape")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event"))
        .collect()
}

/// Every row of every table in the index, sorted.
fn index_rows(index: &Path) -> BTreeMap<String, Vec<String>> {
    let db = rusqlite::Connection::open(index).expect("index");
    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .expect("tables")
        .query_map([], |row| row.get(0))
        .expect("tables")
        .collect::<Result<_, _>>()
        .expect("tables");
    let mut dump = BTreeMap::new();
    for table in tables {
        let mut statement = db
            .prepare(&format!("SELECT * FROM \"{table}\""))
            .expect("select");
        let columns = statement.column_count();
        let mut rows: Vec<String> = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|i| {
                        row.get::<_, rusqlite::types::Value>(i)
                            .map(|v| format!("{v:?}"))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(|values| values.join("|"))
            })
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("rows");
        rows.sort();
        dump.insert(table, rows);
    }
    dump
}

fn append(path: &Path, line: &str) {
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open source");
    writeln!(file, "{line}").expect("append");
}

#[derive(Debug, PartialEq)]
struct Snapshot {
    tapes: BTreeMap<String, Vec<u8>>,
    cursor: Vec<u8>,
    index: BTreeMap<String, Vec<String>>,
}

struct Store {
    temp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    source: PathBuf,
}

impl Store {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).expect("repo");
        let source = repo.join("session.jsonl");
        Store {
            temp,
            home,
            repo,
            source,
        }
    }

    fn ingest(&self) -> Value {
        engram_json(
            &self.repo,
            &self.home,
            &["ingest", self.source.to_str().expect("utf8 path")],
        )
    }

    fn hits(&self, phrase: &str) -> u64 {
        hits(&self.repo, &self.home, phrase)
    }

    fn tapes(&self) -> BTreeMap<String, Vec<u8>> {
        tape_files(self.temp.path())
    }

    fn cursor(&self) -> Value {
        read_cursor(&self.repo, &self.source)
    }

    fn pending(&self) -> bool {
        let root = self.repo.join(".engram");
        let paths = RepoPaths {
            tapes: root.join("tapes"),
            objects: root.join("objects"),
            cursors: root.join("cursors"),
            root,
        };
        has_pending_ingest(&paths, &self.source)
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            tapes: self.tapes(),
            cursor: fs::read(cursor_path(&self.repo, &self.source)).expect("cursor"),
            index: index_rows(&self.home.join(".engram/index.sqlite")),
        }
    }

    /// The store a reader before 0.2.15 left: `lines` ingested with nested
    /// output unread (and plain-string user messages too, for a reader
    /// before 0.2.14), the real bytes back in place, and a cursor without
    /// the flags that reader did not know.
    fn built_by_older_reader(lines: &[(String, bool)], before_0214: bool) -> Self {
        let store = Store::new();
        let mask = Mask {
            nested: true,
            plain: before_0214,
        };
        fs::write(&store.source, render(lines, mask)).expect("masked");
        let first = store.ingest();
        assert_eq!(first["imported_tapes"], 1, "{first}");
        fs::write(&store.source, render(lines, REAL)).expect("real");
        let flags: &[&str] = if before_0214 {
            &[NESTED_FLAG, PLAIN_FLAG]
        } else {
            &[NESTED_FLAG]
        };
        forget(&store.repo, &store.source, flags);
        for phrase in NESTED_PHRASES {
            assert_eq!(store.hits(phrase), 0, "{phrase}");
        }
        store
    }
}

fn repaired_seconds(event: &Value) -> bool {
    REPAIRED_SECONDS
        .iter()
        .any(|second| event["t"] == timestamp(*second))
}

/// What a fresh ingest of `lines` writes for the outputs older readers left
/// empty: their `tool.result` rows and the reads derived from them.
fn expected_additions(lines: &[(String, bool)]) -> Vec<Value> {
    let bytes = render(lines, REAL);
    let tape =
        openclaw_jsonl_to_tape_jsonl(std::str::from_utf8(&bytes).expect("utf8")).expect("tape");
    tape.lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event"))
        .filter(|event| event["k"] != "meta" && repaired_seconds(event))
        .collect()
}

#[test]
fn a_fresh_ingest_reads_nested_output_and_needs_no_repair() {
    let s = Store::new();
    fs::write(&s.source, render(&session_lines(), REAL)).expect("source");

    let first = s.ingest();
    assert_eq!(first["imported_tapes"], 1, "{first}");
    assert!(first.get(NESTED_KEY).is_none(), "{first}");
    for phrase in NESTED_PHRASES {
        assert_eq!(s.hits(phrase), 1, "{phrase}");
    }
    assert_eq!(s.cursor()[NESTED_FLAG], true);
    assert!(!s.pending());

    let again = s.ingest();
    assert_eq!(again["imported_tapes"], 0, "{again}");
    assert!(again.get(NESTED_KEY).is_none(), "{again}");
}

#[test]
fn an_unchanged_file_from_a_0214_store_is_repaired_once() {
    let lines = session_lines();
    let s = Store::built_by_older_reader(&lines, false);
    let before = s.tapes();
    assert_eq!(before.len(), 1);
    let (old_id, old_bytes) = before.iter().next().expect("old tape");
    let old_events = tape_events(old_bytes);
    assert!(s.pending(), "an unrepaired cursor is pending work");

    let upgrade = s.ingest();
    assert_eq!(upgrade["status"], "ok", "{upgrade}");
    assert_eq!(
        upgrade[NESTED_KEY],
        json!({"mode": "one_time_nested_tool_result_v1", "sources": 1, "tapes": 1}),
        "{upgrade}"
    );
    assert!(upgrade.get("openclaw_catch_up").is_none(), "{upgrade}");
    assert_eq!(upgrade["imported_tapes"], 0, "{upgrade}");
    assert_eq!(s.cursor()[NESTED_FLAG], true);
    assert!(!s.pending());

    // Nested output is searchable, once.
    for phrase in NESTED_PHRASES {
        assert_eq!(s.hits(phrase), 1, "{phrase}");
    }
    assert_eq!(s.hits("brambleowl"), 1);
    assert_eq!(s.hits(REPLY), 1);

    // The old tape is untouched; one tape is added.
    let after = s.tapes();
    assert_eq!(after.len(), 2);
    assert_eq!(after.get(old_id), Some(old_bytes));
    let (_, added) = after
        .iter()
        .find(|(id, _)| *id != old_id)
        .expect("added tape");
    let added = tape_events(added);

    // It holds only the repaired results and their derived reads, with the
    // fields a fresh ingest writes.
    assert_eq!(added[0]["k"], "meta");
    assert_eq!(added[0]["source"]["harness"], "openclaw");
    assert_eq!(added[0]["source"]["session_id"], SESSION);
    let expected = expected_additions(&lines);
    assert_eq!(added[1..], expected[..]);
    let kinds: Vec<&str> = expected
        .iter()
        .map(|event| event["k"].as_str().expect("kind"))
        .collect();
    assert_eq!(
        kinds,
        [
            "tool.result",
            "code.read",
            "tool.result",
            "code.read",
            "tool.result",
            "tool.result"
        ]
    );

    // Each added result repairs the empty row of the same call.
    for event in &added[1..] {
        assert_eq!(event["source"]["harness"], "openclaw", "{event}");
        assert_eq!(event["source"]["session_id"], SESSION, "{event}");
        if event["k"] != "tool.result" {
            continue;
        }
        let empty: Vec<&Value> = old_events
            .iter()
            .filter(|old| old["k"] == "tool.result" && old["call_id"] == event["call_id"])
            .collect();
        assert_eq!(empty.len(), 1, "{event}");
        let empty = empty[0];
        assert_eq!(empty["stdout"], "", "{empty}");
        assert_eq!(empty["stderr"], "", "{empty}");
        for field in ["t", "call_id", "tool"] {
            assert_eq!(event[field], empty[field], "{field}: {event}");
        }
    }
}

#[test]
fn a_rerun_and_an_interrupted_run_add_nothing() {
    let s = Store::built_by_older_reader(&session_lines(), false);
    s.ingest();
    let upgraded = s.snapshot();

    let rerun = s.ingest();
    assert!(rerun.get(NESTED_KEY).is_none(), "{rerun}");
    assert_eq!(rerun["imported_tapes"], 0, "{rerun}");
    assert_eq!(s.snapshot(), upgraded);

    // Stopped after the added tape was indexed but before the cursor was
    // saved: the retry rebuilds the same tape, finds it indexed and ends where
    // an uninterrupted upgrade ends.
    forget(&s.repo, &s.source, &[NESTED_FLAG]);
    let retry = s.ingest();
    assert_eq!(
        retry[NESTED_KEY],
        json!({"mode": "one_time_nested_tool_result_v1", "sources": 1, "tapes": 0}),
        "{retry}"
    );
    assert_eq!(s.snapshot(), upgraded);
    for phrase in NESTED_PHRASES {
        assert_eq!(s.hits(phrase), 1, "{phrase}");
    }

    let third = s.ingest();
    assert!(third.get(NESTED_KEY).is_none(), "{third}");
    assert_eq!(s.snapshot(), upgraded);
}

#[test]
fn output_past_the_cursor_is_read_once_by_the_normal_path() {
    let lines = session_lines();
    let s = Store::built_by_older_reader(&lines[..HALF], false);
    fs::write(&s.source, render(&lines, REAL)).expect("full file");
    assert_eq!(s.hits("copperlynx"), 0);

    let upgrade = s.ingest();
    assert_eq!(upgrade[NESTED_KEY]["sources"], 1, "{upgrade}");
    assert_eq!(upgrade[NESTED_KEY]["tapes"], 1, "{upgrade}");
    assert_eq!(upgrade["imported_tapes"], 1, "{upgrade}");
    for phrase in NESTED_PHRASES {
        assert_eq!(s.hits(phrase), 1, "{phrase}");
    }

    // Across every tape, each output appears in exactly one result.
    let results: Vec<Value> = s
        .tapes()
        .values()
        .flat_map(|bytes| tape_events(bytes))
        .filter(|event| event["k"] == "tool.result")
        .collect();
    for output in [READ_OUT, SHELL_OUT, ERROR_OUT, TAIL_OUT, TEXT_OUT] {
        let holding = results
            .iter()
            .filter(|event| event["stdout"] == output || event["stderr"] == output)
            .count();
        assert_eq!(holding, 1, "{output}");
    }

    // Later appends are read from the cursor as usual.
    append(
        &s.source,
        &result("m14", 14, "c6", "bash", false, nested("c6", "ospreyfern\n")),
    );
    let next = s.ingest();
    assert!(next.get(NESTED_KEY).is_none(), "{next}");
    assert_eq!(next["imported_tapes"], 1, "{next}");
    assert_eq!(s.hits("ospreyfern"), 1);
    assert_eq!(s.hits("quartzwren"), 1);
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_is_reported_and_retried() {
    use std::os::unix::fs::PermissionsExt;

    let s = Store::built_by_older_reader(&session_lines(), false);
    let before = s.tapes();
    fs::set_permissions(&s.source, fs::Permissions::from_mode(0o000)).expect("chmod");
    if fs::read(&s.source).is_ok() {
        // Permissions do not bind this user; nothing here can be unreadable.
        fs::set_permissions(&s.source, fs::Permissions::from_mode(0o644)).expect("chmod");
        return;
    }

    let blocked = engram(
        &s.repo,
        &s.home,
        &["ingest", s.source.to_str().expect("utf8 path")],
    );
    fs::set_permissions(&s.source, fs::Permissions::from_mode(0o644)).expect("chmod");
    let blocked: Value = serde_json::from_slice(&blocked.stdout).expect("json stdout");
    assert!(blocked.get(NESTED_KEY).is_none(), "{blocked}");
    let failures = blocked["failures"].as_array().expect("failures");
    assert_eq!(failures.len(), 1, "{blocked}");
    assert_eq!(failures[0]["reason"], NESTED_KEY, "{blocked}");
    assert_eq!(s.tapes(), before);
    assert!(s.cursor().get(NESTED_FLAG).is_none());

    let retry = s.ingest();
    assert_eq!(retry[NESTED_KEY]["sources"], 1, "{retry}");
    assert_eq!(retry[NESTED_KEY]["tapes"], 1, "{retry}");
    assert_eq!(s.hits("quartzwren"), 1);
}

#[test]
fn a_guard_mismatch_reingests_without_an_added_tape() {
    let s = Store::built_by_older_reader(&session_lines(), false);
    // Change one byte of the reply that ends the ingested bytes.
    let mut bytes = fs::read(&s.source).expect("source");
    let at = bytes
        .windows(5)
        .rposition(|window| window == b"long ")
        .expect("reply");
    bytes[at] = b'L';
    fs::write(&s.source, bytes).expect("source");

    let run = s.ingest();
    assert!(run.get(NESTED_KEY).is_none(), "{run}");
    assert_eq!(run["imported_tapes"], 1, "{run}");
    assert_eq!(s.cursor()[NESTED_FLAG], true);
    for phrase in NESTED_PHRASES {
        assert_eq!(s.hits(phrase), 1, "{phrase}");
    }
}

#[test]
fn a_0213_store_gets_both_repairs_in_either_order() {
    let lines = session_lines();
    let all_phrases: Vec<&str> = NESTED_PHRASES
        .iter()
        .copied()
        .chain([PLAIN, REPLY])
        .collect();

    // Straight from a reader before 0.2.14.
    let direct = Store::built_by_older_reader(&lines, true);
    assert_eq!(direct.hits(PLAIN), 0);
    let upgrade = direct.ingest();
    assert_eq!(upgrade["openclaw_catch_up"]["sources"], 1, "{upgrade}");
    assert_eq!(upgrade["openclaw_catch_up"]["tapes"], 1, "{upgrade}");
    assert_eq!(upgrade[NESTED_KEY]["sources"], 1, "{upgrade}");
    assert_eq!(upgrade[NESTED_KEY]["tapes"], 1, "{upgrade}");
    assert_eq!(direct.tapes().len(), 3);

    // Through 0.2.14: only the user-message catch-up runs first, then this
    // repair on the next upgrade.
    let stepped = Store::new();
    let both = Mask {
        nested: true,
        plain: true,
    };
    let nested_only = Mask {
        nested: true,
        plain: false,
    };
    fs::write(&stepped.source, render(&lines, both)).expect("masked");
    stepped.ingest();
    fs::write(&stepped.source, render(&lines, nested_only)).expect("plain back");
    forget(&stepped.repo, &stepped.source, &[PLAIN_FLAG]);
    set_flag(&stepped.repo, &stepped.source, NESTED_FLAG);
    let first = stepped.ingest();
    assert_eq!(first["openclaw_catch_up"]["sources"], 1, "{first}");
    assert!(first.get(NESTED_KEY).is_none(), "{first}");
    fs::write(&stepped.source, render(&lines, REAL)).expect("real");
    forget(&stepped.repo, &stepped.source, &[NESTED_FLAG]);
    let second = stepped.ingest();
    assert!(second.get("openclaw_catch_up").is_none(), "{second}");
    assert_eq!(second[NESTED_KEY]["tapes"], 1, "{second}");

    let ids = |store: &Store| store.tapes().into_keys().collect::<BTreeSet<_>>();
    assert_eq!(ids(&direct), ids(&stepped));
    for phrase in all_phrases {
        assert_eq!(direct.hits(phrase), 1, "{phrase}");
        assert_eq!(stepped.hits(phrase), 1, "{phrase}");
    }
    assert_eq!(direct.cursor(), stepped.cursor());
}

struct Watch(std::process::Child);

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_watcher_repairs_an_unchanged_file_at_start() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let sessions = home.join("sessions");
    let store = temp.path().join("store");
    fs::create_dir_all(home.join(".engram")).expect("engram dir");
    fs::create_dir_all(&sessions).expect("sessions");
    let index = store.join("index.sqlite");
    let yaml = |path: &Path| serde_json::to_string(&path.to_string_lossy()).expect("path");
    fs::write(
        home.join(".engram/config.yml"),
        format!(
            "db: {}\ntapes_dir: {}\nwatch:\n  debounce_secs: 1\n  ingest_timeout_secs: 60\n  log: {}\n  sources:\n    - path: {}\n      pattern: \"*.jsonl\"\n",
            yaml(&index),
            yaml(&store.join("tapes")),
            yaml(&store.join("watch.log")),
            yaml(&sessions),
        ),
    )
    .expect("config");

    // The file has no bytes past its cursor, so no write will announce it.
    let source = sessions.join("session.jsonl");
    let lines = session_lines();
    let masked = Mask {
        nested: true,
        plain: false,
    };
    fs::write(&source, render(&lines, masked)).expect("masked");
    engram_json(&home, &home, &["ingest", sessions.to_str().expect("utf8")]);
    fs::write(&source, render(&lines, REAL)).expect("real");
    forget(&home, &source, &[NESTED_FLAG]);
    assert_eq!(hits(&home, &home, "quartzwren"), 0);
    let tapes_before = tape_files(&store).len();

    let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
    command
        .current_dir(&home)
        .arg("watch")
        .stdout(std::process::Stdio::null())
        .stderr(fs::File::create(store.join("watch.stderr")).expect("stderr file"));
    if cfg!(windows) {
        command.env("USERPROFILE", &home).env_remove("HOME");
    } else {
        command.env("HOME", &home);
    }
    let _watch = Watch(command.spawn().expect("watch spawns"));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while read_cursor(&home, &source)[NESTED_FLAG] != true {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for startup repair; watch log:\n{}\nwatch stderr:\n{}",
            fs::read_to_string(store.join("watch.log")).unwrap_or_default(),
            fs::read_to_string(store.join("watch.stderr")).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    for phrase in NESTED_PHRASES {
        assert_eq!(hits(&home, &home, phrase), 1, "{phrase}");
    }
    assert_eq!(tape_files(&store).len(), tapes_before + 1);
}
