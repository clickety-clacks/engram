//! Ingesting an OpenClaw or Cursor session in pieces indexes the same reads
//! and edits as ingesting it once, even when a call and its result land in
//! different pieces.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use engram::tape::compress::decompress_jsonl;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const EVIDENCE_KEYS: [&str; 8] = [
    "k",
    "t",
    "file",
    "range",
    "range_basis",
    "text",
    "before_text",
    "after_text",
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Reader {
    OpenClaw,
    Cursor,
}

impl Reader {
    /// File names steer reader detection the way real session paths do.
    fn file_name(self) -> &'static str {
        match self {
            Reader::OpenClaw => "openclaw-session.jsonl",
            Reader::Cursor => "cursor-session.jsonl",
        }
    }
}

// OpenClaw rows.

fn oc_ts(second: u32) -> String {
    format!("2026-03-01T03:{:02}:{:02}Z", second / 60, second % 60)
}

fn oc_row(id: &str, second: u32, message: Value) -> String {
    json!({"type": "message", "id": id, "timestamp": oc_ts(second), "message": message}).to_string()
}

fn oc_call(second: u32, call_id: &str, tool: &str, arguments: Value) -> String {
    oc_row(
        &format!("call-{call_id}-{second}"),
        second,
        json!({
            "role": "assistant",
            "content": [{"type": "toolCall", "id": call_id, "name": tool, "arguments": arguments}],
        }),
    )
}

fn oc_result(second: u32, call_id: &str, tool: &str, error: bool, text: &str) -> String {
    oc_row(
        &format!("result-{call_id}-{second}"),
        second,
        json!({
            "role": "toolResult",
            "toolCallId": call_id,
            "toolName": tool,
            "isError": error,
            "content": [{"type": "text", "text": text}],
        }),
    )
}

fn oc_ignored_entry(entry_type: &str) -> String {
    json!({"type": entry_type, "timestamp": oc_ts(12)}).to_string()
}

/// A read, an edit, a shell read, and a read whose first result is an error
/// and whose id later gets a success.
fn openclaw_lines() -> Vec<String> {
    vec![
        json!({"type": "session", "id": "oc-pairing-1", "timestamp": oc_ts(0)}).to_string(),
        oc_row("u1", 1, json!({"role": "user", "content": "read and fix"})),
        oc_call(2, "c1", "read", json!({"path": "/repo/src/a.rs"})),
        oc_result(3, "c1", "read", false, "fn alpha() {}\nfn beta() {}"),
        oc_call(
            4,
            "c2",
            "edit",
            json!({"path": "/repo/src/a.rs", "old_string": "alpha", "new_string": "gamma"}),
        ),
        oc_result(5, "c2", "edit", false, "edited"),
        oc_call(6, "c3", "bash", json!({"command": "cat /repo/src/b.rs"})),
        oc_result(7, "c3", "bash", false, "pub fn bee() {}\n"),
        oc_call(8, "c4", "read", json!({"path": "/repo/src/d.rs"})),
        oc_result(9, "c4", "read", true, "permission denied"),
        oc_result(10, "c4", "read", false, "fn delta() {}"),
        oc_row(
            "a1",
            11,
            json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}),
        ),
    ]
}

fn openclaw_r6_lines() -> Vec<String> {
    vec![
        json!({"type": "session", "id": "oc-r6", "timestamp": oc_ts(0)}).to_string(),
        oc_row(
            "u1",
            1,
            json!({"role": "user", "content": "read two files"}),
        ),
        oc_call(2, "prior", "read", json!({"path": "/repo/src/a.rs"})),
        oc_result(3, "prior", "read", false, "fn alpha() {}"),
        oc_call(4, "pending", "read", json!({"path": "/repo/src/b.rs"})),
        oc_result(5, "pending", "read", false, "fn beta() {}"),
    ]
}

// Cursor rows.

const CURSOR_SESSION: &str = "cursor-pairing-1";
const CURSOR_INIT_TS: &str = "2026-02-22T00:00:00Z";

