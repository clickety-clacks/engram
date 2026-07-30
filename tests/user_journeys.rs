use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use engram::anchor::fingerprint_windows;
use rusqlite::Connection;
use serde_json::{Value, json};

fn run_cli(repo: &Path, args: &[&str], home: &Path) -> Output {
    fs::create_dir_all(home).expect("sandboxed HOME");
    Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(repo)
        .args(args)
        .env("HOME", home)
        .output()
        .expect("compiled engram binary runs")
}

fn run_json(repo: &Path, args: &[&str], home: &Path) -> Value {
    let output = run_cli(repo, args, home);
    assert!(
        output.status.success(),
        "engram command should succeed for journey setup: args={args:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("engram emits JSON")
}

fn write_jsonl(path: &Path, rows: &[Value]) {
    let contents = rows
        .iter()
        .map(|row| serde_json::to_string(row).expect("fixture row serializes"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(path, contents).expect("Claude-format transcript fixture");
}

fn init_sandbox(temp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
    let home = temp.path().join("home");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("journey working directory");
    let initialized = run_json(&repo, &["init"], &home);
    assert_eq!(
        initialized["status"], "ok",
        "sandboxed repository should initialize"
    );
    (home, repo)
}

fn numbered_rust_lines(prefix: &str, count: usize) -> String {
    (1..=count)
        .map(|line| format!("    let {prefix}_{line:02} = input.{prefix}_{line:02} + {line};\n"))
        .collect()
}

fn claude_edit_rows(
    session_id: &str,
    timestamp: &str,
    tool_id: &str,
    file_path: &str,
    old_string: &str,
    new_string: &str,
) -> Vec<Value> {
    vec![
        json!({
            "type": "assistant",
            "session_id": session_id,
            "timestamp": timestamp,
            "message": {
                "model": "claude-fable-5",
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": tool_id,
                    "name": "Edit",
                    "input": {
                        "file_path": file_path,
                        "old_string": old_string,
                        "new_string": new_string
                    }
                }]
            }
        }),
        json!({
            "type": "user",
            "session_id": session_id,
            "timestamp": "2026-07-25T18:00:01Z",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": tool_id,
                    "content": "The file was updated successfully."
                }]
            }
        }),
    ]
}

fn tape_ids(repo: &Path, home: &Path) -> Vec<String> {
    run_json(repo, &["tapes"], home)["tapes"]
        .as_array()
        .expect("tapes array")
        .iter()
        .map(|tape| tape["tape_id"].as_str().expect("tape id").to_string())
        .collect()
}

fn shown_events(repo: &Path, home: &Path, tape_id: &str) -> Vec<Value> {
    run_json(repo, &["show", tape_id], home)["events"]
        .as_array()
        .expect("shown tape events")
        .clone()
}

fn session_has_touch(explain: &Value, session_id: &str, kind: &str) -> bool {
    explain["sessions"]
        .as_array()
        .expect("explain sessions")
        .iter()
        .find(|session| session["session_id"] == session_id)
        .is_some_and(|session| {
            session["confidence"].as_f64().unwrap_or(0.0) > 0.0
                && session["touches"]
                    .as_array()
                    .is_some_and(|touches| touches.iter().any(|touch| touch["kind"] == kind))
        })
}

fn has_warranted_cross_state_edge(
    explain: &Value,
    source_anchors: &HashSet<String>,
    destination_anchors: &HashSet<String>,
) -> bool {
    explain["lineage"]
        .as_array()
        .expect("lineage array")
        .iter()
        .any(|edge| {
            let from = edge["from_anchor"].as_str().unwrap_or("");
            let to = edge["to_anchor"].as_str().unwrap_or("");
            let confidence = edge["confidence"].as_f64().unwrap_or(0.0);
            from != to
                && confidence >= 0.30
                && confidence < 1.0
                && ((source_anchors.contains(from) && destination_anchors.contains(to))
                    || (source_anchors.contains(to) && destination_anchors.contains(from)))
        })
}

