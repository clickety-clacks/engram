use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

fn write_stalled_read_file_fixture(
    root: &std::path::Path,
    request_timeout_ms: u64,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    write_stalled_remote_operation_fixture(root, request_timeout_ms, "read_file")
}

fn write_stalled_remote_operation_fixture(
    root: &std::path::Path,
    request_timeout_ms: u64,
    blocked_operation: &str,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let caller_home = root.join("caller-home");
    let caller_engram = caller_home.join(".engram");
    std::fs::create_dir_all(&caller_engram).expect("caller config directory");
    std::fs::write(
        caller_engram.join("config.yml"),
        "db: ~/.engram/index.sqlite\ntapes_dir: ~/.engram/tapes\n",
    )
    .expect("write caller config");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("caller repo");

    let marker = root.join("read-file-started");
    let script_path = root.join("stalled-read-file-peer.sh");
    let script = [
        "#!/bin/sh",
        "set -eu",
        "marker=\"$1\"",
        "request_timeout_ms=\"$2\"",
        "blocked_operation=\"$3\"",
        "while IFS= read -r request; do",
        r#"  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')"#,
        r#"  op=$(printf '%s\n' "$request" | sed -n 's/.*"op":"\([^"]*\)".*/\1/p')"#,
        "  case \"$op\" in",
        "    open)",
        "      if [ \"$blocked_operation\" = open ]; then touch \"$marker\"; exec /usr/bin/sleep 60; fi",
        r#"      printf '{"id":%s,"data":{"store":"silent/default","status":"ok","db":"/fixture/owner.sqlite","tape_dirs":[],"reader_mode":"live","snapshot_at":"2026-09-25T00:00:00Z"}}\n' "$id""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"self":"silent","build":"@BUILD@","protocol":1,"schema":@SCHEMA@,"query_semantics":@SEMANTICS@,"limits":{"read_file_compressed_bytes":268435456,"decompressed_bytes_per_tape":536870912,"request_timeout_ms":%s}}}\n' "$id" "$request_timeout_ms""#,
        "      ;;",
        "    locate_tapes)",
        "      if [ \"$blocked_operation\" = locate_tapes ]; then",
        r#"        printf '{"id":%s,"data":{"tape_id":"fixture-tape","file":{"machine":"silent","path":"/owner/tapes/fixture-tape.jsonl.zst","kind":"tape"},"size_bytes":1}}\n' "$id""#,
        "        touch \"$marker\"",
        "        exec /usr/bin/sleep 60",
        "      fi",
        r#"      printf '{"id":%s,"data":{"tape_id":"fixture-tape","file":{"machine":"silent","path":"/owner/tapes/fixture-tape.jsonl.zst","kind":"tape"},"size_bytes":1}}\n' "$id""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"located":1}}\n' "$id""#,
        "      ;;",
        "    read_file)",
        "      if [ \"$blocked_operation\" != read_file ]; then exit 78; fi",
        r#"      printf '{"id":%s,"data":{"tape_id":"fixture-tape","offset":0,"bytes_b64":"eA=="}}\n' "$id""#,
        "      touch \"$marker\"",
        "      exec /usr/bin/sleep 60",
        "      ;;",
        "    dispatch_rows)",
        "      if [ \"$blocked_operation\" != dispatch_rows ]; then exit 78; fi",
        r#"      printf '{"id":%s,"data":{"store":"silent/default","tape_id":"fixture-tape","direction":"received","first_turn_index":1,"uuid":"fixture-dispatch"}}\n' "$id""#,
        "      touch \"$marker\"",
        "      exec /usr/bin/sleep 60",
        "      ;;",
        "    peek_lines)",
        "      if [ \"$blocked_operation\" != peek_lines ]; then exit 78; fi",
        r#"      printf '{"id":%s,"data":{"tape_id":"fixture-tape","line":1,"text":"unverified partial window"}}\n' "$id""#,
        "      touch \"$marker\"",
        "      exec /usr/bin/sleep 60",
        "      ;;",
        "    *) exit 78 ;;",
        "  esac",
        "done",
    ]
    .join("\n")
    .replace("@BUILD@", env!("CARGO_PKG_VERSION"))
    .replace("@SCHEMA@", &SCHEMA_VERSION.to_string())
    .replace("@SEMANTICS@", &QUERY_SEMANTICS_VERSION.to_string());
    std::fs::write(&script_path, script).expect("write stalled peer script");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "silent": {
                    "command": ["/bin/sh", script_path, marker, request_timeout_ms.to_string(), blocked_operation],
                    "engram": env!("CARGO_BIN_EXE_engram"),
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");
    (caller_home, repo, marker)
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
            "version: 1\nself: emulated-owner\nexports:\n  default:\n    db: {}\n    tape_dirs:\n      - {}\nlimits:\n  request_timeout_ms: 1234\n",
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
    assert_eq!(owner.limits.get("request_timeout_ms"), Some(&1_234));

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

#[cfg(unix)]
#[test]
fn remote_show_obeys_advertised_request_timeout_for_read_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo, read_started) = write_stalled_read_file_fixture(temp.path(), 100);

    let started = Instant::now();
    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["show", "fixture-tape", "--store", "silent/default"])
        .output()
        .expect("run remote show with a silent read_file peer");
    assert!(!output.status.success());
    assert!(read_started.exists(), "peer did not receive read_file");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "owner request timeout was not applied: {:?}",
        started.elapsed()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let error: serde_json::Value =
        serde_json::from_str(stderr.lines().last().expect("timeout error line"))
            .expect("timeout error JSON");
    assert_eq!(error["error"]["code"], "timeout");
}

