use std::fs;
use std::path::Path;
use std::process::Command;

use engram::tape::adapter::{AdapterId, adapter_claims_input};
use serde_json::Value;

fn run_json(repo: &Path, args: &[&str], home: &Path) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(repo)
        .args(args)
        .env("HOME", home)
        .output()
        .expect("engram should run");
    assert!(
        output.status.success(),
        "args={args:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("engram should emit JSON")
}

#[test]
fn every_adapter_has_a_positive_structural_signature() {
    let fixtures = [
        (
            AdapterId::ClaudeCode,
            "tests/fixtures/claude_adapter_input.jsonl",
        ),
        (
            AdapterId::CodexCli,
            "tests/fixtures/codex/supported_paths.jsonl",
        ),
        (
            AdapterId::OpenCode,
            "tests/fixtures/opencode/session_export.json",
        ),
        (
            AdapterId::GeminiCli,
            "tests/fixtures/gemini/session_with_tools.json",
        ),
        (
            AdapterId::Cursor,
            "tests/fixtures/cursor/supported_paths.jsonl",
        ),
        (
            AdapterId::OpenClaw,
            "tests/fixtures/openclaw/session_log.jsonl",
        ),
    ];

    for (adapter, path) in fixtures {
        let input = fs::read_to_string(path).expect("signature fixture should load");
        assert!(
            adapter_claims_input(adapter, &input),
            "{} should claim {path}",
            adapter.as_str()
        );
    }
}

#[test]
fn foreign_jsonl_has_no_positive_adapter_claims() {
    let input = concat!(
        "{\"customer\":17,\"total\":42.5}\n",
        "{\"customer\":23,\"total\":11.0}\n"
    );

    for adapter in [
        AdapterId::ClaudeCode,
        AdapterId::CodexCli,
        AdapterId::OpenCode,
        AdapterId::GeminiCli,
        AdapterId::Cursor,
        AdapterId::OpenClaw,
    ] {
        assert!(
            !adapter_claims_input(adapter, input),
            "{} must reject foreign JSONL",
            adapter.as_str()
        );
    }
}

#[test]
fn cursor_claims_supported_message_only_incremental_chunks() {
    let input = concat!(
        "{\"type\":\"assistant\",\"session_id\":\"cursor-session\",\"message\":",
        "{\"role\":\"assistant\",\"content\":\"Continuing the session\"}}\n"
    );

    assert!(adapter_claims_input(AdapterId::Cursor, input));
}

#[test]
fn path_hints_do_not_claim_foreign_input() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    let hinted = repo.join(".claude/projects/foreign");
    fs::create_dir_all(&hinted).expect("hinted directory");
    let input = hinted.join("records.jsonl");
    fs::write(&input, "{\"customer\":17,\"total\":42.5}\n").expect("foreign input");
    let _ = run_json(&repo, &["init"], &home);

    let ingest = run_json(&repo, &["ingest", input.to_string_lossy().as_ref()], &home);
    let tapes = run_json(&repo, &["tapes"], &home);

    assert_eq!(ingest["skipped_non_transcript"], 1);
    assert_eq!(tapes["tapes"], serde_json::json!([]));
}

#[test]
fn a_positive_claim_cannot_create_a_meta_only_tape() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    let hinted = repo.join(".codex/sessions/2026/07/25");
    fs::create_dir_all(&hinted).expect("hinted directory");
    let input = hinted.join("session.jsonl");
    fs::write(
        &input,
        "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\"}}\n",
    )
    .expect("meta-only Codex input");
    let _ = run_json(&repo, &["init"], &home);

    let ingest = run_json(&repo, &["ingest", input.to_string_lossy().as_ref()], &home);
    let tapes = run_json(&repo, &["tapes"], &home);

    assert_eq!(ingest["skipped_non_transcript"], 1);
    assert_eq!(tapes["tapes"], serde_json::json!([]));
}
