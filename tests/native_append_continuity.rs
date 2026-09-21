use engram::dispatch::extract_dispatch_links_from_transcript;
use engram::index::DispatchDirection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

const UUID: &str = "788de2da-5970-4814-a2aa-217c5f484d8a";
const LATER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const TEXT: &str = "pub fn isolated_handoff_probe() -> u64 { 73849127 }";
fn cli(root: &Path, args: &[&str], input: Option<&str>) -> Value {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engram"));
    cmd.current_dir(root)
        .env("HOME", root.join("home"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_ne!(value["status"], "partial", "{value}");
    value
}
fn line(row: Value) -> String {
    serde_json::to_string(&row).unwrap() + "\n"
}
fn codex(kind: &str, rest: Value, second: u8) -> String {
    let mut p = rest;
    p["type"] = json!(kind);
    line(
        json!({"type":"response_item","timestamp":format!("2026-09-21T00:00:{second:02}Z"),"payload":p}),
    )
}
fn rows(claude: bool) -> Vec<String> {
    let marker = format!("<engram-src id=\"{UUID}\"/> work");
    if claude {
        let native = |kind: &str, content: Value, second: u8| {
            line(
                json!({"type":kind,"sessionId":"worker","cwd":"/tmp","timestamp":format!("2026-09-21T00:00:{second:02}Z"),"message":{"role":kind,"content":content}}),
            )
        };
        vec![
            native("user", json!([{"type":"text","text":marker}]), 1),
            native(
                "assistant",
                json!([{"type":"tool_use","id":"edit","name":"Write","input":{"file_path":"/tmp/probe.rs","content":TEXT}}]),
                2,
            ),
            native(
                "user",
                json!([{"type":"tool_result","tool_use_id":"edit","content":"written"}]),
                3,
            ),
            native(
                "user",
                json!(format!("<engram-src id=\"{LATER}\"/> later unrelated")),
                4,
            ),
        ]
    } else {
        vec![
            line(
                json!({"type":"session_meta","timestamp":"2026-09-21T00:00:00Z","payload":{"id":"worker","cwd":"/tmp"}}),
            ),
            codex(
                "message",
                json!({"role":"user","content":[{"type":"input_text","text":marker}]}),
                1,
            ),
            codex(
                "custom_tool_call",
                json!({"call_id":"edit","name":"apply_patch","input":format!("*** Begin Patch\n*** Add File: /tmp/probe.rs\n+{TEXT}\n*** End Patch")}),
                2,
            ),
            codex(
                "custom_tool_call_output",
                json!({"call_id":"edit","output":"{\"metadata\":{\"exit_code\":0},\"output\":\"Success. Updated the following files:\\nA /tmp/probe.rs\\n\"}"}),
                3,
            ),
            codex(
                "message",
                json!({"role":"user","content":[{"type":"input_text","text":format!("<engram-src id=\"{LATER}\"/> later unrelated")}]}),
                4,
            ),
        ]
    }
}
fn setup(root: &Path) {
    fs::create_dir_all(root.join("home")).unwrap();
    let parent = line(
        json!({"t":"2026-09-21T00:00:00Z","k":"msg.out","role":"assistant","content":[{"type":"toolCall","arguments":{"payload":format!("<engram-src id=\"{UUID}\"/> work")}}]}),
    );
    cli(root, &["record", "--stdin"], Some(&parent));
}
fn cursor(root: &Path, path: &Path) -> std::path::PathBuf {
    let key = format!(
        "{:x}",
        Sha256::digest(fs::canonicalize(path).unwrap().to_string_lossy().as_bytes())
    );
    root.join(".engram/cursors").join(format!("{key}.json"))
}
fn tape_rows(root: &Path, id: &str) -> Vec<Value> {
    let bytes = fs::read(
        root.join("home/.engram/tapes")
            .join(format!("{id}.jsonl.zst")),
    )
    .unwrap();
    let raw = zstd::stream::decode_all(bytes.as_slice()).unwrap();
    assert_eq!(format!("{:x}", Sha256::digest(&raw)), id);
    String::from_utf8(raw)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn snapshots(root: &Path) -> Vec<(String, Vec<u8>)> {
    fs::read_dir(root.join("home/.engram/tapes"))
        .unwrap()
        .map(|p| p.unwrap().path())
        .filter(|p| p.is_file())
        .map(|p| {
            (
                p.file_name().unwrap().to_str().unwrap().into(),
                fs::read(p).unwrap(),
            )
        })
        .collect()
}
fn logical_result(root: &Path) -> Value {
    let out = cli(root, &["explain", "--", TEXT], None);
    let chain = out["dispatch_lineage"].as_array().unwrap();
    assert_eq!(chain.len(), 1, "{out}");
    assert_eq!(chain[0]["received_uuid"], UUID);
    assert_eq!(chain[0]["received_turn_index"], 0);
    assert_eq!(chain[0]["edit_turn_index"], 1);
    assert_eq!(chain[0]["parent_sent_turn_index"], 0);
    let db = rusqlite::Connection::open(root.join("home/.engram/index.sqlite")).unwrap();
    let counts: Vec<i64> = [
        "evidence_windows",
        "evidence_features",
        "edges",
        "tombstones",
    ]
    .iter()
    .map(|t| {
        db.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |row| row.get(0))
            .unwrap()
    })
    .collect();
    assert!(counts[0] > 0);
    json!({"uuid":chain[0]["received_uuid"],"edit_turn":chain[0]["edit_turn_index"],"receive_turn":chain[0]["received_turn_index"],"parent_turn":chain[0]["parent_sent_turn_index"],"counts":counts})
}
fn ingest_parts(claude: bool, parts: &[usize], legacy: bool) -> Value {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let input = root.join(if claude {
        "receiver.claude.jsonl"
    } else {
        "receiver.codex.jsonl"
    });
    let rows = rows(claude);
    let mut end = 0;
    let mut immutable = Vec::new();
    let mut event_sequence = Vec::new();
    let mut segment_ids = std::collections::HashSet::new();
    for &next in parts {
        let bytes = rows[end..next].concat();
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&input)
            .unwrap()
            .write_all(bytes.as_bytes())
            .unwrap();
        cli(root, &["ingest", input.to_str().unwrap()], None);
        for (name, bytes) in &immutable {
            assert_eq!(
                &fs::read(root.join("home/.engram/tapes").join(name)).unwrap(),
                bytes
            );
        }
        immutable = snapshots(root);
        if let Ok(bytes) = fs::read(cursor(root, &input)) {
            let state: Value = serde_json::from_slice(&bytes).unwrap();
            let id = state["tape_id"].as_str().unwrap();
            if segment_ids.insert(id.to_string()) {
                event_sequence.extend(
                    tape_rows(root, id)
                        .into_iter()
                        .filter(|row| row["k"] != "meta"),
                );
            }
        }
        if legacy && end == 0 {
            let path = cursor(root, &input);
            let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value.as_object_mut().unwrap().remove("continuity");
            fs::write(path, line(value)).unwrap();
        }
        end = next;
    }
    assert_eq!(end, rows.len());
    let mut out = logical_result(root);
    out["events"] = json!(event_sequence);
    let before = snapshots(root);
    let state_before = fs::read(cursor(root, &input)).unwrap();
    let repeat = cli(root, &["ingest", input.to_str().unwrap()], None);
    assert_eq!(repeat["imported_tapes"], 0);
    assert_eq!(repeat["skipped_unchanged"], 1);
    assert_eq!(state_before, fs::read(cursor(root, &input)).unwrap());
    assert_eq!(before.len(), snapshots(root).len());
    for (name, bytes) in before {
        assert_eq!(
            bytes,
            fs::read(root.join("home/.engram/tapes").join(name)).unwrap()
        );
    }
    let state: Value = serde_json::from_slice(&state_before).unwrap();
    assert_eq!(state["byte_cursor"], rows.concat().len());
    assert_eq!(state["continuity"]["message_turns"], 2);
    assert!(
        state["continuity"]["native"]["state"][if claude { "tool_by_id" } else { "calls" }]
            .as_object()
            .unwrap()
            .is_empty()
    );
    let last = tape_rows(root, state["tape_id"].as_str().unwrap());
    if claude {
        assert!(last.iter().all(|r| r["source"]["session_id"] == "worker"));
    } else {
        // Preserve the existing normalization contract: canonical payload.id
        // does not enrich historical raw event source fields.
        assert!(last.iter().all(|r| r["source"]["session_id"].is_null()));
    }
    out
}
#[test]
fn native_codex_every_split_preserves_lineage_and_pending_calls() {
    let full = ingest_parts(false, &[5], false);
    for split in 1..5 {
        assert_eq!(ingest_parts(false, &[split, 5], false), full);
    }
    assert_eq!(ingest_parts(false, &[1, 2, 3, 4, 5], false), full);
}
#[test]
fn native_claude_every_split_preserves_lineage_and_pending_calls() {
    let full = ingest_parts(true, &[4], false);
    for split in 1..4 {
        assert_eq!(ingest_parts(true, &[split, 4], false), full);
    }
    assert_eq!(ingest_parts(true, &[1, 2, 3, 4], false), full);
}
#[test]
fn legacy_cursors_bootstrap_once_without_duplicate_evidence() {
    for claude in [false, true] {
        let n = rows(claude).len();
        let full = ingest_parts(claude, &[n], false);
        assert_eq!(ingest_parts(claude, &[n - 2, n], true), full);
    }
}
#[test]
fn native_dispatch_envelopes_extract_directions_without_tool_output_markers() {
    let marker = format!("<engram-src id=\"{UUID}\"/>");
    for kind in ["custom_tool_call", "function_call"] {
        let input = codex(
            kind,
            json!({"name":"exec","input":marker,"arguments":marker}),
            1,
        );
        let links = extract_dispatch_links_from_transcript(&input);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].direction, DispatchDirection::Sent);
        assert_eq!(links[0].first_turn_index, 0);
    }
    let input = line(
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":marker}}]}}),
    );
    assert_eq!(
        extract_dispatch_links_from_transcript(&input)[0].direction,
        DispatchDirection::Sent
    );
    let input = line(json!({"type":"user","message":{"role":"user","content":marker}}));
    assert_eq!(
        extract_dispatch_links_from_transcript(&input)[0].direction,
        DispatchDirection::Received
    );
    let input = line(
        json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":marker}]}}),
    );
    assert!(extract_dispatch_links_from_transcript(&input).is_empty());
    let input = codex("custom_tool_call_output", json!({"output":marker}), 2);
    assert!(extract_dispatch_links_from_transcript(&input).is_empty());
}

