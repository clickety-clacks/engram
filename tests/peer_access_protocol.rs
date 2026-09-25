use std::process::Command;
use std::time::Duration;

use engram::access::client::{PeerRequest, RemoteOwner};
use engram::config::TopologyPeer;
use engram::index::{
    DispatchDirection, DispatchLink, QUERY_SEMANTICS_VERSION, SCHEMA_VERSION, SqliteIndex,
};
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

fn write_local_grep_source(
    root: &std::path::Path,
    tape_id: &str,
    content: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let caller_home = root.join("caller-home");
    let caller_engram = caller_home.join(".engram");
    std::fs::create_dir_all(&caller_engram).expect("caller home");
    let repo = root.join("repo");
    let local_engram = repo.join(".engram");
    let local_tapes = local_engram.join("tapes");
    std::fs::create_dir_all(&local_tapes).expect("local tapes");
    let local_db = local_engram.join("index.sqlite");
    drop(SqliteIndex::open_writer(local_db.to_str().expect("local DB path")).expect("local DB"));
    std::fs::write(
        caller_engram.join("config.yml"),
        format!(
            "db: {}\ntapes_dir: {}\n",
            local_db.display(),
            local_tapes.display()
        ),
    )
    .expect("caller config");
    let compressed = zstd::stream::encode_all(content.as_bytes(), 0).expect("compress local tape");
    std::fs::write(local_tapes.join(format!("{tape_id}.jsonl.zst")), compressed)
        .expect("write local tape");
    (caller_home, repo)
}

#[test]
fn command_peer_runs_real_peer_serve_against_an_isolated_owner_home() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("owner-home");
    let engram_home = home.join(".engram");
    let tapes = engram_home.join("tapes");
    std::fs::create_dir_all(&tapes).expect("tape directory");
    let db = engram_home.join("index.sqlite");
    drop(SqliteIndex::open_writer(db.to_str().expect("UTF-8 DB path")).expect("create fixture DB"));

    let tape_id = "fixture-1";
    let tape = tapes.join(format!("{tape_id}.jsonl.zst"));
    std::fs::write(&tape, b"compressed fixture placeholder").expect("tape fixture");
    std::fs::write(
        engram_home.join("topology.yml"),
        format!(
            "version: 1\nself: emulated-owner\nexports:\n  default:\n    db: {}\n    tape_dirs:\n      - {}\n",
            db.display(),
            tapes.display()
        ),
    )
    .expect("owner topology");

    let home_arg = format!("HOME={}", home.display());
    let peer = TopologyPeer {
        ssh: None,
        command: Some(vec![
            "/usr/bin/env".into(),
            home_arg,
            env!("CARGO_BIN_EXE_engram").into(),
            "peer-serve".into(),
            "--stdio".into(),
        ]),
        engram: env!("CARGO_BIN_EXE_engram").into(),
        exports: vec!["default".into()],
    };

    let mut owner = RemoteOwner::connect("emulated-owner", "caller", &peer, Duration::from_secs(5))
        .expect("real peer handshake");
    assert!(owner.exports["default"].is_ok());

    let mut outcomes = owner.round(
        &[PeerRequest::new(
            "locate_tapes",
            vec!["default".into()],
            json!({"tape_ids":[tape_id, "missing"]}),
        )],
        Duration::from_secs(5),
    );
    let response = outcomes
        .pop()
        .expect("one operation result")
        .expect("locate operation succeeds");
    assert_eq!(response.data.len(), 2);
    assert_eq!(response.data[0]["tape_id"], tape_id);
    assert_eq!(response.data[0]["file"]["path"].as_str(), tape.to_str());
    assert_eq!(
        response.data[0]["size_bytes"].as_u64(),
        Some(b"compressed fixture placeholder".len() as u64)
    );
    assert_eq!(response.data[1]["tape_id"], "missing");
    assert!(response.data[1]["file"].is_null());

    assert_eq!(
        owner.exports["default"].as_ref().unwrap().db,
        db.to_str().expect("UTF-8 DB path")
    );
}

