//! Read-only stdio owner endpoint used by a selected peer transport.
//!
//! This module deliberately has no command execution, SQL transport, config
//! creation, or write operation. A process opens only the named exports in its
//! home-only topology and keeps their read snapshots until stdin closes.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::SystemTime;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{FileAddress, FileKind, MAX_NON_FILE_RESPONSE_BYTES, MachineRef};
use crate::config::{Topology, TopologyExport, load_topology};
use crate::index::{
    QUERY_SEMANTICS_VERSION, ReaderMode, SCHEMA_VERSION, SqliteIndex, semantic_edge_key,
};
use crate::query::format::{
    DateFilter, collect_files_touched_from_rows, edge_to_json, extract_latest_timestamp_from_rows,
    session_matches_date_filter,
};
use crate::store::tapes::{TapeRow, parse_jsonl_rows};
use crate::tape::compress::decompress_jsonl_with_limit;
use crate::tape::grep::{
    GrepTapeSummary, grep_line_matches, scan_grep_reader, scan_parallel_in_order,
};

pub const PROTOCOL_VERSION: u64 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_BATCH_ITEMS: usize = 128;
pub const MAX_READ_FILE_BYTES: u64 = 256 * 1024 * 1024;
const READ_FILE_CHUNK_BYTES: usize = 720 * 1024;
const DEFAULT_OWNER_SESSION_MAX_SECS: u64 = 120;
const DEFAULT_OWNER_IDLE_TIMEOUT_SECS: u64 = 15;

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

struct OpenExport {
    config: TopologyExport,
    index: SqliteIndex,
    snapshot_at: String,
}

struct PeerSession {
    topology: Topology,
    opened: BTreeMap<String, OpenExport>,
    tape_file_sizes: RefCell<HashMap<PathBuf, Option<u64>>>,
}

#[derive(Debug)]
struct PeerError {
    code: &'static str,
    message: String,
}

impl PeerError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<rusqlite::Error> for PeerError {
    fn from(error: rusqlite::Error) -> Self {
        Self::new("reader_unavailable", error.to_string())
    }
}

/// Serve the bounded newline-delimited JSON protocol on stdin/stdout.
pub fn serve_stdio(home: &Path) -> Result<(), String> {
    crate::platform::ensure_peer_commands_supported().map_err(|error| error.message)?;
    let topology = load_topology(home)
        .map_err(|error| format!("topology_error: {error}"))?
        .ok_or_else(|| "topology_missing: expected ~/.engram/topology.yml".to_string())?;
    let owner_session_max = Duration::from_secs(configured_limit(
        &topology.limits,
        "owner_session_max_secs",
        DEFAULT_OWNER_SESSION_MAX_SECS,
    ));
    let owner_idle_timeout = Duration::from_secs(configured_limit(
        &topology.limits,
        "owner_idle_timeout_secs",
        DEFAULT_OWNER_IDLE_TIMEOUT_SECS,
    ));
    let mut session = PeerSession {
        topology,
        opened: BTreeMap::new(),
        tape_file_sizes: RefCell::new(HashMap::new()),
    };
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    let started = Instant::now();
    let (watchdog_stop_tx, watchdog_stop_rx) = mpsc::channel();
    let watchdog = thread::spawn(move || {
        if matches!(
            watchdog_stop_rx.recv_timeout(owner_session_max),
            Err(RecvTimeoutError::Timeout)
        ) {
            // This endpoint is a short-lived child process. A hard exit closes
            // any pinned SQLite readers even if an owner operation is still
            // running and cannot observe a caller-side cancellation.
            std::process::exit(0);
        }
    });

    let (frames_tx, frames_rx) = mpsc::channel::<Result<Option<Vec<u8>>, String>>();
    let reader = thread::spawn(move || {
        let stdin = io::stdin();
        let mut input = BufReader::new(stdin.lock());
        loop {
            let result = read_frame(&mut input).map_err(|error| error.to_string());
            let finished = !matches!(result, Ok(Some(_)));
            if frames_tx.send(result).is_err() || finished {
                break;
            }
        }
    });

    let result = serve_request_frames(
        frames_rx,
        &mut output,
        started,
        owner_session_max,
        owner_idle_timeout,
        |value, id, output| session.handle(value, id, output),
    );

    let _ = watchdog_stop_tx.send(());
    let _ = watchdog.join();
    // On idle expiry stdin may still be open. The peer command exits after
    // this function returns, which closes the detached reader thread too.
    drop(reader);
    result
}

fn serve_request_frames<W, F>(
    frames_rx: mpsc::Receiver<Result<Option<Vec<u8>>, String>>,
    output: &mut W,
    started: Instant,
    owner_session_max: Duration,
    owner_idle_timeout: Duration,
    mut handle: F,
) -> Result<(), String>
where
    W: Write,
    F: FnMut(&Value, &Value, &mut W) -> Result<Value, PeerError>,
{
    let mut last_activity = Instant::now();
    loop {
        let idle_remaining = owner_idle_timeout.saturating_sub(last_activity.elapsed());
        let session_remaining = owner_session_max.saturating_sub(started.elapsed());
        let wait = idle_remaining.min(session_remaining);
        let request = match frames_rx.recv_timeout(wait) {
            Ok(Ok(Some(frame))) => frame,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(error)) => return Err(format!("frame_error: {error}")),
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Err(RecvTimeoutError::Timeout) => {
                if owner_idle_timeout <= last_activity.elapsed()
                    || owner_session_max <= started.elapsed()
                {
                    return Ok(());
                }
                continue;
            }
        };
        let parsed = serde_json::from_slice::<Value>(&request);
        let (id, result) = match parsed {
            Ok(value) => {
                let id = value.get("id").cloned().unwrap_or(Value::Null);
                let result = handle(&value, &id, output);
                (id, result)
            }
            Err(error) => (
                Value::Null,
                Err(PeerError::new("invalid_json", error.to_string())),
            ),
        };
        let response = match result {
            Ok(stats) => write_terminal(output, &id, true, stats, None)
                .map_err(|error| format!("write_error: {error}")),
            Err(error) => write_terminal(
                output,
                &id,
                false,
                Value::Null,
                Some(json!({"code": error.code, "message": error.message})),
            )
            .map_err(|write_error| format!("write_error: {write_error}")),
        };
        if let Err(error) = response {
            return Err(error);
        }
        // A completed response is activity too. Long owner operations must
        // not leave the next request with an already-expired idle deadline.
        last_activity = Instant::now();
    }
}

fn read_frame<R: BufRead>(input: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unterminated request frame",
            ));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if frame.len().saturating_add(take) > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request frame exceeds 1 MiB",
            ));
        }
        let ended = available.get(take.saturating_sub(1)) == Some(&b'\n');
        frame.extend_from_slice(&available[..take]);
        input.consume(take);
        if ended {
            if frame.last() == Some(&b'\n') {
                frame.pop();
            }
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(Some(frame));
        }
    }
}

impl PeerSession {
    fn batch_cap(&self) -> usize {
        configured_limit(
            &self.topology.limits,
            "items_per_batch",
            MAX_BATCH_ITEMS as u64,
        )
        .clamp(1, MAX_BATCH_ITEMS as u64) as usize
    }

    fn handle<W: Write>(
        &mut self,
        request: &Value,
        id: &Value,
        output: &mut W,
    ) -> Result<Value, PeerError> {
        let object = request
            .as_object()
            .ok_or_else(|| PeerError::new("invalid_request", "request must be an object"))?;
        reject_unknown_keys(object, &["v", "id", "op", "stores", "args"])?;
        if request.get("v").and_then(Value::as_u64) != Some(PROTOCOL_VERSION) {
            return Err(PeerError::new(
                "protocol_mismatch",
                "unsupported protocol version",
            ));
        }
        if request.get("id").and_then(Value::as_u64).is_none() {
            return Err(PeerError::new(
                "invalid_request",
                "id must be a non-negative integer",
            ));
        }
        let op = request
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| PeerError::new("invalid_request", "op must be a string"))?;
        let stores = string_batch(request.get("stores"), "stores")?;
        let args = request
            .get("args")
            .and_then(Value::as_object)
            .ok_or_else(|| PeerError::new("invalid_request", "args must be an object"))?;