fn cursor_init() -> String {
    json!({"timestamp": CURSOR_INIT_TS, "type": "system", "subtype": "init",
        "session_id": CURSOR_SESSION, "model": "gpt-5", "cwd": "/repo"})
    .to_string()
}

fn cursor_user() -> String {
    json!({"timestamp": "2026-02-22T00:00:05Z", "type": "user", "session_id": CURSOR_SESSION,
        "message": {"role": "user", "content": "look around"}})
    .to_string()
}

fn cursor_started(call_id: &str, tool_call: Value) -> String {
    json!({"type": "tool_call", "subtype": "started", "session_id": CURSOR_SESSION,
        "call_id": call_id, "tool_call": tool_call})
    .to_string()
}

fn cursor_completed(call_id: &str, tool_call: Value) -> String {
    json!({"type": "tool_call", "subtype": "completed", "session_id": CURSOR_SESSION,
        "call_id": call_id, "tool_call": tool_call})
    .to_string()
}

fn cursor_read_started(call_id: &str, path: &str) -> String {
    cursor_started(call_id, json!({"readToolCall": {"args": {"path": path}}}))
}

fn cursor_read_completed(call_id: &str, content: &str) -> String {
    cursor_completed(
        call_id,
        json!({"readToolCall": {"result": {"success": {"content": content}}}}),
    )
}

fn cursor_shell_started(call_id: &str, command: &str) -> String {
    cursor_started(
        call_id,
        json!({"function": {"name": "bash", "arguments": {"command": command}}}),
    )
}

fn cursor_shell_completed(call_id: &str, output: &str) -> String {
    cursor_completed(
        call_id,
        json!({"function": {"name": "bash", "result": {"success": {"output": output}}}}),
    )
}

/// A timestamped init with a cwd and a timestamped user message, then
/// untimestamped calls: a read, a relative shell read and a write.
fn cursor_lines() -> Vec<String> {
    vec![
        cursor_init(),
        cursor_user(),
        cursor_read_started("r1", "/repo/src/main.rs"),
        cursor_read_completed("r1", "fn main() {}"),
        cursor_shell_started("s1", "cat src/lib.rs"),
        cursor_shell_completed("s1", "pub fn lib() {}\n"),
        cursor_started(
            "w1",
            json!({"writeToolCall": {"args": {"path": "/repo/src/new.rs", "content": "x"}}}),
        ),
        cursor_completed(
            "w1",
            json!({"writeToolCall": {"result": {"success": {"path": "/repo/src/new.rs"}}}}),
        ),
        json!({"type": "assistant", "session_id": CURSOR_SESSION,
            "message": {"role": "assistant", "content": "done"}})
        .to_string(),
    ]
}

fn lines_for(reader: Reader) -> Vec<String> {
    match reader {
        Reader::OpenClaw => openclaw_lines(),
        Reader::Cursor => cursor_lines(),
    }
}

// Store.

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

fn tape_events(bytes: &[u8]) -> Vec<Value> {
    decompress_jsonl(bytes)
        .expect("tape")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event"))
        .collect()
}

/// The fields that identify a piece of evidence, without the session.
fn evidence_key(event: &Value) -> String {
    let key: BTreeMap<&str, &Value> = EVIDENCE_KEYS
        .iter()
        .filter_map(|field| event.get(*field).map(|value| (*field, value)))
        .collect();
    serde_json::to_string(&key).expect("key")
}

fn is_evidence(event: &Value) -> bool {
    matches!(event["k"].as_str(), Some("code.read" | "code.edit"))
}

struct Store {
    temp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    source: PathBuf,
}

