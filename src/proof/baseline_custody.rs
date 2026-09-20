//! Only the frozen v3 baseline's disposable-copy exception (PO amendment r1).
use super::t1772::{ProofResult, canonical_json_lf, collect_custody};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const RECONSTRUCTION_AMENDMENT_SHA256: &str =
    "8c7223747f618b9253b6dd956309a2e49caf2f2ec6ba0169ca17dda4a362a63b";
pub const BASELINE_SOURCE: &str = "72821518037a9d896f0b4d784fee146800902e78";
pub const COMPARATOR_ROOT: &str =
    "/Users/mike/shared-workspace/engram/proofs/t1772/reconstructed-comparator-asg-931d2f4b-r1";
pub const COMPARATOR_DATABASE: &str = "/Users/mike/shared-workspace/engram/proofs/t1772/reconstructed-comparator-asg-931d2f4b-r1/master/index.sqlite";
pub const COMPARATOR_RECEIPT: &str = "/Users/mike/shared-workspace/engram/proofs/t1772/reconstructed-comparator-asg-931d2f4b-r1/receipt.json";
pub const COMPARISON_LABEL: &str = "reconstructed pinned-baseline versus candidate";

/// Receipt consumption only. This module never constructs a comparator or
/// invokes its binary. All paths below are prospective, not recovered custody.
pub fn read_comparator_receipt(
    db: &Path,
    db_sha: &str,
    receipt: &Path,
    receipt_sha: &str,
) -> ProofResult<Value> {
    use super::{performance, t1772};
    t1772::require_exact_path(db, COMPARATOR_DATABASE, "reconstructed comparator")?;
    t1772::require_exact_path(receipt, COMPARATOR_RECEIPT, "comparator receipt")?;
    let bytes = std::fs::read(receipt)?;
    let value: Value = serde_json::from_slice(&bytes)?;
    if t1772::sha256_bytes(&bytes) != receipt_sha
        || canonical_json_lf(&value)? != bytes
        || value["identity"] != "reconstructed-pinned-baseline-v1"
        || value["amendment_sha256"] != RECONSTRUCTION_AMENDMENT_SHA256
        || value["binary_path"] != performance::BASELINE_BINARY_PATH
        || value["binary_sha256"] != performance::BASELINE_BINARY_SHA256
        || value["source_revision"] != BASELINE_SOURCE
        || value["tape_manifest_sha256"] != t1772::P0_TAPE_MANIFEST_SHA256
        || value["database_path"] != COMPARATOR_DATABASE
        || value["database_sha256"] != db_sha
        || value["database_bytes"].as_u64() != Some(std::fs::metadata(db)?.len())
        || value["database_bytes"].as_u64() == Some(0)
        || value["historical_snapshot_status"] != "unavailable in inspected custody"
        || value["historical_tool_message_postings_replay_delta"] != 87_711
        || value["unexplained_corpus_or_semantic_differences"] != json!([])
        || value["size_shaping_performed"] != false
        || value["snapshot_state"] != "checkpointed-closed-master-no-sidecars"
    {
        return Err("reconstructed comparator receipt identity/bindings invalid".into());
    }
    Ok(value)
}

