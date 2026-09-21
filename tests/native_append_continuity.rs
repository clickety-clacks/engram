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
    let child = chain[0]
        .get("edit_session")
        .unwrap_or(&chain[0]["session"])
        .as_str()
        .unwrap();
    assert_public_chain(
        &out,
        &[
            child.into(),
            chain[0]["parent_session"].as_str().unwrap().into(),
        ],
    );
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
    assert_public_chain(
        &out,
        &[
            chain[0]["session"].as_str().unwrap().into(),
            chain[0]["parent_session"].as_str().unwrap().into(),
            chain[1]["parent_session"].as_str().unwrap().into(),
        ],
    );
    assert!(
        out["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["session_id"] != sibling_state["tape_id"])
    );
    // N2: recovering both the historical edit and its middle sender must keep
    // the complete public three-session chain, not only dispatch tuples.
    let old_tapes = snapshots(root);
    let counts = stored_counts(root);
    for input in [&middle, &child] {
        let path = cursor(root, input);
        let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state.as_object_mut().unwrap().remove("continuity");
        fs::write(&path, line(state)).unwrap();
        let upgrade = cli(root, &["ingest", input.to_str().unwrap()], None);
        assert_eq!(upgrade["failure_count"], 0);
        assert_eq!(upgrade["native_recovery"]["sources"], 1);
        assert_eq!(upgrade["skipped_unchanged"], 1);
    }
    let recovered = cli(root, &["explain", "--", TEXT], None);
    let chain = recovered["dispatch_lineage"].as_array().unwrap();
    assert_eq!(chain.len(), 2, "{recovered}");
    assert_eq!(chain[0]["received_uuid"], LATER);
    assert_eq!(chain[1]["received_uuid"], UUID);
    assert_eq!(chain[0]["parent_session"], chain[1]["session"]);
    assert_public_chain(
        &recovered,
        &[
            chain[0]["edit_session"].as_str().unwrap().into(),
            chain[0]["parent_session"].as_str().unwrap().into(),
            chain[1]["parent_session"].as_str().unwrap().into(),
        ],
    );
    assert_eq!(stored_counts(root), counts);
    for (name, bytes) in old_tapes {
        assert_eq!(
            bytes,
            fs::read(root.join("home/.engram/tapes").join(name)).unwrap()
        );
    }
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

fn native_sender(kind: &str, uuid: &str) -> String {
    let prompt = format!("<engram-src id=\"{uuid}\"/> perform the bounded task");
    let command = format!("tightbeam wake --prompt '{prompt}'");
    match kind {
        "function_call" => codex(
            "function_call",
            json!({"call_id":"send","name":"exec_command","arguments":json!({"cmd":command}).to_string()}),
            0,
        ),
        "custom_tool_call" => codex(
            "custom_tool_call",
            json!({"call_id":"send","name":"exec","input":format!("text(await tools.exec_command({}));", json!({"cmd":command}))}),
            0,
        ),
        "tool_use" => line(
            json!({"type":"assistant","sessionId":"parent","timestamp":"2026-09-21T00:00:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"send","name":"Bash","input":{"command":command}}]}}),
        ),
        _ => panic!("unsupported sender fixture"),
    }
}

#[test]
fn contract_native_sender_shapes_and_normalized_input_follow_actual_parent_across_restarts() {
    for kind in [
        "function_call",
        "custom_tool_call",
        "tool_use",
        "normalized",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("home")).unwrap();
        if kind == "normalized" {
            setup(root);
        } else {
            let sender = root.join(if kind == "tool_use" {
                "sender.claude.jsonl"
            } else {
                "sender.codex.jsonl"
            });
            fs::write(&sender, native_sender(kind, UUID)).unwrap();
            cli(root, &["ingest", sender.to_str().unwrap()], None);
        }
        let db = rusqlite::Connection::open(root.join("home/.engram/index.sqlite")).unwrap();
        let parent: String = db
            .query_row(
                "SELECT tape_id FROM dispatch_links WHERE uuid = ?1 AND direction = 'sent'",
                [UUID],
                |r| r.get(0),
            )
            .unwrap();
        drop(db);
        let claude = kind == "tool_use";
        let receiver = root.join(if claude {
            "receiver.claude.jsonl"
        } else {
            "receiver.codex.jsonl"
        });
        let events = rows(claude);
        // Each call starts a new product process, exercising the persisted cursor
        // through marker, tool call and paired result boundaries.
        for event in &events {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&receiver)
                .unwrap()
                .write_all(event.as_bytes())
                .unwrap();
            cli(root, &["ingest", receiver.to_str().unwrap()], None);
        }
        let result = cli(root, &["explain", "--", TEXT], None);
        assert_eq!(result["dispatch_lineage"].as_array().unwrap().len(), 1);
        assert_eq!(result["dispatch_lineage"][0]["parent_session"], parent);
        assert_eq!(result["dispatch_lineage"][0]["received_uuid"], UUID);
        assert_eq!(result["dispatch_lineage"][0]["received_turn_index"], 0);
        assert_eq!(result["dispatch_lineage"][0]["edit_turn_index"], 1);
        assert_eq!(fs::read_to_string(&receiver).unwrap(), events.concat());
        let repeat = cli(root, &["ingest", receiver.to_str().unwrap()], None);
        assert_eq!(repeat["imported_tapes"], 0);
        assert_eq!(repeat["skipped_unchanged"], 1);
        println!(
            "{}",
            json!({"sender_shape":kind,"actual_parent_tape":parent,"dispatch_hops":1,"receiver_turn":0,"edit_turn":1,"restart_each_append":true,"repeat_imports":0})
        );
    }
}

#[test]
fn contract_negative_controls_do_not_manufacture_native_lineage() {
    for claude in [false, true] {
        for control in [
            "missing_sender",
            "unrelated_uuid",
            "receiver_after_edit",
            "tool_result_sender",
            "quoted_guidance",
            "tool_result_receiver",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join("home")).unwrap();
            let marker = format!("<engram-src id=\"{UUID}\"/>");
            let result_marker = if claude {
                line(
                    json!({"type":"user","sessionId":"worker","timestamp":"2026-09-21T00:00:01Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"quoted","content":format!("quoted output: {marker}")}]}}),
                )
            } else {
                codex(
                    "custom_tool_call_output",
                    json!({"call_id":"quoted","output":format!("quoted output: {marker}")}),
                    1,
                )
            };
            let sender = match control {
                "missing_sender" => None,
                "unrelated_uuid" => Some(native_sender(
                    if claude { "tool_use" } else { "function_call" },
                    LATER,
                )),
                "tool_result_sender" => Some(result_marker.clone()),
                "quoted_guidance" => Some(if claude {
                    line(
                        json!({"type":"assistant","sessionId":"parent","timestamp":"2026-09-21T00:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":format!("Guidance quotes `{marker}`; this prose is not a sending tool call.")}]}}),
                    )
                } else {
                    codex(
                        "message",
                        json!({"role":"assistant","content":[{"type":"output_text","text":format!("Guidance quotes `{marker}`; this prose is not a sending tool call.")}]}),
                        0,
                    )
                }),
                _ => Some(native_sender(
                    if claude {
                        "tool_use"
                    } else {
                        "custom_tool_call"
                    },
                    UUID,
                )),
            };
            if let Some(raw) = sender {
                let path = root.join(if claude {
                    "sender.claude.jsonl"
                } else {
                    "sender.codex.jsonl"
                });
                fs::write(&path, raw).unwrap();
                cli(root, &["ingest", path.to_str().unwrap()], None);
            }
            let mut events = rows(claude);
            events.pop(); // Remove the unrelated later message from the shared fixture.
            let receive = if claude { 0 } else { 1 };
            let marker_event = events.remove(receive);
            if control == "tool_result_receiver" {
                events.insert(receive, result_marker);
            } else if control != "receiver_after_edit" {
                events.insert(receive, marker_event.clone());
            }
            let receiver = root.join(if claude {
                "receiver.claude.jsonl"
            } else {
                "receiver.codex.jsonl"
            });
            fs::write(&receiver, events.concat()).unwrap();
            cli(root, &["ingest", receiver.to_str().unwrap()], None);
            if control == "receiver_after_edit" {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&receiver)
                    .unwrap()
                    .write_all(marker_event.replace("00:00:01Z", "00:00:04Z").as_bytes())
                    .unwrap();
                cli(root, &["ingest", receiver.to_str().unwrap()], None);
            }
            let result = cli(root, &["explain", "--", TEXT], None);
            assert!(
                result["dispatch_lineage"].as_array().unwrap().is_empty(),
                "{control}: {result}"
            );
            let db = rusqlite::Connection::open(root.join("home/.engram/index.sqlite")).unwrap();
            if matches!(
                control,
                "missing_sender" | "unrelated_uuid" | "tool_result_sender" | "quoted_guidance"
            ) {
                let sent: i64 = db.query_row("SELECT COUNT(*) FROM dispatch_links WHERE uuid = ?1 AND direction = 'sent'", [UUID], |r| r.get(0)).unwrap();
                assert_eq!(sent, 0, "{control} manufactured a sender");
            }
            if control == "tool_result_receiver" {
                let received: i64 = db.query_row("SELECT COUNT(*) FROM dispatch_links WHERE uuid = ?1 AND direction = 'received'", [UUID], |r| r.get(0)).unwrap();
                assert_eq!(received, 0);
            }
            assert_eq!(stored_counts(root)["evidence_windows"], 1);
            println!(
                "{}",
                json!({"harness":if claude {"claude"} else {"codex"},"negative_control":control,"dispatch_hops":0,"edit_evidence_retained":1})
            );
        }
    }
}

