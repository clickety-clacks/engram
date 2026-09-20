//! Freeze the existing P1 fixtures and two already-copied historical sources twice.
//! This is normalization custody, not the query expectation oracle.
use engram::tape::adapters::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<_> = std::env::args().collect();
    if a.len() != 4 {
        return Err("usage: SOURCE_ROOT HISTORICAL_COPIES NEW_OUTPUT".into());
    }
    let source = Path::new(&a[1]);
    let historical = Path::new(&a[2]);
    let output = Path::new(&a[3]);
    fs::create_dir(output)?;
    let manifest: Value = serde_json::from_slice(&fs::read(
        source.join("tests/fixtures/t1772/sha256-manifest.json"),
    )?)?;
    let mut receipt = Vec::new();
    for fixture in manifest["files"].as_array().unwrap() {
        let path = source.join(fixture["path"].as_str().unwrap());
        let bytes = fs::read(&path)?;
        assert_eq!(hash(&bytes), fixture["sha256"].as_str().unwrap());
        let name = if path.ends_with("logs.json") {
            "gemini-logs".to_owned()
        } else {
            path.file_stem().unwrap().to_str().unwrap().to_owned()
        };
        let converter: fn(&str) -> Result<String, serde_json::Error> = match name.as_str() {
            "claude" => claude_jsonl_to_tape_jsonl,
            "codex" => codex_jsonl_to_tape_jsonl,
            "cursor" => cursor_jsonl_to_tape_jsonl,
            "gemini" | "gemini-logs" => gemini_json_to_tape_jsonl,
            "openclaw" => openclaw_jsonl_to_tape_jsonl,
            "opencode" => opencode_json_to_tape_jsonl,
            _ => unreachable!(),
        };
        receipt.push(freeze(output, &name, &path, &bytes, converter)?);
    }
    for (name, wanted) in [
        (
            "root-session",
            "387c10e20fb5928e7313c697a256051c5e5076a29d32912785dd82202240ccff",
        ),
        (
            "implementer-session",
            "cd159e1bf1e8d213eb27bd5f405561afc46bb80f7f028677a02720845aae5c53",
        ),
    ] {
        let path = historical.join(format!("{name}.jsonl"));
        let raw = fs::read(&path)?;
        assert_eq!(hash(&raw), wanted);
        receipt.push(freeze(
            output,
            name,
            &path,
            &raw,
            codex_jsonl_to_tape_jsonl,
        )?);
    }
    fs::write(
        output.join("normalization.json"),
        serde_json::to_string(&receipt)? + "\n",
    )?;
    Ok(())
}
fn freeze(
    out: &Path,
    name: &str,
    path: &Path,
    bytes: &[u8],
    convert: fn(&str) -> Result<String, serde_json::Error>,
) -> Result<Value, Box<dyn std::error::Error>> {
    let raw = std::str::from_utf8(bytes)?;
    let first = convert(raw)?;
    let second = convert(raw)?;
    assert_eq!(first, second);
    let id = hash(first.as_bytes());
    let dir = out.join(name);
    fs::create_dir(&dir)?;
    fs::create_dir(dir.join("corpus"))?;
    fs::write(dir.join("normalized.jsonl"), &first)?;
    let compressed = zstd::stream::encode_all(first.as_bytes(), 0)?;
    fs::write(
        dir.join("corpus").join(format!("{id}.jsonl.zst")),
        &compressed,
    )?;
    assert_eq!(hash(&fs::read(path)?), hash(bytes));
    Ok(
        json!({"name":name,"raw_source":path,"raw_sha256":hash(bytes),"raw_bytes":bytes.len(),"normalized_sha256":id,"normalized_bytes":first.len(),"compressed_sha256":hash(&compressed),"repeated_conversion_equal":true,"source_pre_post_equal":true,"normalization_is_candidate_input_not_query_expectation":true}),
    )
}
