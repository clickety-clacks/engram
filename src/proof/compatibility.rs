//! R29-1: candidate comparisons against the frozen lineage/tombstone census.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::t1772::{self, ProofResult, write_canonical_json, write_canonical_jsonl};
use crate::index::SqliteIndex;

// This is the retained census's legacy preimage, not the new artifact encoding.
// Source 0fc03ad99e48f293f0b351ddbc4269ed4b48681bc2ff4a31e6f88403ba6915cd.
fn census_row(hash: &mut Sha256, row: &Value) -> ProofResult<()> {
    let bytes = serde_json::to_vec(row)?;
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    Ok(())
}

pub fn verify(db: &Path, tape_ids: &[String], mapping: &Path, output: &Path) -> ProofResult<()> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let index = SqliteIndex::open_reader(db.to_str().ok_or("non-UTF8 database")?)?;
    let mut edge_hash = Sha256::new();
    let mut edges = Vec::new();
    let mut statement = conn.prepare("SELECT tape_id,event_offset,pair_ordinal,from_window_ordinal,to_window_ordinal,from_anchor,to_anchor,confidence,location_delta,cardinality,agent_link,note FROM edges WHERE source_kind='edit' AND tape_id=?1 ORDER BY event_offset,pair_ordinal")?;
    for tape in tape_ids {
        for row in statement.query_map([tape], |row| {
            Ok(json!([
                "edit",
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                (row.get::<_, f64>(7)? as f32).to_bits(),
                row.get::<_, String>(8)?,
                match row.get::<_, String>(9)?.as_str() {
                    "1:1" => "one_to_one",
                    "1:N" => "one_to_many",
                    "N:1" => "many_to_one",
                    _ => return Err(rusqlite::Error::InvalidQuery),
                },
                row.get::<_, bool>(10)?,
                row.get::<_, String>(11)?
            ]))
        })? {
            let row = row?;
            census_row(&mut edge_hash, &row)?;
            edges.push(row);
        }
    }
    let edge_digest = t1772::hex_digest(edge_hash);
    write_canonical_jsonl(&output.join("edit-edges.jsonl"), edges)?;

    let expected = BufReader::new(File::open(mapping)?)
        .lines()
        .map(|line| Ok(serde_json::from_str::<Value>(&line?)?))
        .collect::<ProofResult<Vec<_>>>()?;
    let mut actual_mapping = Vec::new();
    let mut mapping_hash = Sha256::new();
    let mut legacy_keys = BTreeSet::new();
    let mut expected_journeys = BTreeMap::<String, BTreeSet<String>>::new();
    let mut matches = true;
    let mut lookup = conn.prepare("SELECT t.anchor,t.window_ordinal,t.file_path,t.range_start,t.range_end,t.timestamp FROM tombstone_features f JOIN tombstones t USING(tombstone_id) WHERE f.feature_hash=?1 AND t.tape_id=?2 AND t.event_offset=?3 AND t.window_ordinal=?4")?;
    for row in &expected {
        let key = &row["legacy_key"];
        let feature = key["feature_hash"]
            .as_str()
            .ok_or("mapping feature absent")?;
        let tape = key["tape_id"].as_str().ok_or("mapping tape absent")?;
        let offset = key["event_offset"].as_u64().ok_or("mapping event absent")?;
        let ordinal = row["canonical_window"]["window_ordinal"]
            .as_u64()
            .ok_or("mapping ordinal absent")?;
        legacy_keys.insert((feature.to_string(), tape.to_string(), offset));
        expected_journeys.entry(feature.into()).or_default().insert(
            json!([
                tape,
                offset,
                row["file_path"],
                row["range_start"],
                row["range_end"],
                row["timestamp"]
            ])
            .to_string(),
        );
        let found = lookup.query_map(params![feature,tape,offset,ordinal], |r| Ok(json!({
            "legacy_key":key,"canonical_window":{"anchor":r.get::<_,String>(0)?,"window_ordinal":r.get::<_,u64>(1)?},
            "file_path":r.get::<_,String>(2)?,"range_start":r.get::<_,u64>(3)?,"range_end":r.get::<_,u64>(4)?,"timestamp":r.get::<_,String>(5)?
        })))?.collect::<Result<Vec<_>,_>>()?;
        if found.as_slice() != [row.clone()] {
            matches = false;
        }
        for candidate in &found {
            census_row(&mut mapping_hash, candidate)?;
        }
        actual_mapping.push(json!({"expected":row,"actual":found}));
    }
    write_canonical_jsonl(
        &output.join("tombstone-mapping-comparison.jsonl"),
        actual_mapping,
    )?;
    let mut journeys = Vec::new();
    for (feature, tape, offset) in &legacy_keys {
        // Query every legacy key through the actual product reader. Compare the
        // complete result for its feature, not an intersection with expected IDs.
        let found = index
            .tombstones_for_anchor(feature)?
            .into_iter()
            .map(|t| {
                json!([
                    t.tape_id,
                    t.event_offset,
                    t.file_path,
                    t.range_at_deletion.start,
                    t.range_at_deletion.end,
                    t.timestamp
                ])
                .to_string()
            })
            .collect::<BTreeSet<_>>();
        let wanted = &expected_journeys[feature];
        matches &= &found == wanted;
        journeys.push(json!({"legacy_key":[feature,tape,offset],"expected":wanted.iter().map(|s|serde_json::from_str::<Value>(s).unwrap()).collect::<Vec<_>>(),
            "actual":found.iter().map(|s|serde_json::from_str::<Value>(s).unwrap()).collect::<Vec<_>>(),"equal":&found==wanted}));
    }
    write_canonical_jsonl(&output.join("tombstone-event-journeys.jsonl"), journeys)?;
    let mapping_digest = t1772::hex_digest(mapping_hash);
    let posting_count: u64 =
        conn.query_row("SELECT COUNT(*) FROM tombstone_features", [], |row| {
            row.get(0)
        })?;
    matches &= posting_count == expected.len() as u64;
    let passed = matches
        && edge_digest == t1772::EDIT_EDGE_LOGICAL_SHA256
        && mapping_digest == t1772::TOMBSTONE_MAPPING_SHA256
        && legacy_keys.len() as u64 == t1772::EXPECTED_LEGACY_TOMBSTONE_KEYS;
    write_canonical_json(
        &output.join("result.json"),
        &json!({"passed":passed,
        "edit_edge_logical_sha256":edge_digest,"tombstone_mapping_logical_sha256":mapping_digest,
        "legacy_keys_queried":legacy_keys.len(),"posting_rows":posting_count,"mapping_and_journeys_equal":matches,
        "digest_preimage":"frozen census: u64 LE compact-JSON byte length then those JSON bytes, in frozen manifest/event/window/feature order; no LF in legacy digest",
        "artifact_preimage":t1772::CANONICAL_BYTES_CONTRACT}),
    )?;
    if !passed {
        return Err(
            "frozen edit-edge/tombstone compatibility mismatch; retained candidate results".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_count_tombstone_corruption_is_retained_and_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("fixture.sqlite");
        drop(SqliteIndex::open_writer(db.to_str().unwrap()).unwrap());
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("INSERT INTO tapes VALUES('t');
            INSERT INTO tombstones(tombstone_id,anchor,tape_id,event_offset,file_path,range_start,range_end,timestamp,window_ordinal)
            VALUES(1,'winnow:a,b','t',7,'/right',1,2,'now',0),(2,'winnow:a,c','t',7,'/right',1,2,'now',1);
            INSERT INTO tombstone_features VALUES('winnow:a',1),('winnow:a',2);").unwrap();
        let mapping = dir.path().join("mapping.jsonl");
        write_canonical_jsonl(&mapping,[json!({"legacy_key":{"feature_hash":"winnow:a","tape_id":"t","event_offset":7},
            "canonical_window":{"anchor":"winnow:a,b","window_ordinal":0},"file_path":"/right","range_start":1,"range_end":2,"timestamp":"now"}),
            json!({"legacy_key":{"feature_hash":"winnow:a","tape_id":"t","event_offset":7},
            "canonical_window":{"anchor":"winnow:a,c","window_ordinal":1},"file_path":"/right","range_start":1,"range_end":2,"timestamp":"now"})]).unwrap();
        for corrupted in [false, true] {
            if corrupted {
                conn.execute(
                    "UPDATE tombstones SET file_path='/wrong' WHERE tombstone_id=2",
                    [],
                )
                .unwrap();
            }
            let output = dir
                .path()
                .join(if corrupted { "corrupt" } else { "matching" });
            // A two-row fixture must never pass the frozen 25,305-key P0 gate.
            assert!(verify(&db, &["t".into()], &mapping, &output).is_err());
            let result: Value =
                serde_json::from_reader(File::open(output.join("result.json")).unwrap()).unwrap();
            assert_eq!(result["posting_rows"], 2);
            assert_eq!(result["legacy_keys_queried"], 1);
            assert_eq!(result["mapping_and_journeys_equal"], !corrupted);
            let journey: Value = serde_json::from_str(
                &std::fs::read_to_string(output.join("tombstone-event-journeys.jsonl")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                journey["actual"].as_array().unwrap().len(),
                if corrupted { 2 } else { 1 }
            );
        }
    }
}
