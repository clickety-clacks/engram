//! Two real ingest/query passes against disposable schema-v4 copies.
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::measurement::{file_bytes_or_absent, percentiles};
use super::t1772::{
    ProofResult, RETAINED_READER_SECONDS, WATCHER_TRANSACTIONS, canonical_json_lf, sha256_bytes,
    write_canonical_json,
};
use crate::dispatch::extract_dispatch_links_from_transcript;
use crate::index::lineage::LINK_THRESHOLD_DEFAULT;
use crate::index::{DispatchLink, SqliteIndex};
use crate::query::format::derive_anchor_candidates;
use crate::store::tapes::read_tape_content;
use crate::tape::event::{TapeEventAt, parse_jsonl_events};

struct Tape {
    id: String,
    events: Vec<TapeEventAt>,
    dispatch: Vec<DispatchLink>,
}

pub fn run(
    source: &Path,
    tape_root: &Path,
    ids: &[String],
    manifest: &Path,
    output: &Path,
) -> ProofResult<()> {
    fs::create_dir(output)?;
    let mut tapes = Vec::new();
    // Exact real frozen tapes, with their real IDs/events/dispatch. They are
    // withheld from each staging copy before observation, then ingested once.
    for id in ids.iter().take(WATCHER_TRANSACTIONS as usize) {
        let text = read_tape_content(&tape_root.join(format!("{id}.jsonl.zst")))
            .map_err(|e| format!("{}: {}", e.code, e.message))?;
        tapes.push(Tape {
            id: id.clone(),
            events: parse_jsonl_events(&text)?,
            dispatch: extract_dispatch_links_from_transcript(&text),
        });
    }
    if tapes.len() != WATCHER_TRANSACTIONS as usize {
        return Err("missing watcher tapes".into());
    }
    let manifest: Value = serde_json::from_reader(File::open(manifest)?)?;
    let mut results = Vec::new();
    for mode in ["retained", "short-query-loop"] {
        let root = output.join(mode);
        fs::create_dir(&root)?;
        let db = root.join("index.sqlite");
        fs::copy(source, &db)?;
        prepare(&db, &tapes)?;
        let result = pass(&db, &tapes, &manifest, mode, &root)?;
        let passed = result["passed"] == true;
        results.push(result);
        if !passed {
            write_canonical_json(
                &output.join("result.json"),
                &json!({"passed":false,"passes":results}),
            )?;
            return Err(format!("{mode} concurrency gate failed; see retained raw result").into());
        }
    }
    write_canonical_json(
        &output.join("result.json"),
        &json!({"passed":true,"passes":results}),
    )?;
    Ok(())
}

