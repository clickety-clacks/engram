#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use engram::index::{DispatchLink, SqliteIndex};
use serde_json::json;
use sha2::Digest;

fn write_grep_owner(
    root: &std::path::Path,
    machine: &str,
    binary: &str,
    tapes: &[(&str, &str)],
) -> serde_json::Value {
    let home = root.join(format!("{machine}-home"));
    let engram_home = home.join(".engram");
    let tape_dir = engram_home.join("tapes");
    std::fs::create_dir_all(&tape_dir).expect("owner tape directory");
    let db = engram_home.join("index.sqlite");
    drop(SqliteIndex::open_writer(db.to_str().expect("owner DB path")).expect("owner DB"));
    for (tape_id, content) in tapes {
        let compressed = zstd::stream::encode_all(content.as_bytes(), 0).expect("compress tape");
        std::fs::write(tape_dir.join(format!("{tape_id}.jsonl.zst")), compressed)
            .expect("write owner tape");
    }
    std::fs::write(
        engram_home.join("topology.yml"),
        format!(
            "version: 1\nself: {machine}\nexports:\n  default:\n    db: {}\n    tape_dirs:\n      - {}\n",
            db.display(),
            tape_dir.display()
        ),
    )
    .expect("owner topology");
    json!({
        "command": [
            "/usr/bin/env",
            format!("HOME={}", home.display()),
            binary,
            "peer-serve",
            "--stdio"
        ],
        "engram": binary,
        "exports": ["default"],
    })
}

fn write_ranked_explain_source(
    root: &std::path::Path,
    tapes: &[(String, String)],
) -> (std::path::PathBuf, std::path::PathBuf, Vec<String>) {
    let caller_home = root.join("rank-caller-home");
    let caller_engram = caller_home.join(".engram");
    std::fs::create_dir_all(&caller_engram).expect("rank caller home");
    let repo = root.join("rank-repo");
    let local_engram = repo.join(".engram");
    let local_tapes = local_engram.join("tapes");
    std::fs::create_dir_all(&local_tapes).expect("rank local tapes");
    let local_db = local_engram.join("index.sqlite");
    std::fs::write(
        caller_engram.join("config.yml"),
        format!(
            "db: {}\ntapes_dir: {}\n",
            local_db.display(),
            local_tapes.display()
        ),
    )
    .expect("rank caller config");

    let index = SqliteIndex::open_writer(local_db.to_str().expect("rank local DB path"))
        .expect("open rank local index");
    let mut tape_ids = Vec::new();
    for (tape_id, content) in tapes {
        assert_eq!(
            format!("{:x}", sha2::Sha256::digest(content.as_bytes())),
            tape_id.as_str(),
            "rank fixture tape must retain its content address"
        );
        let compressed =
            zstd::stream::encode_all(content.as_bytes(), 0).expect("compress rank fixture tape");
        std::fs::write(local_tapes.join(format!("{tape_id}.jsonl.zst")), compressed)
            .expect("write rank fixture tape");
        let events =
            engram::tape::event::parse_jsonl_events(content).expect("parse rank fixture events");
        index
            .ingest_tape_events_with_dispatch(
                tape_id,
                &events,
                &[],
                engram::index::lineage::LINK_THRESHOLD_DEFAULT,
            )
            .expect("index rank fixture tape");
        tape_ids.push(tape_id.clone());
    }
    drop(index);
    (caller_home, repo, tape_ids)
}

fn content_addressed_test_tape(events: &[serde_json::Value]) -> (String, String) {
    let content = jsonl(events);
    let tape_id = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
    (tape_id, content)
}

