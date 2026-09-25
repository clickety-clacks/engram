//! One-query peer process with pipelined request rounds and framed responses.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::TopologyPeer;

pub use super::MAX_NON_FILE_RESPONSE_BYTES;
use super::peer::{MAX_FRAME_BYTES, PROTOCOL_VERSION};
use super::transport;

const MAX_STDERR_BYTES: usize = 16 * 1024;
pub const DEFAULT_READ_FILE_COMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
pub const DEFAULT_DECOMPRESSED_BYTES_PER_TAPE: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PeerRequest {
    pub op: String,
    pub stores: Vec<String>,
    pub args: Value,
}

impl PeerRequest {
    pub fn new(op: impl Into<String>, stores: Vec<String>, args: Value) -> Self {
        Self {
            op: op.into(),
            stores,
            args,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PeerResponse {
    pub data: Vec<Value>,
    pub stats: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerFailure {
    pub code: String,
    pub message: String,
}

impl PeerFailure {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

enum ReaderMessage {
    Frame(Value, usize),
    Eof,
    Error(String),
}

pub struct PeerClient {
    child: Child,
    input: Option<ChildStdin>,
    responses: Receiver<ReaderMessage>,
    next_id: u64,
    response_bytes: usize,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerExport {
    pub name: String,
    pub db: String,
    pub tape_dirs: Vec<String>,
    pub reader_mode: String,
    pub snapshot_at: String,
}

/// One selected peer process and the negotiated exports it opened for this
/// invocation. Failed exports remain represented individually.
pub struct RemoteOwner {
    pub machine: String,
    pub build: String,
    pub protocol: u64,
    pub schema: u64,
    pub query_semantics: u64,
    pub limits: HashMap<String, u64>,
    pub exports: BTreeMap<String, Result<PeerExport, PeerFailure>>,
    client: PeerClient,
}

impl RemoteOwner {
    pub fn connect(
        machine: &str,
        caller: &str,
        peer: &TopologyPeer,
        timeout: Duration,
    ) -> Result<Self, PeerFailure> {
        Self::connect_inner(machine, caller, peer, timeout, None)
    }

    pub fn connect_cancellable(
        machine: &str,
        caller: &str,
        peer: &TopologyPeer,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<Self, PeerFailure> {
        Self::connect_inner(machine, caller, peer, timeout, Some(cancelled))
    }

    fn connect_inner(
        machine: &str,
        caller: &str,
        peer: &TopologyPeer,
        timeout: Duration,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self, PeerFailure> {
        if let Some(cancelled) = cancelled
            && cancelled.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(PeerFailure::new(
                "cancelled",
                "peer query was cancelled before opening the owner",
            ));
        }
        if timeout.is_zero() {
            return Err(PeerFailure::new(
                "timeout",
                "peer query deadline expired before opening the owner",
            ));
        }
        let mut client = PeerClient::spawn(peer)
            .map_err(|error| PeerFailure::new("unavailable", error.to_string()))?;
        let request = PeerRequest::new(
            "open",
            peer.exports.clone(),
            Value::Object(Default::default()),
        );
        let mut outcomes = match cancelled {
            Some(cancelled) => client.round_cancellable(&[request], timeout, cancelled),
            None => client.round(&[request], timeout),
        };
        let response = outcomes
            .pop()
            .ok_or_else(|| PeerFailure::new("protocol_error", "peer open returned no outcome"))?
            .map_err(|failure| match failure.code.as_str() {
                "unknown_operation" | "protocol_mismatch" => {
                    PeerFailure::new("incompatible", failure.message)
                }
                _ => failure,
            })?;
        let reported_machine = response
            .stats
            .get("self")
            .and_then(Value::as_str)
            .ok_or_else(|| PeerFailure::new("protocol_error", "open response has no self label"))?;
        if reported_machine != machine || reported_machine == caller {
            return Err(PeerFailure::new(
                "label_mismatch",
                format!("configured peer `{machine}` reported self `{reported_machine}`"),
            ));
        }
        let protocol = required_u64(&response.stats, "protocol")?;
        if protocol != PROTOCOL_VERSION {
            return Err(PeerFailure::new(
                "incompatible",
                format!("peer protocol {protocol} does not match {PROTOCOL_VERSION}"),
            ));
        }
        let schema = required_u64(&response.stats, "schema")?;
        if schema != crate::index::SCHEMA_VERSION as u64 {
            return Err(PeerFailure::new(
                "incompatible",
                format!(
                    "peer schema {schema} does not match {}",
                    crate::index::SCHEMA_VERSION
                ),
            ));
        }
        let query_semantics = required_u64(&response.stats, "query_semantics")?;
        if query_semantics != crate::index::QUERY_SEMANTICS_VERSION as u64 {
            return Err(PeerFailure::new(
                "incompatible_semantics",
                format!(
                    "peer query semantics {query_semantics} does not match {}",
                    crate::index::QUERY_SEMANTICS_VERSION
                ),
            ));
        }
        let build = response
            .stats
            .get("build")
            .and_then(Value::as_str)
            .ok_or_else(|| PeerFailure::new("protocol_error", "open response has no build id"))?
            .to_string();
        let limits = response
            .stats
            .get("limits")
            .and_then(Value::as_object)
            .ok_or_else(|| PeerFailure::new("protocol_error", "open response has no limits"))?
            .iter()
            .map(|(name, value)| {
                value
                    .as_u64()
                    .map(|value| (name.clone(), value))
                    .ok_or_else(|| {
                        PeerFailure::new(
                            "protocol_error",
                            format!("peer limit `{name}` is not numeric"),
                        )
                    })
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        let mut exports = BTreeMap::new();
        for row in response.data {
            let Some(store) = row.get("store").and_then(Value::as_str) else {
                return Err(PeerFailure::new(
                    "protocol_error",
                    "open export result has no store name",
                ));
            };
            let Some(name) = store.strip_prefix(&format!("{machine}/")) else {
                return Err(PeerFailure::new(
                    "label_mismatch",
                    format!("peer returned store `{store}` outside `{machine}`"),
                ));
            };
            if !peer.exports.iter().any(|expected| expected == name) || exports.contains_key(name) {
                return Err(PeerFailure::new(
                    "protocol_error",
                    format!("peer returned an unexpected or duplicate export `{store}`"),
                ));
            }
            let status = row
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("invalid");
            let result = match status {
                "ok" => {
                    let db = row.get("db").and_then(Value::as_str).ok_or_else(|| {
                        PeerFailure::new("protocol_error", "open export has no db path")
                    })?;
                    let tape_dirs = row
                        .get("tape_dirs")
                        .and_then(Value::as_array)
                        .ok_or_else(|| {
                            PeerFailure::new("protocol_error", "open export has no tape_dirs")
                        })?
                        .iter()
                        .map(|value| {
                            value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                                PeerFailure::new(
                                    "protocol_error",
                                    "open tape directory is not a string",
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let reader_mode =
                        row.get("reader_mode")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                PeerFailure::new("protocol_error", "open export has no reader mode")
                            })?;
                    let snapshot_at =
                        row.get("snapshot_at")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                PeerFailure::new(
                                    "protocol_error",
                                    "open export has no snapshot time",
                                )
                            })?;
                    Ok(PeerExport {
                        name: name.to_string(),
                        db: db.to_string(),
                        tape_dirs,
                        reader_mode: reader_mode.to_string(),
                        snapshot_at: snapshot_at.to_string(),
                    })
                }
                "incompatible" => Err(PeerFailure::new(
                    "incompatible",
                    row.get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("peer export schema is incompatible"),
                )),
                "unavailable" => Err(PeerFailure::new(
                    row.get("error")
                        .and_then(|error| error.get("code"))
                        .and_then(Value::as_str)
                        .unwrap_or("reader_unavailable"),
                    row.get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("peer export could not be opened"),
                )),
                "not_exported" => Err(PeerFailure::new(
                    "incompatible",
                    format!("configured export `{name}` was not declared by the peer"),
                )),
                _ => {
                    return Err(PeerFailure::new(
                        "protocol_error",
                        format!("peer returned unknown export status `{status}`"),
                    ));
                }
            };
            exports.insert(name.to_string(), result);
        }
        for name in &peer.exports {
            if !exports.contains_key(name) {
                return Err(PeerFailure::new(
                    "protocol_error",
                    format!("open response omitted configured export `{machine}/{name}`"),
                ));
            }
        }

        Ok(Self {
            machine: machine.to_string(),
            build,
            protocol,
            schema,
            query_semantics,
            limits,
            exports,
            client,
        })
    }

    pub fn round(
        &mut self,
        requests: &[PeerRequest],
        timeout: Duration,
    ) -> Vec<Result<PeerResponse, PeerFailure>> {
        self.client.round(requests, timeout)
    }

    pub fn round_cancellable(
        &mut self,
        requests: &[PeerRequest],
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Vec<Result<PeerResponse, PeerFailure>> {
        self.client.round_cancellable(requests, timeout, cancelled)
    }

    pub fn stderr_text(&self) -> String {
        self.client.stderr_text()
    }
}

/// Decode one strict RFC 4648 base64 payload produced by `read_file`.
pub fn decode_base64_chunk(encoded: &str) -> Result<Vec<u8>, String> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes = encoded.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err("base64 payload length is not a multiple of four".into());
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let a = value(chunk[0]).ok_or_else(|| "invalid base64 alphabet".to_string())?;
        let b = value(chunk[1]).ok_or_else(|| "invalid base64 alphabet".to_string())?;
        decoded.push((a << 2) | (b >> 4));
        match (chunk[2], chunk[3]) {
            (b'=', b'=') if last && b & 0x0f == 0 => {}
            (b'=', _) => return Err("invalid base64 padding".into()),
            (c, b'=') if last => {
                let c = value(c).ok_or_else(|| "invalid base64 alphabet".to_string())?;
                if c & 0x03 != 0 {
                    return Err("non-canonical base64 padding".into());
                }
                decoded.push((b << 4) | (c >> 2));
            }
            (c, d) => {
                let c = value(c).ok_or_else(|| "invalid base64 alphabet".to_string())?;
                let d = value(d).ok_or_else(|| "invalid base64 alphabet".to_string())?;
                decoded.push((b << 4) | (c >> 2));
                decoded.push((c << 6) | d);
            }
        }
    }
    Ok(decoded)
}

fn required_u64(value: &Value, key: &str) -> Result<u64, PeerFailure> {
    value.get(key).and_then(Value::as_u64).ok_or_else(|| {
        PeerFailure::new(
            "protocol_error",
            format!("open response has no numeric {key}"),
        )
    })
}

#[cfg(test)]
mod base64_tests {
    use super::decode_base64_chunk;

    #[test]
    fn decodes_empty_and_padded_chunks() {
        assert_eq!(decode_base64_chunk("").unwrap(), b"");
        assert_eq!(decode_base64_chunk("Zg==").unwrap(), b"f");
        assert_eq!(decode_base64_chunk("Zm8=").unwrap(), b"fo");
        assert_eq!(decode_base64_chunk("Zm9v").unwrap(), b"foo");
    }

    #[test]
    fn rejects_malformed_and_noncanonical_chunks() {
        for value in ["Zg=", "=m9v", "Zg==AAAA", "Zh==", "Zm9=", "Z m8="] {
            assert!(decode_base64_chunk(value).is_err(), "accepted {value:?}");
        }
    }
}

impl PeerClient {
    pub fn spawn(peer: &TopologyPeer) -> io::Result<Self> {
        let mut child = transport::spawn(peer)?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "peer stdin was not piped"))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "peer stdout was not piped")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "peer stderr was not piped")
        })?;
        let (sender, responses) = mpsc::channel();
        let stdout_thread = Some(read_responses(stdout, sender));
        let stderr_bytes = Arc::new(Mutex::new(Vec::new()));
        let stderr_thread = Some(drain_stderr(stderr, Arc::clone(&stderr_bytes)));

        Ok(Self {
            child,
            input: Some(input),
            responses,
            next_id: 1,
            response_bytes: 0,
            stdout_thread,
            stderr_thread,
            stderr: stderr_bytes,
        })
    }

    /// Send every request before draining the response stream, avoiding a full
    /// stdout pipe blocking the peer while the caller is still writing.
    pub fn round(
        &mut self,
        requests: &[PeerRequest],
        timeout: Duration,
    ) -> Vec<Result<PeerResponse, PeerFailure>> {
        self.round_inner(requests, timeout, None)
    }

    pub fn round_cancellable(
        &mut self,
        requests: &[PeerRequest],
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Vec<Result<PeerResponse, PeerFailure>> {
        self.round_inner(requests, timeout, Some(cancelled))
    }

    fn round_inner(
        &mut self,
        requests: &[PeerRequest],
        timeout: Duration,
        cancelled: Option<&AtomicBool>,
    ) -> Vec<Result<PeerResponse, PeerFailure>> {
        if requests.is_empty() {
            return Vec::new();
        }

        if let Some(cancelled) = cancelled
            && cancelled.load(std::sync::atomic::Ordering::SeqCst)
        {
            self.abort();
            let error = PeerFailure::new("cancelled", "peer request cancelled by caller");
            return requests.iter().map(|_| Err(error.clone())).collect();
        }
        if timeout.is_zero() {
            self.abort();
            let error = PeerFailure::new("timeout", "peer request deadline expired");
            return requests.iter().map(|_| Err(error.clone())).collect();
        }
        let started = Instant::now();

        if self.input.is_none() {
            return requests
                .iter()
                .map(|_| Err(PeerFailure::new("unavailable", "peer stdin is closed")))
                .collect();
        }
        let mut ids = Vec::with_capacity(requests.len());
        let mut frames = Vec::with_capacity(requests.len());
        let mut file_requests = HashMap::new();
        for request in requests {
            let id = self.next_id;
            self.next_id = self.next_id.saturating_add(1);
            if request.op == "read_file" {
                file_requests.insert(id, read_file_response_byte_limit(request));
            }
            let frame = json!({
                "v": PROTOCOL_VERSION,
                "id": id,
                "op": request.op,
                "stores": request.stores,
                "args": request.args,
            });
            let encoded = match serde_json::to_vec(&frame) {
                Ok(encoded) if encoded.len() < MAX_FRAME_BYTES => encoded,
                Ok(_) => {
                    let error = PeerFailure::new("invalid_request", "request frame exceeds 1 MiB");
                    return requests.iter().map(|_| Err(error.clone())).collect();
                }
                Err(error) => {
                    let error = PeerFailure::new("invalid_request", error.to_string());
                    return requests.iter().map(|_| Err(error.clone())).collect();
                }
            };
            ids.push(id);
            frames.push(encoded);
        }

        let write_result = (|| -> io::Result<()> {
            let input = self.input.as_mut().expect("checked open peer stdin");
            for frame in &frames {
                input.write_all(frame)?;
                input.write_all(b"\n")?;
            }
            input.flush()
        })();
        if let Err(error) = write_result {
            let error = PeerFailure::new("unavailable", error.to_string());
            self.abort();
            return requests.iter().map(|_| Err(error.clone())).collect();
        }

        let mut pending = ids.iter().copied().collect::<HashSet<_>>();
        let mut data = HashMap::<u64, Vec<Value>>::new();
        let mut outcomes = HashMap::<u64, Result<PeerResponse, PeerFailure>>::new();
        let mut file_response_bytes = HashMap::<u64, usize>::new();
        while !pending.is_empty() {
            if let Some(cancelled) = cancelled
                && cancelled.load(std::sync::atomic::Ordering::SeqCst)
            {
                fail_pending(
                    &mut pending,
                    &mut outcomes,
                    "cancelled",
                    "peer request cancelled by caller",
                );
                break;
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                fail_pending(
                    &mut pending,
                    &mut outcomes,
                    "timeout",
                    "peer request timed out",
                );
                break;
            }
            let wait = if cancelled.is_some() {
                remaining.min(Duration::from_millis(100))
            } else {
                remaining
            };
            match self.responses.recv_timeout(wait) {
                Ok(ReaderMessage::Frame(frame, frame_bytes)) => {
                    let Some(id) = frame.get("id").and_then(Value::as_u64) else {
                        fail_pending(
                            &mut pending,
                            &mut outcomes,
                            "protocol_error",
                            "response frame has no numeric id",
                        );
                        break;
                    };
                    if !pending.contains(&id) {
                        fail_pending(
                            &mut pending,
                            &mut outcomes,
                            "protocol_error",
                            "response frame id was not pending",
                        );
                        break;
                    }
                    if let Some(limit) = file_requests.get(&id) {
                        let used = file_response_bytes.entry(id).or_default();
                        *used = used.saturating_add(frame_bytes);
                        if *used > *limit {
                            fail_pending(
                                &mut pending,
                                &mut outcomes,
                                "budget_exceeded",
                                "peer read_file response exceeds the requested compressed-byte budget",
                            );
                            break;
                        }
                    } else {
                        self.response_bytes = self.response_bytes.saturating_add(frame_bytes);
                        if self.response_bytes > MAX_NON_FILE_RESPONSE_BYTES {
                            fail_pending(
                                &mut pending,
                                &mut outcomes,
                                "budget_exceeded",
                                "peer response exceeds the 32 MiB non-file budget",
                            );
                            break;
                        }
                    }
                    if frame.get("end").and_then(Value::as_bool) == Some(true) {
                        pending.remove(&id);
                        if frame.get("ok").and_then(Value::as_bool) == Some(true) {
                            outcomes.insert(
                                id,
                                Ok(PeerResponse {
                                    data: data.remove(&id).unwrap_or_default(),
                                    stats: frame.get("stats").cloned().unwrap_or(Value::Null),
                                }),
                            );
                        } else {
                            let error = frame.get("error").cloned().unwrap_or(Value::Null);
                            outcomes.insert(
                                id,
                                Err(PeerFailure::new(
                                    error
                                        .get("code")
                                        .and_then(Value::as_str)
                                        .unwrap_or("peer_error"),
                                    error
                                        .get("message")
                                        .and_then(Value::as_str)
                                        .unwrap_or("peer operation failed"),
                                )),
                            );
                        }
                    } else if let Some(value) = frame.get("data") {
                        data.entry(id).or_default().push(value.clone());
                    } else {
                        pending.remove(&id);
                        outcomes.insert(
                            id,
                            Err(PeerFailure::new(
                                "protocol_error",
                                "response is neither a data nor terminal frame",
                            )),
                        );
                    }
                }
                Ok(ReaderMessage::Eof) => {
                    let detail = self.stderr_text();
                    let message = if detail.is_empty() {
                        "peer closed stdout before completing the round".to_string()
                    } else {
                        format!("peer closed stdout: {detail}")
                    };
                    fail_pending(&mut pending, &mut outcomes, "unavailable", &message);
                }
                Ok(ReaderMessage::Error(message)) => {
                    fail_pending(&mut pending, &mut outcomes, "protocol_error", &message);
                }
                Err(RecvTimeoutError::Timeout) => {
                    if started.elapsed() >= timeout {
                        fail_pending(
                            &mut pending,
                            &mut outcomes,
                            "timeout",
                            "peer request timed out",
                        );
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    fail_pending(
                        &mut pending,
                        &mut outcomes,
                        "unavailable",
                        "peer response reader stopped",
                    );
                }
            }
        }

        let abort = outcomes.values().any(|outcome| {
            matches!(
                outcome,
                Err(PeerFailure {
                    code,
                    ..
                }) if matches!(
                    code.as_str(),
                    "timeout" | "cancelled" | "protocol_error" | "budget_exceeded"
                )
            )
        });
        if abort {
            self.abort();
        }

        ids.into_iter()
            .map(|id| {
                outcomes.remove(&id).unwrap_or_else(|| {
                    Err(PeerFailure::new(
                        "unavailable",
                        "peer did not complete the operation",
                    ))
                })
            })
            .collect()
    }

    pub fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
            .unwrap_or_else(|_| "peer stderr buffer is unavailable".into())
    }

    fn abort(&mut self) {
        self.input.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
    }
}

impl Drop for PeerClient {
    fn drop(&mut self) {
        self.input.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

fn fail_pending(
    pending: &mut HashSet<u64>,
    outcomes: &mut HashMap<u64, Result<PeerResponse, PeerFailure>>,
    code: &str,
    message: &str,
) {
    for id in pending.drain() {
        outcomes.insert(id, Err(PeerFailure::new(code, message)));
    }
}

fn read_file_response_byte_limit(request: &PeerRequest) -> usize {
    let compressed_limit = request
        .args
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_READ_FILE_COMPRESSED_BYTES)
        .min(DEFAULT_READ_FILE_COMPRESSED_BYTES);
    let compressed_limit = usize::try_from(compressed_limit).unwrap_or(usize::MAX);
    // Base64 expands compressed bytes by at most 4/3. One maximum frame
    // allows for JSON framing and per-chunk overhead in the owner's stream.
    compressed_limit
        .div_ceil(3)
        .saturating_mul(4)
        .saturating_add(MAX_FRAME_BYTES)
}

fn read_responses(stdout: ChildStdout, sender: Sender<ReaderMessage>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut input = BufReader::new(stdout);
        loop {
            match read_frame(&mut input) {
                Ok(Some(frame)) => match serde_json::from_slice::<Value>(&frame) {
                    Ok(value) => {
                        if sender
                            .send(ReaderMessage::Frame(value, frame.len()))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(ReaderMessage::Error(format!(
                            "invalid JSON response: {error}"
                        )));
                        return;
                    }
                },
                Ok(None) => {
                    let _ = sender.send(ReaderMessage::Eof);
                    return;
                }
                Err(error) => {
                    let _ = sender.send(ReaderMessage::Error(error.to_string()));
                    return;
                }
            }
        }
    })
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
                "unterminated response frame",
            ));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if frame.len().saturating_add(take) > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response frame exceeds 1 MiB",
            ));
        }
        let ended = available.get(take.saturating_sub(1)) == Some(&b'\n');
        frame.extend_from_slice(&available[..take]);
        input.consume(take);
        if ended {
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(Some(frame));
        }
    }
}

