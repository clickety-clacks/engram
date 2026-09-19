//! Measurement helpers. Observations are never substitutes for unobserved maxima.
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::t1772::{ProofResult, canonical_json_lf, write_canonical_json};

/// Nearest-rank percentiles; retained raw samples are the authoritative evidence.
pub fn percentiles(samples: &[u64]) -> ProofResult<Value> {
    if samples.is_empty() {
        return Err("cannot compute percentiles of an empty sample".into());
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = |p: usize| sorted[(sorted.len() * p).div_ceil(100) - 1];
    Ok(json!({"method":"nearest-rank", "samples":sorted.len(),
        "p50":rank(50), "p95":rank(95), "p99":rank(99)}))
}

pub fn file_bytes_or_absent(path: &Path) -> std::io::Result<u64> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

pub struct DiskSampler {
    stop: Sender<()>,
    join: Option<JoinHandle<ProofResult<Value>>>,
}

impl DiskSampler {
    /// Starts synchronously: a first successful sample precedes return.
    /// The trace and summary remain available on early error via Drop.
    pub fn start(root: &Path, trace: &Path, summary: &Path) -> ProofResult<Self> {
        let mut file = File::options().write(true).create_new(true).open(trace)?;
        let root = fs::canonicalize(root)?;
        let summary = summary.to_path_buf();
        let started = Instant::now();
        let initial = disk_sample(&root)?;
        file.write_all(&canonical_json_lf(&json!({"elapsed_microseconds":0,
            "logical_bytes":initial.0,"allocated_bytes":initial.1,"vanished_paths":initial.2}))?)?;
        file.flush()?;
        let (stop, receive) = mpsc::channel();
        let join = thread::spawn(move || {
            let mut peak_logical = initial.0;
            let mut peak_allocated = initial.1;
            let mut samples = 1u64;
            let mut vanished = initial.2;
            let mut max_gap = 0u128;
            let mut previous = started;
            loop {
                let stopping = !matches!(
                    receive.recv_timeout(Duration::from_millis(100)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                );
                let now = Instant::now();
                max_gap = max_gap.max(now.duration_since(previous).as_micros());
                previous = now;
                let sample = match disk_sample(&root) {
                    Ok(value) => value,
                    Err(error) => {
                        write_canonical_json(
                            &summary,
                            &json!({"status":"failed",
                            "error":error.to_string(),"samples":samples}),
                        )?;
                        return Err(error);
                    }
                };
                peak_logical = peak_logical.max(sample.0);
                peak_allocated = peak_allocated.max(sample.1);
                vanished += sample.2;
                samples += 1;
                file.write_all(&canonical_json_lf(&json!({
                    "elapsed_microseconds":started.elapsed().as_micros(),
                    "logical_bytes":sample.0,"allocated_bytes":sample.1,
                    "vanished_paths":sample.2}))?)?;
                file.flush()?;
                if stopping {
                    break;
                }
            }
            file.sync_all()?;
            let result = json!({"status":"observed", "root":root,
                "method":"recursive lstat; unique device/inode; non-directory st_size and st_blocks*512; directory metadata excluded; no symlink traversal",
                "requested_interval_milliseconds":100,"maximum_sample_gap_microseconds":max_gap,
                "peak_observed_logical_bytes":peak_logical,
                "peak_observed_allocated_bytes":peak_allocated,
                "samples":samples,"vanished_paths":vanished,
                "limitation":"sampled named-file high-water mark; unlinked files and allocations created and removed between samples may be missed; not total allocation evidence"});
            write_canonical_json(&summary, &result)?;
            Ok(result)
        });
        Ok(Self {
            stop,
            join: Some(join),
        })
    }

    pub fn finish(mut self) -> ProofResult<Value> {
        self.stop.send(()).ok();
        self.join
            .take()
            .ok_or("sampler already joined")?
            .join()
            .map_err(|_| "disk sampler panicked")?
    }
}

impl Drop for DiskSampler {
    fn drop(&mut self) {
        self.stop.send(()).ok();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn disk_sample(root: &Path) -> ProofResult<(u64, u64, u64)> {
    let mut pending = vec![PathBuf::from(root)];
    let mut seen = BTreeSet::new();
    let (mut logical, mut allocated, mut vanished) = (0, 0, 0);
    while let Some(path) = pending.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                vanished += 1;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if !seen.insert((metadata.dev(), metadata.ino())) {
            continue;
        }
        if !metadata.is_dir() {
            logical += metadata.len();
            allocated += metadata.blocks() * 512;
        }
        if metadata.is_dir() {
            match fs::read_dir(&path) {
                Ok(entries) => {
                    for entry in entries {
                        pending.push(entry?.path());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => vanished += 1,
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok((logical, allocated, vanished))
}