fn jsonl(events: &[serde_json::Value]) -> String {
    events
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn init_git_repo(root: &Path, file: &Path, content: &str) -> String {
    std::fs::create_dir_all(file.parent().expect("source parent"))
        .expect("create source directory");
    std::fs::write(file, content).expect("write query source");
    let output = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(root)
        .output()
        .expect("git is available for exact-rank tests");
    assert!(output.status.success(), "git init failed");
    for args in [
        vec!["config", "user.name", "Exact Rank Test"],
        vec!["config", "user.email", "exact-rank@example.invalid"],
        vec![
            "add",
            file.strip_prefix(root)
                .expect("file under repo")
                .to_str()
                .unwrap(),
        ],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("git is available for exact-rank tests");
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "initial"])
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .expect("git is available for exact-rank tests");
    assert!(
        output.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git is available for exact-rank tests");
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8(output.stdout)
        .expect("git HEAD is UTF-8")
        .trim()
        .to_string()
}

fn set_peer_topology(caller_home: &std::path::Path, peers: serde_json::Value) {
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({"version": 1, "self": "querier", "peers": peers}))
            .expect("serialize peer topology"),
    )
    .expect("write peer topology");
}

fn ingest_owner_test_tape(
    root: &std::path::Path,
    machine: &str,
    tape_id: &str,
    content: &str,
    dispatch_links: &[DispatchLink],
) {
    let db = root.join(format!("{machine}-home/.engram/index.sqlite"));
    let index =
        SqliteIndex::open_writer(db.to_str().expect("owner DB path")).expect("open owner index");
    let events = engram::tape::event::parse_jsonl_events(content).expect("parse owner events");
    index
        .ingest_tape_events_with_dispatch(
            tape_id,
            &events,
            dispatch_links,
            engram::index::lineage::LINK_THRESHOLD_DEFAULT,
        )
        .expect("index owner test tape");
}