        match op {
            "open" => self.open(&stores, args, id, output),
            "locate_tapes" => self.locate_tapes(&stores, args, id, output),
            "dispatch_rows" => self.dispatch_rows(&stores, args, id, output),
            "lookup_anchors" => self.lookup_anchors(&stores, args, id, output),
            "lookup_edges" => self.lookup_edges(&stores, args, id, output),
            "tape_facts" => self.tape_facts(&stores, args, id, output),
            "grep_scan" => self.grep_scan(&stores, args, id, output),
            "peek_lines" => self.peek_lines(&stores, args, id, output),
            "read_file" => self.read_file(&stores, args, id, output),
            _ => Err(PeerError::new(
                "unknown_operation",
                format!("unsupported operation `{op}`"),
            )),
        }
    }

    fn open<W: Write>(
        &mut self,
        stores: &[String],
        args: &serde_json::Map<String, Value>,
        id: &Value,
        output: &mut W,
    ) -> Result<Value, PeerError> {
        reject_unknown_keys(args, &[])?;
        let mut opened = 0usize;
        for name in stores {
            let Some(config) = self.topology.exports.get(name).cloned() else {
                write_data(output, id, json!({"store": name, "status": "not_exported"}))?;
                continue;
            };
            if !self.opened.contains_key(name) {
                let db = config.db.to_string_lossy();
                let index = match SqliteIndex::open_reader_mode(&db, ReaderMode::Live) {
                    Ok(index) => index,
                    Err(rusqlite::Error::InvalidQuery) => {
                        write_open_failure(
                            output,
                            id,
                            &self.topology.self_label,
                            name,
                            "incompatible",
                            "schema_mismatch",
                            format!(
                                "export schema is not supported; expected schema v{SCHEMA_VERSION}"
                            ),
                        )?;
                        continue;
                    }
                    Err(error) => {
                        write_open_failure(
                            output,
                            id,
                            &self.topology.self_label,
                            name,
                            "unavailable",
                            "reader_unavailable",
                            format!(
                                "cannot open Live reader for {}: {error}; grant SQLite write access to the parent directory so it can create -shm, or declare a stable captured copy under frozen_stores",
                                config.db.display()
                            ),
                        )?;
                        continue;
                    }
                };
                if let Err(error) = index.pin_snapshot() {
                    write_open_failure(
                        output,
                        id,
                        &self.topology.self_label,
                        name,
                        "unavailable",
                        "reader_unavailable",
                        format!(
                            "cannot pin Live reader snapshot for {}: {error}; grant SQLite write access to the parent directory so it can create -shm, or declare a stable captured copy under frozen_stores",
                            config.db.display()
                        ),
                    )?;
                    continue;
                }
                let snapshot_at = chrono::DateTime::<chrono::Utc>::from(SystemTime::now())
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                self.opened.insert(
                    name.clone(),
                    OpenExport {
                        config: config.clone(),
                        index,
                        snapshot_at,
                    },
                );
            }
            let export = self.opened.get(name).expect("inserted export");
            opened += 1;
            write_data(
                output,
                id,
                json!({
                    "store": format!("{}/{}", self.topology.self_label, name),
                    "status": "ok",
                    "db": export.config.db,
                    "tape_dirs": export.config.tape_dirs,
                    "reader_mode": "live",
                    "snapshot_at": export.snapshot_at,
                }),
            )?;
        }
        Ok(json!({
            "self": self.topology.self_label,
            "build": env!("CARGO_PKG_VERSION"),
            "protocol": PROTOCOL_VERSION,
            "schema": SCHEMA_VERSION,
            "query_semantics": QUERY_SEMANTICS_VERSION,
            "features": [crate::access::QUERY_FEATURE_EXACT_SPAN_EDIT_RANKING],
            "limits": {
                "request_timeout_ms": configured_limit(
                    &self.topology.limits,
                    "request_timeout_ms",
                    30_000,
                ),
                "read_file_compressed_bytes": configured_limit(
                    &self.topology.limits,
                    "read_file_compressed_bytes",
                    MAX_READ_FILE_BYTES,
                ),
                "decompressed_bytes_per_tape": configured_limit(
                    &self.topology.limits,
                    "decompressed_bytes_per_tape",
                    512 * 1024 * 1024,
                ),
                "owner_session_max_secs": configured_limit(
                    &self.topology.limits,
                    "owner_session_max_secs",
                    DEFAULT_OWNER_SESSION_MAX_SECS,
                ),
                "owner_idle_timeout_secs": configured_limit(
                    &self.topology.limits,
                    "owner_idle_timeout_secs",
                    DEFAULT_OWNER_IDLE_TIMEOUT_SECS,
                ),
                "non_file_response_bytes": configured_limit(
                    &self.topology.limits,
                    "non_file_response_bytes",
                    MAX_NON_FILE_RESPONSE_BYTES as u64,
                ),
                "items_per_batch": self.batch_cap(),
            },
            "opened": opened,
        }))
    }

    fn locate_tapes<W: Write>(
        &self,
        stores: &[String],
        args: &serde_json::Map<String, Value>,
        id: &Value,
        output: &mut W,
    ) -> Result<Value, PeerError> {
        reject_unknown_keys(args, &["tape_ids"])?;
        let tape_ids =
            string_batch_with_cap(args.get("tape_ids"), "args.tape_ids", self.batch_cap())?;
        for store in stores {
            let export = self.require_open(store)?;
            for tape_id in &tape_ids {
                validate_tape_id(tape_id)?;
                let indexed = export.index.has_tape(tape_id)?;
                let file = self.tape_path(&export.config, tape_id);
                write_data(
                    output,
                    id,
                    json!({
                        "store": format!("{}/{}", self.topology.self_label, store),
                        "tape_id": tape_id,
                        "indexed": indexed,
                        "file": file.as_ref().map(|(path, _)| json!({ "machine": self.topology.self_label, "path": path, "kind": "tape" })),
                        "size_bytes": file.map(|(_, size)| size),
                    }),
                )?;
            }
        }
        Ok(json!({"items": stores.len().saturating_mul(tape_ids.len())}))
    }

    fn dispatch_rows<W: Write>(
        &self,
        stores: &[String],
        args: &serde_json::Map<String, Value>,
        id: &Value,
        output: &mut W,
    ) -> Result<Value, PeerError> {
        reject_unknown_keys(args, &["by_tape", "by_uuid"])?;
        let by_tape = optional_string_batch(args.get("by_tape"), "args.by_tape", self.batch_cap())?;
        let by_uuid = optional_string_batch(args.get("by_uuid"), "args.by_uuid", self.batch_cap())?;
        if by_tape.is_empty() && by_uuid.is_empty() {
            return Err(PeerError::new(
                "invalid_request",
                "dispatch_rows requires by_tape or by_uuid",
            ));
        }
        for store in stores {
            let export = self.require_open(store)?;
            for tape_id in &by_tape {
                validate_tape_id(tape_id)?;
                for row in export.index.dispatch_links_for_tape(tape_id)? {
                    write_dispatch_row(
                        output,
                        id,
                        &self.topology.self_label,
                        store,
                        tape_id,
                        &row.uuid,
                        row.first_turn_index,
                        row.direction,
                    )?;
                }
            }
            for uuid in &by_uuid {
                if uuid.is_empty() || uuid.len() > 255 || uuid.contains('\0') {
                    return Err(PeerError::new("invalid_request", "invalid dispatch UUID"));
                }
                for row in export.index.dispatch_links_for_uuid(uuid)? {
                    write_dispatch_row(
                        output,
                        id,
                        &self.topology.self_label,
                        store,
                        &row.tape_id,
                        &row.uuid,
                        row.first_turn_index,
                        row.direction,
                    )?;
                }
            }
        }
        Ok(json!({ "stores": stores.len() }))
    }

    fn lookup_anchors<W: Write>(
        &self,
        stores: &[String],
        args: &serde_json::Map<String, Value>,
        id: &Value,
        output: &mut W,
    ) -> Result<Value, PeerError> {
        reject_unknown_keys(args, &["anchors", "include_deleted"])?;
        let anchors = anchor_batch_with_cap(args.get("anchors"), "args.anchors", self.batch_cap())?;
        let include_deleted = match args.get("include_deleted") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(PeerError::new(
                    "invalid_request",
                    "args.include_deleted must be a boolean",
                ));
            }
        };
        let response_limit = configured_limit(
            &self.topology.limits,
            "non_file_response_bytes",
            MAX_NON_FILE_RESPONSE_BYTES as u64,
        )
        .saturating_sub(1024);
        let mut response_bytes = 0u64;
        let mut records = 0usize;
        for store in stores {
            let export = self.require_open(store)?;
            let store_ref = format!("{}/{}", self.topology.self_label, store);
            for anchor in &anchors {
                let matching = export.index.matching_window_anchors(anchor)?;
                records += write_anchor_result_frames(
                    output,
                    id,
                    &store_ref,
                    anchor,
                    &matching,
                    &mut response_bytes,
                    response_limit,
                )?;

                for fragment in export.index.evidence_for_anchor(anchor)? {
                    let held = self.tape_path(&export.config, &fragment.tape_id).is_some();
                    write_data_limited(
                        output,
                        id,
                        json!({
                            "type": "fragment",
                            "store": store_ref,
                            "anchor": anchor,
                            "tape_id": fragment.tape_id,
                            "event_offset": fragment.event_offset,
                            "kind": match fragment.kind {
                                crate::index::lineage::EvidenceKind::Edit => "edit",
                                crate::index::lineage::EvidenceKind::Read => "read",
                            },
                            "file_path": fragment.file_path,
                            "timestamp": fragment.timestamp,
                            "held": held,
                        }),
                        &mut response_bytes,
                        response_limit,
                    )?;
                    records += 1;
                }

                if include_deleted {
                    for tombstone in export.index.tombstones_for_anchor(anchor)? {
                        write_data_limited(
                            output,
                            id,
                            json!({
                                "type": "tombstone",
                                "store": store_ref,
                                "query_anchor": anchor,
                                "anchor": tombstone.anchor_hashes.first(),
                                "tape_id": tombstone.tape_id,
                                "event_offset": tombstone.event_offset,
                                "file_path": tombstone.file_path,
                                "range": {
                                    "start": tombstone.range_at_deletion.start,
                                    "end": tombstone.range_at_deletion.end,
                                },
                                "timestamp": tombstone.timestamp,
                            }),
                            &mut response_bytes,
                            response_limit,
                        )?;
                        records += 1;
                    }
                }
            }
        }
        Ok(json!({"records": records}))
    }

    fn lookup_edges<W: Write>(
        &self,
        stores: &[String],
        args: &serde_json::Map<String, Value>,
        id: &Value,
        output: &mut W,
    ) -> Result<Value, PeerError> {
        reject_unknown_keys(args, &["nodes", "min_confidence", "include_forensics"])?;
        let nodes = anchor_batch_with_cap(args.get("nodes"), "args.nodes", self.batch_cap())?;
        let min_confidence = match args.get("min_confidence") {
            None => 0.5,
            Some(value) => value.as_f64().ok_or_else(|| {
                PeerError::new("invalid_request", "args.min_confidence must be a number")
            })? as f32,
        };
        if !min_confidence.is_finite() || !(0.0..=1.0).contains(&min_confidence) {
            return Err(PeerError::new(
                "invalid_request",
                "args.min_confidence must be in [0.0, 1.0]",
            ));
        }
        let include_forensics = match args.get("include_forensics") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(PeerError::new(
                    "invalid_request",
                    "args.include_forensics must be a boolean",
                ));
            }
        };
        let response_limit = configured_limit(
            &self.topology.limits,
            "non_file_response_bytes",
            MAX_NON_FILE_RESPONSE_BYTES as u64,
        )
        .saturating_sub(1024);
        let mut response_bytes = 0u64;
        let mut records = 0usize;
        for store in stores {
            let export = self.require_open(store)?;
            let store_ref = format!("{}/{}", self.topology.self_label, store);
            for node in &nodes {
                write_data_limited(
                    output,
                    id,
                    json!({"type": "node_result", "store": store_ref, "node": node}),
                    &mut response_bytes,
                    response_limit,
                )?;
                records += 1;
                let mut seen = HashSet::new();
                let mut edges =
                    export
                        .index
                        .inbound_edges(node, min_confidence, include_forensics)?;
                edges.extend(export.index.outbound_edges(
                    node,
                    min_confidence,
                    include_forensics,
                )?);
                for edge in edges {
                    if !seen.insert(semantic_edge_key(&edge)) {
                        continue;
                    }
                    let mut record = edge_to_json(&edge);
                    record["type"] = json!("edge");
                    record["store"] = json!(store_ref);
                    record["node"] = json!(node);
                    write_data_limited(output, id, record, &mut response_bytes, response_limit)?;
                    records += 1;
                }
            }
        }
        Ok(json!({"records": records}))
    }

    fn tape_facts<W: Write>(
        &self,
        stores: &[String],
        args: &serde_json::Map<String, Value>,
        id: &Value,
        output: &mut W,
    ) -> Result<Value, PeerError> {
        reject_unknown_keys(args, &["items"])?;
        if stores.len() != 1 {
            return Err(PeerError::new(
                "invalid_request",
                "tape_facts requires exactly one store",
            ));
        }
        let items = args
            .get("items")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty() && items.len() <= self.batch_cap())
            .ok_or_else(|| {
                PeerError::new(
                    "invalid_request",
                    format!("args.items must contain 1..={} items", self.batch_cap()),
                )
            })?;
        let export = self.require_open(&stores[0])?;
        let store_ref = format!("{}/{}", self.topology.self_label, stores[0]);
        let response_limit = configured_limit(
            &self.topology.limits,
            "non_file_response_bytes",
            MAX_NON_FILE_RESPONSE_BYTES as u64,
        )
        .saturating_sub(1024);
        let mut response_bytes = 0u64;
        let mut seen_tapes = HashSet::new();
        let mut recovery = crate::ingest::recovery::QueryRecovery::default();

        for item in items {
            let item = item.as_object().ok_or_else(|| {
                PeerError::new("invalid_request", "args.items entries must be objects")
            })?;
            reject_unknown_keys(
                item,
                &[
                    "tape_id",
                    "edit_offsets",
                    "turns",
                    "anchor_offsets",
                    "grep_filter",
                    "window_lines",
                    "include_digest",
                    "rank_span",
                ],
            )?;
            let tape_id = item.get("tape_id").and_then(Value::as_str).ok_or_else(|| {
                PeerError::new("invalid_request", "tape_facts item needs a tape_id")
            })?;
            validate_tape_id(tape_id)?;
            if !seen_tapes.insert(tape_id.to_string()) {
                return Err(PeerError::new(
                    "invalid_request",
                    format!("duplicate tape_facts tape_id `{tape_id}`"),
                ));
            }
            let edit_offsets =
                optional_u64_array(item.get("edit_offsets"), "edit_offsets", self.batch_cap())?;
            let turns =
                optional_nonnegative_i64_array(item.get("turns"), "turns", self.batch_cap())?;
            let anchor_offsets = optional_u64_array(
                item.get("anchor_offsets"),
                "anchor_offsets",
                self.batch_cap(),
            )?;
            let rank_span = item.get("rank_span").map(parse_rank_span).transpose()?;
            let grep_filter = optional_string(item.get("grep_filter"), "grep_filter")?;
            let window_lines = optional_usize(item.get("window_lines"), "window_lines")?
                .unwrap_or(30)
                .max(1);
            let include_digest =
                optional_bool(item.get("include_digest"), "include_digest")?.unwrap_or(false);
            if window_lines > 10_000 {
                return Err(PeerError::new(
                    "invalid_request",
                    "window_lines exceeds 10000",
                ));
            }

            let indexed = export.index.has_tape(tape_id)?;
            let Some((path, compressed_size)) = self.tape_path(&export.config, tape_id) else {
                let mut failure = tape_facts_failure(
                    &store_ref,
                    tape_id,
                    indexed,
                    "unavailable",
                    "tape_unavailable",
                    "tape is not present in this export".into(),
                    None,
                );
                failure["summary"] = empty_tape_summary();
                write_data_limited(output, id, failure, &mut response_bytes, response_limit)?;
                continue;
            };
            let raw_text = match read_tape_for_query(&path, compressed_size, &self.topology.limits)
            {
                Ok(raw_text) => raw_text,
                Err(error) => {
                    write_data_limited(
                        output,
                        id,
                        tape_facts_failure(
                            &store_ref,
                            tape_id,
                            indexed,
                            "failed",
                            error.code,
                            error.message,
                            None,
                        ),
                        &mut response_bytes,
                        response_limit,
                    )?;
                    continue;
                }
            };
            let digest =
                include_digest.then(|| format!("{:x}", Sha256::digest(raw_text.as_bytes())));
            let rows = match parse_jsonl_rows(&raw_text) {
                Ok(rows) => rows,
                Err(error) => {
                    write_data_limited(
                        output,
                        id,
                        tape_facts_failure(
                            &store_ref,
                            tape_id,
                            indexed,
                            "failed",
                            "invalid_tape",
                            error.message,
                            digest.as_deref(),
                        ),
                        &mut response_bytes,
                        response_limit,
                    )?;
                    continue;
                }
            };
            let span_edit_match = rank_span.as_ref().is_some_and(|(file, start, end)| {
                edit_offsets.iter().any(|offset| {
                    rows.iter()
                        .find(|row| row.offset == *offset)
                        .is_some_and(|row| {
                            crate::query::format::structured_edit_overlaps_span(
                                &row.value, file, *start, *end,
                            )
                        })
                })
            });
            let (segment, previous_tape_id) = match tape_segment_metadata(tape_id, &rows) {
                Ok(segment) => segment,
                Err(error) => {
                    write_data_limited(
                        output,
                        id,
                        tape_facts_failure(
                            &store_ref,
                            tape_id,
                            indexed,
                            "failed",
                            error.code,
                            error.message,
                            digest.as_deref(),
                        ),
                        &mut response_bytes,
                        response_limit,
                    )?;
                    continue;
                }
            };

            let locator = recovery.lookup_with_reader(
                &export.config.tape_dirs,
                tape_id,
                response_limit,
                |context_path| {
                    let Some(size) = self.memoized_file_size(context_path) else {
                        return Err(crate::CliError::new(
                            "tape_unavailable",
                            format!("recovery context is missing: {}", context_path.display()),
                        ));
                    };
                    read_tape_for_query(context_path, size, &self.topology.limits)
                        .map_err(|error| crate::CliError::new(error.code, error.message))
                },
            );
            let locator = match locator {
                Ok(locator) => locator,
                Err(error) => {
                    return Err(PeerError::new(
                        error.code,
                        format!("{store_ref}: {}", error.message),
                    ));
                }
            };
            if let Some(locator) = locator
                && let Some(offset) = edit_offsets.iter().find(|offset| {
                    !locator
                        .recovered
                        .points
                        .iter()
                        .any(|point| point.old_offset == **offset)
                })
            {
                return Err(PeerError::new(
                    "native_recovery_error",
                    format!("{store_ref}: edit offset {offset} is missing from recovery points"),
                ));
            }

            let total_lines = raw_text.lines().count();
            let anchor_offset = anchor_offsets
                .iter()
                .filter(|offset| rows.iter().any(|row| row.offset == **offset))
                .min()
                .copied();
            let anchor_line = anchor_offset
                .and_then(|offset| usize::try_from(offset).ok())
                .map(|offset| offset.saturating_add(1))
                .unwrap_or(1);
            let default_before = window_lines.saturating_mul(3) / 4;
            let window_start = anchor_line.saturating_sub(default_before).max(1);
            let window_end = if total_lines == 0 {
                0
            } else {
                usize::min(
                    total_lines,
                    window_start.saturating_add(window_lines).saturating_sub(1),
                )
            };
            let grep_filter_hits_window = if let Some(pattern) = grep_filter.as_ref() {
                Some(
                    raw_text
                        .lines()
                        .skip(window_start.saturating_sub(1))
                        .take(window_end.saturating_sub(window_start).saturating_add(1))
                        .try_fold(false, |matched, line| {
                            if matched {
                                Ok(true)
                            } else {
                                grep_line_matches(line, pattern)
                                    .map_err(|error| PeerError::new("invalid_tape", error.message))
                            }
                        })?,
                )
            } else {
                None
            };
            let summary = json!({
                "total_lines": total_lines,
                "anchor_line": anchor_line,
                "window_start": window_start,
                "window_end": window_end,
                "grep_filter_hits_window": grep_filter_hits_window,
                "latest_timestamp": extract_latest_timestamp_from_rows(&rows),
                "files_touched": collect_files_touched_from_rows(&rows),
            });

            let segment_turn_start = segment["message_turn_start"].as_i64().unwrap_or(0);
            let edit_offset_to_turn = edit_offsets
                .iter()
                .map(|offset| {
                    let present = rows.iter().any(|row| row.offset == *offset);
                    let segment_turn = rows
                        .iter()
                        .filter(|row| {
                            row.offset < *offset && crate::dispatch::is_message_row(&row.value)
                        })
                        .count() as i64;
                    let point = locator.and_then(|locator| {
                        locator
                            .recovered
                            .points
                            .iter()
                            .find(|point| point.old_offset == *offset)
                    });
                    let global_turn = point
                        .map(|point| point.turn)
                        .or_else(|| segment_turn_start.checked_add(segment_turn));
                    json!({
                        "event_offset": offset,
                        "present": present,
                        "segment_turn": if present { Some(segment_turn) } else { None },
                        "turn": if present { global_turn } else { None },
                        "recovered_source_offset": point.map(|point| point.source_offset),
                    })
                })
                .collect::<Vec<_>>();
            let turn_to_offset = turns
                .iter()
                .map(|turn| {
                    let offset = crate::dispatch::message_turn_to_event_offset(&rows, *turn);
                    let recovered_source_offset = locator.and_then(|locator| {
                        locator
                            .recovered
                            .points
                            .iter()
                            .find(|point| point.turn == *turn)
                            .map(|point| point.source_offset)
                    });
                    json!({
                        "turn": turn,
                        "offset": offset,
                        "global_turn": segment_turn_start.checked_add(*turn),
                        "recovered_source_offset": recovered_source_offset,
                    })
                })
                .collect::<Vec<_>>();
            let dispatch_event_times = crate::dispatch::first_dispatch_event_times(&rows, &turns);
            let recovery_binding = locator.map(|locator| {
                json!({
                    "verified": true,
                    "context_tape": locator.context_tape,
                    "points": locator.recovered.points.iter()
                        .filter(|point| edit_offsets.contains(&point.old_offset))
                        .map(|point| json!({
                            "old_offset": point.old_offset,
                            "source_offset": point.source_offset,
                            "turn": point.turn,
                        }))
                        .collect::<Vec<_>>(),
                })
            });

            let mut predecessor_chain = vec![segment.clone()];
            let mut chain_status = "complete";
            let mut unresolved_predecessor = None::<String>;
            let mut visited = HashSet::from([tape_id.to_string()]);
            let max_segments = configured_limit(
                &self.topology.limits,
                "predecessor_segments_per_history",
                256,
            )
            .max(1) as usize;
            let mut next_id = previous_tape_id;
            while let Some(previous) = next_id {
                if predecessor_chain.len() >= max_segments {
                    chain_status = "over_limit";
                    unresolved_predecessor = Some(previous);
                    break;
                }
                validate_tape_id(&previous)?;
                if !visited.insert(previous.clone()) {
                    chain_status = "cycle";
                    unresolved_predecessor = Some(previous);
                    break;
                }
                let Some((previous_path, previous_size)) =
                    self.tape_path(&export.config, &previous)
                else {
                    chain_status = "missing_predecessor";
                    unresolved_predecessor = Some(previous);
                    break;
                };
                let previous_text =
                    match read_tape_for_query(&previous_path, previous_size, &self.topology.limits)
                    {
                        Ok(text) => text,
                        Err(error) => {
                            write_data_limited(
                                output,
                                id,
                                tape_facts_failure(
                                    &store_ref,
                                    tape_id,
                                    indexed,
                                    "failed",
                                    error.code,
                                    error.message,
                                    digest.as_deref(),
                                ),
                                &mut response_bytes,
                                response_limit,
                            )?;
                            chain_status = "failed";
                            break;
                        }
                    };
                let previous_rows = match parse_jsonl_rows(&previous_text) {
                    Ok(rows) => rows,
                    Err(error) => {
                        write_data_limited(
                            output,
                            id,
                            tape_facts_failure(
                                &store_ref,
                                tape_id,
                                indexed,
                                "failed",
                                "invalid_tape",
                                error.message,
                                digest.as_deref(),
                            ),
                            &mut response_bytes,
                            response_limit,
                        )?;
                        chain_status = "failed";
                        break;
                    }
                };
                let (previous_segment, previous_id) =
                    match tape_segment_metadata(&previous, &previous_rows) {
                        Ok(segment) => segment,
                        Err(error) => {
                            write_data_limited(
                                output,
                                id,
                                tape_facts_failure(
                                    &store_ref,
                                    tape_id,
                                    indexed,
                                    "failed",
                                    error.code,
                                    error.message,
                                    digest.as_deref(),
                                ),
                                &mut response_bytes,
                                response_limit,
                            )?;
                            chain_status = "failed";
                            break;
                        }
                    };
                predecessor_chain.push(previous_segment);
                next_id = previous_id;
            }
            if chain_status == "failed" {
                continue;
            }
            let mut tape_fact = json!({
                "type": "tape_facts",
                "store": store_ref.clone(),
                "tape_id": tape_id,
                "status": "ok",
                "indexed": indexed,
                "segment": segment,
                "predecessor_chain": predecessor_chain,
                "chain_status": chain_status,
                "unresolved_predecessor": unresolved_predecessor,
                "edit_offset_to_turn": edit_offset_to_turn,
                "turn_to_offset": turn_to_offset,
                "dispatch_event_times": dispatch_event_times,
                "recovery_binding": recovery_binding,
                "digest": digest,
                "summary": summary,
            });
            if span_edit_match {
                tape_fact["span_edit_match"] = json!(true);
            }
            write_data_limited(output, id, tape_fact, &mut response_bytes, response_limit)?;
        }
        Ok(json!({"items": items.len()}))
    }

    fn grep_scan<W: Write>(
        &self,
        stores: &[String],
        args: &serde_json::Map<String, Value>,
        id: &Value,
        output: &mut W,
    ) -> Result<Value, PeerError> {
        reject_unknown_keys(args, &["pattern", "since", "until", "k"])?;
        if stores.len() != 1 {
            return Err(PeerError::new(
                "invalid_request",
                "grep_scan requires exactly one store",
            ));
        }
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| PeerError::new("invalid_request", "args.pattern must be a string"))?;
        let k = optional_usize(args.get("k"), "args.k")?.unwrap_or(25);
        let k_limit = configured_limit(&self.topology.limits, "grep_k", 10_000);
        if k as u64 > k_limit {
            return Err(PeerError::new(
                "budget_exceeded",
                format!("grep top-k exceeds configured limit {k_limit}"),
            ));
        }
        let since = optional_string(args.get("since"), "args.since")?;
        let until = optional_string(args.get("until"), "args.until")?;
        let date_filter = DateFilter::parse(since.as_deref(), until.as_deref())
            .map_err(|error| PeerError::new(error.code, error.message))?;

        let export = self.require_open(&stores[0])?;
        let mut indexed_tape_ids = HashSet::new();
        let mut tape_ids = HashSet::new();
        for (tape_id, indexed, referenced) in export.index.grep_tape_ids()? {
            if indexed {
                indexed_tape_ids.insert(tape_id.clone());
            }
            if referenced {
                tape_ids.insert(tape_id);
            }
        }
        let mut tape_paths = HashMap::<String, (PathBuf, u64)>::new();
        for dir in &export.config.tape_dirs {
            let entries = fs::read_dir(dir)
                .map_err(|error| PeerError::new("tape_inventory_error", error.to_string()))?;
            for entry in entries {
                let entry = entry
                    .map_err(|error| PeerError::new("tape_inventory_error", error.to_string()))?;
                let path = entry.path();
                if let Some(tape_id) = crate::store::tapes::tape_id_from_path(&path)
                    && validate_tape_id(&tape_id).is_ok()
                {
                    tape_ids.insert(tape_id.clone());
                    if !tape_paths.contains_key(&tape_id)
                        && let Some(size) = self.memoized_file_size(&path)
                    {
                        tape_paths.insert(tape_id, (path, size));
                    }
                }
            }
        }
        let mut tape_ids = tape_ids.into_iter().collect::<Vec<_>>();
        tape_ids.sort();

        let scan_tasks = tape_ids
            .into_iter()
            .map(|tape_id| {
                let indexed = indexed_tape_ids.contains(&tape_id);
                if let Err(error) = validate_tape_id(&tape_id) {
                    return GrepScanTask {
                        tape_id,
                        indexed,
                        path: None,
                        error: Some((error.code, error.message)),
                    };
                }
                GrepScanTask {
                    path: tape_paths.get(&tape_id).cloned(),
                    tape_id,
                    indexed,
                    error: None,
                }
            })
            .collect::<Vec<_>>();

        let mut top = BinaryHeap::new();
        let mut failures = Vec::new();
        let response_limit = configured_limit(
            &self.topology.limits,
            "non_file_response_bytes",
            MAX_NON_FILE_RESPONSE_BYTES as u64,
        );
        let mut top_bytes = 0u64;
        let mut failure_bytes = 0u64;
        let mut total = 0usize;
        let mut time_min: Option<String> = None;
        let mut time_max: Option<String> = None;
        let store_ref = format!("{}/{}", self.topology.self_label, stores[0]);

        let limits = self.topology.limits.clone();
        let scan_workers = scan_parallel_in_order(
            &scan_tasks,
            |task| {
                if let Some((code, message)) = &task.error {
                    return Err(PeerError::new(*code, message.clone()));
                }
                let Some((path, compressed_size)) = &task.path else {
                    return Err(PeerError::new(
                        "tape_unavailable",
                        "indexed tape is not present in this export's tape directories",
                    ));
                };
                scan_tape_for_grep(path, *compressed_size, &limits, pattern)
            },
            |result| match result {
                Ok(summary) => summary.reorder_weight(),
                Err(error) => (
                    std::mem::size_of::<PeerError>().saturating_add(error.message.capacity()),
                    0,
                ),
            },
            |task, result| {
                let tape_id = task.tape_id.as_str();
                let summary = match result {
                    Ok(summary) => summary,
                    Err(error) => {
                        let failure = json!({
                            "type": "failure",
                            "tape_id": tape_id,
                            "error": {"code": error.code, "message": error.message},
                        });
                        append_grep_failure(
                            &mut failures,
                            failure,
                            id,
                            &mut failure_bytes,
                            top_bytes,
                            response_limit,
                        )?;
                        return Ok(());
                    }
                };
                if summary.match_count == 0
                    || !session_matches_date_filter(
                        &json!({"timestamp": summary.timestamp}),
                        &date_filter,
                    )
                {
                    return Ok(());
                }

                total = total.saturating_add(1);
                if !summary.timestamp.is_empty() {
                    if time_min
                        .as_ref()
                        .is_none_or(|current| summary.timestamp < *current)
                    {
                        time_min = Some(summary.timestamp.clone());
                    }
                    if time_max
                        .as_ref()
                        .is_none_or(|current| summary.timestamp > *current)
                    {
                        time_max = Some(summary.timestamp.clone());
                    }
                }
                let (refs_up, refs_down) = dispatch_ref_counts(&export.index, tape_id)?;
                let record = json!({
                    "type": "match",
                    "store": store_ref.clone(),
                    "tape_id": tape_id,
                    "indexed": task.indexed,
                    "match_count": summary.match_count,
                    "provenance_match_count": summary.provenance_match_count,
                    "provenance_event_count": summary.provenance_event_count,
                    "timestamp": summary.timestamp,
                    "total_lines": summary.total_lines,
                    "anchor_line": summary.anchor_line,
                    "files_touched": summary.files_touched,
                    "refs_up": refs_up,
                    "refs_down": refs_down,
                });
                if k > 0 {
                    let mut candidate = GrepCandidate::from_record(record);
                    let replaces_worst = top.peek().is_some_and(|worst| candidate < *worst);
                    if top.len() < k || replaces_worst {
                        candidate.frame_size = grep_data_frame_size(id, &candidate.record)? as u64;
                        if candidate.frame_size as usize > MAX_FRAME_BYTES {
                            return Err(PeerError::new(
                                "budget_exceeded",
                                "a grep result exceeds the maximum frame size",
                            ));
                        }
                    }
                    if top.len() < k {
                        top_bytes = top_bytes.saturating_add(candidate.frame_size);
                        if top_bytes.saturating_add(failure_bytes) > response_limit {
                            return Err(PeerError::new(
                                "budget_exceeded",
                                format!("grep response exceeds {response_limit} byte limit"),
                            ));
                        }
                        top.push(candidate);
                    } else if replaces_worst {
                        let worst = top.pop().expect("peeked top candidate");
                        let next_bytes = top_bytes
                            .saturating_sub(worst.frame_size)
                            .saturating_add(candidate.frame_size);
                        if next_bytes.saturating_add(failure_bytes) > response_limit {
                            return Err(PeerError::new(
                                "budget_exceeded",
                                format!("grep response exceeds {response_limit} byte limit"),
                            ));
                        }
                        top_bytes = next_bytes;
                        top.push(candidate);
                    }
                }
                Ok(())
            },
        )?;

        let time_range = json!({
            "start": time_min,
            "end": time_max,
        });
        let matches = top.into_sorted_vec();
        let returned = matches.len();
        let failure_count = failures.len();
        let stats = json!({
            "store": store_ref,
            "total": total,
            "returned": returned,
            "time_range": time_range,
            "truncated": total > k,
            "failures": failure_count,
            "workers": scan_workers,
        });
        let records = matches
            .into_iter()
            .map(|candidate| candidate.record)
            .chain(failures)
            .collect::<Vec<_>>();
        let terminal_size = serde_json::to_vec(&json!({
            "id": id,
            "end": true,
            "ok": true,
            "stats": stats,
        }))
        .map_err(|error| PeerError::new("json_error", error.to_string()))?
        .len()
        .saturating_add(1) as u64;
        if top_bytes
            .saturating_add(failure_bytes)
            .saturating_add(terminal_size)
            > response_limit
        {
            return Err(PeerError::new(
                "budget_exceeded",
                format!("grep response exceeds {response_limit} byte limit"),
            ));
        }
        for record in records {
            write_data(output, id, record)?;
        }
        Ok(stats)
    }

    fn peek_lines<W: Write>(
        &self,
        stores: &[String],
        args: &serde_json::Map<String, Value>,
        id: &Value,
        output: &mut W,
    ) -> Result<Value, PeerError> {
        reject_unknown_keys(
            args,
            &[
                "tape_id",
                "start",
                "lines",
                "anchor_turn",
                "before",
                "after",
                "grep_filter",
                "grep_context",
            ],
        )?;
        if stores.len() != 1 {
            return Err(PeerError::new(
                "invalid_request",
                "peek_lines requires exactly one store",
            ));
        }
        let tape_id = args
            .get("tape_id")
            .and_then(Value::as_str)
            .ok_or_else(|| PeerError::new("invalid_request", "args.tape_id must be a string"))?;
        validate_tape_id(tape_id)?;
        let start = optional_usize(args.get("start"), "args.start")?;
        let lines_count = optional_usize(args.get("lines"), "args.lines")?;
        let anchor_turn = optional_i64(args.get("anchor_turn"), "args.anchor_turn")?;
        let before = optional_usize(args.get("before"), "args.before")?.unwrap_or(30);
        let after = optional_usize(args.get("after"), "args.after")?.unwrap_or(10);
        let grep_filter = args
            .get("grep_filter")
            .filter(|value| !value.is_null())
            .map(|value| {
                value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                    PeerError::new("invalid_request", "args.grep_filter must be a string")
                })
            })
            .transpose()?;
        let grep_context = optional_usize(args.get("grep_context"), "args.grep_context")?
            .unwrap_or(5)
            .max(1);
        let selected_modes = usize::from(start.is_some())
            + usize::from(anchor_turn.is_some())
            + usize::from(grep_filter.is_some());
        if selected_modes > 1 {
            return Err(PeerError::new(
                "invalid_request",
                "peek_lines accepts only one of start, anchor_turn, or grep_filter",
            ));
        }
        if start == Some(0) {
            return Err(PeerError::new(
                "invalid_request",
                "args.start is a 1-based line number",
            ));
        }

        let export = self.require_open(&stores[0])?;
        let Some((path, compressed_size)) = self.tape_path(&export.config, tape_id) else {
            return Err(PeerError::new(
                "tape_unavailable",
                format!("tape `{tape_id}` is not present in this export"),
            ));
        };
        let raw_text = read_tape_for_query(&path, compressed_size, &self.topology.limits)?;
        let rows = parse_jsonl_rows(&raw_text)
            .map_err(|error| PeerError::new("invalid_tape", error.message))?;
        let total_lines = raw_text.lines().count();
        let timestamp = extract_latest_timestamp_from_rows(&rows);
        let response_limit = configured_limit(
            &self.topology.limits,
            "non_file_response_bytes",
            MAX_NON_FILE_RESPONSE_BYTES as u64,
        ) as usize;
        let minimum_frame_bytes = 32usize;

        let mut selected_ranges = Vec::<(usize, usize)>::new();
        let (window_start, window_end, selected_count) = if let Some(pattern) = grep_filter {
            let mut selected_count = 0usize;
            for (index, line) in raw_text.lines().enumerate() {
                if !grep_line_matches(line, &pattern)
                    .map_err(|error| PeerError::new("invalid_tape", error.message))?
                {
                    continue;
                }
                let range_start = index.saturating_sub(grep_context);
                let range_end = usize::min(
                    total_lines.saturating_sub(1),
                    index.saturating_add(grep_context),
                );
                if let Some(last) = selected_ranges.last_mut()
                    && range_start <= last.1.saturating_add(1)
                {
                    if range_end > last.1 {
                        selected_count = selected_count.saturating_add(range_end - last.1);
                        last.1 = range_end;
                    }
                } else {
                    selected_count = selected_count
                        .saturating_add(range_end.saturating_sub(range_start).saturating_add(1));
                    selected_ranges.push((range_start, range_end));
                }
                if selected_count > response_limit / minimum_frame_bytes {
                    return Err(PeerError::new(
                        "budget_exceeded",
                        "requested peek exceeds the response budget; narrow --lines, the before/after window, or grep context",
                    ));
                }
            }
            if selected_ranges.is_empty() {
                return Err(PeerError::new("no_results", "grep filter matched no lines"));
            }
            let first = selected_ranges.first().expect("nonempty ranges").0;
            let last = selected_ranges.last().expect("nonempty ranges").1;
            (first + 1, last + 1, selected_count)
        } else {
            let anchor_line = if let Some(turn) = anchor_turn {
                crate::dispatch::message_turn_to_event_offset(&rows, turn)
                    .and_then(|offset| rows.iter().position(|row| row.offset == offset))
                    .map(|position| position + 1)
                    .unwrap_or(1)
            } else {
                1
            };
            if let Some(start) = start {
                let line_count = lines_count.unwrap_or(30).max(1);
                let end = usize::min(
                    total_lines,
                    start.saturating_add(line_count).saturating_sub(1),
                );
                let count = if total_lines == 0 || end == 0 || start > end {
                    0
                } else {
                    selected_ranges.push((start - 1, end - 1));
                    end - start + 1
                };
                (start, end, count)
            } else {
                let range_start = anchor_line.saturating_sub(before).max(1);
                let range_end = usize::min(total_lines, anchor_line.saturating_add(after));
                let count = if total_lines == 0 || range_end == 0 || range_start > range_end {
                    0
                } else {
                    selected_ranges.push((range_start - 1, range_end - 1));
                    range_end - range_start + 1
                };
                (range_start, range_end, count)
            }
        };

        if selected_count == 0 {
            return Err(PeerError::new(
                "no_results",
                format!("tape `{tape_id}` has no lines in the requested window"),
            ));
        }
        if selected_count > response_limit / minimum_frame_bytes {
            return Err(PeerError::new(
                "budget_exceeded",
                "requested peek exceeds the response budget; narrow --lines, the before/after window, or grep context",
            ));
        }
        let stats = json!({
            "tape_id": tape_id,
            "total_lines": total_lines,
            "timestamp": timestamp,
            "window_start": window_start,
            "window_end": window_end,
            "returned": selected_count,
        });
        let mut response_bytes = 0usize;
        let mut data = Vec::with_capacity(selected_count);
        let mut range_index = 0usize;
        for (line_index, text) in raw_text.lines().enumerate() {
            while selected_ranges
                .get(range_index)
                .is_some_and(|(_, range_end)| line_index > *range_end)
            {
                range_index += 1;
            }
            if !selected_ranges
                .get(range_index)
                .is_some_and(|(range_start, range_end)| {
                    line_index >= *range_start && line_index <= *range_end
                })
            {
                continue;
            }
            let item = json!({
                "tape_id": tape_id,
                "line": line_index + 1,
                "text": text,
            });
            let frame_size = serde_json::to_vec(&json!({"id": id, "data": item}))
                .map_err(|error| PeerError::new("json_error", error.to_string()))?
                .len()
                .saturating_add(1);
            if frame_size > MAX_FRAME_BYTES {
                return Err(PeerError::new(
                    "budget_exceeded",
                    "a requested peek line exceeds the frame limit; narrow the line window or grep context",
                ));
            }
            response_bytes = response_bytes.saturating_add(frame_size);
            if response_bytes > response_limit {
                return Err(PeerError::new(
                    "budget_exceeded",
                    "requested peek exceeds the response budget; narrow --lines, the before/after window, or grep context",
                ));
            }
            data.push(item);
        }
        let terminal_size = serde_json::to_vec(&json!({
            "id": id,
            "end": true,
            "ok": true,
            "stats": stats,
        }))
        .map_err(|error| PeerError::new("json_error", error.to_string()))?
        .len()
        .saturating_add(1);
        if response_bytes.saturating_add(terminal_size) > response_limit {
            return Err(PeerError::new(
                "budget_exceeded",
                "requested peek exceeds the response budget; narrow --lines, the before/after window, or grep context",
            ));
        }
        for item in data {
            write_data(output, id, item)?;
        }
        Ok(stats)
    }

    fn read_file<W: Write>(
        &self,
        stores: &[String],
        args: &serde_json::Map<String, Value>,
        id: &Value,
        output: &mut W,
    ) -> Result<Value, PeerError> {
        reject_unknown_keys(args, &["address", "max_bytes"])?;
        if stores.len() != 1 {
            return Err(PeerError::new(
                "invalid_request",
                "read_file requires exactly one store",
            ));
        }
        let address = args
            .get("address")
            .and_then(Value::as_object)
            .ok_or_else(|| PeerError::new("invalid_request", "args.address must be an object"))?;
        reject_unknown_keys(address, &["machine", "path", "kind"])?;
        let machine = address
            .get("machine")
            .and_then(Value::as_str)
            .ok_or_else(|| PeerError::new("invalid_request", "address.machine must be a string"))?;
        let address_path = address
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| PeerError::new("invalid_request", "address.path must be a string"))?;
        let kind = address
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| PeerError::new("invalid_request", "address.kind must be a string"))?;
        let address = FileAddress {
            machine: MachineRef(machine.to_string()),
            path: PathBuf::from(address_path),
            kind: match kind {
                "tape" => FileKind::Tape,
                _ => {
                    return Err(PeerError::new(
                        "invalid_file_address",
                        "file address kind must be tape",
                    ));
                }
            },
        };
        let max_bytes = match args.get("max_bytes") {
            None => MAX_READ_FILE_BYTES,
            Some(value) => value.as_u64().ok_or_else(|| {
                PeerError::new(
                    "invalid_request",
                    "args.max_bytes must be a non-negative integer",
                )
            })?,
        };
        if max_bytes == 0 || max_bytes > MAX_READ_FILE_BYTES {
            return Err(PeerError::new(
                "invalid_request",
                "max_bytes is outside the read_file limit",
            ));
        }
        let max_bytes = max_bytes.min(configured_limit(
            &self.topology.limits,
            "read_file_compressed_bytes",
            MAX_READ_FILE_BYTES,
        ));
        let export = self.require_open(&stores[0])?;
        let (path, tape_id) = resolve_file_address(&self.topology.self_label, export, &address)?;
        if self.memoized_file_size(&path).is_none() {
            return Err(PeerError::new(
                "tape_unavailable",
                format!("tape `{tape_id}` is not present in this export"),
            ));
        }
        let mut file = crate::platform::open_read_nofollow(&path)
            .map_err(|error| PeerError::new("tape_unavailable", error.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|error| PeerError::new("tape_unavailable", error.to_string()))?;
        if !metadata.file_type().is_file() {
            return Err(PeerError::new(
                "invalid_file",
                "tape path is not a regular file",
            ));
        }
        let length = metadata.len();
        if length > max_bytes {
            return Err(PeerError::new(
                "over_limit",
                format!("tape is {length} bytes; limit is {max_bytes}"),
            ));
        }
        let mut sent = 0u64;
        let mut buffer = vec![0u8; READ_FILE_CHUNK_BYTES];
        while sent < length {
            let wanted =
                usize::try_from((length - sent).min(buffer.len() as u64)).unwrap_or(buffer.len());
            let mut read = 0usize;
            while read < wanted {
                let count = file
                    .read(&mut buffer[read..wanted])
                    .map_err(|error| PeerError::new("read_error", error.to_string()))?;
                if count == 0 {
                    return Err(PeerError::new("tape_changed", "tape shortened during read"));
                }
                read += count;
            }
            write_data(
                output,
                id,
                json!({"tape_id": tape_id, "offset": sent, "bytes_b64": base64(&buffer[..wanted])}),
            )?;
            sent += wanted as u64;
        }
        let mut extra = [0u8; 1];
        if file
            .read(&mut extra)
            .map_err(|error| PeerError::new("read_error", error.to_string()))?
            != 0
        {
            return Err(PeerError::new("tape_changed", "tape grew during read"));
        }
        Ok(json!({ "tape_id": tape_id, "bytes": sent, "complete": true }))
    }

    fn require_open(&self, name: &str) -> Result<&OpenExport, PeerError> {
        self.opened
            .get(name)
            .ok_or_else(|| PeerError::new("not_opened", format!("export `{name}` was not opened")))
    }

    fn tape_path(&self, export: &TopologyExport, tape_id: &str) -> Option<(PathBuf, u64)> {
        let filename = format!("{tape_id}.jsonl.zst");
        export.tape_dirs.iter().find_map(|dir| {
            let path = dir.join(&filename);
            self.memoized_file_size(&path).map(|size| (path, size))
        })
    }

    fn memoized_file_size(&self, path: &Path) -> Option<u64> {
        if let Some(size) = self.tape_file_sizes.borrow().get(path) {
            return *size;
        }
        let size = fs::symlink_metadata(path)
            .ok()
            .filter(|metadata| metadata.file_type().is_file())
            .map(|metadata| metadata.len());
        self.tape_file_sizes
            .borrow_mut()
            .insert(path.to_path_buf(), size);
        size
    }
}