#[test]
fn remote_show_reads_only_the_selected_peer_tape_and_verifies_its_digest() {
    let temp = tempfile::tempdir().expect("tempdir");
    let owner_home = temp.path().join("owner-home");
    let owner_engram = owner_home.join(".engram");
    let tapes = owner_engram.join("tapes");
    std::fs::create_dir_all(&tapes).expect("owner tape directory");
    let db = owner_engram.join("index.sqlite");
    drop(SqliteIndex::open_writer(db.to_str().expect("UTF-8 DB path")).expect("create owner DB"));

    let content = concat!(
        "{\"t\":\"2026-09-23T12:34:56Z\",\"k\":\"msg.in\",\"content\":\"first line\"}\n",
        "{\"t\":\"2026-09-23T12:34:57Z\",\"k\":\"msg.out\",\"content\":\"second marker\"}\n",
        "{\"t\":\"2026-09-23T12:34:58Z\",\"k\":\"msg.in\",\"content\":\"third marker\"}\n",
        "{\"t\":\"2026-09-23T12:34:59Z\",\"k\":\"note\",\"content\":\"small filler\"}\n",
    )
    .to_string()
        + &format!(
            "{{\"t\":\"2026-09-23T12:35:00Z\",\"k\":\"note\",\"content\":\"{}\"}}\n",
            "x".repeat(1_500)
        );
    let tape_id = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
    let compressed = zstd::stream::encode_all(content.as_bytes(), 0).expect("compress tape");
    let tape = tapes.join(format!("{tape_id}.jsonl.zst"));
    std::fs::write(&tape, &compressed).expect("write owner tape");
    let index = SqliteIndex::open_writer(db.to_str().expect("UTF-8 DB path"))
        .expect("open owner index to seed dispatch row");
    index
        .insert_dispatch_link(
            &tape_id,
            &DispatchLink {
                uuid: "fixture-dispatch".into(),
                first_turn_index: 2,
                direction: DispatchDirection::Received,
            },
        )
        .expect("seed received dispatch row");
    drop(index);
    let opaque_id = "opaque-fixture";
    let opaque_tape = tapes.join(format!("{opaque_id}.jsonl.zst"));
    std::fs::write(&opaque_tape, compressed).expect("write opaque owner tape");
    std::fs::write(
        owner_engram.join("topology.yml"),
        format!(
            "version: 1\nself: emulated-owner\nexports:\n  default:\n    db: {}\n    tape_dirs:\n      - {}\nlimits:\n  non_file_response_bytes: 1024\n",
            db.display(),
            tapes.display()
        ),
    )
    .expect("write owner topology");

    let caller_home = temp.path().join("caller-home");
    let caller_engram = caller_home.join(".engram");
    std::fs::create_dir_all(&caller_engram).expect("caller config directory");
    std::fs::write(
        caller_engram.join("config.yml"),
        "db: ~/.engram/index.sqlite\ntapes_dir: ~/.engram/tapes\npeek:\n  grep_context: 1\n",
    )
    .expect("write caller config");
    let binary = env!("CARGO_BIN_EXE_engram");
    let peer_command = vec![
        "/usr/bin/env".to_string(),
        format!("HOME={}", owner_home.display()),
        binary.to_string(),
        "peer-serve".to_string(),
        "--stdio".to_string(),
    ];
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "emulated-owner": {
                    "command": peer_command,
                    "engram": binary,
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("caller repo");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["show", &tape_id, "--store", "emulated-owner/default"])
        .output()
        .expect("run remote show");
    assert!(
        output.status.success(),
        "remote show failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("show JSON");
    assert_eq!(value["tape_id"], tape_id);
    assert!(value["path"].is_null());
    assert_eq!(value["location"]["machine"], "emulated-owner");
    assert_eq!(value["location"]["store"], "emulated-owner/default");
    assert_eq!(
        value["location"]["path"],
        tape.to_str().expect("UTF-8 tape path")
    );
    assert_eq!(value["digest"], tape_id);
    assert_eq!(value["id_verified"], true);
    assert_eq!(value["event_count"], 5);
    assert!(String::from_utf8_lossy(&output.stderr).contains("peers=emulated-owner"));
    assert!(
        !repo.join(".engram").exists(),
        "remote query created caller store files"
    );

    let explicit_peek = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "peek",
            &tape_id,
            "--store",
            "emulated-owner/default",
            "--start",
            "2",
            "--lines",
            "1",
        ])
        .output()
        .expect("run explicit remote peek");
    assert!(
        explicit_peek.status.success(),
        "explicit remote peek failed: {}",
        String::from_utf8_lossy(&explicit_peek.stderr)
    );
    let explicit_value: serde_json::Value =
        serde_json::from_slice(&explicit_peek.stdout).expect("explicit peek JSON");
    assert_eq!(explicit_value["session"]["content"][0]["line"], 2);
    assert_eq!(
        explicit_value["session"]["content"][0]["text"],
        "{\"t\":\"2026-09-23T12:34:57Z\",\"k\":\"msg.out\",\"content\":\"second marker\"}"
    );
    assert_eq!(
        explicit_value["session"]["location"]["machine"],
        "emulated-owner"
    );

    let anchor_peek = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "peek",
            &tape_id,
            "--store",
            "emulated-owner/default",
            "--before",
            "0",
            "--after",
            "0",
        ])
        .output()
        .expect("run default-anchor remote peek");
    assert!(
        anchor_peek.status.success(),
        "default-anchor remote peek failed: {}",
        String::from_utf8_lossy(&anchor_peek.stderr)
    );
    let anchor_value: serde_json::Value =
        serde_json::from_slice(&anchor_peek.stdout).expect("anchor peek JSON");
    assert_eq!(anchor_value["session"]["window_start"], 3);
    assert_eq!(anchor_value["session"]["window_end"], 3);
    assert_eq!(anchor_value["session"]["content"][0]["line"], 3);

    let grep_peek = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "peek",
            &tape_id,
            "--store",
            "emulated-owner/default",
            "--grep-filter",
            "third marker",
        ])
        .output()
        .expect("run remote grep peek");
    assert!(
        grep_peek.status.success(),
        "remote grep peek failed: {}",
        String::from_utf8_lossy(&grep_peek.stderr)
    );
    let grep_value: serde_json::Value =
        serde_json::from_slice(&grep_peek.stdout).expect("grep peek JSON");
    assert_eq!(grep_value["session"]["content"][1]["line"], 3);

    let oversized_peek = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "peek",
            &tape_id,
            "--store",
            "emulated-owner/default",
            "--start",
            "5",
            "--lines",
            "1",
        ])
        .output()
        .expect("run over-budget remote peek");
    assert!(!oversized_peek.status.success());
    assert!(
        oversized_peek.stdout.is_empty(),
        "over-budget peek emitted partial lines"
    );
    let stderr = String::from_utf8_lossy(&oversized_peek.stderr);
    let error: serde_json::Value =
        serde_json::from_str(stderr.lines().last().expect("budget error line"))
            .expect("budget error JSON");
    assert_eq!(error["error"]["code"], "budget_exceeded");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("narrow")
    );
    assert!(
        !repo.join(".engram").exists(),
        "remote peek created caller store files"
    );

    let raw = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "show",
            &tape_id,
            "--store",
            "emulated-owner/default",
            "--raw",
        ])
        .output()
        .expect("run raw remote show");
    assert!(raw.status.success());
    assert_eq!(raw.stdout, content.as_bytes());

    let opaque = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["show", opaque_id, "--store", "emulated-owner/default"])
        .output()
        .expect("run opaque remote show");
    assert!(
        opaque.status.success(),
        "opaque remote show failed: {}",
        String::from_utf8_lossy(&opaque.stderr)
    );
    let opaque_value: serde_json::Value =
        serde_json::from_slice(&opaque.stdout).expect("opaque show JSON");
    assert_eq!(opaque_value["digest"], tape_id);
    assert_eq!(opaque_value["id_verified"], false);
    assert_eq!(
        opaque_value["location"]["path"],
        opaque_tape.to_str().unwrap()
    );
}

