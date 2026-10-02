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

fn set_readonly(path: &std::path::Path, readonly: bool) {
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_readonly(readonly);
    fs::set_permissions(path, permissions).expect("permissions");
}

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

#[test]
fn live_primary_and_frozen_additional_query_stores_open_without_index_file_mutation() {
    // Live readers remain mode=ro and may need parent-directory access for
    // SQLite's -shm file; this checks that readonly database files stay unchanged.
    let temp = tempfile::tempdir().expect("tempdir");
    let active_dir = temp.path().join("active");
    let frozen_dir = temp.path().join("frozen");
    fs::create_dir_all(&active_dir).expect("active dir");
    fs::create_dir_all(&frozen_dir).expect("frozen dir");
    let primary = active_dir.join("primary.sqlite");
    let additional = frozen_dir.join("additional.sqlite");
    for path in [&primary, &additional] {
        drop(SqliteIndex::open_writer(path.to_str().unwrap()).expect("writer"));
    }
    let primary_before = fs::read(&primary).expect("primary bytes");
    let additional_before = fs::read(&additional).expect("additional bytes");
    let listing_before = fs::read_dir(&frozen_dir)
        .expect("listing")
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();

    set_readonly(&primary, true);
    set_readonly(&additional, true);

    let context = RuntimeContext {
        config_path: temp.path().join("config.yml"),
        db_path: primary.clone(),
        tapes_dir: temp.path().join("tapes"),
        frozen_stores: vec![additional.clone()],
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
        fs::read_dir(&frozen_dir)
            .expect("listing after")
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        listing_before
    );
    set_readonly(&primary, false);
    set_readonly(&additional, false);
}

#[test]
fn live_reader_in_read_only_directory_reports_unavailability_and_fix() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let live_dir = temp.path().join("live");
    fs::create_dir(&live_dir).expect("live directory");
    let primary = live_dir.join("primary.sqlite");
    let legacy_writer = Connection::open(&primary).expect("legacy WAL writer");
    legacy_writer
        .execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA user_version = 4; CREATE TABLE tapes (tape_id TEXT PRIMARY KEY);",
        )
        .expect("seed legacy WAL store");
    drop(legacy_writer);
    assert!(!std::path::PathBuf::from(format!("{}-wal", primary.display())).exists());
    assert!(!std::path::PathBuf::from(format!("{}-shm", primary.display())).exists());
    let before = fs::read(&primary).expect("primary bytes");
    set_readonly(&primary, true);
    set_readonly(&live_dir, true);

    let context = RuntimeContext {
        config_path: temp.path().join("config.yml"),
        db_path: primary.clone(),
        tapes_dir: temp.path().join("tapes"),
        frozen_stores: Vec::new(),
        tape_lookup_dirs: Vec::new(),
        additional_stores: Vec::new(),
        explain_default_limit: 10,
        peek_default_lines: 30,
        peek_default_before: 30,
        peek_default_after: 10,
        peek_grep_context: 5,
        metrics_enabled: true,
        metrics_log: temp.path().join("metrics.jsonl"),
        watch: None,
    };
    let result = open_query_indexes(&context);
    set_readonly(&live_dir, false);
    set_readonly(&primary, false);

    let error = result
        .err()
        .expect("live reader should require -shm access");
    assert_eq!(error.code, "reader_unavailable");
    assert!(error.message.contains(&primary.display().to_string()));
    assert!(error.message.contains("owner-side Engram writer"));
    assert!(error.message.contains("keep the query read-only"));
    assert!(!error.message.contains("grant SQLite write access"));
    assert!(error.message.contains("frozen_stores"));
    assert_eq!(fs::read(&primary).expect("primary after"), before);
}

