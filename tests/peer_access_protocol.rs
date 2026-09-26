use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use engram::access::client::{PeerRequest, RemoteOwner};
use engram::config::TopologyPeer;
use engram::index::{
    DispatchDirection, DispatchLink, QUERY_SEMANTICS_VERSION, SCHEMA_VERSION, SqliteIndex,
};
use serde_json::json;
use sha2::Digest;

#[derive(Clone, Debug, PartialEq, Eq)]
enum SnapshotEntry {
    Directory,
    File(Vec<u8>),
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, SnapshotEntry> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, SnapshotEntry>) {
        let mut entries = std::fs::read_dir(dir)
            .expect("read snapshot directory")
            .map(|entry| entry.expect("snapshot entry"))
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("relative snapshot path");
            let kind = entry.file_type().expect("snapshot file type");
            if kind.is_dir() {
                files.insert(relative.to_path_buf(), SnapshotEntry::Directory);
                visit(root, &path, files);
            } else {
                assert!(
                    kind.is_file(),
                    "fixture contains unexpected special file: {path:?}"
                );
                files.insert(
                    relative.to_path_buf(),
                    SnapshotEntry::File(std::fs::read(&path).expect("snapshot bytes")),
                );
            }
        }
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn assert_owner_unchanged_except_sqlite_sidecars(
    before: &BTreeMap<PathBuf, SnapshotEntry>,
    after: &BTreeMap<PathBuf, SnapshotEntry>,
) {
    let stable = |tree: &BTreeMap<PathBuf, SnapshotEntry>| {
        tree.iter()
            .filter(|(path, _)| {
                !matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("index.sqlite-wal" | "index.sqlite-shm")
                )
            })
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(stable(before), stable(after));
    for path in after.keys().filter(|path| !before.contains_key(*path)) {
        assert!(
            matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("index.sqlite-wal" | "index.sqlite-shm")
            ) && path.parent() == Some(Path::new(".engram")),
            "unexpected new owner path: {path:?}"
        );
    }
}

fn one_shot_handshake_peer(root: &Path, name: &str, frame: &str) -> TopologyPeer {
    let script = root.join(format!("{name}-handshake.sh"));
    std::fs::write(
        &script,
        format!("#!/bin/sh\nIFS= read -r request || exit 0\nprintf '%s\\n' '{frame}'\n"),
    )
    .expect("write one-shot peer");
    TopologyPeer {
        ssh: None,
        command: Some(vec![
            "/bin/sh".into(),
            script.to_string_lossy().into_owned(),
        ]),
        engram: "/unused/engram".into(),
        exports: vec![],
    }
}

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

fn spawn_peer_serve_with_limits(
    home: &std::path::Path,
    session_max_secs: u64,
    idle_timeout_secs: u64,
) -> Child {
    let engram_home = home.join(".engram");
    std::fs::create_dir_all(&engram_home).expect("owner home");
    std::fs::write(
        engram_home.join("topology.yml"),
        format!(
            "version: 1\nself: bounded-owner\nexports: {{}}\nlimits:\n  owner_session_max_secs: {session_max_secs}\n  owner_idle_timeout_secs: {idle_timeout_secs}\n"
        ),
    )
    .expect("owner topology");
    Command::new(env!("CARGO_BIN_EXE_engram"))
        .arg("peer-serve")
        .arg("--stdio")
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn owner peer process")
}

fn wait_for_peer_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("poll peer process") {
            return status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("peer process exceeded its owner-side session bound");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn log_peer_operations(
    root: &std::path::Path,
    machine: &str,
    binary: &str,
    peer: &mut serde_json::Value,
) -> std::path::PathBuf {
    let log_path = root.join(format!("{machine}-peer-operations.log"));
    let request_log_path = root.join(format!("{machine}-peer-requests.jsonl"));
    let script_path = root.join(format!("{machine}-logged-peer.sh"));
    let owner_home = peer["command"][1]
        .as_str()
        .expect("test owner HOME assignment")
        .to_string();
    let script = [
        "#!/bin/sh",
        "set -eu",
        "binary=\"$1\"",
        "log=\"$2\"",
        "requests=\"$3\"",
        "while IFS= read -r request; do",
        r#"  op=$(printf '%s\n' "$request" | sed -n 's/.*"op":"\([^"]*\)".*/\1/p')"#,
        r#"  printf '%s\n' "$op" >> "$log""#,
        r#"  printf '%s\n' "$request" >> "$requests""#,
        r#"  printf '%s\n' "$request""#,
        "done | \"$binary\" peer-serve --stdio",
    ]
    .join("\n");
    std::fs::write(&script_path, script).expect("write logging peer wrapper");
    peer["command"] = json!([
        "/usr/bin/env",
        owner_home,
        "/bin/sh",
        script_path,
        binary,
        log_path,
        request_log_path,
    ]);
    log_path
}

fn log_show_first_round_with_identity_barrier(
    root: &std::path::Path,
    machine: &str,
    binary: &str,
    peer: &mut serde_json::Value,
) -> std::path::PathBuf {
    let log_path = root.join(format!("{machine}-show-operations.log"));
    let request_log_path = root.join(format!("{machine}-show-requests.jsonl"));
    let pending_path = root.join(format!("{machine}-show-pending.jsonl"));
    let ids_path = root.join(format!("{machine}-show-pending.ids"));
    let responses_root = root.join(format!("{machine}-show-responses"));
    let fifo_in = root.join(format!("{machine}-show-peer-in.fifo"));
    let fifo_out = root.join(format!("{machine}-show-peer-out.fifo"));
    let script_path = root.join(format!("{machine}-show-round-barrier.sh"));
    let owner_home = peer["command"][1]
        .as_str()
        .expect("test owner HOME assignment")
        .to_string();
    let script = [
        "#!/bin/sh",
        "set -eu",
        "binary=\"$1\"",
        "log=\"$2\"",
        "requests=\"$3\"",
        "pending=\"$4\"",
        "ids=\"$5\"",
        "responses_root=\"$6\"",
        "fifo_in=\"$7\"",
        "fifo_out=\"$8\"",
        "read_terminal() { expected=\"$1\"; response_file=\"$2\"; : >\"$response_file\"; while IFS= read -r response <&4; do printf '%s\\n' \"$response\" >>\"$response_file\"; response_id=$(printf '%s\\n' \"$response\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p'); if [ \"$response_id\" = \"$expected\" ]; then case \"$response\" in *'\"end\":true'*) return 0 ;; esac; fi; done; return 1; }",
        "mkfifo \"$fifo_in\" \"$fifo_out\"",
        "\"$binary\" peer-serve --stdio <\"$fifo_in\" >\"$fifo_out\" &",
        "peer_pid=$!",
        "cleanup() { kill \"$peer_pid\" 2>/dev/null || true; wait \"$peer_pid\" 2>/dev/null || true; rm -f \"$fifo_in\" \"$fifo_out\"; }",
        "trap cleanup EXIT",
        "exec 3>\"$fifo_in\"",
        "exec 4<\"$fifo_out\"",
        ": >\"$pending\"",
        ": >\"$ids\"",
        "pending_count=0",
        "while IFS= read -r request; do",
        "  op=$(printf '%s\\n' \"$request\" | sed -n 's/.*\"op\":\"\\([^\"]*\\)\".*/\\1/p')",
        "  request_id=$(printf '%s\\n' \"$request\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p')",
        "  printf '%s\\n' \"$op\" >>\"$log\"",
        "  printf '%s\\n' \"$request\" >>\"$requests\"",
        "  case \"$op\" in",
        "    open)",
        "      printf '%s\\n' \"$request\" >&3",
        "      read_terminal \"$request_id\" \"$responses_root.open\"",
        "      cat \"$responses_root.open\"",
        "      ;;",
        "    locate_tapes|tape_facts)",
        "      printf '%s\\n' \"$request\" >>\"$pending\"",
        "      printf '%s\\n' \"$request_id\" >>\"$ids\"",
        "      pending_count=$((pending_count + 1))",
        "      if [ \"$pending_count\" -eq 2 ]; then",
        "        cat \"$pending\" >&3",
        "        index=0",
        "        while IFS= read -r pending_id; do index=$((index + 1)); read_terminal \"$pending_id\" \"$responses_root.$index\"; done <\"$ids\"",
        "        cat \"$responses_root.1\" \"$responses_root.2\"",
        "        : >\"$pending\"",
        "        : >\"$ids\"",
        "        pending_count=0",
        "      fi",
        "      ;;",
        "    *)",
        "      printf '%s\\n' \"$request\" >&3",
        "      read_terminal \"$request_id\" \"$responses_root.next\"",
        "      cat \"$responses_root.next\"",
        "      ;;",
        "  esac",
        "done",
    ]
    .join("\n");
    std::fs::write(&script_path, script).expect("write show round-barrier peer");
    peer["command"] = json!([
        "/usr/bin/env",
        owner_home,
        "/bin/sh",
        script_path,
        binary,
        log_path,
        request_log_path,
        pending_path,
        ids_path,
        responses_root,
        fifo_in,
        fifo_out,
    ]);
    log_path
}

fn log_peer_requests_with_chunk_barrier(
    root: &std::path::Path,
    machine: &str,
    binary: &str,
    peer: &mut serde_json::Value,
    chunks_per_round: usize,
    fail_chunk: Option<usize>,
) -> std::path::PathBuf {
    let log_path = root.join(format!("{machine}-peer-requests.jsonl"));
    let pending_path = root.join(format!("{machine}-pending-requests.jsonl"));
    let responses_path = root.join(format!("{machine}-responses.jsonl"));
    let fifo_in = root.join(format!("{machine}-peer-in.fifo"));
    let fifo_out = root.join(format!("{machine}-peer-out.fifo"));
    let script_path = root.join(format!("{machine}-chunk-barrier-peer.sh"));
    let owner_home = peer["command"][1]
        .as_str()
        .expect("test owner HOME assignment")
        .to_string();
    let script = [
        "#!/bin/sh",
        "set -eu",
        "binary=\"$1\"",
        "log=\"$2\"",
        "pending=\"$3\"",
        "responses=\"$4\"",
        "fifo_in=\"$5\"",
        "fifo_out=\"$6\"",
        "chunks_per_round=\"$7\"",
        "fail_chunk=\"$8\"",
        "read_responses() { response_file=\"$1\"; : >\"$response_file\"; while IFS= read -r response <&4; do printf '%s\\n' \"$response\" >>\"$response_file\"; case \"$response\" in *'\"end\":true'*) return 0 ;; esac; done; return 1; }",
        "mkfifo \"$fifo_in\" \"$fifo_out\"",
        "\"$binary\" peer-serve --stdio <\"$fifo_in\" >\"$fifo_out\" &",
        "peer_pid=$!",
        "cleanup() { kill \"$peer_pid\" 2>/dev/null || true; wait \"$peer_pid\" 2>/dev/null || true; rm -f \"$fifo_in\" \"$fifo_out\"; }",
        "trap cleanup EXIT",
        "exec 3>\"$fifo_in\"",
        "exec 4<\"$fifo_out\"",
        ": >\"$pending\"",
        ": >\"$pending.ids\"",
        "edge_requests=0",
        "while IFS= read -r request; do",
        "  printf '%s\\n' \"$request\" >>\"$log\"",
        "  op=$(printf '%s\\n' \"$request\" | sed -n 's/.*\"op\":\"\\([^\"]*\\)\".*/\\1/p')",
        "  request_id=$(printf '%s\\n' \"$request\" | sed -n 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/p')",
        "  if [ \"$op\" = lookup_edges ]; then",
        "    edge_requests=$((edge_requests + 1))",
        "    if [ \"$edge_requests\" -ge 2 ] && [ \"$edge_requests\" -le $((chunks_per_round + 1)) ]; then",
        "      printf '%s\\n' \"$request\" >>\"$pending\"",
        "      printf '%s\\n' \"$request_id\" >>\"$pending.ids\"",
        "      if [ \"$edge_requests\" -eq $((chunks_per_round + 1)) ]; then",
        "        cat \"$pending\" >&3",
        "        : >\"$pending\"",
        "        response_count=0",
        "        while IFS= read -r chunk_id; do",
        "          response_count=$((response_count + 1))",
        "          read_responses \"$responses.$response_count\" \"$chunk_id\"",
        "          if [ \"$response_count\" -eq \"$fail_chunk\" ]; then printf '{\\\"id\\\":%s,\\\"end\\\":true,\\\"ok\\\":false,\\\"stats\\\":{},\\\"error\\\":{\\\"code\\\":\\\"synthetic_failure\\\",\\\"message\\\":\\\"injected chunk failure\\\"}}\\n' \"$chunk_id\" >\"$responses.$response_count\"; fi",
        "        done <\"$pending.ids\"",
        "        while [ \"$response_count\" -gt 0 ]; do cat \"$responses.$response_count\"; response_count=$((response_count - 1)); done",
        "        : >\"$pending.ids\"",
      "      fi",
        "      continue",
        "    fi",
        "  fi",
        "  printf '%s\\n' \"$request\" >&3",
        "  read_responses \"$responses.single\" \"$request_id\"",
        "  cat \"$responses.single\"",
        "done",
    ]
    .join("\n");
    std::fs::write(&script_path, script).expect("write chunk barrier peer");
    peer["command"] = json!([
        "/usr/bin/env",
        owner_home,
        "/bin/sh",
        script_path,
        binary,
        log_path,
        pending_path,
        responses_path,
        fifo_in,
        fifo_out,
        chunks_per_round.to_string(),
        fail_chunk.unwrap_or_default().to_string(),
    ]);
    log_path
}

fn operation_count(log_path: &std::path::Path, operation: &str) -> usize {
    std::fs::read_to_string(log_path)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == operation)
        .count()
}

#[cfg(unix)]
#[test]
fn peer_round_discards_incomplete_frames_after_one_frame_for_each_operation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let script_path = temp.path().join("drop-after-one-frame-peer.sh");
    let script = [
        "#!/bin/sh",
        "set -eu",
        "drop_op=\"$1\"",
        "operation_log=\"$2\"",
        "while IFS= read -r request; do",
        r#"  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')"#,
        r#"  op=$(printf '%s\n' "$request" | sed -n 's/.*"op":"\([^"]*\)".*/\1/p')"#,
        r#"  printf '%s\n' "$op" >> "$operation_log""#,
        "  if [ \"$op\" = open ]; then",
        "    if [ \"$drop_op\" = open ]; then",
        r#"      printf '{"id":%s,"data":{"store":"matrix/default","status":"ok","db":"/fixture/matrix.sqlite","tape_dirs":[],"reader_mode":"live","snapshot_at":"2026-09-25T00:00:00Z"}}\n' "$id""#,
        "      exit 0",
        "    fi",
        r#"    printf '{"id":%s,"data":{"store":"matrix/default","status":"ok","db":"/fixture/matrix.sqlite","tape_dirs":[],"reader_mode":"live","snapshot_at":"2026-09-25T00:00:00Z"}}\n' "$id""#,
        r#"    printf '{"id":%s,"end":true,"ok":true,"stats":{"self":"matrix","build":"@BUILD@","protocol":1,"schema":@SCHEMA@,"query_semantics":@SEMANTICS@,"limits":{}}}\n' "$id""#,
        "    continue",
        "  fi",
        "  if [ \"$op\" = \"$drop_op\" ]; then",
        r#"    printf '{"id":%s,"data":{"partial":true,"op":"%s"}}\n' "$id" "$op""#,
        "    exit 0",
        "  fi",
        r#"  printf '{"id":%s,"data":{"completed":true,"op":"%s"}}\n' "$id" "$op""#,
        r#"  printf '{"id":%s,"end":true,"ok":true,"stats":{"completed":true}}\n' "$id""#,
        "done",
    ]
    .join("\n")
    .replace("@BUILD@", env!("CARGO_PKG_VERSION"))
    .replace("@SCHEMA@", &SCHEMA_VERSION.to_string())
    .replace("@SEMANTICS@", &QUERY_SEMANTICS_VERSION.to_string());
    std::fs::write(&script_path, script).expect("write drop-after-frame peer");

    let operations = [
        "open",
        "lookup_anchors",
        "lookup_edges",
        "dispatch_rows",
        "locate_tapes",
        "tape_facts",
        "grep_scan",
        "peek_lines",
        "read_file",
    ];
    for operation in operations {
        let operation_log = temp.path().join(format!("{operation}-operations.log"));
        let peer = TopologyPeer {
            ssh: None,
            command: Some(vec![
                "/bin/sh".into(),
                script_path.to_string_lossy().into_owned(),
                operation.into(),
                operation_log.to_string_lossy().into_owned(),
            ]),
            engram: binary.into(),
            exports: vec!["default".into()],
        };

        if operation == "open" {
            let error =
                match RemoteOwner::connect("matrix", "caller", &peer, Duration::from_secs(2)) {
                    Err(error) => error,
                    Ok(_) => panic!("open accepted a response without its terminal frame"),
                };
            assert_eq!(error.code, "unavailable");
            assert_eq!(
                std::fs::read_to_string(&operation_log)
                    .expect("open operation log")
                    .lines()
                    .collect::<Vec<_>>(),
                vec!["open"]
            );
            continue;
        }

        let mut owner = RemoteOwner::connect("matrix", "caller", &peer, Duration::from_secs(2))
            .unwrap_or_else(|error| panic!("open failed before {operation}: {error:?}"));
        let completed_operation = if operation == "lookup_anchors" {
            "lookup_edges"
        } else {
            "lookup_anchors"
        };
        let outcomes = owner.round(
            &[
                PeerRequest::new(completed_operation, vec!["default".into()], json!({})),
                PeerRequest::new(operation, vec!["default".into()], json!({})),
            ],
            Duration::from_secs(2),
        );
        assert_eq!(outcomes.len(), 2);
        let completed = outcomes[0].as_ref().unwrap_or_else(|error| {
            panic!("completed operation was lost before {operation}: {error:?}")
        });
        assert_eq!(completed.data[0]["completed"], true);
        assert_eq!(completed.data[0]["op"], completed_operation);
        assert_eq!(
            outcomes[1].as_ref().unwrap_err().code,
            "unavailable",
            "{operation} must fail when the peer omits its terminal frame"
        );
        assert!(
            !owner.is_connected(),
            "{operation} disconnect must retire the owner for later phases"
        );
        let follow_up = owner.round(
            &[PeerRequest::new(
                "followup_probe",
                vec!["default".into()],
                json!({}),
            )],
            Duration::from_secs(2),
        );
        assert_eq!(follow_up[0].as_ref().unwrap_err().code, "unavailable");
        let expected = vec!["open", completed_operation, operation];
        assert_eq!(
            std::fs::read_to_string(&operation_log)
                .expect("operation log")
                .lines()
                .collect::<Vec<_>>(),
            expected,
            "the failed owner must not receive another phase after {operation}"
        );
    }
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

fn set_peer_topology(caller_home: &std::path::Path, peers: serde_json::Value) {
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({"version": 1, "self": "querier", "peers": peers}))
            .expect("serialize peer topology"),
    )
    .expect("write peer topology");
}