#[test]
fn command_peer_grep_scan_returns_ranked_page_and_per_tape_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    let owner_home = temp.path().join("owner-home");
    let owner_engram = owner_home.join(".engram");
    let tapes = owner_engram.join("tapes");
    std::fs::create_dir_all(&tapes).expect("owner tape directory");
    let db = owner_engram.join("index.sqlite");
    drop(SqliteIndex::open_writer(db.to_str().expect("UTF-8 DB path")).expect("create owner DB"));

    let provenance = concat!(
        "{\"t\":\"2026-09-24T12:34:56Z\",\"k\":\"code.edit\",\"file\":\"src/a.rs\",\"after_text\":\"needle in source\"}\n",
        "{\"t\":\"2026-09-24T12:35:00Z\",\"k\":\"msg.in\",\"content\":\"needle in discussion\"}\n",
    );
    let mentions = concat!(
        "{\"t\":\"2026-09-25T12:34:56Z\",\"k\":\"note\",\"content\":\"needle one\"}\n",
        "{\"t\":\"2026-09-25T12:35:00Z\",\"k\":\"note\",\"content\":\"needle two\"}\n",
    );
    for (tape_id, content) in [("a", provenance), ("b", mentions)] {
        let compressed = zstd::stream::encode_all(content.as_bytes(), 0).expect("compress tape");
        std::fs::write(tapes.join(format!("{tape_id}.jsonl.zst")), compressed)
            .expect("write owner tape");
    }
    std::fs::write(tapes.join("broken.jsonl.zst"), b"not a zstd tape")
        .expect("write malformed tape");
    std::fs::write(
        owner_engram.join("topology.yml"),
        format!(
            "version: 1\nself: emulated-owner\nexports:\n  default:\n    db: {}\n    tape_dirs:\n      - {}\nlimits:\n  non_file_response_bytes: 8388608\n",
            db.display(),
            tapes.display()
        ),
    )
    .expect("write owner topology");

    let peer = TopologyPeer {
        ssh: None,
        command: Some(vec![
            "/usr/bin/env".into(),
            format!("HOME={}", owner_home.display()),
            env!("CARGO_BIN_EXE_engram").into(),
            "peer-serve".into(),
            "--stdio".into(),
        ]),
        engram: env!("CARGO_BIN_EXE_engram").into(),
        exports: vec!["default".into()],
    };
    let mut owner = RemoteOwner::connect("emulated-owner", "caller", &peer, Duration::from_secs(5))
        .expect("real peer handshake");
    let response = owner
        .round(
            &[PeerRequest::new(
                "grep_scan",
                vec!["default".into()],
                json!({"pattern":"needle", "k":1}),
            )],
            Duration::from_secs(5),
        )
        .pop()
        .expect("grep outcome")
        .expect("grep operation succeeds with per-tape failure");

    assert_eq!(response.stats["total"], 2);
    assert_eq!(response.stats["returned"], 1);
    assert_eq!(response.stats["truncated"], true);
    assert_eq!(response.stats["failures"], 1);
    assert_eq!(
        response.stats["time_range"]["start"],
        "2026-09-24T12:35:00Z"
    );
    assert_eq!(response.stats["time_range"]["end"], "2026-09-25T12:35:00Z");
    assert_eq!(response.data[0]["type"], "match");
    assert_eq!(response.data[0]["tape_id"], "a");
    assert_eq!(response.data[0]["indexed"], false);
    assert_eq!(response.data[0]["match_count"], 2);
    assert_eq!(response.data[0]["provenance_match_count"], 1);
    assert_eq!(response.data[0]["anchor_line"], 1);
    assert_eq!(response.data[0]["files_touched"], json!(["src/a.rs"]));
    assert_eq!(response.data[1]["type"], "failure");
    assert_eq!(response.data[1]["tape_id"], "broken");
    assert_eq!(response.data[1]["error"]["code"], "invalid_tape");
}

