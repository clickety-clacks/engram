use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use sha2::{Digest, Sha256};

fn run_cli(repo: &Path, args: &[&str], stdin: Option<&str>, home: &Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engram"));
    fs::create_dir_all(home).expect("home dir");
    cmd.current_dir(repo).args(args).env("HOME", home);
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

fn run_json(repo: &Path, args: &[&str], stdin: Option<&str>, home: &Path) -> Value {
    let output = run_cli(repo, args, stdin, home);
    assert!(
        output.status.success(),
        "command failed: args={args:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json stdout")
}

fn stderr_json_line(stderr: &[u8]) -> Value {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .find(|candidate| candidate.trim_start().starts_with('{'))
        .expect("json line in stderr");
    serde_json::from_str(line).expect("stderr json")
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[test]
fn ingest_is_local_scoped_incremental_and_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(&outside).expect("outside");

    let source_path = repo.join("input.codex.jsonl");
    fs::write(
        &source_path,
        include_str!("fixtures/codex/supported_paths.jsonl"),
    )
    .expect("seed source");
    fs::write(
        outside.join("outside.codex.jsonl"),
        include_str!("fixtures/codex/supported_paths.jsonl"),
    )
    .expect("outside source");

    let first = run_json(&repo, &["ingest"], None, &home);
    assert_eq!(first["status"], "ok");
    assert_eq!(first["imported_tapes"], 1);
    assert_eq!(first["skipped_unchanged"], 0);
    assert_eq!(first["skipped_non_transcript"], 0);
    let default_tapes = home.join(".engram/tapes");
    let tape_count = fs::read_dir(&default_tapes)
        .expect("tapes dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .count();
    assert_eq!(tape_count, 1, "expected ingest to write home-scoped tape");
    let user_config_path = home.join(".engram/config.yml");
    assert!(user_config_path.exists(), "expected auto-created user config");
    assert_eq!(
        fs::read_to_string(user_config_path).expect("user config"),
        "db: ~/.engram/index.sqlite\ntapes_dir: .engram/tapes\n",
        "expected auto-generated user config to carry explicit tapes_dir"
    );

    let second = run_json(&repo, &["ingest"], None, &home);
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

    let third = run_json(&repo, &["ingest"], None, &home);
    assert_eq!(third["status"], "ok");
    assert_eq!(third["imported_tapes"], 1);
}

#[test]
fn ingest_discovers_codex_sessions_for_repo_via_adapter_hook() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo");
    let codex_root = home.join(".codex/sessions/2026/03/10");
    fs::create_dir_all(&codex_root).expect("codex root");
    fs::write(
        codex_root.join("session.jsonl"),
        format!(
            concat!(
                "{{\"timestamp\":\"2026-02-22T00:00:00Z\",\"type\":\"session_meta\",",
                "\"payload\":{{\"cwd\":\"{}\",\"git\":{{\"commit_hash\":\"abc123\"}}}}}}\n",
                "{{\"timestamp\":\"2026-02-22T00:00:01Z\",\"type\":\"response_item\",",
                "\"payload\":{{\"type\":\"function_call\",\"name\":\"exec_command\",",
                "\"call_id\":\"call_1\",\"arguments\":\"{{\\\"cmd\\\":\\\"echo hi\\\"}}\"}}}}\n",
                "{{\"timestamp\":\"2026-02-22T00:00:02Z\",\"type\":\"response_item\",",
                "\"payload\":{{\"type\":\"function_call_output\",\"call_id\":\"call_1\",",
                "\"output\":\"Process exited with code 0\\nOutput:\\nhi\"}}}}\n"
            ),
            repo.to_string_lossy()
        ),
    )
    .expect("codex session");

    let ingest = run_json(&repo, &["ingest"], None, &home);
    assert_eq!(ingest["status"], "ok");
    assert_eq!(ingest["imported_tapes"], 1);
    assert!(ingest["scanned_inputs"].as_u64().unwrap_or(0) >= 1);
}

#[test]
fn ingest_writes_to_configured_tapes_dir() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let workspace = home.join("workspace");
    let repo = workspace.join("repo");
    fs::create_dir_all(repo.join(".engram")).expect("repo .engram");
    fs::create_dir_all(home.join(".engram")).expect("home .engram");
    fs::write(home.join(".engram/config.yml"), "db: ~/.engram/index.sqlite\n").expect("home config");
    fs::write(
        repo.join(".engram/config.yml"),
        "db: .engram/repo.sqlite\ntapes_dir: ~/.engram/compiled-tapes\n",
    )
    .expect("repo config");
    fs::write(
        repo.join("input.codex.jsonl"),
        include_str!("fixtures/codex/supported_paths.jsonl"),
    )
    .expect("input");

    let ingest = run_json(&repo, &["ingest"], None, &home);
    assert_eq!(ingest["status"], "ok");
    assert_eq!(ingest["imported_tapes"], 1);

    let compiled = home.join(".engram/compiled-tapes");
    let compiled_count = fs::read_dir(&compiled)
        .expect("compiled tapes dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .count();
    assert_eq!(compiled_count, 1, "expected one compiled tape in tapes_dir");

    let default_repo_tapes = repo.join(".engram/tapes");
    let repo_count = fs::read_dir(&default_repo_tapes)
        .expect("repo tapes dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .count();
    assert_eq!(repo_count, 0, "expected no ingest tape in default repo tapes dir");
}

#[test]
fn ingest_resolves_relative_tapes_dir_from_config_base() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let workspace = home.join("workspace");
    let repo = workspace.join("repo");
    fs::create_dir_all(repo.join(".engram")).expect("repo .engram");
    fs::create_dir_all(home.join(".engram")).expect("home .engram");
    fs::write(home.join(".engram/config.yml"), "db: ~/.engram/index.sqlite\n").expect("home config");
    fs::write(
        repo.join(".engram/config.yml"),
        "db: .engram/repo.sqlite\ntapes_dir: ../compiled-relative\n",
    )
    .expect("repo config");
    fs::write(
        repo.join("input.codex.jsonl"),
        include_str!("fixtures/codex/supported_paths.jsonl"),
    )
    .expect("input");

    let ingest = run_json(&repo, &["ingest"], None, &home);
    assert_eq!(ingest["status"], "ok");
    assert_eq!(ingest["imported_tapes"], 1);

    let compiled = workspace.join("compiled-relative");
    let compiled_count = fs::read_dir(&compiled)
        .expect("compiled tapes dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .count();
    assert_eq!(compiled_count, 1, "expected one compiled tape in relative tapes_dir");
}

#[test]
fn config_walkup_cascades_with_nearest_key_wins() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let workspace = home.join("workspace");
    let repo = workspace.join("repo");
    fs::create_dir_all(repo.join(".engram")).expect("repo .engram");
    fs::create_dir_all(workspace.join(".engram")).expect("workspace .engram");
    fs::create_dir_all(home.join(".engram")).expect("home .engram");

    fs::write(
        home.join(".engram/config.yml"),
        "db: ~/.engram/global.sqlite\n",
    )
    .expect("home config");
    fs::write(
        workspace.join(".engram/config.yml"),
        "db: /tmp/workspace.sqlite\n",
    )
    .expect("workspace config");
    fs::write(repo.join(".engram/config.yml"), "db: .engram/repo.sqlite\n").expect("repo config");

    fs::write(
        repo.join("input.codex.jsonl"),
        include_str!("fixtures/codex/supported_paths.jsonl"),
    )
    .expect("input");

    let ingest = run_cli(&repo, &["ingest"], None, &home);
    assert!(
        ingest.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&ingest.stdout),
        String::from_utf8_lossy(&ingest.stderr)
    );
    let stderr = String::from_utf8_lossy(&ingest.stderr);
    assert!(stderr.contains("config: "));
    assert!(stderr.contains("db: "));
    assert!(stderr.contains(repo.join(".engram/config.yml").to_string_lossy().as_ref()));
    assert!(stderr.contains(repo.join(".engram/repo.sqlite").to_string_lossy().as_ref()));
    assert!(repo.join(".engram/repo.sqlite").exists());
}