#[test]
fn explain_file_range_prioritizes_exact_edits_before_default_page_locally_and_remotely() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let source = (1..=12)
        .map(|line| {
            format!(
                "pub fn exact_span_line_{line}() {{ let value_{line} = {line}; consume(value_{line}); }}\n"
            )
        })
        .collect::<String>();
    let repo = temp.path().join("rank-repo");
    let source_file = repo.join("src/module.rs");
    let source_path = source_file.to_string_lossy().to_string();
    let wrong_path = repo.join("other/module.rs").to_string_lossy().to_string();
    let repo_head = init_git_repo(&repo, &source_file, &source);
    let meta =
        json!({"t":"2026-09-28T09:00:00Z","k":"meta","model":"rank-test","repo_head":repo_head});
    let mut local_tapes = Vec::new();

    let (local_exact_id, local_exact_content) = content_addressed_test_tape(&[
        meta.clone(),
        json!({
            "t":"2026-09-28T10:00:00Z",
            "k":"code.edit",
            "file":source_path,
            "after_range":[1,12],
            "after_text":source,
        }),
    ]);
    local_tapes.push((local_exact_id.clone(), local_exact_content));

    let (local_wrong_path_id, local_wrong_path_content) = content_addressed_test_tape(&[
        meta.clone(),
        json!({
            "t":"2026-09-28T10:02:00Z",
            "k":"code.edit",
            "file":wrong_path,
            "after_range":[1,12],
            "after_text":source,
        }),
    ]);
    local_tapes.push((local_wrong_path_id.clone(), local_wrong_path_content));

    let (local_nonoverlap_id, local_nonoverlap_content) = content_addressed_test_tape(&[
        meta.clone(),
        json!({
            "t":"2026-09-28T10:03:00Z",
            "k":"code.edit",
            "file":source_path,
            "before_range":[30,41],
            "after_range":[30,41],
            "after_text":source,
        }),
    ]);
    local_tapes.push((local_nonoverlap_id.clone(), local_nonoverlap_content));

    for echo in 0..12 {
        let minute = 20 + echo;
        let (tape_id, content) = content_addressed_test_tape(&[
            meta.clone(),
            json!({
                "t":format!("2026-09-28T11:{minute:02}:00Z"),
                "k":"msg.in",
                "content":format!("Echo mentions src/module.rs and the queried text:\n{source}"),
            }),
            json!({
                "t":format!("2026-09-28T11:{minute:02}:01Z"),
                "k":"code.read",
                "file":source_path,
                "range":[1,12],
                "text":source,
            }),
        ]);
        local_tapes.push((tape_id, content));
    }

    let (caller_home, indexed_repo, local_ids) =
        write_ranked_explain_source(temp.path(), &local_tapes);
    assert_eq!(repo, indexed_repo);

    let local_output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["explain", "src/module.rs:1-12"])
        .output()
        .expect("run local exact-span explain");
    assert!(
        local_output.status.success(),
        "local exact-span explain failed: {}",
        String::from_utf8_lossy(&local_output.stderr)
    );
    let local_result: serde_json::Value =
        serde_json::from_slice(&local_output.stdout).expect("local explain JSON");
    assert_eq!(local_result["total"], 15);
    assert_eq!(local_result["returned"], 10);
    assert_eq!(local_result["truncated"], true);
    assert_eq!(local_result["sessions"][0]["session_id"], local_exact_id);
    assert_ne!(
        local_result["sessions"][0]["session_id"],
        local_wrong_path_id
    );
    assert_ne!(
        local_result["sessions"][0]["session_id"],
        local_nonoverlap_id
    );
    assert_eq!(local_result["dispatch_lineage"], json!([]));
    assert!(local_result.get("dispatch_unresolved").is_none());
    assert_eq!(local_ids.len(), 15);

    let local_all_output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["explain", "src/module.rs:1-12", "--limit", "20"])
        .output()
        .expect("run full local exact-span explain");
    assert!(
        local_all_output.status.success(),
        "full local exact-span explain failed: {}",
        String::from_utf8_lossy(&local_all_output.stderr)
    );
    let local_all: serde_json::Value =
        serde_json::from_slice(&local_all_output.stdout).expect("full local explain JSON");
    let local_all_ids = local_all["sessions"]
        .as_array()
        .expect("local sessions")
        .iter()
        .filter_map(|session| session["session_id"].as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(local_all_ids.len(), 15);
    assert!(local_all_ids.contains(local_wrong_path_id.as_str()));
    assert!(local_all_ids.contains(local_nonoverlap_id.as_str()));

    let (remote_exact_id, remote_exact_content) = content_addressed_test_tape(&[
        meta.clone(),
        json!({
            "t":"2026-09-28T12:00:00Z",
            "k":"code.edit",
            "file":source_path,
            "after_range":[1,12],
            "after_text":source,
        }),
    ]);
    let (remote_wrong_path_id, remote_wrong_path_content) = content_addressed_test_tape(&[
        meta.clone(),
        json!({
            "t":"2026-09-28T13:00:00Z",
            "k":"code.edit",
            "file":wrong_path,
            "after_range":[1,12],
            "after_text":source,
        }),
    ]);
    let (remote_nonoverlap_id, remote_nonoverlap_content) = content_addressed_test_tape(&[
        meta,
        json!({
            "t":"2026-09-28T14:00:00Z",
            "k":"code.edit",
            "file":source_path,
            "before_range":[30,41],
            "after_range":[30,41],
            "after_text":source,
        }),
    ]);
    let remote_tapes = [
        (remote_exact_id.as_str(), remote_exact_content.as_str()),
        (
            remote_wrong_path_id.as_str(),
            remote_wrong_path_content.as_str(),
        ),
        (
            remote_nonoverlap_id.as_str(),
            remote_nonoverlap_content.as_str(),
        ),
    ];
    let remote_peer = write_grep_owner(temp.path(), "alpha", binary, &remote_tapes);
    for (tape_id, content) in &remote_tapes {
        ingest_owner_test_tape(temp.path(), "alpha", tape_id, content, &[]);
    }
    set_peer_topology(&caller_home, json!({"alpha": remote_peer}));

    let federated_output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "explain",
            "src/module.rs:1-12",
            "--peers",
            "alpha",
        ])
        .output()
        .expect("run federated exact-span explain");
    assert!(
        federated_output.status.success(),
        "federated exact-span explain failed: {}",
        String::from_utf8_lossy(&federated_output.stderr)
    );
    let federated_result: serde_json::Value =
        serde_json::from_slice(&federated_output.stdout).expect("federated explain JSON");
    assert_eq!(federated_result["federation"]["coverage"], "complete");
    assert_eq!(
        federated_result["later_discussion"]["status"],
        "partial",
        "relaxed explain keeps core peer coverage while disclosing the unsupported optional scan"
    );
    assert!(
        federated_result["later_discussion"]["coverage"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["store"] == "alpha/default"
                && source["status"] == "unsupported")
    );
    assert_eq!(federated_result["total"], 18);
    assert_eq!(federated_result["returned"], 10);
    assert_eq!(federated_result["truncated"], true);
    // A different machine cannot prove the caller's local Git/worktree
    // identity for a peer's physical source path. Keep the local exact match
    // first and retain the remote session as ordinary evidence.
    assert_eq!(
        federated_result["sessions"][0]["session_id"],
        local_exact_id
    );
    assert!(
        federated_result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["session_id"] == remote_exact_id)
    );
    let remote_exact_position = federated_result["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .position(|session| session["session_id"] == remote_exact_id)
        .expect("remote exact-shaped edit remains in ordinary results");
    assert!(
        remote_exact_position > 0,
        "unknown remote path identity must not receive exact-span priority"
    );
    let remote_exact = &federated_result["sessions"][remote_exact_position];
    assert_eq!(remote_exact["evidence"]["label"], "file_identity_unknown");
    assert_eq!(remote_exact["evidence"]["file_identity"], "unknown");
    assert_eq!(remote_exact["evidence"]["matched_kind"], "edit");
    assert!(remote_exact["tape_facts"].get("span_edit_match").is_none());
    assert!(
        federated_result["sessions"][0]["tape_facts"]
            .get("span_edit_match")
            .is_none()
    );
    assert_eq!(federated_result["dispatch_lineage"], json!([]));
    assert_eq!(federated_result["dispatch_unresolved"], json!([]));
    assert_eq!(federated_result["dispatch_ambiguous"], json!([]));

    let strict_output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "explain",
            "src/module.rs:1-12",
            "--peers",
            "alpha",
            "--require-complete",
        ])
        .output()
        .expect("run strict federated exact-span explain");
    assert!(!strict_output.status.success());
    let strict_error = String::from_utf8_lossy(&strict_output.stderr);
    assert!(strict_error.contains("incomplete_coverage"), "{strict_error}");
    assert!(strict_error.contains("alpha/default"), "{strict_error}");
    assert!(strict_error.contains("later_discussion_content_scan"), "{strict_error}");

    let federated_all_output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "explain",
            "src/module.rs:1-12",
            "--peers",
            "alpha",
            "--limit",
            "25",
        ])
        .output()
        .expect("run full federated exact-span explain");
    assert!(
        federated_all_output.status.success(),
        "full federated exact-span explain failed: {}",
        String::from_utf8_lossy(&federated_all_output.stderr)
    );
    let federated_all: serde_json::Value =
        serde_json::from_slice(&federated_all_output.stdout).expect("full federated explain JSON");
    let federated_all_ids = federated_all["sessions"]
        .as_array()
        .expect("federated sessions")
        .iter()
        .filter_map(|session| session["session_id"].as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(federated_all_ids.len(), 18);
    assert!(federated_all_ids.contains(local_exact_id.as_str()));
    assert!(federated_all_ids.contains(remote_wrong_path_id.as_str()));
    assert!(federated_all_ids.contains(remote_nonoverlap_id.as_str()));
    let remote_exact_session = federated_all["sessions"]
        .as_array()
        .expect("federated sessions")
        .iter()
        .find(|session| session["session_id"] == remote_exact_id)
        .expect("remote exact-shaped session");
    assert_eq!(
        remote_exact_session["evidence"]["label"],
        "file_identity_unknown"
    );
    assert_eq!(remote_exact_session["evidence"]["file_identity"], "unknown");
    assert_eq!(remote_exact_session["evidence"]["matched_kind"], "edit");
    assert!(remote_exact_session["tape_facts"]["span_edit_match"].is_null());
    assert_eq!(
        remote_exact_session["evidence"]["source_revision"],
        repo_head
    );
}