fn configured_limit(limits: &BTreeMap<String, u64>, key: &str, default: u64) -> u64 {
    limits.get(key).copied().unwrap_or(default).min(default)
}

#[derive(Debug)]
struct GrepScanTask {
    tape_id: String,
    indexed: bool,
    path: Option<(PathBuf, u64)>,
    error: Option<(&'static str, String)>,
}

#[derive(Debug)]
struct GrepCandidate {
    record: Value,
    tape_id: String,
    provenance_match_count: usize,
    match_count: usize,
    provenance_event_count: usize,
    timestamp: String,
    frame_size: u64,
}

impl GrepCandidate {
    fn from_record(record: Value) -> Self {
        let tape_id = record
            .get("tape_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let number = |name: &str| {
            record
                .get(name)
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or_default()
        };
        Self {
            tape_id,
            provenance_match_count: number("provenance_match_count"),
            match_count: number("match_count"),
            provenance_event_count: number("provenance_event_count"),
            timestamp: record
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            record,
            frame_size: 0,
        }
    }
}

impl PartialEq for GrepCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for GrepCandidate {}

impl PartialOrd for GrepCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GrepCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Less means a better result. BinaryHeap therefore keeps its worst
        // retained result at the root, ready to be replaced by a better one.
        other
            .provenance_match_count
            .cmp(&self.provenance_match_count)
            .then_with(|| other.match_count.cmp(&self.match_count))
            .then_with(|| {
                other
                    .provenance_event_count
                    .cmp(&self.provenance_event_count)
            })
            .then_with(|| other.timestamp.cmp(&self.timestamp))
            .then_with(|| self.tape_id.cmp(&other.tape_id))
    }
}