#[cfg(unix)]
fn interrupt_remote_command(
    repo: &std::path::Path,
    caller_home: &std::path::Path,
    args: &[&str],
    operation_started: &std::path::Path,
) -> (std::process::Output, Duration) {
    let binary = env!("CARGO_BIN_EXE_engram");
    let mut child = Command::new(binary)
        .current_dir(repo)
        .env("HOME", caller_home)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn remote peer query");

    let start_deadline = Instant::now() + Duration::from_secs(5);
    while !operation_started.exists() && Instant::now() < start_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !operation_started.exists() {
        let _ = child.kill();
        let _ = child.wait_with_output();
        panic!("peer did not receive the selected operation");
    }

    let signal_at = Instant::now();
    let signal_result = unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    assert_eq!(signal_result, 0, "send SIGINT to remote peer query");
    let exit_deadline = Instant::now() + Duration::from_secs(3);
    let mut exited = false;
    while Instant::now() < exit_deadline {
        if child
            .try_wait()
            .expect("check remote show status")
            .is_some()
        {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !exited {
        let _ = child.kill();
        let _ = child.wait_with_output();
        panic!("Ctrl-C did not cancel the remote operation within three seconds");
    }

    let output = child
        .wait_with_output()
        .expect("collect cancelled remote query output");
    (output, signal_at.elapsed())
}

#[cfg(unix)]
fn assert_remote_sigint_error(output: &std::process::Output) {
    assert_eq!(output.status.code(), Some(130), "SIGINT must exit 130");
    assert!(
        output.stdout.is_empty(),
        "cancelled content must not be emitted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let error: serde_json::Value =
        serde_json::from_str(stderr.lines().last().expect("cancellation error line"))
            .expect("cancellation error JSON");
    assert_eq!(error["error"]["code"], "cancelled");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("caller SIGINT")
    );
}

#[cfg(unix)]
#[test]
fn remote_show_ctrl_c_cancels_and_aborts_read_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (caller_home, repo, read_started) = write_stalled_read_file_fixture(temp.path(), 10_000);
    let (output, elapsed) = interrupt_remote_command(
        &repo,
        &caller_home,
        &["show", "fixture-tape", "--store", "silent/default"],
        &read_started,
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "remote read_file cancellation exceeded its deadline"
    );
    assert_remote_sigint_error(&output);
}

#[cfg(unix)]
#[test]
fn remote_show_ctrl_c_during_locate_returns_130_without_content() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (caller_home, repo, locate_started) =
        write_stalled_remote_operation_fixture(temp.path(), 10_000, "locate_tapes");
    let (output, elapsed) = interrupt_remote_command(
        &repo,
        &caller_home,
        &["show", "fixture-tape", "--store", "silent/default"],
        &locate_started,
    );
    assert!(elapsed < Duration::from_secs(3));
    assert_remote_sigint_error(&output);
}

#[cfg(unix)]
#[test]
fn remote_peek_ctrl_c_during_lines_returns_130_without_partial_window() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (caller_home, repo, peek_started) =
        write_stalled_remote_operation_fixture(temp.path(), 10_000, "peek_lines");
    let (output, elapsed) = interrupt_remote_command(
        &repo,
        &caller_home,
        &[
            "peek",
            "fixture-tape",
            "--store",
            "silent/default",
            "--start",
            "1",
            "--lines",
            "1",
        ],
        &peek_started,
    );
    assert!(elapsed < Duration::from_secs(3));
    assert_remote_sigint_error(&output);
}

#[cfg(unix)]
#[test]
fn remote_peek_ctrl_c_during_dispatch_metadata_returns_130() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (caller_home, repo, dispatch_started) =
        write_stalled_remote_operation_fixture(temp.path(), 10_000, "dispatch_rows");
    let (output, elapsed) = interrupt_remote_command(
        &repo,
        &caller_home,
        &["peek", "fixture-tape", "--store", "silent/default"],
        &dispatch_started,
    );
    assert!(elapsed < Duration::from_secs(3));
    assert_remote_sigint_error(&output);
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

