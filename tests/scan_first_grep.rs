#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use engram::index::SqliteIndex;
use serde_json::{Value, json};

fn write_grep_owner(root: &Path, machine: &str, binary: &str, tapes: &[(&str, &str)]) -> Value {
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
        "exports": ["default"]
    })
}

fn write_local_source(root: &Path, tape_id: &str, content: &str) -> (PathBuf, PathBuf) {
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

fn set_peer_topology(caller_home: &Path, peers: Value) {
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({"version": 1, "self": "querier", "peers": peers}))
            .expect("serialize peer topology"),
    )
    .expect("write peer topology");
}

#[test]
fn decoded_local_and_peer_matches_keep_pages_and_per_tape_failure_coverage() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let remote = write_grep_owner(
        temp.path(),
        "remote",
        binary,
        &[
            (
                "remote-unicode",
                r#"{"t":"2026-09-25T00:00:00Z","k":"msg.in","content":"\u006eeedle remote"}
"#,
            ),
            (
                "remote-args",
                r#"{"t":"2026-09-24T00:00:00Z","k":"tool.call","tool":"exec_command","args":"{\"command\":\"echo needle argument\"}"}
"#,
            ),
            (
                "envelope-decoy",
                r#"{"t":"2026-09-23T00:00:00Z","k":"msg.in","needle":"metadata key only","content":"quiet"}
"#,
            ),
        ],
    );
    std::fs::write(
        temp.path()
            .join("remote-home/.engram/tapes/broken.jsonl.zst"),
        b"not a zstd tape",
    )
    .expect("write malformed peer tape");
    let (caller_home, repo) = write_local_source(
        temp.path(),
        "local-hit",
        r#"{"t":"2026-09-26T00:00:00Z","k":"msg.in","content":"needle local"}
"#,
    );
    set_peer_topology(&caller_home, json!({"remote": remote}));

    let mut seen = std::collections::HashSet::new();
    for (offset, expected_tape, expected_peer_returned, expected_peer_truncated) in [
        (0usize, "local-hit", 1u64, true),
        (1, "remote-unicode", 2, false),
        (2, "remote-args", 2, false),
    ] {
        let offset_arg = offset.to_string();
        let output = Command::new(binary)
            .current_dir(&repo)
            .env("HOME", &caller_home)
            .args([
                "grep",
                "needle",
                "--peers",
                "remote",
                "--limit",
                "1",
                "--offset",
                &offset_arg,
            ])
            .output()
            .expect("run decoded federated grep page");
        assert!(
            output.status.success(),
            "page at offset {offset} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).expect("grep page JSON");
        assert_eq!(result["federation"]["coverage"], "partial");
        let sessions = result["sessions"].as_array().expect("page sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["tape_id"], expected_tape);
        assert!(seen.insert(expected_tape.to_string()));

        let source = result["federation"]["sources"]
            .as_array()
            .expect("federation source rows")
            .iter()
            .find(|source| source["store"] == "remote/default")
            .expect("remote source row");
        assert_eq!(source["status"], "partial");
        assert_eq!(source["phase"], "grep_scan");
        assert_eq!(source["failures"][0]["tape_id"], "broken");
        assert_eq!(source["failures"][0]["error"]["code"], "invalid_tape");
        assert_eq!(source["grep_scan"]["total"], 2);
        assert_eq!(source["grep_scan"]["returned"], expected_peer_returned);
        assert_eq!(source["grep_scan"]["truncated"], expected_peer_truncated);
    }
    assert_eq!(
        seen.len(),
        3,
        "all decoded local and peer matches span pages"
    );
}

#[test]
fn decoded_marker_grep_finds_native_exec_argument_on_peer_without_joining_fields() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let uuid = "ccc7e8fa-6325-4110-98b5-95e5dffb6333";
    let marker = format!("<engram-src id=\"{uuid}\"/>");
    let escaped_marker = marker.replace('"', "\\\"");
    let native_call = format!(
        r#"const r = await tools.exec_command({{cmd:"tightbeam dispatch --to agent:example --subject 'Marker handoff' --brief 'Send {escaped_marker}' --work-item wi_example",yield_time_ms:10000,max_output_tokens:1500}}); text(r.output);"#
    );
    assert!(!native_call.contains(&marker));
    assert!(native_call.contains(&escaped_marker));
    let sender_event = format!(
        "{}\n",
        serde_json::to_string(&json!({
            "t": "2026-09-28T13:38:51.535Z",
            "k": "tool.call",
            "source": {"harness": "codex-cli", "session_id": "01a0e710-b907-7f82-a0ac-bc4647ff3317"},
            "tool": "exec",
            "call_id": "call_1c2DyppcQad9Cq1BX1XF6iF9",
            "args": native_call,
        }))
        .expect("serialize native Codex tool event")
    );
    let split_fields_event = format!(
        "{}\n",
        serde_json::to_string(&json!({
            "t": "2026-09-28T13:38:52Z",
            "k": "tool.call",
            "tool": "exec_command",
            "args": {
                "prefix": format!("<engram-src id=\"{uuid}"),
                "suffix": "\"/>",
            },
        }))
        .expect("serialize split-field decoy")
    );
    let nested_split_call = format!(
        r#"const r = await tools.exec_command({{cmd:"<engram-src id=\"{uuid}",workdir:"\"/>"}}); text(r.output);"#
    );
    let nested_split_event = format!(
        "{}\n",
        serde_json::to_string(&json!({
            "t": "2026-09-28T13:38:53Z",
            "k": "tool.call",
            "source": {"harness": "codex-cli", "session_id": "split-sender"},
            "tool": "exec",
            "args": nested_split_call,
        }))
        .expect("serialize nested split-field decoy")
    );
    let remote = write_grep_owner(
        temp.path(),
        "remote",
        binary,
        &[
            ("genuine-sender", sender_event.as_str()),
            ("split-fields", split_fields_event.as_str()),
            ("nested-split-fields", nested_split_event.as_str()),
        ],
    );
    let receiver_event = format!(
        "{}\n",
        serde_json::to_string(&json!({
            "t": "2026-09-28T13:41:14.986Z",
            "k": "msg.in",
            "content": format!("Received {marker}"),
        }))
        .expect("serialize receiver event")
    );
    let (caller_home, repo) = write_local_source(temp.path(), "receiver", receiver_event.as_str());
    set_peer_topology(&caller_home, json!({"remote": remote}));

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            marker.as_str(),
            "--peers",
            "remote",
            "--require-complete",
            "--limit",
            "10",
        ])
        .output()
        .expect("run decoded marker grep");
    assert!(
        output.status.success(),
        "grep failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert_eq!(
        result["federation"]["coverage"], "complete",
        "unexpected grep result: {result}"
    );
    assert_eq!(result["returned"], 2);
    let sessions = result["sessions"].as_array().expect("grep sessions");
    let mut tape_ids = sessions
        .iter()
        .map(|session| session["tape_id"].as_str().expect("tape ID"))
        .collect::<Vec<_>>();
    tape_ids.sort_unstable();
    assert_eq!(tape_ids, ["genuine-sender", "receiver"]);

    let peer = result["federation"]["sources"]
        .as_array()
        .expect("federation source rows")
        .iter()
        .find(|source| source["store"] == "remote/default")
        .expect("remote source row");
    assert_eq!(peer["status"], "ok");
    assert_eq!(peer["grep_scan"]["truncated"], false);
    assert_eq!(peer["grep_scan"]["total"], 1);
    assert_eq!(peer["grep_scan"]["returned"], 1);
}