#[test]
fn federated_match_strength_precedes_weak_read_volume_and_recency() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let span = r##"# ELIXIR_ERL_OPTIONS would leak the name into child BEAMs spawned by tests.
gate_marker=$(mktemp -d "${TMPDIR:-/tmp}/tightbeam-mix-gate.XXXXXXXX")
trap 'rmdir "$gate_marker" 2>/dev/null || :' EXIT HUP INT TERM
gate_node="tightbeam_mix_gate_${gate_marker##*.}"

export TIGHTBEAM_AUTHORITATIVE_GATE=1
export TIGHTBEAM_GATE_NODE="$gate_node""##;
    let prefix = (1..=18)
        .map(|line| format!("# prefix {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let source = format!("{prefix}\n{span}\n");
    let meta = json!({"t":"2026-08-09T02:00:00Z","k":"meta","model":"rank-test"});
    let (strong_id, strong_content) = content_addressed_test_tape(&[
        meta.clone(),
        json!({
            "t":"2026-08-09T02:40:31Z",
            "k":"code.read",
            "file":"scripts/verify_mix.sh",
            "range":[19,25],
            "text":span,
        }),
    ]);
    let (caller_home, repo, _) =
        write_ranked_explain_source(temp.path(), &[(strong_id.clone(), strong_content)]);
    let source_file = repo.join("scripts/verify_mix.sh");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source directory");
    std::fs::write(&source_file, source).expect("write query source");

    let weak_line = r##"gate_node="tightbeam_mix_gate_${gate_marker##*.}""##;
    let mut weak_events = vec![meta];
    for minute in 0..8 {
        weak_events.push(json!({
            "t":format!("2026-08-13T10:{minute:02}:00Z"),
            "k":"code.read",
            "file":"scripts/verify_mix.sh",
            "range":[22,22],
            "text":weak_line,
        }));
    }
    let (weak_id, weak_content) = content_addressed_test_tape(&weak_events);
    let remote_tapes = [(weak_id.as_str(), weak_content.as_str())];
    let remote_peer = write_grep_owner(temp.path(), "alpha", binary, &remote_tapes);
    ingest_owner_test_tape(
        temp.path(),
        "alpha",
        weak_id.as_str(),
        weak_content.as_str(),
        &[],
    );
    set_peer_topology(&caller_home, json!({"alpha": remote_peer}));

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "explain",
            "scripts/verify_mix.sh:19-25",
            "--peers",
            "alpha",
            "--require-complete",
        ])
        .output()
        .expect("run federated weak-read/strong-match explain");
    assert!(
        output.status.success(),
        "federated explain failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("federated explain JSON");
    assert_eq!(result["federation"]["coverage"], "complete");
    assert_eq!(result["total"], 2);
    let sessions = result["sessions"].as_array().expect("sessions");
    let strong = sessions
        .iter()
        .find(|session| session["session_id"] == strong_id)
        .expect("strong local match");
    let weak = sessions
        .iter()
        .find(|session| session["session_id"] == weak_id)
        .expect("weak remote reads");
    assert!(strong["confidence"].as_f64().unwrap() > weak["confidence"].as_f64().unwrap());
    assert!(
        weak["touches"].as_array().unwrap().len() > strong["touches"].as_array().unwrap().len()
    );
    assert_eq!(sessions[0]["session_id"], strong_id);
    assert!(strong["tape_facts"].get("span_edit_match").is_none());
    assert!(weak["tape_facts"].get("span_edit_match").is_none());
}