#[test]
fn remote_show_rejects_a_corrupted_content_addressed_tape() {
    let temp = tempfile::tempdir().expect("tempdir");
    let owner_home = temp.path().join("owner-home");
    let owner_engram = owner_home.join(".engram");
    let tapes = owner_engram.join("tapes");
    std::fs::create_dir_all(&tapes).expect("owner tape directory");
    let db = owner_engram.join("index.sqlite");
    drop(SqliteIndex::open_writer(db.to_str().expect("UTF-8 DB path")).expect("create owner DB"));

    let tape_id = "a".repeat(64);
    let content = "{\"t\":\"2026-09-23T12:34:56Z\",\"k\":\"msg.in\",\"content\":\"corrupt\"}\n";
    let compressed = zstd::stream::encode_all(content.as_bytes(), 0).expect("compress tape");
    std::fs::write(tapes.join(format!("{tape_id}.jsonl.zst")), compressed)
        .expect("write mismatched tape");
    std::fs::write(
        owner_engram.join("topology.yml"),
        format!(
            "version: 1\nself: emulated-owner\nexports:\n  default:\n    db: {}\n    tape_dirs:\n      - {}\n",
            db.display(),
            tapes.display()
        ),
    )
    .expect("write owner topology");

    let caller_home = temp.path().join("caller-home");
    let caller_engram = caller_home.join(".engram");
    std::fs::create_dir_all(&caller_engram).expect("caller config directory");
    std::fs::write(
        caller_engram.join("config.yml"),
        "db: ~/.engram/index.sqlite\ntapes_dir: ~/.engram/tapes\n",
    )
    .expect("write caller config");
    let binary = env!("CARGO_BIN_EXE_engram");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "emulated-owner": {
                    "command": [
                        "/usr/bin/env",
                        format!("HOME={}", owner_home.display()),
                        binary,
                        "peer-serve",
                        "--stdio",
                    ],
                    "engram": binary,
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("caller repo");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["show", &tape_id, "--store", "emulated-owner/default"])
        .output()
        .expect("run remote show");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let error_line = stderr.lines().last().expect("error output");
    let error: serde_json::Value = serde_json::from_str(error_line)
        .unwrap_or_else(|_| panic!("error JSON not found in stderr: {stderr}"));
    assert_eq!(error["error"]["code"], "id_mismatch");
}

#[test]
fn grep_with_one_selected_peer_merges_local_and_remote_results() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let owner_home = temp.path().join("owner-home");
    let owner_engram = owner_home.join(".engram");
    let owner_tapes = owner_engram.join("tapes");
    std::fs::create_dir_all(&owner_tapes).expect("owner tape directory");
    let owner_db = owner_engram.join("index.sqlite");
    drop(SqliteIndex::open_writer(owner_db.to_str().expect("owner DB path")).expect("owner DB"));
    let remote_id = "remote-grep-session";
    let remote_content = "{\"t\":\"2026-09-24T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-federated remote\"}\n";
    let compressed =
        zstd::stream::encode_all(remote_content.as_bytes(), 0).expect("compress remote tape");
    std::fs::write(
        owner_tapes.join(format!("{remote_id}.jsonl.zst")),
        compressed,
    )
    .expect("write remote tape");
    let shared_content = "{\"t\":\"2026-09-24T11:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-federated shared\"}\n";
    let compressed =
        zstd::stream::encode_all(shared_content.as_bytes(), 0).expect("compress shared tape");
    std::fs::write(owner_tapes.join("local-grep-session.jsonl.zst"), compressed)
        .expect("write duplicate remote tape");
    let conflicting_content = concat!(
        "{\"t\":\"2026-09-24T10:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-federated conflict\"}\n",
        "{\"t\":\"2026-09-24T10:01:00Z\",\"k\":\"msg.in\",\"content\":\"needle-federated conflict again\"}\n",
    );
    let compressed = zstd::stream::encode_all(conflicting_content.as_bytes(), 0)
        .expect("compress conflicting tape");
    std::fs::write(owner_tapes.join("collision-session.jsonl.zst"), compressed)
        .expect("write conflicting remote tape");
    std::fs::write(
        owner_engram.join("topology.yml"),
        format!(
            "version: 1\nself: emulated-owner\nexports:\n  default:\n    db: {}\n    tape_dirs:\n      - {}\n",
            owner_db.display(),
            owner_tapes.display()
        ),
    )
    .expect("owner topology");

    let caller_home = temp.path().join("caller-home");
    let caller_engram = caller_home.join(".engram");
    std::fs::create_dir_all(&caller_engram).expect("caller home");
    let repo = temp.path().join("repo");
    let local_engram = repo.join(".engram");
    let local_tapes = local_engram.join("tapes");
    std::fs::create_dir_all(&local_tapes).expect("local tapes");
    let local_db = local_engram.join("index.sqlite");
    drop(SqliteIndex::open_writer(local_db.to_str().expect("local DB path")).expect("local DB"));
    std::fs::write(
        caller_engram.join("config.yml"),
        format!(
            "db: {}\ntapes_dir: {}\n",
            local_db.display(),
            local_tapes.display()
        ),
    )
    .expect("local config");
    let local_id = "local-grep-session";
    let compressed =
        zstd::stream::encode_all(shared_content.as_bytes(), 0).expect("compress local tape");
    std::fs::write(
        local_tapes.join(format!("{local_id}.jsonl.zst")),
        compressed,
    )
    .expect("write local tape");
    let local_conflict = "{\"t\":\"2026-09-24T10:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-federated conflict\"}\n";
    let compressed = zstd::stream::encode_all(local_conflict.as_bytes(), 0)
        .expect("compress local conflicting tape");
    std::fs::write(local_tapes.join("collision-session.jsonl.zst"), compressed)
        .expect("write local conflicting tape");

    let unselected_marker = temp.path().join("unselected-peer-was-started");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "emulated-owner": {
                    "command": ["/usr/bin/env", format!("HOME={}", owner_home.display()), binary, "peer-serve", "--stdio"],
                    "engram": binary,
                    "exports": ["default"],
                },
                "not-selected": {
                    "command": ["/usr/bin/touch", unselected_marker],
                    "engram": "/unused/engram",
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize caller topology"),
    )
    .expect("caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle-federated", "--peers", "emulated-owner"])
        .output()
        .expect("run federated grep");
    assert!(
        output.status.success(),
        "federated grep failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    let sessions = result["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 4);
    let local = sessions
        .iter()
        .find(|session| session["tape_id"] == local_id)
        .expect("local result");
    assert_eq!(local["location"]["machine"], "caller");
    assert_eq!(local["locations"].as_array().unwrap().len(), 2);
    let remote = sessions
        .iter()
        .find(|session| session["tape_id"] == remote_id)
        .expect("remote result");
    assert_eq!(remote["location"]["machine"], "emulated-owner");
    assert_eq!(remote["location"]["store"], "emulated-owner/default");
    assert_eq!(
        result["federation"]["identity_conflicts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        sessions
            .iter()
            .any(|session| { session["session_id"] == "collision-session@caller/local:0" })
    );
    assert!(
        sessions
            .iter()
            .any(|session| { session["session_id"] == "collision-session@emulated-owner/default" })
    );
    assert_eq!(result["federation"]["coverage"], "complete");
    assert_eq!(result["total"], 4);
    assert!(
        result["federation"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["store"] == "not-selected/default"
                && source["status"] == "not_selected")
    );
    assert!(!unselected_marker.exists(), "unselected peer was launched");
}

