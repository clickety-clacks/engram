use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use engram::dispatch::extract_dispatch_links_from_transcript;
use engram::index::SqliteIndex;
use engram::index::lineage::LINK_THRESHOLD_DEFAULT;
use engram::proof::t1772::{
    self, BASELINE_BYTES, CANDIDATE_BASE, DISPATCH_JSONL_SHA256, DISPATCH_ROWS,
    EXPECTED_EDIT_EDGES, EXPECTED_EDIT_WINDOWS, EXPECTED_EVENTS, EXPECTED_EVIDENCE_FEATURES,
    EXPECTED_EVIDENCE_WINDOWS, EXPECTED_INPUTS, EXPECTED_LEGACY_TOMBSTONE_KEYS,
    EXPECTED_READ_WINDOWS, EXPECTED_SEMANTIC_EDGES, EXPECTED_TOMBSTONE_FEATURES,
    EXPECTED_TOMBSTONE_WINDOWS, INPUT_ROOT, MANIFEST_NAME, MANIFEST_SHA256, PROOF_ROOT,
    ProofResult, canonical_json_lf, hex_digest, require_exact_path, sha256_file,
    suspend_for_controller, write_canonical_json, write_canonical_jsonl,
};
use engram::query::format::derive_anchor_candidates;
use engram::store::tapes::read_tape_content;
use engram::tape::event::parse_jsonl_events;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const BUILD_REVISION: &str = match option_env!("T1772_BUILD_REVISION") {
    Some(value) => value,
    None => "UNSET",
};

