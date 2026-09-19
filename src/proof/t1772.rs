use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub const CANDIDATE_BASE: &str = "9f8afc65d0b365444446a473c04389adf80bd4b3";
pub const INPUT_ROOT: &str = "/Users/mike/shared-workspace/engram/proofs/t1772/inputs";
pub const PROOF_ROOT: &str =
    "/Users/mike/shared-workspace/engram/proofs/t1772/final-staging-asg-b260c05f-r27";
pub const MANIFEST_NAME: &str = "t1772-inputs-r27-9f8afc65d0b365444446a473c04389adf80bd4b3.sha256";
pub const MANIFEST_SHA256: &str =
    "0d279da30da8b118ae1b4433c86c78736ddda8d2ae6f2d8c00ecfcb1b47fd89a";
pub const P0_TAPE_MANIFEST_SHA256: &str =
    "29800d70e6812b446581c1f505ddd1b78c25effcd76a8c467c892553913f5757";
pub const DISPATCH_ROWS: u64 = 14_369;
pub const DISPATCH_JSONL_SHA256: &str =
    "de556156054c6299407b0e61efaff8e59a0c2bc67c1b5d172d22f85817eac661";
pub const GLOBAL_ORACLE_SHA256: &str =
    "eaaf582c89dd8437825ff2fae8829a93c98e3fa42e0eec367d80ac0f956e48ca";
pub const PER_TAPE_ORACLE_SHA256: &str =
    "216389baba02cae5e722118b8cdee9cc5dc128cd222a8a44d34f70610374c2a7";
pub const BASELINE_BYTES: u64 = 120_001_798_144;
pub const REBUILDS: u64 = 2;
pub const RETAINED_READER_SECONDS: u64 = 60;
pub const WATCHER_TRANSACTIONS: u64 = 100;

pub const EXPECTED_INPUTS: u64 = 45_758;
pub const EXPECTED_EVENTS: u64 = 3_250_068;
pub const EXPECTED_READ_WINDOWS: u64 = 305_699;
pub const EXPECTED_EDIT_WINDOWS: u64 = 223_463;
pub const EXPECTED_EVIDENCE_WINDOWS: u64 = 529_162;
pub const EXPECTED_EVIDENCE_FEATURES: u64 = 43_856_633;
pub const EXPECTED_TOMBSTONE_WINDOWS: u64 = 1_114;
pub const EXPECTED_TOMBSTONE_FEATURES: u64 = 41_964;
pub const EXPECTED_EDIT_EDGES: u64 = 150_357;
pub const EXPECTED_SEMANTIC_EDGES: u64 = 109_761;
pub const EXPECTED_LEGACY_TOMBSTONE_KEYS: u64 = 25_305;

pub const EDIT_EDGE_LOGICAL_SHA256: &str =
    "0d76b715afb6651b68ccc3f2b153c2737cdf09b389ed50b6456ffb58164eeb7d";
pub const TOMBSTONE_MAPPING_SHA256: &str =
    "a5ed642d4b9b1c05dd01ad3f021d2b5ce83ceb18b865a58a216cfb78a536b5f2";

pub const CANONICAL_BYTES_CONTRACT: &str = "t1772-canonical-json-lf-v1: recursively sort object keys by UTF-8 bytes; preserve array order and JSON scalar types; serde_json compact UTF-8 encoding; append exactly one LF to every JSON document or JSONL record; hash the exact emitted bytes";

pub type ProofResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestEntry {
    pub sha256: String,
    pub relative_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustodyEntry {
    pub path: String,
    pub file_type: String,
    pub bytes: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub device: u64,
    pub inode: u64,
    pub mode: u32,
    pub sha256: Option<String>,
    pub symlink_target: Option<String>,
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(hasher)
}

pub fn sha256_file(path: &Path) -> ProofResult<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex_digest(hasher))
}

pub fn hex_digest(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonical_value(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_value).collect()),
        scalar => scalar.clone(),
    }
}

pub fn canonical_json_lf(value: &Value) -> ProofResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec(&canonical_value(value))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn write_canonical_json(path: &Path, value: &Value) -> ProofResult<String> {
    let bytes = canonical_json_lf(value)?;
    write_new_file(path, &bytes)?;
    Ok(sha256_bytes(&bytes))
}

pub fn write_canonical_jsonl<I>(path: &Path, values: I) -> ProofResult<String>
where
    I: IntoIterator<Item = Value>,
{
    let parent = path.parent().ok_or("JSONL output has no parent")?;
    fs::create_dir_all(parent)?;
    let file = File::options().write(true).create_new(true).open(path)?;
    let mut writer = BufWriter::new(file);
    let mut hasher = Sha256::new();
    for value in values {
        let bytes = canonical_json_lf(&value)?;
        writer.write_all(&bytes)?;
        hasher.update(&bytes);
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(hex_digest(hasher))
}

pub fn write_new_file(path: &Path, bytes: &[u8]) -> ProofResult<()> {
    let parent = path.parent().ok_or("output has no parent")?;
    fs::create_dir_all(parent)?;
    let mut file = File::options().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn require_exact_path(actual: &Path, expected: &str, name: &str) -> ProofResult<()> {
    if actual.as_os_str() != expected {
        return Err(format!(
            "{name} must be exactly {expected}, got {}",
            actual.display()
        )
        .into());
    }
    Ok(())
}

pub fn safe_relative_path(raw: &str) -> ProofResult<PathBuf> {
    let path = Path::new(raw);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("manifest path is not a simple relative path: {raw}").into());
    }
    Ok(path.to_path_buf())
}