/// Preflight, while PROOF_ROOT is absent: independently inspect bytes, schema,
/// logical accounting and corpus; verify the attributable reconstruction log.
pub fn verify_comparator(
    db: &Path,
    db_sha: &str,
    receipt: &Path,
    receipt_sha: &str,
    input_root: &Path,
    tape_root: &Path,
) -> ProofResult<Value> {
    use super::t1772;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    let value = read_comparator_receipt(db, db_sha, receipt, receipt_sha)?;
    if db.canonicalize()? != db
        || receipt.canonicalize()? != receipt
        || std::fs::metadata(db)?.permissions().mode() & 0o222 != 0
        || std::fs::metadata(db.parent().ok_or("master parent missing")?)?
            .permissions()
            .mode()
            & 0o222
            != 0
        || t1772::sha256_file(db)? != db_sha
    {
        return Err("comparator master must be canonical, read-only and hash-bound".into());
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        if std::fs::symlink_metadata(format!("{}{suffix}", db.display())).is_ok() {
            return Err("comparator master must be closed/checkpointed without sidecars".into());
        }
    }
    let observed = snapshot(db)?;
    for field in ["schema", "user_version", "tables"] {
        if value["accounting"][field] != observed[field] {
            return Err(format!("comparator {field} accounting differs from receipt").into());
        }
    }
    let ids_path = input_root.join("p0-tape-ids.txt");
    if t1772::sha256_file(&ids_path)? != t1772::P0_TAPE_MANIFEST_SHA256 {
        return Err("comparator tape manifest changed".into());
    }
    let ids_text = std::fs::read_to_string(ids_path)?;
    let ids = ids_text.lines().collect::<Vec<_>>();
    let mut sorted_ids = ids.clone();
    sorted_ids.sort_unstable();
    if ids.len() as u64 != t1772::EXPECTED_INPUTS || sorted_ids.windows(2).any(|p| p[0] == p[1]) {
        return Err("comparator corpus count/uniqueness mismatch".into());
    }
    let conn = open_copy_reader(db)?;
    let consistency: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if consistency != "ok" {
        return Err("comparator SQLite consistency check failed".into());
    }
    let mut query = conn.prepare("SELECT tape_id FROM tapes ORDER BY tape_id")?;
    let registered = query
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if registered != sorted_ids {
        return Err("comparator registered corpus differs from frozen manifest".into());
    }
    let mut ordered = conn.prepare("SELECT tape_id FROM tapes ORDER BY rowid")?;
    let registered_order = ordered
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if registered_order != ids {
        return Err("comparator registration order differs from frozen manifest".into());
    }
    let root = Path::new(COMPARATOR_ROOT);
    let artifact = |field: &str| -> ProofResult<std::path::PathBuf> {
        let path = std::path::PathBuf::from(
            value[field]["path"]
                .as_str()
                .ok_or("receipt artifact path absent")?,
        );
        if !path.starts_with(root)
            || path.canonicalize()? != path
            || !std::fs::symlink_metadata(&path)?.is_file()
            || std::fs::metadata(&path)?.len() == 0
            || t1772::sha256_file(&path)?
                != value[field]["sha256"]
                    .as_str()
                    .ok_or("receipt artifact hash absent")?
        {
            return Err(format!("comparator {field} artifact custody invalid").into());
        }
        Ok(path)
    };
    artifact("provenance")?; // Attributable historical-difference explanation; review remains required.
    let log = artifact("build_transcript")?;
    let mut lines = BufReader::new(std::fs::File::open(log)?).split(b'\n');
    for (ordinal, id) in ids.iter().enumerate() {
        let mut bytes = lines
            .next()
            .ok_or("reconstruction transcript truncated")??;
        bytes.push(b'\n');
        let row: Value = serde_json::from_slice(&bytes)?;
        let cwd = Path::new(row["cwd"].as_str().ok_or("build cwd missing")?);
        let env = row["environment"]
            .as_object()
            .ok_or("build environment missing")?;
        let source = tape_root.join(format!("{id}.jsonl.zst"));
        if canonical_json_lf(&row)? != bytes
            || row["ordinal"] != ordinal
            || row["tape_id"] != *id
            || row["visible_tape_ids"] != json!([id])
            || row["argv"] != json!([super::performance::BASELINE_BINARY_PATH, "fingerprint"])
            || !cwd.starts_with(root)
            || cwd.canonicalize()? != cwd
            || row["environment_cleared"] != true
            || env.is_empty()
            || !env.values().all(Value::is_string)
            || row["source_path"] != source.to_string_lossy().as_ref()
            || row["source_blob_sha256"] != t1772::sha256_file(&source)?
            || row["exit_code"] != 0
            || !row["stderr"].is_string()
            || row["stdout"]["status"] != "ok"
            || row["stdout"]["scanned_tapes"] != 1
            || row["stdout"]["fingerprinted_tapes"] != 1
            || row["stdout"]["skipped_existing_tapes"] != 0
            || row["stdout"]["failure_count"] != 0
            || row["stdout"]["failures"] != json!([])
        {
            return Err(format!("comparator build transcript slot {ordinal} invalid").into());
        }
        // Bind the normalized content address as well as the compressed blob.
        let text = crate::store::tapes::read_tape_content(&source)
            .map_err(|e| format!("comparator source read: {}", e.message))?;
        if t1772::sha256_bytes(text.as_bytes()) != *id {
            return Err("comparator normalized source identity mismatch".into());
        }
        let config_path = Path::new(
            row["config"]["path"]
                .as_str()
                .ok_or("build config missing")?,
        );
        if !config_path.starts_with(root)
            || config_path.canonicalize()? != config_path
            || t1772::sha256_file(config_path)?
                != row["config"]["sha256"]
                    .as_str()
                    .ok_or("config hash missing")?
        {
            return Err("comparator build configuration custody invalid".into());
        }
    }
    if lines.next().is_some() || t1772::sha256_file(db)? != db_sha {
        return Err("comparator transcript has extra slots or master changed".into());
    }
    Ok(value)
}