fn scan_tape_for_grep(
    path: &Path,
    expected_size: u64,
    limits: &BTreeMap<String, u64>,
    pattern: &str,
) -> Result<GrepTapeSummary, PeerError> {
    let compressed_limit =
        configured_limit(limits, "read_file_compressed_bytes", MAX_READ_FILE_BYTES);
    if expected_size > compressed_limit {
        return Err(PeerError::new(
            "budget_exceeded",
            format!("compressed tape exceeds {compressed_limit} byte limit"),
        ));
    }
    let file = crate::platform::open_read_nofollow(path)
        .map_err(|error| PeerError::new("tape_unavailable", error.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|error| PeerError::new("tape_unavailable", error.to_string()))?;
    if !metadata.file_type().is_file() {
        return Err(PeerError::new(
            "invalid_file",
            "tape path is not a regular file",
        ));
    }
    if metadata.len() != expected_size {
        return Err(PeerError::new(
            "tape_changed",
            "tape size changed during the query",
        ));
    }

    let decompressed_limit =
        configured_limit(limits, "decompressed_bytes_per_tape", 512 * 1024 * 1024);
    scan_grep_reader(
        file.take(compressed_limit.saturating_add(1)),
        Some(decompressed_limit),
        pattern,
    )
    .map_err(|error| {
        let code = if error.code == "over_limit" {
            "over_limit"
        } else {
            "invalid_tape"
        };
        PeerError::new(code, error.message)
    })
}

fn dispatch_ref_counts(index: &SqliteIndex, tape_id: &str) -> Result<(usize, usize), PeerError> {
    let mut up = 0usize;
    let mut down = 0usize;
    let mut seen = HashSet::new();
    for link in index.dispatch_links_for_tape(tape_id)? {
        let received = matches!(link.direction, crate::index::DispatchDirection::Received);
        if !seen.insert((link.uuid, received)) {
            continue;
        }
        if received {
            up = up.saturating_add(1);
        } else {
            down = down.saturating_add(1);
        }
    }
    Ok((up, down))
}

fn optional_string(value: Option<&Value>, name: &str) -> Result<Option<String>, PeerError> {
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            let text = value.as_str().ok_or_else(|| {
                PeerError::new("invalid_request", format!("{name} must be a string"))
            })?;
            if text.contains('\0') {
                return Err(PeerError::new(
                    "invalid_request",
                    format!("{name} contains a NUL byte"),
                ));
            }
            Ok(text.to_string())
        })
        .transpose()
}

