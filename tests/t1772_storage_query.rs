use std::fs;

use engram::RuntimeContext;
use engram::anchor::fingerprint_windows;
use engram::index::SqliteIndex;
use engram::index::lineage::LINK_THRESHOLD_DEFAULT;
use engram::query::explain::{ExplainTraversal, explain_by_anchor};
use engram::query::format::open_query_indexes;
use engram::tape::event::{
    CodeEditEvent, CodeReadEvent, FileRange, SpanLinkEvent, TapeEvent, TapeEventAt, TapeEventData,
};
use rusqlite::Connection;

fn code(prefix: &str, lines: usize) -> String {
    (1..=lines)
        .map(|line| format!("fn {prefix}_{line}() {{ value_{line}(); }}\n"))
        .collect()
}

fn event(offset: u64, data: TapeEventData) -> TapeEventAt {
    TapeEventAt {
        offset,
        event: TapeEvent {
            timestamp: format!("2026-07-29T00:00:{offset:02}Z"),
            data,
        },
    }
}

#[test]
fn feature_composite_span_and_tombstone_modes_are_explicit() {
    let index = SqliteIndex::open_in_memory().expect("schema v4");
    let read_text = code("read", 24);
    let deleted_text = code("deleted", 24);
    let read_window = fingerprint_windows(&read_text).remove(0);
    let deleted_window = fingerprint_windows(&deleted_text).remove(0);
    index
        .ingest_tape_events(
            "tape",
            &[
                event(
                    1,
                    TapeEventData::CodeRead(CodeReadEvent {
                        file: "src/lib.rs".into(),
                        range: FileRange { start: 1, end: 24 },
                        text: Some(read_text),
                        anchor_hashes: Vec::new(),
                    }),
                ),
                event(
                    2,
                    TapeEventData::CodeEdit(CodeEditEvent {
                        file: "src/deleted.rs".into(),
                        before_range: Some(FileRange { start: 10, end: 33 }),
                        after_range: None,
                        before_text: Some(deleted_text),
                        after_text: None,
                        before_hash: None,
                        after_hash: None,
                        before_anchor_hashes: Vec::new(),
                        after_anchor_hashes: Vec::new(),
                        similarity: None,
                    }),
                ),
                event(
                    3,
                    TapeEventData::SpanLink(SpanLinkEvent {
                        from_file: "src/a.rs".into(),
                        from_range: FileRange { start: 1, end: 2 },
                        to_file: "src/b.rs".into(),
                        to_range: FileRange { start: 9, end: 10 },
                        note: Some("agent lineage".into()),
                    }),
                ),
            ],
            LINK_THRESHOLD_DEFAULT,
        )
        .expect("ingest");

    let by_feature = explain_by_anchor(
        &index,
        &read_window.features,
        ExplainTraversal::default(),
        false,
    )
    .expect("feature query");
    assert_eq!(by_feature.direct.len(), 1);
    assert!(by_feature.touched_anchors.contains(&read_window.anchor));

    let by_composite = explain_by_anchor(
        &index,
        std::slice::from_ref(&read_window.anchor),
        ExplainTraversal::default(),
        false,
    )
    .expect("composite query");
    assert_eq!(by_composite.direct.len(), 1);

    let by_span = explain_by_anchor(
        &index,
        &["span:src/a.rs:1-2".into()],
        ExplainTraversal::default(),
        false,
    )
    .expect("span query");
    assert!(by_span.direct.is_empty());
    assert_eq!(by_span.lineage.len(), 1);

    let tombstones = index
        .tombstones_for_anchor(&deleted_window.features[0])
        .expect("feature tombstone query");
    assert_eq!(tombstones.len(), 1);
    assert_eq!(tombstones[0].file_path, "src/deleted.rs");
}

#[cfg(unix)]
#[test]
fn primary_and_additional_query_stores_open_without_mutation_when_non_writable() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let primary = temp.path().join("primary.sqlite");
    let additional = temp.path().join("additional.sqlite");
    for path in [&primary, &additional] {
        drop(SqliteIndex::open_writer(path.to_str().unwrap()).expect("writer"));
    }
    let primary_before = fs::read(&primary).expect("primary bytes");
    let additional_before = fs::read(&additional).expect("additional bytes");
    let listing_before = fs::read_dir(temp.path())
        .expect("listing")
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();

    fs::set_permissions(&primary, fs::Permissions::from_mode(0o444)).expect("primary mode");
    fs::set_permissions(&additional, fs::Permissions::from_mode(0o444)).expect("additional mode");
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o555)).expect("dir mode");

    let context = RuntimeContext {
        config_path: temp.path().join("config.yml"),
        db_path: primary.clone(),
        tapes_dir: temp.path().join("tapes"),
        tape_lookup_dirs: Vec::new(),
        additional_stores: vec![additional.clone()],
        explain_default_limit: 10,
        peek_default_lines: 30,
        peek_default_before: 30,
        peek_default_after: 10,
        peek_grep_context: 5,
        metrics_enabled: true,
        metrics_log: temp.path().join("metrics.jsonl"),
        watch: None,
    };
    let indexes = open_query_indexes(&context).expect("strict readers");
    assert_eq!(indexes.len(), 2);
    drop(indexes);

    assert_eq!(fs::read(&primary).expect("primary after"), primary_before);
    assert_eq!(
        fs::read(&additional).expect("additional after"),
        additional_before
    );
    assert_eq!(
        fs::read_dir(temp.path())
            .expect("listing after")
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        listing_before
    );
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).expect("restore mode");
}

#[test]
fn schema_v4_exactly_separates_physical_windows_and_postings() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("index.sqlite");
    let text = code("wide", 48);
    let windows = fingerprint_windows(&text);
    {
        let index = SqliteIndex::open_writer(path.to_str().unwrap()).expect("writer");
        index
            .ingest_tape_events(
                "tape",
                &[event(
                    1,
                    TapeEventData::CodeRead(CodeReadEvent {
                        file: "src/lib.rs".into(),
                        range: FileRange { start: 1, end: 48 },
                        text: Some(text),
                        anchor_hashes: Vec::new(),
                    }),
                )],
                LINK_THRESHOLD_DEFAULT,
            )
            .expect("ingest");
    }
    let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("accounting reader");
    let window_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM evidence_windows", [], |row| {
            row.get(0)
        })
        .unwrap();
    let posting_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM evidence_features", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(window_count as usize, windows.len());
    assert_eq!(
        posting_count as usize,
        windows
            .iter()
            .map(|window| window.features.len())
            .sum::<usize>()
    );
}