#[cfg(unix)]
#[test]
fn grep_runs_selected_peer_scan_rounds_concurrently() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let script_path = temp.path().join("concurrent-scan-peer.sh");
    let script = [
        "#!/bin/sh",
        "set -eu",
        "machine=\"$1\"",
        "mine=\"$2\"",
        "other=\"$3\"",
        "while IFS= read -r request; do",
        r#"  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')"#,
        r#"  op=$(printf '%s\n' "$request" | sed -n 's/.*"op":"\([^"]*\)".*/\1/p')"#,
        "  case \"$op\" in",
        "    open)",
        r#"      printf '{"id":%s,"data":{"store":"%s/default","status":"ok","db":"/fixture/%s.sqlite","tape_dirs":[],"reader_mode":"live","snapshot_at":"2026-09-25T00:00:00Z"}}\n' "$id" "$machine" "$machine""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"self":"%s","build":"0.2.1","protocol":1,"schema":4,"query_semantics":1,"limits":{"grep_k":10000}}}\n' "$id" "$machine""#,
        "      ;;",
        "    grep_scan)",
        "      touch \"$mine\"",
        "      i=0",
        "      while [ ! -e \"$other\" ] && [ \"$i\" -lt 200 ]; do",
        "        sleep 0.01",
        "        i=$((i + 1))",
        "      done",
        "      if [ ! -e \"$other\" ]; then",
        r#"        printf '{"id":%s,"end":true,"ok":false,"error":{"code":"scan_was_serial","message":"other selected peer did not start its scan"}}\n' "$id""#,
        "        continue",
        "      fi",
        r#"      printf '{"id":%s,"data":{"type":"match","tape_id":"%s-tape","timestamp":"2026-09-25T00:00:00Z","total_lines":1,"anchor_line":1,"match_count":1,"provenance_match_count":0,"provenance_event_count":1,"refs_up":0,"refs_down":0,"files_touched":[]}}\n' "$id" "$machine""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"total":1,"returned":1,"time_range":{"start":"2026-09-25T00:00:00Z","end":"2026-09-25T00:00:00Z"},"truncated":false}}\n' "$id""#,
        "      ;;",
        "    dispatch_rows)",
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{}}\n' "$id""#,
        "      ;;",
        "    *) exit 78 ;;",
        "  esac",
        "done",
    ]
    .join("\n");
    std::fs::write(&script_path, script).expect("write protocol peer script");
    let alpha_started = temp.path().join("alpha-scan-started");
    let beta_started = temp.path().join("beta-scan-started");
    let peer = |machine: &str, mine: &std::path::Path, other: &std::path::Path| {
        json!({
            "command": ["/bin/sh", script_path, machine, mine, other],
            "engram": binary,
            "exports": ["default"],
        })
    };
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-nonmatch",
        "{\"t\":\"2026-09-25T00:00:00Z\",\"k\":\"msg.in\",\"content\":\"unrelated\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "alpha": peer("alpha", &alpha_started, &beta_started),
                "beta": peer("beta", &beta_started, &alpha_started),
            }
        }))
        .expect("serialize topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle-parallel-scan", "--peers", "alpha,beta"])
        .output()
        .expect("run concurrent scan grep");
    assert!(
        output.status.success(),
        "concurrent scan grep failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert_eq!(result["federation"]["coverage"], "complete");
    assert_eq!(result["sessions"].as_array().unwrap().len(), 2);
    assert!(alpha_started.exists(), "alpha scan did not start");
    assert!(
        beta_started.exists(),
        "beta scan did not start while alpha was scanning"
    );
}

#[cfg(unix)]
fn spawn_grep_waiting_for_peer(
    temp: &tempfile::TempDir,
    pattern: &str,
    local_content: &str,
    blocked_operation: &str,
    require_complete: bool,
) -> (std::process::Child, std::path::PathBuf) {
    let binary = env!("CARGO_BIN_EXE_engram");
    let script_path = temp.path().join("blocking-peer.sh");
    let operation_started = temp.path().join("peer-operation-started");
    let script = [
        "#!/bin/sh",
        "set -eu",
        "blocked_operation=\"$1\"",
        "started=\"$2\"",
        "while IFS= read -r request; do",
        r#"  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')"#,
        r#"  op=$(printf '%s\n' "$request" | sed -n 's/.*"op":"\([^"]*\)".*/\1/p')"#,
        "  case \"$op\" in",
        "    open)",
        r#"      printf '{"id":%s,"data":{"store":"alpha/default","status":"ok","db":"/fixture/alpha.sqlite","tape_dirs":[],"reader_mode":"live","snapshot_at":"2026-09-25T00:00:00Z"}}\n' "$id""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"self":"alpha","build":"0.2.1","protocol":1,"schema":4,"query_semantics":1,"limits":{"grep_k":10000}}}\n' "$id""#,
        "      ;;",
        "    grep_scan)",
        "      if [ \"$blocked_operation\" = grep_scan ]; then",
        "        touch \"$started\"",
        "        while IFS= read -r ignored; do :; done",
        "        exit 0",
        "      fi",
        r#"      printf '{"id":%s,"data":{"type":"match","tape_id":"alpha-tape","timestamp":"2026-09-25T00:00:00Z","total_lines":1,"anchor_line":1,"match_count":1,"provenance_match_count":0,"provenance_event_count":1,"refs_up":0,"refs_down":0,"files_touched":[]}}\n' "$id""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"total":1,"returned":1,"time_range":{"start":"2026-09-25T00:00:00Z","end":"2026-09-25T00:00:00Z"},"truncated":false}}\n' "$id""#,
        "      ;;",
        "    dispatch_rows)",
        "      if [ \"$blocked_operation\" = dispatch_rows ]; then",
        "        touch \"$started\"",
        "        while IFS= read -r ignored; do :; done",
        "        exit 0",
        "      fi",
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{}}\n' "$id""#,
        "      ;;",
        "    *) exit 78 ;;",
        "  esac",
        "done",
    ]
    .join("\n");
    std::fs::write(&script_path, script).expect("write blocking peer script");
    let (caller_home, repo) =
        write_local_grep_source(temp.path(), "caller-before-cancel", local_content);
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "alpha": {
                    "command": ["/bin/sh", script_path, blocked_operation, operation_started],
                    "engram": binary,
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize topology"),
    )
    .expect("write caller topology");

    let mut command = Command::new(binary);
    command
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", pattern, "--peers", "alpha"]);
    if require_complete {
        command.arg("--require-complete");
    }
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn peer grep");
    (child, operation_started)
}

