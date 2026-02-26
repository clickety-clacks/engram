use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use engram::anchor::fingerprint_text;
use serde_json::Value;

fn run_cli(repo: &Path, args: &[&str], stdin: Option<&str>, envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engram"));
    cmd.current_dir(repo).args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    if stdin.is_none() {
        return cmd.output().expect("command runs");
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("command spawns");
    {
        let mut pipe = child.stdin.take().expect("stdin pipe");
        pipe.write_all(stdin.expect("stdin content").as_bytes())
            .expect("stdin write");
    }
    child.wait_with_output().expect("command output")
}

fn run_json(repo: &Path, args: &[&str], stdin: Option<&str>, envs: &[(&str, &str)]) -> Value {
    let output = run_cli(repo, args, stdin, envs);
    assert!(
        output.status.success(),
        "command failed: args={args:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json stdout")
}

#[test]
fn ingest_is_incremental_and_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let source_path = repo.join("input.codex.jsonl");
    fs::write(
        &source_path,
        include_str!("fixtures/codex/supported_paths.jsonl"),
    )
    .expect("seed source");

    let _ = run_json(repo, &["init"], None, &[]);
    fs::write(
        repo.join(".engram/config.yml"),
        format!(
            "sources:\n  - path: {}\n    adapter: codex\nexclude: []\n",
            source_path.display()
        ),
    )
    .expect("write config");

    let first = run_json(repo, &["ingest"], None, &[]);
    assert_eq!(first["status"], "ok");
    assert_eq!(first["imported_tapes"], 1);
    assert_eq!(first["skipped_unchanged"], 0);

    let second = run_json(repo, &["ingest"], None, &[]);
    assert_eq!(second["status"], "ok");
    assert_eq!(second["imported_tapes"], 0);
    assert_eq!(second["skipped_unchanged"], 1);

    fs::OpenOptions::new()
        .append(true)
        .open(&source_path)
        .expect("open source")
        .write_all(
            b"{\"timestamp\":\"2026-02-22T00:00:03Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"added\"}]}}\n",
        )
        .expect("append");

    let third = run_json(repo, &["ingest"], None, &[]);
    assert_eq!(third["status"], "ok");
    assert_eq!(third["imported_tapes"], 1);

    let cursor_state_path = repo
        .join(".engram-cache")
        .join("cursors")
        .join("ingest-state.json");
    let cursor_state = fs::read_to_string(cursor_state_path).expect("cursor state exists");
    let parsed: Value = serde_json::from_str(&cursor_state).expect("cursor state is valid json");
    assert!(
        parsed
            .get("files")
            .and_then(Value::as_object)
            .map(|m| !m.is_empty())
            .unwrap_or(false),
        "expected cursor state with tracked files"
    );
}

#[test]
fn ingest_honors_exclude_patterns() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path();
    let src_dir = repo.join("fixtures");
    fs::create_dir_all(&src_dir).expect("fixtures dir");
    fs::write(
        src_dir.join("keep.codex.jsonl"),
        include_str!("fixtures/codex/supported_paths.jsonl"),
    )
    .expect("keep file");
    fs::write(
        src_dir.join("ignore.codex.jsonl"),
        include_str!("fixtures/codex/supported_paths.jsonl"),
    )
    .expect("ignore file");

    let _ = run_json(repo, &["init"], None, &[]);
    fs::write(
        repo.join(".engram/config.yml"),
        format!(
            "sources:\n  - path: {}/**/*.jsonl\n    adapter: codex\nexclude:\n  - {}/ignore*.jsonl\n",
            src_dir.display(),
            src_dir.display()
        ),
    )
    .expect("write config");

    let ingest = run_json(repo, &["ingest"], None, &[]);
    assert_eq!(ingest["status"], "ok");
    assert_eq!(ingest["scanned_inputs"], 1);
    assert_eq!(ingest["imported_tapes"], 1);
}

#[test]
fn global_mode_uses_home_roots_and_explain_queries_ingested_evidence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home dir");

    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("repo src");
    fs::write(repo.join("src/lib.rs"), "alpha\nomega\nzeta\n").expect("seed file");

    let home_str = home.to_string_lossy().to_string();
    let envs = [("HOME", home_str.as_str())];
    let _ = run_json(&repo, &["init", "--global"], None, &envs);

    let anchor = fingerprint_text("omega").fingerprint;
    let source_path = home.join("openclaw-session.jsonl");
    fs::write(
        &source_path,
        format!(
            concat!(
                "{{\"timestamp\":\"2026-02-26T00:00:00Z\",\"type\":\"user\",\"role\":\"user\",\"session_id\":\"oc-g1\",\"content\":\"Investigate span\"}}\n",
                "{{\"timestamp\":\"2026-02-26T00:00:01Z\",\"type\":\"code.edit\",\"file\":\"src/lib.rs\",\"before_hash\":\"x\",\"after_hash\":\"{0}\",\"similarity\":0.91}}\n"
            ),
            anchor
        ),
    )
    .expect("write source");

    fs::write(
        home.join(".engram/config.yml"),
        format!(
            "sources:\n  - path: {}\n    adapter: openclaw\nexclude: []\n",
            source_path.display()
        ),
    )
    .expect("write global config");

    let ingest = run_json(&repo, &["ingest", "--global"], None, &envs);
    assert_eq!(ingest["status"], "ok");
    assert_eq!(ingest["imported_tapes"], 1);

    let explain = run_json(&repo, &["explain", "src/lib.rs:2-2", "--global"], None, &envs);
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["touch_count"], 1);
}

#[test]
fn ingest_merges_user_and_repo_config_sources() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home dir");
    fs::create_dir_all(home.join(".engram")).expect("user engram");

    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");

    let codex_source = repo.join("repo-source.jsonl");
    fs::write(
        &codex_source,
        include_str!("fixtures/codex/supported_paths.jsonl"),
    )
    .expect("codex source");

    let openclaw_source = repo.join("user-source.jsonl");
    fs::write(
        &openclaw_source,
        include_str!("fixtures/openclaw/session_log.jsonl"),
    )
    .expect("openclaw source");

    let home_str = home.to_string_lossy().to_string();
    let envs = [("HOME", home_str.as_str())];
    let _ = run_json(&repo, &["init"], None, &envs);

    fs::write(
        repo.join(".engram/config.yml"),
        format!(
            "sources:\n  - path: {}\n    adapter: codex\nexclude: []\n",
            codex_source.display()
        ),
    )
    .expect("repo config");
    fs::write(
        home.join(".engram/config.yml"),
        format!(
            "sources:\n  - path: {}\n    adapter: openclaw\nexclude: []\n",
            openclaw_source.display()
        ),
    )
    .expect("user config");

    let ingest = run_json(&repo, &["ingest"], None, &envs);
    assert_eq!(ingest["status"], "ok");
    assert_eq!(ingest["imported_tapes"], 2);
}
