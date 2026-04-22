mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{Value, json};
use support::{run_cli, run_json, run_json_timed};

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
    let window_start = first["window_start"].as_u64().expect("window_start");
    let window_end = first["window_end"].as_u64().expect("window_end");
    let total_lines = first["total_lines"].as_u64().expect("total_lines");
    assert!(window_start >= 1);
    assert!(window_end >= window_start);
    assert!(window_end <= total_lines.max(1));
    assert!(total_lines >= 1);
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

    for rev in 0..12 {
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
    assert_eq!(
        explain["returned"].as_u64(),
        Some(10),
        "default explain limit should return top 10 sessions"
    );
    assert_eq!(
        explain["total"].as_u64(),
        Some(12),
        "fixture should produce 12 matching sessions"
    );
    assert_eq!(
        explain["truncated"].as_bool(),
        Some(true),
        "default explain limit should truncate when more than 10 match"
    );
    assert_eq!(sessions.len(), 10);
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
    assert_eq!(sessions[0]["window_start"], Value::from(1));
    assert_eq!(sessions[0]["window_end"], Value::from(1));
    assert_eq!(sessions[0]["total_lines"], Value::from(1));
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
    assert_eq!(sessions[0]["window_start"], Value::from(1));
    assert_eq!(sessions[0]["window_end"], Value::from(1));
    assert_eq!(sessions[0]["total_lines"], Value::from(1));
}

#[test]
fn explain_supports_string_file_and_peek_navigation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");

    let code_text = "fn explain_nav() { return 42; }\n";
    write_repo_file(&repo, "src/lib.rs", code_text);

    let _ = run_json(&repo, &["init"], None, &home);
    let record_line = json!({
        "t": "2026-03-18T02:00:00Z",
        "k": "code.edit",
        "file": "src/lib.rs",
        "before_range": [1, 1],
        "after_range": [1, 1],
        "before_text": "fn explain_nav() { return 0; }\n",
        "after_text": code_text,
        "similarity": 0.95,
    })
    .to_string()
        + "\n";
    let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);

    let explain = run_json(&repo, &["explain", code_text], None, &home);
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert!(
        !sessions.is_empty(),
        "expected string explain to return sessions"
    );
    let session_id = sessions[0]["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    assert_eq!(sessions[0]["window_start"], Value::from(1));
    assert_eq!(sessions[0]["window_end"], Value::from(1));
    assert_eq!(sessions[0]["total_lines"], Value::from(1));
    assert!(sessions[0]["confidence"].as_f64().unwrap_or(0.0) > 0.0);
    assert_eq!(sessions[0]["refs_up"], Value::from(0));
    assert_eq!(sessions[0]["refs_down"], Value::from(0));
    assert_eq!(
        sessions[0]["files_touched"],
        json!(["src/lib.rs"]),
        "single-file fixture should report one touched file"
    );
    assert!(sessions[0].get("content").is_none());
    assert!(sessions[0].get("windows").is_none());
    assert!(sessions[0].get("touches").is_none());

    let by_session = run_json(
        &repo,
        &["peek", &session_id, "--start", "1", "--lines", "5"],
        None,
        &home,
    );
    let peek_session = &by_session["session"];
    assert_eq!(peek_session["session_id"], Value::String(session_id));
    assert_eq!(peek_session["window_start"], Value::from(1));

    let whole_file = run_json(&repo, &["explain", "src/lib.rs"], None, &home);
    assert!(
        !whole_file["sessions"]
            .as_array()
            .expect("sessions")
            .is_empty(),
        "whole-file explain should return sessions"
    );
}

#[test]
fn grep_uses_explain_output_shape_and_truncation_header() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");

    write_repo_file(&repo, "src/lib.rs", "fn grep_case() {}\n");
    let _ = run_json(&repo, &["init"], None, &home);

    for i in 0..2 {
        let ts = format!("2026-03-18T02:0{i}:00Z");
        let record_line = json!({
            "t": ts,
            "k": "msg.out",
            "role": "assistant",
            "content": format!("needle marker {i}"),
        })
        .to_string()
            + "\n";
        let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);
    }

    let grep = run_json(&repo, &["grep", "needle", "--limit", "1"], None, &home);
    let sessions = grep["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(grep["returned"], Value::from(1));
    assert_eq!(grep["total"], Value::from(2));
    assert_eq!(grep["truncated"], Value::Bool(true));
    assert_eq!(
        grep["time_range"],
        json!({"start":"2026-03-18T02:00:00Z","end":"2026-03-18T02:01:00Z"})
    );
    assert!(sessions[0]["session_id"].as_str().is_some());
    assert_eq!(sessions[0]["window_start"], Value::from(1));
    assert_eq!(sessions[0]["window_end"], Value::from(1));
}