pub fn size_comparison(candidate_bytes: u64, comparator_bytes: u64) -> Value {
    let difference = i128::from(comparator_bytes) - i128::from(candidate_bytes);
    json!({"comparison_label":COMPARISON_LABEL,
        "historical_candidate_ceiling_bytes":super::t1772::BASELINE_BYTES,
        "candidate_bytes":candidate_bytes,"reconstructed_comparator_bytes":comparator_bytes,
        "comparator_minus_candidate_bytes":difference,
        "positive_savings_against_reconstructed_comparator":difference > 0,
        "historical_snapshot_measured":false,"live_reclamation_measured":false})
}

pub const AMENDMENT_SHA256: &str =
    "3ce0623b7ebb575962be92bf1f9f3e94b2195682a4c8f8f5e96e88d24e95c85e";
const TABLES: [&str; 7] = [
    "dispatch_links",
    "edges",
    "evidence",
    "query_results",
    "result_feedback",
    "tapes",
    "tombstones",
];
const INDEXES: [&str; 8] = [
    "idx_dispatch_links_received",
    "idx_dispatch_links_tape",
    "idx_dispatch_links_uuid",
    "idx_edges_from_anchor",
    "idx_edges_to_anchor",
    "idx_evidence_anchor",
    "idx_query_results_command",
    "idx_tombstones_anchor",
];

