use std::time::Duration;

use engram::access::client::{PeerRequest, RemoteOwner};
use engram::config::TopologyPeer;
use engram::index::SqliteIndex;
use serde_json::json;

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