fn jsonl(events: &[serde_json::Value]) -> String {
    events
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
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

fn install_native_fixture_subset(
    root: &std::path::Path,
    fixture: &serde_json::Value,
    tape_ids: &[&str],
    file_names: &[&str],
) {
    let home = root.join("home");
    let tape_dir = home.join(".engram/tapes");
    std::fs::create_dir_all(&tape_dir).expect("native fixture tape directory");
    std::fs::create_dir_all(root.join(".engram/cursors")).expect("native fixture cursors");
    std::fs::write(
        home.join(".engram/config.yml"),
        "db: ~/.engram/index.sqlite\ntapes_dir: ~/.engram/tapes\n",
    )
    .expect("native fixture owner config");
    let db = home.join(".engram/index.sqlite");
    let index = SqliteIndex::open_writer(db.to_str().expect("native fixture DB path"))
        .expect("native fixture DB");
    for tape_id in tape_ids {
        let raw = fixture["tapes"][*tape_id]
            .as_str()
            .expect("native fixture tape content");
        assert_eq!(
            format!("{:x}", sha2::Sha256::digest(raw.as_bytes())),
            *tape_id,
            "native fixture tape content must retain its content address"
        );
        std::fs::write(
            tape_dir.join(format!("{tape_id}.jsonl.zst")),
            zstd::stream::encode_all(raw.as_bytes(), 0).expect("compress fixture tape"),
        )
        .expect("write fixture tape");
        let links = fixture["dispatch_links"]
            .as_array()
            .expect("fixture dispatch links")
            .iter()
            .filter(|row| row[0] == *tape_id)
            .map(|row| DispatchLink {
                uuid: row[1].as_str().expect("fixture UUID").into(),
                first_turn_index: row[2].as_i64().expect("fixture turn"),
                direction: if row[3] == "sent" {
                    DispatchDirection::Sent
                } else {
                    DispatchDirection::Received
                },
            })
            .collect::<Vec<_>>();
        let parsed = engram::tape::event::parse_jsonl_events(raw).expect("parse fixture tape");
        index
            .ingest_tape_events_with_dispatch(
                tape_id,
                &parsed,
                &links,
                engram::index::lineage::LINK_THRESHOLD_DEFAULT,
            )
            .expect("index native fixture tape");
    }
    drop(index);

    for name in file_names {
        let raw = fixture["files"][*name]
            .as_str()
            .expect("native fixture source file");
        let input = root.join(name);
        std::fs::write(&input, raw).expect("write native fixture source");
        let key = format!(
            "{:x}",
            sha2::Sha256::digest(
                std::fs::canonicalize(&input)
                    .expect("canonical fixture input")
                    .to_string_lossy()
                    .as_bytes()
            )
        );
        let cursor =
            serde_json::to_vec(&fixture["cursors"][*name]).expect("serialize fixture cursor");
        std::fs::write(
            root.join(".engram/cursors").join(format!("{key}.json")),
            [cursor.as_slice(), b"\n"].concat(),
        )
        .expect("write fixture cursor");
    }
}

fn run_fixture_ingest(binary: &str, root: &std::path::Path, filename: &str) {
    let output = Command::new(binary)
        .current_dir(root)
        .env("HOME", root.join("home"))
        .args(["ingest", filename])
        .output()
        .expect("run native fixture ingest");
    assert!(
        output.status.success(),
        "native fixture ingest failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn edit_event(file: &str, before_text: &str, after_text: &str) -> serde_json::Value {
    json!({
        "t":"2026-09-25T12:02:00Z",
        "k":"code.edit",
        "file":file,
        "before_range":[1,12],
        "after_range":[1,12],
        "before_text":before_text,
        "after_text":after_text,
    })
}

fn insert_test_span_edge(
    index: &SqliteIndex,
    tape_id: &str,
    ordinal: u32,
    from_anchor: &str,
    to_anchor: &str,
) {
    index
        .insert_edge(
            &engram::index::EdgeSource {
                source_kind: engram::index::EdgeSourceKind::SpanLink,
                tape_id: tape_id.into(),
                event_offset: 1,
                pair_ordinal: ordinal,
                from_window_ordinal: i64::from(ordinal),
                to_window_ordinal: i64::from(ordinal) + 1,
            },
            &engram::index::lineage::SpanEdge {
                from_anchor: from_anchor.into(),
                to_anchor: to_anchor.into(),
                confidence: 0.95,
                location_delta: engram::index::lineage::LocationDelta::Adjacent,
                cardinality: engram::index::lineage::Cardinality::OneToOne,
                agent_link: false,
                note: Some("federated acceptance fixture".into()),
            },
        )
        .expect("insert synthetic span edge");
}

fn run_peer_explain(
    binary: &str,
    caller_home: &std::path::Path,
    repo: &std::path::Path,
    file: &str,
    peers: &str,
) -> std::process::Output {
    Command::new(binary)
        .current_dir(repo)
        .env("HOME", caller_home)
        .args(["explain", file, "--peers", peers])
        .output()
        .expect("run federated explain fixture")
}

fn owner_dispatch_rows(
    binary: &str,
    machine: &str,
    peer_config: &serde_json::Value,
    uuid: &str,
) -> Vec<serde_json::Value> {
    let peer = TopologyPeer {
        ssh: None,
        command: Some(
            peer_config["command"]
                .as_array()
                .expect("owner command")
                .iter()
                .map(|arg| arg.as_str().expect("command arg").to_string())
                .collect(),
        ),
        engram: binary.to_string(),
        exports: vec!["default".into()],
    };
    let mut owner = RemoteOwner::connect(machine, "querier", &peer, Duration::from_secs(5))
        .expect("connect fixture owner for dispatch-row check");
    owner
        .round(
            &[PeerRequest::new(
                "dispatch_rows",
                vec!["default".into()],
                json!({"by_uuid":[uuid]}),
            )],
            Duration::from_secs(5),
        )
        .pop()
        .expect("dispatch rows outcome")
        .expect("dispatch rows response")
        .data
}

fn explain_reference_projection(mut value: serde_json::Value) -> serde_json::Value {
    let object = value.as_object_mut().expect("explain payload object");
    object.remove("federation");
    for field in ["dispatch_ambiguous", "dispatch_unresolved"] {
        if object
            .get(field)
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            object.remove(field);
        }
    }
    if let Some(query) = object
        .get_mut("query")
        .and_then(serde_json::Value::as_object_mut)
    {
        query.remove("peers");
    }
    if let Some(sessions) = object
        .get_mut("sessions")
        .and_then(serde_json::Value::as_array_mut)
    {
        for session in sessions {
            if let Some(session) = session.as_object_mut() {
                session.remove("location");
                session.remove("locations");
                session.remove("tape_id");
                session.remove("physical_identity");
                session.remove("store");
                session.remove("tape_facts");
                session.remove("tape_present_locally");
            }
        }
    }
    value
}

fn write_unavailable_explain_fixture(
    root: &std::path::Path,
    binary: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let (caller_home, repo) = write_local_grep_source(
        root,
        "caller-unmatched-explain",
        "{\"t\":\"2026-09-25T00:00:00Z\",\"k\":\"note\",\"content\":\"unrelated local fixture\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
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
        .expect("serialize unavailable explain topology"),
    )
    .expect("write unavailable explain topology");
    (caller_home, repo)
}

fn scripted_anchor_validation_peer(
    root: &Path,
    machine: &str,
    binary: &str,
    scenario: &str,
) -> serde_json::Value {
    let script_path = root.join(format!("{machine}-anchor-validation-peer.sh"));
    let operation_log = root.join(format!("{machine}-anchor-validation-operations.log"));
    let script = [
        "#!/bin/sh",
        "set -eu",
        "machine=\"$1\"",
        "scenario=\"$2\"",
        "operation_log=\"$3\"",
        "lookup_count=0",
        "while IFS= read -r request; do",
        r#"  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')"#,
        r#"  op=$(printf '%s\n' "$request" | sed -n 's/.*"op":"\([^"]*\)".*/\1/p')"#,
        r#"  printf '%s\n' "$op" >> "$operation_log""#,
        "  case \"$op\" in",
        "    open)",
        r#"      printf '{"id":%s,"data":{"store":"%s/default","status":"ok","db":"/fixture/%s.sqlite","tape_dirs":[],"reader_mode":"live","snapshot_at":"2026-09-25T00:00:00Z"}}\n' "$id" "$machine" "$machine""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"self":"%s","build":"@BUILD@","protocol":1,"schema":@SCHEMA@,"query_semantics":@SEMANTICS@,"limits":{"items_per_batch":64}}}\n' "$id" "$machine""#,
        "      ;;",
        "    lookup_anchors)",
        "      lookup_count=$((lookup_count + 1))",
        "      if [ \"$lookup_count\" -eq 1 ]; then",
        r#"        printf '{"id":%s,"data":{"type":"anchor_result","store":"%s/default","anchor":"query-anchor","matching_window_anchors":["reached-anchor"]}}\n' "$id" "$machine""#,
        "        case \"$scenario\" in",
        "          direct-missing)",
        r#"            printf '{"id":%s,"data":{"type":"fragment","store":"%s/default","tape_id":"%s-tape","event_offset":1,"kind":"edit","file_path":"fixture.rs","timestamp":"2026-09-25T12:00:00Z"}}\n' "$id" "$machine" "$machine""#,
        "            ;;",
        "          direct-wrong)",
        r#"            printf '{"id":%s,"data":{"type":"fragment","store":"%s/default","tape_id":"%s-tape","event_offset":1,"anchor":17,"kind":"edit","file_path":"fixture.rs","timestamp":"2026-09-25T12:00:00Z"}}\n' "$id" "$machine" "$machine""#,
        "            ;;",
        "          direct-foreign)",
        "            if [ \"$machine\" = bad ]; then",
        r#"              printf '{"id":%s,"data":{"type":"fragment","store":"%s/default","tape_id":"%s-tape","event_offset":1,"anchor":"foreign-anchor","kind":"edit","file_path":"fixture.rs","timestamp":"2026-09-25T12:00:00Z"}}\n' "$id" "$machine" "$machine""#,
        "            else",
        r#"              printf '{"id":%s,"data":{"type":"fragment","store":"%s/default","tape_id":"%s-tape","event_offset":1,"anchor":"query-anchor","kind":"edit","file_path":"fixture.rs","timestamp":"2026-09-25T12:00:00Z"}}\n' "$id" "$machine" "$machine""#,
        "            fi",
        "            ;;",
        "          *)",
        r#"            printf '{"id":%s,"data":{"type":"fragment","store":"%s/default","tape_id":"%s-tape","event_offset":1,"anchor":"query-anchor","kind":"edit","file_path":"fixture.rs","timestamp":"2026-09-25T12:00:00Z"}}\n' "$id" "$machine" "$machine""#,
        "            ;;",
        "        esac",
        "      else",
        "        case \"$scenario\" in",
        "          touch-missing)",
        r#"            printf '{"id":%s,"data":{"type":"fragment","store":"%s/default","tape_id":"%s-tape","event_offset":2,"kind":"edit","file_path":"touch.rs","timestamp":"2026-09-25T12:01:00Z"}}\n' "$id" "$machine" "$machine""#,
        "            ;;",
        "          touch-wrong)",
        r#"            printf '{"id":%s,"data":{"type":"fragment","store":"%s/default","tape_id":"%s-tape","event_offset":2,"anchor":17,"kind":"edit","file_path":"touch.rs","timestamp":"2026-09-25T12:01:00Z"}}\n' "$id" "$machine" "$machine""#,
        "            ;;",
        "          touch-unrequested)",
        r#"            printf '{"id":%s,"data":{"type":"fragment","store":"%s/default","tape_id":"%s-tape","event_offset":2,"anchor":"never-requested","kind":"edit","file_path":"touch.rs","timestamp":"2026-09-25T12:01:00Z"}}\n' "$id" "$machine" "$machine""#,
        "            ;;",
        "          touch-reached)",
        r#"            printf '{"id":%s,"data":{"type":"fragment","store":"%s/default","tape_id":"%s-tape","event_offset":2,"anchor":"reached-anchor","kind":"edit","file_path":"touch.rs","timestamp":"2026-09-25T12:01:00Z"}}\n' "$id" "$machine" "$machine""#,
        "            ;;",
        "          *) ;;",
        "        esac",
        "      fi",
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"records":1}}\n' "$id""#,
        "      ;;",
        "    lookup_edges)",
        "      if [ \"$scenario\" = drop-edges ]; then",
        r#"        printf '{"id":%s,"data":{"type":"edge","store":"%s/default","node":"reached-anchor","from_anchor":"reached-anchor","to_anchor":"partial-parent","confidence":0.95,"location_delta":"moved","cardinality":"1:1","agent_link":false,"note":"must be discarded before terminal success"}}\n' "$id" "$machine""#,
        "        exit 0",
        "      fi",
        r#"      printf '{"id":%s,"data":{"type":"node_result","store":"%s/default","node":"reached-anchor"}}\n' "$id" "$machine""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"records":1}}\n' "$id""#,
        "      ;;",
        "    tape_facts)",
        r#"      printf '{"id":%s,"data":{"type":"tape_facts","store":"%s/default","tape_id":"%s-tape","status":"ok","indexed":true,"segment":{"tape_id":"%s-tape","message_turn_start":0},"predecessor_chain":[{"tape_id":"%s-tape","message_turn_start":0}],"chain_status":"complete","unresolved_predecessor":null,"edit_offset_to_turn":[],"turn_to_offset":[],"recovery_binding":null,"summary":{"total_lines":1,"window_start":1,"window_end":1,"latest_timestamp":"2026-09-25T12:00:00Z","files_touched":[]}}}\n' "$id" "$machine" "$machine" "$machine" "$machine""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"items":1}}\n' "$id""#,
        "      ;;",
        "    locate_tapes)",
        r#"      printf '{"id":%s,"data":{"store":"%s/default","tape_id":"%s-tape","indexed":true,"file":{"machine":"%s","path":"/fixture/%s-tape.jsonl.zst","kind":"tape"},"size_bytes":1}}\n' "$id" "$machine" "$machine" "$machine" "$machine""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"items":1}}\n' "$id""#,
        "      ;;",
        "    dispatch_rows)",
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{}}\n' "$id""#,
        "      ;;",
        "    *) exit 78 ;;",
        "  esac",
        "done",
    ]
    .join("\n")
    .replace("@BUILD@", env!("CARGO_PKG_VERSION"))
    .replace("@SCHEMA@", &SCHEMA_VERSION.to_string())
    .replace("@SEMANTICS@", &QUERY_SEMANTICS_VERSION.to_string());
    std::fs::write(&script_path, script).expect("write scripted anchor-validation peer");
    json!({
        "command": ["/bin/sh", script_path, machine, scenario, operation_log],
        "engram": binary,
        "exports": ["default"],
    })
}

fn run_anchor_validation_explain(
    binary: &str,
    caller_home: &Path,
    repo: &Path,
    peers: &str,
    require_complete: bool,
) -> std::process::Output {
    let mut command = Command::new(binary);
    command.current_dir(repo).env("HOME", caller_home).args([
        "explain",
        "query-anchor",
        "--anchor",
        "--peers",
        peers,
    ]);
    if require_complete {
        command.arg("--require-complete");
    }
    command.output().expect("run anchor-validation explain")
}

#[test]
fn explain_peer_failure_without_local_matches_emits_partial_sources() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo) = write_unavailable_explain_fixture(temp.path(), binary);

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "explain",
            "unmatched-explain-anchor",
            "--anchor",
            "--peers",
            "offline",
        ])
        .output()
        .expect("run partial explain with no local matches");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no_results"),
        "unexpected empty-partial error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("partial explain JSON");
    assert_eq!(result["query"]["command"], "explain");
    assert_eq!(result["sessions"], json!([]));
    assert_eq!(result["federation"]["coverage"], "partial");
    let offline = result["federation"]["sources"]
        .as_array()
        .expect("federated explain sources")
        .iter()
        .find(|source| source["store"] == "offline/default")
        .expect("unavailable peer source");
    assert_eq!(offline["status"], "unavailable");
    assert_eq!(offline["phase"], "open");
    assert!(offline["error"]["code"].is_string());
}

#[test]
fn explain_require_complete_peer_failure_precedes_no_results() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo) = write_unavailable_explain_fixture(temp.path(), binary);

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "explain",
            "unmatched-explain-anchor",
            "--anchor",
            "--peers",
            "offline",
            "--require-complete",
        ])
        .output()
        .expect("run require-complete explain with no local matches");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("incomplete_results"),
        "require-complete did not report incomplete coverage: {stderr}"
    );
    assert!(!stderr.contains("no_results"));
}

#[test]
fn explain_discards_interrupted_edge_and_keeps_completed_anchor_evidence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "interrupted-edge-caller",
        "{\"t\":\"2026-09-25T10:00:00Z\",\"k\":\"note\",\"content\":\"empty\"}\n",
    );
    let peer = scripted_anchor_validation_peer(temp.path(), "broken", binary, "drop-edges");
    set_peer_topology(&caller_home, json!({"broken": peer}));

    let output = run_anchor_validation_explain(binary, &caller_home, &repo, "broken", false);
    assert!(
        output.status.success(),
        "completed anchor evidence should remain useful: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert_eq!(result["federation"]["coverage"], "partial");
    assert!(
        result["lineage"].as_array().unwrap().is_empty(),
        "edge data without a terminal success frame must be discarded: {result:#}"
    );
    let completed_session = result["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["store"] == "broken/default")
        .expect("completed direct anchor result remains attributed");
    assert_eq!(completed_session["tape_id"], "broken-tape");
    assert!(completed_session["tape_facts"].is_null());

    let source = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "broken/default")
        .expect("failed source entry");
    assert_eq!(source["status"], "failed");
    assert_eq!(source["phase"], "lookup_edges");
    assert_eq!(source["error"]["code"], "unavailable");
    assert_eq!(
        std::fs::read_to_string(temp.path().join("broken-anchor-validation-operations.log"))
            .expect("peer operation log")
            .lines()
            .collect::<Vec<_>>(),
        vec!["open", "lookup_anchors", "lookup_edges"],
        "a peer that dropped its response must not receive later operations"
    );
}

#[test]
fn explain_peers_rejects_unbound_direct_anchor_fragments() {
    let binary = env!("CARGO_BIN_EXE_engram");
    for scenario in ["direct-missing", "direct-wrong"] {
        let temp = tempfile::tempdir().expect("tempdir");
        let (caller_home, repo) = write_local_grep_source(
            temp.path(),
            "unbound-direct-caller",
            "{\"t\":\"2026-09-25T10:00:00Z\",\"k\":\"note\",\"content\":\"empty\"}\n",
        );
        let peer = scripted_anchor_validation_peer(temp.path(), "bad", binary, scenario);
        set_peer_topology(&caller_home, json!({"bad": peer}));

        let partial = run_anchor_validation_explain(binary, &caller_home, &repo, "bad", false);
        assert!(!partial.status.success());
        assert!(String::from_utf8_lossy(&partial.stderr).contains("no_results"));
        let value: serde_json::Value =
            serde_json::from_slice(&partial.stdout).expect("partial explain JSON");
        assert!(value["sessions"].as_array().unwrap().is_empty());
        assert_eq!(value["federation"]["coverage"], "partial");
        let source = value["federation"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|source| source["store"] == "bad/default")
            .expect("bad peer source");
        assert_eq!(source["store"], "bad/default");
        assert_eq!(source["phase"], "lookup_anchors");
        assert_eq!(source["error"]["code"], "protocol_error");

        let required = run_anchor_validation_explain(binary, &caller_home, &repo, "bad", true);
        assert!(!required.status.success());
        let stderr = String::from_utf8_lossy(&required.stderr);
        assert!(stderr.contains("incomplete_results"), "{stderr}");
        assert!(!stderr.contains("no_results"));
    }
}

#[test]
fn explain_peers_rejects_unrequested_direct_anchor_and_keeps_healthy_peer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "foreign-direct-caller",
        "{\"t\":\"2026-09-25T10:00:00Z\",\"k\":\"note\",\"content\":\"empty\"}\n",
    );
    let bad = scripted_anchor_validation_peer(temp.path(), "bad", binary, "direct-foreign");
    let healthy = scripted_anchor_validation_peer(temp.path(), "healthy", binary, "direct-foreign");
    set_peer_topology(&caller_home, json!({"bad": bad, "healthy": healthy}));

    let output = run_anchor_validation_explain(binary, &caller_home, &repo, "bad,healthy", false);
    assert!(
        output.status.success(),
        "healthy peer result was lost: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert_eq!(value["federation"]["coverage"], "partial");
    let sessions = value["sessions"].as_array().expect("sessions");
    assert_eq!(
        sessions.len(),
        1,
        "foreign fragment became a session: {value:#}"
    );
    assert_eq!(sessions[0]["store"], "healthy/default");
    assert_eq!(sessions[0]["confidence"], 1.0);
    let sources = value["federation"]["sources"].as_array().expect("sources");
    let bad_source = sources
        .iter()
        .find(|source| source["store"] == "bad/default")
        .expect("bad peer source");
    assert_eq!(bad_source["phase"], "lookup_anchors");
    assert_eq!(bad_source["error"]["code"], "protocol_error");
}

#[test]
fn explain_peers_rejects_unbound_or_unrequested_touch_fragments() {
    let binary = env!("CARGO_BIN_EXE_engram");
    for scenario in ["touch-missing", "touch-wrong", "touch-unrequested"] {
        let temp = tempfile::tempdir().expect("tempdir");
        let (caller_home, repo) = write_local_grep_source(
            temp.path(),
            "unbound-touch-caller",
            "{\"t\":\"2026-09-25T10:00:00Z\",\"k\":\"note\",\"content\":\"empty\"}\n",
        );
        let peer = scripted_anchor_validation_peer(temp.path(), "touch", binary, scenario);
        set_peer_topology(&caller_home, json!({"touch": peer}));

        let output = run_anchor_validation_explain(binary, &caller_home, &repo, "touch", false);
        assert!(
            output.status.success(),
            "direct evidence should survive {scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("explain JSON");
        assert_eq!(
            value["federation"]["coverage"], "partial",
            "{scenario}: {value:#}"
        );
        let source = value["federation"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|source| source["store"] == "touch/default")
            .expect("touch source");
        assert_eq!(source["phase"], "lookup_anchors");
        assert_eq!(source["error"]["code"], "protocol_error");
        let session = value["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["store"] == "touch/default")
            .expect("completed direct fragment remains available");
        let offsets = session["touches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|touch| touch["event_offset"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            offsets,
            vec![1],
            "invalid touch fragment was retained: {value:#}"
        );
    }
}

#[test]
fn explain_peers_keeps_reached_only_touch_without_rescoring() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "reached-touch-caller",
        "{\"t\":\"2026-09-25T10:00:00Z\",\"k\":\"note\",\"content\":\"empty\"}\n",
    );
    let peer = scripted_anchor_validation_peer(temp.path(), "reached", binary, "touch-reached");
    set_peer_topology(&caller_home, json!({"reached": peer}));

    let output = run_anchor_validation_explain(binary, &caller_home, &repo, "reached", false);
    assert!(
        output.status.success(),
        "valid reached-only touch was rejected: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert_eq!(value["federation"]["coverage"], "complete");
    let session = value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["store"] == "reached/default")
        .expect("remote direct-evidence session");
    assert_eq!(
        session["confidence"], 1.0,
        "reached-only hit affected direct score"
    );
    let offsets = session["touches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|touch| touch["event_offset"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        offsets,
        vec![1, 2],
        "valid reached-only fragment was discarded"
    );
    assert_eq!(
        session["touches"][1]["event_offset"], 2,
        "second touch is the reached-only evidence"
    );
}