impl Store {
    fn new(reader: Reader) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).expect("repo");
        let source = repo.join(reader.file_name());
        fs::write(&source, "").expect("source");
        Store {
            temp: temp,
            home,
            repo,
            source,
        }
    }

    fn append(&self, lines: &[String]) {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&self.source)
            .expect("open source");
        for line in lines {
            writeln!(file, "{line}").expect("append");
        }
    }

    fn ingest(&self) -> Value {
        let args = ["ingest", self.source.to_str().expect("utf8 path")];
        let output = engram(&self.repo, &self.home, &args);
        assert!(
            output.status.success(),
            "engram {args:?} failed\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("json stdout")
    }

    fn cursor_path(&self) -> PathBuf {
        let identity = fs::canonicalize(&self.source).expect("canonical path");
        let key: String = Sha256::digest(identity.to_string_lossy().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.repo
            .join(".engram/cursors")
            .join(format!("{key}.json"))
    }

    fn cursor(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.cursor_path()).expect("cursor"))
            .expect("cursor json")
    }

    fn pending_call_ids(&self) -> Vec<String> {
        let cursor = self.cursor();
        let calls = cursor["pending_calls"]["calls"]
            .as_array()
            .unwrap_or_else(|| panic!("pending_calls.calls in {cursor}"));
        calls
            .iter()
            .map(|call| call["call_id"].as_str().expect("call_id").to_string())
            .collect()
    }

    fn indexed_tape_ids(&self) -> BTreeSet<String> {
        let db = rusqlite::Connection::open(self.home.join(".engram/index.sqlite")).expect("index");
        let mut statement = db.prepare("SELECT tape_id FROM tapes").expect("select");
        statement
            .query_map([], |row| row.get(0))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("rows")
    }

    fn tape_path(&self, tape_id: &str) -> PathBuf {
        let file_name = format!("{tape_id}.jsonl.zst");
        let mut dirs = vec![self.temp.path().to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(dir).expect("read dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.file_name().and_then(|name| name.to_str())
                    == Some(file_name.as_str())
                {
                    return path;
                }
            }
        }
        panic!("tape file for {tape_id} not found");
    }

    fn tape_events_for_id(&self, tape_id: &str) -> Vec<Value> {
        tape_events(&fs::read(self.tape_path(tape_id)).expect("tape bytes"))
    }

    fn remove_tape_file(&self, tape_id: &str) {
        fs::remove_file(self.tape_path(tape_id)).expect("remove tape file");
    }

    /// Events of every indexed tape, by tape id.
    fn tapes(&self) -> BTreeMap<String, Vec<Value>> {
        let indexed = self.indexed_tape_ids();
        let mut tapes = BTreeMap::new();
        let mut dirs = vec![self.temp.path().to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(&dir).expect("read dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    dirs.push(path);
                } else if let Some(id) = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_suffix(".jsonl.zst"))
                    && indexed.contains(id)
                {
                    let events = tape_events(&fs::read(&path).expect("tape bytes"));
                    tapes.insert(id.to_string(), events);
                }
            }
        }
        assert_eq!(
            tapes.len(),
            indexed.len(),
            "every indexed tape has a tape file"
        );
        tapes
    }

    /// The multiset of indexed evidence keys, sorted.
    fn evidence(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .tapes()
            .values()
            .flatten()
            .filter(|event| is_evidence(event))
            .map(evidence_key)
            .collect();
        keys.sort();
        keys
    }

    /// Events of tapes indexed since `before`.
    fn new_events(&self, before: &BTreeSet<String>) -> Vec<Value> {
        self.tapes()
            .into_iter()
            .filter(|(id, _)| !before.contains(id))
            .flat_map(|(_, events)| events)
            .collect()
    }
}

/// The evidence one ingest of `lines` indexes.
fn one_pass(reader: Reader, lines: &[String]) -> Vec<String> {
    let store = Store::new(reader);
    store.append(lines);
    store.ingest();
    store.evidence()
}

fn files(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|event| is_evidence(event))
        .map(|event| event["file"].as_str().expect("file").to_string())
        .collect()
}

fn openclaw_store_with_pending_read() -> (Store, Vec<String>) {
    let lines = openclaw_r6_lines();
    let store = Store::new(Reader::OpenClaw);
    store.append(&lines[..2]);
    store.ingest();
    store.append(&lines[2..4]);
    store.ingest();
    store.append(&lines[4..5]);
    store.ingest();
    assert_eq!(store.evidence(), one_pass(Reader::OpenClaw, &lines[..5]));
    (store, lines)
}

