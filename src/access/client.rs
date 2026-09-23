//! One-query peer process with pipelined request rounds and framed responses.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::TopologyPeer;

use super::peer::{MAX_FRAME_BYTES, PROTOCOL_VERSION};
use super::transport;

pub const MAX_NON_FILE_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;

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
        if requests.is_empty() {
            return Vec::new();
        }

        if self.input.is_none() {
            return requests
                .iter()
                .map(|_| Err(PeerFailure::new("unavailable", "peer stdin is closed")))
                .collect();
        }
        let mut ids = Vec::with_capacity(requests.len());
        let mut frames = Vec::with_capacity(requests.len());
        let mut file_requests = HashSet::new();
        for request in requests {
            let id = self.next_id;
            self.next_id = self.next_id.saturating_add(1);
            if request.op == "read_file" {
                file_requests.insert(id);
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

        let started = Instant::now();
        let mut pending = ids.iter().copied().collect::<HashSet<_>>();
        let mut data = HashMap::<u64, Vec<Value>>::new();
        let mut outcomes = HashMap::<u64, Result<PeerResponse, PeerFailure>>::new();
        while !pending.is_empty() {
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
            match self.responses.recv_timeout(remaining) {
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
                    if !file_requests.contains(&id) {
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
                    fail_pending(
                        &mut pending,
                        &mut outcomes,
                        "timeout",
                        "peer request timed out",
                    );
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
                }) if matches!(code.as_str(), "timeout" | "protocol_error" | "budget_exceeded")
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
}