#[test]
fn explain_supports_grep_filter_offset_count_and_date_bounds() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    write_repo_file(&repo, "src/lib.rs", "fn filters_case() {}\n");
    let _ = run_json(&repo, &["init"], None, &home);

    for i in 0..4 {
        let ts = format!("2026-03-18T04:0{i}:00Z");
        let record_line = json!({
            "t": ts,
            "k": "code.edit",
            "file": "src/lib.rs",
            "before_range": [1, 1],
            "after_range": [1, 1],
            "before_text": "fn filters_case() {}\n",
            "after_text": format!("fn filters_case_{i}() {{}}\n"),
            "similarity": 0.9,
        })
        .to_string()
            + "\n";
        let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);
    }

    let filtered = run_json(
        &repo,
        &[
            "explain",
            "src/lib.rs:1-1",
            "--grep-filter",
            "filters_case",
            "--since",
            "2026-03-18",
            "--until",
            "2026-03-18",
            "--offset",
            "1",
            "--limit",
            "2",
            "--count",
        ],
        None,
        &home,
    );
    assert_eq!(filtered["returned"], Value::from(2));
    assert_eq!(filtered["query"]["count"], Value::Bool(true));
    assert_eq!(filtered["query"]["offset"], Value::from(1));
}

#[test]
fn api_surface_errors_are_json_objects() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    write_repo_file(&repo, "src/lib.rs", "fn invalid_span() {}\n");
    let _ = run_json(&repo, &["init"], None, &home);

    let invalid_span = run_cli(&repo, &["explain", "src/lib.rs:2-a"], None, &home);
    assert!(!invalid_span.status.success());
    let invalid_stderr = String::from_utf8_lossy(&invalid_span.stderr);
    let invalid_json = invalid_stderr
        .lines()
        .last()
        .expect("invalid span stderr line")
        .as_bytes()
        .to_vec();
    let invalid_payload: Value = serde_json::from_slice(&invalid_json).expect("invalid span json error");
    assert_eq!(invalid_payload["error"], Value::String("invalid_span".to_string()));

    let no_results = run_cli(&repo, &["grep", "definitely-missing-pattern"], None, &home);
    assert!(!no_results.status.success());
    let no_results_stderr = String::from_utf8_lossy(&no_results.stderr);
    let no_results_json = no_results_stderr
        .lines()
        .last()
        .expect("no_results stderr line")
        .as_bytes()
        .to_vec();
    let no_results_payload: Value = serde_json::from_slice(&no_results_json).expect("no_results json error");
    assert_eq!(no_results_payload["error"], Value::String("no_results".to_string()));

    let missing_session = run_cli(&repo, &["peek", "missing-session-id"], None, &home);
    assert!(!missing_session.status.success());
    let session_stderr = String::from_utf8_lossy(&missing_session.stderr);
    let session_json = session_stderr
        .lines()
        .last()
        .expect("session_not_found stderr line")
        .as_bytes()
        .to_vec();
    let session_payload: Value = serde_json::from_slice(&session_json).expect("session_not_found json error");
    assert_eq!(
        session_payload["error"],
        Value::String("session_not_found".to_string())
    );
}

#[test]
fn grep_defaults_to_ten_sessions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");

    write_repo_file(&repo, "src/lib.rs", "fn grep_default_limit() {}\n");
    let _ = run_json(&repo, &["init"], None, &home);

    for i in 0..12 {
        let ts = format!("2026-03-18T03:{i:02}:00Z");
        let record_line = json!({
            "t": ts,
            "k": "msg.out",
            "role": "assistant",
            "content": format!("default-limit needle {i}"),
        })
        .to_string()
            + "\n";
        let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);
    }

    let grep = run_json(&repo, &["grep", "default-limit needle"], None, &home);
    let sessions = grep["sessions"].as_array().expect("sessions");
    assert_eq!(grep["returned"], Value::from(10));
    assert_eq!(grep["total"], Value::from(12));
    assert_eq!(grep["truncated"], Value::Bool(true));
    assert_eq!(sessions.len(), 10);
}

#[test]
fn grep_count_returns_metadata_only() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    write_repo_file(&repo, "src/lib.rs", "fn grep_count_case() {}\n");
    let _ = run_json(&repo, &["init"], None, &home);

    for i in 0..3 {
        let ts = format!("2026-03-18T05:0{i}:00Z");
        let record_line = json!({
            "t": ts,
            "k": "msg.out",
            "role": "assistant",
            "content": format!("count-only needle {i}"),
        })
        .to_string()
            + "\n";
        let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);
    }

    let grep = run_json(&repo, &["grep", "count-only needle", "--count"], None, &home);
    assert_eq!(grep["returned"], Value::from(3));
    assert_eq!(grep["total"], Value::from(3));
    assert_eq!(grep["sessions"], json!([]));
}

