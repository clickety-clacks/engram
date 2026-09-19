//! Only the frozen v3 baseline's disposable-copy exception (PO amendment r1).
use super::t1772::{ProofResult, canonical_json_lf, collect_custody};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

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
    // This function must only receive the owned staging copy, never a master.
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