#[cfg(unix)]
fn interrupt_waiting_grep(
    mut child: std::process::Child,
    operation_started: &std::path::Path,
) -> (std::process::Output, Duration) {
    let start_deadline = Instant::now() + Duration::from_secs(5);
    while !operation_started.exists() && Instant::now() < start_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(operation_started.exists(), "peer operation did not start");
    let signal_at = Instant::now();
    let signal_result = unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    assert_eq!(signal_result, 0, "send SIGINT to grep caller");

    let exit_deadline = Instant::now() + Duration::from_secs(3);
    let mut exited = false;
    while Instant::now() < exit_deadline {
        if child.try_wait().expect("check caller status").is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !exited {
        let _ = child.kill();
        let _ = child.wait_with_output();
        panic!("Ctrl-C did not cancel the peer operation within three seconds");
    }
    let output = child
        .wait_with_output()
        .expect("collect cancelled grep output");
    (output, signal_at.elapsed())
}

#[cfg(unix)]
fn assert_caller_sigint_result(output: &std::process::Output) -> serde_json::Value {
    assert_eq!(
        output.status.code(),
        Some(130),
        "SIGINT must exit 130: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).lines().count(),
        1,
        "emit one structured result"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("\"error\""));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert_eq!(result["cancellation"]["status"], "cancelled");
    assert_eq!(result["cancellation"]["source"], "caller_sigint");
    result
}

#[cfg(unix)]
#[test]
fn grep_ctrl_c_cancels_and_aborts_a_selected_peer_scan() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (child, operation_started) = spawn_grep_waiting_for_peer(
        &temp,
        "needle-cancel",
        "{\"t\":\"2026-09-25T00:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-cancel local\"}\n",
        "grep_scan",
        false,
    );
    let (output, elapsed) = interrupt_waiting_grep(child, &operation_started);
    assert!(
        elapsed < Duration::from_secs(3),
        "peer cancellation exceeded its deadline"
    );
    let result = assert_caller_sigint_result(&output);
    assert!(
        result["federation"]["coverage"] == "partial",
        "the interrupted scan should make coverage partial"
    );
    assert_eq!(result["sessions"][0]["tape_id"], "caller-before-cancel");
    let peer_source = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "alpha/default")
        .expect("cancelled peer source");
    assert_eq!(peer_source["error"]["code"], "cancelled",);
    assert_eq!(peer_source["phase"], "grep_scan");
    assert_eq!(
        result["cancellation"]["incomplete_sources"][0]["store"],
        "alpha/default"
    );
    assert_eq!(
        result["cancellation"]["incomplete_sources"][0]["phase"],
        "grep_scan"
    );
}

#[cfg(unix)]
#[test]
fn grep_ctrl_c_overrides_require_complete_with_exit_130() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (child, operation_started) = spawn_grep_waiting_for_peer(
        &temp,
        "needle-cancel-required",
        "{\"t\":\"2026-09-25T00:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-cancel-required local\"}\n",
        "grep_scan",
        true,
    );
    let (output, elapsed) = interrupt_waiting_grep(child, &operation_started);
    assert!(elapsed < Duration::from_secs(3));
    let result = assert_caller_sigint_result(&output);
    assert_eq!(result["federation"]["coverage"], "partial");
    assert_eq!(result["sessions"][0]["tape_id"], "caller-before-cancel");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("incomplete_coverage"));
}