#[test]
fn split_middle_sender_preserves_a_b_c_chain_and_excludes_sibling() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let middle = root.join("middle.codex.jsonl");
    fs::write(&middle, rows(false)[..2].concat()).unwrap();
    cli(root, &["ingest", middle.to_str().unwrap()], None);
    let send = codex(
        "custom_tool_call",
        json!({"call_id":"send","name":"exec","input":format!("tightbeam wake <engram-src id=\"{LATER}\"/>")}),
        2,
    );
    fs::OpenOptions::new()
        .append(true)
        .open(&middle)
        .unwrap()
        .write_all(send.as_bytes())
        .unwrap();
    cli(root, &["ingest", middle.to_str().unwrap()], None);
    let child = root.join("child.codex.jsonl");
    let text = rows(false)[..4]
        .concat()
        .replace(UUID, LATER)
        .replace("worker", "child");
    fs::write(&child, text).unwrap();
    cli(root, &["ingest", child.to_str().unwrap()], None);
    let sibling = root.join("sibling.codex.jsonl");
    fs::write(
        &sibling,
        rows(false)[..2].concat().replace("worker", "sibling"),
    )
    .unwrap();
    cli(root, &["ingest", sibling.to_str().unwrap()], None);
    let sibling_state: Value =
        serde_json::from_slice(&fs::read(cursor(root, &sibling)).unwrap()).unwrap();
    let out = cli(root, &["explain", "--", TEXT], None);
    let chain = out["dispatch_lineage"].as_array().unwrap();
    assert_eq!(chain.len(), 2, "{out}");
    assert_eq!(chain[0]["received_uuid"], LATER);
    assert_eq!(chain[1]["received_uuid"], UUID);
    assert_eq!(chain[0]["parent_sent_turn_index"], 1);
    assert_eq!(chain[1]["edit_turn_index"], 1);
    assert!(
        out["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["session_id"] != sibling_state["tape_id"])
    );
}