#[test]
fn explain_peers_attributes_remote_only_edits_to_their_physical_owner() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let source = (1..=24)
        .map(|line| format!("fn remote_only_{line}() {{ value_{line}(); }}\n"))
        .collect::<String>();
    let before = source.replace("remote_only", "before_remote_only");
    let tape_id = "remote-explain-tape";
    let events = [
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test"}),
        json!({
            "t":"2026-09-25T12:01:00Z",
            "k":"code.edit",
            "file":"remote-only.rs",
            "before_range":[1,24],
            "after_range":[1,24],
            "before_text":before,
            "after_text":source,
        }),
    ]
    .iter()
    .map(serde_json::Value::to_string)
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    let remote = write_grep_owner(temp.path(), "remote-owner", binary, &[(tape_id, &events)]);
    let owner_home = temp.path().join("remote-owner-home");
    let owner_db = owner_home.join(".engram/index.sqlite");
    let index = SqliteIndex::open_writer(owner_db.to_str().expect("owner DB path"))
        .expect("open owner index");
    let parsed = engram::tape::event::parse_jsonl_events(&events).expect("parse tape events");
    index
        .ingest_tape_events(
            tape_id,
            &parsed,
            engram::index::lineage::LINK_THRESHOLD_DEFAULT,
        )
        .expect("index remote-only edit");
    let query_anchors =
        engram::query::format::derive_anchor_candidates(std::slice::from_ref(&source));
    let distinct_direct_hits = query_anchors
        .iter()
        .filter(|anchor| {
            index
                .evidence_for_anchor(anchor)
                .expect("query peer evidence by anchor")
                .iter()
                .any(|fragment| fragment.tape_id == tape_id)
        })
        .count();
    assert!(
        distinct_direct_hits >= 2,
        "fixture needs distinct query anchors for the same physical tape; anchors={}, hits={distinct_direct_hits}",
        query_anchors.len()
    );
    let after_window = engram::anchor::fingerprint_windows(&source)
        .into_iter()
        .next()
        .expect("source fingerprint window");
    let after_anchor = after_window.anchor;
    let before_anchor = engram::anchor::fingerprint_windows(&before)
        .remove(0)
        .anchor;
    index
        .insert_edge(
            &engram::index::EdgeSource {
                source_kind: engram::index::EdgeSourceKind::Edit,
                tape_id: tape_id.into(),
                event_offset: 1,
                pair_ordinal: 99,
                from_window_ordinal: 0,
                to_window_ordinal: 0,
            },
            &engram::index::lineage::SpanEdge {
                from_anchor: after_anchor.clone(),
                to_anchor: before_anchor,
                confidence: 0.95,
                location_delta: engram::index::lineage::LocationDelta::Moved,
                cardinality: engram::index::lineage::Cardinality::OneToOne,
                agent_link: false,
                note: Some("synthetic remote lineage edge".into()),
            },
        )
        .expect("insert synthetic owner edge");
    assert!(
        index
            .outbound_edges(&after_anchor, 0.5, false)
            .expect("query inserted owner edge")
            .iter()
            .any(|edge| edge.from_anchor == after_anchor)
    );
    assert!(after_window.features.iter().any(|feature| {
        index
            .matching_window_anchors(feature)
            .expect("match source feature")
            .contains(&after_anchor)
    }));
    drop(index);

    let mut remote = remote;
    let remote_operations = log_peer_operations(temp.path(), "remote-owner", binary, &mut remote);
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-empty-tape",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"note\",\"content\":\"no matching source\"}\n",
    );
    std::fs::write(repo.join("remote-only.rs"), &source).expect("write query source");
    let caller_engram = caller_home.join(".engram");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&json!({"version":1,"self":"caller","peers":{"remote-owner":remote}}))
            .expect("serialize caller topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["explain", "remote-only.rs", "--peers", "remote-owner"])
        .output()
        .expect("run federated explain");
    assert!(
        output.status.success(),
        "federated explain failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    let session = value["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|session| session["session_id"] == tape_id)
        .expect("remote-only edit session");
    assert_eq!(session["location"]["machine"], "remote-owner");
    assert_eq!(session["location"]["store"], "remote-owner/default");
    let expected_confidence = distinct_direct_hits as f64 / query_anchors.len() as f64;
    let actual_confidence = session["confidence"]
        .as_f64()
        .expect("remote direct-anchor score");
    assert!(
        (actual_confidence - expected_confidence).abs() < 1e-6,
        "distinct requested anchors should score one deduplicated physical fragment: actual={actual_confidence}, expected={expected_confidence}, anchors={}, hits={distinct_direct_hits}",
        query_anchors.len()
    );
    assert_eq!(session["physical_identity"]["tape_id"], tape_id);
    assert_eq!(
        session["physical_identity"]["file"]["machine"],
        "remote-owner"
    );
    assert_eq!(
        session["physical_identity"]["file"]["path"],
        owner_home
            .join(".engram/tapes")
            .join(format!("{tape_id}.jsonl.zst"))
            .to_str()
            .expect("owner tape path")
    );
    assert_eq!(value["federation"]["coverage"], "complete");
    assert_eq!(operation_count(&remote_operations, "lookup_edges"), 2);
    assert!(
        value["lineage"]
            .as_array()
            .is_some_and(|edges| !edges.is_empty()),
        "remote lineage edge was not traversed; lookup_edges={} response={value:#}",
        operation_count(&remote_operations, "lookup_edges")
    );
    assert!(value["lineage"].as_array().unwrap().iter().any(|edge| {
        edge["store"] == "remote-owner/default"
            && edge["from_anchor"].as_str().is_some()
            && edge["to_anchor"].as_str().is_some()
    }));
    assert_eq!(operation_count(&remote_operations, "lookup_anchors"), 2);
    assert_eq!(operation_count(&remote_operations, "lookup_edges"), 2);
    assert_eq!(operation_count(&remote_operations, "tape_facts"), 1);
    assert_eq!(operation_count(&remote_operations, "locate_tapes"), 1);
    assert_eq!(operation_count(&remote_operations, "read_file"), 0);
}

#[test]
fn explain_peers_matches_local_multi_store_reference() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let source = "fn parity_target() { let result = alpha + beta; consume(result); }\n".repeat(12);
    let before = source.replace("parity_target", "before_parity_target");
    let file = "parity.rs";
    let remote_id = "parity-remote-tape";
    let remote_events = jsonl(&[
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test"}),
        edit_event(file, &before, &source),
    ]);
    let mut remote = write_grep_owner(
        temp.path(),
        "remote-owner",
        binary,
        &[(remote_id, &remote_events)],
    );
    ingest_owner_test_tape(temp.path(), "remote-owner", remote_id, &remote_events, &[]);
    let remote_operations = log_peer_operations(temp.path(), "remote-owner", binary, &mut remote);

    let local_id = "local-parity-tape";
    let local_events = jsonl(&[
        json!({"t":"2026-09-25T11:00:00Z","k":"meta","model":"peer-test"}),
        edit_event(file, &before, &source),
    ]);
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-note-tape",
        "{\"t\":\"2026-09-25T10:00:00Z\",\"k\":\"note\",\"content\":\"unrelated\"}\n",
    );
    std::fs::write(repo.join(file), &source).expect("write parity target");
    let local_tapes = repo.join(".engram/tapes");
    std::fs::write(
        local_tapes.join(format!("{local_id}.jsonl.zst")),
        zstd::stream::encode_all(local_events.as_bytes(), 0).expect("compress local parity tape"),
    )
    .expect("write local parity tape");
    let local_db = repo.join(".engram/index.sqlite");
    let index = SqliteIndex::open_writer(local_db.to_str().expect("local DB path"))
        .expect("open local index");
    let parsed =
        engram::tape::event::parse_jsonl_events(&local_events).expect("parse local events");
    index
        .ingest_tape_events_with_dispatch(
            local_id,
            &parsed,
            &[],
            engram::index::lineage::LINK_THRESHOLD_DEFAULT,
        )
        .expect("index local parity tape");
    drop(index);

    let remote_db = temp.path().join("remote-owner-home/.engram/index.sqlite");
    let local_config = caller_home.join(".engram/config.yml");
    std::fs::write(
        &local_config,
        format!(
            "db: {}\ntapes_dir: {}\nadditional_stores:\n  - {}\n",
            local_db.display(),
            local_tapes.display(),
            remote_db.display(),
        ),
    )
    .expect("write local reference layout");
    set_peer_topology(&caller_home, json!({"remote-owner":remote.clone()}));
    let local = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["explain", file])
        .output()
        .expect("run local multi-store reference");
    assert!(
        local.status.success(),
        "local reference failed: {}",
        String::from_utf8_lossy(&local.stderr)
    );
    let local: serde_json::Value =
        serde_json::from_slice(&local.stdout).expect("local explain JSON");

    std::fs::write(
        &local_config,
        format!(
            "db: {}\ntapes_dir: {}\n",
            local_db.display(),
            local_tapes.display()
        ),
    )
    .expect("restore split caller layout");
    let federated = run_peer_explain(binary, &caller_home, &repo, file, "remote-owner");
    assert!(
        federated.status.success(),
        "federated explain failed: {}",
        String::from_utf8_lossy(&federated.stderr)
    );
    let federated: serde_json::Value =
        serde_json::from_slice(&federated.stdout).expect("federated explain JSON");
    assert_eq!(
        federated["federation"]["coverage"], "complete",
        "federated parity result: {federated:#}"
    );
    assert_eq!(operation_count(&remote_operations, "read_file"), 0);
    assert_eq!(
        explain_reference_projection(federated),
        explain_reference_projection(local),
        "federated result must match the same local two-store reference after removing physical and federation-only fields"
    );
}

#[test]
fn explain_peers_folds_a_remote_received_marker_to_one_local_sender() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let uuid = "123e4567-e89b-12d3-a456-426614174000";
    let source = "fn remotely_edited() { dispatch_marker_is_real(); }\n".repeat(12);
    let before = source.replace("remotely_edited", "before_remote_edit");
    let receiver_tape = "remote-receiver-tape";
    let receiver_events = [
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T12:01:00Z","k":"msg.in","content":format!("<engram-src id=\"{uuid}\"/>")}),
        json!({
            "t":"2026-09-25T12:02:00Z",
            "k":"code.edit",
            "file":"handoff.rs",
            "before_range":[1,12],
            "after_range":[1,12],
            "before_text":before,
            "after_text":source,
        }),
    ]
    .iter()
    .map(serde_json::Value::to_string)
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    let remote = write_grep_owner(
        temp.path(),
        "remote-owner",
        binary,
        &[(receiver_tape, &receiver_events)],
    );
    let owner_db = temp.path().join("remote-owner-home/.engram/index.sqlite");
    let owner_index =
        SqliteIndex::open_writer(owner_db.to_str().expect("owner DB path")).expect("owner index");
    let receiver_parsed = engram::tape::event::parse_jsonl_events(&receiver_events)
        .expect("parse remote receiver events");
    owner_index
        .ingest_tape_events_with_dispatch(
            receiver_tape,
            &receiver_parsed,
            &[DispatchLink {
                uuid: uuid.into(),
                first_turn_index: 0,
                direction: DispatchDirection::Received,
            }],
            engram::index::lineage::LINK_THRESHOLD_DEFAULT,
        )
        .expect("index receiver and incoming marker");
    drop(owner_index);

    let sender_tape = "local-sender-tape";
    let sender_events = format!(
        "{{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"meta\",\"model\":\"local-test\"}}\n{{\"t\":\"2026-09-25T11:01:00Z\",\"k\":\"msg.out\",\"content\":\"<engram-src id=\\\"{uuid}\\\"/>\"}}\n"
    );
    let (caller_home, repo) = write_local_grep_source(temp.path(), sender_tape, &sender_events);
    let caller_db = repo.join(".engram/index.sqlite");
    let caller_index = SqliteIndex::open_writer(caller_db.to_str().expect("caller DB path"))
        .expect("caller index");
    caller_index
        .insert_dispatch_link(
            sender_tape,
            &DispatchLink {
                uuid: uuid.into(),
                first_turn_index: 0,
                direction: DispatchDirection::Sent,
            },
        )
        .expect("index local outgoing marker");
    drop(caller_index);
    std::fs::write(repo.join("handoff.rs"), &source).expect("write query source");

    let mut remote = remote;
    let operations = log_peer_operations(temp.path(), "remote-owner", binary, &mut remote);
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({"version":1,"self":"caller","peers":{"remote-owner":remote}}))
            .expect("serialize caller topology"),
    )
    .expect("write caller topology");
    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["explain", "handoff.rs", "--peers", "remote-owner"])
        .output()
        .expect("run federated explain");
    assert!(
        output.status.success(),
        "federated explain failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert_eq!(value["federation"]["coverage"], "complete");
    let hop = value["dispatch_lineage"]
        .as_array()
        .expect("dispatch lineage")
        .first()
        .expect("two-sided handoff");
    assert_eq!(hop["session"], receiver_tape);
    assert_eq!(hop["received_uuid"], uuid);
    assert_eq!(hop["parent_session"], sender_tape);
    assert_eq!(hop["session_location"], "remote-owner/default");
    assert_eq!(hop["parent_location"], "caller/local:0");
    assert_eq!(hop["selection_coverage"], "complete");
    assert!(value["sessions"].as_array().unwrap().iter().any(|session| {
        session["session_id"] == sender_tape && session["location"]["store"] == "caller/local:0"
    }));
    assert!(operation_count(&operations, "dispatch_rows") >= 2);
    assert_eq!(operation_count(&operations, "read_file"), 0);
}