fn prepare(db: &Path, tapes: &[Tape]) -> ProofResult<()> {
    let mut conn = Connection::open(db)?;
    conn.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
    )?;
    let tx = conn.transaction()?;
    for tape in tapes {
        for table in [
            "evidence_windows",
            "tombstones",
            "edges",
            "dispatch_links",
            "tapes",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE tape_id=?1"),
                params![tape.id],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn projection(index: &SqliteIndex, anchors: &[String]) -> rusqlite::Result<Value> {
    let rows = index.evidence_for_anchors(anchors)?;
    Ok(json!(
        rows.into_iter()
            .map(|r| json!({"tape_id":r.tape_id,
        "event_offset":r.event_offset,"file_path":r.file_path,"timestamp":r.timestamp,
        "kind":format!("{:?}",r.kind)}))
            .collect::<Vec<_>>()
    ))
}

fn select_query(index: &SqliteIndex, manifest: &Value) -> ProofResult<(String, Vec<String>)> {
    for query in manifest["queries"].as_array().ok_or("missing queries")? {
        let anchors = derive_anchor_candidates(&[query["target"]
            .as_str()
            .ok_or("missing target")?
            .to_string()]);
        let mut contributing = 0;
        for anchor in &anchors {
            if !index.evidence_for_anchor(anchor)?.is_empty() {
                contributing += 1;
            }
        }
        if contributing >= 2 && index.evidence_for_anchors(&anchors)?.len() >= 2 {
            return Ok((
                query["id"].as_str().ok_or("missing query id")?.to_string(),
                anchors,
            ));
        }
    }
    Err("no real multi-posting manifest query remains after withholding watcher tapes".into())
}

fn pass(
    db: &Path,
    tapes: &[Tape],
    manifest: &Value,
    mode: &str,
    output: &Path,
) -> ProofResult<Value> {
    let writer = SqliteIndex::open_writer(db.to_str().ok_or("non-UTF8 db")?)?;
    writer.set_busy_timeout(Duration::ZERO)?;
    // Make WAL sidecars exist while the writer stays alive. This does not add
    // synthetic rows: the separate connection only materializes WAL mode.
    let checkpoint = Connection::open(db)?;
    checkpoint.busy_timeout(Duration::ZERO)?;
    checkpoint.execute_batch("BEGIN IMMEDIATE; PRAGMA user_version=4; COMMIT;")?;
    let selector = SqliteIndex::open_reader(db.to_str().ok_or("non-UTF8 db")?)?;
    let (query_id, anchors) = select_query(&selector, manifest)?;
    drop(selector);
    let retained = mode == "retained";
    let reader_path = db.to_path_buf();
    let reader_ids = tapes.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
    let (ready_send, ready_receive) = mpsc::channel();
    let (reader_stop, stop_receive) = mpsc::channel();
    let reader = thread::spawn(move || -> ProofResult<Value> {
        let index = SqliteIndex::open_reader(reader_path.to_str().ok_or("non-UTF8 db")?)?;
        index.set_busy_timeout(Duration::ZERO)?;
        let operation = |_: &Connection| -> rusqlite::Result<Value> {
            let start = Instant::now();
            let before = projection(&index, &anchors)?;
            for id in &reader_ids {
                if index.has_tape(id)? {
                    return Err(rusqlite::Error::InvalidQuery);
                }
            }
            ready_send
                .send(())
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            stop_receive
                .recv()
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            // Retain through the actual final writer event AND at least 60s.
            thread::sleep(
                Duration::from_secs(RETAINED_READER_SECONDS).saturating_sub(start.elapsed()),
            );
            let after = projection(&index, &anchors)?;
            let mut absent = true;
            for id in &reader_ids {
                absent &= !index.has_tape(id)?;
            }
            Ok(
                json!({"mode":"retained", "elapsed_microseconds":start.elapsed().as_micros(),
                "rows":before.as_array().map(Vec::len),"snapshot_stable":before==after,
                "withheld_tapes_still_absent":absent,"projection_before":before,"projection_after":after}),
            )
        };
        if retained {
            return Ok(index.with_read_transaction(operation)?);
        }
        let start = Instant::now();
        let mut queries = 0u64;
        let mut stopped = false;
        loop {
            index.with_read_transaction(|_| projection(&index, &anchors))?;
            queries += 1;
            if queries == 1 {
                ready_send.send(())?;
            }
            stopped |= !matches!(stop_receive.try_recv(), Err(mpsc::TryRecvError::Empty));
            if stopped && start.elapsed() >= Duration::from_secs(RETAINED_READER_SECONDS) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(
            json!({"mode":"short-query-loop","completed_queries":queries,
            "elapsed_microseconds":start.elapsed().as_micros()}),
        )
    });
    if ready_receive
        .recv_timeout(Duration::from_secs(120))
        .is_err()
    {
        drop(reader_stop);
        return match reader.join() {
            Ok(Err(error)) => Err(error),
            _ => Err("real reader failed to establish snapshot before deadline".into()),
        };
    }
    let (checkpoint_stop, checkpoint_receive) = mpsc::channel();
    let wal = db.with_extension("sqlite-wal");
    let checkpoint_wal = wal.clone();
    let checkpoints = thread::spawn(move || -> ProofResult<Vec<Value>> {
        let start = Instant::now();
        let mut next = start;
        let mut rows = Vec::new();
        loop {
            let stopping = !matches!(
                checkpoint_receive.recv_timeout(next.saturating_duration_since(Instant::now())),
                Err(mpsc::RecvTimeoutError::Timeout)
            );
            let result = checkpoint.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            });
            let (primary, extended, error) = sqlite_result(&result);
            rows.push(json!({"elapsed_microseconds":start.elapsed().as_micros(),
                "after_reader":stopping,"tuple":result.ok().map(|r|[r.0,r.1,r.2]),
                "primary_code":primary,"extended_code":extended,"error":error,
                "retries":0,"wal_bytes":file_bytes_or_absent(&checkpoint_wal)?}));
            if stopping {
                break;
            }
            next += Duration::from_secs(1);
        }
        Ok(rows)
    });
    let began = Instant::now();
    let mut commits = Vec::new();
    // Errors are retained, not silently retried or converted into a success count.
    for (ordinal, tape) in tapes.iter().enumerate() {
        let deadline = began + Duration::from_millis(600 * ordinal as u64);
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let started = Instant::now();
        let result = writer.ingest_tape_events_with_dispatch(
            &tape.id,
            &tape.events,
            &tape.dispatch,
            LINK_THRESHOLD_DEFAULT,
        );
        let elapsed = started.elapsed().as_micros() as u64;
        let (primary, extended, error) = sqlite_result(&result);
        let wal_size = file_bytes_or_absent(&wal);
        commits.push(
            json!({"ordinal":ordinal+1,"tape_id":tape.id,"events":tape.events.len(),
            "start_microseconds":started.duration_since(began).as_micros(),
            "scheduled_microseconds":600_000 * ordinal as u64,
            "elapsed_microseconds":elapsed,"committed":result.is_ok(),
            "primary_code":primary,"extended_code":extended,"error":error,
            "retries":0,"wal_bytes":wal_size.as_ref().ok(),
            "wal_observation_error":wal_size.err().map(|e|e.to_string())}),
        );
    }
    reader_stop.send(()).ok();
    let reader_result = reader.join().map_err(|_| "reader panicked")?;
    // The final passive checkpoint is explicitly ordered after reader completion.
    checkpoint_stop.send(()).ok();
    let checkpoint_rows = checkpoints
        .join()
        .map_err(|_| "checkpoint worker panicked")??;
    let new_reader = SqliteIndex::open_reader(db.to_str().ok_or("non-UTF8 db")?)?;
    let mut all_visible = true;
    for tape in tapes {
        all_visible &= new_reader.has_tape(&tape.id)?;
    }
    let mut histogram = BTreeMap::<String, u64>::new();
    for item in commits.iter().chain(checkpoint_rows.iter()) {
        *histogram
            .entry(item["extended_code"].to_string())
            .or_default() += 1;
    }
    let reader_report = match &reader_result {
        Ok(value) => {
            *histogram.entry("0".into()).or_default() += if retained {
                1
            } else {
                value["completed_queries"].as_u64().unwrap_or(0)
            };
            value.clone()
        }
        Err(error) => {
            let (primary, extended) = error
                .downcast_ref::<rusqlite::Error>()
                .map(sqlite_codes)
                .unwrap_or((-1, -1));
            *histogram.entry(extended.to_string()).or_default() += 1;
            json!({"error":error.to_string(),"primary_code":primary,"extended_code":extended})
        }
    };
    let commit_times = commits
        .iter()
        .filter_map(|c| c["elapsed_microseconds"].as_u64())
        .collect::<Vec<_>>();
    let max_wal = commits
        .iter()
        .chain(checkpoint_rows.iter())
        .filter_map(|c| c["wal_bytes"].as_u64())
        .max();
    let final_tuple = &checkpoint_rows.last().ok_or("no checkpoints")?["tuple"];
    let final_complete = final_tuple[0] == 0 && final_tuple[1] == final_tuple[2];
    let pass = all_visible
        && reader_result.is_ok()
        && (!retained
            || (reader_report["snapshot_stable"] == true
                && reader_report["withheld_tapes_still_absent"] == true))
        && commits
            .iter()
            .all(|c| c["committed"] == true && c["wal_observation_error"].is_null())
        && checkpoint_rows.iter().all(|c| c["primary_code"] == 0)
        && checkpoint_rows
            .iter()
            .filter(|c| c["after_reader"] == false)
            .count()
            >= 2
        && final_complete;
    let report = json!({"passed":pass,"mode":mode,"query_id":query_id,
        "reader":reader_report,"writer_transactions":commits,
        "writer_cadence_milliseconds":600,"checkpoint_interval_milliseconds":1000,
        "latency_scope":"complete one-tape ingestion transaction including COMMIT; tape parsing excluded",
        "commit_microseconds":percentiles(&commit_times)?,"maximum_observed_wal_bytes":max_wal,
        "checkpoint_observations":checkpoint_rows,"extended_code_histogram":histogram,
        "histogram_scope":"each writer transaction attempt, checkpoint call and reader transaction (one retained transaction or each short-loop transaction)",
        "application_retries":0,"sqlite_busy_timeout_milliseconds":0,
        "new_reader_sees_all_commits":all_visible,"final_checkpoint_complete":final_complete,
        "real_workload_sha256":sha256_bytes(&canonical_json_lf(&json!(tapes.iter().map(|t|&t.id).collect::<Vec<_>>()))?)});
    write_canonical_json(&output.join("result.json"), &report)?;
    Ok(report)
}

fn sqlite_codes(error: &rusqlite::Error) -> (i32, i32) {
    match error {
        rusqlite::Error::SqliteFailure(code, _) => (code.extended_code & 255, code.extended_code),
        _ => (-1, -1),
    }
}

fn sqlite_result<T>(result: &rusqlite::Result<T>) -> (i32, i32, Option<String>) {
    match result {
        Ok(_) => (0, 0, None),
        Err(error) => {
            let (primary, extended) = sqlite_codes(error);
            (primary, extended, Some(error.to_string()))
        }
    }
}