#[test]
fn every_two_piece_split_indexes_what_one_ingest_does() {
    for reader in [Reader::OpenClaw, Reader::Cursor] {
        let lines = lines_for(reader);
        let expected = one_pass(reader, &lines);
        assert!(expected.len() >= 3, "{reader:?} fixture has evidence");
        for split in 1..lines.len() {
            let store = Store::new(reader);
            store.append(&lines[..split]);
            store.ingest();
            store.append(&lines[split..]);
            store.ingest();
            assert_eq!(
                store.evidence(),
                expected,
                "{reader:?} split at line {split}"
            );
        }
    }
}

#[test]
fn ingesting_after_every_line_indexes_what_one_ingest_does() {
    for reader in [Reader::OpenClaw, Reader::Cursor] {
        let lines = lines_for(reader);
        let expected = one_pass(reader, &lines);
        let store = Store::new(reader);
        for line in &lines {
            store.append(std::slice::from_ref(line));
            store.ingest();
        }
        assert_eq!(store.evidence(), expected, "{reader:?}");
    }
}

#[test]
fn a_cursor_tail_after_the_init_takes_the_init_time_and_cwd() {
    let lines = cursor_lines();
    let store = Store::new(Reader::Cursor);
    store.append(&lines[..2]);
    store.ingest();
    let before = store.indexed_tape_ids();
    assert_eq!(before.len(), 1, "the first piece is indexed");

    store.append(&lines[2..]);
    store.ingest();
    let tail = store.new_events(&before);
    assert!(!tail.is_empty(), "the tail is indexed");
    for event in &tail {
        assert_eq!(event["t"], CURSOR_INIT_TS, "{event}");
    }
    let tail_files = files(&tail);
    assert!(
        tail_files.contains(&"/repo/src/lib.rs".to_string()),
        "{tail_files:?}"
    );
    assert!(
        !tail_files.contains(&"src/lib.rs".to_string()),
        "{tail_files:?}"
    );
}

#[test]
fn a_cursor_tail_with_only_a_carried_read_result_has_full_read_coverage() {
    let store = Store::new(Reader::Cursor);
    store.append(&[
        cursor_init(),
        cursor_user(),
        cursor_read_started("r1", "/repo/src/main.rs"),
    ]);
    store.ingest();
    let before = store.indexed_tape_ids();

    store.append(&[cursor_read_completed("r1", "fn main() {}")]);
    store.ingest();
    let tail = store.new_events(&before);
    assert_eq!(files(&tail), vec!["/repo/src/main.rs".to_string()]);
    let meta = tail
        .iter()
        .find(|event| event["k"] == "meta")
        .expect("meta");
    assert_eq!(meta["coverage.read"], "full", "{meta}");
}

#[test]
fn the_oldest_of_500_open_calls_pairs_after_a_commit() {
    for reader in [Reader::OpenClaw, Reader::Cursor] {
        let store = Store::new(reader);
        let (head, calls, oldest_result): (Vec<String>, Vec<String>, String) = match reader {
            Reader::OpenClaw => (
                openclaw_lines()[..2].to_vec(),
                (0..500)
                    .map(|i| {
                        oc_call(
                            20 + i,
                            &format!("open{i}"),
                            "read",
                            json!({"path": format!("/repo/src/f{i}.rs")}),
                        )
                    })
                    .collect(),
                oc_result(600, "open0", "read", false, "fn oldest() {}"),
            ),
            Reader::Cursor => (
                vec![cursor_init(), cursor_user()],
                (0..500)
                    .map(|i| {
                        cursor_read_started(&format!("open{i}"), &format!("/repo/src/f{i}.rs"))
                    })
                    .collect(),
                cursor_read_completed("open0", "fn oldest() {}"),
            ),
        };
        store.append(&head);
        store.append(&calls);
        store.ingest();
        assert_eq!(store.pending_call_ids().len(), 500, "{reader:?}");
        let before = store.indexed_tape_ids();

        store.append(&[oldest_result]);
        store.ingest();
        assert_eq!(
            files(&store.new_events(&before)),
            vec!["/repo/src/f0.rs".to_string()],
            "{reader:?}"
        );
        let left = store.pending_call_ids();
        assert_eq!(left.len(), 499, "{reader:?}");
        assert!(!left.contains(&"open0".to_string()), "{reader:?}");
    }
}