#[test]
fn partial_tool_result_does_not_advance_or_duplicate_pending_call() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let input = root.join("receiver.codex.jsonl");
    let rows = rows(false);
    let half = rows[3].len() / 2;
    fs::write(&input, rows[..3].concat() + &rows[3][..half]).unwrap();
    cli(root, &["ingest", input.to_str().unwrap()], None);
    let before: Value = serde_json::from_slice(&fs::read(cursor(root, &input)).unwrap()).unwrap();
    assert_eq!(before["byte_cursor"], rows[..3].concat().len());
    assert_eq!(
        before["continuity"]["native"]["state"]["calls"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    fs::OpenOptions::new()
        .append(true)
        .open(&input)
        .unwrap()
        .write_all(rows[3][half..].as_bytes())
        .unwrap();
    cli(root, &["ingest", input.to_str().unwrap()], None);
    assert_eq!(logical_result(root)["edit_turn"], 1);
    let after: Value = serde_json::from_slice(&fs::read(cursor(root, &input)).unwrap()).unwrap();
    assert!(
        after["continuity"]["native"]["state"]["calls"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn rewritten_source_does_not_inherit_old_dispatch_context() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let input = root.join("receiver.codex.jsonl");
    let rows = rows(false);
    fs::write(&input, rows[..2].concat()).unwrap();
    cli(root, &["ingest", input.to_str().unwrap()], None);
    fs::write(&input, rows[0].clone() + &rows[2..4].concat()).unwrap();
    cli(root, &["ingest", input.to_str().unwrap()], None);
    let result = cli(root, &["explain", "--", TEXT], None);
    assert!(result["dispatch_lineage"].as_array().unwrap().is_empty());
}

#[test]
fn metadata_only_append_advances_cursor_without_replaying_history() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let input = root.join("receiver.codex.jsonl");
    fs::write(&input, rows(false)[..4].concat()).unwrap();
    cli(root, &["ingest", input.to_str().unwrap()], None);
    let before = logical_result(root);
    let tapes = snapshots(root);
    let metadata = line(json!({"type":"event_msg","payload":{"type":"token_count","info":{}}}));
    fs::OpenOptions::new()
        .append(true)
        .open(&input)
        .unwrap()
        .write_all(metadata.as_bytes())
        .unwrap();
    let result = cli(root, &["ingest", input.to_str().unwrap()], None);
    assert_eq!(result["imported_tapes"], 0);
    assert_eq!(snapshots(root).len(), tapes.len());
    let state: Value = serde_json::from_slice(&fs::read(cursor(root, &input)).unwrap()).unwrap();
    assert_eq!(state["byte_cursor"], fs::metadata(&input).unwrap().len());
    assert_eq!(before, logical_result(root));
    assert_eq!(
        cli(root, &["ingest", input.to_str().unwrap()], None)["skipped_unchanged"],
        1
    );
}

#[test]
fn legacy_unchanged_cursor_bootstraps_once_and_context_rebuild_adds_no_evidence() {
    for claude in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        setup(root);
        let input = root.join(if claude {
            "receiver.claude.jsonl"
        } else {
            "receiver.codex.jsonl"
        });
        fs::write(&input, rows(claude).concat()).unwrap();
        cli(root, &["ingest", input.to_str().unwrap()], None);
        let before = logical_result(root);
        let immutable = snapshots(root);
        let state_path = cursor(root, &input);
        let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state.as_object_mut().unwrap().remove("continuity");
        fs::write(&state_path, line(state)).unwrap();
        cli(root, &["ingest", input.to_str().unwrap()], None);
        assert_eq!(before, logical_result(root));
        assert_eq!(snapshots(root).len(), immutable.len() + 1);
        let upgraded = fs::read(&state_path).unwrap();
        cli(root, &["ingest", input.to_str().unwrap()], None);
        assert_eq!(upgraded, fs::read(&state_path).unwrap());
        for (name, bytes) in immutable {
            assert_eq!(
                bytes,
                fs::read(root.join("home/.engram/tapes").join(name)).unwrap()
            );
        }
        // fingerprint deliberately scans project-local tapes, unlike ingest's
        // home store. Rebuild a copied immutable archive in that supported layout.
        fs::create_dir_all(root.join(".engram/tapes")).unwrap();
        for (name, bytes) in snapshots(root) {
            fs::write(root.join(".engram/tapes").join(name), bytes).unwrap();
        }
        fs::remove_file(root.join("home/.engram/index.sqlite")).unwrap();
        cli(root, &["fingerprint"], None);
        assert_eq!(before, logical_result(root));
    }
}

#[test]
fn sender_call_split_from_same_timestamp_message_keeps_full_ingest_turn() {
    for split in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        setup(root);
        let middle = root.join("middle.codex.jsonl");
        let prefix = rows(false)[..2].concat()
            + &codex(
                "message",
                json!({"role":"assistant","content":[{"type":"output_text","text":"dispatching"}]}),
                2,
            );
        let send = codex(
            "custom_tool_call",
            json!({"call_id":"send","name":"exec","input":format!("tightbeam wake <engram-src id=\"{LATER}\"/>")}),
            2,
        );
        fs::write(&middle, &prefix).unwrap();
        if split {
            cli(root, &["ingest", middle.to_str().unwrap()], None);
        }
        fs::OpenOptions::new()
            .append(true)
            .open(&middle)
            .unwrap()
            .write_all(send.as_bytes())
            .unwrap();
        cli(root, &["ingest", middle.to_str().unwrap()], None);
        let child = root.join("child.codex.jsonl");
        fs::write(
            &child,
            rows(false)[..4]
                .concat()
                .replace(UUID, LATER)
                .replace("worker", "child"),
        )
        .unwrap();
        cli(root, &["ingest", child.to_str().unwrap()], None);
        let result = cli(root, &["explain", "--", TEXT], None);
        let chain = result["dispatch_lineage"].as_array().unwrap();
        assert_eq!(chain.len(), 2, "{result}");
        assert_eq!(chain[0]["parent_sent_turn_index"], 1);
        assert_eq!(chain[1]["edit_turn_index"], 1);
        assert_eq!(chain[1]["received_turn_index"], 0);
    }
}

