use std::process::Command;
use std::time::Duration;

use engram::access::client::{PeerRequest, RemoteOwner};
use engram::config::TopologyPeer;
use engram::index::SqliteIndex;
use serde_json::json;
use sha2::Digest;

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

    let content = "{\"t\":\"2026-09-23T12:34:56Z\",\"k\":\"msg.in\",\"content\":\"from peer\"}\n";
    let tape_id = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
    let compressed = zstd::stream::encode_all(content.as_bytes(), 0).expect("compress tape");
    let tape = tapes.join(format!("{tape_id}.jsonl.zst"));
    std::fs::write(&tape, &compressed).expect("write owner tape");
    let opaque_id = "opaque-fixture";
    let opaque_tape = tapes.join(format!("{opaque_id}.jsonl.zst"));
    std::fs::write(&opaque_tape, compressed).expect("write opaque owner tape");
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
    assert_eq!(value["event_count"], 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("peers=emulated-owner"));
    assert!(
        !repo.join(".engram").exists(),
        "remote query created caller store files"
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
