//! Separate SQL observations: these are never counters from the product CLI.
use super::t1772::{ProofResult, write_canonical_json};
use rusqlite::{Connection, Row, StatementStatus};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::path::Path;

pub const COUNTER_SCOPE: &str = "instrumented SQL probe excluded from product CLI wall timing";
const BASELINE_DIRECT: &str = "SELECT tape_id, event_offset, kind, file_path, timestamp FROM evidence WHERE anchor = ?1 ORDER BY timestamp ASC, tape_id ASC, event_offset ASC";
const CANDIDATE_EXACT: &str = "SELECT evidence_id, anchor, tape_id, event_offset, kind, file_path, timestamp FROM evidence_windows WHERE anchor = ?1";
const CANDIDATE_FEATURE: &str = "SELECT w.evidence_id, w.anchor, w.tape_id, w.event_offset, w.kind, w.file_path, w.timestamp FROM evidence_features f JOIN evidence_windows w ON w.evidence_id = f.evidence_id WHERE f.feature_hash = ?1";
const CANDIDATE_SEEDS: &str = "SELECT w.anchor FROM evidence_features f JOIN evidence_windows w ON w.evidence_id = f.evidence_id WHERE f.feature_hash = ?1";

/// The binding is the same object embedded in the adjacent product process row.
/// Run only after its wall timer and temp sampler have stopped. A cold slot's
/// probe follows the product read; it makes no separate cold-latency claim.
pub fn run(db: &Path, binding: &Value, output: &Path) -> ProofResult<Value> {
    let baseline = match binding["variant"].as_str() {
        Some("baseline") => true,
        Some("candidate") => false,
        _ => return Err("unknown probe variant".into()),
    };
    let anchors = binding["derived_anchors"]
        .as_array()
        .ok_or("probe anchors missing")?
        .iter()
        .map(|v| v.as_str().map(str::to_owned).ok_or("invalid anchor"))
        .collect::<Result<Vec<_>, _>>()?;
    if anchors.is_empty() {
        return Err("probe requires derived anchors".into());
    }
    let flags = &binding["flags"];
    let max_depth = flags["depth"].as_u64().ok_or("depth missing")? as usize;
    let max_edges = flags["max_edges"].as_u64().ok_or("edge limit missing")? as usize;
    let max_fanout = flags["max_fanout"].as_u64().ok_or("fanout missing")? as usize;
    let minimum = flags["min_confidence"]
        .as_f64()
        .ok_or("confidence missing")? as f32;
    if flags["forensics"] != false || flags["include_deleted"] != false || flags["pretty"] != false
    {
        return Err("unsupported probe flags".into());
    }
    let conn = super::baseline_custody::open_copy_reader(db)?;
    let mut statements = Vec::new();
    let mut version = 0;
    observe(
        &conn,
        "PRAGMA user_version",
        None,
        "schema",
        &mut statements,
        |row| {
            version = row.get::<_, i64>(0)?;
            Ok(())
        },
    )?;
    if version != if baseline { 3 } else { 4 } {
        return Err("probe schema does not match bound variant".into());
    }
    let mut seeds = Vec::new();
    let mut seen_seeds = HashSet::new();
    for anchor in &anchors {
        if baseline {
            observe(
                &conn,
                BASELINE_DIRECT,
                Some(anchor),
                "direct",
                &mut statements,
                |_| Ok(()),
            )?;
            if seen_seeds.insert(anchor.clone()) {
                seeds.push(anchor.clone());
            }
        } else if anchor.starts_with("span:") {
            if seen_seeds.insert(anchor.clone()) {
                seeds.push(anchor.clone());
            }
        } else if anchor.starts_with("winnow:") {
            let sql = if anchor.contains(',') {
                CANDIDATE_EXACT
            } else {
                CANDIDATE_FEATURE
            };
            observe(&conn, sql, Some(anchor), "direct", &mut statements, |_| {
                Ok(())
            })?;
            let mut matching = Vec::new();
            if anchor.contains(',') {
                matching.push(anchor.clone());
            } else {
                observe(
                    &conn,
                    CANDIDATE_SEEDS,
                    Some(anchor),
                    "posting_seeds",
                    &mut statements,
                    |row| {
                        matching.push(row.get::<_, String>(0)?);
                        Ok(())
                    },
                )?;
                matching.sort();
                matching.dedup();
            }
            for seed in matching {
                if seen_seeds.insert(seed.clone()) {
                    seeds.push(seed);
                }
            }
        }
    }
    // Probe the same bounded provenance expansion. Presentation, tape loading,
    // and query-result recording are outside this SQL counter scope.
    let mut queue: VecDeque<_> = seeds.into_iter().map(|s| (s, 0)).collect();
    let mut visited = HashSet::new();
    let mut seen_edges = HashSet::new();
    let mut selected_edges = 0;
    while let Some((anchor, depth)) = queue.pop_front() {
        if !visited.insert(anchor.clone()) || depth >= max_depth {
            continue;
        }
        if selected_edges >= max_edges {
            break;
        }
        let mut edges = Vec::new();
        for column in ["to_anchor", "from_anchor"] {
            let tie = if baseline { "" } else { ", edge_id ASC" };
            let sql = format!(
                "SELECT from_anchor, to_anchor, confidence, location_delta, cardinality, agent_link, note FROM edges WHERE {column} = ?1 ORDER BY confidence DESC{tie}"
            );
            observe(
                &conn,
                &sql,
                Some(&anchor),
                "lineage",
                &mut statements,
                |row| {
                    let confidence = row.get::<_, f32>(2)?;
                    let agent = row.get::<_, i64>(5)? != 0;
                    if !agent && confidence < minimum {
                        return Ok(());
                    }
                    let from = row.get::<_, String>(0)?;
                    let to = row.get::<_, String>(1)?;
                    let key = (
                        from.clone(),
                        to.clone(),
                        confidence.to_bits(),
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        agent,
                        row.get::<_, String>(6)?,
                    );
                    edges.push((from, to, confidence, key));
                    Ok(())
                },
            )?;
        }
        let mut local = HashSet::new();
        edges.retain(|e| !seen_edges.contains(&e.3) && local.insert(e.3.clone()));
        edges.sort_by(|a, b| b.2.total_cmp(&a.2));
        for (from, to, _, key) in edges.into_iter().take(max_fanout) {
            if selected_edges >= max_edges {
                break;
            }
            seen_edges.insert(key);
            selected_edges += 1;
            let next = if from == anchor { to } else { from };
            if !visited.contains(&next) {
                queue.push_back((next, depth + 1));
            }
        }
    }
    if !statements.iter().any(|r| r["kind"] == "direct") {
        return Err("no direct probe statements".into());
    }
    let total = |kind: &str, field: &str| -> u64 {
        statements
            .iter()
            .filter(|r| r["kind"] == kind)
            .map(|r| r[field].as_u64().unwrap())
            .sum()
    };
    let result = json!({"binding":binding,"counter_scope":COUNTER_SCOPE,
        "database_sha256_at_probe":super::t1772::sha256_file(db)?,
        "cache_observation":"same slot and clone, after product CLI; probe latency is not measured",
        "coverage":"direct evidence, posting seed expansion, bounded provenance edges; excludes presentation, tape I/O, feedback writes",
        "visited_rows_definition":"rows delivered by sqlite3_step to probe, before Rust filters/dedup; not all SQLite internal page or index visits",
        "posting_rows_definition":"candidate feature-join rows delivered; exact/baseline evidence rows reported separately",
        "direct_touch_sort":total("direct", "sort"),"direct_touch_autoindex":total("direct", "autoindex"),
        "direct_rows_visited":total("direct", "rows_visited"),
        "posting_rows_visited":statements.iter().map(|r|r["posting_rows_visited"].as_u64().unwrap()).sum::<u64>(),
        "lineage_rows_visited":total("lineage", "rows_visited"),
        "lineage_edges_selected":selected_edges,"statements":statements});
    write_canonical_json(output, &result)?;
    Ok(result)
}