#[cfg(unix)]
#[test]
fn grep_ctrl_c_without_matches_is_cancelled_not_no_results() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (child, operation_started) = spawn_grep_waiting_for_peer(
        &temp,
        "needle-cancel-empty",
        "{\"t\":\"2026-09-25T00:00:00Z\",\"k\":\"msg.in\",\"content\":\"unrelated local\"}\n",
        "grep_scan",
        false,
    );
    let (output, elapsed) = interrupt_waiting_grep(child, &operation_started);
    assert!(elapsed < Duration::from_secs(3));
    let result = assert_caller_sigint_result(&output);
    assert_eq!(result["federation"]["coverage"], "partial");
    assert_eq!(result["sessions"], json!([]));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("no_results"));
}

#[cfg(unix)]
#[test]
fn grep_ctrl_c_during_dispatch_keeps_completed_scan_aggregates() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (child, operation_started) = spawn_grep_waiting_for_peer(
        &temp,
        "needle-cancel-metadata",
        "{\"t\":\"2026-09-24T10:00:00Z\",\"k\":\"msg.in\",\"content\":\"unrelated local\"}\n",
        "dispatch_rows",
        false,
    );
    let (output, elapsed) = interrupt_waiting_grep(child, &operation_started);
    assert!(elapsed < Duration::from_secs(3));
    let result = assert_caller_sigint_result(&output);
    assert_eq!(result["federation"]["coverage"], "partial");
    assert_eq!(result["total"], 1);
    assert_eq!(result["time_range"]["start"], "2026-09-25T00:00:00Z");
    assert_eq!(result["time_range"]["end"], "2026-09-25T00:00:00Z");
    assert_eq!(result["truncated"], false);
    assert_eq!(result["sessions"][0]["tape_id"], "alpha-tape");
    assert_eq!(result["sessions"][0]["refs_up"], serde_json::Value::Null);
    let source = result["cancellation"]["incomplete_sources"][0].clone();
    assert_eq!(source["store"], "alpha/default");
    assert_eq!(source["phase"], "dispatch_rows");
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
fn topology_status_reports_handshake_and_unselected_peer_without_starting_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let remote = write_grep_owner(temp.path(), "remote", binary, &[]);
    let caller_home = temp.path().join("caller-home");
    let caller_engram = caller_home.join(".engram");
    std::fs::create_dir_all(&caller_engram).expect("caller home");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("caller repo");
    let idle_marker = temp.path().join("unselected-peer-started");
    let topology = json!({
        "version": 1,
        "self": "caller",
        "peers": {
            "remote": remote,
            "idle": {
                "command": ["/bin/sh", "-c", format!("touch {}", idle_marker.display())],
                "engram": binary,
                "exports": ["default"],
            },
        }
    });
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&topology).expect("serialize topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["topology", "status", "--peers", "remote"])
        .output()
        .expect("run topology status");
    assert!(
        output.status.success(),
        "topology status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(result["self"], "caller");
    assert_eq!(result["status"], "ok");
    assert_eq!(result["watcher_caught_up"], "unknown");
    let peers = result["peers"].as_array().expect("peer rows");
    let remote = peers
        .iter()
        .find(|peer| peer["machine"] == "remote")
        .expect("selected remote row");
    assert_eq!(remote["status"], "ok");
    assert_eq!(remote["handshake"]["self"], "remote");
    assert_eq!(remote["handshake"]["protocol"], 1);
    assert_eq!(remote["handshake"]["schema"], SCHEMA_VERSION);
    assert_eq!(
        remote["handshake"]["query_semantics"],
        QUERY_SEMANTICS_VERSION
    );
    assert_eq!(remote["exports"][0]["store"], "remote/default");
    assert_eq!(remote["exports"][0]["status"], "ok");
    let idle = peers
        .iter()
        .find(|peer| peer["machine"] == "idle")
        .expect("unselected peer row");
    assert_eq!(idle["status"], "not_selected");
    assert!(!idle_marker.exists(), "unselected peer was launched");
}

#[test]
fn topology_status_check_exports_counts_indexed_tapes_without_regular_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let caller_home = temp.path().join("caller-home");
    let caller_engram = caller_home.join(".engram");
    let tape_dir = caller_engram.join("tapes");
    std::fs::create_dir_all(&tape_dir).expect("caller tape directory");
    let db = caller_engram.join("index.sqlite");
    let writer = SqliteIndex::open_writer(db.to_str().expect("db path")).expect("index");
    writer
        .ingest_tape_events("present", &[], 0.5)
        .expect("index present tape");
    writer
        .ingest_tape_events("missing", &[], 0.5)
        .expect("index missing tape");
    drop(writer);
    let tape = zstd::stream::encode_all(&b""[..], 0).expect("compress empty tape");
    std::fs::write(tape_dir.join("present.jsonl.zst"), tape).expect("write present tape");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "exports": {
                "default": {"db": db, "tape_dirs": [tape_dir]},
            }
        }))
        .expect("serialize topology"),
    )
    .expect("write topology");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("caller repo");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["topology", "status", "--check-exports"])
        .output()
        .expect("run export status check");
    assert!(
        output.status.success(),
        "export status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(result["status"], "ok");
    assert_eq!(result["check_exports"], true);
    assert_eq!(result["watcher_caught_up"], "unknown");
    assert_eq!(result["local_exports"][0]["store"], "caller/default");
    assert_eq!(result["local_exports"][0]["indexed_tape_count"], 2);
    assert_eq!(result["local_exports"][0]["indexed_tapes_without_file"], 1);
}