pub fn read_sha256_manifest(path: &Path) -> ProofResult<Vec<ManifestEntry>> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let (sha256, relative_path) = line
            .split_once("  ")
            .ok_or_else(|| format!("invalid SHA-256 manifest line: {line:?}"))?;
        if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("invalid SHA-256 field: {sha256}").into());
        }
        safe_relative_path(relative_path)?;
        if !seen.insert(relative_path.to_string()) {
            return Err(format!("duplicate manifest path: {relative_path}").into());
        }
        entries.push(ManifestEntry {
            sha256: sha256.to_ascii_lowercase(),
            relative_path: relative_path.to_string(),
        });
    }
    Ok(entries)
}

pub fn verify_input_manifest(
    input_root: &Path,
    manifest: &Path,
) -> ProofResult<Vec<ManifestEntry>> {
    require_exact_path(input_root, INPUT_ROOT, "INPUT_ROOT")?;
    if manifest != input_root.join(MANIFEST_NAME) {
        return Err(format!("manifest path must be {INPUT_ROOT}/{MANIFEST_NAME}").into());
    }
    let manifest_hash = sha256_file(manifest)?;
    if manifest_hash != MANIFEST_SHA256 {
        return Err(format!("manifest SHA-256 mismatch: {manifest_hash}").into());
    }
    let entries = read_sha256_manifest(manifest)?;
    if entries.len() != 11 {
        return Err(format!("manifest must contain 11 entries, got {}", entries.len()).into());
    }
    for entry in &entries {
        let path = input_root.join(safe_relative_path(&entry.relative_path)?);
        let observed = sha256_file(&path)?;
        if observed != entry.sha256 {
            return Err(format!("input hash mismatch for {}: {observed}", path.display()).into());
        }
    }
    Ok(entries)
}

pub fn manifest_result(entries: &[ManifestEntry]) -> Vec<Value> {
    entries
        .iter()
        .map(|entry| {
            json!({
                "path": entry.relative_path,
                "sha256": entry.sha256,
                "status": "ok"
            })
        })
        .collect()
}

pub fn collect_custody(roots: &[PathBuf]) -> ProofResult<Vec<CustodyEntry>> {
    let mut paths = BTreeSet::new();
    for root in roots {
        collect_paths(root, &mut paths)?;
    }
    paths.into_iter().map(|path| custody_entry(&path)).collect()
}

fn collect_paths(path: &Path, out: &mut BTreeSet<PathBuf>) -> ProofResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    out.insert(path.to_path_buf());
    if metadata.is_dir() {
        let mut children = fs::read_dir(path)?
            .map(|entry| entry.map(|item| item.path()))
            .collect::<Result<Vec<_>, _>>()?;
        children.sort();
        for child in children {
            collect_paths(&child, out)?;
        }
    }
    Ok(())
}

fn custody_entry(path: &Path) -> ProofResult<CustodyEntry> {
    let metadata = fs::symlink_metadata(path)?;
    let (file_type, sha256, symlink_target) = if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?.to_string_lossy().into_owned();
        (
            "symlink".to_string(),
            Some(sha256_bytes(target.as_bytes())),
            Some(target),
        )
    } else if metadata.is_file() {
        ("file".to_string(), Some(sha256_file(path)?), None)
    } else if metadata.is_dir() {
        ("directory".to_string(), None, None)
    } else {
        ("other".to_string(), None, None)
    };
    Ok(CustodyEntry {
        path: path.to_string_lossy().into_owned(),
        file_type,
        bytes: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.permissions().mode(),
        sha256,
        symlink_target,
    })
}

pub fn compare_immutable_custody(
    before: &[CustodyEntry],
    after: &[CustodyEntry],
) -> ProofResult<Vec<Value>> {
    let before = before
        .iter()
        .map(|entry| (&entry.path, entry))
        .collect::<BTreeMap<_, _>>();
    let after = after
        .iter()
        .map(|entry| (&entry.path, entry))
        .collect::<BTreeMap<_, _>>();
    let mut differences = Vec::new();
    for path in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        match (before.get(path), after.get(path)) {
            (Some(left), Some(right)) if left == right => {}
            (Some(left), Some(right)) => {
                differences.push(json!({"path": path, "before": left, "after": right}))
            }
            (Some(left), None) => {
                differences.push(json!({"path": path, "before": left, "after": null}))
            }
            (None, Some(right)) => {
                differences.push(json!({"path": path, "before": null, "after": right}))
            }
            (None, None) => unreachable!(),
        }
    }
    if !differences.is_empty() {
        return Err(format!("immutable custody changed at {} paths", differences.len()).into());
    }
    Ok(differences)
}

pub fn canonical_preimage_hash(value: &Value) -> ProofResult<String> {
    Ok(sha256_bytes(&canonical_json_lf(value)?))
}

#[cfg(target_os = "macos")]
pub const SIGCONT_NUMBER: i32 = 19;
#[cfg(target_os = "linux")]
pub const SIGCONT_NUMBER: i32 = 18;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub const SIGCONT_NUMBER: i32 = libc::SIGCONT;

#[cfg(target_os = "macos")]
pub const SIGSTOP_NUMBER: i32 = 17;
#[cfg(target_os = "linux")]
pub const SIGSTOP_NUMBER: i32 = 19;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub const SIGSTOP_NUMBER: i32 = libc::SIGSTOP;

pub fn suspend_for_controller() -> ProofResult<()> {
    if std::env::var_os("T1772_START_SUSPENDED").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Err("runner requires T1772_START_SUSPENDED=1 from the reviewed controller".into());
    }
    let rc = unsafe { libc::raise(SIGSTOP_NUMBER) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