#[test]
fn explain_peers_finds_two_sided_handoff_when_querier_holds_neither_endpoint() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let uuid = "223e4567-e89b-12d3-a456-426614174010";
    let file = "remote-handoff.rs";
    let source =
        "fn physically_split_edit() { let value = one + two; use_value(value); }\n".repeat(12);
    let before = source.replace("physically_split_edit", "before_split_edit");
    let sender_id = "machine-a-sender";
    let sender_events = jsonl(&[
        json!({"t":"2026-09-25T11:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T11:01:00Z","k":"msg.out","content":format!("<engram-src id=\"{uuid}\"/>")}),
    ]);
    let receiver_id = "machine-b-receiver";
    let receiver_events = jsonl(&[
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T12:01:00Z","k":"msg.in","content":format!("<engram-src id=\"{uuid}\"/>")}),
        edit_event(file, &before, &source),
    ]);
    let mut sender = write_grep_owner(
        temp.path(),
        "machine-a",
        binary,
        &[(sender_id, &sender_events)],
    );
    ingest_owner_test_tape(
        temp.path(),
        "machine-a",
        sender_id,
        &sender_events,
        &[DispatchLink {
            uuid: uuid.into(),
            first_turn_index: 0,
            direction: DispatchDirection::Sent,
        }],
    );
    let mut receiver = write_grep_owner(
        temp.path(),
        "machine-b",
        binary,
        &[(receiver_id, &receiver_events)],
    );
    ingest_owner_test_tape(
        temp.path(),
        "machine-b",
        receiver_id,
        &receiver_events,
        &[DispatchLink {
            uuid: uuid.into(),
            first_turn_index: 0,
            direction: DispatchDirection::Received,
        }],
    );
    let remote_sender_rows = owner_dispatch_rows(binary, "machine-a", &sender, uuid);
    assert!(
        remote_sender_rows
            .iter()
            .any(|row| row["tape_id"] == sender_id && row["direction"] == "sent"),
        "remote sender owner omitted its indexed UUID row: {remote_sender_rows:#?}"
    );
    let remote_receiver_rows = owner_dispatch_rows(binary, "machine-b", &receiver, uuid);
    assert!(
        remote_receiver_rows
            .iter()
            .any(|row| row["tape_id"] == receiver_id && row["direction"] == "received"),
        "remote receiver owner omitted its indexed UUID row: {remote_receiver_rows:#?}"
    );
    let sender_operations = log_peer_operations(temp.path(), "machine-a", binary, &mut sender);
    let receiver_operations = log_peer_operations(temp.path(), "machine-b", binary, &mut receiver);

    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "querier-note",
        "{\"t\":\"2026-09-25T10:00:00Z\",\"k\":\"note\",\"content\":\"querier holds neither handoff endpoint\"}\n",
    );
    std::fs::write(repo.join(file), &source).expect("write target source");
    let shadow_id = "querier-shadow-edit";
    let shadow_events = jsonl(&[
        json!({"t":"2026-09-25T10:30:00Z","k":"meta","model":"peer-test"}),
        edit_event(file, &before, &source),
    ]);
    std::fs::write(
        repo.join(format!(".engram/tapes/{shadow_id}.jsonl.zst")),
        zstd::stream::encode_all(shadow_events.as_bytes(), 0).expect("compress shadow tape"),
    )
    .expect("write shadow tape");
    let local_index = SqliteIndex::open_writer(
        repo.join(".engram/index.sqlite")
            .to_str()
            .expect("querier DB path"),
    )
    .expect("open querier index");
    let parsed = engram::tape::event::parse_jsonl_events(&shadow_events).expect("parse shadow");
    local_index
        .ingest_tape_events_with_dispatch(
            shadow_id,
            &parsed,
            &[],
            engram::index::lineage::LINK_THRESHOLD_DEFAULT,
        )
        .expect("index shadow edit");
    drop(local_index);

    set_peer_topology(
        &caller_home,
        json!({"machine-a":sender.clone(), "machine-b":receiver.clone()}),
    );
    let complete = run_peer_explain(binary, &caller_home, &repo, file, "machine-a,machine-b");
    assert!(
        complete.status.success(),
        "two-sided federated explain failed: {}",
        String::from_utf8_lossy(&complete.stderr)
    );
    let complete: serde_json::Value =
        serde_json::from_slice(&complete.stdout).expect("explain JSON");
    let [hop] = complete["dispatch_lineage"].as_array().unwrap().as_slice() else {
        panic!(
            "expected one physical two-sided hop: {complete:#}\nsender requests:\n{}\nreceiver requests:\n{}",
            std::fs::read_to_string(temp.path().join("machine-a-peer-requests.jsonl"))
                .unwrap_or_default(),
            std::fs::read_to_string(temp.path().join("machine-b-peer-requests.jsonl"))
                .unwrap_or_default()
        );
    };
    assert_eq!(hop["session"], receiver_id);
    assert_eq!(hop["parent_session"], sender_id);
    assert_eq!(hop["received_uuid"], uuid);
    assert_eq!(hop["session_location"], "machine-b/default");
    assert_eq!(hop["parent_location"], "machine-a/default");
    assert!(
        complete["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| {
                session["session_id"] == receiver_id
                    && session["physical_identity"]["machine"] == "machine-b"
            })
    );
    assert!(
        complete["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| {
                session["session_id"] == sender_id
                    && session["physical_identity"]["machine"] == "machine-a"
            })
    );
    assert_eq!(operation_count(&sender_operations, "read_file"), 0);
    assert_eq!(operation_count(&receiver_operations, "read_file"), 0);

    set_peer_topology(&caller_home, json!({"machine-b":receiver}));
    let sender_removed = run_peer_explain(binary, &caller_home, &repo, file, "machine-b");
    assert!(sender_removed.status.success());
    let sender_removed: serde_json::Value =
        serde_json::from_slice(&sender_removed.stdout).expect("sender-removed explain JSON");
    assert!(
        sender_removed["dispatch_lineage"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        sender_removed["dispatch_unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["reason"] == "no_sender_observed" && row["uuid"] == uuid })
    );

    set_peer_topology(&caller_home, json!({"machine-a":sender}));
    let receiver_removed = run_peer_explain(binary, &caller_home, &repo, file, "machine-a");
    assert!(receiver_removed.status.success());
    let receiver_removed: serde_json::Value =
        serde_json::from_slice(&receiver_removed.stdout).expect("receiver-removed explain JSON");
    assert!(
        receiver_removed["dispatch_lineage"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        receiver_removed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| { session["session_id"] == shadow_id })
    );
}

#[test]
fn explain_peers_follows_three_machine_chain_and_excludes_sibling_receiver() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let first_uuid = "323e4567-e89b-12d3-a456-426614174020";
    let second_uuid = "323e4567-e89b-12d3-a456-426614174021";
    let file = "three-machine.rs";
    let source =
        "fn three_machine_edit() { let answer = left + right; use_answer(answer); }\n".repeat(12);
    let before = source.replace("three_machine_edit", "before_three_machine_edit");

    let a_id = "machine-a-origin";
    let a_events = jsonl(&[
        json!({"t":"2026-09-25T10:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T10:01:00Z","k":"msg.out","content":format!("<engram-src id=\"{first_uuid}\"/>")}),
    ]);
    let mut a = write_grep_owner(temp.path(), "machine-a", binary, &[(a_id, &a_events)]);
    ingest_owner_test_tape(
        temp.path(),
        "machine-a",
        a_id,
        &a_events,
        &[DispatchLink {
            uuid: first_uuid.into(),
            first_turn_index: 0,
            direction: DispatchDirection::Sent,
        }],
    );

    let b_id = "machine-b-middle";
    let b_events = jsonl(&[
        json!({"t":"2026-09-25T11:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T11:01:00Z","k":"msg.in","content":format!("<engram-src id=\"{first_uuid}\"/>")}),
        json!({"t":"2026-09-25T11:02:00Z","k":"msg.out","content":format!("<engram-src id=\"{second_uuid}\"/>")}),
    ]);
    let sibling_id = "machine-b-sibling";
    let sibling_events = jsonl(&[
        json!({"t":"2026-09-25T11:10:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T11:11:00Z","k":"msg.in","content":format!("<engram-src id=\"{first_uuid}\"/>")}),
    ]);
    let mut b = write_grep_owner(
        temp.path(),
        "machine-b",
        binary,
        &[(b_id, &b_events), (sibling_id, &sibling_events)],
    );
    ingest_owner_test_tape(
        temp.path(),
        "machine-b",
        b_id,
        &b_events,
        &[
            DispatchLink {
                uuid: first_uuid.into(),
                first_turn_index: 0,
                direction: DispatchDirection::Received,
            },
            DispatchLink {
                uuid: second_uuid.into(),
                first_turn_index: 1,
                direction: DispatchDirection::Sent,
            },
        ],
    );
    ingest_owner_test_tape(
        temp.path(),
        "machine-b",
        sibling_id,
        &sibling_events,
        &[DispatchLink {
            uuid: first_uuid.into(),
            first_turn_index: 0,
            direction: DispatchDirection::Received,
        }],
    );

    let c_id = "machine-c-receiver";
    let c_events = jsonl(&[
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T12:01:00Z","k":"msg.in","content":format!("<engram-src id=\"{second_uuid}\"/>")}),
        edit_event(file, &before, &source),
    ]);
    let mut c = write_grep_owner(temp.path(), "machine-c", binary, &[(c_id, &c_events)]);
    ingest_owner_test_tape(
        temp.path(),
        "machine-c",
        c_id,
        &c_events,
        &[DispatchLink {
            uuid: second_uuid.into(),
            first_turn_index: 0,
            direction: DispatchDirection::Received,
        }],
    );
    let a_operations = log_peer_operations(temp.path(), "machine-a", binary, &mut a);
    let b_operations = log_peer_operations(temp.path(), "machine-b", binary, &mut b);
    let c_operations = log_peer_operations(temp.path(), "machine-c", binary, &mut c);

    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "querier-unrelated",
        "{\"t\":\"2026-09-25T09:00:00Z\",\"k\":\"note\",\"content\":\"no handoff endpoints here\"}\n",
    );
    std::fs::write(repo.join(file), &source).expect("write three-machine target");
    set_peer_topology(
        &caller_home,
        json!({"machine-a":a, "machine-b":b, "machine-c":c}),
    );
    let output = run_peer_explain(
        binary,
        &caller_home,
        &repo,
        file,
        "machine-a,machine-b,machine-c",
    );
    assert!(
        output.status.success(),
        "three-machine federated explain failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert_eq!(value["federation"]["coverage"], "complete");
    let hops = value["dispatch_lineage"]
        .as_array()
        .expect("dispatch lineage");
    assert_eq!(hops.len(), 2, "expected A -> B -> C only: {value:#}");
    let hop_by_session = |session: &str| {
        hops.iter()
            .find(|hop| hop["session"] == session)
            .unwrap_or_else(|| panic!("missing hop for {session}: {value:#}"))
    };
    assert_eq!(hop_by_session(c_id)["parent_session"], b_id);
    assert_eq!(
        hop_by_session(c_id)["session_location"],
        "machine-c/default"
    );
    assert_eq!(hop_by_session(c_id)["parent_location"], "machine-b/default");
    assert_eq!(hop_by_session(b_id)["parent_session"], a_id);
    assert_eq!(
        hop_by_session(b_id)["session_location"],
        "machine-b/default"
    );
    assert_eq!(hop_by_session(b_id)["parent_location"], "machine-a/default");
    assert!(
        value["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|session| { session["session_id"] != sibling_id })
    );
    for operations in [&a_operations, &b_operations, &c_operations] {
        assert_eq!(operation_count(operations, "read_file"), 0);
    }
}

#[test]
fn explain_peers_matches_local_native_split_and_legacy_recovery_with_remote_sender() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native-upgrade/codex-split.json"))
            .expect("native split fixture");
    let sender_file = "sender.codex.jsonl";
    let receiver_file = "receiver.codex.jsonl";
    let sender_tape = fixture["cursors"][sender_file]["tape_id"]
        .as_str()
        .expect("legacy sender tape");
    let receiver_current = fixture["cursors"][receiver_file]["tape_id"]
        .as_str()
        .expect("legacy receiver tape");
    let receiver_prefix = fixture["dispatch_links"][0][0]
        .as_str()
        .expect("split receiver prefix tape");
    let tape_ids = [sender_tape, receiver_prefix, receiver_current];
    let source = "pub fn isolated_handoff_probe() -> u64 { 73849127 }\n";
    let uuid = fixture["dispatch_links"][0][1]
        .as_str()
        .expect("native handoff UUID");
    let later = json!({
        "type":"response_item",
        "timestamp":"2026-09-21T00:00:04Z",
        "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":format!("<engram-src id=\"{uuid}\"/> later unrelated")} ]},
    })
    .to_string()
        + "\n";
    let binary = env!("CARGO_BIN_EXE_engram");

    let baseline_root = tempfile::tempdir().expect("local baseline tempdir");
    install_native_fixture_subset(
        baseline_root.path(),
        &fixture,
        &tape_ids,
        &[sender_file, receiver_file],
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(baseline_root.path().join(receiver_file))
        .expect("open local baseline receiver")
        .write_all(later.as_bytes())
        .expect("append local baseline receiver");
    run_fixture_ingest(binary, baseline_root.path(), receiver_file);
    run_fixture_ingest(binary, baseline_root.path(), sender_file);
    let baseline_repo = baseline_root.path().join("repo");
    std::fs::create_dir_all(&baseline_repo).expect("baseline repo");
    std::fs::write(baseline_repo.join("probe.rs"), source).expect("write baseline query source");
    let baseline = Command::new(binary)
        .current_dir(&baseline_repo)
        .env("HOME", baseline_root.path().join("home"))
        .args(["explain", "probe.rs"])
        .output()
        .expect("run local native baseline explain");
    assert!(
        baseline.status.success(),
        "local native baseline failed: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    let baseline: serde_json::Value =
        serde_json::from_slice(&baseline.stdout).expect("baseline JSON");
    assert_eq!(baseline["dispatch_lineage"].as_array().unwrap().len(), 1);

    let sender_root = tempfile::tempdir().expect("remote sender tempdir");
    install_native_fixture_subset(sender_root.path(), &fixture, &[sender_tape], &[sender_file]);
    run_fixture_ingest(binary, sender_root.path(), sender_file);
    let receiver_root = tempfile::tempdir().expect("remote receiver tempdir");
    install_native_fixture_subset(
        receiver_root.path(),
        &fixture,
        &[receiver_prefix, receiver_current],
        &[receiver_file],
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(receiver_root.path().join(receiver_file))
        .expect("open remote receiver")
        .write_all(later.as_bytes())
        .expect("append remote receiver");
    run_fixture_ingest(binary, receiver_root.path(), receiver_file);

    let configure_native_peer = |root: &std::path::Path, machine: &str| {
        let owner_home = root.join("home");
        let engram_home = owner_home.join(".engram");
        let db = engram_home.join("index.sqlite");
        let tapes = engram_home.join("tapes");
        std::fs::write(
            engram_home.join("topology.yml"),
            format!(
                "version: 1\nself: {machine}\nexports:\n  default:\n    db: {}\n    tape_dirs:\n      - {}\n",
                db.display(),
                tapes.display()
            ),
        )
        .expect("write native owner topology");
        json!({
            "command":["/usr/bin/env", format!("HOME={}", owner_home.display()), binary, "peer-serve", "--stdio"],
            "engram":binary,
            "exports":["default"],
        })
    };
    let sender_peer = configure_native_peer(sender_root.path(), "machine-a");
    let receiver_peer = configure_native_peer(receiver_root.path(), "machine-b");
    let caller_root = tempfile::tempdir().expect("querier tempdir");
    let (caller_home, repo) = write_local_grep_source(
        caller_root.path(),
        "querier-empty",
        "{\"t\":\"2026-09-25T09:00:00Z\",\"k\":\"note\",\"content\":\"querier holds neither endpoint\"}\n",
    );
    std::fs::write(repo.join("probe.rs"), source).expect("write federated query source");
    set_peer_topology(
        &caller_home,
        json!({"machine-a":sender_peer, "machine-b":receiver_peer}),
    );
    let federated = run_peer_explain(
        binary,
        &caller_home,
        &repo,
        "probe.rs",
        "machine-a,machine-b",
    );
    assert!(
        federated.status.success(),
        "federated native explain failed: {}",
        String::from_utf8_lossy(&federated.stderr)
    );
    let federated: serde_json::Value =
        serde_json::from_slice(&federated.stdout).expect("federated native JSON");
    assert_eq!(
        federated["federation"]["coverage"], "complete",
        "native split owner result: {federated:#}"
    );
    let local_hop = &baseline["dispatch_lineage"][0];
    let remote_hop = federated["dispatch_lineage"]
        .as_array()
        .unwrap()
        .first()
        .expect("federated recovered handoff");
    for field in [
        "session",
        "edit_session",
        "edit_event_offset",
        "received_uuid",
        "received_turn_index",
        "edit_turn_index",
        "parent_session",
        "parent_sent_turn_index",
    ] {
        assert_eq!(remote_hop[field], local_hop[field], "field {field}");
    }
    assert_eq!(remote_hop["received_uuid"], uuid);
    assert_eq!(remote_hop["parent_location"], "machine-a/default");
    assert_eq!(remote_hop["session_location"], "machine-b/default");
    let local_sessions = baseline["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|session| session["session_id"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let remote_sessions = federated["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|session| session["session_id"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(remote_sessions, local_sessions);
}

#[test]
fn explain_peers_reports_missing_segment_and_missing_tape_without_dropping_sessions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let uuid = "423e4567-e89b-12d3-a456-426614174030";
    let file = "incomplete-history.rs";
    let source = "fn incomplete_history_edit() { let result = red + blue; use_result(result); }\n"
        .repeat(12);
    let before = source.replace("incomplete_history_edit", "before_incomplete_history_edit");

    let sender_id = "complete-sender";
    let sender_events = jsonl(&[
        json!({"t":"2026-09-25T10:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T10:01:00Z","k":"msg.out","content":format!("<engram-src id=\"{uuid}\"/>")}),
    ]);
    let mut sender = write_grep_owner(
        temp.path(),
        "sender-owner",
        binary,
        &[(sender_id, &sender_events)],
    );
    ingest_owner_test_tape(
        temp.path(),
        "sender-owner",
        sender_id,
        &sender_events,
        &[DispatchLink {
            uuid: uuid.into(),
            first_turn_index: 0,
            direction: DispatchDirection::Sent,
        }],
    );

    let missing_predecessor_id = "missing-predecessor-receiver";
    let missing_predecessor_events = jsonl(&[
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test","ingest_continuation":{"previous_tape_id":"deleted-predecessor-segment"}}),
        json!({"t":"2026-09-25T12:01:00Z","k":"msg.in","content":format!("<engram-src id=\"{uuid}\"/>")}),
        edit_event(file, &before, &source),
    ]);
    let missing_tape_id = "missing-file-receiver";
    let missing_tape_events = jsonl(&[
        json!({"t":"2026-09-25T12:10:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T12:11:00Z","k":"msg.in","content":format!("<engram-src id=\"{uuid}\"/>")}),
        edit_event(file, &before, &source),
    ]);
    let mut receiver = write_grep_owner(
        temp.path(),
        "receiver-owner",
        binary,
        &[
            (missing_predecessor_id, &missing_predecessor_events),
            (missing_tape_id, &missing_tape_events),
        ],
    );
    for (tape_id, events) in [
        (missing_predecessor_id, &missing_predecessor_events),
        (missing_tape_id, &missing_tape_events),
    ] {
        ingest_owner_test_tape(
            temp.path(),
            "receiver-owner",
            tape_id,
            events,
            &[DispatchLink {
                uuid: uuid.into(),
                first_turn_index: 0,
                direction: DispatchDirection::Received,
            }],
        );
    }
    std::fs::remove_file(
        temp.path()
            .join("receiver-owner-home/.engram/tapes")
            .join(format!("{missing_tape_id}.jsonl.zst")),
    )
    .expect("remove indexed tape file");
    let _sender_operations = log_peer_operations(temp.path(), "sender-owner", binary, &mut sender);
    let receiver_operations =
        log_peer_operations(temp.path(), "receiver-owner", binary, &mut receiver);

    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "querier-unrelated",
        "{\"t\":\"2026-09-25T09:00:00Z\",\"k\":\"note\",\"content\":\"empty\"}\n",
    );
    std::fs::write(repo.join(file), &source).expect("write incomplete-history target");
    set_peer_topology(
        &caller_home,
        json!({"sender-owner":sender, "receiver-owner":receiver}),
    );
    let output = run_peer_explain(
        binary,
        &caller_home,
        &repo,
        file,
        "sender-owner,receiver-owner",
    );
    assert!(
        output.status.success(),
        "partial history should remain explainable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert_eq!(value["federation"]["coverage"], "partial");
    assert!(value["dispatch_lineage"].as_array().unwrap().is_empty());
    assert!(
        value["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| { session["session_id"] == missing_predecessor_id })
    );
    assert!(
        value["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| { session["session_id"] == missing_tape_id })
    );
    let unresolved = value["dispatch_unresolved"]
        .as_array()
        .expect("unresolved rows");
    assert!(unresolved.iter().any(|row| {
        row["reason"] == "history_incomplete"
            && row["session"] == missing_predecessor_id
            && row["missing"] == "deleted-predecessor-segment"
    }));
    assert!(
        unresolved.iter().any(|row| {
            row["reason"] == "tape_unavailable" && row["session"] == missing_tape_id
        })
    );
    assert_eq!(operation_count(&receiver_operations, "read_file"), 0);
}

#[test]
fn explain_peers_reports_missing_sender_owner_without_inventing_a_hop() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let uuid = "123e4567-e89b-12d3-a456-426614174001";
    let source =
        "fn remote_edit_without_observed_sender() { marker_is_received_only(); }\n".repeat(8);
    let before = source.replace(
        "remote_edit_without_observed_sender",
        "before_missing_sender",
    );
    let receiver_tape = "remote-receiver-without-sender";
    let receiver_events = [
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T12:01:00Z","k":"msg.in","content":format!("<engram-src id=\"{uuid}\"/>")}),
        json!({
            "t":"2026-09-25T12:02:00Z",
            "k":"code.edit",
            "file":"missing-sender.rs",
            "before_range":[1,8],
            "after_range":[1,8],
            "before_text":before,
            "after_text":source,
        }),
    ]
    .iter()
    .map(serde_json::Value::to_string)
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    let remote = write_grep_owner(
        temp.path(),
        "remote-owner",
        binary,
        &[(receiver_tape, &receiver_events)],
    );
    let owner_db = temp.path().join("remote-owner-home/.engram/index.sqlite");
    let owner_index =
        SqliteIndex::open_writer(owner_db.to_str().expect("owner DB path")).expect("owner index");
    let receiver_parsed = engram::tape::event::parse_jsonl_events(&receiver_events)
        .expect("parse remote receiver events");
    owner_index
        .ingest_tape_events_with_dispatch(
            receiver_tape,
            &receiver_parsed,
            &[DispatchLink {
                uuid: uuid.into(),
                first_turn_index: 0,
                direction: DispatchDirection::Received,
            }],
            engram::index::lineage::LINK_THRESHOLD_DEFAULT,
        )
        .expect("index received-only marker");
    drop(owner_index);
    let remote = remote;
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "empty-caller-tape",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"note\",\"content\":\"no sender\"}\n",
    );
    std::fs::write(repo.join("missing-sender.rs"), &source).expect("write query source");
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version":1,
            "self":"caller",
            "peers":{
                "remote-owner":remote,
                "silent-owner":{"command":["/bin/false"],"engram":"engram","exports":["default"]},
            }
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");
    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "explain",
            "missing-sender.rs",
            "--peers",
            "remote-owner,silent-owner",
        ])
        .output()
        .expect("run federated explain");
    assert!(
        output.status.success(),
        "partial explain should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert_eq!(value["federation"]["coverage"], "partial");
    assert!(value["dispatch_lineage"].as_array().unwrap().is_empty());
    let unresolved = value["dispatch_unresolved"]
        .as_array()
        .expect("dispatch unresolved");
    assert!(
        unresolved.iter().any(|row| {
            row["reason"] == "no_sender_observed"
                && row["uuid"] == uuid
                && row["stores_unavailable"].as_array().is_some_and(|stores| {
                    stores.iter().any(|store| store == "silent-owner/default")
                })
        }),
        "missing sender attribution was not preserved: {value:#}"
    );
}

#[test]
fn explain_peers_keeps_two_independent_remote_senders_ambiguous() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let uuid = "523e4567-e89b-12d3-a456-426614174040";
    let file = "ambiguous-handoff.rs";
    let source =
        "fn ambiguous_remote_edit() { let output = one + two; use_output(output); }\n".repeat(12);
    let before = source.replace("ambiguous_remote_edit", "before_ambiguous_remote_edit");
    let mut peers = serde_json::Map::new();
    let mut senders = Vec::new();
    for machine in ["sender-alpha", "sender-beta"] {
        let tape_id = format!("{machine}-session");
        let events = jsonl(&[
            json!({"t":"2026-09-25T11:00:00Z","k":"meta","model":"peer-test"}),
            json!({"t":"2026-09-25T11:01:00Z","k":"msg.out","content":format!("<engram-src id=\"{uuid}\"/>")}),
        ]);
        let peer = write_grep_owner(temp.path(), machine, binary, &[(tape_id.as_str(), &events)]);
        ingest_owner_test_tape(
            temp.path(),
            machine,
            &tape_id,
            &events,
            &[DispatchLink {
                uuid: uuid.into(),
                first_turn_index: 0,
                direction: DispatchDirection::Sent,
            }],
        );
        peers.insert(machine.into(), peer);
        senders.push(tape_id);
    }

    let receiver_id = "ambiguous-receiver";
    let receiver_events = jsonl(&[
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T12:01:00Z","k":"msg.in","content":format!("<engram-src id=\"{uuid}\"/>")}),
        edit_event(file, &before, &source),
    ]);
    let receiver = write_grep_owner(
        temp.path(),
        "receiver",
        binary,
        &[(receiver_id, &receiver_events)],
    );
    ingest_owner_test_tape(
        temp.path(),
        "receiver",
        receiver_id,
        &receiver_events,
        &[DispatchLink {
            uuid: uuid.into(),
            first_turn_index: 0,
            direction: DispatchDirection::Received,
        }],
    );
    peers.insert("receiver".into(), receiver);

    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "querier-empty",
        "{\"t\":\"2026-09-25T10:00:00Z\",\"k\":\"note\",\"content\":\"no endpoints\"}\n",
    );
    std::fs::write(repo.join(file), &source).expect("write ambiguity target");
    set_peer_topology(&caller_home, serde_json::Value::Object(peers));
    let output = run_peer_explain(
        binary,
        &caller_home,
        &repo,
        file,
        "sender-alpha,sender-beta,receiver",
    );
    assert!(
        output.status.success(),
        "ambiguity query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert!(value["dispatch_lineage"].as_array().unwrap().is_empty());
    let [ambiguity] = value["dispatch_ambiguous"].as_array().unwrap().as_slice() else {
        panic!("expected one ambiguous marker: {value:#}");
    };
    assert_eq!(ambiguity["received_uuid"], uuid);
    assert_eq!(ambiguity["received_session"], receiver_id);
    let mut candidates = ambiguity["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|candidate| candidate["session"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    candidates.sort();
    let mut expected = senders;
    expected.sort();
    assert_eq!(candidates, expected);
}

#[test]
fn explain_peers_projects_two_remote_task_parents_for_one_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let uuid_a = "623e4567-e89b-12d3-a456-426614174050";
    let uuid_b = "623e4567-e89b-12d3-a456-426614174051";
    let file = "multi-parent.rs";
    let source =
        "fn edits_from_two_tasks() { let answer = red + blue; use_answer(answer); }\n".repeat(12);
    let before = source.replace("edits_from_two_tasks", "before_multi_parent");
    let mut peers = serde_json::Map::new();
    let mut parent_ids = Vec::new();
    for (machine, uuid) in [("task-alpha", uuid_a), ("task-beta", uuid_b)] {
        let tape_id = format!("{machine}-sender");
        let events = jsonl(&[
            json!({"t":"2026-09-25T10:00:00Z","k":"meta","model":"peer-test"}),
            json!({"t":"2026-09-25T10:01:00Z","k":"msg.out","content":format!("<engram-src id=\"{uuid}\"/>")}),
        ]);
        let peer = write_grep_owner(temp.path(), machine, binary, &[(tape_id.as_str(), &events)]);
        ingest_owner_test_tape(
            temp.path(),
            machine,
            &tape_id,
            &events,
            &[DispatchLink {
                uuid: uuid.into(),
                first_turn_index: 0,
                direction: DispatchDirection::Sent,
            }],
        );
        peers.insert(machine.into(), peer);
        parent_ids.push(tape_id);
    }

    let receiver_id = "two-task-receiver";
    let mut second_edit = edit_event(file, &before, &source);
    second_edit["t"] = json!("2026-09-25T12:04:00Z");
    let receiver_events = jsonl(&[
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T12:01:00Z","k":"msg.in","content":format!("<engram-src id=\"{uuid_a}\"/>")}),
        edit_event(file, &before, &source),
        json!({"t":"2026-09-25T12:03:00Z","k":"msg.in","content":format!("<engram-src id=\"{uuid_b}\"/>")}),
        second_edit,
    ]);
    let receiver = write_grep_owner(
        temp.path(),
        "receiver",
        binary,
        &[(receiver_id, &receiver_events)],
    );
    ingest_owner_test_tape(
        temp.path(),
        "receiver",
        receiver_id,
        &receiver_events,
        &[
            DispatchLink {
                uuid: uuid_a.into(),
                first_turn_index: 0,
                direction: DispatchDirection::Received,
            },
            DispatchLink {
                uuid: uuid_b.into(),
                first_turn_index: 1,
                direction: DispatchDirection::Received,
            },
        ],
    );
    peers.insert("receiver".into(), receiver);

    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "querier-empty",
        "{\"t\":\"2026-09-25T09:00:00Z\",\"k\":\"note\",\"content\":\"no endpoints\"}\n",
    );
    std::fs::write(repo.join(file), &source).expect("write multiple-parent target");
    set_peer_topology(&caller_home, serde_json::Value::Object(peers));
    let output = run_peer_explain(
        binary,
        &caller_home,
        &repo,
        file,
        "task-alpha,task-beta,receiver",
    );
    assert!(
        output.status.success(),
        "multiple-parent query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    let session = value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["session_id"] == receiver_id)
        .expect("receiver session");
    assert_eq!(session["parent"], serde_json::Value::Null);
    let mut actual = session["parents"]
        .as_array()
        .expect("multiple parents")
        .iter()
        .map(|parent| parent.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    actual.sort();
    parent_ids.sort();
    assert_eq!(actual, parent_ids);
    assert_eq!(
        value["dispatch_lineage"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|hop| hop["session"] == receiver_id)
            .count(),
        2
    );
}

#[test]
fn explain_peers_terminates_mutual_remote_dispatch_cycle() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let uuid_a = "723e4567-e89b-12d3-a456-426614174060";
    let uuid_b = "723e4567-e89b-12d3-a456-426614174061";
    let file = "cycle.rs";
    let source =
        "fn remote_cycle_edit() { let value = first + second; use_value(value); }\n".repeat(12);
    let before = source.replace("remote_cycle_edit", "before_remote_cycle_edit");
    let a_id = "cycle-session-a";
    let mut a_edit = edit_event(file, &before, &source);
    a_edit["t"] = json!("2026-09-25T12:03:00Z");
    let a_events = jsonl(&[
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T12:01:00Z","k":"msg.in","content":format!("<engram-src id=\"{uuid_b}\"/>")}),
        json!({"t":"2026-09-25T12:02:00Z","k":"msg.out","content":format!("<engram-src id=\"{uuid_a}\"/>")}),
        a_edit,
    ]);
    let a = write_grep_owner(temp.path(), "cycle-a", binary, &[(a_id, &a_events)]);
    ingest_owner_test_tape(
        temp.path(),
        "cycle-a",
        a_id,
        &a_events,
        &[
            DispatchLink {
                uuid: uuid_b.into(),
                first_turn_index: 0,
                direction: DispatchDirection::Received,
            },
            DispatchLink {
                uuid: uuid_a.into(),
                first_turn_index: 1,
                direction: DispatchDirection::Sent,
            },
        ],
    );
    let b_id = "cycle-session-b";
    let b_events = jsonl(&[
        json!({"t":"2026-09-25T11:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T11:01:00Z","k":"msg.in","content":format!("<engram-src id=\"{uuid_a}\"/>")}),
        json!({"t":"2026-09-25T11:02:00Z","k":"msg.out","content":format!("<engram-src id=\"{uuid_b}\"/>")}),
    ]);
    let b = write_grep_owner(temp.path(), "cycle-b", binary, &[(b_id, &b_events)]);
    ingest_owner_test_tape(
        temp.path(),
        "cycle-b",
        b_id,
        &b_events,
        &[
            DispatchLink {
                uuid: uuid_a.into(),
                first_turn_index: 0,
                direction: DispatchDirection::Received,
            },
            DispatchLink {
                uuid: uuid_b.into(),
                first_turn_index: 1,
                direction: DispatchDirection::Sent,
            },
        ],
    );

    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "querier-empty",
        "{\"t\":\"2026-09-25T09:00:00Z\",\"k\":\"note\",\"content\":\"no endpoints\"}\n",
    );
    std::fs::write(repo.join(file), &source).expect("write cycle target");
    set_peer_topology(&caller_home, json!({"cycle-a":a, "cycle-b":b}));
    let output = run_peer_explain(binary, &caller_home, &repo, file, "cycle-a,cycle-b");
    assert!(
        output.status.success(),
        "mutual-dispatch query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    let chains = value["chains"].as_array().expect("chain metadata");
    assert_eq!(
        chains.len(),
        1,
        "expected one finite cyclic component: {value:#}"
    );
    assert_eq!(chains[0]["cycle"], true, "federated result: {value:#}");
    assert!(chains[0]["descendants"].as_array().unwrap().len() <= 2);
}