#[test]
fn a_legacy_cursor_seeds_its_calls_from_a_prefix_with_a_damaged_line() {
    for reader in [Reader::OpenClaw, Reader::Cursor] {
        let (head, call, result) = match reader {
            Reader::OpenClaw => (
                openclaw_lines()[..2].to_vec(),
                oc_call(2, "c1", "read", json!({"path": "/repo/src/a.rs"})),
                oc_result(3, "c1", "read", false, "fn alpha() {}"),
            ),
            Reader::Cursor => (
                vec![cursor_init(), cursor_user()],
                cursor_read_started("c1", "/repo/src/a.rs"),
                cursor_read_completed("c1", "fn alpha() {}"),
            ),
        };
        let mut lines = head.clone();
        lines.push(call.clone());
        lines.push(result.clone());
        let expected = one_pass(reader, &lines);

        let store = Store::new(reader);
        store.append(&head);
        store.append(&["{\"damaged\": ".to_string(), call]);
        store.ingest();
        // A reader before 0.2.16 saved no pending calls.
        let mut cursor = store.cursor();
        cursor
            .as_object_mut()
            .expect("cursor")
            .remove("pending_calls");
        fs::write(store.cursor_path(), cursor.to_string()).expect("cursor write");

        store.append(&[result]);
        let report = store.ingest();
        assert!(
            report["failures"].as_array().is_none_or(Vec::is_empty),
            "{report}"
        );
        assert_eq!(store.evidence(), expected, "{reader:?}");
        assert!(store.pending_call_ids().is_empty(), "{reader:?}");
    }
}

#[test]
fn a_balanced_piece_saves_an_empty_pending_calls_field() {
    for reader in [Reader::OpenClaw, Reader::Cursor] {
        let lines = lines_for(reader);
        let store = Store::new(reader);
        store.append(&lines[..4]);
        store.ingest();
        let cursor = store.cursor();
        assert_eq!(
            cursor["pending_calls"]["calls"],
            json!([]),
            "{reader:?} {cursor}"
        );
        if reader == Reader::Cursor {
            assert_eq!(cursor["pending_calls"]["session_cwd"], "/repo", "{cursor}");
            assert_eq!(
                cursor["pending_calls"]["first_timestamp"], CURSOR_INIT_TS,
                "{cursor}"
            );
        }
    }
}

#[test]
fn a_full_reread_starts_with_no_pending_calls() {
    for reader in [Reader::OpenClaw, Reader::Cursor] {
        let (head, call, orphan) = match reader {
            Reader::OpenClaw => (
                openclaw_lines()[..2].to_vec(),
                oc_call(2, "c1", "read", json!({"path": "/repo/src/a.rs"})),
                oc_result(3, "c1", "read", false, "fn alpha() {}"),
            ),
            Reader::Cursor => (
                vec![cursor_init(), cursor_user()],
                cursor_read_started("c1", "/repo/src/a.rs"),
                cursor_read_completed("c1", "fn alpha() {}"),
            ),
        };
        let store = Store::new(reader);
        store.append(&head);
        store.append(&[call]);
        store.ingest();
        assert_eq!(
            store.pending_call_ids(),
            vec!["c1".to_string()],
            "{reader:?}"
        );
        let before = store.indexed_tape_ids();

        // Rewritten bytes fail the cursor guard, so the file is read again
        // from the start, where c1 was never called.
        fs::write(&store.source, "").expect("rewrite");
        let mut rewritten = head.clone();
        rewritten[1] = rewritten[1].replace("2026", "2025");
        rewritten.push(orphan);
        store.append(&rewritten);
        store.ingest();
        let reread = store.new_events(&before);
        assert!(!reread.is_empty(), "{reader:?} reread is indexed");
        assert!(files(&reread).is_empty(), "{reader:?} {reread:?}");
        assert!(store.pending_call_ids().is_empty(), "{reader:?}");
    }
}

