//! Candidate direct-touch SQL only; never counters from the timed product CLI.
use super::t1772::{ProofResult, write_canonical_json};
use rusqlite::StatementStatus;
use serde_json::{Value, json};
use std::path::Path;

pub const COUNTER_SCOPE: &str = "instrumented SQL probe excluded from product CLI wall timing";
const CANDIDATE_EXACT: &str = "SELECT evidence_id, anchor, tape_id, event_offset, kind, file_path, timestamp FROM evidence_windows WHERE anchor = ?1";
const CANDIDATE_FEATURE: &str = "SELECT w.evidence_id, w.anchor, w.tape_id, w.event_offset, w.kind, w.file_path, w.timestamp FROM evidence_features f JOIN evidence_windows w ON w.evidence_id = f.evidence_id WHERE f.feature_hash = ?1";

/// Same slot/database/binding as the completed CLI, before its post-slot custody.
/// Only att_8c6cb697 direct-touch counters and section 9.4 rows/postings are observed.
pub fn run(db: &Path, binding: &Value, output: &Path) -> ProofResult<Value> {
    if binding["variant"] != "candidate" {
        return Err("direct-touch probe requires candidate variant".into());
    }
    let anchors = binding["derived_anchors"]
        .as_array()
        .ok_or("probe anchors missing")?;
    let conn = super::baseline_custody::open_copy_reader(db)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != 4 {
        return Err("candidate probe requires schema v4".into());
    }
    let mut statements = Vec::new();
    for value in anchors {
        let anchor = value.as_str().ok_or("invalid probe anchor")?;
        if !anchor.starts_with("winnow:") {
            continue;
        }
        let sql = if anchor.contains(',') {
            CANDIDATE_EXACT
        } else {
            CANDIDATE_FEATURE
        };
        let mut statement = conn.prepare(sql)?;
        let mut count = 0_u64;
        {
            let mut rows = statement.query([anchor])?;
            while rows.next()?.is_some() {
                count += 1;
            }
        }
        let counter = |status| -> ProofResult<u64> {
            Ok(u64::try_from(statement.get_status(status))
                .map_err(|_| "negative/overflowed statement counter")?)
        };
        statements.push(json!({"ordinal":statements.len(),"kind":"direct","sql":sql,
            "bound_anchor":anchor,"sort":counter(StatementStatus::Sort)?,
            "autoindex":counter(StatementStatus::AutoIndex)?,"rows_visited":count,
            "posting_rows_visited":if sql == CANDIDATE_FEATURE {count}else{0}}));
    }
    let total = |field: &str| -> u64 {
        statements
            .iter()
            .map(|r| r[field].as_u64().expect("observed u64"))
            .sum()
    };
    let result = json!({"binding":binding,"counter_scope":COUNTER_SCOPE,
        "database_identity":"adjacent pre/post full-slot custody; no independent probe rehash",
        "coverage":"candidate direct exact-anchor and feature-posting lookup only",
        "visited_rows_definition":"rows delivered by sqlite3_step before Rust deduplication; not internal page/index visits",
        "posting_rows_definition":"feature-join rows delivered once; exact-anchor rows are not postings",
        "direct_touch_sort":total("sort"),"direct_touch_autoindex":total("autoindex"),
        "direct_rows_visited":total("rows_visited"),"posting_rows_visited":total("posting_rows_visited"),
        "statements":statements});
    write_canonical_json(output, &result)?;
    validate_candidate_direct_statements(&result)?;
    Ok(result)
}

/// Validate raw statement coverage and zeros for each warmup/measured candidate
/// slot. This is deliberately restricted to the existing exact/feature probes.
pub fn validate_candidate_direct_statements(report: &Value) -> ProofResult<()> {
    let anchors = report["binding"]["derived_anchors"]
        .as_array()
        .ok_or("probe anchors absent")?;
    let statements = report["statements"]
        .as_array()
        .ok_or("raw probe statements absent")?;
    let required = anchors
        .iter()
        .filter_map(Value::as_str)
        .filter(|a| a.starts_with("winnow:"))
        .collect::<Vec<_>>();
    let direct = statements
        .iter()
        .filter(|s| s["kind"] == "direct")
        .collect::<Vec<_>>();
    if required.is_empty() || direct.len() != required.len() {
        return Err("candidate direct-touch probe coverage incomplete".into());
    }
    for anchor in required {
        let matching = direct
            .iter()
            .filter(|s| s["bound_anchor"] == anchor)
            .collect::<Vec<_>>();
        let expected_sql = if anchor.contains(',') {
            CANDIDATE_EXACT
        } else {
            CANDIDATE_FEATURE
        };
        if matching.len() != 1
            || matching[0]["sql"] != expected_sql
            || matching[0]["sort"].as_u64() != Some(0)
            || matching[0]["autoindex"].as_u64() != Some(0)
        {
            return Err(
                "candidate direct-touch probe missing, mismatched or nonzero SORT/AUTOINDEX".into(),
            );
        }
    }
    Ok(())
}