#[test]
fn grep_merges_multiple_explicit_peers_and_keeps_unselected_peers_idle() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let alpha = write_grep_owner(
        temp.path(),
        "alpha",
        binary,
        &[
            (
                "alpha-tape",
                "{\"t\":\"2026-09-24T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-multi alpha\"}\n",
            ),
            (
                "shared-tape",
                "{\"t\":\"2026-09-24T11:30:00Z\",\"k\":\"msg.in\",\"content\":\"needle-multi shared\"}\n",
            ),
        ],
    );
    let beta = write_grep_owner(
        temp.path(),
        "beta",
        binary,
        &[
            (
                "beta-tape",
                "{\"t\":\"2026-09-24T13:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-multi beta\"}\n",
            ),
            (
                "shared-tape",
                "{\"t\":\"2026-09-24T11:30:00Z\",\"k\":\"msg.in\",\"content\":\"needle-multi shared\"}\n",
            ),
        ],
    );
    let unselected_marker = temp.path().join("unselected-peer-was-started");
    let peers = json!({
        "alpha": alpha,
        "beta": beta,
        "not-selected": {
            "command": ["/usr/bin/touch", unselected_marker],
            "engram": "/unused/engram",
            "exports": ["default"],
        }
    });
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-tape",
        "{\"t\":\"2026-09-24T11:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-multi caller\"}\n",
    );
    let caller_engram = caller_home.join(".engram");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&json!({"version": 1, "self": "caller", "peers": peers}))
            .expect("serialize topology"),
    )
    .expect("caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle-multi", "--peers", "beta, alpha"])
        .output()
        .expect("run multi-peer grep");
    assert!(
        output.status.success(),
        "multi-peer grep failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    let sessions = result["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 4);
    let local = sessions
        .iter()
        .find(|session| session["tape_id"] == "caller-tape")
        .expect("local result");
    assert_eq!(local["location"]["machine"], "caller");
    assert_eq!(local["location"]["store"], "caller/local:0");
    for (tape_id, machine) in [("alpha-tape", "alpha"), ("beta-tape", "beta")] {
        let session = sessions
            .iter()
            .find(|session| session["tape_id"] == tape_id)
            .expect("peer result");
        assert_eq!(session["location"]["machine"], machine);
        assert_eq!(session["location"]["store"], format!("{machine}/default"));
    }
    let shared = sessions
        .iter()
        .find(|session| session["tape_id"] == "shared-tape")
        .expect("deduplicated peer result");
    assert_eq!(shared["locations"].as_array().unwrap().len(), 2);
    assert_eq!(result["truncated"], false);
    assert!(result.get("total_bounds").is_none());
    assert_eq!(result["federation"]["coverage"], "complete");
    assert_eq!(result["stores_queried"], 3);
    assert_eq!(result["total"], 4);
    assert_eq!(result["time_range"]["start"], "2026-09-24T11:00:00Z");
    assert_eq!(result["time_range"]["end"], "2026-09-24T13:00:00Z");
    let sources = result["federation"]["sources"].as_array().expect("sources");
    for machine in ["alpha", "beta"] {
        let source = sources
            .iter()
            .find(|source| source["store"] == format!("{machine}/default"))
            .expect("selected source");
        assert_eq!(source["status"], "ok");
        assert_eq!(source["grep_scan"]["total"], 2);
        assert_eq!(source["grep_scan"]["returned"], 2);
    }
    assert!(sources.iter().any(|source| {
        source["store"] == "not-selected/default" && source["status"] == "not_selected"
    }));
    assert!(!unselected_marker.exists(), "unselected peer was launched");
}