#[test]
fn grep_ranks_provenance_matches_before_recent_text_mentions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    write_repo_file(&repo, "src/ranked.rs", "fn ranked() {}\n");
    let _ = run_json(&repo, &["init"], None, &home);

    let provenance_record = json!({
        "t": "2026-03-17T00:00:00Z",
        "k": "code.edit",
        "file": "src/ranked.rs",
        "before_range": [1, 1],
        "after_range": [1, 1],
        "before_text": "fn ranked() {}\n",
        "after_text": "fn ranked() { grep_rank_target(); }\n",
        "similarity": 0.9,
    })
    .to_string()
        + "\n";
    let _ = run_json(
        &repo,
        &["record", "--stdin"],
        Some(&provenance_record),
        &home,
    );

    for i in 0..11 {
        let record_line = json!({
            "t": format!("2026-03-18T03:{i:02}:00Z"),
            "k": "msg.out",
            "role": "assistant",
            "content": format!("grep_rank_target text mention {i}"),
        })
        .to_string()
            + "\n";
        let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);
    }

    let grep = run_json(&repo, &["grep", "grep_rank_target"], None, &home);
    let sessions = grep["sessions"].as_array().expect("sessions");
    assert_eq!(grep["total"], Value::from(12));
    assert_eq!(grep["returned"], Value::from(10));
    assert_eq!(grep["truncated"], Value::Bool(true));
    assert_eq!(
        sessions[0]["timestamp"],
        Value::from("2026-03-17T00:00:00Z")
    );
    assert_eq!(sessions[0]["files_touched"], json!(["src/ranked.rs"]));
    assert!(
        sessions
            .iter()
            .any(|session| session["timestamp"] == Value::from("2026-03-17T00:00:00Z")),
        "older provenance-bearing match should survive default-limit truncation"
    );
}

#[test]
fn metrics_logging_writes_expected_jsonl_row() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(repo.join(".engram")).expect("repo");
    write_repo_file(
        &repo,
        ".engram/config.yml",
        "db: .engram/index.sqlite\ntapes_dir: .engram/tapes\nmetrics:\n  log: .engram/metrics.jsonl\n",
    );
    write_repo_file(&repo, "src/lib.rs", "fn metrics_case() {}\n");

    let _ = run_json(&repo, &["init"], None, &home);
    let record_line = json!({
        "t": "2026-03-18T05:20:00Z",
        "k": "code.edit",
        "file": "src/lib.rs",
        "before_range": [1, 1],
        "after_range": [1, 1],
        "before_text": "fn metrics_case_old() {}\n",
        "after_text": "fn metrics_case() {}\n",
        "similarity": 0.9,
    })
    .to_string()
        + "\n";
    let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);

    let _ = run_json(&repo, &["explain", "src/lib.rs:1-1"], None, &home);
    let metrics_path = repo.join(".engram/metrics.jsonl");
    assert!(metrics_path.exists(), "metrics log should be created");
    let content = fs::read_to_string(metrics_path).expect("metrics content");
    let last = content.lines().last().expect("metrics row");
    let row: Value = serde_json::from_str(last).expect("metrics row json");
    assert_eq!(row["command"], Value::String("explain".to_string()));
    assert_eq!(row["target"], Value::String("src/lib.rs:1-1".to_string()));
    assert!(!row["ts"].as_str().unwrap_or("").is_empty());
    assert_eq!(row["window_lines"], Value::Null);
}