fn install_legacy_fixture(root: &Path, fixture: &Value) {
    use engram::index::{DispatchLink, SqliteIndex};
    use engram::tape::event::parse_jsonl_events;
    fs::create_dir_all(root.join("home/.engram/tapes")).unwrap();
    fs::create_dir_all(root.join(".engram/cursors")).unwrap();
    fs::write(
        root.join("home/.engram/config.yml"),
        "db: ~/.engram/index.sqlite\ntapes_dir: ~/.engram/tapes\n",
    )
    .unwrap();
    let index =
        SqliteIndex::open_writer(root.join("home/.engram/index.sqlite").to_str().unwrap()).unwrap();
    for (id, raw) in fixture["tapes"].as_object().unwrap() {
        let raw = raw.as_str().unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(raw.as_bytes())), *id);
        fs::write(
            root.join("home/.engram/tapes")
                .join(format!("{id}.jsonl.zst")),
            zstd::stream::encode_all(raw.as_bytes(), 0).unwrap(),
        )
        .unwrap();
        let links: Vec<_> = fixture["dispatch_links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r[0] == *id)
            .map(|r| DispatchLink {
                uuid: r[1].as_str().unwrap().into(),
                first_turn_index: r[2].as_i64().unwrap(),
                direction: if r[3] == "sent" {
                    DispatchDirection::Sent
                } else {
                    DispatchDirection::Received
                },
            })
            .collect();
        index
            .ingest_tape_events_with_dispatch(
                id,
                &parse_jsonl_events(raw).unwrap(),
                &links,
                engram::index::lineage::LINK_THRESHOLD_DEFAULT,
            )
            .unwrap();
    }
    for (name, raw) in fixture["files"].as_object().unwrap() {
        let input = root.join(name);
        fs::write(&input, raw.as_str().unwrap()).unwrap();
        fs::write(cursor(root, &input), line(fixture["cursors"][name].clone())).unwrap();
    }
}
fn stored_counts(root: &Path) -> Value {
    let db = rusqlite::Connection::open(root.join("home/.engram/index.sqlite")).unwrap();
    let mut out = serde_json::Map::new();
    for table in [
        "evidence_windows",
        "evidence_features",
        "edges",
        "tombstones",
    ] {
        let count: i64 = db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        out.insert(table.into(), json!(count));
    }
    Value::Object(out)
}
fn assert_context_only_archives(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let db = rusqlite::Connection::open(root.join("home/.engram/index.sqlite")).unwrap();
    let mut contexts = std::collections::BTreeMap::new();
    for (name, bytes) in snapshots(root) {
        let id = name.strip_suffix(".jsonl.zst").unwrap();
        let rows = tape_rows(root, id);
        if !rows
            .iter()
            .any(|row| row["k"] == "meta" && row["ingest_context_only"] == true)
        {
            continue;
        }
        assert!(rows.iter().all(|row| matches!(
            row["k"].as_str(),
            Some("meta" | "msg.in" | "msg.out" | "tool.call")
        )));
        for table in ["evidence_windows", "edges", "tombstones"] {
            let count: i64 = db
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE tape_id = ?1"),
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "context-only tape {id} acquired {table} rows");
        }
        let postings: i64 = db.query_row("SELECT COUNT(*) FROM evidence_features f JOIN evidence_windows w USING(evidence_id) WHERE w.tape_id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(postings, 0, "context-only tape acquired postings");
        contexts.insert(id.to_string(), bytes);
    }
    assert!(!contexts.is_empty());
    contexts
}
#[test]
fn old_binary_full_and_split_stores_recover_historical_lineage_once() {
    for raw in [
        include_str!("fixtures/native-upgrade/codex-full.json"),
        include_str!("fixtures/native-upgrade/codex-split.json"),
        include_str!("fixtures/native-upgrade/claude-full.json"),
        include_str!("fixtures/native-upgrade/claude-split.json"),
    ] {
        let fixture: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            fixture["old_binary_sha256"],
            "65c3f982ee470da93f6a3cde2211899c72bf36af53181cee2db6205970ac3db8"
        );
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        install_legacy_fixture(root, &fixture);
        let immutable = snapshots(root);
        assert_eq!(stored_counts(root), fixture["counts"]);
        assert!(
            cli(root, &["explain", "--", TEXT], None)["dispatch_lineage"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let files: Vec<_> = fixture["files"]
            .as_object()
            .unwrap()
            .keys()
            .map(|name| root.join(name))
            .collect();
        let source_bytes: Vec<_> = files.iter().map(|file| fs::read(file).unwrap()).collect();
        for file in &files {
            let previous: Value =
                serde_json::from_slice(&fs::read(cursor(root, file)).unwrap()).unwrap();
            assert_eq!(previous["byte_cursor"], fs::metadata(file).unwrap().len());
            let upgrade = cli(root, &["ingest", file.to_str().unwrap()], None);
            assert_eq!(upgrade["imported_tapes"], 0);
            assert_eq!(upgrade["skipped_unchanged"], 1);
            assert_eq!(upgrade["native_recovery"]["sources"], 1);
        }
        assert_eq!(
            source_bytes,
            files
                .iter()
                .map(|file| fs::read(file).unwrap())
                .collect::<Vec<_>>()
        );
        assert_eq!(logical_result(root)["edit_turn"], 1);
        assert_eq!(stored_counts(root), fixture["counts"]);
        let repaired = cli(root, &["explain", "--", TEXT], None);
        let historical = &repaired["dispatch_lineage"][0];
        let context_bytes = assert_context_only_archives(root);
        assert_eq!(context_bytes.len(), files.len());
        assert!(context_bytes.contains_key(historical["session"].as_str().unwrap()));
        let old_edit_rows = tape_rows(root, historical["edit_session"].as_str().unwrap());
        assert_eq!(
            old_edit_rows[historical["edit_event_offset"].as_u64().unwrap() as usize]["k"],
            "code.edit"
        );
        assert!(
            fixture["tapes"]
                .get(historical["edit_session"].as_str().unwrap())
                .is_some()
        );
        for (name, bytes) in &immutable {
            assert_eq!(
                *bytes,
                fs::read(root.join("home/.engram/tapes").join(name)).unwrap()
            );
        }
        let count = snapshots(root).len();
        let catalog =
            fs::read(root.join("home/.engram/tapes/native-upgrade-v1/catalog.json")).unwrap();
        for file in &files {
            let prior = fs::read(cursor(root, file)).unwrap();
            let repeat = cli(root, &["ingest", file.to_str().unwrap()], None);
            assert_eq!(repeat["imported_tapes"], 0);
            assert_eq!(repeat["skipped_unchanged"], 1);
            assert_eq!(prior, fs::read(cursor(root, file)).unwrap());
        }
        assert_eq!(
            catalog,
            fs::read(root.join("home/.engram/tapes/native-upgrade-v1/catalog.json")).unwrap()
        );
        assert_eq!(count, snapshots(root).len());
        assert_eq!(stored_counts(root), fixture["counts"]);
        // Rebuild only SQLite from unchanged archives. Recovery remains usable.
        fs::create_dir_all(root.join(".engram/tapes")).unwrap();
        for (name, bytes) in snapshots(root) {
            fs::write(root.join(".engram/tapes").join(name), bytes).unwrap();
        }
        fs::remove_file(root.join("home/.engram/index.sqlite")).unwrap();
        cli(root, &["fingerprint"], None);
        assert_eq!(logical_result(root)["edit_turn"], 1);
        assert_eq!(stored_counts(root), fixture["counts"]);
        assert_eq!(context_bytes, assert_context_only_archives(root));
        assert_eq!(
            source_bytes,
            files
                .iter()
                .map(|file| fs::read(file).unwrap())
                .collect::<Vec<_>>()
        );
        println!(
            "{}",
            json!({"case":files.iter().map(|p| p.file_name().unwrap().to_string_lossy()).collect::<Vec<_>>(),
            "legacy_tapes":fixture["tapes"].as_object().unwrap().len(),
            "new_source_bytes":0,"context_tapes":context_bytes.keys().collect::<Vec<_>>(),
            "historical_edit_session":historical["edit_session"],"historical_edit_offset":historical["edit_event_offset"],
            "counts_before_and_after_rebuild":fixture["counts"],"context_only_bytes_unchanged":true,"context_evidence_and_postings":0})
        );
    }
}

#[test]
fn legacy_recovery_uses_committed_prefix_and_rejects_unbound_locator() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/native-upgrade/codex-split.json")).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    install_legacy_fixture(root, &fixture);
    let receiver = root.join("receiver.codex.jsonl");
    let later = codex(
        "message",
        json!({"role":"user","content":[{"type":"input_text","text":format!("<engram-src id=\"{LATER}\"/> later") }]}),
        4,
    );
    fs::OpenOptions::new()
        .append(true)
        .open(&receiver)
        .unwrap()
        .write_all(later.as_bytes())
        .unwrap();
    cli(root, &["ingest", receiver.to_str().unwrap()], None);
    cli(
        root,
        &["ingest", root.join("sender.codex.jsonl").to_str().unwrap()],
        None,
    );
    let result = cli(root, &["explain", "--", TEXT], None);
    assert_eq!(result["dispatch_lineage"][0]["received_uuid"], UUID);
    let old = result["dispatch_lineage"][0]["edit_session"]
        .as_str()
        .unwrap();
    let locator = root
        .join("home/.engram/tapes/native-upgrade-v1")
        .join(format!("{old}.json"));
    let mut value: Value = serde_json::from_slice(&fs::read(&locator).unwrap()).unwrap();
    value["recovered"]["points"][0]["turn"] = json!(999);
    fs::write(&locator, line(value)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(root)
        .env("HOME", root.join("home"))
        .args(["explain", "--", TEXT])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("recovery locator not bound by context"));
}