#[test]
fn init_creates_local_config_and_store_dirs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo");

    let init = run_cli(&repo, &["init"], None, &home);
    assert!(
        init.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    let payload: Value = serde_json::from_slice(&init.stdout).expect("json");
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["created"], true);

    let config_path = repo.join(".engram/config.yml");
    assert_eq!(
        fs::read_to_string(&config_path).expect("config"),
        "db: .engram/index.sqlite\ntapes_dir: .engram/tapes\n"
    );
    assert_eq!(
        fs::read_to_string(home.join(".engram/config.yml")).expect("home config"),
        "db: ~/.engram/index.sqlite\ntapes_dir: .engram/tapes\n",
        "expected auto-generated user config to carry explicit tapes_dir"
    );
    assert!(repo.join(".engram").is_dir());
    assert!(repo.join(".engram/tapes").is_dir());
    assert!(repo.join(".engram/objects").is_dir());
    assert!(repo.join(".engram/cursors").is_dir());

    let stderr = String::from_utf8_lossy(&init.stderr);
    assert!(stderr.contains(config_path.to_string_lossy().as_ref()));
    assert!(
        stderr.contains(repo.join(".engram/index.sqlite").to_string_lossy().as_ref()),
        "stderr={stderr}"
    );
}

#[test]
fn init_is_idempotent_when_local_config_exists() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo");

    let first = run_json(&repo, &["init"], None, &home);
    assert_eq!(first["created"], true);
    let second = run_json(&repo, &["init"], None, &home);
    assert_eq!(second["status"], "ok");
    assert_eq!(second["created"], false);
    assert!(
        second["message"]
            .as_str()
            .expect("message")
            .contains("already exists")
    );
}