fn observe(
    conn: &Connection,
    sql: &str,
    anchor: Option<&str>,
    kind: &str,
    observations: &mut Vec<Value>,
    mut visit: impl FnMut(&Row<'_>) -> rusqlite::Result<()>,
) -> ProofResult<()> {
    let mut statement = conn.prepare(sql)?;
    let mut visited = 0_u64;
    {
        let mut rows = match anchor {
            Some(a) => statement.query([a])?,
            None => statement.query([])?,
        };
        while let Some(row) = rows.next()? {
            visited += 1;
            visit(row)?;
        }
    }
    let counter = |status| -> ProofResult<u64> {
        Ok(u64::try_from(statement.get_status(status))
            .map_err(|_| "negative/overflowed statement counter")?)
    };
    observations.push(json!({"ordinal":observations.len(),"kind":kind,"sql":sql,"bound_anchor":anchor,
        "sort":counter(StatementStatus::Sort)?,"autoindex":counter(StatementStatus::AutoIndex)?,
        "fullscan_steps":counter(StatementStatus::FullscanStep)?,"vm_steps":counter(StatementStatus::VmStep)?,
        "rows_visited":visited,"posting_rows_visited":if sql == CANDIDATE_FEATURE || sql == CANDIDATE_SEEDS {visited} else {0}}));
    Ok(())
}