fn drain_stderr(stderr: impl Read + Send + 'static, saved: Arc<Mutex<Vec<u8>>>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut stderr = stderr;
        let mut buffer = [0u8; 4096];
        loop {
            let count = match stderr.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(count) => count,
            };
            if let Ok(mut saved) = saved.lock()
                && saved.len() < MAX_STDERR_BYTES
            {
                let remaining = MAX_STDERR_BYTES - saved.len();
                saved.extend_from_slice(&buffer[..count.min(remaining)]);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn command_peer_round_pipelines_requests_and_collects_data_until_terminal_frames() {
        let peer = TopologyPeer {
            ssh: None,
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "while IFS= read -r request; do id=$(printf '%s\\n' \"$request\" | sed -n 's/.*\\\"id\\\":\\([0-9][0-9]*\\).*/\\1/p'); printf '{\"id\":%s,\"data\":{\"seen\":%s}}\\n{\"id\":%s,\"end\":true,\"ok\":true,\"stats\":{\"done\":true}}\\n' \"$id\" \"$id\" \"$id\"; done".into(),
            ]),
            engram: "/unused".into(),
            exports: vec![],
        };
        let mut client = PeerClient::spawn(&peer).expect("spawn command peer");
        let results = client.round(
            &[
                PeerRequest::new("open", vec!["default".into()], json!({})),
                PeerRequest::new(
                    "locate_tapes",
                    vec!["default".into()],
                    json!({"tape_ids":[]}),
                ),
            ],
            Duration::from_secs(5),
        );
        assert_eq!(results.len(), 2);
        for (index, result) in results.into_iter().enumerate() {
            let response = result.expect("terminal success");
            assert_eq!(response.data[0]["seen"], index as u64 + 1);
            assert_eq!(response.stats["done"], true);
        }
    }

    #[cfg(unix)]
    #[test]
    fn peer_round_stops_buffering_over_budget_read_file_responses() {
        let peer = TopologyPeer {
            ssh: None,
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"while IFS= read -r request; do id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'); payload=$(printf '%0750000d' 0); printf '{"id":%s,"data":{"padding":"%s"}}\n' "$id" "$payload"; printf '{"id":%s,"data":{"padding":"%s"}}\n' "$id" "$payload"; done"#.into(),
            ]),
            engram: "/unused".into(),
            exports: vec![],
        };
        let mut client = PeerClient::spawn(&peer).expect("spawn oversized file peer");
        let outcomes = client.round(
            &[PeerRequest::new(
                "read_file",
                vec!["default".into()],
                json!({"max_bytes": 1}),
            )],
            Duration::from_secs(5),
        );

        assert_eq!(outcomes.len(), 1);
        let failure = outcomes[0]
            .as_ref()
            .expect_err("oversized response should be refused before terminal frame");
        assert_eq!(failure.code, "budget_exceeded");
    }

    #[cfg(unix)]
    #[test]
    fn peer_round_cancellation_aborts_owner_process() {
        let peer = TopologyPeer {
            ssh: None,
            command: Some(vec!["/bin/sh".into(), "-c".into(), "cat >/dev/null".into()]),
            engram: "/unused".into(),
            exports: vec![],
        };
        let mut client = PeerClient::spawn(&peer).expect("spawn waiting command peer");
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        let signal_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            signal.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let outcomes = client.round_cancellable(
            &[PeerRequest::new(
                "grep_scan",
                vec!["default".into()],
                json!({}),
            )],
            Duration::from_secs(5),
            &cancelled,
        );
        signal_thread.join().expect("signal cancellation");
        assert_eq!(outcomes[0].as_ref().unwrap_err().code, "cancelled");
        let _ = client.child.wait().expect("wait for aborted owner");
    }

    #[cfg(unix)]
    #[test]
    fn expired_peer_round_aborts_without_writing_a_request() {
        let peer = TopologyPeer {
            ssh: None,
            command: Some(vec!["/bin/sh".into(), "-c".into(), "cat >/dev/null".into()]),
            engram: "/unused".into(),
            exports: vec![],
        };
        let mut client = PeerClient::spawn(&peer).expect("spawn waiting command peer");
        let outcomes = client.round(
            &[PeerRequest::new(
                "grep_scan",
                vec!["default".into()],
                json!({}),
            )],
            Duration::ZERO,
        );
        assert_eq!(outcomes[0].as_ref().unwrap_err().code, "timeout");
        let _ = client.child.wait().expect("wait for expired owner");
    }

    #[cfg(unix)]
    #[test]
    fn remote_owner_validates_identity_and_records_open_export_metadata() {
        let peer = TopologyPeer {
            ssh: None,
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "while IFS= read -r request; do id=$(printf '%s\\n' \"$request\" | sed -n 's/.*\\\"id\\\":\\([0-9][0-9]*\\).*/\\1/p'); printf '{\"id\":%s,\"data\":{\"store\":\"eezo/default\",\"status\":\"ok\",\"db\":\"/owner/index.sqlite\",\"tape_dirs\":[\"/owner/tapes\"],\"reader_mode\":\"live\",\"snapshot_at\":\"2026-09-23T00:00:00Z\"}}\\n{\"id\":%s,\"end\":true,\"ok\":true,\"stats\":{\"self\":\"eezo\",\"build\":\"0.2.1\",\"protocol\":1,\"schema\":4,\"query_semantics\":1,\"limits\":{\"read_file_compressed_bytes\":268435456,\"decompressed_bytes_per_tape\":536870912}}}\\n' \"$id\" \"$id\"; done".into(),
            ]),
            engram: "/unused".into(),
            exports: vec!["default".into()],
        };
        let owner = RemoteOwner::connect("eezo", "gibson", &peer, Duration::from_secs(5))
            .expect("compatible peer");
        assert_eq!(owner.machine, "eezo");
        assert_eq!(owner.build, "0.2.1");
        assert_eq!(owner.protocol, PROTOCOL_VERSION);
        assert_eq!(owner.schema, crate::index::SCHEMA_VERSION as u64);
        assert_eq!(
            owner.query_semantics,
            crate::index::QUERY_SEMANTICS_VERSION as u64
        );
        let export = owner.exports["default"]
            .as_ref()
            .expect("open export metadata");
        assert_eq!(export.db, "/owner/index.sqlite");
        assert_eq!(export.tape_dirs, ["/owner/tapes"]);
        assert_eq!(export.reader_mode, "live");
    }
}