#[test]
fn explain_peers_qualifies_equal_span_anchors_but_joins_winnow_globally() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let source =
        "fn anchor_scope_fixture() { let value = north + south; use_value(value); }\n".repeat(12);
    let before = source.replace("anchor_scope_fixture", "before_anchor_scope_fixture");
    let root_tape = "span-scope-root-edit";
    let root_events = jsonl(&[
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test"}),
        edit_event("scope.rs", &before, &source),
    ]);
    let alpha = write_grep_owner(temp.path(), "alpha", binary, &[(root_tape, &root_events)]);
    ingest_owner_test_tape(temp.path(), "alpha", root_tape, &root_events, &[]);
    let alpha_db = temp.path().join("alpha-home/.engram/index.sqlite");
    let alpha_index = SqliteIndex::open_writer(alpha_db.to_str().expect("alpha DB path"))
        .expect("open alpha index");
    let root_window = engram::anchor::fingerprint_windows(&source)
        .into_iter()
        .next()
        .expect("root fingerprint window");
    let root_anchor = root_window.anchor.clone();
    let query_anchor = root_window
        .features
        .iter()
        .find(|feature| {
            alpha_index
                .matching_window_anchors(feature)
                .is_ok_and(|anchors| anchors == vec![root_anchor.clone()])
        })
        .expect("feature uniquely identifies the query root")
        .clone();
    let shared_span = "span:src/lib.rs:1-12";
    let shared_winnow = "winnow:shared-across-stores";
    insert_test_span_edge(&alpha_index, root_tape, 0, &root_anchor, shared_span);
    insert_test_span_edge(&alpha_index, root_tape, 1, &root_anchor, shared_winnow);
    insert_test_span_edge(
        &alpha_index,
        root_tape,
        2,
        shared_span,
        "winnow:span-child-alpha",
    );
    drop(alpha_index);

    let beta_tape = "span-scope-beta-note";
    let beta_events = jsonl(&[
        json!({"t":"2026-09-25T11:00:00Z","k":"meta","model":"peer-test"}),
        json!({"t":"2026-09-25T11:01:00Z","k":"note","content":"edges without a local query touch"}),
    ]);
    let beta = write_grep_owner(temp.path(), "beta", binary, &[(beta_tape, &beta_events)]);
    ingest_owner_test_tape(temp.path(), "beta", beta_tape, &beta_events, &[]);
    let beta_db = temp.path().join("beta-home/.engram/index.sqlite");
    let beta_index =
        SqliteIndex::open_writer(beta_db.to_str().expect("beta DB path")).expect("open beta index");
    insert_test_span_edge(
        &beta_index,
        beta_tape,
        0,
        shared_span,
        "winnow:must-not-cross-store-span",
    );
    insert_test_span_edge(
        &beta_index,
        beta_tape,
        1,
        shared_winnow,
        "winnow:cross-store-child-beta",
    );
    drop(beta_index);

    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "querier-empty",
        "{\"t\":\"2026-09-25T10:00:00Z\",\"k\":\"note\",\"content\":\"no indexed query anchor\"}\n",
    );
    set_peer_topology(&caller_home, json!({"alpha":alpha, "beta":beta}));
    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "explain",
            query_anchor.as_str(),
            "--anchor",
            "--peers",
            "alpha,beta",
            "--depth",
            "4",
        ])
        .output()
        .expect("run span qualification explain");
    assert!(
        output.status.success(),
        "span qualification query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert_eq!(value["federation"]["coverage"], "complete");
    let lineage = value["lineage"].as_array().expect("lineage");
    assert!(lineage.iter().any(|edge| {
        edge["from_anchor"] == root_anchor
            && edge["to_anchor"] == shared_span
            && edge["store"] == "alpha/default"
    }));
    assert!(lineage.iter().any(|edge| {
        edge["from_anchor"] == shared_span
            && edge["to_anchor"] == "winnow:span-child-alpha"
            && edge["store"] == "alpha/default"
    }));
    assert!(lineage.iter().any(|edge| {
        edge["from_anchor"] == shared_winnow
            && edge["to_anchor"] == "winnow:cross-store-child-beta"
            && edge["store"] == "beta/default"
    }));
    assert!(
        lineage
            .iter()
            .all(|edge| { edge["to_anchor"] != "winnow:must-not-cross-store-span" })
    );
}

#[test]
fn explain_peers_pipelines_negotiated_edge_chunks_and_keeps_semantic_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let source = (1..=24)
        .map(|line| format!("fn chunk_frontier_{line}() {{ stable_value_{line}(); }}\n"))
        .collect::<String>();
    let before = source.replace("chunk_frontier", "before_chunk_frontier");
    let tape_id = "large-frontier-tape";
    let events = [
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"peer-test"}),
        json!({
            "t":"2026-09-25T12:01:00Z",
            "k":"code.edit",
            "file":"frontier.rs",
            "before_range":[1,24],
            "after_range":[1,24],
            "before_text":before,
            "after_text":source,
        }),
    ]
    .iter()
    .map(serde_json::Value::to_string)
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    let mut peer = write_grep_owner(temp.path(), "remote-owner", binary, &[(tape_id, &events)]);
    let peer_template = peer.clone();
    let owner_home = temp.path().join("remote-owner-home");
    let owner_engram = owner_home.join(".engram");
    let owner_db = owner_engram.join("index.sqlite");
    let owner_tapes = owner_engram.join("tapes");
    std::fs::write(
        owner_engram.join("topology.yml"),
        format!(
            "version: 1\nself: remote-owner\nexports:\n  default:\n    db: {}\n    tape_dirs:\n      - {}\nlimits:\n  items_per_batch: 37\n",
            owner_db.display(),
            owner_tapes.display()
        ),
    )
    .expect("write lower-cap owner topology");

    let index = SqliteIndex::open_writer(owner_db.to_str().expect("owner DB path"))
        .expect("open owner index");
    let parsed = engram::tape::event::parse_jsonl_events(&events).expect("parse edit tape");
    index
        .ingest_tape_events(
            tape_id,
            &parsed,
            engram::index::lineage::LINK_THRESHOLD_DEFAULT,
        )
        .expect("index remote edit");
    let root_window = engram::anchor::fingerprint_windows(&source)
        .into_iter()
        .next()
        .expect("source anchor window");
    let root_anchor = root_window.anchor.clone();
    let query_anchor = root_window
        .features
        .iter()
        .find(|feature| {
            index
                .matching_window_anchors(feature)
                .is_ok_and(|anchors| anchors == vec![root_anchor.clone()])
        })
        .expect("feature uniquely identifies the root window")
        .clone();
    for child in (0_u32..140).rev() {
        index
            .insert_edge(
                &engram::index::EdgeSource {
                    source_kind: engram::index::EdgeSourceKind::SpanLink,
                    tape_id: tape_id.into(),
                    event_offset: 1,
                    pair_ordinal: child,
                    from_window_ordinal: 0,
                    to_window_ordinal: i64::from(child),
                },
                &engram::index::lineage::SpanEdge {
                    from_anchor: root_anchor.clone(),
                    to_anchor: format!("span:child-{child:03}"),
                    confidence: 0.95,
                    location_delta: engram::index::lineage::LocationDelta::Adjacent,
                    cardinality: engram::index::lineage::Cardinality::OneToMany,
                    agent_link: false,
                    note: Some("synthetic frontier edge".into()),
                },
            )
            .expect("insert frontier edge");
    }
    drop(index);

    let request_log = log_peer_requests_with_chunk_barrier(
        temp.path(),
        "remote-owner",
        binary,
        &mut peer,
        4,
        None,
    );
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "empty-caller-tape",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"note\",\"content\":\"empty\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({"version":1,"self":"caller","peers":{"remote-owner":peer}}))
            .expect("serialize caller topology"),
    )
    .expect("write caller topology");
    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "explain",
            query_anchor.as_str(),
            "--anchor",
            "--peers",
            "remote-owner",
            "--max-fanout",
            "200",
            "--max-edges",
            "200",
            "--depth",
            "2",
        ])
        .output()
        .expect("run chunked federated explain");
    assert!(
        output.status.success(),
        "chunked explain failed: {}; peer requests: {}",
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(&request_log).unwrap_or_default()
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert_eq!(value["federation"]["coverage"], "complete");
    let children = value["lineage"]
        .as_array()
        .expect("lineage")
        .iter()
        .filter_map(|edge| edge["to_anchor"].as_str())
        .filter(|anchor| anchor.starts_with("span:child-"))
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 140, "all children must cross the cap");
    let mut sorted_children = children.clone();
    sorted_children.sort_unstable();
    assert_eq!(
        children, sorted_children,
        "semantic order must ignore reversed replies"
    );

    let requests = std::fs::read_to_string(&request_log)
        .expect("read peer request trace")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("request JSON"))
        .filter(|request| request["op"] == "lookup_edges")
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        5,
        "one root request plus four pipelined chunks"
    );
    let sizes = requests
        .iter()
        .map(|request| {
            request["args"]["nodes"]
                .as_array()
                .expect("node chunk")
                .len()
        })
        .collect::<Vec<_>>();
    assert_eq!(sizes[0], 1, "first logical level has its one root");
    let mut chunk_sizes = sizes[1..].to_vec();
    chunk_sizes.sort_unstable();
    assert_eq!(chunk_sizes, vec![29, 37, 37, 37]);
    assert!(sizes.iter().all(|size| *size <= 37));
    assert_eq!(sizes[1..].iter().sum::<usize>(), 140);

    let capped_proxy_root = temp.path().join("capped-proxy");
    std::fs::create_dir_all(&capped_proxy_root).expect("create capped proxy root");
    let mut capped_peer = peer_template.clone();
    let capped_operation_log =
        log_peer_operations(&capped_proxy_root, "remote-owner", binary, &mut capped_peer);
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(
            &json!({"version":1,"self":"caller","peers":{"remote-owner":capped_peer}}),
        )
        .expect("serialize capped caller topology"),
    )
    .expect("write capped caller topology");
    let capped_output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "explain",
            query_anchor.as_str(),
            "--anchor",
            "--peers",
            "remote-owner",
            "--max-fanout",
            "200",
            "--max-edges",
            "50",
            "--depth",
            "2",
        ])
        .output()
        .expect("run globally capped federated explain");
    assert!(
        capped_output.status.success(),
        "capped explain failed: {}; peer requests: {}",
        String::from_utf8_lossy(&capped_output.stderr),
        std::fs::read_to_string(&capped_operation_log).unwrap_or_default()
    );
    let capped_value: serde_json::Value =
        serde_json::from_slice(&capped_output.stdout).expect("capped explain JSON");
    assert_eq!(
        capped_value["lineage"]
            .as_array()
            .expect("capped lineage")
            .len(),
        50,
        "the traversal edge budget must apply across all chunks"
    );

    let failed_proxy_root = temp.path().join("failed-proxy");
    std::fs::create_dir_all(&failed_proxy_root).expect("create failed proxy root");
    let mut failed_peer = peer_template;
    let failed_request_log = log_peer_requests_with_chunk_barrier(
        &failed_proxy_root,
        "remote-owner",
        binary,
        &mut failed_peer,
        4,
        Some(2),
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(
            &json!({"version":1,"self":"caller","peers":{"remote-owner":failed_peer}}),
        )
        .expect("serialize failed caller topology"),
    )
    .expect("write failed caller topology");
    let failed_output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "explain",
            query_anchor.as_str(),
            "--anchor",
            "--peers",
            "remote-owner",
            "--max-fanout",
            "200",
            "--max-edges",
            "200",
            "--depth",
            "2",
        ])
        .output()
        .expect("run explain with failed edge chunk");
    assert!(
        failed_output.status.success(),
        "partial explain should preserve successful evidence: {}",
        String::from_utf8_lossy(&failed_output.stderr)
    );
    let failed_value: serde_json::Value =
        serde_json::from_slice(&failed_output.stdout).expect("partial explain JSON");
    assert_eq!(failed_value["federation"]["coverage"], "partial");
    assert_eq!(
        failed_value["lineage"]
            .as_array()
            .expect("partial lineage")
            .iter()
            .filter(|edge| edge["to_anchor"]
                .as_str()
                .is_some_and(|anchor| anchor.starts_with("span:child-")))
            .count(),
        140,
        "completed root-level edges remain attributable when a later chunk fails"
    );
    let failed_requests = std::fs::read_to_string(failed_request_log)
        .expect("read failed chunk request trace")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("failed request JSON"))
        .filter(|request| request["op"] == "lookup_edges")
        .count();
    assert_eq!(
        failed_requests, 5,
        "all chunks in the failed logical level were sent"
    );
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
    let wal = db.with_extension("sqlite-wal");
    let shm = db.with_extension("sqlite-shm");
    for sidecar in [&wal, &shm] {
        if sidecar.exists() {
            std::fs::remove_file(sidecar).expect("clear closed fixture sidecar");
        }
    }
    assert!(!wal.exists(), "snapshot fixture starts without a WAL");
    assert!(
        !shm.exists(),
        "snapshot fixture starts without shared memory"
    );

    let tape_id = "fixture-1";
    let tape = tapes.join(format!("{tape_id}.jsonl.zst"));
    std::fs::write(&tape, b"compressed fixture placeholder").expect("tape fixture");
    let large_tape = tapes.join("large-fixture.jsonl.zst");
    let large_file_bytes = 32 * 1024 * 1024;
    std::fs::write(&large_tape, vec![b'x'; large_file_bytes]).expect("large read fixture");
    std::fs::write(
        engram_home.join("topology.yml"),
        format!(
            "version: 1\nself: emulated-owner\nexports:\n  default:\n    db: {}\n    tape_dirs:\n      - {}\nlimits:\n  request_timeout_ms: 1234\n  owner_session_max_secs: 60\n  owner_idle_timeout_secs: 7\n",
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
    let large_tape_path = large_tape.to_str().expect("UTF-8 large tape path");
    let large_read_request = || {
        PeerRequest::new(
            "read_file",
            vec!["default".into()],
            json!({
                "address":{"machine":"emulated-owner","path":large_tape_path,"kind":"tape"},
                "max_bytes":large_file_bytes,
            }),
        )
    };
    assert!(owner.exports["default"].is_ok());
    assert_eq!(owner.limits.get("request_timeout_ms"), Some(&1_234));
    assert_eq!(owner.limits.get("owner_session_max_secs"), Some(&60));
    assert_eq!(owner.limits.get("owner_idle_timeout_secs"), Some(&7));

    let writer = SqliteIndex::open_writer(db.to_str().expect("UTF-8 DB path"))
        .expect("open concurrent writer after owner snapshot");
    writer
        .insert_dispatch_link(
            tape_id,
            &DispatchLink {
                uuid: "visible-after-owner-exit".into(),
                first_turn_index: 9,
                direction: DispatchDirection::Sent,
            },
        )
        .expect("commit concurrent row while peer session stays open");
    drop(writer);
    let pinned_rows = owner
        .round(
            &[PeerRequest::new(
                "dispatch_rows",
                vec!["default".into()],
                json!({"by_tape":[tape_id]}),
            )],
            Duration::from_secs(5),
        )
        .pop()
        .expect("dispatch rows outcome")
        .expect("read from pinned owner snapshot");
    assert!(
        pinned_rows.data.is_empty(),
        "an open owner query must retain the pre-writer snapshot"
    );

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
    drop(owner);

    let checkpoint = rusqlite::Connection::open(&db)
        .expect("open checkpoint connection after peer exit")
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .expect("checkpoint after caller drops remote owner");
    assert_eq!(
        checkpoint.0, 0,
        "caller cancellation/exit must release the owner's read transaction"
    );
    let fresh = SqliteIndex::open_reader(db.to_str().expect("UTF-8 DB path"))
        .expect("fresh snapshot after peer exit");
    assert_eq!(
        fresh
            .dispatch_links_for_uuid("visible-after-owner-exit")
            .expect("fresh dispatch rows")[0]
            .uuid,
        "visible-after-owner-exit"
    );
    drop(fresh);

    let mut deadline_owner =
        RemoteOwner::connect("emulated-owner", "caller", &peer, Duration::from_secs(5))
            .expect("real peer handshake before request deadline");
    let writer = SqliteIndex::open_writer(db.to_str().expect("UTF-8 DB path"))
        .expect("open writer during deadline-bound owner session");
    writer
        .insert_dispatch_link(
            tape_id,
            &DispatchLink {
                uuid: "visible-after-deadline".into(),
                first_turn_index: 10,
                direction: DispatchDirection::Sent,
            },
        )
        .expect("commit row behind deadline-bound reader");
    drop(writer);
    let pinned_rows = deadline_owner
        .round(
            &[PeerRequest::new(
                "dispatch_rows",
                vec!["default".into()],
                json!({"by_tape":[tape_id]}),
            )],
            Duration::from_secs(5),
        )
        .pop()
        .expect("deadline snapshot outcome")
        .expect("deadline snapshot query");
    assert!(
        pinned_rows
            .data
            .iter()
            .all(|row| row["uuid"] != "visible-after-deadline"),
        "deadline-bound owner must retain the pre-writer snapshot"
    );
    let started = Instant::now();
    let deadline_error = deadline_owner
        .round(&[large_read_request()], Duration::from_millis(1))
        .pop()
        .expect("deadline outcome")
        .expect_err("in-flight read_file deadline must abort the owner session");
    assert_eq!(deadline_error.code, "timeout");
    drop(deadline_owner);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "deadline did not release the owner within the session bound"
    );
    assert_owner_transaction_released(&db, "visible-after-deadline");

    let mut cancelled_owner =
        RemoteOwner::connect("emulated-owner", "caller", &peer, Duration::from_secs(5))
            .expect("real peer handshake before caller cancellation");
    let writer = SqliteIndex::open_writer(db.to_str().expect("UTF-8 DB path"))
        .expect("open writer during cancellable owner session");
    writer
        .insert_dispatch_link(
            tape_id,
            &DispatchLink {
                uuid: "visible-after-cancel".into(),
                first_turn_index: 11,
                direction: DispatchDirection::Sent,
            },
        )
        .expect("commit row behind cancellable reader");
    drop(writer);
    let pinned_rows = cancelled_owner
        .round(
            &[PeerRequest::new(
                "dispatch_rows",
                vec!["default".into()],
                json!({"by_tape":[tape_id]}),
            )],
            Duration::from_secs(5),
        )
        .pop()
        .expect("cancellation snapshot outcome")
        .expect("cancellation snapshot query");
    assert!(
        pinned_rows
            .data
            .iter()
            .all(|row| row["uuid"] != "visible-after-cancel"),
        "cancellable owner must retain the pre-writer snapshot"
    );
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let request_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let caller_cancel = std::sync::Arc::clone(&cancelled);
    let signal_request_started = std::sync::Arc::clone(&request_started);
    let cancellation_request = large_read_request();
    let started = Instant::now();
    let cancel_waiter = std::thread::spawn(move || {
        signal_request_started.store(true, std::sync::atomic::Ordering::SeqCst);
        let result = cancelled_owner
            .round_cancellable(
                &[cancellation_request],
                Duration::from_secs(5),
                &caller_cancel,
            )
            .pop()
            .expect("cancellation outcome");
        drop(cancelled_owner);
        result
    });
    let signal_deadline = Instant::now() + Duration::from_secs(3);
    while !request_started.load(std::sync::atomic::Ordering::SeqCst)
        && Instant::now() < signal_deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(request_started.load(std::sync::atomic::Ordering::SeqCst));
    // The large response remains in flight while the caller cancellation arrives.
    std::thread::sleep(Duration::from_millis(20));
    cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    let cancellation_error = cancel_waiter
        .join()
        .expect("cancelled owner request thread")
        .expect_err("caller cancellation must abort the in-flight owner session");
    assert_eq!(cancellation_error.code, "cancelled");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "caller cancellation did not release the owner within the session bound"
    );
    assert_owner_transaction_released(&db, "visible-after-cancel");
}

