//! R29-1 only: compare stored candidate rows with the frozen typed oracle.
//! Format reference: oracle-independent-main.rs SHA 4ef2532b5ad3e8a8be31f99c1ce1e2c3a42fa1e4399ef83873b6a13169318a25.
use super::t1772::{self, ProofResult, canonical_json_lf, sha256_file, write_canonical_json};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
};

const CATEGORIES: [&str; 8] = [
    "evidence_window",
    "evidence_feature",
    "event_touch",
    "edge",
    "tombstone",
    "tombstone_feature",
    "dispatch",
    "legacy_tombstone_journey",
];
pub const SUPPLEMENT_ROOT: &str =
    "/Users/mike/shared-workspace/engram/proofs/t1772/oracle-inputs-r29";
pub fn verify_inputs(root: &Path) -> ProofResult<Value> {
    let mut rows = Vec::new();
    for (name, expected) in [
        ("global-oracle.json", t1772::GLOBAL_ORACLE_SHA256),
        ("per-tape-oracle.jsonl", t1772::PER_TAPE_ORACLE_SHA256),
    ] {
        let path = root.join(name);
        let actual = sha256_file(&path)?;
        if actual != expected {
            return Err(format!("supplemental oracle hash mismatch: {}", path.display()).into());
        }
        rows.push(json!({"path":path,"bytes":fs::metadata(&path)?.len(),"sha256":actual}));
    }
    Ok(json!({"supplemental_inputs":rows,"frozen_11_entry_manifest_unchanged":true}))
}

#[derive(Clone)]
struct Field(u8, Vec<u8>);
fn s(v: &str) -> Field {
    Field(b's', v.as_bytes().to_vec())
}
fn u(v: u64) -> Field {
    Field(b'u', v.to_be_bytes().to_vec())
}
fn i(v: i64) -> Field {
    Field(b'i', v.to_be_bytes().to_vec())
}
fn blob(out: &mut Vec<u8>, tag: u8, bytes: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}
fn frame(fields: &[Field]) -> Vec<u8> {
    let mut out = vec![b'R'];
    out.extend_from_slice(&(fields.len() as u32).to_be_bytes());
    for Field(tag, bytes) in fields {
        blob(&mut out, *tag, bytes);
    }
    out.push(b'E');
    out
}
struct Rows {
    hash: Sha256,
    count: u64,
}
impl Rows {
    fn new(category: &str, scope: &str) -> Self {
        let mut bytes = Vec::new();
        for (tag, v) in [
            (b'D', "ENGRAM-T1772-P0-ORACLE-v1"),
            (b'C', category),
            (b'S', scope),
        ] {
            blob(&mut bytes, tag, v.as_bytes());
        }
        Self {
            hash: Sha256::new().chain_update(bytes),
            count: 0,
        }
    }
    fn add(&mut self, fields: &[Field]) {
        self.hash.update(frame(fields));
        self.count += 1;
    }
    fn summary(&self) -> Value {
        json!({"count":self.count,"sha256":t1772::hex_digest(self.hash.clone())})
    }
}
struct Oracle {
    scope: String,
    rows: BTreeMap<&'static str, Rows>,
    semantic: BTreeSet<Vec<u8>>,
    metrics: BTreeMap<&'static str, u64>,
}
impl Oracle {
    fn new(scope: &str) -> Self {
        Self {
            scope: scope.into(),
            rows: CATEGORIES
                .into_iter()
                .map(|c| (c, Rows::new(c, scope)))
                .collect(),
            semantic: BTreeSet::new(),
            metrics: [
                "normalized_events",
                "read_windows",
                "edit_windows",
                "evidence_features",
                "event_touches",
                "edit_edges",
                "span_link_edges",
                "tombstone_windows",
                "tombstone_features",
                "dispatch_rows",
                "legacy_journey_rows",
            ]
            .into_iter()
            .map(|k| (k, 0))
            .collect(),
        }
    }
    fn inc(&mut self, key: &'static str, n: u64) {
        *self.metrics.get_mut(key).unwrap() += n;
    }
    fn summary(&self) -> Value {
        let categories: BTreeMap<_, _> = self.rows.iter().map(|(k, v)| (*k, v.summary())).collect();
        let mut combined = Rows::new("combined", &self.scope);
        for c in CATEGORIES {
            let v = &categories[c];
            combined.add(&[
                s(c),
                u(v["count"].as_u64().unwrap()),
                s(v["sha256"].as_str().unwrap()),
            ]);
        }
        let mut semantic = Rows::new("semantic_edge", &self.scope);
        for row in &self.semantic {
            let mut bytes = Vec::new();
            blob(&mut bytes, b'R', row);
            semantic.hash.update(bytes);
            semantic.count += 1;
        }
        json!({"categories":categories,"combined":combined.summary(),"semantic_edges":semantic.summary(),"metrics":self.metrics})
    }
}
fn emit(local: &mut Oracle, global: &mut Oracle, category: &str, fields: &[Field]) {
    local.rows.get_mut(category).unwrap().add(fields);
    global.rows.get_mut(category).unwrap().add(fields);
}