#[test]
fn a_tail_without_evidence_saves_its_open_call() {
    for reader in [Reader::OpenClaw, Reader::Cursor] {
        let lines = lines_for(reader);
        let store = Store::new(reader);
        store.append(&lines[..2]);
        store.ingest();
        // The tail holds only a call, so it indexes no evidence.
        store.append(&lines[2..3]);
        store.ingest();
        let size = fs::metadata(&store.source).expect("source").len();
        assert_eq!(store.cursor()["byte_cursor"], size, "{reader:?}");
        assert_eq!(store.pending_call_ids().len(), 1, "{reader:?}");

        store.append(&lines[3..]);
        store.ingest();
        assert_eq!(store.evidence(), one_pass(reader, &lines), "{reader:?}");
    }
}

#[test]
fn a_replayed_tail_converts_to_the_same_tape_and_cursor() {
    for reader in [Reader::OpenClaw, Reader::Cursor] {
        let lines = lines_for(reader);
        let split = 3;
        let store = Store::new(reader);
        store.append(&lines[..split]);
        store.ingest();
        let saved = fs::read(store.cursor_path()).expect("cursor");

        store.append(&lines[split..]);
        store.ingest();
        let after = fs::read(store.cursor_path()).expect("cursor");
        let tapes = store.indexed_tape_ids();

        fs::write(store.cursor_path(), &saved).expect("restore");
        let replay = store.ingest();
        assert_eq!(replay["imported_tapes"], 0, "{reader:?} {replay}");
        assert_eq!(store.indexed_tape_ids(), tapes, "{reader:?}");
        assert_eq!(
            fs::read(store.cursor_path()).expect("cursor"),
            after,
            "{reader:?}"
        );
        assert_eq!(store.evidence(), one_pass(reader, &lines), "{reader:?}");
    }
}

/// A cursor save lost after its tape was indexed: the retry converts both
/// pieces from the state before the lost one, so the lost piece's evidence
/// is indexed twice. Readers before 0.2.16 do the same.
#[test]
fn a_lost_cursor_save_indexes_the_lost_piece_twice() {
    for reader in [Reader::OpenClaw, Reader::Cursor] {
        let lines = lines_for(reader);
        // The prefix ends before the first call; the lost piece holds the
        // first call and its result; the retried piece holds the rest.
        let (prefix, lost) = (2, 4);
        let store = Store::new(reader);
        store.append(&lines[..prefix]);
        store.ingest();
        let saved = fs::read(store.cursor_path()).expect("cursor");

        store.append(&lines[prefix..lost]);
        store.ingest();
        fs::write(store.cursor_path(), &saved).expect("restore");

        store.append(&lines[lost..]);
        store.ingest();

        let mut expected = one_pass(reader, &lines);
        expected.extend(one_pass(reader, &lines[..lost]));
        expected.sort();
        assert_eq!(store.evidence(), expected, "{reader:?}");
    }
}

#[test]
fn r6_openclaw_ignored_entry_tails_keep_pending_calls_without_rereading_prefix() {
    let ignored = [
        ("custom", vec![oc_ignored_entry("custom")]),
        ("custom_message", vec![oc_ignored_entry("custom_message")]),
        ("compaction", vec![oc_ignored_entry("compaction")]),
        ("model_change", vec![oc_ignored_entry("model_change")]),
        (
            "thinking_level_change",
            vec![oc_ignored_entry("thinking_level_change")],
        ),
        (
            "all ignored entry types",
            [
                "custom",
                "custom_message",
                "compaction",
                "model_change",
                "thinking_level_change",
            ]
            .into_iter()
            .map(oc_ignored_entry)
            .collect(),
        ),
    ];

    for (name, tail) in ignored {
        let (store, lines) = openclaw_store_with_pending_read();
        let expected_lines = [&lines[..5], tail.as_slice(), &lines[5..]].concat();
        let expected = one_pass(Reader::OpenClaw, &expected_lines);
        let cursor_before = store.cursor();
        let tapes_before = store.indexed_tape_ids();
        let evidence_before = store.evidence();

        store.append(&tail);
        let report = store.ingest();
        assert_eq!(report["imported_tapes"], 0, "{name}: {report}");
        assert_eq!(store.cursor(), cursor_before, "{name}");
        assert_eq!(store.indexed_tape_ids(), tapes_before, "{name}");
        assert_eq!(store.evidence(), evidence_before, "{name}");
        if cursor_before.get("pending_calls").is_some() {
            assert_eq!(store.pending_call_ids(), vec!["pending"], "{name}");
        }

        store.append(&lines[5..]);
        store.ingest();
        assert_eq!(store.evidence(), expected, "{name}");
        assert!(store.pending_call_ids().is_empty(), "{name}");
    }
}