#[test]
fn ambiguous_legacy_events_fail_without_advancing_cursor_or_publishing_context() {
    let mut fixture: Value =
        serde_json::from_str(include_str!("fixtures/native-upgrade/codex-split.json")).unwrap();
    let name = "receiver.codex.jsonl";
    let raw = fixture["files"][name].as_str().unwrap();
    let mut rows: Vec<_> = raw.lines().map(|s| s.to_string() + "\n").collect();
    rows.insert(2, rows[1].clone());
    let changed = rows.concat();
    fixture["files"][name] = json!(changed);
    // This models an old cursor after duplicate native frames: it has no source
    // segment ledger with which to disambiguate the identical earlier records.
    let guard_start = changed.len().saturating_sub(512);
    fixture["cursors"][name]["byte_cursor"] = json!(changed.len());
    fixture["cursors"][name]["cursor_guard"] = json!({"offset":guard_start,"len":changed.len()-guard_start,"hash":format!("{:x}", Sha256::digest(&changed.as_bytes()[guard_start..]))});
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    install_legacy_fixture(root, &fixture);
    let input = root.join(name);
    let state_before = fs::read(cursor(root, &input)).unwrap();
    let tapes_before = snapshots(root);
    let out = Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(root)
        .env("HOME", root.join("home"))
        .args(["ingest", input.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ambiguous legacy event"));
    assert_eq!(state_before, fs::read(cursor(root, &input)).unwrap());
    assert_eq!(tapes_before.len(), snapshots(root).len());
    assert_eq!(stored_counts(root), fixture["counts"]);
}