#[test]
fn ingest_after_init_uses_local_db() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    let _ = run_json(&repo, &["init"], None, &home);
    fs::write(
        repo.join("input.codex.jsonl"),
        include_str!("fixtures/codex/supported_paths.jsonl"),
    )
    .expect("input");

    let ingest = run_cli(&repo, &["ingest"], None, &home);
    assert!(
        ingest.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&ingest.stdout),
        String::from_utf8_lossy(&ingest.stderr)
    );
    let stderr = String::from_utf8_lossy(&ingest.stderr);
    assert!(
        stderr.contains(repo.join(".engram/config.yml").to_string_lossy().as_ref()),
        "stderr={stderr}"
    );
    assert!(
        stderr.contains(repo.join(".engram/index.sqlite").to_string_lossy().as_ref()),
        "stderr={stderr}"
    );
    assert!(repo.join(".engram/index.sqlite").exists());
}

#[test]
fn fingerprint_indexes_only_local_tapes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let outside = temp.path().join("outside");
    fs::create_dir_all(repo.join(".engram/tapes")).expect("repo tapes");
    fs::create_dir_all(outside.join(".engram/tapes")).expect("outside tapes");

    let transcript = concat!(
        r#"{"t":"2026-02-22T00:00:00Z","k":"code.read","file":"src/lib.rs","range":[1,1],"anchor_hashes":["fp-anchor"]}"#,
        "\n"
    );
    let tape_id = sha256_hex(transcript);
    let compressed = zstd::stream::encode_all(transcript.as_bytes(), 0).expect("compress");
    fs::write(
        repo.join(".engram/tapes")
            .join(format!("{tape_id}.jsonl.zst")),
        compressed.clone(),
    )
    .expect("repo tape");
    fs::write(
        outside
            .join(".engram/tapes")
            .join(format!("{tape_id}-outside.jsonl.zst")),
        compressed,
    )
    .expect("outside tape");

    let fingerprint = run_json(&repo, &["fingerprint"], None, &home);
    assert_eq!(fingerprint["status"], "ok");
    assert_eq!(fingerprint["scanned_tapes"], 1);
    assert_eq!(fingerprint["fingerprinted_tapes"], 1);
    assert_eq!(fingerprint["skipped_existing_tapes"], 0);
}