#[test]
fn live_reader_first_use_and_restart_survive_wal_sidecar_lifecycle() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let source_dir = temp.path().join("source");
    let copy_dir = temp.path().join("isolated-copy");
    fs::create_dir(&source_dir).expect("source directory");
    fs::create_dir(&copy_dir).expect("copy directory");
    let source_path = source_dir.join("index.sqlite");
    let copy_path = copy_dir.join("index.sqlite");
    let sidecar = |path: &std::path::Path, suffix: &str| {
        std::path::PathBuf::from(format!("{}{suffix}", path.display()))
    };

    // Copy a committed WAL database while its source writer remains open, but
    // omit the transient WAL index. All subsequent changes happen on the copy.
    let source_writer =
        SqliteIndex::open_writer(source_path.to_str().unwrap()).expect("source writer");
    source_writer
        .ingest_tape_events(
            "before-copy",
            &[event(
                1,
                TapeEventData::CodeRead(CodeReadEvent {
                    file: "src/example.rs".into(),
                    range: FileRange { start: 1, end: 1 },
                    text: Some("fn before_copy() {}\n".into()),
                    anchor_hashes: Vec::new(),
                }),
            )],
            LINK_THRESHOLD_DEFAULT,
        )
        .expect("seed source WAL");
    let source_wal = sidecar(&source_path, "-wal");
    let source_shm = sidecar(&source_path, "-shm");
    assert!(source_wal.exists(), "source writer should retain a live WAL");
    assert!(source_shm.exists(), "source writer should have a WAL index");
    let wal_bytes = fs::metadata(&source_wal).expect("source WAL metadata").len();
    assert!(wal_bytes > 0, "source WAL should contain committed frames");
    fs::copy(&source_path, &copy_path).expect("copy main database");
    let copy_wal = sidecar(&copy_path, "-wal");
    let copy_shm = sidecar(&copy_path, "-shm");
    fs::copy(&source_wal, &copy_wal).expect("copy WAL without its index");
    assert!(!copy_shm.exists(), "isolated copy omits transient WAL index");

    // The first normal read-only open has neither a usable WAL index nor
    // permission to create one. Capture its exact outcome, then let an owner
    // writer establish sidecars on this disposable copy.
    set_readonly(&copy_path, true);
    set_readonly(&copy_wal, true);
    set_readonly(&copy_dir, true);
    let first_read = SqliteIndex::open_reader_mode(
        copy_path.to_str().unwrap(),
        engram::index::ReaderMode::Live,
    )
        .map(|reader| {
            reader.with_read_transaction(|conn| {
                conn.query_row("SELECT COUNT(*) FROM tapes", [], |row| row.get::<_, i64>(0))
            })
        })
        .and_then(|rows| rows)
        .map_err(|error| error.to_string());

    set_readonly(&copy_dir, false);
    set_readonly(&copy_path, false);
    set_readonly(&copy_wal, false);
    let copy_writer = SqliteIndex::open_writer(copy_path.to_str().unwrap()).expect("copy writer");
    assert!(
        copy_wal.exists() && copy_shm.exists(),
        "owner writer should establish WAL sidecars"
    );

    set_readonly(&copy_path, true);
    set_readonly(&copy_wal, true);
    set_readonly(&copy_shm, true);
    set_readonly(&copy_dir, true);
    let mut concurrent_rows = None;
    let concurrent_reader = SqliteIndex::open_reader_mode(
        copy_path.to_str().unwrap(),
        engram::index::ReaderMode::Live,
    )
    .map_err(|error| error.to_string())
    .and_then(|reader| {
        let before = reader
            .with_read_transaction(|conn| {
                conn.query_row("SELECT COUNT(*) FROM tapes", [], |row| row.get::<_, i64>(0))
            })
            .map_err(|error| error.to_string())?;
        copy_writer
            .ingest_tape_events(
                "writer-while-reader-attached",
                &[event(
                    2,
                    TapeEventData::CodeRead(CodeReadEvent {
                        file: "src/example.rs".into(),
                        range: FileRange { start: 2, end: 2 },
                        text: Some("fn writer_while_reader_attached() {}\n".into()),
                        anchor_hashes: Vec::new(),
                    }),
                )],
                LINK_THRESHOLD_DEFAULT,
            )
            .map_err(|error| error.to_string())?;
        concurrent_rows = Some((
            before,
            reader
                .with_read_transaction(|conn| {
                    conn.query_row("SELECT COUNT(*) FROM tapes", [], |row| row.get::<_, i64>(0))
                })
                .map_err(|error| error.to_string())?,
        ));
        Ok(())
    });

    // The owner writer checkpoints on close but preserves both sidecars for a
    // restarted read-only client with no directory write permission.
    set_readonly(&copy_dir, false);
    set_readonly(&copy_path, false);
    set_readonly(&copy_wal, false);
    set_readonly(&copy_shm, false);
    drop(copy_writer);
    let sidecars_after_writer_close = (copy_wal.exists(), copy_shm.exists());
    set_readonly(&copy_path, true);
    if copy_wal.exists() {
        set_readonly(&copy_wal, true);
    }
    if copy_shm.exists() {
        set_readonly(&copy_shm, true);
    }
    set_readonly(&copy_dir, true);
    let restart_read = SqliteIndex::open_reader_mode(
        copy_path.to_str().unwrap(),
        engram::index::ReaderMode::Live,
    )
    .map(|reader| {
        reader.with_read_transaction(|conn| {
            conn.query_row("SELECT COUNT(*) FROM tapes", [], |row| row.get::<_, i64>(0))
        })
    })
    .and_then(|rows| rows)
    .map_err(|error| error.to_string());
    set_readonly(&copy_dir, false);
    set_readonly(&copy_path, false);
    if copy_wal.exists() {
        set_readonly(&copy_wal, false);
    }
    if copy_shm.exists() {
        set_readonly(&copy_shm, false);
    }

    println!(
        "isolated WAL reproduction: source_wal_bytes={wal_bytes}, copied_shm=false, first_read={first_read:?}, writer_created_sidecars=true, concurrent_reader={concurrent_reader:?}, concurrent_rows={concurrent_rows:?}, sidecars_after_writer_close={sidecars_after_writer_close:?}, restart_read={restart_read:?}"
    );
    assert!(
        first_read.as_ref().is_err_and(|error| error == "unable to open database file"),
        "a copied WAL missing -shm in a read-only parent should report the known first-use limitation"
    );
    assert!(
        concurrent_reader.is_ok() && concurrent_rows == Some((1, 2)),
        "an existing WAL/SHM pair must support coherent read-only access alongside a live writer"
    );
    assert_eq!(
        sidecars_after_writer_close,
        (true, true),
        "Engram writers must retain readable WAL/SHM sidecars for later read-only clients"
    );
    assert!(
        restart_read.is_ok(),
        "a restarted read-only client must work after the owner writer closes"
    );
    drop(source_writer);
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
