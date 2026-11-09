//! OpenClaw user messages whose `content` is a plain string are indexed like
//! list-form text, and a store built by a reader that skipped them catches up
//! once, without importing anything twice.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use sha2::{Digest, Sha256};

const PLAIN_A: &str = "quillmoth plain question";
const PLAIN_B: &str = "saffronkite plain follow up";
const PLAIN_C: &str = "tinderhawk plain after upgrade";
const LISTED: &str = "marbleheron listed question";
const REPLY: &str = "juniperfox assistant reply";

fn row(id: &str, second: u32, message: Value) -> String {
    serde_json::to_string(&serde_json::json!({
        "type": "message",
        "id": id,
        "timestamp": format!("2026-02-26T01:00:{second:02}Z"),
        "message": message,
    }))
    .expect("row")
}

fn plain_user(id: &str, second: u32, text: &str) -> String {
    row(
        id,
        second,
        serde_json::json!({"role": "user", "content": text}),
    )
}

fn listed(id: &str, second: u32, role: &str, text: &str) -> String {
    row(
        id,
        second,
        serde_json::json!({"role": role, "content": [{"type": "text", "text": text}]}),
    )
}

/// A row no reader indexes, padded to `len` bytes.
fn ignored_row(len: usize) -> String {
    let empty = r#"{"type":"custom","pad":""}"#;
    let pad = "x".repeat(len - empty.len());
    format!(r#"{{"type":"custom","pad":"{pad}"}}"#)
}

/// A session with plain-string user messages early on and a long assistant
/// reply last, so the cursor guard window holds only list-form text.
fn session_lines() -> Vec<(String, bool)> {
    vec![
        (
            r#"{"type":"session","id":"oc-plain-1","timestamp":"2026-02-26T01:00:00Z"}"#.into(),
            false,
        ),
        (plain_user("m1", 1, PLAIN_A), true),
        (listed("m2", 2, "assistant", "first answer"), false),
        (listed("m3", 3, "user", LISTED), false),
        (plain_user("m4", 4, PLAIN_B), true),
        (
            listed(
                "m5",
                5,
                "assistant",
                &format!("{REPLY} {}", "long ".repeat(200)),
            ),
            false,
        ),
    ]
}

fn render(lines: &[(String, bool)], mask_plain: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (line, plain) in lines {
        if mask_plain && *plain {
            bytes.extend_from_slice(ignored_row(line.len()).as_bytes());
        } else {
            bytes.extend_from_slice(line.as_bytes());
        }
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

/// Number of indexed sessions holding `phrase`.
fn hits(repo: &Path, home: &Path, phrase: &str) -> u64 {
    let output = engram(repo, home, &["grep", phrase, "--count"]);
    if !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("no_results") {
        return 0;
    }
    let result: Value = serde_json::from_slice(&output.stdout).expect("json stdout");
    result["total"].as_u64().expect("total")
}

fn indexed_tapes(index: &Path) -> i64 {
    let db = rusqlite::Connection::open(index).expect("index");
    db.query_row("SELECT COUNT(*) FROM tapes", [], |row| row.get(0))
        .expect("tape count")
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

/// Rewrite the cursor the way a reader before 0.2.14 saved it.
fn forget_catch_up(repo: &Path, source: &Path) {
    let mut state = read_cursor(repo, source);
    state
        .as_object_mut()
        .expect("cursor object")
        .remove("openclaw_plain_string_user");
    fs::write(cursor_path(repo, source), state.to_string()).expect("cursor write");
}

fn append(path: &Path, line: &str) {
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open source");
    writeln!(file, "{line}").expect("append");
}

struct Store {
    _temp: tempfile::TempDir,
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
            _temp: temp,
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

    fn tapes(&self) -> i64 {
        indexed_tapes(&self.home.join(".engram/index.sqlite"))
    }

    /// The store an older reader left: the session ingested with its
    /// plain-string user messages skipped and a cursor without the catch-up
    /// flag. Masking those rows with same-length rows no reader indexes
    /// yields the same tape, cursor and guard.
    fn built_by_older_reader() -> Self {
        let store = Store::new();
        let lines = session_lines();
        fs::write(&store.source, render(&lines, true)).expect("masked");
        let first = store.ingest();
        assert_eq!(first["imported_tapes"], 1, "{first}");
        fs::write(&store.source, render(&lines, false)).expect("real");
        forget_catch_up(&store.repo, &store.source);
        assert_eq!(store.hits(PLAIN_A), 0);
        assert_eq!(store.hits(LISTED), 1);
        store
    }
}

#[test]
fn plain_string_user_messages_are_indexed_on_a_fresh_ingest() {
    let s = Store::new();
    fs::write(&s.source, render(&session_lines(), false)).expect("source");

    let first = s.ingest();
    assert_eq!(first["imported_tapes"], 1, "{first}");
    assert!(first.get("openclaw_catch_up").is_none(), "{first}");
    for phrase in [PLAIN_A, PLAIN_B, LISTED, REPLY] {
        assert_eq!(s.hits(phrase), 1, "{phrase}");
    }
    assert_eq!(
        read_cursor(&s.repo, &s.source)["openclaw_plain_string_user"],
        true
    );

    let again = s.ingest();
    assert_eq!(again["imported_tapes"], 0, "{again}");
    assert!(again.get("openclaw_catch_up").is_none(), "{again}");
    assert_eq!(s.hits(PLAIN_A), 1);
}

#[test]
fn an_older_store_catches_up_once_without_duplicates() {
    let s = Store::built_by_older_reader();
    let tapes_before = s.tapes();

    let upgrade = s.ingest();
    assert_eq!(upgrade["status"], "ok", "{upgrade}");
    assert_eq!(upgrade["openclaw_catch_up"]["sources"], 1, "{upgrade}");
    assert_eq!(upgrade["openclaw_catch_up"]["tapes"], 1, "{upgrade}");
    assert_eq!(upgrade["imported_tapes"], 0, "{upgrade}");
    assert_eq!(s.tapes(), tapes_before + 1);
    for phrase in [PLAIN_A, PLAIN_B, LISTED, REPLY] {
        assert_eq!(s.hits(phrase), 1, "{phrase}");
    }
    assert_eq!(
        read_cursor(&s.repo, &s.source)["openclaw_plain_string_user"],
        true
    );

    // A re-run finds nothing to catch up and imports nothing.
    let rerun = s.ingest();
    assert!(rerun.get("openclaw_catch_up").is_none(), "{rerun}");
    assert_eq!(rerun["imported_tapes"], 0, "{rerun}");
    assert_eq!(s.tapes(), tapes_before + 1);

    // Interrupted before the cursor was saved: the retry finds its tape
    // already indexed and adds nothing.
    forget_catch_up(&s.repo, &s.source);
    let retry = s.ingest();
    assert_eq!(retry["openclaw_catch_up"]["sources"], 1, "{retry}");
    assert_eq!(retry["openclaw_catch_up"]["tapes"], 0, "{retry}");
    assert_eq!(s.tapes(), tapes_before + 1);
    for phrase in [PLAIN_A, PLAIN_B, LISTED] {
        assert_eq!(s.hits(phrase), 1, "{phrase}");
    }

    // Later appends are read from the cursor as usual.
    append(&s.source, &plain_user("m6", 6, PLAIN_C));
    let next = s.ingest();
    assert!(next.get("openclaw_catch_up").is_none(), "{next}");
    assert_eq!(next["imported_tapes"], 1, "{next}");
    assert_eq!(s.hits(PLAIN_C), 1);
    for phrase in [PLAIN_A, PLAIN_B, LISTED, REPLY] {
        assert_eq!(s.hits(phrase), 1, "{phrase}");
    }
}

#[test]
fn an_append_of_only_plain_string_user_messages_is_read_incrementally() {
    let s = Store::new();
    fs::write(&s.source, render(&session_lines(), false)).expect("source");
    s.ingest();

    append(&s.source, &plain_user("m6", 6, PLAIN_C));
    let next = s.ingest();
    assert_eq!(next["imported_tapes"], 1, "{next}");
    assert_eq!(s.hits(PLAIN_C), 1);
    // Re-reading the whole file would index the earlier messages twice.
    assert_eq!(s.hits(PLAIN_A), 1);
    assert_eq!(s.hits(LISTED), 1);
}

struct Watch(std::process::Child);

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_watcher_catches_up_an_older_store_at_start() {
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
    fs::write(&source, render(&lines, true)).expect("masked");
    engram_json(&home, &home, &["ingest", sessions.to_str().expect("utf8")]);
    fs::write(&source, render(&lines, false)).expect("real");
    forget_catch_up(&home, &source);
    assert_eq!(hits(&home, &home, PLAIN_A), 0);
    let tapes_before = indexed_tapes(&index);

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
    while read_cursor(&home, &source)["openclaw_plain_string_user"] != true {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for startup catch-up; watch log:\n{}\nwatch stderr:\n{}",
            fs::read_to_string(store.join("watch.log")).unwrap_or_default(),
            fs::read_to_string(store.join("watch.stderr")).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(hits(&home, &home, PLAIN_A), 1);
    assert_eq!(hits(&home, &home, PLAIN_B), 1);
    assert_eq!(hits(&home, &home, LISTED), 1);
    assert_eq!(indexed_tapes(&index), tapes_before + 1);
}