#[derive(Debug, Parser)]
#[command(name = "t1772-p0-runner")]
struct Args {
    #[arg(long)]
    candidate_base: String,
    #[arg(long)]
    input_root: PathBuf,
    #[arg(long)]
    oracle_root: PathBuf,
    #[arg(long)]
    proof_root: PathBuf,
    #[arg(long)]
    tape_root: PathBuf,
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    candidate_binary: PathBuf,
    #[arg(long)]
    baseline_binary: PathBuf,
    #[arg(long)]
    baseline_database: PathBuf,
    #[arg(long)]
    baseline_database_sha256: String,
    #[arg(long)]
    comparator_receipt: PathBuf,
    #[arg(long)]
    comparator_receipt_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Counts {
    input_tapes: u64,
    normalized_events: u64,
    read_windows: u64,
    edit_windows: u64,
    evidence_windows: u64,
    evidence_features: u64,
    tombstone_windows: u64,
    tombstone_features: u64,
    edit_edges: u64,
    span_link_edges: u64,
    semantic_edit_edges: u64,
    legacy_tombstone_keys: u64,
    registered_tapes: u64,
    dispatch_links: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DbReport {
    path: String,
    bytes: u64,
    page_size: u64,
    page_count: u64,
    freelist_count: u64,
    schema_sha256: String,
    dbstat: Vec<DbStatRow>,
    logical_sha256: BTreeMap<String, String>,
    counts: Counts,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DbStatRow {
    name: String,
    pages: u64,
    bytes: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("t1772 runner failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> ProofResult<()> {
    let args = Args::parse();
    validate_args(&args)?;

    // The reviewed controller observes this post-exec stop, records csops, and
    // resumes with Darwin SIGCONT 19. No proof output is opened before resume.
    suspend_for_controller()?;
    let comparator = engram::proof::baseline_custody::read_comparator_receipt(
        &args.baseline_database,
        &args.baseline_database_sha256,
        &args.comparator_receipt,
        &args.comparator_receipt_sha256,
    )?;

    let runner_root = args.proof_root.join("runner");
    fs::create_dir(&runner_root)?;
    let started = Instant::now();

    let tape_ids = read_tape_ids(&args.input_root.join("p0-tape-ids.txt"))?;
    if tape_ids.len() as u64 != EXPECTED_INPUTS {
        return Err(format!(
            "expected {EXPECTED_INPUTS} tape IDs, got {}",
            tape_ids.len()
        )
        .into());
    }

    let mut reports = Vec::new();
    for ordinal in 1..=2 {
        let rebuild_root = runner_root.join(format!("rebuild-{ordinal}"));
        fs::create_dir(&rebuild_root)?;
        let db_path = rebuild_root.join("index.sqlite");
        let rebuild_started = Instant::now();
        let event_count = build_database(&db_path, &args.tape_root, &tape_ids)?;
        let mut report = inspect_database(&db_path, &tape_ids, event_count)?;
        write_canonical_json(
            &rebuild_root.join("database-report.json"),
            &serde_json::to_value(&report)?,
        )?;
        write_canonical_json(
            &rebuild_root.join("timing.json"),
            &json!({"elapsed_milliseconds": rebuild_started.elapsed().as_millis()}),
        )?;
        report.path = db_path.to_string_lossy().into_owned();
        reports.push(report);
    }

    if reports[0].counts != reports[1].counts
        || reports[0].schema_sha256 != reports[1].schema_sha256
        || reports[0].logical_sha256 != reports[1].logical_sha256
    {
        return Err("the two independent rebuilds differ logically".into());
    }
    validate_counts(&reports[0].counts)?;
    if reports[0].bytes >= BASELINE_BYTES {
        return Err(format!(
            "candidate database {} is not below historical ceiling {BASELINE_BYTES}",
            reports[0].bytes
        )
        .into());
    }

    let mut query_hashes = Vec::new();
    let mut compatibility_hashes = Vec::new();
    let mut accounting_hashes = Vec::new();
    let mut canonical_hashes = Vec::new();
    for ordinal in 1..=2 {
        let db = runner_root.join(format!("rebuild-{ordinal}/index.sqlite"));
        let output = runner_root.join(format!("queries/rebuild-{ordinal}"));
        verify_per_tape_accounting(
            &db,
            &args.input_root.join("p0-window-accounting.csv"),
            &args.input_root.join("p0-lineage-accounting.csv"),
            &output.join("per-tape-accounting.jsonl"),
        )?;
        verify_dispatch_oracle(&db, &tape_ids, &output.join("dispatch-oracle.jsonl"))?;
        engram::proof::canonical_oracle::verify(
            &db,
            &args.tape_root,
            &tape_ids,
            &args.oracle_root,
            &output.join("canonical-oracle"),
        )?;
        engram::proof::compatibility::verify(
            &db,
            &tape_ids,
            &args.input_root.join("p0-tombstone-key-to-window.jsonl"),
            &output.join("compatibility"),
        )?;
        verify_direct_touch_oracle(
            &db,
            &args.input_root.join("p0-performance-query-manifest.json"),
            &args
                .input_root
                .join("p0-performance-expected-direct-touches.json"),
            &output.join("direct-touch-results.json"),
        )?;
        accounting_hashes.push(sha256_file(&output.join("per-tape-accounting.jsonl"))?);
        canonical_hashes.push(sha256_file(
            &output.join("canonical-oracle/global-comparison.json"),
        )?);
        query_hashes.push(sha256_file(&output.join("direct-touch-results.json"))?);
        compatibility_hashes.push(sha256_file(&output.join("compatibility/result.json"))?);
    }
    write_canonical_json(
        &runner_root.join("queries/cross-rebuild-comparison.json"),
        &json!({"direct_touch_sha256":query_hashes,"compatibility_sha256":compatibility_hashes,"accounting_sha256":accounting_hashes,"canonical_oracle_sha256":canonical_hashes,
            "per_tape_accounting_matches_frozen_csv_both_rebuilds":true}),
    )?;
    if query_hashes[0] != query_hashes[1]
        || compatibility_hashes[0] != compatibility_hashes[1]
        || accounting_hashes[0] != accounting_hashes[1]
        || canonical_hashes[0] != canonical_hashes[1]
    {
        return Err("cross-rebuild query or compatibility results differ".into());
    }
    record_query_plan(
        &runner_root.join("rebuild-1/index.sqlite"),
        &runner_root.join("queries/query-plan.json"),
    )?;
    engram::proof::concurrency::run(
        &runner_root.join("rebuild-1/index.sqlite"),
        &args.tape_root,
        &tape_ids,
        &args.input_root.join("p0-performance-query-manifest.json"),
        &runner_root.join("concurrency"),
    )?;
    engram::proof::performance::run(
        &engram::proof::performance::Inputs {
            baseline_binary: &args.baseline_binary,
            baseline_database: &args.baseline_database,
            baseline_database_sha256: &args.baseline_database_sha256,
            comparator_receipt: &comparator,
            comparator_receipt_sha256: &args.comparator_receipt_sha256,
            candidate_binary: &args.candidate_binary,
            candidate_database: &runner_root.join("rebuild-1/index.sqlite"),
            tape_root: &args.tape_root,
            manifest: &args.input_root.join("p0-performance-query-manifest.json"),
        },
        &runner_root.join("performance"),
    )?;
    write_test_definitions(&runner_root.join("test-definitions.json"))?;

    let output_manifest = manifest_outputs(&runner_root)?;
    write_canonical_jsonl(
        &runner_root.join("runner-output-manifest.jsonl"),
        output_manifest,
    )?;
    let summary = json!({
        "schema": "t1772-p0-runner-summary-v1",
        "status": "blocked",
        "full_p0_section_9_3_journeys": {"status":"unavailable","reason":"No standalone frozen complete P0 journey expectations supplied; the 12 performance projections and exhaustive tombstone journeys are bounded subsets, not the complete gate"},
        "candidate_base": CANDIDATE_BASE,
        "build_revision": BUILD_REVISION,
        "manifest_sha256": MANIFEST_SHA256,
        "proof_root": PROOF_ROOT,
        "dispatch_rows": DISPATCH_ROWS,
        "canonical_bytes_contract": t1772::CANONICAL_BYTES_CONTRACT,
        "counts": reports[0].counts,
        "rebuilds_logically_identical": true,
        "candidate_bytes_below_historical_ceiling": true,
        "reconstruction_amendment_sha256":engram::proof::baseline_custody::RECONSTRUCTION_AMENDMENT_SHA256,
        "comparator_receipt_sha256":args.comparator_receipt_sha256,
        "cold_custody_amendment_sha256":engram::proof::baseline_custody::COLD_AMENDMENT_SHA256,
        "cold_copy_limitation":engram::proof::baseline_custody::COLD_LIMITATION,
        "size_comparison":engram::proof::baseline_custody::size_comparison(reports[0].bytes, comparator["database_bytes"].as_u64().ok_or("comparator bytes absent")?),
        "elapsed_milliseconds": started.elapsed().as_millis(),
        "no_live_write_targets": true,
        "publication_performed": false
    });
    write_canonical_json(&runner_root.join("runner-summary.json"), &summary)?;
    println!("{}", serde_json::to_string(&summary)?);
    Err("full P0 section 9.3 journey expectations unavailable; full proof cannot pass".into())
}

fn validate_args(args: &Args) -> ProofResult<()> {
    if args.candidate_base != CANDIDATE_BASE {
        return Err(format!("candidate must be {CANDIDATE_BASE}").into());
    }
    require_exact_path(&args.input_root, INPUT_ROOT, "INPUT_ROOT")?;
    require_exact_path(
        &args.oracle_root,
        engram::proof::canonical_oracle::SUPPLEMENT_ROOT,
        "ORACLE_ROOT",
    )?;
    engram::proof::canonical_oracle::verify_inputs(&args.oracle_root)?;
    require_exact_path(&args.proof_root, PROOF_ROOT, "PROOF_ROOT")?;
    if args.manifest != args.input_root.join(MANIFEST_NAME) {
        return Err("manifest path is not the reviewed R29 manifest".into());
    }
    if sha256_file(&args.manifest)? != MANIFEST_SHA256 {
        return Err("manifest SHA-256 mismatch".into());
    }
    if BUILD_REVISION == "UNSET" || BUILD_REVISION.len() != 40 {
        return Err("runner was not built with T1772_BUILD_REVISION=<40-hex revision>".into());
    }
    if !args.tape_root.is_dir() || !args.candidate_binary.is_file() {
        return Err("one or more required input paths are absent".into());
    }
    Ok(())
}

fn read_tape_ids(path: &Path) -> ProofResult<Vec<String>> {
    if sha256_file(path)? != t1772::P0_TAPE_MANIFEST_SHA256 {
        return Err("P0 tape manifest hash mismatch".into());
    }
    let mut ids = Vec::new();
    let mut seen = BTreeSet::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let id = line?;
        if id.len() != 64
            || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !seen.insert(id.clone())
        {
            return Err(format!("invalid or duplicate tape ID: {id:?}").into());
        }
        ids.push(id);
    }
    Ok(ids)
}

fn build_database(db_path: &Path, tape_root: &Path, tape_ids: &[String]) -> ProofResult<u64> {
    let index = SqliteIndex::open_writer(db_path.to_str().ok_or("non-UTF8 database path")?)?;
    let mut events = 0u64;
    for (position, tape_id) in tape_ids.iter().enumerate() {
        let path = tape_root.join(format!("{tape_id}.jsonl.zst"));
        let transcript = read_tape_content(&path).map_err(|error| {
            format!("read {}: {}: {}", path.display(), error.code, error.message)
        })?;
        let parsed = parse_jsonl_events(&transcript)?;
        events += parsed.len() as u64;
        let dispatch = extract_dispatch_links_from_transcript(&transcript);
        index.ingest_tape_events_with_dispatch(
            tape_id,
            &parsed,
            &dispatch,
            LINK_THRESHOLD_DEFAULT,
        )?;
        if (position + 1) % 1000 == 0 {
            eprintln!(
                "rebuild progress: {}/{} tapes",
                position + 1,
                tape_ids.len()
            );
        }
    }
    drop(index);
    Ok(events)
}

fn inspect_database(db_path: &Path, tape_ids: &[String], events: u64) -> ProofResult<DbReport> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let page_size = scalar_u64(&conn, "PRAGMA page_size")?;
    let page_count = scalar_u64(&conn, "PRAGMA page_count")?;
    let freelist_count = scalar_u64(&conn, "PRAGMA freelist_count")?;
    let schema = schema_sql(&conn)?;
    let schema_sha256 = t1772::sha256_bytes(&schema);
    let dbstat = dbstat_rows(&conn)?;

    let read_windows = scalar_u64(
        &conn,
        "SELECT COUNT(*) FROM evidence_windows WHERE kind='read'",
    )?;
    let edit_windows = scalar_u64(
        &conn,
        "SELECT COUNT(*) FROM evidence_windows WHERE kind='edit'",
    )?;
    let edit_edges = scalar_u64(&conn, "SELECT COUNT(*) FROM edges WHERE source_kind='edit'")?;
    let semantic_edit_edges = scalar_u64(
        &conn,
        "SELECT COUNT(*) FROM (SELECT DISTINCT from_anchor,to_anchor,confidence,location_delta,cardinality,agent_link,note FROM edges WHERE source_kind='edit')",
    )?;
    let legacy_tombstone_keys = scalar_u64(
        &conn,
        "SELECT COUNT(*) FROM (SELECT DISTINCT feature_hash,tape_id,event_offset FROM tombstone_features JOIN tombstones USING(tombstone_id))",
    )?;
    let counts = Counts {
        input_tapes: tape_ids.len() as u64,
        normalized_events: events,
        read_windows,
        edit_windows,
        evidence_windows: scalar_u64(&conn, "SELECT COUNT(*) FROM evidence_windows")?,
        evidence_features: scalar_u64(&conn, "SELECT COUNT(*) FROM evidence_features")?,
        tombstone_windows: scalar_u64(&conn, "SELECT COUNT(*) FROM tombstones")?,
        tombstone_features: scalar_u64(&conn, "SELECT COUNT(*) FROM tombstone_features")?,
        edit_edges,
        span_link_edges: scalar_u64(
            &conn,
            "SELECT COUNT(*) FROM edges WHERE source_kind='span_link'",
        )?,
        semantic_edit_edges,
        legacy_tombstone_keys,
        registered_tapes: scalar_u64(&conn, "SELECT COUNT(*) FROM tapes")?,
        dispatch_links: scalar_u64(&conn, "SELECT COUNT(*) FROM dispatch_links")?,
    };

    let mut logical_sha256 = BTreeMap::new();
    for (table, query) in logical_queries() {
        logical_sha256.insert(table.to_string(), digest_query_jsonl(&conn, query)?);
    }
    Ok(DbReport {
        path: db_path.to_string_lossy().into_owned(),
        bytes: fs::metadata(db_path)?.len(),
        page_size,
        page_count,
        freelist_count,
        schema_sha256,
        dbstat,
        logical_sha256,
        counts,
    })
}

fn dbstat_rows(conn: &Connection) -> ProofResult<Vec<DbStatRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, COUNT(*) AS pages, SUM(pgsize) AS bytes FROM dbstat GROUP BY name ORDER BY name",
    )?;
    Ok(stmt
        .query_map([], |row| {
            Ok(DbStatRow {
                name: row.get(0)?,
                pages: row.get(1)?,
                bytes: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn validate_counts(counts: &Counts) -> ProofResult<()> {
    let expected = Counts {
        input_tapes: EXPECTED_INPUTS,
        normalized_events: EXPECTED_EVENTS,
        read_windows: EXPECTED_READ_WINDOWS,
        edit_windows: EXPECTED_EDIT_WINDOWS,
        evidence_windows: EXPECTED_EVIDENCE_WINDOWS,
        evidence_features: EXPECTED_EVIDENCE_FEATURES,
        tombstone_windows: EXPECTED_TOMBSTONE_WINDOWS,
        tombstone_features: EXPECTED_TOMBSTONE_FEATURES,
        edit_edges: EXPECTED_EDIT_EDGES,
        span_link_edges: 0,
        semantic_edit_edges: EXPECTED_SEMANTIC_EDGES,
        legacy_tombstone_keys: EXPECTED_LEGACY_TOMBSTONE_KEYS,
        registered_tapes: EXPECTED_INPUTS,
        dispatch_links: DISPATCH_ROWS,
    };
    if counts != &expected {
        return Err(format!(
            "exact P0 accounting mismatch: observed={counts:?} expected={expected:?}"
        )
        .into());
    }
    Ok(())
}

fn scalar_u64(conn: &Connection, sql: &str) -> ProofResult<u64> {
    Ok(conn.query_row(sql, [], |row| row.get::<_, u64>(0))?)
}

fn schema_sql(conn: &Connection) -> ProofResult<Vec<u8>> {
    let mut stmt = conn.prepare(
        "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY type,name",
    )?;
    let values = stmt
        .query_map([], |row| {
            Ok(json!([
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?
            ]))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut bytes = Vec::new();
    for value in values {
        bytes.extend(canonical_json_lf(&value)?);
    }
    Ok(bytes)
}

fn logical_queries() -> [(&'static str, &'static str); 7] {
    [
        (
            "evidence_windows",
            "SELECT anchor,tape_id,event_offset,kind,file_path,timestamp,window_ordinal FROM evidence_windows ORDER BY tape_id,event_offset,kind,window_ordinal",
        ),
        (
            "evidence_features",
            "SELECT feature_hash,evidence_id FROM evidence_features ORDER BY feature_hash,evidence_id",
        ),
        (
            "edges",
            "SELECT source_kind,tape_id,event_offset,pair_ordinal,from_window_ordinal,to_window_ordinal,from_anchor,to_anchor,confidence,location_delta,cardinality,agent_link,note FROM edges ORDER BY source_kind,tape_id,event_offset,pair_ordinal",
        ),
        (
            "tombstones",
            "SELECT anchor,tape_id,event_offset,file_path,range_start,range_end,timestamp,window_ordinal FROM tombstones ORDER BY tape_id,event_offset,window_ordinal",
        ),
        (
            "tombstone_features",
            "SELECT feature_hash,tombstone_id FROM tombstone_features ORDER BY feature_hash,tombstone_id",
        ),
        ("tapes", "SELECT tape_id FROM tapes ORDER BY tape_id"),
        (
            "dispatch_links",
            "SELECT tape_id,uuid,first_turn_index,direction FROM dispatch_links ORDER BY tape_id,first_turn_index,uuid",
        ),
    ]
}

fn digest_query_jsonl(conn: &Connection, sql: &str) -> ProofResult<String> {
    let mut stmt = conn.prepare(sql)?;
    let column_count = stmt.column_count();
    let mut rows = stmt.query([])?;
    let mut hasher = Sha256::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(column_count);
        for column in 0..column_count {
            values.push(match row.get_ref(column)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => json!(value),
                ValueRef::Real(value) => json!(value),
                ValueRef::Text(value) => json!(std::str::from_utf8(value)?),
                ValueRef::Blob(value) => {
                    json!({"blob_sha256": t1772::sha256_bytes(value), "bytes": value.len()})
                }
            });
        }
        hasher.update(canonical_json_lf(&Value::Array(values))?);
    }
    Ok(hex_digest(hasher))
}

fn verify_per_tape_accounting(
    db_path: &Path,
    window_csv: &Path,
    lineage_csv: &Path,
    output: &Path,
) -> ProofResult<()> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut actual = HashMap::<String, [u64; 9]>::new();
    for (slot, sql) in [
        "SELECT tape_id,COUNT(*) FROM evidence_windows WHERE kind='read' GROUP BY tape_id",
        "SELECT tape_id,COUNT(*) FROM evidence_windows WHERE kind='edit' GROUP BY tape_id",
        "SELECT w.tape_id,COUNT(*) FROM evidence_features f JOIN evidence_windows w USING(evidence_id) GROUP BY w.tape_id",
        "SELECT tape_id,COUNT(*) FROM tombstones GROUP BY tape_id",
        "SELECT t.tape_id,COUNT(*) FROM tombstone_features f JOIN tombstones t USING(tombstone_id) GROUP BY t.tape_id",
        "SELECT tape_id,COUNT(*) FROM edges WHERE source_kind='edit' GROUP BY tape_id",
        "SELECT tape_id,COUNT(*) FROM edges WHERE source_kind='span_link' GROUP BY tape_id",
    ].iter().enumerate() {
        let mut stmt = conn.prepare(sql)?;
        for row in stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)))? {
            let (tape, count) = row?;
            actual.entry(tape).or_insert([0; 9])[slot] = count;
        }
    }
    let mut records = actual.iter().collect::<Vec<_>>();
    records.sort_by_key(|(tape, _)| *tape);
    write_canonical_jsonl(output,records.into_iter().map(|(tape,counts)|json!({"tape_id":tape,"counts_read_edit_features_tombstones_tombstone_features_edit_edges_span_links":&counts[..7]})))?;
    compare_csv(window_csv, &actual, &[0, 1, 2, 3, 4, 5])?;
    compare_csv(lineage_csv, &actual, &[5, 6, 3, 4])?;
    Ok(())
}

fn compare_csv(
    path: &Path,
    actual: &HashMap<String, [u64; 9]>,
    slots: &[usize],
) -> ProofResult<()> {
    let mut lines = BufReader::new(File::open(path)?).lines();
    let _header = lines.next().ok_or("empty accounting CSV")??;
    for line in lines {
        let line = line?;
        let fields = line.split(',').collect::<Vec<_>>();
        if fields.len() != slots.len() + 1 {
            return Err("accounting CSV width mismatch".into());
        }
        let observed = actual.get(fields[0]).copied().unwrap_or([0; 9]);
        for (field, slot) in fields.iter().skip(1).zip(slots) {
            if field.parse::<u64>()? != observed[*slot] {
                return Err(format!(
                    "per-tape accounting mismatch in {} for {}",
                    path.display(),
                    fields[0]
                )
                .into());
            }
        }
    }
    Ok(())
}

fn verify_dispatch_oracle(db_path: &Path, tapes: &[String], output: &Path) -> ProofResult<()> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT uuid,first_turn_index,direction FROM dispatch_links WHERE tape_id=?1 ORDER BY first_turn_index,uuid")?;
    let mut rows = Vec::new();
    for (ordinal, tape) in tapes.iter().enumerate() {
        for row in stmt.query_map([tape], |r| Ok(json!({"direction":r.get::<_,String>(2)?,"first_turn_index":r.get::<_,i64>(1)?,"tape_id":tape,"tape_ordinal":ordinal,"uuid":r.get::<_,String>(0)?})))? {rows.push(row?);}
    }
    let count = rows.len() as u64;
    let digest = write_canonical_jsonl(output, rows)?;
    if count != DISPATCH_ROWS || digest != DISPATCH_JSONL_SHA256 {
        return Err(format!("dispatch oracle mismatch: {count} rows / {digest}").into());
    }
    Ok(())
}

fn verify_direct_touch_oracle(
    db_path: &Path,
    manifest_path: &Path,
    expected_path: &Path,
    output: &Path,
) -> ProofResult<()> {
    let manifest: Value = serde_json::from_reader(File::open(manifest_path)?)?;
    let expected: Value = serde_json::from_reader(File::open(expected_path)?)?;
    let expected_by_id = expected["queries"]
        .as_array()
        .ok_or("expected queries missing")?
        .iter()
        .map(|query| {
            (
                query["id"].as_str().unwrap_or_default().to_string(),
                query.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let index = SqliteIndex::open_reader(db_path.to_str().ok_or("non-UTF8 db path")?)?;
    let mut results = Vec::new();
    for query in manifest["queries"]
        .as_array()
        .ok_or("performance queries missing")?
    {
        let id = query["id"].as_str().ok_or("query id missing")?;
        let target = query["target"].as_str().ok_or("query target missing")?;
        let anchors = derive_anchor_candidates(&[target.to_string()]);
        let touches = t1772::canonical_event_touches(&index.evidence_for_anchors(&anchors)?);
        let expected_touches = expected_by_id
            .get(id)
            .ok_or_else(|| format!("missing expected query {id}"))?["touches"]
            .clone();
        if touches != expected_touches {
            return Err(format!("direct-touch oracle mismatch for {id}").into());
        }
        results.push(json!({"id": id, "touches": touches}));
    }
    write_canonical_json(output, &json!({"queries": results}))?;
    Ok(())
}

fn record_query_plan(db_path: &Path, output: &Path) -> ProofResult<()> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let sample: String = conn.query_row(
        "SELECT feature_hash FROM evidence_features ORDER BY feature_hash LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare("EXPLAIN QUERY PLAN SELECT w.evidence_id,w.anchor,w.tape_id,w.event_offset,w.kind,w.file_path,w.timestamp FROM evidence_features f JOIN evidence_windows w ON w.evidence_id=f.evidence_id WHERE f.feature_hash=?1")?;
    let rows = stmt.query_map(params![sample], |row| Ok(json!({"id": row.get::<_, i64>(0)?, "parent": row.get::<_, i64>(1)?, "detail": row.get::<_, String>(3)?})))?.collect::<Result<Vec<_>, _>>()?;
    let detail = rows
        .iter()
        .filter_map(|row| row["detail"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if detail.contains("SCAN w") || detail.contains("USE TEMP B-TREE") {
        return Err(format!("query plan violates direct-touch gate: {detail}").into());
    }
    write_canonical_json(
        output,
        &json!({"rows": rows, "full_window_scan": false, "temporary_btree": false}),
    )?;
    Ok(())
}

fn write_test_definitions(path: &Path) -> ProofResult<()> {
    write_canonical_json(
        path,
        &json!({
            "schema": "t1772-static-test-definitions-v1",
            "definitions": [
                {"id":"inputs-before-root", "assertion":"controller verifies manifest SHA and 11 entries while PROOF_ROOT is absent"},
                {"id":"two-rebuilds", "assertion":"two independent databases have identical schema and canonical JSON/LF logical table digests"},
                {"id":"exact-accounting", "assertion":"all frozen cardinalities, per-tape CSV rows, and 14,369 dispatch rows match"},
                {"id":"query-equivalence", "assertion":"both rebuilds: exact global/per-tape typed digests, complete 25305 legacy tombstone-key journeys and 12 fixed canonical event-touch projections; full P0 section 9.3 journeys remain unavailable and block passing summary"},
                {"id":"read-only-plan", "assertion":"feature lookup uses posting/window keys without full evidence scan or temporary B-tree"},
                {"id":"performance", "assertion":"reconstructed pinned-baseline versus candidate: 12 identical manifest queries, both binaries, hot and filesystem-cold; 3 warmups and 30 measured fresh processes each, alternating order; raw RSS/elapsed and p50/p95/p99 thresholds retained; baseline CLI counters unavailable/non-comparable, never substituted; every candidate warmup/measured slot requires full ordered same-invocation oracle equality, bound per-statement direct-touch probe coverage with SORT=0/AUTOINDEX=0, and successful zero-observed-temp evidence; retain collector paths/interval/gaps/errors and unlinked/between-sample limitations, never claim zero total temp allocation", "telemetry_amendment_sha256":engram::proof::performance::TELEMETRY_AMENDMENT_SHA256},
                {"id":"reconstructed-comparator-custody", "assertion":"before root creation reject missing/changed receipt, binary/source/manifest/blob identity, schema/table accounting, corpus registration, sidecars, mutable master, reordered/extra/missing transcript slots or non-single-tape fingerprint invocations; accept distinct actual comparator size without historical equality; candidate historical ceiling remains strict; signed actual difference permits savings only when positive; provenance attribution and effective invocation remain independent inspection requirements", "amendment_sha256":engram::proof::baseline_custody::RECONSTRUCTION_AMENDMENT_SHA256},
                {"id":"concurrency", "assertion":"real multi-posting open_reader snapshot held >=60s through 100 actual frozen-tape ingest commits at fixed cadence; interval passive checkpoints; repeat short-query loop; exact error histogram/no retries, commit percentiles, WAL maximum and final checkpoint"},
                {"id":"peak-staging", "assertion":"controller continuously samples staged file sizes and allocated blocks every requested 100ms across operation; preserve raw samples, maximum gap and observed peaks; join sampler before output hashing"},
                {"id":"performance-custody-placement", "assertion":"hot baseline series retain pre/post full hashes and post logical/schema checks against verified master state; cold slots require recorded successful APFS CoW operation, protected stable master/absent sidecars, distinct destination identity, exact invocation/config binding, pre/post metadata and source-write-contract evidence; cold copy digests/logical validation null with accepted limitation; every anomaly or failed/missing invocation receipt fails; candidate full-slot pre/post custody remains; 24 baseline and 1584 candidate working-file hash reads; io-plan reports fresh-copy/cache and shared-block limits"},
                {"id":"minimal-direct-probes", "assertion":"candidate relevant direct exact/feature SQL only, raw SORT/AUTOINDEX/rows/postings with exact product/probe binding and row percentiles; baseline fields unavailable/null; no baseline probe, duplicate seed/lineage traversal or timed-CLI counter claim"},
                {"id":"custody", "assertion":"controller repeats full immutable input custody and admits publication only after comparison"},
                {"id":"live-manifest-clones", "assertion":"at pre/post manifest boundaries retain APFS CoW clones of live index/WAL/SHM and hash only those clones; record read-only source metadata, absence and per-file capture times; no byte-copy or direct-live-hash fallback, no multi-file atomicity claim"},
                {"id":"lifecycle", "assertion":"controller observes stopped post-exec runner, csops status, Darwin SIGCONT 19, stdout, stderr, and exit status"}
            ],
            "canonical_bytes_contract": t1772::CANONICAL_BYTES_CONTRACT
        }),
    )?;
    Ok(())
}

fn manifest_outputs(root: &Path) -> ProofResult<Vec<Value>> {
    let mut paths = Vec::new();
    collect_files(root, &mut paths)?;
    paths.sort();
    let mut values = Vec::new();
    for path in paths {
        if path.file_name().and_then(|name| name.to_str()) == Some("runner-output-manifest.jsonl") {
            continue;
        }
        values.push(json!({
            "path": path.strip_prefix(root)?.to_string_lossy(),
            "bytes": fs::metadata(&path)?.len(),
            "sha256": sha256_file(&path)?
        }));
    }
    Ok(values)
}

fn collect_files(path: &Path, output: &mut Vec<PathBuf>) -> ProofResult<()> {
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, output)?;
        } else if path.is_file() {
            output.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod correction_tests {
    use super::*;
    #[test]
    fn dispatch_retains_frozen_ordinal_shape_and_turn_before_uuid_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        drop(SqliteIndex::open_writer(db.to_str().unwrap()).unwrap());
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("INSERT INTO dispatch_links VALUES('b','a',9,'sent'),('b','z',1,'received'),('a','q',2,'received');").unwrap();
        let output = dir.path().join("dispatch.jsonl");
        assert!(verify_dispatch_oracle(&db, &["b".into(), "a".into()], &output).is_err());
        let rows = BufReader::new(File::open(output).unwrap())
            .lines()
            .map(|line| serde_json::from_str::<Value>(&line.unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            rows[0],
            json!({"direction":"received","first_turn_index":1,"tape_id":"b","tape_ordinal":0,"uuid":"z"})
        );
        assert_eq!(rows[1]["uuid"], "a");
        assert_eq!(rows[2]["tape_ordinal"], 1);
    }
}