#[test]
fn uj1_messages_and_tool_outputs_are_raw_context_not_direct_evidence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (home, repo) = init_sandbox(&temp);
    fs::create_dir_all(repo.join("src")).expect("source directory");

    let snippet_a = format!(
        "pub fn assemble_widget(input: &WidgetInput) -> Widget {{\n{}    Widget::new()\n}}\n",
        numbered_rust_lines("widget", 28)
    );
    let snippet_b = format!(
        "pub fn calibrate_gadget(input: &GadgetInput) -> Gadget {{\n{}    Gadget::ready()\n}}\n",
        numbered_rust_lines("gadget", 28)
    );
    fs::write(repo.join("src/widget.rs"), &snippet_a).expect("materialized widget edit");
    fs::write(repo.join("src/gadget.rs"), &snippet_b).expect("materialized gadget edit");

    let hinted = repo.join(".claude/projects/uj1");
    fs::create_dir_all(&hinted).expect("path-hinted Claude transcript directory");
    let discussion_path = hinted.join("orchestrator.jsonl");
    write_jsonl(
        &discussion_path,
        &[
            json!({
                "type": "assistant",
                "session_id": "uj1-orchestrator",
                "timestamp": "2026-07-25T17:00:00Z",
                "message": {
                    "model": "claude-fable-5",
                    "role": "assistant",
                    "content": [{
                        "type": "text",
                        "text": format!("Use this exact widget implementation:\n{snippet_a}")
                    }]
                }
            }),
            json!({
                "type": "assistant",
                "session_id": "uj1-orchestrator",
                "timestamp": "2026-07-25T17:00:01Z",
                "message": {
                    "model": "claude-fable-5",
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_uj1_gadget",
                        "name": "Bash",
                        "input": {"command": "render approved gadget snippet"}
                    }]
                }
            }),
            json!({
                "type": "user",
                "session_id": "uj1-orchestrator",
                "timestamp": "2026-07-25T17:00:02Z",
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_uj1_gadget",
                        "content": snippet_b
                    }]
                }
            }),
        ],
    );

    let implementation_path = hinted.join("implementation.jsonl");
    let mut implementation_rows = claude_edit_rows(
        "uj1-implementation",
        "2026-07-25T17:10:00Z",
        "toolu_uj1_widget_edit",
        "src/widget.rs",
        "pub fn assemble_widget() {}\n",
        &snippet_a,
    );
    implementation_rows.extend(claude_edit_rows(
        "uj1-implementation",
        "2026-07-25T17:10:02Z",
        "toolu_uj1_gadget_edit",
        "src/gadget.rs",
        "pub fn calibrate_gadget() {}\n",
        &snippet_b,
    ));
    write_jsonl(&implementation_path, &implementation_rows);

    let ingest = run_json(
        &repo,
        &[
            "ingest",
            discussion_path.to_string_lossy().as_ref(),
            implementation_path.to_string_lossy().as_ref(),
        ],
        &home,
    );
    assert_eq!(
        ingest["imported_tapes"], 2,
        "both real Claude transcripts should ingest"
    );

    let discussion_tape = tape_ids(&repo, &home)
        .into_iter()
        .find(|tape_id| {
            let events = shown_events(&repo, &home, tape_id);
            events.iter().any(|event| event["k"] == "msg.out")
                && events.iter().any(|event| event["k"] == "tool.result")
                && !events.iter().any(|event| event["k"] == "code.edit")
        })
        .expect("discussion-only tape");
    let implementation_tape = tape_ids(&repo, &home)
        .into_iter()
        .find(|tape_id| {
            shown_events(&repo, &home, tape_id)
                .iter()
                .any(|event| event["k"] == "code.edit")
        })
        .expect("implementation tape");

    let widget = run_json(&repo, &["explain", "src/widget.rs:1-31"], &home);
    let gadget = run_json(&repo, &["explain", "src/gadget.rs:1-31"], &home);
    let widget_edit = session_has_touch(&widget, &implementation_tape, "edit");
    let gadget_edit = session_has_touch(&gadget, &implementation_tape, "edit");
    let discussion_is_direct = widget["sessions"]
        .as_array()
        .expect("widget sessions")
        .iter()
        .chain(gadget["sessions"].as_array().expect("gadget sessions"))
        .any(|session| session["session_id"] == discussion_tape);
    let discussion_events = shown_events(&repo, &home, &discussion_tape);
    let raw_message_and_tool_are_preserved = discussion_events
        .iter()
        .any(|event| event["k"] == "msg.out")
        && discussion_events
            .iter()
            .any(|event| event["k"] == "tool.result");

    assert!(
        widget_edit && gadget_edit && !discussion_is_direct && raw_message_and_tool_are_preserved,
        "v4 expectation: text-backed edits are direct evidence while discussion message/tool rows \
         remain raw context only; widget_edit={widget_edit} gadget_edit={gadget_edit} \
         discussion_is_direct={discussion_is_direct} \
         raw_message_and_tool_are_preserved={raw_message_and_tool_are_preserved}"
    );
}

