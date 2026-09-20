//! Read-only §9.3 reference material from canonical tapes, never a query/database.
//! Shares only the hash-bound canonical fingerprint primitive; no index/query API.
use engram::anchor::{fingerprint_similarity, fingerprint_text, fingerprint_windows};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn windows(v: &Value) -> Vec<engram::anchor::FingerprintedWindow> {
    v.as_str().map(fingerprint_windows).unwrap_or_default()
}
fn emit(w: &mut BufWriter<File>, v: Value) {
    serde_json::to_writer(&mut *w, &v).unwrap();
    writeln!(w).unwrap();
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        hash(include_bytes!("../src/anchor/mod.rs")),
        "7564df814a3d5d4bfad5aabd84736448050acb703cf9fb34f8dd5d11929d7ae3"
    );
    assert_eq!(
        hash(include_bytes!("../src/anchor/winnow.rs")),
        "c02408f5ba2a02a62e0b007deefb0041f2494f1f083281c980c54a1a8e9a23e2"
    );
    let a: Vec<_> = std::env::args().collect();
    if a.len() != 5 {
        return Err("usage: reference CORPUS IDS QUERY_MANIFEST NEW_OUTPUT".into());
    }
    let ids = fs::read_to_string(&a[2])?;
    let queries: Value = serde_json::from_slice(&fs::read(&a[3])?)?;
    let mut wanted = BTreeSet::new();
    let mut out = BufWriter::new(File::options().write(true).create_new(true).open(&a[4])?);
    for q in queries["queries"].as_array().ok_or("queries array")? {
        let mut features = Vec::new();
        let mut seen = BTreeSet::new();
        let texts = q
            .get("target_text_variants")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| vec![q["target"].clone()]);
        for text in &texts {
            for w in windows(text) {
                for f in w.features {
                    if seen.insert(f.clone()) {
                        features.push(f);
                    }
                }
            }
        }
        if q["flags"]["anchor"].as_bool() == Some(true) {
            features = vec![q["target"].as_str().ok_or("anchor target")?.into()];
        }
        if features.len() > 16 {
            let last = features.len() - 1;
            features = (0..16).map(|i| features[i * last / 15].clone()).collect();
        }
        wanted.extend(features.iter().cloned());
        emit(
            &mut out,
            json!({"kind":"query","id":q["id"],"anchors":features}),
        );
    }
    let mut endpoints = BTreeSet::new();
    let mut events = 0u64;
    let mut edits = 0u64;
    let mut spans = 0u64;
    // Two bounded sequential passes: edge endpoints first, then only evidence
    // needed by fixed query roots or lineage. No full posting export/SQL index.
    for pass in 0..2 {
        if pass == 1 {
            wanted.extend(
                endpoints
                    .iter()
                    .filter(|a: &&String| !a.contains(','))
                    .cloned(),
            );
        }
        for (ordinal, id) in ids.lines().enumerate() {
            if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("invalid ID".into());
            }
            let compressed = fs::read(Path::new(&a[1]).join(format!("{id}.jsonl.zst")))?;
            let raw = zstd::stream::decode_all(compressed.as_slice())?;
            if hash(&raw) != id {
                return Err(format!("normalized hash mismatch: {id}").into());
            }
            let content = std::str::from_utf8(&raw)?;
            for (offset, line) in content.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let e: Value = serde_json::from_str(line)?;
                if pass == 0 {
                    events += 1;
                }
                let k = e["k"].as_str().unwrap_or("");
                if pass == 0 && k == "span.link" {
                    spans += 1;
                    let from = format!(
                        "span:{}:{}-{}",
                        e["from_file"].as_str().ok_or("link file")?,
                        e["from_range"][0],
                        e["from_range"][1]
                    );
                    let to = format!(
                        "span:{}:{}-{}",
                        e["to_file"].as_str().ok_or("link file")?,
                        e["to_range"][0],
                        e["to_range"][1]
                    );
                    emit(
                        &mut out,
                        json!({"kind":"edge","tape":id,"offset":offset,"pair":0,"from":from,"to":to,"confidence":1.0,"agent_link":true,"note":e["note"]}),
                    );
                }
                if !e["file"].is_string() {
                    continue;
                }
                if k == "code.edit" && pass == 0 {
                    let before = windows(&e["before_text"]);
                    let after = windows(&e["after_text"]);
                    if !before.is_empty() && !after.is_empty() {
                        let sim = match (e["before_text"].as_str(), e["after_text"].as_str()) {
                            (Some(b), Some(a)) => fingerprint_similarity(
                                &fingerprint_text(b).fingerprint,
                                &fingerprint_text(a).fingerprint,
                            ),
                            _ => None,
                        }
                        .or_else(|| e["similarity"].as_f64().map(|v| v as f32))
                        .unwrap_or(0.0);
                        if !sim.is_finite() {
                            return Err("nonfinite confidence".into());
                        }
                        let n = before.len().max(after.len());
                        for i in 0..n {
                            let b = &before[i * before.len() / n];
                            let a = &after[i * after.len() / n];
                            endpoints.insert(b.anchor.clone());
                            endpoints.insert(a.anchor.clone());
                            edits += 1;
                            emit(
                                &mut out,
                                json!({"kind":"edge","tape":id,"offset":offset,"pair":i,"from":b.anchor,"to":a.anchor,"confidence":f64::from(sim)}),
                            );
                        }
                    } else if !before.is_empty() && after.is_empty() {
                        for w in before {
                            emit(
                                &mut out,
                                json!({"kind":"tombstone","tape":id,"offset":offset,"file":e["file"],"timestamp":e["t"],"range":e.get("before_range").or(e.get("after_range")).cloned().unwrap_or(json!([0,0])),"anchor":w.anchor,"ordinal":w.ordinal}),
                            );
                        }
                    }
                }
                if pass == 1 {
                    let text = match k {
                        "code.read" if e["range"].is_array() => &e["text"],
                        "code.edit" => &e["after_text"],
                        _ => continue,
                    };
                    for w in windows(text) {
                        if wanted.contains(&w.anchor)
                            || endpoints.contains(&w.anchor)
                            || w.features.iter().any(|f| wanted.contains(f))
                        {
                            emit(
                                &mut out,
                                json!({"kind":"window","tape":id,"offset":offset,"file":e["file"],"timestamp":e["t"],"touch_kind":if k=="code.read"{"read"}else{"edit"},"anchor":w.anchor,"ordinal":w.ordinal}),
                            );
                        }
                    }
                }
            }
            if ordinal % 5000 == 0 {
                eprintln!("pass {}: {ordinal} tapes", pass + 1);
            }
        }
    }
    emit(
        &mut out,
        json!({"kind":"census","events":events,"edit_edges":edits,"span_links":spans,"tapes":ids.lines().count(),"ids_sha256":hash(ids.as_bytes()),"queries_sha256":hash(&fs::read(&a[3])?)}),
    );
    out.flush()?;
    Ok(())
}