#[test]
fn grep_connects_selected_peers_concurrently_before_scanning() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let mut alpha = write_grep_owner(
        temp.path(),
        "alpha",
        binary,
        &[(
            "alpha-tape",
            "{\"t\":\"2026-09-24T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-concurrent alpha\"}\n",
        )],
    );
    let mut beta = write_grep_owner(
        temp.path(),
        "beta",
        binary,
        &[(
            "beta-tape",
            "{\"t\":\"2026-09-24T13:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-concurrent beta\"}\n",
        )],
    );
    let gate = temp.path().join("peer-start-gate.sh");
    std::fs::write(
        &gate,
        "#!/bin/sh\nset -eu\nmine=\"$1\"\nother=\"$2\"\nshift 2\ntouch \"$mine\"\ni=0\nwhile [ \"$i\" -lt 100 ]; do\n  if [ -e \"$other\" ]; then exec \"$@\"; fi\n  sleep 0.01\n  i=$((i + 1))\ndone\nexit 79\n",
    )
    .expect("write peer start gate");
    let alpha_started = temp.path().join("alpha-started");
    let beta_started = temp.path().join("beta-started");
    let gated_command = |machine: &str, mine: &std::path::Path, other: &std::path::Path| {
        let owner_home = temp.path().join(format!("{machine}-home"));
        json!([
            "/bin/sh",
            gate,
            mine,
            other,
            "/usr/bin/env",
            format!("HOME={}", owner_home.display()),
            binary,
            "peer-serve",
            "--stdio",
        ])
    };
    alpha["command"] = gated_command("alpha", &alpha_started, &beta_started);
    beta["command"] = gated_command("beta", &beta_started, &alpha_started);

    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-tape",
        "{\"t\":\"2026-09-24T11:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-concurrent caller\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {"alpha": alpha, "beta": beta}
        }))
        .expect("serialize caller topology"),
    )
    .expect("caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle-concurrent", "--peers", "alpha,beta"])
        .output()
        .expect("run concurrent multi-peer grep");
    assert!(
        output.status.success(),
        "concurrent grep failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert_eq!(result["federation"]["coverage"], "complete");
    assert_eq!(result["sessions"].as_array().unwrap().len(), 3);
    assert!(alpha_started.exists(), "alpha peer was not launched");
    assert!(
        beta_started.exists(),
        "beta peer was not launched concurrently"
    );
}

#[test]
fn grep_keeps_successful_peer_results_when_another_selected_peer_is_unavailable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let available = write_grep_owner(
        temp.path(),
        "available",
        binary,
        &[(
            "available-tape",
            "{\"t\":\"2026-09-24T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-partial-multi\"}\n",
        )],
    );
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-partial-tape",
        "{\"t\":\"2026-09-24T11:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-partial-multi local\"}\n",
    );
    let caller_engram = caller_home.join(".engram");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "available": available,
                "offline": {
                    "command": ["/usr/bin/false"],
                    "engram": binary,
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize topology"),
    )
    .expect("caller topology");

    let partial = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-partial-multi",
            "--peers",
            "available,offline",
        ])
        .output()
        .expect("run partial multi-peer grep");
    assert!(
        partial.status.success(),
        "partial grep should keep successful results: {}",
        String::from_utf8_lossy(&partial.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&partial.stdout).expect("partial JSON");
    assert_eq!(result["federation"]["coverage"], "partial");
    assert_eq!(result["sessions"].as_array().unwrap().len(), 2);
    assert!(
        result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["tape_id"] == "caller-partial-tape")
    );
    assert_eq!(result["sessions"][0]["tape_id"], "available-tape");
    assert_eq!(result["total"], serde_json::Value::Null);
    assert_eq!(result["total_bounds"]["min"], 2);
    assert_eq!(result["total_bounds"]["max"], serde_json::Value::Null);
    assert_eq!(result["time_range"], serde_json::Value::Null);
    assert_eq!(result["truncated"], serde_json::Value::Null);
    let sources = result["federation"]["sources"].as_array().expect("sources");
    let available_source = sources
        .iter()
        .find(|source| source["store"] == "available/default")
        .expect("available source");
    assert_eq!(available_source["status"], "ok");
    assert_eq!(available_source["grep_scan"]["total"], 1);
    assert_eq!(
        available_source["grep_scan"]["time_range"]["start"],
        "2026-09-24T12:00:00Z"
    );
    let offline_source = sources
        .iter()
        .find(|source| source["store"] == "offline/default")
        .expect("offline source");
    assert_eq!(offline_source["status"], "unavailable");
    assert_eq!(offline_source["phase"], "open");

    let offset = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-partial-multi",
            "--peers",
            "available,offline",
            "--offset",
            "1",
            "--limit",
            "1",
        ])
        .output()
        .expect("run partial grep with nonzero offset");
    assert!(
        offset.status.success(),
        "offset grep should retain observed page: {}",
        String::from_utf8_lossy(&offset.stderr)
    );
    let offset_result: serde_json::Value =
        serde_json::from_slice(&offset.stdout).expect("offset grep JSON");
    assert_eq!(offset_result["returned"], 1);
    assert_eq!(offset_result["truncated"], serde_json::Value::Null);

    let counted = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-partial-multi",
            "--peers",
            "available,offline",
            "--count",
        ])
        .output()
        .expect("run partial count grep");
    assert!(
        counted.status.success(),
        "partial count should retain its aggregate payload: {}",
        String::from_utf8_lossy(&counted.stderr)
    );
    let count_result: serde_json::Value =
        serde_json::from_slice(&counted.stdout).expect("count grep JSON");
    assert!(count_result["sessions"].as_array().unwrap().is_empty());
    assert_eq!(count_result["returned"], 2);
    assert_eq!(count_result["total"], serde_json::Value::Null);
    assert_eq!(count_result["total_bounds"]["min"], 2);
    assert_eq!(count_result["total_bounds"]["max"], serde_json::Value::Null);
    assert_eq!(count_result["time_range"], serde_json::Value::Null);
    assert_eq!(count_result["truncated"], serde_json::Value::Null);

    let required = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-partial-multi",
            "--peers",
            "available,offline",
            "--require-complete",
        ])
        .output()
        .expect("run require-complete multi-peer grep");
    assert!(!required.status.success());
    assert!(
        String::from_utf8_lossy(&required.stderr).contains("incomplete_coverage"),
        "unexpected require-complete failure: {}",
        String::from_utf8_lossy(&required.stderr)
    );
}