fn assert_owner_transaction_released(db: &std::path::Path, uuid: &str) {
    let checkpoint = rusqlite::Connection::open(db)
        .expect("open checkpoint connection after owner abort")
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .expect("checkpoint after owner abort");
    assert_eq!(checkpoint.0, 0, "owner abort must release its WAL reader");
    let fresh = SqliteIndex::open_reader(db.to_str().expect("UTF-8 DB path"))
        .expect("fresh snapshot after owner abort");
    assert!(
        fresh
            .dispatch_links_for_uuid(uuid)
            .expect("fresh dispatch rows")
            .iter()
            .any(|link| link.uuid == uuid),
        "fresh read after owner abort must see {uuid}"
    );
}

#[test]
fn peer_open_negotiation_types_each_incompatible_peer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cases = [
        (
            "unknown-subcommand",
            json!({
                "id": 1,
                "end": true,
                "ok": false,
                "error": {"code": "unknown_operation", "message": "unknown subcommand"}
            })
            .to_string(),
            "incompatible",
        ),
        (
            "protocol-mismatch",
            json!({
                "id": 1,
                "end": true,
                "ok": true,
                "stats": {
                    "self": "remote", "build": "fixture", "protocol": 2,
                    "schema": SCHEMA_VERSION, "query_semantics": QUERY_SEMANTICS_VERSION,
                    "limits": {}
                }
            })
            .to_string(),
            "incompatible",
        ),
        (
            "schema-v3",
            json!({
                "id": 1,
                "end": true,
                "ok": true,
                "stats": {
                    "self": "remote", "build": "fixture", "protocol": 1,
                    "schema": 3, "query_semantics": QUERY_SEMANTICS_VERSION,
                    "limits": {}
                }
            })
            .to_string(),
            "incompatible",
        ),
        (
            "semantics-mismatch",
            json!({
                "id": 1,
                "end": true,
                "ok": true,
                "stats": {
                    "self": "remote", "build": "fixture", "protocol": 1,
                    "schema": SCHEMA_VERSION, "query_semantics": QUERY_SEMANTICS_VERSION + 1,
                    "limits": {}
                }
            })
            .to_string(),
            "incompatible_semantics",
        ),
        (
            "label-mismatch",
            json!({
                "id": 1,
                "end": true,
                "ok": true,
                "stats": {
                    "self": "unexpected", "build": "fixture", "protocol": 1,
                    "schema": SCHEMA_VERSION, "query_semantics": QUERY_SEMANTICS_VERSION,
                    "limits": {}
                }
            })
            .to_string(),
            "label_mismatch",
        ),
    ];

    for (name, frame, expected_code) in cases {
        let peer = one_shot_handshake_peer(temp.path(), name, &frame);
        let error = match RemoteOwner::connect("remote", "caller", &peer, Duration::from_secs(1)) {
            Ok(_) => panic!("case {name} unexpectedly opened"),
            Err(error) => error,
        };
        assert_eq!(error.code, expected_code, "case {name}: {error:?}");
    }
}

#[test]
fn peer_serve_exits_on_owner_idle_timeout_with_stdin_still_open() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut child = spawn_peer_serve_with_limits(temp.path(), 5, 1);
    let started = Instant::now();
    let status = wait_for_peer_exit(&mut child, Duration::from_secs(3));
    let elapsed = started.elapsed();

    assert_eq!(status.code(), Some(0));
    assert!(elapsed >= Duration::from_millis(900));
    assert!(elapsed < Duration::from_secs(2));
}

#[test]
fn peer_serve_enforces_owner_session_max_during_continuous_requests() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut child = spawn_peer_serve_with_limits(temp.path(), 1, 5);
    let mut input = child.stdin.take().expect("peer stdin");
    let started = Instant::now();
    let mut request_id = 1u64;

    let status = loop {
        let request = json!({
            "v": 1,
            "id": request_id,
            "op": "open",
            "stores": [],
            "args": {},
        });
        request_id += 1;
        let _ = writeln!(input, "{request}");
        let _ = input.flush();
        if let Some(status) = child.try_wait().expect("poll peer process") {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("peer process exceeded its owner session maximum");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let elapsed = started.elapsed();

    assert_eq!(status.code(), Some(0));
    assert!(elapsed >= Duration::from_millis(900));
    assert!(elapsed < Duration::from_secs(2));
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
    let mut peer = json!({
        "command": peer_command,
        "engram": binary,
        "exports": ["default"],
    });
    let operation_log = log_peer_operations(temp.path(), "emulated-owner", binary, &mut peer);
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "self": "caller",
            "peers": {"emulated-owner": peer}
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
    assert_eq!(operation_count(&operation_log, "locate_tapes"), 1);
    assert_eq!(operation_count(&operation_log, "read_file"), 1);
    assert_eq!(operation_count(&operation_log, "tape_facts"), 0);
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
    assert_eq!(operation_count(&operation_log, "peek_lines"), 1);
    assert_eq!(operation_count(&operation_log, "dispatch_rows"), 0);

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
    assert_eq!(operation_count(&operation_log, "dispatch_rows"), 1);
    assert_eq!(operation_count(&operation_log, "peek_lines"), 2);

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
    assert_eq!(operation_count(&operation_log, "peek_lines"), 3);

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
fn show_with_selected_peers_reads_and_deduplicates_matching_remote_tapes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let content = concat!(
        "{\"t\":\"2026-09-25T11:59:00Z\",\"k\":\"meta\",\"model\":\"peer-test\"}\n",
        "{\"t\":\"2026-09-25T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"show from selected peers\"}\n",
    );
    let tape_id = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
    let mut alpha = write_grep_owner(temp.path(), "alpha", binary, &[(tape_id.as_str(), content)]);
    let alpha_operations =
        log_show_first_round_with_identity_barrier(temp.path(), "alpha", binary, &mut alpha);
    let mut beta = write_grep_owner(temp.path(), "beta", binary, &[(tape_id.as_str(), content)]);
    let beta_operations =
        log_show_first_round_with_identity_barrier(temp.path(), "beta", binary, &mut beta);
    let unselected_marker = temp.path().join("show-unselected-peer-was-started");
    let unselected = json!({
        "command": ["/usr/bin/touch", unselected_marker],
        "engram": "/unused/engram",
        "exports": ["default"],
    });
    let peers = json!({
        "alpha": alpha,
        "beta": beta,
        "not-selected": unselected,
    });
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-other-tape",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"note\",\"content\":\"local tape\"}\n",
    );
    let caller_engram = caller_home.join(".engram");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": peers,
            "limits": {"total_query_deadline_ms": 1_500}
        }))
        .expect("serialize topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["show", &tape_id, "--peers", "beta,alpha"])
        .output()
        .expect("run selected-peer show");
    assert!(
        output.status.success(),
        "selected-peer show failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("show JSON");
    assert_eq!(value["tape_id"], tape_id);
    assert!(value["path"].is_null());
    assert_eq!(value["location"]["machine"], "beta");
    assert_eq!(value["location"]["store"], "beta/default");
    assert_eq!(value["digest"], tape_id);
    assert_eq!(value["id_verified"], true);
    assert_eq!(
        value["locations"].as_array().unwrap().len(),
        2,
        "selected-peer show response: {value:#}"
    );
    assert_eq!(value["federation"]["coverage"], "complete");
    assert_eq!(operation_count(&alpha_operations, "tape_facts"), 1);
    assert_eq!(operation_count(&beta_operations, "tape_facts"), 1);
    assert_eq!(operation_count(&alpha_operations, "locate_tapes"), 1);
    assert_eq!(operation_count(&beta_operations, "locate_tapes"), 1);
    assert_eq!(operation_count(&alpha_operations, "read_file"), 0);
    assert_eq!(operation_count(&beta_operations, "read_file"), 1);
    assert!(
        value["federation"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["store"] == "not-selected/default"
                && source["status"] == "not_selected")
    );
    assert!(!unselected_marker.exists(), "unselected peer was launched");
}

#[test]
fn show_keeps_holder_digest_when_unrelated_tape_facts_metadata_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let tape_id = "fingerprint-without-meta";
    let content =
        "{\"t\":\"2026-09-25T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"no metadata row\"}\n";
    let mut alpha = write_grep_owner(temp.path(), "alpha", binary, &[(tape_id, content)]);
    let alpha_operations = log_peer_operations(temp.path(), "alpha", binary, &mut alpha);
    let mut beta = write_grep_owner(temp.path(), "beta", binary, &[(tape_id, content)]);
    let beta_operations = log_peer_operations(temp.path(), "beta", binary, &mut beta);
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-other-tape",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"note\",\"content\":\"local tape\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {"alpha": alpha, "beta": beta},
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["show", tape_id, "--peers", "alpha,beta"])
        .output()
        .expect("run show with metadata-invalid duplicate holders");
    assert!(
        output.status.success(),
        "show should use the current tape digest despite unrelated metadata failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("show JSON");
    assert_eq!(value["location"]["machine"], "alpha");
    assert_eq!(value["locations"].as_array().unwrap().len(), 2);
    assert_eq!(value["federation"]["coverage"], "complete");
    assert_eq!(operation_count(&alpha_operations, "read_file"), 1);
    assert_eq!(operation_count(&beta_operations, "read_file"), 0);
}

#[test]
fn show_keeps_holder_digest_when_predecessor_chain_metadata_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let tape_id = "fingerprint-with-bad-predecessor";
    let previous_id = "previous-segment";
    let current = concat!(
        "{\"t\":\"2026-09-25T11:59:00Z\",\"k\":\"meta\",\"ingest_continuation\":{\"previous_tape_id\":\"previous-segment\"}}\n",
        "{\"t\":\"2026-09-25T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"current tape\"}\n",
    );
    let bad_previous =
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"msg.in\",\"content\":\"no meta\"}\n";
    let good_previous = concat!(
        "{\"k\":\"meta\"}\n",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"msg.in\",\"content\":\"prior tape\"}\n",
    );
    let mut alpha = write_grep_owner(
        temp.path(),
        "alpha",
        binary,
        &[(tape_id, current), (previous_id, bad_previous)],
    );
    let alpha_operations = log_peer_operations(temp.path(), "alpha", binary, &mut alpha);
    let mut beta = write_grep_owner(
        temp.path(),
        "beta",
        binary,
        &[(tape_id, current), (previous_id, good_previous)],
    );
    let beta_operations = log_peer_operations(temp.path(), "beta", binary, &mut beta);
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-other-tape",
        "{\"t\":\"2026-09-25T10:00:00Z\",\"k\":\"note\",\"content\":\"local tape\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {"alpha": alpha, "beta": beta},
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["show", tape_id, "--peers", "alpha,beta"])
        .output()
        .expect("run show with broken predecessor holder");
    assert!(
        output.status.success(),
        "show should retain a current tape digest when its predecessor metadata fails: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("show JSON");
    assert_eq!(value["location"]["machine"], "alpha");
    assert_eq!(value["locations"].as_array().unwrap().len(), 2);
    assert_eq!(value["federation"]["coverage"], "complete");
    assert_eq!(operation_count(&alpha_operations, "read_file"), 1);
    assert_eq!(operation_count(&beta_operations, "read_file"), 0);
}

#[test]
fn show_with_local_and_remote_holders_keeps_local_choice_and_only_reads_digests_remotely() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let content = concat!(
        "{\"t\":\"2026-09-25T11:59:00Z\",\"k\":\"meta\",\"model\":\"peer-test\"}\n",
        "{\"t\":\"2026-09-25T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"local show holder\"}\n",
    );
    let tape_id = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
    let mut alpha = write_grep_owner(temp.path(), "alpha", binary, &[(tape_id.as_str(), content)]);
    let alpha_operations = log_peer_operations(temp.path(), "alpha", binary, &mut alpha);
    let mut beta = write_grep_owner(temp.path(), "beta", binary, &[(tape_id.as_str(), content)]);
    let beta_operations = log_peer_operations(temp.path(), "beta", binary, &mut beta);
    let (caller_home, repo) = write_local_grep_source(temp.path(), &tape_id, content);
    let caller_engram = caller_home.join(".engram");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {"alpha": alpha, "beta": beta},
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["show", &tape_id, "--peers", "beta,alpha"])
        .output()
        .expect("run local-first selected-peer show");
    assert!(
        output.status.success(),
        "local-first show failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("show JSON");
    let local_path = repo
        .join(".engram/tapes")
        .join(format!("{tape_id}.jsonl.zst"));
    assert_eq!(value["location"]["machine"], "caller");
    assert_eq!(value["path"], local_path.to_str().unwrap());
    assert_eq!(
        value["locations"].as_array().unwrap().len(),
        3,
        "local-holder show response: {value:#}"
    );
    assert_eq!(value["digest"], tape_id);
    assert_eq!(value["federation"]["coverage"], "complete");
    for operations in [&alpha_operations, &beta_operations] {
        assert_eq!(operation_count(operations, "tape_facts"), 1);
        assert_eq!(operation_count(operations, "read_file"), 0);
    }
}

#[test]
fn show_detects_multi_holder_fingerprint_conflict_from_digests_without_read_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let tape_id = "fingerprint-show-conflict";
    let alpha_content = concat!(
        "{\"t\":\"2026-09-25T11:59:00Z\",\"k\":\"meta\",\"model\":\"peer-test\"}\n",
        "{\"t\":\"2026-09-25T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"alpha bytes\"}\n",
    );
    let beta_content = concat!(
        "{\"t\":\"2026-09-25T11:59:00Z\",\"k\":\"meta\",\"model\":\"peer-test\"}\n",
        "{\"t\":\"2026-09-25T12:01:00Z\",\"k\":\"msg.in\",\"content\":\"beta bytes\"}\n",
    );
    let mut alpha = write_grep_owner(temp.path(), "alpha", binary, &[(tape_id, alpha_content)]);
    let alpha_operations = log_peer_operations(temp.path(), "alpha", binary, &mut alpha);
    let mut beta = write_grep_owner(temp.path(), "beta", binary, &[(tape_id, beta_content)]);
    let beta_operations = log_peer_operations(temp.path(), "beta", binary, &mut beta);
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-other-tape",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"note\",\"content\":\"local tape\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {"alpha": alpha, "beta": beta},
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["show", tape_id, "--peers", "beta,alpha"])
        .output()
        .expect("run conflicting multi-holder show");
    assert!(
        !output.status.success(),
        "expected identity conflict; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let error: serde_json::Value = serde_json::from_str(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .last()
            .expect("identity conflict error"),
    )
    .expect("structured show error");
    assert_eq!(error["error"]["code"], "identity_conflict");
    for operations in [&alpha_operations, &beta_operations] {
        assert_eq!(operation_count(operations, "tape_facts"), 1);
        assert_eq!(operation_count(operations, "read_file"), 0);
    }
}

#[test]
fn show_with_selected_peers_keeps_tape_and_reports_partial_unavailability() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let content = "{\"t\":\"2026-09-25T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"show partial selected peers\"}\n";
    let tape_id = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
    let available = write_grep_owner(
        temp.path(),
        "available",
        binary,
        &[(tape_id.as_str(), content)],
    );
    let offline = json!({
        "command": ["/usr/bin/false"],
        "engram": "/unused/engram",
        "exports": ["default"],
    });
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-other-tape",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"note\",\"content\":\"local tape\"}\n",
    );
    let caller_engram = caller_home.join(".engram");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {"available": available, "offline": offline},
        }))
        .expect("serialize topology"),
    )
    .expect("write caller topology");

    let partial = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["show", &tape_id, "--peers", "available,offline"])
        .output()
        .expect("run partial selected-peer show");
    assert!(
        partial.status.success(),
        "partial show should keep the completed tape: {}",
        String::from_utf8_lossy(&partial.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&partial.stdout).expect("show JSON");
    assert_eq!(value["tape_id"], tape_id);
    assert_eq!(value["federation"]["coverage"], "partial");
    assert!(value["federation"]["sources"].as_array().unwrap().iter().any(
        |source| source["store"] == "offline/default" && source["status"] == "unavailable"
    ));

    let required = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "show",
            &tape_id,
            "--peers",
            "available,offline",
            "--require-complete",
        ])
        .output()
        .expect("run require-complete selected-peer show");
    assert!(!required.status.success());
    let stderr = String::from_utf8_lossy(&required.stderr);
    let error: serde_json::Value = serde_json::from_str(
        stderr
            .lines()
            .last()
            .expect("incomplete coverage error line"),
    )
    .expect("error JSON");
    assert_eq!(error["error"]["code"], "incomplete_coverage");
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
    let local_conflict = "{\"t\":\"2026-09-24T10:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-federated conflict\"}\n";
    let collision_id = format!("{:x}", sha2::Sha256::digest(local_conflict.as_bytes()));
    let compressed = zstd::stream::encode_all(conflicting_content.as_bytes(), 0)
        .expect("compress conflicting tape");
    std::fs::write(
        owner_tapes.join(format!("{collision_id}.jsonl.zst")),
        compressed,
    )
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
    let compressed = zstd::stream::encode_all(local_conflict.as_bytes(), 0)
        .expect("compress local conflicting tape");
    std::fs::write(
        local_tapes.join(format!("{collision_id}.jsonl.zst")),
        compressed,
    )
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
            .any(|session| { session["session_id"] == format!("{collision_id}@caller/local:0") })
    );
    assert!(sessions.iter().any(|session| {
        session["session_id"] == format!("{collision_id}@emulated-owner/default")
    }));
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
        .args([
            "grep",
            "needle-multi",
            "--peers",
            "beta, alpha",
            "--limit",
            "4",
        ])
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
    assert_eq!(result["returned"], 4);
    assert_eq!(
        sessions
            .iter()
            .map(|session| session["tape_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["beta-tape", "alpha-tape", "shared-tape", "caller-tape"]
    );
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

    let end_page = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-multi",
            "--peers",
            "beta, alpha",
            "--offset",
            "2",
            "--limit",
            "4",
        ])
        .output()
        .expect("run short complete end page");
    assert!(
        end_page.status.success(),
        "short end page failed: {}",
        String::from_utf8_lossy(&end_page.stderr)
    );
    let end_result: serde_json::Value =
        serde_json::from_slice(&end_page.stdout).expect("short end-page JSON");
    assert_eq!(end_result["returned"], 2);
    assert_eq!(end_result["total"], 4);
    assert_eq!(end_result["truncated"], false);
    assert_eq!(end_result["time_range"], result["time_range"]);
    assert_eq!(
        end_result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["tape_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["shared-tape", "caller-tape"]
    );

    let counted_end_page = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-multi",
            "--peers",
            "beta, alpha",
            "--offset",
            "2",
            "--limit",
            "4",
            "--count",
        ])
        .output()
        .expect("run counted complete end page with duplicate holders");
    assert!(
        counted_end_page.status.success(),
        "counted end page failed: {}",
        String::from_utf8_lossy(&counted_end_page.stderr)
    );
    let counted_end_result: serde_json::Value =
        serde_json::from_slice(&counted_end_page.stdout).expect("counted end-page JSON");
    assert!(
        counted_end_result["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(counted_end_result["returned"], 2);
    assert_eq!(counted_end_result["total"], 4);
    assert_eq!(counted_end_result["truncated"], false);
    assert_eq!(counted_end_result["time_range"], result["time_range"]);

    let bounded = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-multi",
            "--peers",
            "beta, alpha",
            "--limit",
            "1",
        ])
        .output()
        .expect("run bounded overlapping peer grep");
    assert!(
        bounded.status.success(),
        "bounded overlapping grep failed: {}",
        String::from_utf8_lossy(&bounded.stderr)
    );
    let bounded_result: serde_json::Value =
        serde_json::from_slice(&bounded.stdout).expect("bounded grep JSON");
    assert_eq!(bounded_result["federation"]["coverage"], "complete");
    assert_eq!(bounded_result["returned"], 1);
    assert_eq!(bounded_result["total"], serde_json::Value::Null);
    assert_eq!(bounded_result["total_bounds"]["min"], 3);
    assert_eq!(bounded_result["total_bounds"]["max"], 5);
    let reference_total = result["total"].as_u64().expect("exact reference total");
    assert!(
        bounded_result["total_bounds"]["min"]
            .as_u64()
            .expect("bounded minimum")
            <= reference_total
    );
    assert!(
        bounded_result["total_bounds"]["max"]
            .as_u64()
            .expect("bounded maximum")
            >= reference_total
    );
    assert_eq!(bounded_result["truncated"], true);
    assert_eq!(
        bounded_result["time_range"]["start"],
        "2026-09-24T11:00:00Z"
    );
    assert_eq!(bounded_result["time_range"]["end"], "2026-09-24T13:00:00Z");

    let bounded_count = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-multi",
            "--peers",
            "beta, alpha",
            "--count",
            "--limit",
            "1",
        ])
        .output()
        .expect("run bounded overlapping peer count");
    assert!(
        bounded_count.status.success(),
        "bounded count grep failed: {}",
        String::from_utf8_lossy(&bounded_count.stderr)
    );
    let bounded_count_result: serde_json::Value =
        serde_json::from_slice(&bounded_count.stdout).expect("bounded count JSON");
    assert!(
        bounded_count_result["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(bounded_count_result["returned"], 1);
    assert_eq!(bounded_count_result["total"], serde_json::Value::Null);
    assert_eq!(bounded_count_result["total_bounds"]["min"], 3);
    assert_eq!(bounded_count_result["total_bounds"]["max"], 5);
    assert!(
        bounded_count_result["total_bounds"]["min"]
            .as_u64()
            .expect("bounded count minimum")
            <= reference_total
    );
    assert!(
        bounded_count_result["total_bounds"]["max"]
            .as_u64()
            .expect("bounded count maximum")
            >= reference_total
    );
    assert_eq!(bounded_count_result["truncated"], true);
    assert_eq!(
        bounded_count_result["time_range"],
        bounded_result["time_range"]
    );
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
    include_completed_peer_match: bool,
) -> (
    std::process::Child,
    std::path::PathBuf,
    Option<std::path::PathBuf>,
) {
    let binary = env!("CARGO_BIN_EXE_engram");
    let script_path = temp.path().join("blocking-peer.sh");
    let operation_started = temp.path().join("peer-operation-started");
    let followup_request = temp.path().join("unfinished-peer-followup");
    let completed_peer_scan =
        include_completed_peer_match.then(|| temp.path().join("completed-peer-scan"));
    let script = [
        "#!/bin/sh",
        "set -eu",
        "machine=\"$1\"",
        "blocked_operation=\"$2\"",
        "started=\"$3\"",
        "followup=\"$4\"",
        "completed=\"$5\"",
        "completed_stage=\"$6\"",
        "while IFS= read -r request; do",
        r#"  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')"#,
        r#"  op=$(printf '%s\n' "$request" | sed -n 's/.*"op":"\([^"]*\)".*/\1/p')"#,
        "  case \"$op\" in",
        "    open)",
        r#"      printf '{"id":%s,"data":{"store":"%s/default","status":"ok","db":"/fixture/%s.sqlite","tape_dirs":[],"reader_mode":"live","snapshot_at":"2026-09-25T00:00:00Z"}}\n' "$id" "$machine" "$machine""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"self":"%s","build":"0.2.1","protocol":1,"schema":4,"query_semantics":1,"limits":{"grep_k":10000}}}\n' "$id" "$machine""#,
        "      ;;",
        "    grep_scan)",
        "      if [ \"$blocked_operation\" = grep_scan ]; then",
        "        touch \"$started\"",
        "        while IFS= read -r ignored; do touch \"$followup\"; done",
        "        exit 0",
        "      fi",
        r#"      printf '{"id":%s,"data":{"type":"match","tape_id":"%s-tape","timestamp":"2026-09-25T00:00:00Z","total_lines":1,"anchor_line":1,"match_count":1,"provenance_match_count":0,"provenance_event_count":1,"refs_up":0,"refs_down":0,"files_touched":[]}}\n' "$id" "$machine""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"total":1,"returned":1,"time_range":{"start":"2026-09-25T00:00:00Z","end":"2026-09-25T00:00:00Z"},"truncated":false}}\n' "$id""#,
        "      if [ -n \"$completed\" ] && [ \"$completed_stage\" = grep_scan ]; then touch \"$completed\"; sleep 0.1; fi",
        "      ;;",
        "    dispatch_rows)",
        "      if [ \"$blocked_operation\" = dispatch_rows ]; then",
        "        touch \"$started\"",
        "        while IFS= read -r ignored; do touch \"$followup\"; done",
        "        exit 0",
        "      fi",
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{}}\n' "$id""#,
        "      if [ -n \"$completed\" ] && [ \"$completed_stage\" = dispatch_rows ]; then touch \"$completed\"; sleep 0.1; fi",
        "      ;;",
        "    *) exit 78 ;;",
        "  esac",
        "done",
    ]
    .join("\n");
    std::fs::write(&script_path, script).expect("write blocking peer script");
    let (caller_home, repo) =
        write_local_grep_source(temp.path(), "caller-before-cancel", local_content);
    let mut peers = json!({
        "alpha": {
            "command": [
                "/bin/sh",
                script_path,
                "alpha",
                blocked_operation,
                operation_started,
                followup_request,
                "",
                "none",
            ],
            "engram": binary,
            "exports": ["default"],
        }
    });
    if let Some(completed_peer_scan) = &completed_peer_scan {
        let beta_started = temp.path().join("beta-operation-started");
        peers["beta"] = json!({
            "command": [
                "/bin/sh",
                script_path,
                "beta",
                "none",
                beta_started,
                followup_request,
                completed_peer_scan,
                blocked_operation,
            ],
            "engram": binary,
            "exports": ["default"],
        });
    }
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": peers,
        }))
        .expect("serialize topology"),
    )
    .expect("write caller topology");

    let mut command = Command::new(binary);
    command.current_dir(&repo).env("HOME", &caller_home).args([
        "grep",
        pattern,
        "--peers",
        if include_completed_peer_match {
            "alpha,beta"
        } else {
            "alpha"
        },
    ]);
    if require_complete {
        command.arg("--require-complete");
    }
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn peer grep");
    (child, operation_started, completed_peer_scan)
}