fn optional_bool(value: Option<&Value>, name: &str) -> Result<Option<bool>, PeerError> {
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                PeerError::new("invalid_request", format!("{name} must be a boolean"))
            })
        })
        .transpose()
}

fn grep_data_frame_size(id: &Value, record: &Value) -> Result<usize, PeerError> {
    serde_json::to_vec(&json!({"id": id, "data": record}))
        .map(|frame| frame.len().saturating_add(1))
        .map_err(|error| PeerError::new("json_error", error.to_string()))
}

fn append_grep_failure(
    failures: &mut Vec<Value>,
    failure: Value,
    id: &Value,
    failure_bytes: &mut u64,
    top_bytes: u64,
    response_limit: u64,
) -> Result<(), PeerError> {
    let size = grep_data_frame_size(id, &failure)?;
    if size > MAX_FRAME_BYTES {
        return Err(PeerError::new(
            "budget_exceeded",
            "a grep failure exceeds the maximum frame size",
        ));
    }
    let next_bytes = failure_bytes.saturating_add(size as u64);
    if top_bytes.saturating_add(next_bytes) > response_limit {
        return Err(PeerError::new(
            "budget_exceeded",
            format!("grep response exceeds {response_limit} byte limit"),
        ));
    }
    *failure_bytes = next_bytes;
    failures.push(failure);
    Ok(())
}

fn optional_usize(value: Option<&Value>, name: &str) -> Result<Option<usize>, PeerError> {
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| usize::try_from(number).ok())
                .ok_or_else(|| PeerError::new("invalid_request", format!("{name} is invalid")))
        })
        .transpose()
}

fn optional_i64(value: Option<&Value>, name: &str) -> Result<Option<i64>, PeerError> {
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            value.as_i64().ok_or_else(|| {
                PeerError::new(
                    "invalid_request",
                    format!("{name} must be a signed integer"),
                )
            })
        })
        .transpose()
}