#[test]
fn peer_anchor_and_edge_lookups_preserve_membership_held_and_forensics_data() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let before = (1..=24)
        .map(|line| format!("fn before_{line}() {{ value_{line}(); }}\n"))
        .collect::<String>();
    let after = (1..=24)
        .map(|line| format!("fn after_{line}() {{ value_{line}(); }}\n"))
        .collect::<String>();
    let events = [
        json!({
            "t": "2026-09-25T12:00:00Z",
            "k": "code.edit",
            "file": "src/lib.rs",
            "before_range": [1, 24],
            "after_range": [1, 24],
            "before_text": before,
            "after_text": after,
        })
        .to_string(),
        json!({
            "t": "2026-09-25T12:01:00Z",
            "k": "code.edit",
            "file": "src/lib.rs",
            "before_range": [1, 24],
            "before_text": before,
            "after_text": null,
        })
        .to_string(),
    ]
    .join("\n")
        + "\n";

    let remote = write_grep_owner(
        temp.path(),
        "anchor-owner",
        binary,
        &[("held-tape", &events)],
    );
    let owner_home = temp.path().join("anchor-owner-home");
    let owner_db = owner_home.join(".engram/index.sqlite");
    let writer =
        SqliteIndex::open_writer(owner_db.to_str().expect("owner DB path")).expect("owner index");
    let parsed = engram::tape::event::parse_jsonl_events(&events).expect("parse events");
    writer
        .ingest_tape_events(
            "held-tape",
            &parsed,
            engram::index::lineage::LINK_THRESHOLD_DEFAULT,
        )
        .expect("index held tape");
    writer
        .ingest_tape_events(
            "missing-file-tape",
            &parsed,
            engram::index::lineage::LINK_THRESHOLD_DEFAULT,
        )
        .expect("index missing-file tape");
    drop(writer);

    let peer = TopologyPeer {
        ssh: None,
        command: Some(vec![
            "/usr/bin/env".into(),
            format!("HOME={}", owner_home.display()),
            binary.into(),
            "peer-serve".into(),
            "--stdio".into(),
        ]),
        engram: binary.to_string(),
        exports: remote["exports"]
            .as_array()
            .expect("configured exports")
            .iter()
            .map(|export| export.as_str().expect("export name").to_string())
            .collect(),
    };
    let mut owner = RemoteOwner::connect("anchor-owner", "caller", &peer, Duration::from_secs(5))
        .expect("connect owner");
    let after_window = engram::anchor::fingerprint_windows(&after)
        .into_iter()
        .next()
        .expect("after fingerprint window");
    let before_window = engram::anchor::fingerprint_windows(&before)
        .into_iter()
        .next()
        .expect("before fingerprint window");

    let anchor_response = owner.round(
        &[PeerRequest::new(
            "lookup_anchors",
            vec!["default".into()],
            json!({"anchors": [after_window.features[0]], "include_deleted": false}),
        )],
        Duration::from_secs(5),
    );
    let anchor_response = anchor_response
        .into_iter()
        .next()
        .expect("anchor response")
        .expect("anchor lookup");
    assert!(anchor_response.data.iter().any(|row| {
        row["type"] == "anchor_result"
            && row["matching_window_anchors"]
                .as_array()
                .is_some_and(|anchors| anchors.iter().any(|anchor| anchor == &after_window.anchor))
    }));
    let fragments = anchor_response
        .data
        .iter()
        .filter(|row| row["type"] == "fragment")
        .collect::<Vec<_>>();
    assert!(fragments.iter().any(|row| {
        row["tape_id"] == "held-tape" && row["held"] == true && row["kind"] == "edit"
    }));
    assert!(
        fragments
            .iter()
            .any(|row| { row["tape_id"] == "missing-file-tape" && row["held"] == false })
    );

    let edge_response = owner.round(
        &[PeerRequest::new(
            "lookup_edges",
            vec!["default".into()],
            json!({
                "nodes": [after_window.anchor],
                "min_confidence": 0.0,
                "include_forensics": true,
            }),
        )],
        Duration::from_secs(5),
    );
    let edge_response = edge_response
        .into_iter()
        .next()
        .expect("edge response")
        .expect("edge lookup");
    assert!(
        edge_response.data.iter().any(|row| {
            row["type"] == "edge"
                && row["node"] == after_window.anchor
                && (row["from_anchor"] == after_window.anchor
                    || row["to_anchor"] == after_window.anchor)
                && row["stored_class"] == "location_only"
        }),
        "forensics lookup omitted the location-only edge for the queried node"
    );

    let filtered_edge_response = owner.round(
        &[PeerRequest::new(
            "lookup_edges",
            vec!["default".into()],
            json!({
                "nodes": [after_window.anchor],
                "min_confidence": 0.0,
                "include_forensics": false,
            }),
        )],
        Duration::from_secs(5),
    );
    let filtered_edge_response = filtered_edge_response
        .into_iter()
        .next()
        .expect("filtered edge response")
        .expect("filtered edge lookup");
    assert!(
        filtered_edge_response
            .data
            .iter()
            .all(|row| row["type"] != "edge"),
        "default traversal returned a location-only edge"
    );

    let tombstone_response = owner.round(
        &[PeerRequest::new(
            "lookup_anchors",
            vec!["default".into()],
            json!({"anchors": [before_window.features[0]], "include_deleted": true}),
        )],
        Duration::from_secs(5),
    );
    let tombstone_response = tombstone_response
        .into_iter()
        .next()
        .expect("tombstone response")
        .expect("tombstone lookup");
    assert!(tombstone_response.data.iter().any(|row| {
        row["type"] == "tombstone"
            && row["tape_id"] == "held-tape"
            && row["range"]["start"] == 1
            && row["range"]["end"] == 24
    }));
}