#[cfg(unix)]
fn interrupt_waiting_grep(
    mut child: std::process::Child,
    operation_started: &std::path::Path,
    completed_peer_scan: Option<&std::path::Path>,
) -> (std::process::Output, Duration) {
    let start_deadline = Instant::now() + Duration::from_secs(5);
    while (!operation_started.exists() || completed_peer_scan.is_some_and(|path| !path.exists()))
        && Instant::now() < start_deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(operation_started.exists(), "peer operation did not start");
    if let Some(completed_peer_scan) = completed_peer_scan {
        assert!(
            completed_peer_scan.exists(),
            "second peer did not complete its requested phase"
        );
        // Let the caller's response reader consume the terminal frame before SIGINT.
        std::thread::sleep(Duration::from_millis(100));
    }
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
    let (child, operation_started, completed_peer_scan) = spawn_grep_waiting_for_peer(
        &temp,
        "needle-cancel",
        "{\"t\":\"2026-09-25T00:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-cancel local\"}\n",
        "grep_scan",
        false,
        true,
    );
    let (output, elapsed) =
        interrupt_waiting_grep(child, &operation_started, completed_peer_scan.as_deref());
    assert!(
        elapsed < Duration::from_secs(3),
        "peer cancellation exceeded its deadline"
    );
    let result = assert_caller_sigint_result(&output);
    assert!(
        result["federation"]["coverage"] == "partial",
        "the interrupted scan should make coverage partial"
    );
    assert!(
        result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| { session["tape_id"] == "caller-before-cancel" })
    );
    assert!(
        result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| { session["tape_id"] == "beta-tape" })
    );
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
    assert_eq!(
        result["cancellation"]["incomplete_sources"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "incomplete sources: {}",
        result["cancellation"]["incomplete_sources"]
    );
    let beta = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "beta/default")
        .expect("completed peer source");
    assert!(
        beta["error"].is_null(),
        "a completed source must not be labelled cancelled"
    );
    assert!(
        !temp.path().join("unfinished-peer-followup").exists(),
        "caller sent a later request to an unfinished scan"
    );
}

#[cfg(unix)]
#[test]
fn grep_ctrl_c_overrides_require_complete_with_exit_130() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (child, operation_started, completed_peer_scan) = spawn_grep_waiting_for_peer(
        &temp,
        "needle-cancel-required",
        "{\"t\":\"2026-09-25T00:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-cancel-required local\"}\n",
        "grep_scan",
        true,
        true,
    );
    let (output, elapsed) =
        interrupt_waiting_grep(child, &operation_started, completed_peer_scan.as_deref());
    assert!(elapsed < Duration::from_secs(3));
    let result = assert_caller_sigint_result(&output);
    assert_eq!(result["federation"]["coverage"], "partial");
    assert!(
        result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| { session["tape_id"] == "caller-before-cancel" })
    );
    assert!(
        result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| { session["tape_id"] == "beta-tape" })
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("incomplete_coverage"));
    assert_eq!(
        result["cancellation"]["incomplete_sources"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "incomplete sources: {}",
        result["cancellation"]["incomplete_sources"]
    );
    let beta = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "beta/default")
        .expect("completed peer source");
    assert!(
        beta["error"].is_null(),
        "a completed source must not be labelled cancelled"
    );
    assert!(
        !temp.path().join("unfinished-peer-followup").exists(),
        "caller sent a later request to an unfinished scan"
    );
}

#[cfg(unix)]
#[test]
fn grep_ctrl_c_without_matches_is_cancelled_not_no_results() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (child, operation_started, completed_peer_scan) = spawn_grep_waiting_for_peer(
        &temp,
        "needle-cancel-empty",
        "{\"t\":\"2026-09-25T00:00:00Z\",\"k\":\"msg.in\",\"content\":\"unrelated local\"}\n",
        "grep_scan",
        false,
        false,
    );
    let (output, elapsed) = interrupt_waiting_grep(child, &operation_started, None);
    assert!(elapsed < Duration::from_secs(3));
    let result = assert_caller_sigint_result(&output);
    assert_eq!(result["federation"]["coverage"], "partial");
    assert_eq!(result["sessions"], json!([]));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("no_results"));
    assert!(completed_peer_scan.is_none());
    assert!(
        !temp.path().join("unfinished-peer-followup").exists(),
        "caller sent a later request to an unfinished scan"
    );
}

#[cfg(unix)]
#[test]
fn grep_ctrl_c_during_dispatch_keeps_completed_scan_aggregates() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (child, operation_started, completed_peer_scan) = spawn_grep_waiting_for_peer(
        &temp,
        "needle-cancel-metadata",
        "{\"t\":\"2026-09-24T10:00:00Z\",\"k\":\"msg.in\",\"content\":\"unrelated local\"}\n",
        "dispatch_rows",
        false,
        false,
    );
    let (output, elapsed) = interrupt_waiting_grep(child, &operation_started, None);
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
    assert!(completed_peer_scan.is_none());
    assert!(
        !temp.path().join("unfinished-peer-followup").exists(),
        "caller sent a later request after metadata cancellation"
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
    assert_eq!(offset_result["total"], serde_json::Value::Null);
    assert_eq!(offset_result["total_bounds"]["min"], 2);
    assert_eq!(
        offset_result["total_bounds"]["max"],
        serde_json::Value::Null
    );
    assert_eq!(offset_result["time_range"], serde_json::Value::Null);
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

    let offset_counted = Command::new(binary)
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
            "--count",
        ])
        .output()
        .expect("run partial count grep with a nonzero offset");
    assert!(
        offset_counted.status.success(),
        "offset count should retain observed coverage: {}",
        String::from_utf8_lossy(&offset_counted.stderr)
    );
    let offset_count_result: serde_json::Value =
        serde_json::from_slice(&offset_counted.stdout).expect("offset count JSON");
    assert!(
        offset_count_result["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(offset_count_result["returned"], 1);
    assert_eq!(offset_count_result["total"], serde_json::Value::Null);
    assert_eq!(offset_count_result["total_bounds"]["min"], 2);
    assert_eq!(
        offset_count_result["total_bounds"]["max"],
        serde_json::Value::Null
    );
    assert_eq!(offset_count_result["time_range"], serde_json::Value::Null);
    assert_eq!(offset_count_result["truncated"], serde_json::Value::Null);

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

#[cfg(unix)]
#[test]
fn topology_status_refills_open_worker_slot_without_waiting_for_slow_peer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let release_slow_peer = temp.path().join("release-slow-peer");

    let mut slow = write_grep_owner(temp.path(), "a", binary, &[]);
    let slow_home = slow["command"][1]
        .as_str()
        .expect("slow peer HOME assignment")
        .to_string();
    let slow_script_path = temp.path().join("wait-for-later-peer.sh");
    let slow_script = [
        "#!/bin/sh",
        "set -eu",
        "release=\"$1\"",
        "binary=\"$2\"",
        "i=0",
        "while [ ! -e \"$release\" ] && [ \"$i\" -lt 300 ]; do",
        "  sleep 0.01",
        "  i=$((i + 1))",
        "done",
        "[ -e \"$release\" ] || exit 76",
        "exec \"$binary\" peer-serve --stdio",
    ]
    .join("\n");
    std::fs::write(&slow_script_path, slow_script).expect("write delayed peer launcher");
    slow["command"] = json!([
        "/usr/bin/env",
        slow_home,
        "/bin/sh",
        slow_script_path,
        release_slow_peer,
        binary,
    ]);

    let mut fast = Vec::new();
    for machine in ["b", "c", "d"] {
        fast.push(write_grep_owner(temp.path(), machine, binary, &[]));
    }

    let mut later = write_grep_owner(temp.path(), "e", binary, &[]);
    let later_home = later["command"][1]
        .as_str()
        .expect("later peer HOME assignment")
        .to_string();
    let later_script_path = temp.path().join("release-slow-peer.sh");
    let later_script = [
        "#!/bin/sh",
        "set -eu",
        "release=\"$1\"",
        "binary=\"$2\"",
        "touch \"$release\"",
        "exec \"$binary\" peer-serve --stdio",
    ]
    .join("\n");
    std::fs::write(&later_script_path, later_script).expect("write later peer launcher");
    later["command"] = json!([
        "/usr/bin/env",
        later_home,
        "/bin/sh",
        later_script_path,
        release_slow_peer,
        binary,
    ]);

    let caller_home = temp.path().join("caller-home");
    let caller_engram = caller_home.join(".engram");
    std::fs::create_dir_all(&caller_engram).expect("caller home");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("caller repo");
    std::fs::write(
        caller_engram.join("topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "a": slow,
                "b": fast.remove(0),
                "c": fast.remove(0),
                "d": fast.remove(0),
                "e": later,
            },
        }))
        .expect("serialize topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["topology", "status", "--peers", "all"])
        .output()
        .expect("run topology status with a blocked open slot");
    assert!(
        output.status.success(),
        "topology status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(result["status"], "ok", "topology status: {result:#}");
    assert!(
        release_slow_peer.exists(),
        "later peer never released slow peer"
    );
    for machine in ["a", "b", "c", "d", "e"] {
        assert_eq!(
            result["peers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|peer| peer["machine"] == machine)
                .unwrap_or_else(|| panic!("missing peer {machine}"))["status"],
            "ok",
            "peer {machine} should have opened successfully: {result:#}"
        );
    }
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
        &[("current-segment", current), ("previous-segment", previous)],
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
                            "include_digest": true,
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
    assert_eq!(
        facts["digest"],
        format!("{:x}", sha2::Sha256::digest(current.as_bytes()))
    );

    let missing = response
        .data
        .iter()
        .find(|row| row["tape_id"] == "missing-segment")
        .expect("missing tape outcome");
    assert_eq!(missing["status"], "unavailable");
    assert_eq!(missing["error"]["code"], "tape_unavailable");
    assert!(missing["digest"].is_null());
    assert_eq!(missing["summary"]["total_lines"], 0);
    assert_eq!(missing["summary"]["files_touched"], json!([]));
}

#[test]
fn peer_tape_facts_uses_smallest_resolvable_anchor_offset() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let content = concat!(
        "{\"k\":\"meta\"}\n",
        "\n",
        "{\"t\":\"2026-09-25T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle anchor\"}\n",
    );
    let remote = write_grep_owner(
        temp.path(),
        "stale-anchor-owner",
        binary,
        &[("stale-anchor-tape", content)],
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
    let mut owner = RemoteOwner::connect(
        "stale-anchor-owner",
        "caller",
        &peer,
        Duration::from_secs(5),
    )
    .expect("connect owner");
    let response = owner
        .round(
            &[PeerRequest::new(
                "tape_facts",
                vec!["default".into()],
                json!({
                    "items": [{
                        "tape_id": "stale-anchor-tape",
                        "anchor_offsets": [1, 2],
                        "grep_filter": "needle",
                        "window_lines": 1,
                    }]
                }),
            )],
            Duration::from_secs(5),
        )
        .into_iter()
        .next()
        .expect("tape_facts response")
        .expect("stale index anchor is resolved against available rows");

    let facts = &response.data[0];
    assert_eq!(facts["status"], "ok");
    assert_eq!(facts["summary"]["anchor_line"], 3);
    assert_eq!(facts["summary"]["window_start"], 3);
    assert_eq!(facts["summary"]["window_end"], 3);
    assert_eq!(facts["summary"]["grep_filter_hits_window"], true);
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
    assert_eq!(
        facts["edit_offset_to_turn"][0]["recovered_source_offset"],
        3
    );
    assert!(facts["digest"].is_null());

    let missing_point = owner
        .round(
            &[PeerRequest::new(
                "tape_facts",
                vec!["default".into()],
                json!({
                    "items": [{"tape_id": "legacy-segment", "edit_offsets": [1]}]
                }),
            )],
            Duration::from_secs(5),
        )
        .into_iter()
        .next()
        .expect("recovery integrity response")
        .expect_err("missing recovery edit offset must be fatal");
    assert_eq!(missing_point.code, "native_recovery_error");
    assert!(
        missing_point
            .message
            .contains("missing from recovery points")
    );

    let locator_path = locator_dir.join("legacy-segment.json");
    let mut tampered: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&locator_path).expect("read recovery locator"))
            .expect("parse recovery locator");
    tampered["recovered"]["points"][0]["turn"] = json!(999);
    std::fs::write(
        &locator_path,
        serde_json::to_vec(&tampered).expect("serialize tampered locator"),
    )
    .expect("tamper recovery locator");
    let tampered_locator = owner
        .round(
            &[PeerRequest::new(
                "tape_facts",
                vec!["default".into()],
                json!({
                    "items": [{"tape_id":"legacy-segment", "edit_offsets":[2]}]
                }),
            )],
            Duration::from_secs(5),
        )
        .pop()
        .expect("tampered locator outcome")
        .expect_err("tampered legacy locator must fail closed");
    assert_eq!(tampered_locator.code, "native_recovery_error");
    assert!(tampered_locator.message.contains("not bound by context"));
}

#[test]
fn peer_decompression_caps_are_reported_as_over_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let tape_id = "decompression-cap-tape";
    let content = format!(
        "{{\"t\":\"2026-09-25T12:00:00Z\",\"k\":\"meta\"}}\n{{\"t\":\"2026-09-25T12:01:00Z\",\"k\":\"note\",\"content\":\"{}\"}}\n",
        "needle content that is larger than the configured decompression cap".repeat(3)
    );
    let peer_config =
        write_grep_owner(temp.path(), "limited-owner", binary, &[(tape_id, &content)]);
    let owner_home = temp.path().join("limited-owner-home");
    let topology_path = owner_home.join(".engram/topology.yml");
    let topology = std::fs::read_to_string(&topology_path).expect("read owner topology");
    std::fs::write(
        &topology_path,
        format!("{topology}limits:\n  decompressed_bytes_per_tape: 32\n"),
    )
    .expect("set owner decompression cap");

    let peer = TopologyPeer {
        ssh: None,
        command: Some(
            peer_config["command"]
                .as_array()
                .expect("owner command")
                .iter()
                .map(|arg| arg.as_str().expect("command arg").to_string())
                .collect(),
        ),
        engram: binary.to_string(),
        exports: vec!["default".into()],
    };
    let mut owner = RemoteOwner::connect("limited-owner", "querier", &peer, Duration::from_secs(5))
        .expect("connect limited owner");
    let facts = owner
        .round(
            &[PeerRequest::new(
                "tape_facts",
                vec!["default".into()],
                json!({"items":[{"tape_id":tape_id}]}),
            )],
            Duration::from_secs(5),
        )
        .pop()
        .expect("tape facts outcome")
        .expect("per-tape cap failure is reported in data");
    assert_eq!(facts.data[0]["status"], "failed");
    assert_eq!(facts.data[0]["error"]["code"], "over_limit");

    let grep = owner
        .round(
            &[PeerRequest::new(
                "grep_scan",
                vec!["default".into()],
                json!({"pattern":"needle", "k":10}),
            )],
            Duration::from_secs(5),
        )
        .pop()
        .expect("grep outcome")
        .expect("per-tape cap failure is reported in data");
    assert_eq!(grep.data[0]["type"], "failure");
    assert_eq!(grep.data[0]["error"]["code"], "over_limit");
    drop(owner);

    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-empty-tape",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"note\",\"content\":\"empty\"}\n",
    );
    set_peer_topology(&caller_home, json!({"limited-owner":peer_config}));
    let peek = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "peek",
            tape_id,
            "--store",
            "limited-owner/default",
            "--start",
            "1",
            "--lines",
            "1",
        ])
        .output()
        .expect("run bounded remote peek");
    assert!(!peek.status.success());
    assert!(peek.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_str(
        String::from_utf8_lossy(&peek.stderr)
            .lines()
            .last()
            .expect("structured over-limit error"),
    )
    .expect("over-limit JSON");
    assert_eq!(error["error"]["code"], "over_limit");
}

#[test]
fn peer_grep_returns_budget_exceeded_instead_of_a_truncated_page() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let contents = (0..24)
        .map(|index| {
            (
                format!("budget-tape-{index:02}"),
                format!(
                    "{{\"t\":\"2026-09-25T12:{index:02}:00Z\",\"k\":\"meta\"}}\n{{\"t\":\"2026-09-25T12:{index:02}:01Z\",\"k\":\"note\",\"content\":\"needle result {index}\"}}\n"
                ),
            )
        })
        .collect::<Vec<_>>();
    let tape_refs = contents
        .iter()
        .map(|(tape_id, content)| (tape_id.as_str(), content.as_str()))
        .collect::<Vec<_>>();
    let remote = write_grep_owner(temp.path(), "budget-owner", binary, &tape_refs);
    let owner_home = temp.path().join("budget-owner-home");
    let topology_path = owner_home.join(".engram/topology.yml");
    let mut topology = std::fs::read_to_string(&topology_path).expect("owner topology");
    topology.push_str("limits:\n  non_file_response_bytes: 1200\n");
    std::fs::write(&topology_path, topology).expect("set response budget");
    let peer = TopologyPeer {
        ssh: None,
        command: Some(
            remote["command"]
                .as_array()
                .expect("owner command")
                .iter()
                .map(|arg| arg.as_str().expect("command arg").to_string())
                .collect(),
        ),
        engram: binary.to_string(),
        exports: vec!["default".into()],
    };
    let mut owner = RemoteOwner::connect("budget-owner", "caller", &peer, Duration::from_secs(5))
        .expect("connect budget owner");
    let failure = owner
        .round(
            &[PeerRequest::new(
                "grep_scan",
                vec!["default".into()],
                json!({"pattern":"needle", "k":24}),
            )],
            Duration::from_secs(5),
        )
        .pop()
        .expect("grep outcome")
        .expect_err("owner must fail the operation instead of emitting a partial page");
    assert_eq!(failure.code, "budget_exceeded");
}