#[test]
fn explain_fans_out_to_additional_stores_and_dedupes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let project_a = home.join("project-a");
    let project_b = home.join("project-b");
    fs::create_dir_all(project_a.join(".engram/tapes")).expect("a tapes");
    fs::create_dir_all(project_b.join(".engram/tapes")).expect("b tapes");
    fs::create_dir_all(home.join(".engram")).expect("home .engram");

    let primary_db = home.join(".engram/primary.sqlite");
    let extra_db = home.join(".engram/extra.sqlite");
    fs::write(
        home.join(".engram/config.yml"),
        format!("db: {}\n", primary_db.to_string_lossy()),
    )
    .expect("home config");

    let anchor = "shared-anchor";
    let transcript_a = concat!(
        r#"{"t":"2026-02-22T00:00:00Z","k":"code.read","file":"src/a.rs","range":[1,1],"anchor_hashes":["shared-anchor"]}"#,
        "\n"
    );
    let transcript_b = concat!(
        r#"{"t":"2026-02-22T00:00:01Z","k":"code.edit","file":"src/b.rs","before_range":[1,1],"after_range":[1,1],"before_hash":"old","after_hash":"shared-anchor","similarity":0.91}"#,
        "\n"
    );
    let tape_a = sha256_hex(transcript_a);
    let tape_b = sha256_hex(transcript_b);

    fs::write(
        project_a
            .join(".engram/tapes")
            .join(format!("{tape_a}.jsonl.zst")),
        zstd::stream::encode_all(transcript_a.as_bytes(), 0).expect("compress a"),
    )
    .expect("write tape a");
    fs::write(
        project_b
            .join(".engram/tapes")
            .join(format!("{tape_b}.jsonl.zst")),
        zstd::stream::encode_all(transcript_b.as_bytes(), 0).expect("compress b"),
    )
    .expect("write tape b");

    let fp_a = run_json(&project_a, &["fingerprint"], None, &home);
    assert_eq!(fp_a["fingerprinted_tapes"], 1);

    fs::write(
        project_b.join(".engram/config.yml"),
        format!("db: {}\n", extra_db.to_string_lossy()),
    )
    .expect("project b config");
    let fp_b = run_json(&project_b, &["fingerprint"], None, &home);
    assert_eq!(fp_b["fingerprinted_tapes"], 1);

    fs::write(
        project_a.join(".engram/config.yml"),
        format!(
            "db: {}\nadditional_stores:\n  - {}\n",
            primary_db.to_string_lossy(),
            extra_db.to_string_lossy()
        ),
    )
    .expect("project a config");

    let explain = run_json(&project_a, &["explain", anchor, "--anchor"], None, &home);
    assert_eq!(explain["stores_queried"], 2);
    let sessions = explain["sessions"].as_array().expect("sessions");
    assert!(
        sessions.iter().any(|session| session["tape_id"] == tape_a),
        "sessions={sessions:?}"
    );
    assert!(
        sessions.iter().any(|session| session["tape_id"] == tape_b),
        "sessions={sessions:?}"
    );
}

#[test]
fn ingest_errors_when_db_parent_is_not_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(repo.join(".engram")).expect("repo");
    fs::create_dir_all(home.join(".engram")).expect("home");
    fs::write(
        repo.join("input.codex.jsonl"),
        include_str!("fixtures/codex/supported_paths.jsonl"),
    )
    .expect("input");

    let file_parent = temp.path().join("not-a-dir");
    fs::write(&file_parent, "x").expect("file parent");
    fs::write(
        repo.join(".engram/config.yml"),
        format!("db: {}/index.sqlite\n", file_parent.to_string_lossy()),
    )
    .expect("config");

    let output = run_cli(&repo, &["ingest"], None, &home);
    assert!(
        !output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let err = stderr_json_line(&output.stderr);
    assert_eq!(err["error"]["code"], "mkdir_error");
}

#[test]
fn explain_errors_when_additional_store_is_not_sqlite_database() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join(".engram")).expect("repo .engram");
    fs::create_dir_all(home.join(".engram")).expect("home .engram");

    let bad_store = temp.path().join("bad-store");
    fs::create_dir_all(&bad_store).expect("bad store directory");
    fs::write(
        home.join(".engram/config.yml"),
        format!(
            "db: ~/.engram/index.sqlite\nadditional_stores:\n  - {}\n",
            bad_store.to_string_lossy()
        ),
    )
    .expect("home config");
    fs::write(repo.join("src.rs"), "fn main() {}\n").expect("source");

    let output = run_cli(&repo, &["explain", "src.rs:1-1"], None, &home);
    assert!(
        !output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let err = stderr_json_line(&output.stderr);
    assert_eq!(err["error"]["code"], "sqlite_error");
}