#[test]
fn peer_tape_facts_returns_segment_history_turn_maps_and_bounded_summaries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let current = concat!(
        "{\"k\":\"meta\",\"ingest_continuation\":{\"previous_tape_id\":\"previous-segment\",\"message_turn_start\":2}}\n",
        "{\"t\":\"2026-09-24T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"question\"}\n",
        "{\"t\":\"2026-09-24T12:01:00Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",\"content\":\"needle edit\"}\n",
        "{\"t\":\"2026-09-24T12:02:00Z\",\"k\":\"msg.out\",\"content\":\"answer\"}\n",
    );
    let previous = concat!(
        "{\"k\":\"meta\"}\n",
        "{\"t\":\"2026-09-24T11:00:00Z\",\"k\":\"msg.in\",\"content\":\"earlier\"}\n",
    );
    let remote = write_grep_owner(
        temp.path(),
        "facts-owner",
        binary,
        &[
            ("current-segment", current),
            ("previous-segment", previous),
        ],
    );
    let peer = TopologyPeer {
        ssh: None,
        command: Some(
            remote["command"]
                .as_array()
                .expect("command array")
                .iter()
                .map(|arg| arg.as_str().expect("command string").to_string())
                .collect(),
        ),
        engram: binary.to_string(),
        exports: vec!["default".into()],
    };
    let mut owner = RemoteOwner::connect("facts-owner", "caller", &peer, Duration::from_secs(5))
        .expect("connect owner");
    let response = owner
        .round(
            &[PeerRequest::new(
                "tape_facts",
                vec!["default".into()],
                json!({
                    "items": [
                        {
                            "tape_id": "current-segment",
                            "edit_offsets": [2],
                            "turns": [0, 1, 2],
                            "anchor_offsets": [2],
                            "grep_filter": "code.edit",
                            "window_lines": 4,
                        },
                        {"tape_id": "missing-segment"},
                    ]
                }),
            )],
            Duration::from_secs(5),
        )
        .into_iter()
        .next()
        .expect("tape_facts response")
        .expect("tape_facts succeeds with a per-tape unavailable outcome");

    let facts = response
        .data
        .iter()
        .find(|row| row["tape_id"] == "current-segment")
        .expect("current tape facts");
    assert_eq!(facts["status"], "ok");
    assert_eq!(facts["indexed"], false);
    assert_eq!(facts["segment"]["previous_tape_id"], "previous-segment");
    assert_eq!(facts["segment"]["message_turn_start"], 2);
    assert_eq!(facts["predecessor_chain"].as_array().unwrap().len(), 2);
    assert_eq!(facts["chain_status"], "complete");
    assert_eq!(facts["edit_offset_to_turn"][0]["segment_turn"], 1);
    assert_eq!(facts["edit_offset_to_turn"][0]["turn"], 3);
    assert_eq!(facts["turn_to_offset"][0]["offset"], 1);
    assert_eq!(facts["turn_to_offset"][1]["offset"], 3);
    assert!(facts["turn_to_offset"][2]["offset"].is_null());
    assert!(facts["recovery_binding"].is_null());
    assert_eq!(facts["summary"]["total_lines"], 4);
    assert_eq!(facts["summary"]["anchor_line"], 3);
    assert_eq!(facts["summary"]["window_start"], 1);
    assert_eq!(facts["summary"]["window_end"], 4);
    assert_eq!(facts["summary"]["grep_filter_hits_window"], true);
    assert_eq!(facts["summary"]["latest_timestamp"], "2026-09-24T12:02:00Z");
    assert_eq!(facts["summary"]["files_touched"], json!(["src/lib.rs"]));

    let missing = response
        .data
        .iter()
        .find(|row| row["tape_id"] == "missing-segment")
        .expect("missing tape outcome");
    assert_eq!(missing["status"], "unavailable");
    assert_eq!(missing["error"]["code"], "tape_unavailable");
    assert_eq!(missing["summary"]["total_lines"], 0);
    assert_eq!(missing["summary"]["files_touched"], json!([]));
}