#[test]
fn grep_peer_rounds_do_not_scale_with_the_number_of_matching_tapes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let tapes = (0..24)
        .map(|index| {
            (
                format!("round-tape-{index:02}"),
                format!(
                    "{{\"t\":\"2026-09-25T12:{index:02}:00Z\",\"k\":\"meta\"}}\n{{\"t\":\"2026-09-25T12:{index:02}:01Z\",\"k\":\"note\",\"content\":\"needle round result {index}\"}}\n"
                ),
            )
        })
        .collect::<Vec<_>>();
    let tape_refs = tapes
        .iter()
        .map(|(tape_id, content)| (tape_id.as_str(), content.as_str()))
        .collect::<Vec<_>>();
    let mut peer = write_grep_owner(temp.path(), "round-owner", binary, &tape_refs);
    let operations = log_peer_operations(temp.path(), "round-owner", binary, &mut peer);
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "round-caller",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"note\",\"content\":\"no remote match\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {"round-owner": peer},
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle", "--limit", "24", "--peers", "round-owner"])
        .output()
        .expect("run many-tape remote grep");
    assert!(
        output.status.success(),
        "many-tape grep failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert_eq!(value["federation"]["coverage"], "complete");
    assert_eq!(value["sessions"].as_array().unwrap().len(), 24);
    assert_eq!(operation_count(&operations, "grep_scan"), 1);
    assert_eq!(operation_count(&operations, "dispatch_rows"), 1);
    assert_eq!(operation_count(&operations, "read_file"), 0);
}

#[test]
fn peer_open_deadline_bounds_black_hole_latency_with_scheduling_margin() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-before-open-timeout",
        "{\"t\":\"2026-09-25T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-connect-deadline\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "blackhole": {
                    "command": ["/usr/bin/sleep", "60"],
                    "engram": binary,
                    "exports": ["default"],
                }
            },
            "limits": {
                "connect_open_deadline_ms": 200,
                "total_query_deadline_ms": 2_000,
            }
        }))
        .expect("serialize topology"),
    )
    .expect("write caller topology");

    let started = Instant::now();
    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle-connect-deadline", "--peers", "blackhole"])
        .output()
        .expect("run grep against black-holed peer");
    let elapsed = started.elapsed();
    assert!(
        output.status.success(),
        "partial local result should survive: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert_eq!(result["federation"]["coverage"], "partial");
    let source = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "blackhole/default")
        .expect("black-hole source");
    assert_eq!(source["status"], "unavailable");
    assert_eq!(source["error"]["code"], "timeout");
    assert!(
        elapsed >= Duration::from_millis(150),
        "deadline returned early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "200ms connect deadline exceeded scheduling margin: {elapsed:?}"
    );
}

#[test]
fn federated_show_adds_no_application_files_to_caller_or_owner() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let content = concat!(
        "{\"t\":\"2026-09-25T11:59:00Z\",\"k\":\"meta\",\"model\":\"file-audit\"}\n",
        "{\"t\":\"2026-09-25T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"read-only owner fixture\"}\n",
    );
    let tape_id = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
    let _peer_config =
        write_grep_owner(temp.path(), "owner", binary, &[(tape_id.as_str(), content)]);
    ingest_owner_test_tape(temp.path(), "owner", &tape_id, content, &[]);
    let owner_home = temp.path().join("owner-home");
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-file-audit",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"note\",\"content\":\"local baseline\"}\n",
    );
    let caller_config = caller_home.join(".engram");
    let peer_command = vec![
        "/usr/bin/env".to_string(),
        format!("HOME={}", owner_home.display()),
        binary.to_string(),
        "peer-serve".to_string(),
        "--stdio".to_string(),
    ];
    std::fs::write(
        caller_config.join("topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "owner": {"command": peer_command, "engram": binary, "exports": ["default"]}
            }
        }))
        .expect("serialize topology"),
    )
    .expect("write caller topology");
    let caller_before = snapshot_tree(&caller_home);
    let owner_before = snapshot_tree(&owner_home);
    let repo_before = snapshot_tree(&repo);
    for query_number in 0..100 {
        let output = Command::new(binary)
            .current_dir(&repo)
            .env("HOME", &caller_home)
            .args(["show", &tape_id, "--peers", "owner"])
            .output()
            .expect("run federated show");
        assert!(
            output.status.success(),
            "federated show {query_number} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("show JSON");
        assert_eq!(result["digest"], tape_id);
        assert_eq!(result["location"]["machine"], "owner");
        assert_eq!(result["federation"]["coverage"], "complete");
    }

    assert_eq!(snapshot_tree(&caller_home), caller_before);
    assert_eq!(snapshot_tree(&repo), repo_before);
    assert_owner_unchanged_except_sqlite_sidecars(&owner_before, &snapshot_tree(&owner_home));
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
    assert_eq!(result["total_bounds"]["min"], 2);
    assert_eq!(result["total_bounds"]["max"], serde_json::Value::Null);
    assert_eq!(result["time_range"], serde_json::Value::Null);
    let available_source = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "available/default")
        .expect("available source");
    assert_eq!(available_source["grep_scan"]["total"], 2);
    assert_eq!(available_source["grep_scan"]["truncated"], true);
    assert_eq!(
        available_source["grep_scan"]["time_range"]["start"],
        "2026-09-24T12:00:00Z"
    );

    let counted = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-partial-truncated",
            "--peers",
            "available,offline",
            "--count",
            "--limit",
            "1",
        ])
        .output()
        .expect("run partial count with proven tail");
    assert!(
        counted.status.success(),
        "partial count should retain known tail evidence: {}",
        String::from_utf8_lossy(&counted.stderr)
    );
    let count_result: serde_json::Value =
        serde_json::from_slice(&counted.stdout).expect("partial count JSON");
    assert!(count_result["sessions"].as_array().unwrap().is_empty());
    assert_eq!(count_result["federation"]["coverage"], "partial");
    assert_eq!(count_result["returned"], 1);
    assert_eq!(count_result["total"], serde_json::Value::Null);
    assert_eq!(count_result["total_bounds"]["min"], 2);
    assert_eq!(count_result["total_bounds"]["max"], serde_json::Value::Null);
    assert_eq!(count_result["time_range"], serde_json::Value::Null);
    assert_eq!(count_result["truncated"], true);
}

#[test]
fn grep_uses_known_merged_page_to_prove_truncation_with_missing_peer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-lower-bound-first",
        "{\"t\":\"2026-09-24T11:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-lower-bound local first\"}\n",
    );
    let extra_content = "{\"t\":\"2026-09-24T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-lower-bound local second\"}\n";
    let compressed = zstd::stream::encode_all(extra_content.as_bytes(), 0).expect("compress tape");
    std::fs::write(
        repo.join(".engram/tapes/caller-lower-bound-second.jsonl.zst"),
        compressed,
    )
    .expect("write second local tape");
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
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
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");

    for extra_args in [vec!["--limit", "1"], vec!["--count", "--limit", "1"]] {
        let output = Command::new(binary)
            .current_dir(&repo)
            .env("HOME", &caller_home)
            .args(["grep", "needle-lower-bound", "--peers", "offline"])
            .args(extra_args.iter().copied())
            .output()
            .expect("run grep with incomplete selected peer");
        assert!(
            output.status.success(),
            "known local tail should survive missing peer: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
        assert_eq!(result["federation"]["coverage"], "partial");
        assert_eq!(result["returned"], 1);
        assert_eq!(result["total"], serde_json::Value::Null);
        assert_eq!(result["total_bounds"]["min"], 2);
        assert_eq!(result["total_bounds"]["max"], serde_json::Value::Null);
        assert_eq!(result["time_range"], serde_json::Value::Null);
        assert_eq!(result["truncated"], true);
        if extra_args.first().copied() == Some("--count") {
            assert!(result["sessions"].as_array().unwrap().is_empty());
        } else {
            assert_eq!(result["sessions"].as_array().unwrap().len(), 1);
        }
    }
}

#[test]
fn grep_keeps_empty_completed_scan_facts_but_unknowns_missing_scope() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let empty = write_grep_owner(temp.path(), "empty", binary, &[]);
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-empty-observation",
        "{\"t\":\"2026-09-24T10:00:00Z\",\"k\":\"msg.in\",\"content\":\"unrelated\"}\n",
    );
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "empty": empty,
                "offline": {
                    "command": ["/usr/bin/false"],
                    "engram": binary,
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-empty-observation",
            "--peers",
            "empty,offline",
            "--count",
        ])
        .output()
        .expect("run empty partial count grep");
    assert!(
        !output.status.success(),
        "empty grep should report no results"
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("partial JSON");
    assert_eq!(result["federation"]["coverage"], "partial");
    assert!(result["sessions"].as_array().unwrap().is_empty());
    assert_eq!(result["returned"], 0);
    assert_eq!(result["total"], serde_json::Value::Null);
    assert_eq!(result["total_bounds"]["min"], 0);
    assert_eq!(result["total_bounds"]["max"], serde_json::Value::Null);
    assert_eq!(result["time_range"], serde_json::Value::Null);
    assert_eq!(result["truncated"], serde_json::Value::Null);
    let sources = result["federation"]["sources"].as_array().unwrap();
    let empty_source = sources
        .iter()
        .find(|source| source["store"] == "empty/default")
        .expect("completed empty source");
    assert_eq!(empty_source["status"], "ok");
    assert_eq!(empty_source["grep_scan"]["total"], 0);
    assert_eq!(empty_source["grep_scan"]["returned"], 0);
    assert_eq!(
        empty_source["grep_scan"]["time_range"],
        json!({"start": null, "end": null})
    );
    assert_eq!(empty_source["grep_scan"]["truncated"], false);
    let offline_source = sources
        .iter()
        .find(|source| source["store"] == "offline/default")
        .expect("missing source");
    assert_eq!(offline_source["phase"], "open");
}

#[test]
fn grep_discards_match_frames_from_failed_scan_when_aggregating() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-incomplete-frame",
        "{\"t\":\"2026-09-24T11:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle-incomplete-frame local\"}\n",
    );
    let script_path = temp.path().join("failed-scan-peer.sh");
    let script = [
        "#!/bin/sh",
        "while IFS= read -r request; do",
        r#"  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')"#,
        r#"  op=$(printf '%s\n' "$request" | sed -n 's/.*"op":"\([^"]*\)".*/\1/p')"#,
        r#"  case "$op" in"#,
        "    open)",
        r#"      printf '{"id":%s,"data":{"store":"broken/default","status":"ok","db":"/fixture/broken.sqlite","tape_dirs":[],"reader_mode":"live","snapshot_at":"2026-09-25T14:00:00Z"}}\n' "$id""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"self":"broken","build":"@BUILD@","protocol":1,"schema":@SCHEMA@,"query_semantics":@SEMANTICS@,"limits":{"grep_k":10000}}}\n' "$id""#,
        "      ;;",
        "    grep_scan)",
        r#"      printf '{"id":%s,"data":{"type":"match","tape_id":"must-not-appear","timestamp":"2099-01-01T00:00:00Z","total_lines":1,"anchor_line":1,"match_count":1,"provenance_match_count":0,"provenance_event_count":1,"refs_up":0,"refs_down":0,"files_touched":[]}}\n' "$id""#,
        r#"      printf '{"id":%s,"end":true,"ok":false,"error":{"code":"injected_scan_failure","message":"scan did not complete"}}\n' "$id""#,
        "      ;;",
        "    *)",
        r#"      printf '{"id":%s,"end":true,"ok":false,"error":{"code":"unexpected_operation","message":"unexpected operation"}}\n' "$id""#,
        "      ;;",
        "  esac",
        "done",
    ]
    .join("\n")
    .replace("@BUILD@", env!("CARGO_PKG_VERSION"))
    .replace("@SCHEMA@", &SCHEMA_VERSION.to_string())
    .replace("@SEMANTICS@", &QUERY_SEMANTICS_VERSION.to_string());
    std::fs::write(&script_path, script).expect("write failed-scan peer");
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "broken": {
                    "command": ["/bin/sh", script_path],
                    "engram": binary,
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize caller topology"),
    )
    .expect("write caller topology");

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args([
            "grep",
            "needle-incomplete-frame",
            "--peers",
            "broken",
            "--limit",
            "1",
        ])
        .output()
        .expect("run grep with failed terminal frame");
    assert!(
        output.status.success(),
        "local results should survive a failed peer scan: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert_eq!(result["federation"]["coverage"], "partial");
    assert_eq!(result["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(result["sessions"][0]["tape_id"], "caller-incomplete-frame");
    assert_eq!(result["total"], serde_json::Value::Null);
    assert_eq!(result["total_bounds"]["min"], 1);
    assert_eq!(result["total_bounds"]["max"], serde_json::Value::Null);
    assert_eq!(result["time_range"], serde_json::Value::Null);
    assert_eq!(result["truncated"], serde_json::Value::Null);
    let peer_source = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "broken/default")
        .expect("failed peer source");
    assert_eq!(peer_source["status"], "failed");
    assert_eq!(peer_source["phase"], "grep_scan");
    assert_eq!(peer_source["error"]["code"], "injected_scan_failure");
    assert!(peer_source.get("grep_scan").is_none());
}

#[test]
fn grep_discards_incomplete_peer_scan_after_disconnect_and_keeps_concurrent_peer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let complete = write_grep_owner(
        temp.path(),
        "complete",
        binary,
        &[(
            "complete-tape",
            "{\"t\":\"2026-09-25T12:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle complete peer\"}\n",
        )],
    );
    let script_path = temp.path().join("disconnect-during-scan.sh");
    let script = [
        "#!/bin/sh",
        "set -eu",
        "while IFS= read -r request; do",
        r#"  id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')"#,
        r#"  op=$(printf '%s\n' "$request" | sed -n 's/.*"op":"\([^"]*\)".*/\1/p')"#,
        r#"  case "$op" in"#,
        "    open)",
        r#"      printf '{"id":%s,"data":{"store":"broken/default","status":"ok","db":"/fixture/broken.sqlite","tape_dirs":[],"reader_mode":"live","snapshot_at":"2026-09-25T00:00:00Z"}}\n' "$id""#,
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"self":"broken","build":"fixture","protocol":1,"schema":4,"query_semantics":1,"limits":{"grep_k":10000}}}\n' "$id""#,
        "      ;;",
        "    grep_scan)",
        r#"      printf '{"id":%s,"data":{"type":"match","store":"broken/default","tape_id":"must-be-discarded","timestamp":"2026-09-25T13:00:00Z","total_lines":1,"anchor_line":1,"match_count":1,"provenance_match_count":0,"provenance_event_count":1,"refs_up":0,"refs_down":0,"files_touched":[]}}\n' "$id""#,
        "      exit 0",
        "      ;;",
        "    *) exit 78 ;;",
        "  esac",
        "done",
    ]
    .join("\n");
    std::fs::write(&script_path, script).expect("write disconnecting peer");
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "caller-before-disconnect",
        "{\"t\":\"2026-09-25T11:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle local\"}\n",
    );
    set_peer_topology(
        &caller_home,
        json!({
            "complete": complete,
            "broken": {
                "command": ["/bin/sh", script_path],
                "engram": binary,
                "exports": ["default"],
            }
        }),
    );

    let output = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle", "--peers", "complete,broken"])
        .output()
        .expect("run concurrent grep with mid-response disconnect");
    assert!(
        output.status.success(),
        "completed data should survive peer disconnect: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert_eq!(result["federation"]["coverage"], "partial");
    assert!(
        result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| { session["tape_id"] == "caller-before-disconnect" })
    );
    assert!(
        result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| { session["tape_id"] == "complete-tape" })
    );
    assert!(
        result["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|session| { session["tape_id"] != "must-be-discarded" })
    );
    assert_eq!(result["total"], serde_json::Value::Null);
    assert_eq!(result["truncated"], serde_json::Value::Null);
    let broken = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "broken/default")
        .expect("broken source row");
    assert_eq!(broken["status"], "failed");
    assert_eq!(broken["phase"], "grep_scan");
    assert!(broken.get("grep_scan").is_none());
    let complete = result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "complete/default")
        .expect("completed source row");
    assert_eq!(complete["status"], "ok");
    assert_eq!(complete["grep_scan"]["total"], 1);
}

#[test]
fn local_explain_and_peek_without_peer_selection_do_not_start_configured_peers() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let file = "local-offline.rs";
    let source =
        "fn local_offline_target() { let value = alpha + beta; consume(value); }\n".repeat(12);
    let before = source.replace("local_offline_target", "before_local_offline_target");
    let tape_id = "local-offline-explain";
    let events = jsonl(&[
        json!({"t":"2026-09-25T12:00:00Z","k":"meta","model":"local-test"}),
        edit_event(file, &before, &source),
    ]);
    let (caller_home, repo) = write_local_grep_source(temp.path(), tape_id, &events);
    std::fs::write(repo.join(file), &source).expect("write local explain source");
    let db = repo.join(".engram/index.sqlite");
    let parsed = engram::tape::event::parse_jsonl_events(&events).expect("parse local tape");
    let index =
        SqliteIndex::open_writer(db.to_str().expect("local DB path")).expect("open local index");
    index
        .ingest_tape_events(
            tape_id,
            &parsed,
            engram::index::lineage::LINK_THRESHOLD_DEFAULT,
        )
        .expect("index local explain tape");
    drop(index);

    let marker = temp.path().join("peer-was-started");
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
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
    .expect("write topology");

    let explain = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["explain", file])
        .output()
        .expect("run local explain");
    assert!(
        explain.status.success(),
        "local explain failed: {}",
        String::from_utf8_lossy(&explain.stderr)
    );
    let explain: serde_json::Value = serde_json::from_slice(&explain.stdout).expect("explain JSON");
    assert!(
        explain["sessions"]
            .as_array()
            .is_some_and(|sessions| !sessions.is_empty())
    );
    assert!(explain.get("federation").is_none());
    assert!(!marker.exists(), "local explain started a configured peer");

    let peek = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["peek", tape_id, "--start", "1", "--lines", "1"])
        .output()
        .expect("run local peek");
    assert!(
        peek.status.success(),
        "local peek failed: {}",
        String::from_utf8_lossy(&peek.stderr)
    );
    let peek: serde_json::Value = serde_json::from_slice(&peek.stdout).expect("peek JSON");
    assert!(peek.get("federation").is_none());
    assert!(!marker.exists(), "local peek started a configured peer");
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
fn grep_uses_completed_local_count_as_truncation_proof() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_engram");
    let (caller_home, repo) = write_local_grep_source(
        temp.path(),
        "local-nonmatch-count-proof",
        "{\"t\":\"2026-09-24T10:00:00Z\",\"k\":\"msg.in\",\"content\":\"unrelated\"}\n",
    );
    let script_path = temp.path().join("count-proof-peer.sh");
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
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"total":100,"returned":1,"time_range":{"start":"2026-09-24T12:00:00Z","end":"2026-09-24T12:00:00Z"},"truncated":false}}\n' "$id""#,
        "      ;;",
        "    dispatch_rows)",
        r#"      printf '{"id":%s,"end":true,"ok":true,"stats":{"records":0}}\n' "$id""#,
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
    std::fs::write(&script_path, script).expect("write count-proof peer");
    std::fs::write(
        caller_home.join(".engram/topology.yml"),
        serde_json::to_vec(&json!({
            "version": 1,
            "self": "caller",
            "peers": {
                "alpha": {
                    "command": ["/bin/sh", script_path],
                    "engram": binary,
                    "exports": ["default"],
                }
            }
        }))
        .expect("serialize topology"),
    )
    .expect("write caller topology");

    for count in [false, true] {
        let mut command = Command::new(binary);
        command.current_dir(&repo).env("HOME", &caller_home).args([
            "grep",
            "needle-count-proof",
            "--peers",
            "alpha",
        ]);
        if count {
            command.arg("--count");
        }
        let output = command
            .args(["--limit", "1"])
            .output()
            .expect("run grep with contradictory peer count and tail flag");
        assert!(
            output.status.success(),
            "completed peer count should prove truncation: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
        assert_eq!(result["federation"]["coverage"], "complete");
        assert_eq!(result["returned"], 1);
        assert_eq!(result["total"], 100);
        assert!(result.get("total_bounds").is_none());
        assert_eq!(result["truncated"], true);
        assert_eq!(
            result["time_range"],
            json!({"start": "2026-09-24T12:00:00Z", "end": "2026-09-24T12:00:00Z"})
        );
        if count {
            assert!(result["sessions"].as_array().unwrap().is_empty());
        } else {
            assert_eq!(result["sessions"].as_array().unwrap().len(), 1);
        }
    }
}

#[test]
fn grep_keeps_completed_scan_aggregates_when_dispatch_response_disconnects() {
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
        r#"      printf '{"id":%s,"data":{"uuid":"incomplete-dispatch-row"}}\n' "$id""#,
        "      exit 0",
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
        "partial metadata failure should retain matching results; stderr: {}; stdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("grep JSON");
    assert_eq!(result["federation"]["coverage"], "partial");
    assert_eq!(result["total"], 1);
    assert!(result.get("total_bounds").is_none());
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
    assert!(source.get("dispatch_rows").is_none());
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

    let counted = Command::new(binary)
        .current_dir(&repo)
        .env("HOME", &caller_home)
        .args(["grep", "needle-after-scan", "--peers", "alpha", "--count"])
        .output()
        .expect("run count grep after metadata failure");
    assert!(
        counted.status.success(),
        "count mode should retain completed scan aggregates: {}",
        String::from_utf8_lossy(&counted.stderr)
    );
    let count_result: serde_json::Value =
        serde_json::from_slice(&counted.stdout).expect("count result from completed scan");
    assert_eq!(count_result["federation"]["coverage"], "complete");
    assert!(count_result["sessions"].as_array().unwrap().is_empty());
    assert_eq!(count_result["returned"], 1);
    assert_eq!(count_result["total"], 1);
    assert!(count_result.get("total_bounds").is_none());
    assert_eq!(count_result["time_range"]["start"], "2026-09-24T12:00:00Z");
    assert_eq!(count_result["time_range"]["end"], "2026-09-24T12:00:00Z");
    assert_eq!(count_result["truncated"], false);
    let count_source = count_result["federation"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["store"] == "alpha/default")
        .expect("selected peer source in count mode");
    assert_eq!(count_source["status"], "ok");
    assert_eq!(count_source["grep_scan"]["total"], 1);
}