#[test]
fn r6_openclaw_ignored_entry_tails_allow_empty_and_whitespace_lines() {
    let (store, lines) = openclaw_store_with_pending_read();
    let tail = vec![
        oc_ignored_entry("custom"),
        String::new(),
        " \t".to_string(),
        oc_ignored_entry("compaction"),
    ];
    let expected_lines = [&lines[..5], tail.as_slice(), &lines[5..]].concat();
    let expected = one_pass(Reader::OpenClaw, &expected_lines);
    let cursor_before = store.cursor();
    let tapes_before = store.indexed_tape_ids();

    store.append(&tail);
    let report = store.ingest();
    assert_eq!(report["imported_tapes"], 0, "{report}");
    assert_eq!(store.cursor(), cursor_before);
    assert_eq!(store.indexed_tape_ids(), tapes_before);
    if cursor_before.get("pending_calls").is_some() {
        assert_eq!(store.pending_call_ids(), vec!["pending"]);
    }

    store.append(&lines[5..]);
    store.ingest();
    assert_eq!(store.evidence(), expected);
}

#[test]
fn r6_openclaw_tails_outside_the_ignored_entry_rule_keep_the_full_reread() {
    let cases = vec![
        (
            "another row type",
            vec![json!({"type": "frobnicate"}).to_string()],
        ),
        ("damaged line", vec!["{damaged".to_string()]),
        ("blank only", vec![String::new(), " \t".to_string()]),
        (
            "ignored plus damaged",
            vec![oc_ignored_entry("custom"), "{damaged".to_string()],
        ),
        (
            "ignored plus another row type",
            vec![
                oc_ignored_entry("custom"),
                json!({"type": "frobnicate"}).to_string(),
            ],
        ),
        (
            "ignored plus non-object value",
            vec![oc_ignored_entry("custom"), "[1]".to_string()],
        ),
    ];

    for (name, tail) in cases {
        let (store, _) = openclaw_store_with_pending_read();
        let cursor_before = store.cursor();
        let tapes_before = store.indexed_tape_ids();
        let evidence_before = store.evidence();

        store.append(&tail);
        let report = store.ingest();
        assert!(
            report["imported_tapes"].as_u64().unwrap_or(0) > 0,
            "{name}: {report}"
        );
        assert_ne!(
            store.cursor()["tape_id"],
            cursor_before["tape_id"],
            "{name}"
        );
        assert!(store.evidence().len() > evidence_before.len(), "{name}");
        let reread = store.new_events(&tapes_before);
        assert!(
            files(&reread).contains(&"/repo/src/a.rs".to_string()),
            "{name}: {reread:?}"
        );
    }
}

#[test]
fn r6_legacy_openclaw_cursor_seeds_across_an_ignored_tail_and_pairs_once() {
    let (store, lines) = openclaw_store_with_pending_read();
    let expected = one_pass(Reader::OpenClaw, &lines);
    let mut legacy_cursor = store.cursor();
    legacy_cursor
        .as_object_mut()
        .expect("cursor object")
        .remove("pending_calls");
    fs::write(store.cursor_path(), legacy_cursor.to_string()).expect("legacy cursor write");
    let cursor_before = store.cursor();
    let tapes_before = store.indexed_tape_ids();

    store.append(&[oc_ignored_entry("custom_message")]);
    let report = store.ingest();
    assert_eq!(report["imported_tapes"], 0, "{report}");
    assert_eq!(store.cursor(), cursor_before);
    assert_eq!(store.indexed_tape_ids(), tapes_before);
    assert!(store.cursor().get("pending_calls").is_none());

    store.append(&lines[5..]);
    store.ingest();
    assert_eq!(store.evidence(), expected);
    assert!(store.cursor().get("pending_calls").is_some());
    assert!(store.pending_call_ids().is_empty());
}