#[test]
fn uj2_bidirectional_edit_lineage() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (home, repo) = init_sandbox(&temp);
    fs::create_dir_all(repo.join("src")).expect("source directory");
    fs::create_dir_all(repo.join("snapshots")).expect("snapshot directory");

    let unchanged = (1..=20)
        .map(|line| format!("    accumulator += stable_value_{line:02};\n"))
        .collect::<String>();
    let before_changed = (21..=30)
        .map(|line| format!("    accumulator += legacy_value_{line:02};\n"))
        .collect::<String>();
    let after_changed = (21..=30)
        .map(|line| format!("    accumulator += current_value_{line:02} * 2;\n"))
        .collect::<String>();
    let before = format!(
        "pub fn compute_total() -> i64 {{\n{unchanged}{before_changed}    accumulator\n}}\n"
    );
    let after = format!(
        "pub fn compute_total() -> i64 {{\n{unchanged}{after_changed}    accumulator\n}}\n"
    );
    fs::write(repo.join("snapshots/before.rs"), &before).expect("before-state file");
    fs::write(repo.join("src/current.rs"), &after).expect("after-state file");

    let hinted = repo.join(".claude/projects/uj2");
    fs::create_dir_all(&hinted).expect("path-hinted Claude transcript directory");
    let transcript = hinted.join("edit.jsonl");
    write_jsonl(
        &transcript,
        &claude_edit_rows(
            "uj2-editor",
            "2026-07-25T18:00:00Z",
            "toolu_uj2_edit",
            "src/current.rs",
            &before,
            &after,
        ),
    );
    let ingest = run_json(
        &repo,
        &["ingest", transcript.to_string_lossy().as_ref()],
        &home,
    );
    assert_eq!(
        ingest["imported_tapes"], 1,
        "the real Claude edit transcript should ingest"
    );

    let explain_before = run_json(
        &repo,
        &["explain", "snapshots/before.rs", "--min-confidence", "0.30"],
        &home,
    );
    let explain_after = run_json(
        &repo,
        &["explain", "src/current.rs", "--min-confidence", "0.30"],
        &home,
    );
    let before_windows = fingerprint_windows(&before);
    let after_windows = fingerprint_windows(&after);
    let before_anchors = before_windows
        .iter()
        .map(|window| window.anchor.clone())
        .collect::<HashSet<_>>();
    let after_anchors = after_windows
        .iter()
        .map(|window| window.anchor.clone())
        .collect::<HashSet<_>>();
    let forward = has_warranted_cross_state_edge(&explain_before, &before_anchors, &after_anchors);
    let backward = has_warranted_cross_state_edge(&explain_after, &before_anchors, &after_anchors);

    let conn = Connection::open(repo.join(".engram/index.sqlite")).expect("open v4 index");
    let mut stmt = conn
        .prepare(
            "SELECT pair_ordinal, from_window_ordinal, to_window_ordinal
             FROM edges
             WHERE source_kind = 'edit'
             ORDER BY pair_ordinal",
        )
        .expect("physical edge query");
    let physical_pairs = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .expect("physical edge rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("physical pairs");
    let expected_pairs = before_windows.len().max(after_windows.len());
    let physical_identity_is_complete = physical_pairs.len() == expected_pairs
        && physical_pairs.iter().enumerate().all(
            |(expected_ordinal, (pair_ordinal, from_ordinal, to_ordinal))| {
                *pair_ordinal == expected_ordinal as i64 && *from_ordinal >= 0 && *to_ordinal >= 0
            },
        );

    assert!(
        forward && backward && physical_identity_is_complete,
        "v4 expectation: every proportional physical pair has stable identity and semantic lineage \
         traverses in both directions with warranted confidence in [0.30, 1.0); \
         old_to_new={forward} new_to_old={backward} \
         physical_pairs={} expected_pairs={expected_pairs}",
        physical_pairs.len()
    );
}