#[test]
fn grep_preserves_known_truncation_when_another_selected_peer_is_unavailable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let available = write_grep_owner(
        temp.path(),
        "available",
        binary,
        &[
            (
                "available-first",
                "{\"t\":\"2026-09-24T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-partial-truncated first\"}\n",
            ),
            (
                "available-second",
                "{\"t\":\"2026-09-24T13:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-partial-truncated second\"}\n",
            ),
        ],
    );
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-partial-truncated",
        "{\"t\":\"2026-09-24T11:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-partial-truncated local\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "available": available,
                "offline": {
                    "command": ["/usr/bin/false"],
                    "engram": binary,
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize caller topology"),
    )
    .expect("caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-partial-truncated",
            "--peers",
            "available,offline",
            "--limit",
            "1",
        ])
        .output()
        .expect("run partially covered truncated grep");
    assert!(
        output.status.success(),
        "partial grep should keep known results: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert_eq!(result["federation"]["coverage"], "partial");
    assert_eq!(result["truncated"], true);
    assert_eq!(result["total"], serde_json::Value::Null);
    assert_eq!(result["total_bounds"]["max"], serde_json::Value::Null);
    let available_source = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "available/default")
        .expect("available source");
    assert_eq!(available_source["grep_scan"]["total"], 2);
    assert_eq!(available_source["grep_scan"]["truncated"], true);
}

#[test]
fn grep_without_peer_selection_does_not_start_configured_peers() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let caller_home = temp.path().join("caller-home");
    let caller_engram = caller_home.join(".engram");
    std::fs::create_dir_all(&caller_engram).expect("caller home");
    let marker = temp.path().join("peer-was-started");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "sentinel": {
                    "command": ["/usr/bin/touch", marker],
                    "engram": "/unused/engram",
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize topology"),
    )
    .expect("topology");
    let repo = temp.path().join("repo");
    let local_engram = repo.join(".engram");
    let tapes = local_engram.join("tapes");
    std::fs::create_dir_all(&tapes).expect("local tapes");
    let db = local_engram.join("index.sqlite");
    drop(SqliteIndex::open_writer(db.to_str().expect("DB path")).expect("local DB"));
    std::fs::write(
        caller_engram.join("config.yml"),
        format!("db: {}\ntapes_dir: {}\n", db.display(), tapes.display()),
    )
    .expect("local config");
    let content =
        "{\"t\":\"2026-09-24T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-local\"}\n";
    let compressed = zstd::stream::encode_all(content.as_bytes(), 0).expect("compress tape");
    std::fs::write(tapes.join("local-only.jsonl.zst"), compressed).expect("write local tape");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle-local"])
        .output()
        .expect("run local grep");
    assert!(
        output.status.success(),
        "local grep failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert!(result.get("federation").is_none());
    assert!(!marker.exists(), "local grep started a configured peer");
}

#[test]
fn grep_unavailable_peer_is_partial_and_require_complete_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let caller_home = temp.path().join("caller-home");
    let caller_engram = caller_home.join(".engram");
    std::fs::create_dir_all(&caller_engram).expect("caller home");
    let repo = temp.path().join("repo");
    let local_engram = repo.join(".engram");
    let tapes = local_engram.join("tapes");
    std::fs::create_dir_all(&tapes).expect("local tapes");
    let db = local_engram.join("index.sqlite");
    drop(SqliteIndex::open_writer(db.to_str().expect("DB path")).expect("local DB"));
    std::fs::write(
        caller_engram.join("config.yml"),
        format!("db: {}\ntapes_dir: {}\n", db.display(), tapes.display()),
    )
    .expect("local config");
    let content =
        "{\"t\":\"2026-09-24T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-partial\"}\n";
    let compressed = zstd::stream::encode_all(content.as_bytes(), 0).expect("compress tape");
    std::fs::write(tapes.join("local-partial.jsonl.zst"), compressed).expect("write local tape");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "offline": {
                    "command": ["/usr/bin/false"],
                    "engram": binary,
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize topology"),
    )
    .expect("topology");

    let partial = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle-partial", "--peers", "offline"])
        .output()
        .expect("run partial grep");
    assert!(
        partial.status.success(),
        "partial grep should succeed: {}",
        String::from_utf8_lossy(&partial.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&partial.stdout).expect("partial JSON");
    assert_eq!(result["federation"]["coverage"], "partial");
    assert!(result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["store"] == "offline/default" && source["status"] == "unavailable"));
    assert_eq!(result["total"], serde_json::Value::Null);
    assert_eq!(result["total_bounds"]["max"], serde_json::Value::Null);

    let required = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-partial",
            "--peers",
            "offline",
            "--require-complete",
        ])
        .output()
        .expect("run require-complete grep");
    assert!(!required.status.success());
    let stderr = String::from_utf8_lossy(&required.stderr);
    assert!(
        stderr.contains("incomplete_coverage"),
        "unexpected error: {stderr}"
    );
}