// N1: identical native chronology must select the same first marker, regardless
// of which complete raw record ends a poll (including a legacy cursor bootstrap).
fn n1_message(claude: bool, uuid: &str, second: u8) -> String {
    let text = format!("<engram-src id=\"{uuid}\"/> received");
    if claude {
        line(
            json!({"type":"user","sessionId":"worker","timestamp":format!("2026-09-21T00:00:{second:02}Z"),"message":{"role":"user","content":text}}),
        )
    } else {
        codex(
            "message",
            json!({"role":"user","content":[{"type":"input_text","text":text}]}),
            second,
        )
    }
}
fn n1_send(claude: bool, uuid: &str, second: u8) -> String {
    let mut row: Value = serde_json::from_str(&native_sender(
        if claude {
            "tool_use"
        } else {
            "custom_tool_call"
        },
        uuid,
    ))
    .unwrap();
    row["timestamp"] = json!(format!("2026-09-21T00:00:{second:02}Z"));
    // Distinct raw calls; only the dispatch UUID repeats.
    if claude {
        row["sessionId"] = json!("worker");
        row["message"]["content"][0]["id"] = json!(format!("send-{second}"));
    } else {
        row["payload"]["call_id"] = json!(format!("send-{second}"));
    }
    line(row)
}
fn n1_ingest(
    root: &Path,
    input: &Path,
    rows: &[String],
    split: Option<usize>,
    legacy: bool,
) -> Vec<Value> {
    let mut events = Vec::new();
    let mut from = 0;
    for end in split.into_iter().chain(std::iter::once(rows.len())) {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(input)
            .unwrap()
            .write_all(rows[from..end].concat().as_bytes())
            .unwrap();
        let before = if root.join("home/.engram/tapes").exists() {
            snapshots(root)
        } else {
            Vec::new()
        };
        let result = cli(root, &["ingest", input.to_str().unwrap()], None);
        assert_eq!(result["failure_count"], 0);
        for (name, bytes) in before {
            assert_eq!(
                bytes,
                fs::read(root.join("home/.engram/tapes").join(name)).unwrap()
            );
        }
        let state: Value = serde_json::from_slice(&fs::read(cursor(root, input)).unwrap()).unwrap();
        events.extend(
            tape_rows(root, state["tape_id"].as_str().unwrap())
                .into_iter()
                .filter(|r| r["k"] != "meta"),
        );
        if legacy && from == 0 {
            let mut old = state;
            old.as_object_mut().unwrap().remove("continuity");
            fs::write(cursor(root, input), line(old)).unwrap();
        }
        from = end;
    }
    events
}
fn assert_public_chain(out: &Value, leaf_to_root: &[String]) {
    let sessions = out["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), leaf_to_root.len(), "{out}");
    let mut descendants = Vec::new();
    for (i, id) in leaf_to_root.iter().enumerate() {
        let s = sessions.iter().find(|s| s["session_id"] == *id).unwrap();
        let parent = leaf_to_root.get(i + 1).map_or(Value::Null, |id| json!(id));
        let children = if i == 0 {
            json!([])
        } else {
            json!([leaf_to_root[i - 1]])
        };
        assert_eq!(s["parent"], parent, "{out}");
        assert_eq!(s["children"], children, "{out}");
        assert_eq!(s["depth"], leaf_to_root.len() - i - 1, "{out}");
        assert_eq!(s["chain_length"], leaf_to_root.len(), "{out}");
        descendants.push(json!({"session_id":id,"parent":parent,"children":children,"depth":leaf_to_root.len()-i-1}));
    }
    descendants.reverse();
    assert_eq!(
        out["chains"],
        json!([{"root_session_id":leaf_to_root.last().unwrap(),"descendants":descendants}]),
        "{out}"
    );
}
#[test]
fn n1_repeated_receives_and_direction_ties_match_full_split_and_legacy() {
    for claude in [false, true] {
        for case in [
            "uvu",
            "uu",
            "sent_then_received",
            "sent_received_tie",
            "received_sent_tie",
        ] {
            let mut prefix = match case {
                "uvu" => vec![
                    n1_message(claude, UUID, 1),
                    n1_message(claude, LATER, 2),
                    n1_message(claude, UUID, 3),
                ],
                "uu" => vec![n1_message(claude, UUID, 1), n1_message(claude, UUID, 2)],
                "sent_then_received" => vec![
                    n1_send(claude, UUID, 1),
                    n1_message(claude, LATER, 2),
                    n1_message(claude, UUID, 3),
                ],
                "sent_received_tie" => vec![n1_send(claude, UUID, 1), n1_message(claude, UUID, 1)],
                _ => vec![n1_message(claude, UUID, 1), n1_send(claude, UUID, 1)],
            };
            let prefix_len = prefix.len();
            let offset = if claude { 1 } else { 2 };
            for raw in &rows(claude)[offset..offset + 2] {
                let mut row: Value = serde_json::from_str(raw).unwrap();
                row["timestamp"] = json!(format!("2026-09-21T00:00:{:02}Z", prefix.len() + 4));
                prefix.push(line(row));
            }
            let mut expected = None;
            for mode in ["full", "split", "legacy"] {
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path();
                setup(root);
                let parent = line(
                    json!({"t":"2026-09-21T00:00:00Z","k":"tool.call","tool":"exec","args":{"prompt":format!("<engram-src id=\"{LATER}\"/>")}}),
                );
                cli(root, &["record", "--stdin"], Some(&parent));
                let input = root.join(if claude {
                    "receiver.claude.jsonl"
                } else {
                    "receiver.codex.jsonl"
                });
                let events = n1_ingest(
                    root,
                    &input,
                    &prefix,
                    (mode != "full").then_some(prefix_len - 1),
                    mode == "legacy",
                );
                let out = cli(root, &["explain", "--", TEXT], None);
                let hop = &out["dispatch_lineage"][0];
                assert_eq!(
                    out["dispatch_lineage"].as_array().unwrap().len(),
                    1,
                    "{case} {mode}: {out}"
                );
                let (uuid, received, edit) = match case {
                    "uvu" => (LATER, 1, 3),
                    "uu" => (UUID, 0, 2),
                    "sent_then_received" => (LATER, 0, 2),
                    _ => (UUID, 0, 1),
                };
                assert_eq!(hop["received_uuid"], uuid, "{case} {mode}: {out}");
                assert_eq!(hop["received_turn_index"], received);
                assert_eq!(hop["edit_turn_index"], edit);
                assert_eq!(hop["parent_sent_turn_index"], 0);
                assert_public_chain(
                    &out,
                    &[
                        hop["session"].as_str().unwrap().into(),
                        hop["parent_session"].as_str().unwrap().into(),
                    ],
                );
                let logical = json!({"uuid":hop["received_uuid"],"received":received,"edit":edit,"parent":hop["parent_session"],"parent_turn":hop["parent_sent_turn_index"],"counts":stored_counts(root),"events":events});
                if let Some(expected) = &expected {
                    assert_eq!(&logical, expected, "{claude} {case} {mode}");
                } else {
                    expected = Some(logical);
                }
                let repeat = cli(root, &["ingest", input.to_str().unwrap()], None);
                assert_eq!(repeat["skipped_unchanged"], 1);
                assert_eq!(repeat["failure_count"], 0);
            }
            println!("N1 receiver: claude={claude} case={case} full/split/legacy equal");
        }
    }
}

#[test]
fn n1_repeated_sender_and_received_direction_preserve_parent_identity() {
    for claude in [false, true] {
        for received_first in [false, true] {
            let mut expected = None;
            for mode in ["full", "split", "legacy"] {
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path();
                fs::create_dir_all(root.join("home")).unwrap();
                let parent = root.join(if claude {
                    "parent.claude.jsonl"
                } else {
                    "parent.codex.jsonl"
                });
                let sequence = vec![
                    if received_first {
                        n1_message(claude, UUID, 1)
                    } else {
                        n1_send(claude, UUID, 1)
                    },
                    n1_message(claude, LATER, 2),
                    n1_send(claude, UUID, 3),
                ];
                n1_ingest(
                    root,
                    &parent,
                    &sequence,
                    (mode != "full").then_some(2),
                    mode == "legacy",
                );
                let input = root.join(if claude {
                    "receiver.claude.jsonl"
                } else {
                    "receiver.codex.jsonl"
                });
                let receiver = rows(claude);
                let end = if claude { 3 } else { 4 };
                fs::write(&input, receiver[..end].concat()).unwrap();
                cli(root, &["ingest", input.to_str().unwrap()], None);
                let out = cli(root, &["explain", "--", TEXT], None);
                if received_first {
                    assert_eq!(out["dispatch_lineage"], json!([]), "{out}");
                    continue;
                }
                let hop = &out["dispatch_lineage"][0];
                assert_eq!(hop["parent_sent_turn_index"], 0, "{out}");
                // Parent tape may be segmented/context-only, but it must contain
                // the original sender call, never the repeated later call.
                let parent_rows = tape_rows(root, hop["parent_session"].as_str().unwrap());
                assert!(
                    parent_rows
                        .iter()
                        .any(|r| r["k"] == "tool.call" && r["t"] == "2026-09-21T00:00:01Z")
                );
                assert_public_chain(
                    &out,
                    &[
                        hop["session"].as_str().unwrap().into(),
                        hop["parent_session"].as_str().unwrap().into(),
                    ],
                );
                let logical = json!([
                    hop["received_uuid"],
                    hop["received_turn_index"],
                    hop["edit_turn_index"],
                    hop["parent_sent_turn_index"],
                    stored_counts(root)
                ]);
                if let Some(expected) = &expected {
                    assert_eq!(&logical, expected);
                } else {
                    expected = Some(logical);
                }
            }
            println!(
                "N1 sender: claude={claude} received_first={received_first} full/split/legacy equal"
            );
        }
    }
}

#[test]
fn legacy_recovery_binds_repeated_events_by_unique_whole_sequence() {
    let mut fixture: Value =
        serde_json::from_str(include_str!("fixtures/native-upgrade/codex-full.json")).unwrap();
    let name = "receiver.codex.jsonl";
    let mut raw: Vec<String> = fixture["files"][name]
        .as_str()
        .unwrap()
        .lines()
        .map(|l| l.to_string() + "\n")
        .collect();
    raw.insert(2, raw[1].clone());
    let raw = raw.concat();
    fixture["files"][name] = json!(raw);
    let old_id = fixture["cursors"][name]["tape_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut normalized: Vec<String> = fixture["tapes"][&old_id]
        .as_str()
        .unwrap()
        .lines()
        .map(|l| l.to_string() + "\n")
        .collect();
    normalized.insert(2, normalized[1].clone());
    let normalized = normalized.concat();
    let new_id = format!("{:x}", Sha256::digest(normalized.as_bytes()));
    fixture["tapes"].as_object_mut().unwrap().remove(&old_id);
    fixture["tapes"][&new_id] = json!(normalized);
    fixture["cursors"][name]["tape_id"] = json!(new_id);
    let guard = raw.len().saturating_sub(512);
    fixture["cursors"][name]["byte_cursor"] = json!(raw.len());
    fixture["cursors"][name]["cursor_guard"] = json!({"offset":guard,"len":raw.len()-guard,"hash":format!("{:x}",Sha256::digest(&raw.as_bytes()[guard..]))});
    for link in fixture["dispatch_links"].as_array_mut().unwrap() {
        if link[0] == old_id {
            link[0] = json!(new_id);
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    install_legacy_fixture(root, &fixture);
    let immutable = snapshots(root);
    let counts = stored_counts(root);
    for file in [name, "sender.codex.jsonl"] {
        let path = root.join(file);
        let result = cli(root, &["ingest", path.to_str().unwrap()], None);
        assert_eq!(result["failure_count"], 0);
        assert_eq!(result["native_recovery"]["sources"], 1);
    }
    let locator: Value = serde_json::from_slice(
        &fs::read(
            root.join("home/.engram/tapes/native-upgrade-v1")
                .join(format!("{new_id}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        locator["recovered"]["points"],
        json!([{"old_offset":5,"source_offset":5,"turn":2}])
    );
    let query = cli(root, &["explain", "--", TEXT], None);
    let hop = &query["dispatch_lineage"][0];
    assert_eq!(hop["received_turn_index"], 0);
    assert_eq!(hop["edit_turn_index"], 2);
    assert_eq!(hop["edit_event_offset"], 5);
    assert_public_chain(
        &query,
        &[new_id, hop["parent_session"].as_str().unwrap().into()],
    );
    assert_eq!(stored_counts(root), counts);
    for (name, bytes) in immutable {
        assert_eq!(
            bytes,
            fs::read(root.join("home/.engram/tapes").join(name)).unwrap()
        );
    }
    let path = root.join(name);
    let before = fs::read(cursor(root, &path)).unwrap();
    let repeat = cli(root, &["ingest", path.to_str().unwrap()], None);
    assert_eq!(repeat["skipped_unchanged"], 1);
    assert!(repeat.get("native_recovery").is_none());
    assert_eq!(before, fs::read(cursor(root, &path)).unwrap());
}

#[test]
fn legacy_recovery_excludes_foreign_session_candidates_but_rejects_current_mismatch() {
    for mismatched_current in [false, true] {
        let mut fixture: Value =
            serde_json::from_str(include_str!("fixtures/native-upgrade/claude-full.json")).unwrap();
        let name = "receiver.claude.jsonl";
        let current = fixture["cursors"][name]["tape_id"]
            .as_str()
            .unwrap()
            .to_string();
        let other: String = fixture["tapes"][&current]
            .as_str()
            .unwrap()
            .lines()
            .map(|l| {
                let mut row: Value = serde_json::from_str(l).unwrap();
                row["source"]["session_id"] = json!("unrelated-session");
                line(row)
            })
            .collect();
        let foreign = format!("{:x}", Sha256::digest(other.as_bytes()));
        fixture["tapes"][&foreign] = json!(other);
        if mismatched_current {
            fixture["cursors"][name]["tape_id"] = json!(foreign);
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        install_legacy_fixture(root, &fixture);
        let path = root.join(name);
        let immutable = snapshots(root);
        let counts = stored_counts(root);
        let before = fs::read(cursor(root, &path)).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(root)
            .env("HOME", root.join("home"))
            .args(["ingest", path.to_str().unwrap()])
            .output()
            .unwrap();
        if mismatched_current {
            assert!(!result.status.success());
            assert!(
                String::from_utf8_lossy(&result.stderr)
                    .contains("legacy session/tool mismatch in current segment")
            );
            assert_eq!(before, fs::read(cursor(root, &path)).unwrap());
            assert_eq!(snapshots(root).len(), immutable.len());
        } else {
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let result: Value = serde_json::from_slice(&result.stdout).unwrap();
            assert_eq!(result["failure_count"], 0);
            assert_eq!(result["native_recovery"]["tapes"], 1);
            let dir = root.join("home/.engram/tapes/native-upgrade-v1");
            assert!(dir.join(format!("{current}.json")).exists());
            assert!(!dir.join(format!("{foreign}.json")).exists());
            let locator: Value =
                serde_json::from_slice(&fs::read(dir.join(format!("{current}.json"))).unwrap())
                    .unwrap();
            assert_eq!(
                locator["recovered"]["points"],
                json!([{"old_offset":4,"source_offset":4,"turn":1}])
            );
            let repeat = cli(root, &["ingest", path.to_str().unwrap()], None);
            assert_eq!(repeat["skipped_unchanged"], 1);
            assert!(repeat.get("native_recovery").is_none());
        }
        assert_eq!(stored_counts(root), counts);
        for (name, bytes) in immutable {
            assert_eq!(
                bytes,
                fs::read(root.join("home/.engram/tapes").join(name)).unwrap()
            );
        }
    }
}
