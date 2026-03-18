mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{Value, json};
use support::{run_json, run_json_timed};

fn write_repo_file(repo: &Path, rel: &str, content: &str) {
    let path = repo.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent dir");
    }
    fs::write(path, content).expect("write file");
}

fn make_large_file(line_count: usize, prefix: &str) -> String {
    (1..=line_count)
        .map(|line| format!("fn {prefix}_{line}() {{ value_{line}(); }}\n"))
        .collect::<String>()
}

#[test]
fn explain_arbitrary_span_returns_sessions_for_windowed_edits() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");

    let file_text = make_large_file(140, "granularity");
    write_repo_file(&repo, "src/lib.rs", &file_text);

    let _ = run_json(&repo, &["init"], None, &home);
    let record_line = json!({
        "t": "2026-03-18T00:00:00Z",
        "k": "code.edit",
        "file": "src/lib.rs",
        "before_range": [1, 1],
        "after_range": [1, 140],
        "before_text": "fn legacy() { old(); }\n",
        "after_text": file_text,
        "similarity": 0.93,
    })
    .to_string()
        + "\n";
    let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);

    let explain = run_json(&repo, &["explain", "src/lib.rs:55-72"], None, &home);
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert!(
        !sessions.is_empty(),
        "expected explain to return at least one session for arbitrary span"
    );

    let first = &sessions[0];
    assert!(
        !first["touches"].as_array().expect("touches").is_empty(),
        "expected explain touches for arbitrary span"
    );
    assert!(
        !first["windows"].as_array().expect("windows").is_empty(),
        "expected explain transcript windows for arbitrary span"
    );
}

#[test]
fn explain_scaled_fixture_meets_perf_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");

    let file_text = make_large_file(2400, "perf");
    write_repo_file(&repo, "src/lib.rs", &file_text);

    let _ = run_json(&repo, &["init"], None, &home);

    for rev in 0..8 {
        let revised_text = format!("// revision {rev}\n{file_text}");
        let record_line = json!({
            "t": "2026-03-18T00:00:00Z",
            "k": "code.edit",
            "file": "src/lib.rs",
            "before_range": [1, 1],
            "after_range": [1, 2401],
            "before_text": "fn bootstrap() { seed(); }\n",
            "after_text": revised_text,
            "similarity": 0.91,
        })
        .to_string()
            + "\n";
        let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);
    }

    let db_path = repo.join(".engram/index.sqlite");
    let conn = Connection::open(db_path).expect("open db");
    let evidence_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM evidence", [], |row| row.get(0))
        .expect("count evidence");
    assert!(
        evidence_rows >= 20_000,
        "scaled fixture should have substantial evidence cardinality; rows={evidence_rows}"
    );

    let (explain, elapsed) = run_json_timed(&repo, &["explain", "src/lib.rs:900-930"], None, &home);
    assert!(
        elapsed < Duration::from_secs(5),
        "explain exceeded perf budget: {:?}",
        elapsed
    );

    let sessions = explain["sessions"].as_array().expect("sessions");
    assert!(
        !sessions.is_empty(),
        "expected explain to return sessions on scaled fixture"
    );
}

#[test]
fn config_walkup_uses_global_db_and_repo_tapes_dir() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(home.join(".engram")).expect("home .engram");
    fs::create_dir_all(&repo).expect("repo");

    write_repo_file(&repo, "src/lib.rs", "fn app() { run(); }\n");
    write_repo_file(
        &home,
        ".engram/config.yml",
        "db: ~/.engram/index.sqlite\nadditional_stores: []\n",
    );
    write_repo_file(&repo, ".engram/config.yml", "tapes_dir: .engram/tapes\n");

    let record_line = json!({
        "t": "2026-03-18T00:00:00Z",
        "k": "code.read",
        "file": "src/lib.rs",
        "range": [1, 1],
        "anchor_hashes": ["winnow:0000000000000a11"],
    })
    .to_string()
        + "\n";
    let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);

    assert!(
        home.join(".engram/index.sqlite").exists(),
        "global db should be used"
    );
    assert!(
        !repo.join(".engram/index.sqlite").exists(),
        "repo should not create a local db when config only sets tapes_dir"
    );

    let explain = run_json(
        &repo,
        &["explain", "winnow:0000000000000a11", "--anchor"],
        None,
        &home,
    );
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1, "expected session from global db lookup");
    assert!(
        !sessions[0]["windows"]
            .as_array()
            .expect("windows")
            .is_empty(),
        "expected windows resolved from repo-level tapes_dir"
    );
}

#[test]
fn explain_additional_store_resolves_windows_from_store_tapes_path() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let producer = home.join("producer");
    let consumer = home.join("consumer");
    fs::create_dir_all(home.join(".engram")).expect("home .engram");
    fs::create_dir_all(&producer).expect("producer");
    fs::create_dir_all(&consumer).expect("consumer");

    write_repo_file(&home, ".engram/config.yml", "db: ~/.engram/index.sqlite\n");
    write_repo_file(
        &producer,
        ".engram/config.yml",
        "db: .engram/index.sqlite\ntapes_dir: .engram/tapes\n",
    );
    write_repo_file(&producer, "src/lib.rs", "fn shared() { v1(); }\n");

    let producer_tape = json!({
        "t": "2026-03-18T01:00:00Z",
        "k": "code.edit",
        "file": "src/lib.rs",
        "before_range": [1, 1],
        "after_range": [1, 1],
        "before_anchor_hashes": ["winnow:0000000000000a20"],
        "after_anchor_hashes": ["winnow:0000000000000a21"],
        "similarity": 0.95,
    })
    .to_string()
        + "\n";
    let _ = run_json(
        &producer,
        &["record", "--stdin"],
        Some(&producer_tape),
        &home,
    );

    let additional_store = producer.join(".engram/index.sqlite");
    assert!(additional_store.exists(), "producer index should exist");

    write_repo_file(
        &consumer,
        ".engram/config.yml",
        &format!(
            "db: .engram/index.sqlite\ntapes_dir: .engram/tapes\nadditional_stores:\n  - {}\n",
            additional_store.display()
        ),
    );

    let explain = run_json(
        &consumer,
        &["explain", "winnow:0000000000000a21", "--anchor"],
        None,
        &home,
    );

    assert_eq!(explain["stores_queried"].as_u64(), Some(2));
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1, "expected match from additional store");
    assert_eq!(
        sessions[0]["tape_present_locally"],
        Value::Bool(true),
        "expected tape lookup to resolve via additional store path"
    );
    assert!(
        !sessions[0]["windows"]
            .as_array()
            .expect("windows")
            .is_empty(),
        "expected windows for additional-store session"
    );
}