fn stored_tape(conn: &Connection, tape: &str, global: &mut Oracle) -> ProofResult<Oracle> {
    let mut local = Oracle::new(&format!("tape:{tape}"));
    let mut evidence=conn.prepare("SELECT evidence_id,event_offset,kind,file_path,timestamp,window_ordinal,anchor FROM evidence_windows WHERE tape_id=?1 ORDER BY event_offset,kind,window_ordinal")?;
    let mut posting = conn.prepare(
        "SELECT EXISTS(SELECT 1 FROM evidence_features WHERE feature_hash=?1 AND evidence_id=?2)",
    )?;
    let mut previous = None;
    let mut rows = evidence.query([tape])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let event: u64 = row.get(1)?;
        let kind: String = row.get(2)?;
        let file: String = row.get(3)?;
        let timestamp: String = row.get(4)?;
        let ordinal: u64 = row.get(5)?;
        let anchor: String = row.get(6)?;
        let event_key = (event, kind.clone(), file.clone(), timestamp.clone());
        if previous.as_ref() != Some(&event_key) {
            emit(
                &mut local,
                global,
                "event_touch",
                &[s(tape), u(event), s(&kind), s(&file), s(&timestamp)],
            );
            local.inc("event_touches", 1);
            previous = Some(event_key);
        }
        emit(
            &mut local,
            global,
            "evidence_window",
            &[
                s(tape),
                u(event),
                s(&kind),
                s(&file),
                s(&timestamp),
                u(ordinal),
                s(&anchor),
            ],
        );
        local.inc(
            if kind == "read" {
                "read_windows"
            } else {
                "edit_windows"
            },
            1,
        );
        for feature in features(&anchor)? {
            let exists: bool = posting.query_row(params![feature, id], |r| r.get(0))?;
            if !exists {
                return Err(format!("missing stored evidence posting {id}/{feature}").into());
            }
            emit(
                &mut local,
                global,
                "evidence_feature",
                &[s(&feature), s(tape), u(event), s(&kind), u(ordinal)],
            );
            local.inc("evidence_features", 1);
        }
    }
    let mut edges=conn.prepare("SELECT source_kind,event_offset,pair_ordinal,from_window_ordinal,to_window_ordinal,from_anchor,to_anchor,confidence,location_delta,cardinality,agent_link,note FROM edges WHERE tape_id=?1 ORDER BY event_offset,source_kind,pair_ordinal")?;
    let mut rows = edges.query([tape])?;
    while let Some(r) = rows.next()? {
        let kind: String = r.get(0)?;
        let from: String = r.get(5)?;
        let to: String = r.get(6)?;
        let location: String = r.get(8)?;
        let cardinality: String = r.get(9)?;
        let note: String = r.get(11)?;
        let cardinality = match cardinality.as_str() {
            "1:1" => "1:1",
            "1:N" => "1:N",
            "N:1" => "N:1",
            _ => return Err("unknown edge cardinality".into()),
        };
        let semantic = vec![
            s(&from),
            s(&to),
            Field(
                b'f',
                (r.get::<_, f64>(7)? as f32)
                    .to_bits()
                    .to_be_bytes()
                    .to_vec(),
            ),
            s(&location),
            s(cardinality),
            Field(b'b', vec![u8::from(r.get::<_, bool>(10)?)]),
            s(&note),
        ];
        let mut fields = vec![
            s(&kind),
            s(tape),
            u(r.get(1)?),
            u(r.get(2)?),
            i(r.get(3)?),
            i(r.get(4)?),
        ];
        fields.extend(semantic.clone());
        emit(&mut local, global, "edge", &fields);
        let framed = frame(&semantic);
        local.semantic.insert(framed.clone());
        global.semantic.insert(framed);
        local.inc(
            if kind == "edit" {
                "edit_edges"
            } else {
                "span_link_edges"
            },
            1,
        );
    }
    let mut tombstones=conn.prepare("SELECT tombstone_id,event_offset,file_path,range_start,range_end,timestamp,window_ordinal,anchor FROM tombstones WHERE tape_id=?1 ORDER BY event_offset,window_ordinal")?;
    let mut posting = conn.prepare(
        "SELECT EXISTS(SELECT 1 FROM tombstone_features WHERE feature_hash=?1 AND tombstone_id=?2)",
    )?;
    let mut rows = tombstones.query([tape])?;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let event: u64 = r.get(1)?;
        let file: String = r.get(2)?;
        let start: u64 = r.get(3)?;
        let end: u64 = r.get(4)?;
        let timestamp: String = r.get(5)?;
        let ordinal: u64 = r.get(6)?;
        let anchor: String = r.get(7)?;
        emit(
            &mut local,
            global,
            "tombstone",
            &[
                s(tape),
                u(event),
                s(&file),
                u(start),
                u(end),
                s(&timestamp),
                u(ordinal),
                s(&anchor),
            ],
        );
        local.inc("tombstone_windows", 1);
        for feature in features(&anchor)? {
            if !posting.query_row(params![feature, id], |r| r.get::<_, bool>(0))? {
                return Err("missing stored tombstone posting".into());
            }
            emit(
                &mut local,
                global,
                "tombstone_feature",
                &[s(&feature), s(tape), u(event), u(ordinal)],
            );
            emit(
                &mut local,
                global,
                "legacy_tombstone_journey",
                &[
                    s(&feature),
                    s(tape),
                    u(event),
                    u(ordinal),
                    s(&anchor),
                    s(&file),
                    u(start),
                    u(end),
                    s(&timestamp),
                ],
            );
            local.inc("tombstone_features", 1);
            local.inc("legacy_journey_rows", 1);
        }
    }
    let mut dispatch=conn.prepare("SELECT uuid,first_turn_index,direction FROM dispatch_links WHERE tape_id=?1 ORDER BY first_turn_index,uuid")?;
    let mut rows = dispatch.query([tape])?;
    while let Some(r) = rows.next()? {
        emit(
            &mut local,
            global,
            "dispatch",
            &[
                s(tape),
                s(&r.get::<_, String>(0)?),
                i(r.get(1)?),
                s(&r.get::<_, String>(2)?),
            ],
        );
        local.inc("dispatch_rows", 1);
    }
    Ok(local)
}
fn features(anchor: &str) -> ProofResult<Vec<String>> {
    let raw = anchor
        .strip_prefix("winnow:")
        .ok_or("non-winnow oracle anchor")?;
    let features = raw
        .split(',')
        .map(|v| format!("winnow:{v}"))
        .collect::<Vec<_>>();
    if raw.is_empty() || features.iter().collect::<BTreeSet<_>>().len() != features.len() {
        return Err("invalid/duplicate anchor features".into());
    }
    Ok(features)
}