#[test]
fn local_show_without_peer_selection_does_not_spawn_configured_peers() {
    let temp = tempfile::tempdir().expect("tempdir");
    let caller_home = temp.path().join("caller-home");
    let caller_engram = caller_home.join(".engram");
    std::fs::create_dir_all(&caller_engram).expect("caller config directory");
    let marker = temp.path().join("peer-was-started");
    let binary = env!("CARGO_BIN_EXE_engram");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "sentinel": {
                    "command": ["/usr/bin/touch", marker],
                    "engram": "/unused/engram",
                    "exports": [],
                }
            }
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");
    let repo = temp.path().join("repo");
    let tapes = repo.join(".engram/tapes");
    std::fs::create_dir_all(&tapes).expect("local tape directory");
    let tape_id = "local-fixture";
    let content = "{\"t\":\"2026-09-23T12:34:56Z\",\"k\":\"msg.in\",\"content\":\"local\"}\n";
    let compressed = zstd::stream::encode_all(content.as_bytes(), 0).expect("compress tape");
    std::fs::write(tapes.join(format!("{tape_id}.jsonl.zst")), compressed)
        .expect("write local tape");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["show", tape_id])
        .output()
        .expect("run local show");
    assert!(
        output.status.success(),
        "local show failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists(), "local show spawned a configured peer");
}

#[test]
fn grep_keeps_completed_scan_aggregates_when_dispatch_metadata_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "local-nonmatch",
        "{\"t\":\"2026-09-24T10:00:00Z\",\"k\":\"msg.in\",\"content\":\"unrelated\"}\n",
    );
    let script_path = temp.path().join("metadata-failure-peer.sh");
    let script = [
        "#!/bin/sh",
        "while IFS= read -r request; do",
        r#"  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')"#,
        r#"  op=$(printf '%s\n' "$request" | sed -n 's/.*"op":"\([^"]*\)".*/\1/p')"#,
        r#"  case "$op" in"#,
        "    open)",
        r#"      printf '{"id":%s,"data":{"store":"alpha/default","status":"ok","db":"/fixture/alpha.sqlite","tape_dirs":[],"reader_mode":"live","snapshot_at":"2026-09-25T14:00:00Z"}}\n' "$id""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"self":"alpha","build":"@BUILD@","protocol":1,"schema":@SCHEMA@,"query_semantics":@SEMANTICS@,"limits":{"grep_k":10000}}}\n' "$id""#,
        "      ;;",
        "    grep_scan)",
        r#"      printf '{"id":%s,"data":{"type":"match","tape_id":"alpha-tape","timestamp":"2026-09-24T12:00:00Z","total_lines":1,"anchor_line":1,"match_count":1,"provenance_match_count":0,"provenance_event_count":1,"refs_up":0,"refs_down":0,"files_touched":[]}}\n' "$id""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"total":1,"returned":1,"time_range":{"start":"2026-09-24T12:00:00Z","end":"2026-09-24T12:00:00Z"},"truncated":false}}\n' "$id""#,
        "      ;;",
        "    dispatch_rows)",
        r#"      printf '{"id":%s,"end":true,"ok":false,"error":{"code":"injected_failure","message":"dispatch metadata unavailable"}}\n' "$id""#,
        "      ;;",
        "    *)",
        r#"      printf '{"id":%s,"end":true,"ok":false,"error":{"code":"unknown_operation","message":"unexpected operation"}}\n' "$id""#,
        "      ;;",
        "  esac",
        "done",
    ]
    .join("\n")
    .replace("@BUILD@", env!("CARGO_PKG_VERSION"))
    .replace("@SCHEMA@", &SCHEMA_VERSION.to_string())
    .replace("@SEMANTICS@", &QUERY_SEMANTICS_VERSION.to_string());
    std::fs::write(&script_path, script).expect("write injected peer");
    let topology = json!({
        "version": 1,
        "self": "caller",
        "peers": {
            "alpha": {
                "command": ["/bin/sh", script_path],
                "engram": binary,
                "exports": ["default"],
            }
        }
    });
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&topology).expect("serialize caller topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle-after-scan", "--peers", "alpha"])
        .output()
        .expect("run grep with failed metadata phase");
    assert!(
        output.status.success(),
        "partial metadata failure should retain matching results: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert_eq!(result["federation"]["coverage"], "partial");
    assert_eq!(result["total"], 1);
    assert_eq!(result["time_range"]["start"], "2026-09-24T12:00:00Z");
    assert_eq!(result["time_range"]["end"], "2026-09-24T12:00:00Z");
    assert_eq!(result["truncated"], false);
    assert_eq!(result["sessions"][0]["refs_up"], serde_json::Value::Null);
    let source = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "alpha/default")
        .expect("selected peer source");
    assert_eq!(source["status"], "failed");
    assert_eq!(source["phase"], "dispatch_rows");
    assert_eq!(source["grep_scan"]["total"], 1);
    assert_eq!(
        source["grep_scan"]["time_range"]["start"],
        "2026-09-24T12:00:00Z"
    );

    let required = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-after-scan",
            "--peers",
            "alpha",
            "--require-complete",
        ])
        .output()
        .expect("run require-complete grep after metadata failure");
    assert!(!required.status.success());
    assert!(String::from_utf8_lossy(&required.stderr).contains("incomplete_coverage"));
}