fn read_tape_for_query(
    path: &Path,
    expected_size: u64,
    limits: &BTreeMap<String, u64>,
) -> Result<String, PeerError> {
    let compressed_limit =
        configured_limit(limits, "read_file_compressed_bytes", MAX_READ_FILE_BYTES);
    if expected_size > compressed_limit {
        return Err(PeerError::new(
            "budget_exceeded",
            format!("compressed tape exceeds {compressed_limit} byte limit"),
        ));
    }
    let file = crate::platform::open_read_nofollow(path)
        .map_err(|error| PeerError::new("tape_unavailable", error.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|error| PeerError::new("tape_unavailable", error.to_string()))?;
    if !metadata.file_type().is_file() {
        return Err(PeerError::new(
            "invalid_file",
            "tape path is not a regular file",
        ));
    }
    if metadata.len() != expected_size {
        return Err(PeerError::new(
            "tape_changed",
            "tape size changed during the query",
        ));
    }
    let capacity = usize::try_from(expected_size)
        .map_err(|_| PeerError::new("budget_exceeded", "tape size exceeds address space"))?;
    let mut compressed = Vec::with_capacity(capacity);
    file.take(compressed_limit.saturating_add(1))
        .read_to_end(&mut compressed)
        .map_err(|error| PeerError::new("read_error", error.to_string()))?;
    if compressed.len() as u64 > compressed_limit {
        return Err(PeerError::new(
            "budget_exceeded",
            format!("compressed tape exceeds {compressed_limit} byte limit"),
        ));
    }
    if compressed.len() as u64 != expected_size {
        return Err(PeerError::new(
            "tape_changed",
            "tape size changed while it was read",
        ));
    }
    let decompressed_limit =
        configured_limit(limits, "decompressed_bytes_per_tape", 512 * 1024 * 1024);
    decompress_jsonl_with_limit(&compressed, decompressed_limit).map_err(|error| {
        if error.to_string().starts_with("decompressed tape exceeds ") {
            PeerError::new("over_limit", error.to_string())
        } else {
            PeerError::new("invalid_tape", error.to_string())
        }
    })
}

fn write_open_failure<W: Write>(
    output: &mut W,
    id: &Value,
    machine: &str,
    export: &str,
    status: &str,
    code: &str,
    message: String,
) -> Result<(), PeerError> {
    write_data(
        output,
        id,
        json!({
            "store": format!("{machine}/{export}"),
            "status": status,
            "phase": "open",
            "error": {"code": code, "message": message},
        }),
    )
}

fn write_dispatch_row<W: Write>(
    output: &mut W,
    id: &Value,
    machine: &str,
    store: &str,
    tape_id: &str,
    uuid: &str,
    turn: i64,
    direction: crate::index::DispatchDirection,
) -> Result<(), PeerError> {
    let direction = match direction {
        crate::index::DispatchDirection::Received => "received",
        crate::index::DispatchDirection::Sent => "sent",
    };
    write_data(
        output,
        id,
        json!({
            "store": format!("{machine}/{store}"),
            "tape_id": tape_id,
            "uuid": uuid,
            "first_turn_index": turn,
            "direction": direction,
        }),
    )
}

fn resolve_file_address(
    machine: &str,
    export: &OpenExport,
    address: &FileAddress,
) -> Result<(PathBuf, String), PeerError> {
    if address.machine.0 != machine || address.kind != FileKind::Tape || !address.path.is_absolute()
    {
        return Err(PeerError::new(
            "invalid_file_address",
            "file address must name this owner, be absolute, and have kind tape",
        ));
    }
    let filename = address
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PeerError::new("invalid_file_address", "tape filename is invalid"))?;
    let tape_id = filename
        .strip_suffix(".jsonl.zst")
        .ok_or_else(|| PeerError::new("invalid_file_address", "path is not a tape file"))?;
    validate_tape_id(tape_id)?;
    let parent = address
        .path
        .parent()
        .ok_or_else(|| PeerError::new("invalid_file_address", "tape path has no parent"))?;
    if !export.config.tape_dirs.iter().any(|dir| dir == parent)
        || address.path != parent.join(filename)
    {
        return Err(PeerError::new(
            "invalid_file_address",
            "path must be a tape directly inside a declared tape directory",
        ));
    }
    Ok((address.path.clone(), tape_id.to_string()))
}

fn validate_tape_id(id: &str) -> Result<(), PeerError> {
    if id.is_empty()
        || id.len() > 255
        || id.starts_with('.')
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(PeerError::new(
            "invalid_tape_id",
            "tape ID must match [A-Za-z0-9._-]{1,255} and must not start with a dot",
        ));
    }
    Ok(())
}

fn parse_rank_span(value: &Value) -> Result<(String, u32, u32), PeerError> {
    let object = value
        .as_object()
        .ok_or_else(|| PeerError::new("invalid_request", "rank_span must be an object"))?;
    reject_unknown_keys(object, &["file", "start", "end"])?;
    let file = object
        .get("file")
        .and_then(Value::as_str)
        .filter(|file| !file.is_empty())
        .ok_or_else(|| PeerError::new("invalid_request", "rank_span.file must be a string"))?
        .to_string();
    let start = object
        .get("start")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| PeerError::new("invalid_request", "rank_span.start must be positive"))?;
    let end = object
        .get("end")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value >= start)
        .ok_or_else(|| PeerError::new("invalid_request", "rank_span.end must be >= start"))?;
    Ok((file, start, end))
}

fn optional_u64_array(
    value: Option<&Value>,
    name: &str,
    batch_cap: usize,
) -> Result<Vec<u64>, PeerError> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) if values.len() <= batch_cap => values
            .iter()
            .map(|value| {
                value.as_u64().ok_or_else(|| {
                    PeerError::new("invalid_request", format!("{name} items must be integers"))
                })
            })
            .collect(),
        Some(Value::Array(_)) => Err(PeerError::new(
            "invalid_request",
            format!("{name} exceeds {batch_cap} items"),
        )),
        Some(_) => Err(PeerError::new(
            "invalid_request",
            format!("{name} must be an array"),
        )),
    }
}

fn optional_nonnegative_i64_array(
    value: Option<&Value>,
    name: &str,
    batch_cap: usize,
) -> Result<Vec<i64>, PeerError> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) if values.len() <= batch_cap => values
            .iter()
            .map(|value| {
                value.as_i64().filter(|value| *value >= 0).ok_or_else(|| {
                    PeerError::new(
                        "invalid_request",
                        format!("{name} items must be non-negative integers"),
                    )
                })
            })
            .collect(),
        Some(Value::Array(_)) => Err(PeerError::new(
            "invalid_request",
            format!("{name} exceeds {batch_cap} items"),
        )),
        Some(_) => Err(PeerError::new(
            "invalid_request",
            format!("{name} must be an array"),
        )),
    }
}

fn tape_segment_metadata(
    tape_id: &str,
    rows: &[TapeRow],
) -> Result<(Value, Option<String>), PeerError> {
    let meta = rows
        .iter()
        .find(|row| row.value.get("k").and_then(Value::as_str) == Some("meta"))
        .ok_or_else(|| PeerError::new("invalid_tape", "tape is missing its meta row"))?;
    let continuation = meta.value.get("ingest_continuation");
    if continuation.is_some_and(|value| !value.is_null() && !value.is_object()) {
        return Err(PeerError::new(
            "invalid_tape",
            "ingest_continuation metadata must be an object",
        ));
    }
    let previous = continuation
        .and_then(|value| value.get("previous_tape_id"))
        .filter(|value| !value.is_null())
        .map(|value| {
            value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                PeerError::new("invalid_tape", "previous_tape_id metadata must be a string")
            })
        })
        .transpose()?;
    if let Some(previous) = previous.as_deref() {
        validate_tape_id(previous).map_err(|_| {
            PeerError::new(
                "invalid_tape",
                "previous_tape_id metadata contains an invalid tape ID",
            )
        })?;
    }
    let segment = json!({
        "tape_id": tape_id,
        "previous_tape_id": previous,
        "message_turn_start": continuation
            .and_then(|value| value.get("message_turn_start"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        "ingest_context_only": meta.value.get("ingest_context_only") == Some(&Value::Bool(true)),
    });
    Ok((segment, previous))
}

fn empty_tape_summary() -> Value {
    json!({
        "total_lines": 0,
        "anchor_line": 0,
        "window_start": 0,
        "window_end": 0,
        "grep_filter_hits_window": false,
        "latest_timestamp": "",
        "files_touched": [],
    })
}

fn tape_facts_failure(
    store: &str,
    tape_id: &str,
    indexed: bool,
    status: &str,
    code: &'static str,
    message: String,
    digest: Option<&str>,
) -> Value {
    json!({
        "type": "tape_facts",
        "store": store,
        "tape_id": tape_id,
        "status": status,
        "indexed": indexed,
        "digest": digest,
        "error": {"code": code, "message": message},
    })
}

fn string_batch(value: Option<&Value>, label: &str) -> Result<Vec<String>, PeerError> {
    string_batch_limited(value, label, 255)
}

fn string_batch_with_cap(
    value: Option<&Value>,
    label: &str,
    batch_cap: usize,
) -> Result<Vec<String>, PeerError> {
    string_batch_limited_count(value, label, 255, batch_cap)
}

fn anchor_batch_with_cap(
    value: Option<&Value>,
    label: &str,
    batch_cap: usize,
) -> Result<Vec<String>, PeerError> {
    // The incoming request has already passed the serialized 1 MiB frame
    // boundary. An additional per-anchor limit would reject valid exact
    // composite fingerprints before their frame budget is reached.
    string_batch_limited_count(value, label, usize::MAX, batch_cap)
}

fn string_batch_limited(
    value: Option<&Value>,
    label: &str,
    max_item_bytes: usize,
) -> Result<Vec<String>, PeerError> {
    string_batch_limited_count(value, label, max_item_bytes, MAX_BATCH_ITEMS)
}

fn string_batch_limited_count(
    value: Option<&Value>,
    label: &str,
    max_item_bytes: usize,
    batch_cap: usize,
) -> Result<Vec<String>, PeerError> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| PeerError::new("invalid_request", format!("{label} must be an array")))?;
    if values.is_empty() || values.len() > batch_cap {
        return Err(PeerError::new(
            "invalid_request",
            format!("{label} must contain 1..={batch_cap} items"),
        ));
    }
    let mut out = Vec::with_capacity(values.len());
    let mut seen = HashSet::new();
    for value in values {
        let item = value.as_str().ok_or_else(|| {
            PeerError::new("invalid_request", format!("{label} items must be strings"))
        })?;
        if item.is_empty() || item.len() > max_item_bytes || item.contains('\0') {
            return Err(PeerError::new(
                "invalid_request",
                format!("{label} contains an invalid string"),
            ));
        }
        if seen.insert(item.to_string()) {
            out.push(item.to_string());
        }
    }
    Ok(out)
}

fn optional_string_batch(
    value: Option<&Value>,
    label: &str,
    batch_cap: usize,
) -> Result<Vec<String>, PeerError> {
    match value {
        None => Ok(Vec::new()),
        Some(value) => string_batch_with_cap(Some(value), label, batch_cap),
    }
}

fn reject_unknown_keys(
    object: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), PeerError> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(PeerError::new(
            "invalid_request",
            format!("unknown field `{key}`"),
        ));
    }
    Ok(())
}

fn write_data<W: Write>(output: &mut W, id: &Value, data: Value) -> Result<(), PeerError> {
    write_frame(output, &json!({"id": id, "data": data}))
}

fn write_data_limited<W: Write>(
    output: &mut W,
    id: &Value,
    data: Value,
    used_bytes: &mut u64,
    response_limit: u64,
) -> Result<(), PeerError> {
    let mut frame = serde_json::to_vec(&json!({"id": id, "data": data}))
        .map_err(|error| PeerError::new("json_error", error.to_string()))?;
    let frame_bytes = frame.len().saturating_add(1) as u64;
    if frame_bytes as usize > MAX_FRAME_BYTES {
        return Err(PeerError::new(
            "budget_exceeded",
            "a lookup result exceeds the maximum response frame size",
        ));
    }
    if used_bytes.saturating_add(frame_bytes) > response_limit {
        return Err(PeerError::new(
            "budget_exceeded",
            format!("lookup response exceeds {response_limit} byte limit"),
        ));
    }
    frame.push(b'\n');
    output
        .write_all(&frame)
        .and_then(|_| output.flush())
        .map_err(|error| PeerError::new("write_error", error.to_string()))?;
    *used_bytes = used_bytes.saturating_add(frame_bytes);
    Ok(())
}

fn write_anchor_result_frames<W: Write>(
    output: &mut W,
    id: &Value,
    store: &str,
    query_anchor: &str,
    matching: &[String],
    used_bytes: &mut u64,
    response_limit: u64,
) -> Result<usize, PeerError> {
    let chunks = anchor_result_frame_ranges(id, store, query_anchor, matching, MAX_FRAME_BYTES)?;
    for range in &chunks {
        write_data_limited(
            output,
            id,
            json!({
                "type": "anchor_result",
                "store": store,
                "anchor": query_anchor,
                "matching_window_anchors": &matching[range.clone()],
            }),
            used_bytes,
            response_limit,
        )?;
    }
    Ok(chunks.len())
}