#[test]
fn uj3_positive_adapter_detection_no_foreign_acceptance() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (home, repo) = init_sandbox(&temp);
    let plain = repo.join("incoming");
    fs::create_dir_all(&plain).expect("plain transcript drop directory");

    let before = format!(
        "pub fn parse_document() -> Document {{\n{}    Document::empty()\n}}\n",
        numbered_rust_lines("legacy", 28)
    );
    let after = format!(
        "pub fn parse_document() -> Document {{\n{}    Document::parsed()\n}}\n",
        numbered_rust_lines("parsed", 28)
    );
    let valid_path = plain.join("session.jsonl");
    write_jsonl(
        &valid_path,
        &claude_edit_rows(
            "uj3-valid-session",
            "2026-07-25T19:00:00Z",
            "toolu_uj3_edit",
            "src/document.rs",
            &before,
            &after,
        ),
    );
    let foreign_path = plain.join("records.jsonl");
    write_jsonl(
        &foreign_path,
        &[
            json!({"customer": 17, "total": 42.50, "tags": ["priority", "west"]}),
            json!({"customer": 23, "total": 11.00, "tags": ["trial"]}),
            json!({"customer": 41, "total": 98.25, "tags": []}),
        ],
    );

    let ingest = run_json(
        &repo,
        &[
            "ingest",
            valid_path.to_string_lossy().as_ref(),
            foreign_path.to_string_lossy().as_ref(),
        ],
        &home,
    );
    let ids = tape_ids(&repo, &home);
    let shown = ids
        .iter()
        .map(|tape_id| shown_events(&repo, &home, tape_id))
        .collect::<Vec<_>>();
    let valid_has_edit = shown
        .iter()
        .any(|events| events.iter().any(|event| event["k"] == "code.edit"));
    let has_meta_only_tape = shown.iter().any(|events| {
        !events.is_empty()
            && events
                .iter()
                .all(|event| event["k"].as_str() == Some("meta"))
    });
    let foreign_rejected_without_tape =
        ingest["skipped_non_transcript"].as_u64().unwrap_or(0) >= 1 && ids.len() == 1;

    assert!(
        valid_has_edit && foreign_rejected_without_tape && !has_meta_only_tape,
        "product expectation: positive adapter detection must preserve the Claude code.edit, explicitly reject the foreign JSONL without creating a tape, and create no meta-only tape; valid_has_edit={valid_has_edit} skipped_non_transcript={} tape_count={} has_meta_only_tape={has_meta_only_tape}",
        ingest["skipped_non_transcript"],
        ids.len()
    );
}

#[test]
fn uj4_gc_preserves_immutable_tapes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (home, repo) = init_sandbox(&temp);
    fs::create_dir_all(repo.join("src")).expect("source directory");

    let hinted = repo.join(".claude/projects/uj4");
    fs::create_dir_all(&hinted).expect("path-hinted Claude transcript directory");
    let first_path = hinted.join("first.jsonl");
    let second_path = hinted.join("second.jsonl");
    let first_text = format!(
        "pub fn retain_first_tape() {{\n{}    persist();\n}}\n",
        numbered_rust_lines("first", 28)
    );
    let second_text = format!(
        "pub fn retain_second_tape() {{\n{}    persist();\n}}\n",
        numbered_rust_lines("second", 28)
    );
    fs::write(repo.join("src/first.rs"), &first_text).expect("first materialized edit");
    fs::write(repo.join("src/second.rs"), &second_text).expect("second materialized edit");
    write_jsonl(
        &first_path,
        &claude_edit_rows(
            "uj4-first",
            "2026-07-25T20:00:00Z",
            "toolu_uj4_first",
            "src/first.rs",
            "pub fn retain_first_tape() {}\n",
            &first_text,
        ),
    );
    write_jsonl(
        &second_path,
        &claude_edit_rows(
            "uj4-second",
            "2026-07-25T20:01:00Z",
            "toolu_uj4_second",
            "src/second.rs",
            "pub fn retain_second_tape() {}\n",
            &second_text,
        ),
    );
    let ingest = run_json(
        &repo,
        &[
            "ingest",
            first_path.to_string_lossy().as_ref(),
            second_path.to_string_lossy().as_ref(),
        ],
        &home,
    );
    assert_eq!(
        ingest["imported_tapes"], 2,
        "two real Claude transcripts should produce immutable tapes"
    );

    let tapes_dir = repo.join(".engram/tapes");
    let before = fs::read_dir(&tapes_dir)
        .expect("resolved tapes directory")
        .map(|entry| entry.expect("tape directory entry").path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    assert!(
        before.len() >= 2,
        "journey setup should create at least two tapes"
    );
    fs::remove_file(repo.join(".engram/index.sqlite")).expect("delete disposable derived index");

    let gc = run_json(&repo, &["gc"], &home);
    let all_tapes_preserved = before.iter().all(|path| path.exists());
    let reports_zero_deleted = gc["deleted_count"].as_u64() == Some(0);

    assert!(
        all_tapes_preserved && reports_zero_deleted,
        "product expectation: immutable tapes must survive gc when the disposable derived index is absent, and gc must report zero deleted tapes; all_tapes_preserved={all_tapes_preserved} deleted_count={}",
        gc["deleted_count"]
    );
}
