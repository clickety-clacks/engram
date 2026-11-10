use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use engram::anchor::fingerprint_anchor_hashes;
use engram::index::SqliteIndex;
use engram::index::lineage::LINK_THRESHOLD_DEFAULT;
use engram::query::{ExplainTraversal, explain_by_anchor};
use engram::tape::adapters::{
    claude_jsonl_to_tape_jsonl, codex_jsonl_to_tape_jsonl, gemini_json_to_tape_jsonl,
    openclaw_jsonl_to_tape_jsonl,
};
use engram::tape::event::TapeEventData;
use engram::tape::parse_jsonl_events;

fn home_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").expect("HOME should be set"))
}

fn collect_winnow_anchors(events: &[engram::tape::TapeEventAt]) -> BTreeSet<String> {
    let mut anchors = BTreeSet::new();

    for item in events {
        match &item.event.data {
            TapeEventData::CodeRead(read) => {
                if let Some(text) = read.text.as_deref() {
                    anchors.extend(fingerprint_anchor_hashes(text));
                } else {
                    anchors.extend(
                        read.anchor_hashes
                            .iter()
                            .filter(|anchor| anchor.starts_with("winnow:"))
                            .cloned(),
                    );
                }
            }
            TapeEventData::CodeEdit(edit) => {
                if let Some(text) = edit.before_text.as_deref() {
                    anchors.extend(fingerprint_anchor_hashes(text));
                } else {
                    anchors.extend(
                        edit.before_anchor_hashes
                            .iter()
                            .filter(|anchor| anchor.starts_with("winnow:"))
                            .cloned(),
                    );
                }
                if let Some(text) = edit.after_text.as_deref() {
                    anchors.extend(fingerprint_anchor_hashes(text));
                } else {
                    anchors.extend(
                        edit.after_anchor_hashes
                            .iter()
                            .filter(|anchor| anchor.starts_with("winnow:"))
                            .cloned(),
                    );
                }
            }
            TapeEventData::SpanLink(_)
            | TapeEventData::Meta(_)
            | TapeEventData::Textual(_)
            | TapeEventData::Gap(_)
            | TapeEventData::Other { .. } => {}
        }
    }

    anchors
}

fn assert_live_sample_has_winnow_evidence<E, F>(label: &str, path: &Path, convert: F)
where
    E: std::fmt::Display,
    F: Fn(&str) -> Result<String, E>,
{
    assert!(
        path.exists(),
        "expected live sample to exist for {label}: {}",
        path.display()
    );

    let input = fs::read_to_string(path).expect("live sample should load");
    let normalized = convert(&input).unwrap_or_else(|err| {
        panic!(
            "adapter should parse {label} live sample {}: {err}",
            path.display()
        )
    });
    let events = parse_jsonl_events(&normalized).expect("normalized tape jsonl should parse");
    let winnow_anchors = collect_winnow_anchors(&events);

    assert!(
        !winnow_anchors.is_empty(),
        "expected {label} live sample to emit at least one winnow anchor"
    );

    let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
    index
        .ingest_tape_events(label, &events, LINK_THRESHOLD_DEFAULT)
        .expect("ingest should succeed");

    let evidence_rows: usize = winnow_anchors
        .iter()
        .map(|anchor| {
            index
                .evidence_for_anchor(anchor)
                .expect("evidence query should succeed")
                .len()
        })
        .sum();

    assert!(
        evidence_rows > 0,
        "expected {label} live sample to store evidence rows for winnow anchors"
    );

    let explain = explain_by_anchor(
        &index,
        &[winnow_anchors
            .iter()
            .next()
            .expect("sample anchor")
            .to_string()],
        ExplainTraversal::default(),
        true,
    )
    .expect("explain should succeed");
    assert!(
        !explain.direct.is_empty(),
        "expected {label} live sample explain query to return at least one direct hit"
    );
}

fn assert_live_sample_parses_without_crash<E, F>(label: &str, path: &Path, convert: F)
where
    E: std::fmt::Display,
    F: Fn(&str) -> Result<String, E>,
{
    assert!(
        path.exists(),
        "expected live sample to exist for {label}: {}",
        path.display()
    );

    let input = fs::read_to_string(path).expect("live sample should load");
    let normalized = convert(&input).unwrap_or_else(|err| {
        panic!(
            "adapter should parse {label} live sample {}: {err}",
            path.display()
        )
    });
    let events = parse_jsonl_events(&normalized).expect("normalized tape jsonl should parse");

    let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
    index
        .ingest_tape_events(label, &events, LINK_THRESHOLD_DEFAULT)
        .expect("ingest should succeed");
}

fn find_claude_live_sample() -> Option<PathBuf> {
    let projects_dir = home_dir().join(".claude/projects");
    let mut candidates = Vec::new();
    for project_entry in fs::read_dir(&projects_dir).ok()?.flatten() {
        let project_dir = project_entry.path();
        if !project_dir.is_dir() {
            continue;
        }
        for session_entry in fs::read_dir(&project_dir).ok()?.flatten() {
            let path = session_entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                candidates.push(path);
            }
        }
    }
    // Directory iteration order is not guaranteed, so sort for a deterministic
    // pick, and require the candidate to actually carry winnow evidence rather
    // than trusting whichever file happens to come first.
    candidates.sort();
    candidates.into_iter().find(|path| {
        let Ok(input) = fs::read_to_string(path) else {
            return false;
        };
        let Ok(normalized) = claude_jsonl_to_tape_jsonl(&input) else {
            return false;
        };
        let Ok(events) = parse_jsonl_events(&normalized) else {
            return false;
        };
        !collect_winnow_anchors(&events).is_empty()
    })
}

#[test]
#[ignore = "requires a live Claude Code session sample under ~/.claude/projects"]
fn claude_live_sample_yields_winnow_evidence_rows() {
    let path = find_claude_live_sample()
        .expect("expected at least one live Claude Code session sample under ~/.claude/projects/*/*.jsonl");
    assert_live_sample_has_winnow_evidence("claude-live", &path, claude_jsonl_to_tape_jsonl);
}

#[test]
#[ignore = "requires host-b live Codex sample"]
fn codex_live_sample_yields_winnow_evidence_rows() {
    let path = home_dir().join(
        ".codex/sessions/2025/11/03/rollout-2025-11-03T12-59-25-019a4b84-7c94-7783-a08b-fb4674e68b65.jsonl",
    );
    assert_live_sample_has_winnow_evidence("codex-live", &path, codex_jsonl_to_tape_jsonl);
}

#[test]
#[ignore = "requires host-b live OpenClaw sample"]
fn openclaw_live_sample_yields_winnow_evidence_rows() {
    let path = home_dir().join(
        "shared-workspace/shared/openclaw-sessions/218e302f-86a9-434c-a16c-3bed54fd7a81.jsonl",
    );
    assert_live_sample_has_winnow_evidence("openclaw-live", &path, openclaw_jsonl_to_tape_jsonl);
}

#[test]
#[ignore = "requires host-b live Gemini sample"]
fn gemini_live_sample_parses_without_crash() {
    let path = home_dir().join(
        ".gemini/tmp/8958be08858fa3e266aa67bcb40164f9dea01c5a74b39dde59cb3fbb5a48b650/chats/session-2026-03-10T01-32-029b7052.json",
    );
    assert_live_sample_parses_without_crash("gemini-live", &path, gemini_json_to_tape_jsonl);
}
