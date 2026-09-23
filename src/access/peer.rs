//! Read-only stdio owner endpoint used by a selected peer transport.
//!
//! This module deliberately has no command execution, SQL transport, config
//! creation, or write operation. A process opens only the named exports in its
//! home-only topology and keeps their read snapshots until stdin closes.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Value, json};

use super::{FileAddress, FileKind, MachineRef};
use crate::config::{Topology, TopologyExport, load_topology};
use crate::index::{QUERY_SEMANTICS_VERSION, ReaderMode, SCHEMA_VERSION, SqliteIndex};

pub const PROTOCOL_VERSION: u64 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_BATCH_ITEMS: usize = 128;
pub const MAX_READ_FILE_BYTES: u64 = 256 * 1024 * 1024;
const READ_FILE_CHUNK_BYTES: usize = 720 * 1024;

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
    let topology = load_topology(home)
        .map_err(|error| format!("topology_error: {error}"))?
        .ok_or_else(|| "topology_missing: expected ~/.engram/topology.yml".to_string())?;
    let mut session = PeerSession {
        topology,
        opened: BTreeMap::new(),
        tape_file_sizes: RefCell::new(HashMap::new()),
    };
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = BufReader::new(stdin.lock());
    let mut output = io::BufWriter::new(stdout.lock());

    loop {
        let request = match read_frame(&mut input) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(()),
            Err(error) => return Err(format!("frame_error: {error}")),
        };
        let parsed = serde_json::from_slice::<Value>(&request);
        let (id, result) = match parsed {
            Ok(value) => {
                let id = value.get("id").cloned().unwrap_or(Value::Null);
                let result = session.handle(&value, &id, &mut output);
                (id, result)
            }
            Err(error) => (
                Value::Null,
                Err(PeerError::new("invalid_json", error.to_string())),
            ),
        };
        match result {
            Ok(stats) => write_terminal(&mut output, &id, true, stats, None)
                .map_err(|error| format!("write_error: {error}"))?,
            Err(error) => write_terminal(
                &mut output,
                &id,
                false,
                Value::Null,
                Some(json!({"code": error.code, "message": error.message})),
            )
            .map_err(|write_error| format!("write_error: {write_error}"))?,
        }
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
        let tape_ids = string_batch(args.get("tape_ids"), "args.tape_ids")?;
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
        let by_tape = optional_string_batch(args.get("by_tape"), "args.by_tape")?;
        let by_uuid = optional_string_batch(args.get("by_uuid"), "args.by_uuid")?;
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
        let export = self.require_open(&stores[0])?;
        let (path, tape_id) = resolve_file_address(&self.topology.self_label, export, &address)?;
        if self.memoized_file_size(&path).is_none() {
            return Err(PeerError::new(
                "tape_unavailable",
                format!("tape `{tape_id}` is not present in this export"),
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&path)
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

fn string_batch(value: Option<&Value>, label: &str) -> Result<Vec<String>, PeerError> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| PeerError::new("invalid_request", format!("{label} must be an array")))?;
    if values.is_empty() || values.len() > MAX_BATCH_ITEMS {
        return Err(PeerError::new(
            "invalid_request",
            format!("{label} must contain 1..={MAX_BATCH_ITEMS} items"),
        ));
    }
    let mut out = Vec::with_capacity(values.len());
    let mut seen = HashSet::new();
    for value in values {
        let item = value.as_str().ok_or_else(|| {
            PeerError::new("invalid_request", format!("{label} items must be strings"))
        })?;
        if item.is_empty() || item.len() > 255 || item.contains('\0') {
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

fn optional_string_batch(value: Option<&Value>, label: &str) -> Result<Vec<String>, PeerError> {
    match value {
        None => Ok(Vec::new()),
        Some(value) => string_batch(Some(value), label),
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

    fn tape_address(machine: &str, path: &Path) -> Value {
        json!({"machine": machine, "path": path, "kind": "tape"})
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
            present.to_str()
        );
        assert_eq!(still_present["data"]["size_bytes"], 6);
    }

    #[test]
    fn read_file_confines_paths_and_frames_compressed_bytes_as_base64() {
        let (_temp, home, _db, tapes) = configured_home();
        let tape = tapes.join("abc.jsonl.zst");
        fs::write(&tape, [0, 1, 2, 250, 255]).expect("tape");
        #[cfg(unix)]
        std::os::unix::fs::symlink("abc.jsonl.zst", tapes.join("link.jsonl.zst")).expect("symlink");
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
    fn rejects_symlinked_tape_files_and_unknown_operations() {
        let (_temp, home, _db, tapes) = configured_home();
        fs::write(tapes.join("real.jsonl.zst"), b"data").expect("tape");
        #[cfg(unix)]
        std::os::unix::fs::symlink("real.jsonl.zst", tapes.join("link.jsonl.zst"))
            .expect("symlink");
        let input = format!(
            "{}\n{}\n",
            request(1, "open", &["default"], json!({})),
            request(
                2,
                "read_file",
                &["default"],
                json!({"address":tape_address("test-owner", &tapes.join("link.jsonl.zst"))})
            )
        );
        let output = run(&home, &input);
        let frames = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames[2]["error"]["code"], "tape_unavailable");
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