#[test]
fn r6_openclaw_ignored_tail_still_full_rereads_for_guard_cursor_and_tape_failures() {
    for reason in ["guard mismatch", "cursor past EOF", "missing tape"] {
        let (store, _) = openclaw_store_with_pending_read();
        let mut cursor = store.cursor();
        let prior_tape = cursor["tape_id"].as_str().expect("tape id").to_string();
        store.append(&[oc_ignored_entry("custom")]);

        match reason {
            "guard mismatch" => {
                let source = fs::read_to_string(&store.source).expect("source");
                let rewritten = source.replace("/repo/src/b.rs", "/repo/src/c.rs");
                assert_ne!(source, rewritten, "fixture has guarded text");
                let guard_offset = cursor["cursor_guard"]["offset"]
                    .as_u64()
                    .expect("guard offset") as usize;
                let guard_len = cursor["cursor_guard"]["len"]
                    .as_u64()
                    .expect("guard length") as usize;
                assert_ne!(
                    &source.as_bytes()[guard_offset..guard_offset + guard_len],
                    &rewritten.as_bytes()[guard_offset..guard_offset + guard_len],
                    "rewrite changes bytes inside the cursor guard"
                );
                fs::write(&store.source, rewritten).expect("rewrite guarded prefix");
            }
            "cursor past EOF" => {
                cursor["byte_cursor"] =
                    json!(fs::metadata(&store.source).expect("source metadata").len() + 1);
                fs::write(store.cursor_path(), cursor.to_string()).expect("cursor write");
            }
            "missing tape" => store.remove_tape_file(&prior_tape),
            _ => unreachable!(),
        }

        let report = store.ingest();
        assert!(
            report["imported_tapes"].as_u64().unwrap_or(0) > 0,
            "{reason}: {report}"
        );
        let after = store.cursor();
        assert_ne!(after["tape_id"], prior_tape, "{reason}");
        assert_eq!(
            after["byte_cursor"],
            fs::metadata(&store.source).expect("source metadata").len(),
            "{reason}"
        );
        let reread = store.tape_events_for_id(after["tape_id"].as_str().expect("tape id"));
        assert!(
            files(&reread).contains(&"/repo/src/a.rs".to_string()),
            "{reason}: {reread:?}"
        );
    }
}

#[test]
fn r6_fresh_file_with_only_ignored_openclaw_rows_is_still_skipped() {
    let store = Store::new(Reader::OpenClaw);
    store.append(&[oc_ignored_entry("custom")]);
    let report = store.ingest();
    assert_eq!(report["skipped_non_transcript"], 1, "{report}");
    assert!(store.indexed_tape_ids().is_empty());
    assert!(!store.cursor_path().exists());
}

#[test]
fn r6_cursor_custom_tail_keeps_the_existing_full_reread_behavior() {
    let lines = cursor_lines();
    let store = Store::new(Reader::Cursor);
    store.append(&lines[..4]);
    store.ingest();
    store.append(&[cursor_read_started("pending", "/repo/src/b.rs")]);
    store.ingest();
    let tapes_before = store.indexed_tape_ids();
    let cursor_before = store.cursor();

    store.append(&[oc_ignored_entry("custom")]);
    let report = store.ingest();
    assert!(
        report["imported_tapes"].as_u64().unwrap_or(0) > 0,
        "{report}"
    );
    assert_ne!(store.cursor()["tape_id"], cursor_before["tape_id"]);
    let reread = store.new_events(&tapes_before);
    assert!(
        files(&reread).contains(&"/repo/src/main.rs".to_string()),
        "{reread:?}"
    );
}