pub(crate) fn open_copy_reader(db: &Path) -> ProofResult<Connection> {
    // Writable measurement copies or a verified checkpointed read-only master.
    // Immutable URI when checkpointed avoids observer-created WAL/SHM. If the
    // baseline left WAL, the read-only connection must see its committed rows.
    let has_wal =
        db.with_extension("sqlite-wal").exists() || db.with_extension("sqlite-shm").exists();
    Ok(if has_wal {
        Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?
    } else {
        let encoded: String = db
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .map(|b| format!("%{b:02X}"))
            .collect();
        Connection::open_with_flags(
            format!("file:{encoded}?mode=ro&immutable=1"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?
    })
}

pub fn snapshot(db: &Path) -> ProofResult<Value> {
    let conn = open_copy_reader(db)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != 3 {
        return Err("baseline copy must already have schema v3".into());
    }
    let mut stmt =
        conn.prepare("SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name")?;
    let schema = stmt
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?
            ]))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for name in TABLES {
        if !schema.iter().any(|r| r[0] == "table" && r[1] == name) {
            return Err(format!("missing baseline table {name}; no creation permitted").into());
        }
    }
    for name in INDEXES {
        if !schema.iter().any(|r| r[0] == "index" && r[1] == name) {
            return Err(format!("missing baseline index {name}; no creation permitted").into());
        }
    }
    for row in &schema {
        let name = row[1].as_str().ok_or("schema name absent")?;
        let allowed = (row[0] == "table" && TABLES.contains(&name))
            || (row[0] == "index"
                && (INDEXES.contains(&name) || name.starts_with("sqlite_autoindex_")));
        if !allowed {
            return Err(format!("unexpected baseline schema object {name}").into());
        }
    }
    // Check the pinned baseline writer's feedback column contract before launch.
    conn.prepare("SELECT result_id,command,payload_json,created_at FROM query_results LIMIT 0")?;
    conn.prepare("SELECT result_id,outcome,note,rated_at FROM result_feedback LIMIT 0")?;
    for sql in [
        "SELECT anchor,tape_id,event_offset,kind,file_path,timestamp FROM evidence LIMIT 0",
        "SELECT from_anchor,to_anchor,confidence,location_delta,cardinality,agent_link,note FROM edges LIMIT 0",
        "SELECT anchor,tape_id,event_offset,file_path,range_start,range_end,timestamp FROM tombstones LIMIT 0",
        "SELECT tape_id FROM tapes LIMIT 0",
        "SELECT tape_id,uuid,first_turn_index,direction FROM dispatch_links LIMIT 0",
    ] {
        conn.prepare(sql)?;
    }
    let mut tables = serde_json::Map::new();
    for name in TABLES {
        // Fixed v3 tables are rowid tables. Include rowid to reject otherwise
        // unexplained rewrites. Each typed row has the canonical JSON/LF preimage.
        let mut statement = conn.prepare(&format!("SELECT rowid,* FROM {name} ORDER BY rowid"))?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        let mut digest = Sha256::new();
        let mut count = 0_u64;
        while let Some(row) = rows.next()? {
            let mut values = Vec::with_capacity(columns);
            for column in 0..columns {
                values.push(match row.get_ref(column)? {
                    ValueRef::Null => json!(["null"]),
                    ValueRef::Integer(n) => json!(["integer", n]),
                    ValueRef::Real(n) => json!(["real_bits", n.to_bits()]),
                    ValueRef::Text(b) => json!(["text_bytes", b]),
                    ValueRef::Blob(b) => json!(["blob_bytes", b]),
                });
            }
            digest.update(canonical_json_lf(&json!(values))?);
            count += 1;
        }
        tables.insert(
            name.into(),
            json!({"rows":count,"sha256":format!("{:x}",digest.finalize())}),
        );
    }
    drop(stmt);
    drop(conn);
    let files = copy_files(db)?;
    Ok(
        json!({"schema":schema,"user_version":version,"tables":tables,"files":files,
        "observer":"read-only SQL; existing WAL read when present; typed rows in rowid order; file custody after observer closes"}),
    )
}

pub fn compare(before: &Value, after: &Value) -> ProofResult<()> {
    if before["schema"] != after["schema"] || before["user_version"] != after["user_version"] {
        return Err("baseline changed schema".into());
    }
    for table in TABLES {
        if table != "query_results" && before["tables"][table] != after["tables"][table] {
            return Err(format!("baseline changed forbidden table {table}").into());
        }
    }
    Ok(())
}

pub fn copy_files(db: &Path) -> ProofResult<Value> {
    let directory = db.parent().ok_or("copy directory absent")?;
    let name = db
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("copy filename absent")?;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let filename = entry.file_name();
        let filename = filename.to_str().ok_or("non-UTF8 copy entry")?;
        if ![
            name.to_string(),
            format!("{name}-wal"),
            format!("{name}-shm"),
            format!("{name}-journal"),
        ]
        .contains(&filename.to_string())
            || !entry.file_type()?.is_file()
        {
            return Err(format!("unexplained baseline copy entry {filename}").into());
        }
    }
    Ok(serde_json::to_value(collect_custody(&[
        directory.to_path_buf()
    ])?)?)
}