pub fn verify(
    db: &Path,
    tape_root: &Path,
    tapes: &[String],
    oracle_root: &Path,
    output: &Path,
) -> ProofResult<()> {
    verify_inputs(oracle_root)?;
    fs::create_dir_all(output)?;
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut expected =
        BufReader::new(File::open(oracle_root.join("per-tape-oracle.jsonl"))?).lines();
    let mut retained = BufWriter::new(File::create(output.join("per-tape-comparison.jsonl"))?);
    let mut global = Oracle::new("global");
    let mut passed = true;
    for (ordinal, tape) in tapes.iter().enumerate() {
        let wanted: Value =
            serde_json::from_str(&expected.next().ok_or("missing per-tape oracle row")??)?;
        let mut actual = stored_tape(&conn, tape, &mut global)?;
        let path = tape_root.join(format!("{tape}.jsonl.zst"));
        let content = crate::store::tapes::read_tape_content(&path).map_err(|e| e.message)?;
        actual.inc(
            "normalized_events",
            content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count() as u64,
        );
        for (key, count) in &actual.metrics {
            global.inc(key, *count);
        }
        let mut record = actual.summary();
        record["ordinal"] = json!(ordinal);
        record["tape_id"] = json!(tape);
        record["compressed_bytes"] = json!(fs::metadata(&path)?.len());
        record["compressed_sha256"] = json!(sha256_file(&path)?);
        record["normalized_bytes"] = json!(content.len());
        record["normalized_sha256"] = json!(t1772::hex_digest(
            Sha256::new().chain_update(content.as_bytes())
        ));
        let equal = record == wanted && record["normalized_sha256"] == *tape;
        passed &= equal;
        retained.write_all(&canonical_json_lf(
            &json!({"actual":record,"expected":wanted,"equal":equal}),
        )?)?;
    }
    retained.flush()?;
    retained.get_ref().sync_all()?;
    passed &= expected.next().is_none();
    let wanted: Value =
        serde_json::from_reader(File::open(oracle_root.join("global-oracle.json"))?)?;
    let actual = global.summary();
    let mut comparisons = BTreeMap::new();
    for key in ["categories", "combined", "semantic_edges", "metrics"] {
        let equal = actual[key] == wanted[key];
        passed &= equal;
        comparisons.insert(key, equal);
    }
    // Every expected posting was looked up by its full primary key. Counts
    // exclude extra/orphan postings without a repeated per-window table scan.
    for (table, metric) in [
        ("evidence_features", "evidence_features"),
        ("tombstone_features", "tombstone_features"),
    ] {
        let count: u64 =
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
        passed &= count == global.metrics[metric];
        comparisons.insert(table, count == global.metrics[metric]);
    }
    write_canonical_json(
        &output.join("global-comparison.json"),
        &json!({"passed":passed,"actual":actual,"expected":wanted,"comparisons":comparisons,
        "legacy_digest_format":"ENGRAM-T1772-P0-ORACLE-v1 typed big-endian length frames; frozen category and physical/feature order",
        "artifact_preimage":t1772::CANONICAL_BYTES_CONTRACT}),
    )?;
    if !passed {
        return Err(
            "frozen global/per-tape canonical oracle mismatch; comparisons retained".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_empty_tape_framing_and_full_field_mutation() {
        let tape = "0000cfadf68a37aee8ec89ff70e58f08b01d43bf7e74532583f980ca789e1e69";
        let oracle = Oracle::new(&format!("tape:{tape}"));
        let summary = oracle.summary();
        assert_eq!(
            summary["combined"]["sha256"],
            "b7955727f8266d2eec88d49278034b86cee305d01da017c3fe7650b920ac289a"
        );
        assert_eq!(
            summary["semantic_edges"]["sha256"],
            "05a0fde191b65715b0789a311538d65747f6c99f2ce4eace50e3f8d46910d690"
        );
        assert_ne!(frame(&[u(1)]), frame(&[i(1)]));
        assert_ne!(frame(&[s("ab"), s("c")]), frame(&[s("a"), s("bc")]));
    }
    #[test]
    fn stored_fields_and_posting_membership_are_not_replaced_by_counts() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        drop(crate::index::SqliteIndex::open_writer(db.to_str().unwrap()).unwrap());
        let conn = Connection::open(db).unwrap();
        conn.execute_batch("INSERT INTO evidence_windows VALUES(1,'winnow:a,b','t',2,'read','/right','now',0);INSERT INTO evidence_features VALUES('winnow:a',1),('winnow:b',1);").unwrap();
        let before = stored_tape(&conn, "t", &mut Oracle::new("global"))
            .unwrap()
            .summary();
        assert_eq!(
            before["categories"]["evidence_window"]["sha256"],
            "7faaab44d1c1698453f9eaad18e7661a286b7880fe0303b3d1c330045c517ccd"
        );
        conn.execute("UPDATE evidence_windows SET file_path='/wrong'", [])
            .unwrap();
        let after = stored_tape(&conn, "t", &mut Oracle::new("global"))
            .unwrap()
            .summary();
        assert_eq!(before["metrics"], after["metrics"]);
        assert_ne!(before["combined"], after["combined"]);
        conn.execute("UPDATE evidence_features SET feature_hash='winnow:wrong' WHERE feature_hash='winnow:b'",[]).unwrap();
        assert!(stored_tape(&conn, "t", &mut Oracle::new("global")).is_err());
    }
}