fn anchor_result_frame_ranges(
    id: &Value,
    store: &str,
    query_anchor: &str,
    matching: &[String],
    frame_limit: usize,
) -> Result<Vec<std::ops::Range<usize>>, PeerError> {
    let empty_frame_bytes = anchor_result_frame_bytes(id, store, query_anchor, &[])?;
    if empty_frame_bytes > frame_limit {
        return Err(anchor_result_over_limit(
            "query anchor",
            query_anchor,
            empty_frame_bytes,
            frame_limit,
        ));
    }
    if matching.is_empty() {
        return Ok(vec![0..0]);
    }

    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut frame_bytes = empty_frame_bytes;
    for (index, matched_anchor) in matching.iter().enumerate() {
        let encoded_anchor_bytes = serde_json::to_vec(matched_anchor)
            .map_err(|error| PeerError::new("json_error", error.to_string()))?
            .len();
        let separator_bytes = usize::from(index > start);
        let candidate_bytes = frame_bytes
            .saturating_add(encoded_anchor_bytes)
            .saturating_add(separator_bytes);
        if candidate_bytes <= frame_limit {
            frame_bytes = candidate_bytes;
            continue;
        }

        if index == start {
            return Err(anchor_result_over_limit(
                "matched anchor",
                matched_anchor,
                candidate_bytes,
                frame_limit,
            ));
        }
        ranges.push(start..index);
        start = index;
        frame_bytes = empty_frame_bytes.saturating_add(encoded_anchor_bytes);
        if frame_bytes > frame_limit {
            return Err(anchor_result_over_limit(
                "matched anchor",
                matched_anchor,
                frame_bytes,
                frame_limit,
            ));
        }
    }
    ranges.push(start..matching.len());
    Ok(ranges)
}

fn anchor_result_frame_bytes(
    id: &Value,
    store: &str,
    query_anchor: &str,
    matching: &[String],
) -> Result<usize, PeerError> {
    serde_json::to_vec(&json!({
        "id": id,
        "data": {
            "type": "anchor_result",
            "store": store,
            "anchor": query_anchor,
            "matching_window_anchors": matching,
        },
    }))
    .map(|frame| frame.len().saturating_add(1))
    .map_err(|error| PeerError::new("json_error", error.to_string()))
}

fn anchor_result_over_limit(
    label: &str,
    anchor: &str,
    frame_bytes: usize,
    frame_limit: usize,
) -> PeerError {
    let digest = format!("{:x}", Sha256::digest(anchor.as_bytes()));
    PeerError::new(
        "budget_exceeded",
        format!(
            "lookup_anchors anchor_result frame is {frame_bytes} bytes including envelope and newline (limit {frame_limit}); {label} is {} bytes (sha256 {})",
            anchor.len(),
            &digest[..12],
        ),
    )
}

fn write_terminal<W: Write>(
    output: &mut W,
    id: &Value,
    ok: bool,
    stats: Value,
    error: Option<Value>,
) -> io::Result<()> {
    let mut frame = json!({"id": id, "end": true, "ok": ok, "stats": stats});
    if let Some(error) = error {
        frame["error"] = error;
    }
    write_frame(output, &frame)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.message))
}

fn write_frame<W: Write>(output: &mut W, frame: &Value) -> Result<(), PeerError> {
    let mut bytes = serde_json::to_vec(frame)
        .map_err(|error| PeerError::new("json_error", error.to_string()))?;
    if bytes.len().saturating_add(1) > MAX_FRAME_BYTES {
        return Err(PeerError::new(
            "frame_over_limit",
            "response frame exceeds 1 MiB",
        ));
    }
    bytes.push(b'\n');
    output
        .write_all(&bytes)
        .and_then(|_| output.flush())
        .map_err(|error| PeerError::new("write_error", error.to_string()))
}

fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(BASE64[(a >> 2) as usize] as char);
        out.push(BASE64[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64[(c & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{DispatchDirection, DispatchLink};

    fn request(id: u64, op: &str, stores: &[&str], args: Value) -> String {
        json!({"v": PROTOCOL_VERSION, "id": id, "op": op, "stores": stores, "args": args})
            .to_string()
    }

    fn canonical_winnow_anchor(feature_count: usize) -> String {
        format!(
            "winnow:{}",
            (0..feature_count)
                .map(|feature| format!("{feature:016x}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    }

    struct DelayedFirstFlush {
        bytes: Vec<u8>,
        flush_completed: Option<mpsc::Sender<()>>,
    }

    impl Write for DelayedFirstFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if let Some(completed) = self.flush_completed.take() {
                thread::sleep(Duration::from_millis(1_200));
                completed.send(()).map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "flush waiter disappeared")
                })?;
            }
            Ok(())
        }
    }

    fn tape_address(machine: &str, path: &Path) -> Value {
        let parent = path
            .parent()
            .expect("tape address parent")
            .canonicalize()
            .expect("canonical tape address parent");
        let filename = path.file_name().expect("tape address filename");
        let address_path = parent.join(filename);
        json!({"machine": machine, "path": address_path, "kind": "tape"})
    }

    fn configured_home() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let db = home.join(".engram/index.sqlite");
        let tapes = home.join(".engram/tapes");
        fs::create_dir_all(&tapes).expect("tape dir");
        let index = SqliteIndex::open_writer(db.to_str().expect("utf8 path")).expect("index");
        index
            .insert_dispatch_link(
                "abc",
                &DispatchLink {
                    uuid: "u-1".into(),
                    first_turn_index: 2,
                    direction: DispatchDirection::Received,
                },
            )
            .expect("dispatch link");
        drop(index);
        fs::write(home.join(".engram/topology.yml"), format!(
            "version: 1\nself: test-owner\nexports:\n  default:\n    db: {}\n    tape_dirs:\n      - {}\n",
            db.display(), tapes.display()
        )).expect("topology");
        (temp, home, db, tapes)
    }

    #[test]
    fn anchor_result_frames_preserve_real_shaped_anchors_and_whole_match_order() {
        let observed_shape = canonical_winnow_anchor(1_103);
        assert_eq!(observed_shape.len(), 18_757);
        let larger_shape = canonical_winnow_anchor(2_100);
        assert!(larger_shape.len() > 32 * 1024);

        for anchor in [&observed_shape, &larger_shape] {
            let matching = vec![anchor.clone()];
            let ranges = anchor_result_frame_ranges(
                &json!(7),
                "eezo/default",
                "winnow:0123456789abcdef",
                &matching,
                MAX_FRAME_BYTES,
            )
            .expect("real-shaped exact anchor should fit one bounded frame");
            assert_eq!(ranges, vec![0..1]);
            assert_eq!(matching[ranges[0].clone()], [anchor.clone()]);
        }

        let matching = vec![
            "winnow:quote\"and\\slash".to_string(),
            "winnow:line\nbreak".to_string(),
            "winnow:multibyte-λ".to_string(),
            "winnow:tail".to_string(),
            "winnow:tail".to_string(),
        ];
        let ranges = anchor_result_frame_ranges(
            &json!(7),
            "eezo/default",
            "winnow:0123456789abcdef",
            &matching,
            180,
        )
        .expect("small frame bound should split between whole anchors");
        let reconstructed = ranges
            .iter()
            .flat_map(|range| matching[range.clone()].iter().cloned())
            .collect::<Vec<_>>();
        assert_eq!(reconstructed, matching);
        assert!(ranges.len() > 1);
        for range in ranges {
            assert!(
                anchor_result_frame_bytes(
                    &json!(7),
                    "eezo/default",
                    "winnow:0123456789abcdef",
                    &matching[range],
                )
                .expect("measure serialized frame")
                    <= 180
            );
        }
    }

    #[test]
    fn anchor_result_frames_keep_empty_results_and_reject_an_indivisible_over_limit_item() {
        let empty = anchor_result_frame_ranges(&json!(1), "eezo/default", "query-anchor", &[], 256)
            .expect("empty result fits");
        assert_eq!(empty, vec![0..0]);

        let query = "query-anchor";
        let matched = "winnow:matched-anchor".to_string();
        let base = anchor_result_frame_bytes(&json!(1), "eezo/default", query, &[])
            .expect("measure empty result");
        let one = base
            + serde_json::to_vec(&matched)
                .expect("encode matched value")
                .len();
        let failure = anchor_result_frame_ranges(
            &json!(1),
            "eezo/default",
            query,
            &[matched.clone()],
            one - 1,
        )
        .expect_err("indivisible matching anchor cannot be split");
        assert_eq!(failure.code, "budget_exceeded");
        assert!(failure.message.contains("lookup_anchors anchor_result"));
        assert!(failure.message.contains(&matched.len().to_string()));
        assert!(failure.message.contains("sha256"));
        assert!(!failure.message.contains(&matched));
    }

    #[test]
    fn anchor_result_frames_account_for_each_frame_in_the_cumulative_budget() {
        let first = format!("winnow:{}", "a".repeat(700_000));
        let second = format!("winnow:{}", "b".repeat(700_000));
        let matching = vec![first.clone(), second];
        let first_bytes =
            anchor_result_frame_bytes(&json!(1), "eezo/default", "query-anchor", &matching[..1])
                .expect("first frame size");
        let mut output = Vec::new();
        let mut used_bytes = 0;
        let failure = write_anchor_result_frames(
            &mut output,
            &json!(1),
            "eezo/default",
            "query-anchor",
            &matching,
            &mut used_bytes,
            first_bytes as u64,
        )
        .expect_err("second bounded frame exceeds the unchanged cumulative budget");
        assert_eq!(failure.code, "budget_exceeded");
        assert_eq!(used_bytes, first_bytes as u64);
        let frames = output
            .split(|byte| *byte == b'\n')
            .filter(|frame| !frame.is_empty())
            .map(|frame| serde_json::from_slice::<Value>(frame).expect("data frame JSON"))
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["data"]["type"], "anchor_result");
        assert_eq!(frames[0]["data"]["matching_window_anchors"], json!([first]));
    }

    fn run(home: &Path, input: &str) -> String {
        let topology = load_topology(home).expect("topology").expect("configured");
        let mut session = PeerSession {
            topology,
            opened: BTreeMap::new(),
            tape_file_sizes: RefCell::new(HashMap::new()),
        };
        let mut output = Vec::new();
        for line in input.lines() {
            let value: Value = serde_json::from_str(line).expect("request JSON");
            let id = value["id"].clone();
            let result = session.handle(&value, &id, &mut output);
            match result {
                Ok(stats) => write_terminal(&mut output, &id, true, stats, None).expect("terminal"),
                Err(error) => write_terminal(
                    &mut output,
                    &id,
                    false,
                    Value::Null,
                    Some(json!({"code":error.code,"message":error.message})),
                )
                .expect("terminal"),
            }
        }
        String::from_utf8(output).expect("utf8 output")
    }

    #[test]
    fn terminal_response_restarts_idle_timeout_after_a_long_owner_operation() {
        let (frames_tx, frames_rx) = mpsc::channel::<Result<Option<Vec<u8>>, String>>();
        let (flush_tx, flush_rx) = mpsc::channel();
        frames_tx
            .send(Ok(Some(request(1, "test", &[], json!({})).into_bytes())))
            .expect("first request");
        let producer = thread::spawn(move || {
            flush_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("first terminal response flush");
            thread::sleep(Duration::from_millis(200));
            frames_tx
                .send(Ok(Some(request(2, "test", &[], json!({})).into_bytes())))
                .expect("second request should still have a live owner");
        });

        let mut output = DelayedFirstFlush {
            bytes: Vec::new(),
            flush_completed: Some(flush_tx),
        };
        let mut handled = 0;
        serve_request_frames(
            frames_rx,
            &mut output,
            Instant::now(),
            Duration::from_secs(4),
            Duration::from_secs(1),
            |_, id, _| {
                handled += 1;
                Ok(json!({"handled": id}))
            },
        )
        .expect("serve requests");
        producer.join().expect("request producer");

        let frames = String::from_utf8(output.bytes)
            .expect("UTF-8 protocol output")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("terminal JSON"))
            .collect::<Vec<_>>();
        assert_eq!(handled, 2);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["id"], 1);
        assert_eq!(frames[1]["id"], 2);
    }

    #[test]
    fn opens_only_declared_exports_and_returns_snapshot_identity() {
        let (_temp, home, _db, _tapes) = configured_home();
        let output = run(
            &home,
            &format!(
                "{}\n",
                request(1, "open", &["default", "hidden"], json!({}))
            ),
        );
        let frames = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0]["data"]["status"], "ok");
        assert_eq!(frames[0]["data"]["store"], "test-owner/default");
        assert_eq!(frames[1]["data"]["status"], "not_exported");
        assert_eq!(frames[2]["end"], true);
    }

    #[test]
    fn open_excludes_schema_v3_per_export_without_aborting_compatible_exports() {
        let (_temp, home, _db, _tapes) = configured_home();
        let legacy_db = home.join(".engram/legacy.sqlite");
        let legacy_tapes = home.join(".engram/legacy-tapes");
        fs::create_dir_all(&legacy_tapes).expect("legacy tape directory");
        let legacy = rusqlite::Connection::open(&legacy_db).expect("legacy database");
        legacy
            .pragma_update(None, "user_version", 3)
            .expect("schema v3 version");
        drop(legacy);

        let topology_path = home.join(".engram/topology.yml");
        let mut topology = fs::read_to_string(&topology_path).expect("topology");
        topology.push_str(&format!(
            "  legacy:\n    db: {}\n    tape_dirs:\n      - {}\n",
            legacy_db.display(),
            legacy_tapes.display()
        ));
        fs::write(&topology_path, topology).expect("add legacy export");

        let output = run(
            &home,
            &format!(
                "{}\n",
                request(1, "open", &["legacy", "default"], json!({}))
            ),
        );
        let frames = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0]["data"]["status"], "incompatible");
        assert_eq!(frames[0]["data"]["phase"], "open");
        assert_eq!(frames[0]["data"]["error"]["code"], "schema_mismatch");
        assert_eq!(frames[1]["data"]["status"], "ok");
        assert_eq!(frames[2]["stats"]["opened"], 1);
        assert_eq!(frames[2]["stats"]["protocol"], PROTOCOL_VERSION);
    }

    #[test]
    fn dispatch_and_locate_are_owner_local_and_read_only() {
        let (_temp, home, _db, tapes) = configured_home();
        fs::write(tapes.join("abc.jsonl.zst"), b"not a real zstd tape").expect("tape fixture");
        let input = format!(
            "{}\n{}\n{}\n",
            request(1, "open", &["default"], json!({})),
            request(2, "dispatch_rows", &["default"], json!({"by_uuid":["u-1"]})),
            request(3, "locate_tapes", &["default"], json!({"tape_ids":["abc"]}))
        );
        let output = run(&home, &input);
        let frames = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames[2]["data"]["uuid"], "u-1");
        assert_eq!(frames[2]["data"]["direction"], "received");
        assert_eq!(frames[4]["data"]["indexed"], false);
        assert_eq!(frames[4]["data"]["file"]["kind"], "tape");
        assert_eq!(frames[4]["data"]["size_bytes"], 20);
        assert_eq!(frames[5]["ok"], true);
    }

    #[test]
    fn locate_tapes_returns_a_readable_owner_issued_file_address() {
        let (_temp, home, _db, tapes) = configured_home();
        let tape = tapes.join("roundtrip.jsonl.zst");
        let bytes = [0, 1, 2, 250, 255];
        fs::write(&tape, bytes).expect("tape fixture");

        let locate_output = run(
            &home,
            &format!(
                "{}\n{}\n",
                request(1, "open", &["default"], json!({})),
                request(
                    2,
                    "locate_tapes",
                    &["default"],
                    json!({"tape_ids":["roundtrip"]})
                )
            ),
        );
        let locate_frames = locate_output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let address = locate_frames[2]["data"]["file"].clone();
        let canonical_tapes = tapes.canonicalize().expect("canonical tape directory");
        let expected_path = canonical_tapes.join("roundtrip.jsonl.zst");
        assert_eq!(
            address["path"].as_str(),
            expected_path.to_str(),
            "owner-issued file address should use the canonical export path"
        );

        let read_output = run(
            &home,
            &format!(
                "{}\n{}\n",
                request(3, "open", &["default"], json!({})),
                request(
                    4,
                    "read_file",
                    &["default"],
                    json!({"address":address,"max_bytes":bytes.len()})
                )
            ),
        );
        let read_frames = read_output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(read_frames[2]["data"]["bytes_b64"], "AAEC+v8=");
        assert_eq!(read_frames[3]["ok"], true);
    }

    #[test]
    fn tape_existence_is_memoized_both_when_present_and_missing() {
        let (_temp, home, _db, tapes) = configured_home();
        let present = tapes.join("present.jsonl.zst");
        fs::write(&present, b"cached").expect("present tape");
        let topology = load_topology(&home).expect("topology").expect("configured");
        let mut session = PeerSession {
            topology,
            opened: BTreeMap::new(),
            tape_file_sizes: RefCell::new(HashMap::new()),
        };
        let mut output = Vec::new();
        let open: Value = serde_json::from_str(&request(1, "open", &["default"], json!({})))
            .expect("open request");
        session
            .handle(&open, &open["id"], &mut output)
            .expect("open owner");
        output.clear();

        let locate_missing: Value = serde_json::from_str(&request(
            2,
            "locate_tapes",
            &["default"],
            json!({"tape_ids":["appears-later"]}),
        ))
        .expect("locate missing request");
        session
            .handle(&locate_missing, &locate_missing["id"], &mut output)
            .expect("first missing lookup");
        let first_missing: Value = serde_json::from_slice(&output).expect("missing frame");
        assert_eq!(first_missing["data"]["file"], Value::Null);
        output.clear();

        let appeared = tapes.join("appears-later.jsonl.zst");
        fs::write(&appeared, b"new tape").expect("create tape mid-session");
        let locate_missing_again: Value = serde_json::from_str(&request(
            3,
            "locate_tapes",
            &["default"],
            json!({"tape_ids":["appears-later"]}),
        ))
        .expect("second locate missing request");
        session
            .handle(
                &locate_missing_again,
                &locate_missing_again["id"],
                &mut output,
            )
            .expect("memoized missing lookup");
        let still_missing: Value = serde_json::from_slice(&output).expect("missing frame");
        assert_eq!(still_missing["data"]["file"], Value::Null);
        output.clear();

        let read_new_file: Value = serde_json::from_str(&request(
            4,
            "read_file",
            &["default"],
            json!({"address":tape_address("test-owner", &appeared)}),
        ))
        .expect("read newly appeared tape request");
        let error = session
            .handle(&read_new_file, &read_new_file["id"], &mut output)
            .expect_err("negative existence result is retained");
        assert_eq!(error.code, "tape_unavailable");

        let locate_present: Value = serde_json::from_str(&request(
            5,
            "locate_tapes",
            &["default"],
            json!({"tape_ids":["present"]}),
        ))
        .expect("locate present request");
        session
            .handle(&locate_present, &locate_present["id"], &mut output)
            .expect("first present lookup");
        let first_present: Value = serde_json::from_slice(&output).expect("present frame");
        assert_eq!(first_present["data"]["size_bytes"], 6);
        output.clear();

        fs::remove_file(&present).expect("remove tape mid-session");
        let locate_present_again: Value = serde_json::from_str(&request(
            6,
            "locate_tapes",
            &["default"],
            json!({"tape_ids":["present"]}),
        ))
        .expect("second locate present request");
        session
            .handle(
                &locate_present_again,
                &locate_present_again["id"],
                &mut output,
            )
            .expect("memoized present lookup");
        let still_present: Value = serde_json::from_slice(&output).expect("present frame");
        assert_eq!(
            still_present["data"]["file"]["path"].as_str(),
            tapes
                .canonicalize()
                .expect("canonical tape directory")
                .join("present.jsonl.zst")
                .to_str()
        );
        assert_eq!(still_present["data"]["size_bytes"], 6);
    }

    #[test]
    fn read_file_confines_paths_and_frames_compressed_bytes_as_base64() {
        let (_temp, home, _db, tapes) = configured_home();
        let tape = tapes.join("abc.jsonl.zst");
        fs::write(&tape, [0, 1, 2, 250, 255]).expect("tape");
        let input = format!(
            "{}\n{}\n{}\n",
            request(1, "open", &["default"], json!({})),
            request(
                2,
                "read_file",
                &["default"],
                json!({"address":tape_address("test-owner", &tape),"max_bytes":5})
            ),
            request(
                3,
                "read_file",
                &["default"],
                json!({"address":tape_address("test-owner", &tapes.join("../secret.jsonl.zst"))})
            )
        );
        let output = run(&home, &input);
        let frames = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames[2]["data"]["bytes_b64"], "AAEC+v8=");
        assert_eq!(frames[3]["stats"]["bytes"], 5);
        assert_eq!(frames[4]["error"]["code"], "invalid_file_address");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn rejects_unknown_operations() {
        let (_temp, home, _db, _tapes) = configured_home();
        let output = run(
            &home,
            &format!(
                "{}\n{}\n",
                request(1, "open", &["default"], json!({})),
                request(2, "nope", &["default"], json!({}))
            ),
        );
        let frames = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames[2]["error"]["code"], "unknown_operation");
    }

    #[test]
    fn open_pins_a_read_only_snapshot_for_the_session() {
        let (_temp, home, db, _tapes) = configured_home();
        let topology = load_topology(&home).expect("topology").expect("configured");
        let mut session = PeerSession {
            topology,
            opened: BTreeMap::new(),
            tape_file_sizes: RefCell::new(HashMap::new()),
        };
        let mut output = Vec::new();
        let open: Value = serde_json::from_str(&request(1, "open", &["default"], json!({})))
            .expect("open request");
        session
            .handle(&open, &open["id"], &mut output)
            .expect("open owner");

        let writer = SqliteIndex::open_writer(db.to_str().expect("utf8 path")).expect("writer");
        writer
            .insert_dispatch_link(
                "abc",
                &DispatchLink {
                    uuid: "u-later".into(),
                    first_turn_index: 3,
                    direction: DispatchDirection::Sent,
                },
            )
            .expect("writer commit");
        assert!(
            session.opened["default"]
                .index
                .dispatch_links_for_uuid("u-later")
                .expect("pinned read")
                .is_empty()
        );

        assert!(
            session.opened["default"]
                .index
                .insert_dispatch_link(
                    "abc",
                    &DispatchLink {
                        uuid: "must-not-write".into(),
                        first_turn_index: 4,
                        direction: DispatchDirection::Sent,
                    },
                )
                .is_err()
        );
        session.opened["default"]
            .index
            .end_pinned_snapshot()
            .expect("end snapshot");
        assert_eq!(
            session.opened["default"]
                .index
                .dispatch_links_for_uuid("u-later")
                .expect("fresh read")[0]
                .uuid,
            "u-later"
        );
    }

    #[test]
    fn dropping_owner_session_releases_its_pinned_reader_transaction() {
        let (_temp, home, db, _tapes) = configured_home();
        let topology = load_topology(&home).expect("topology").expect("configured");
        let mut session = PeerSession {
            topology,
            opened: BTreeMap::new(),
            tape_file_sizes: RefCell::new(HashMap::new()),
        };
        let mut output = Vec::new();
        let open: Value = serde_json::from_str(&request(1, "open", &["default"], json!({})))
            .expect("open request");
        session
            .handle(&open, &open["id"], &mut output)
            .expect("open owner");

        {
            let writer = SqliteIndex::open_writer(db.to_str().expect("utf8 path"))
                .expect("writer while owner is open");
            writer
                .insert_dispatch_link(
                    "abc",
                    &DispatchLink {
                        uuid: "visible-after-owner-exit".into(),
                        first_turn_index: 9,
                        direction: DispatchDirection::Sent,
                    },
                )
                .expect("writer commit while pinned reader is open");
        }

        assert!(
            session.opened["default"]
                .index
                .dispatch_links_for_uuid("visible-after-owner-exit")
                .expect("pinned read")
                .is_empty(),
            "owner should retain its original snapshot until the session ends"
        );

        // The real peer exits by dropping PeerSession when stdin closes or its
        // watchdog expires. Dropping the owner must release the WAL snapshot,
        // so a checkpoint can pass it instead of waiting for the session cap.
        drop(session);
        let checkpoint = rusqlite::Connection::open(&db)
            .expect("open checkpoint connection")
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .expect("checkpoint after owner session exit");
        assert_eq!(
            checkpoint.0, 0,
            "checkpoint must not be blocked by the owner"
        );

        let fresh = SqliteIndex::open_reader(db.to_str().expect("utf8 path"))
            .expect("fresh reader after owner exit");
        assert_eq!(
            fresh
                .dispatch_links_for_uuid("visible-after-owner-exit")
                .expect("fresh snapshot")[0]
                .uuid,
            "visible-after-owner-exit"
        );
    }

    #[test]
    fn topology_parser_rejects_unknown_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path();
        fs::create_dir_all(home.join(".engram")).expect("config dir");
        fs::write(
            home.join(".engram/topology.yml"),
            "version: 1\nself: owner\nunknown: true\n",
        )
        .expect("topology");
        let error = load_topology(home).expect_err("unknown field must fail");
        assert!(error.to_string().contains("unknown field"));
    }
}