#[test]
fn peer_tape_facts_verifies_and_returns_native_recovery_binding() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let points = json!({
        "tape_id": "legacy-segment",
        "points": [{"old_offset": 2, "source_offset": 3, "turn": 2}],
    });
    let context = format!(
        "{{\"k\":\"meta\",\"native_recovery_v1\":[{}]}}\n{{\"t\":\"2026-09-24T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"context\"}}\n",
        points
    );
    let context_id = format!("{:x}", sha2::Sha256::digest(context.as_bytes()));
    let legacy = concat!(
        "{\"k\":\"meta\"}\n",
        "{\"t\":\"2026-09-24T12:01:00Z\",\"k\":\"msg.in\",\"content\":\"old\"}\n",
        "{\"t\":\"2026-09-24T12:02:00Z\",\"k\":\"code.edit\",\"file\":\"src/main.rs\"}\n",
    );
    let _remote = write_grep_owner(
        temp.path(),
        "recovery-owner",
        binary,
        &[("legacy-segment", legacy), (&context_id, &context)],
    );
    let tape_dir = temp.path().join("recovery-owner-home/.engram/tapes");
    let locator_dir = tape_dir.join("native-upgrade-v1");
    std::fs::create_dir_all(&locator_dir).expect("recovery locator directory");
    std::fs::write(
        locator_dir.join("legacy-segment.json"),
        serde_json::to_vec(&json!({
            "context_tape": context_id,
            "recovered": points,
        }))
        .expect("serialize recovery locator"),
    )
    .expect("write recovery locator");
    let caller_home = temp.path().join("recovery-owner-home");
    let peer_command = vec![
        "/usr/bin/env".into(),
        format!("HOME={}", caller_home.display()),
        binary.into(),
        "peer-serve".into(),
        "--stdio".into(),
    ];
    let peer = TopologyPeer {
        ssh: None,
        command: Some(peer_command),
        engram: binary.into(),
        exports: vec!["default".into()],
    };
    let mut owner = RemoteOwner::connect("recovery-owner", "caller", &peer, Duration::from_secs(5))
        .expect("connect owner");
    let response = owner
        .round(
            &[PeerRequest::new(
                "tape_facts",
                vec!["default".into()],
                json!({
                    "items": [{"tape_id": "legacy-segment", "edit_offsets": [2], "turns": [2]}]
                }),
            )],
            Duration::from_secs(5),
        )
        .into_iter()
        .next()
        .expect("tape_facts response")
        .expect("verified recovery binding");
    let facts = &response.data[0];
    assert_eq!(facts["status"], "ok");
    assert_eq!(facts["recovery_binding"]["verified"], true);
    assert_eq!(facts["recovery_binding"]["context_tape"], context_id);
    assert_eq!(facts["recovery_binding"]["points"][0]["old_offset"], 2);
    assert_eq!(facts["recovery_binding"]["points"][0]["source_offset"], 3);
    assert_eq!(facts["edit_offset_to_turn"][0]["turn"], 2);
    assert_eq!(facts["edit_offset_to_turn"][0]["recovered_source_offset"], 3);
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
fn grep_peer_that_never_answers_is_partial_and_require_complete_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-no-response",
        "{\"t\":\"2026-09-24T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-peer-no-response local\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "silent": {
                    "command": ["/usr/bin/sleep", "60"],
                    "engram": binary,
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize topology"),
    )
    .expect("write topology");

    let partial = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle-peer-no-response", "--peers", "silent"])
        .output()
        .expect("run grep with a peer that never answers");
    assert!(
        partial.status.success(),
        "partial grep should retain local results: {}",
        String::from_utf8_lossy(&partial.stderr)
    );
    let result: serde_json::Value =
        serde_json::from_slice(&partial.stdout).expect("partial grep JSON");
    assert_eq!(result["federation"]["coverage"], "partial");
    assert!(result.get("cancellation").is_none());
    assert_eq!(result["sessions"][0]["tape_id"], "caller-no-response");
    assert_eq!(result["total"], serde_json::Value::Null);
    assert_eq!(result["total_bounds"]["max"], serde_json::Value::Null);
    let silent = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "silent/default")
        .expect("silent peer source");
    assert_eq!(silent["status"], "unavailable");
    assert_eq!(silent["phase"], "open");
    assert_eq!(silent["error"]["code"], "timeout");

    let required = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-peer-no-response",
            "--peers",
            "silent",
            "--require-complete",
        ])
        .output()
        .expect("run require-complete grep with a peer that never answers");
    assert!(!required.status.success());
    assert!(
        String::from_utf8_lossy(&required.stderr).contains("incomplete_coverage"),
        "unexpected require-complete error: {}",
        String::from_utf8_lossy(&required.stderr)
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