#[test]
fn peek_start_and_lines_returns_exact_absolute_range() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");

    let _ = run_json(&repo, &["init"], None, &home);
    let transcript = (1..=8)
        .map(|i| {
            json!({
                "t": format!("2026-03-19T00:{i:02}:00Z"),
                "k": "msg.out",
                "role": "assistant",
                "content": format!("absolute-line-{i}")
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let recorded = run_json(&repo, &["record", "--stdin"], Some(&transcript), &home);
    let session_id = recorded["tape_id"].as_str().expect("tape_id");

    let peek = run_json(
        &repo,
        &["peek", session_id, "--start", "3", "--lines", "2"],
        None,
        &home,
    );
    let session = &peek["session"];
    assert_eq!(session["window_start"], Value::from(3));
    assert_eq!(session["window_end"], Value::from(4));
    let content = session["content"].as_array().expect("content");
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["line"], Value::from(3));
    assert_eq!(content[1]["line"], Value::from(4));
    assert!(
        content[0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("absolute-line-3")
    );
    assert!(
        content[1]["text"]
            .as_str()
            .unwrap_or("")
            .contains("absolute-line-4")
    );
}

#[test]
fn peek_before_after_returns_anchor_relative_range() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");

    let _ = run_json(&repo, &["init"], None, &home);
    let transcript = (1..=6)
        .map(|i| {
            json!({
                "t": format!("2026-03-19T01:{i:02}:00Z"),
                "k": "msg.out",
                "role": "assistant",
                "content": format!("anchor-line-{i}")
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let recorded = run_json(&repo, &["record", "--stdin"], Some(&transcript), &home);
    let session_id = recorded["tape_id"].as_str().expect("tape_id");

    let peek = run_json(
        &repo,
        &["peek", session_id, "--before", "0", "--after", "2"],
        None,
        &home,
    );
    let session = &peek["session"];
    assert_eq!(session["window_start"], Value::from(1));
    assert_eq!(session["window_end"], Value::from(3));
    let content = session["content"].as_array().expect("content");
    assert_eq!(content.len(), 3);
    assert_eq!(content[0]["line"], Value::from(1));
    assert_eq!(content[2]["line"], Value::from(3));
}

#[test]
fn peek_grep_filter_returns_match_context_window() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(repo.join(".engram")).expect("repo");
    write_repo_file(
        &repo,
        ".engram/config.yml",
        "db: .engram/index.sqlite\ntapes_dir: .engram/tapes\npeek:\n  grep_context: 1\n",
    );

    let _ = run_json(&repo, &["init"], None, &home);
    let transcript = (1..=15)
        .map(|i| {
            let content = if i == 9 {
                "needle-in-session".to_string()
            } else {
                format!("plain-line-{i}")
            };
            json!({
                "t": format!("2026-03-19T02:{i:02}:00Z"),
                "k": "msg.out",
                "role": "assistant",
                "content": content
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let recorded = run_json(&repo, &["record", "--stdin"], Some(&transcript), &home);
    let session_id = recorded["tape_id"].as_str().expect("tape_id");

    let peek = run_json(
        &repo,
        &["peek", session_id, "--grep-filter", "needle-in-session"],
        None,
        &home,
    );
    let session = &peek["session"];
    assert_eq!(session["window_start"], Value::from(8));
    assert_eq!(session["window_end"], Value::from(10));
    let content = session["content"].as_array().expect("content");
    assert_eq!(content.len(), 3);
    assert!(
        content
            .iter()
            .any(|line| line["text"].as_str().unwrap_or("").contains("needle-in-session")),
        "grep-filter output should include the matching line"
    );
}

#[test]
fn explain_chain_metadata_populates_parent_children_and_depth() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    write_repo_file(&repo, "src/lib.rs", "fn chain_target() { child(); }\n");

    let _ = run_json(&repo, &["init"], None, &home);
    let dispatch_uuid = "11111111-1111-4111-8111-111111111111";

    let parent_transcript = json!({
        "t": "2026-03-19T03:00:00Z",
        "k": "msg.out",
        "role": "assistant",
        "content": "parent dispatch created",
        "metadata": { "dispatch_id": format!("<engram-src id=\"{dispatch_uuid}\"/>") }
    })
    .to_string()
        + "\n";
    let parent = run_json(&repo, &["record", "--stdin"], Some(&parent_transcript), &home);
    let parent_id = parent["tape_id"].as_str().expect("parent tape id").to_string();

    let child_transcript = format!(
        "{}\n{}\n",
        json!({
            "t": "2026-03-19T03:05:00Z",
            "k": "msg.in",
            "role": "user",
            "content": format!("context <engram-src id=\"{dispatch_uuid}\"/>")
        }),
        json!({
            "t": "2026-03-19T03:06:00Z",
            "k": "code.edit",
            "file": "src/lib.rs",
            "before_range": [1, 1],
            "after_range": [1, 1],
            "before_text": "fn chain_target() { old(); }\n",
            "after_text": "fn chain_target() { child(); }\n",
            "similarity": 0.96
        })
    );
    let child = run_json(&repo, &["record", "--stdin"], Some(&child_transcript), &home);
    let child_id = child["tape_id"].as_str().expect("child tape id").to_string();

    let explain = run_json(&repo, &["explain", "src/lib.rs:1-1", "--limit", "10"], None, &home);
    let sessions = explain["sessions"].as_array().expect("sessions");

    let parent_session = sessions
        .iter()
        .find(|session| session["session_id"] == Value::String(parent_id.clone()))
        .expect("parent session in explain output");
    let child_session = sessions
        .iter()
        .find(|session| session["session_id"] == Value::String(child_id.clone()))
        .expect("child session in explain output");

    assert_eq!(parent_session["depth"], Value::from(0));
    assert_eq!(parent_session["parent"], Value::Null);
    assert_eq!(parent_session["chain_length"], Value::from(2));
    assert!(
        parent_session["children"]
            .as_array()
            .expect("parent children")
            .iter()
            .any(|value| value.as_str() == Some(child_id.as_str()))
    );

    assert_eq!(child_session["depth"], Value::from(1));
    assert_eq!(child_session["parent"], Value::String(parent_id.clone()));
    assert_eq!(child_session["chain_length"], Value::from(2));

    let chains = explain["chains"].as_array().expect("chains");
    let parent_chain = chains
        .iter()
        .find(|chain| chain["root_session_id"] == Value::String(parent_id.clone()))
        .expect("root chain");
    let descendants = parent_chain["descendants"].as_array().expect("descendants");
    assert_eq!(descendants.len(), 2);
    assert!(
        descendants
            .iter()
            .any(|value| value["session_id"] == Value::String(child_id.clone()))
    );
}

#[test]
fn grep_truncates_at_safe_threshold_boundary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    write_repo_file(&repo, "src/lib.rs", "fn threshold_case() {}\n");
    let _ = run_json(&repo, &["init"], None, &home);

    for i in 0..26 {
        let record_line = json!({
            "t": format!("2026-03-19T04:{i:02}:00Z"),
            "k": "msg.out",
            "role": "assistant",
            "content": format!("threshold needle {i}"),
        })
        .to_string()
            + "\n";
        let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);
    }

    let grep = run_json(
        &repo,
        &["grep", "threshold needle", "--limit", "100"],
        None,
        &home,
    );
    assert_eq!(grep["total"], Value::from(26));
    assert_eq!(grep["returned"], Value::from(25));
    assert_eq!(grep["truncated"], Value::Bool(true));
    assert!(grep["time_range"].get("start").is_some());
    assert!(grep["time_range"].get("end").is_some());
    assert_eq!(grep["sessions"].as_array().expect("sessions").len(), 25);
}

#[test]
fn explain_since_until_filters_sessions_by_timestamp() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    write_repo_file(&repo, "src/lib.rs", "fn date_filter_target() { live(); }\n");
    let _ = run_json(&repo, &["init"], None, &home);

    for (idx, ts) in [
        "2026-03-10T10:00:00Z",
        "2026-03-15T10:00:00Z",
        "2026-03-20T10:00:00Z",
    ]
    .into_iter()
    .enumerate()
    {
        let record_line = json!({
            "t": ts,
            "k": "code.edit",
            "file": "src/lib.rs",
            "before_range": [1, 1],
            "after_range": [1, 1],
            "before_text": format!("fn date_filter_target() {{ old_{idx}(); }}\n"),
            "after_text": "fn date_filter_target() { live(); }\n",
            "similarity": 0.9,
        })
        .to_string()
            + "\n";
        let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);
    }

    let explain = run_json(
        &repo,
        &[
            "explain",
            "src/lib.rs:1-1",
            "--since",
            "2026-03-12",
            "--until",
            "2026-03-18",
            "--limit",
            "10",
        ],
        None,
        &home,
    );
    assert_eq!(explain["total"], Value::from(1));
    assert_eq!(explain["returned"], Value::from(1));
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0]["timestamp"],
        Value::String("2026-03-15T10:00:00Z".to_string())
    );
}

#[test]
fn explain_direct_string_query_returns_fingerprint_matches() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");

    let query = "fn direct_string_match() { value_42(); }\n";
    write_repo_file(&repo, "src/lib.rs", query);
    let _ = run_json(&repo, &["init"], None, &home);
    let record_line = json!({
        "t": "2026-03-19T05:00:00Z",
        "k": "code.edit",
        "file": "src/lib.rs",
        "before_range": [1, 1],
        "after_range": [1, 1],
        "before_text": "fn direct_string_match() { value_0(); }\n",
        "after_text": query,
        "similarity": 0.97,
    })
    .to_string()
        + "\n";
    let _ = run_json(&repo, &["record", "--stdin"], Some(&record_line), &home);

    let explain = run_json(&repo, &["explain", query], None, &home);
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert!(!sessions.is_empty(), "expected literal explain to return matches");
    assert!(
        sessions[0]["confidence"].as_f64().unwrap_or(0.0) > 0.0,
        "direct string explain should return a positive confidence match"
    );
}
