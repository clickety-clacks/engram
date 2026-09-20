//! Exact §9.3 product observations against a separately frozen expectation package.
use super::t1772::{ProofResult, sha256_file, write_canonical_json};
use crate::{
    dispatch::extract_dispatch_links_from_transcript, index::SqliteIndex,
    tape::event::parse_jsonl_events,
};
use serde_json::{Value, json};
use sha2::Digest;
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const ROOT: &str = "/Users/mike/shared-workspace/engram/proofs/t1772/journey-inputs-r29";
pub const CONTRACT: &str = "29fd50d40d56860b6c3619b53ba342c7afc6b06d0b0b3b005420d4efbe62db83";
fn string<'a>(v: &'a Value, k: &str) -> ProofResult<&'a str> {
    v[k].as_str()
        .ok_or_else(|| format!("missing string {k}").into())
}
fn list<'a>(v: &'a Value, k: &str) -> ProofResult<&'a Vec<Value>> {
    v[k].as_array()
        .ok_or_else(|| format!("missing list {k}").into())
}
fn input(root: &Path, relative: &str) -> ProofResult<PathBuf> {
    let p = Path::new(relative);
    if p.is_absolute()
        || p.components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err("unsafe journey input path".into());
    }
    let path = root.join(p);
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(root.canonicalize()?)
        || fs::symlink_metadata(&path)?.file_type().is_symlink()
    {
        return Err("journey input escapes custody".into());
    }
    Ok(path)
}
pub fn verify_inputs(root: &Path, hash: &str) -> ProofResult<Value> {
    let manifest = root.join("manifest.json");
    if sha256_file(&manifest)? != hash {
        return Err("journey manifest hash mismatch".into());
    }
    let m: Value = serde_json::from_slice(&fs::read(manifest)?)?;
    if m["schema"] != "t1772-section93-package-v1"
        || m["contract_sha256"] != CONTRACT
        || m["complete"] != true
    {
        return Err("incomplete/unbound section 9.3 package".into());
    }
    let expected_files = list(&m, "files")?
        .iter()
        .map(|r| r["path"].as_str().unwrap_or("").to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let mut actual_files = std::collections::BTreeSet::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let e = entry?;
        if e.file_type().is_symlink() {
            return Err("symlink in journey custody".into());
        }
        if e.file_type().is_file() {
            let rel = e.path().strip_prefix(root)?.to_str().ok_or("input utf8")?;
            if rel != "manifest.json" {
                actual_files.insert(rel.to_string());
            }
        }
    }
    if actual_files != expected_files {
        return Err("journey file ledger is not exhaustive".into());
    }
    for row in list(&m, "files")? {
        if sha256_file(&input(root, string(row, "path")?)?)? != string(row, "sha256")? {
            return Err("journey file hash mismatch".into());
        }
    }
    let groups = list(&m, "groups")?;
    for required in [
        "P0",
        "unaligned",
        "unaligned-read",
        "forensics-span",
        "tombstone",
        "ordering",
        "additional",
        "dispatch",
        "reingest",
        "normalized-claude",
        "normalized-codex",
        "normalized-cursor",
        "normalized-gemini",
        "normalized-openclaw",
        "normalized-opencode",
        "normalized-gemini-logs",
        "normalized-root-session",
        "normalized-implementer-session",
    ] {
        if !groups
            .iter()
            .any(|g| g["id"] == required && g["cases"].as_array().is_some_and(|c| !c.is_empty()))
        {
            return Err(format!("missing required journey group {required}").into());
        }
    }
    for (group, required) in [
        ("unaligned", vec!["fixed-unaligned-edit-full-explain"]),
        ("unaligned-read", vec!["fixed-unaligned-read-full-explain"]),
        (
            "forensics-span",
            vec![
                "fixed-ordinary-full-explain",
                "fixed-forensics-full-explain",
                "fixed-span-link-full-explain",
            ],
        ),
        (
            "tombstone",
            vec![
                "fixed-deleted-hidden-full-explain",
                "fixed-deleted-visible-full-explain",
            ],
        ),
        ("ordering", vec!["fixed-session-order-full-explain"]),
        ("additional", vec!["fixed-additional-dedupe-full-explain"]),
        ("dispatch", vec!["fixed-dispatch-chain-full-explain"]),
        ("reingest", vec!["fixed-restart-reingest-full-explain"]),
    ] {
        let g = groups.iter().find(|g| g["id"] == group).unwrap();
        for id in required {
            if !list(g, "cases")?.iter().any(|c| c["id"] == id) {
                return Err(format!("missing required fixed observation {id}").into());
            }
        }
        if group == "reingest"
            && (g["reingest"].is_null()
                || g["restart"].is_null()
                || !list(g, "before_cases")?
                    .iter()
                    .any(|c| c["id"] == "fixed-restart-before-full-explain"))
        {
            return Err("missing required reingest writer step".into());
        }
    }
    let p0 = groups.iter().find(|g| g["id"] == "P0").unwrap();
    for n in 1..=12 {
        let id = format!("p0-q{n:02}-full-explain");
        if !list(p0, "cases")?.iter().any(|c| c["id"] == id) {
            return Err(format!("missing complete P0 observation {id}").into());
        }
        for w in 0..10 {
            let window = format!("p0-q{n:02}-window-{w:02}");
            if !list(p0, "cases")?.iter().any(|c| c["id"] == window) {
                return Err(format!("missing P0 raw window {window}").into());
            }
        }
    }
    Ok(m)
}
fn protect(dbs: &[PathBuf], readonly: bool) -> ProofResult<()> {
    for db in dbs {
        fs::set_permissions(
            db,
            fs::Permissions::from_mode(if readonly { 0o444 } else { 0o644 }),
        )?;
        fs::set_permissions(
            db.parent().ok_or("db parent")?,
            fs::Permissions::from_mode(if readonly { 0o555 } else { 0o755 }),
        )?;
    }
    Ok(())
}
fn custody(dbs: &[PathBuf]) -> ProofResult<Value> {
    let mut rows = Vec::new();
    for db in dbs {
        let mut listing = fs::read_dir(db.parent().ok_or("db parent")?)?
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        listing.sort();
        let mut files = Vec::new();
        for suffix in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{suffix}", db.display()));
            files.push(if p.exists() {
                json!({"path":p,"sha256":sha256_file(&p)?,"bytes":fs::metadata(&p)?.len()})
            } else {
                json!({"path":p,"absent":true})
            });
        }
        rows.push(json!({"db":db,"db_mode":fs::metadata(db)?.permissions().mode()&0o777,"directory_mode":fs::metadata(db.parent().unwrap())?.permissions().mode()&0o777,"files":files,"listing":listing}));
    }
    Ok(json!(rows))
}
fn setup_db(db: &Path, tapes: &Path, ids: &[String]) -> ProofResult<()> {
    let index = SqliteIndex::open_writer(db.to_str().ok_or("db utf8")?)?;
    for id in ids {
        let compressed = fs::read(tapes.join(format!("{id}.jsonl.zst")))?;
        let bytes = zstd::stream::decode_all(compressed.as_slice())?;
        if format!("{:x}", sha2::Sha256::digest(&bytes)) != *id {
            return Err("normalized fixture hash mismatch".into());
        }
        let content = String::from_utf8(bytes)?;
        index.ingest_tape_events_with_dispatch(
            id,
            &parse_jsonl_events(&content)?,
            &extract_dispatch_links_from_transcript(&content),
            crate::index::lineage::LINK_THRESHOLD_DEFAULT,
        )?;
    }
    Ok(())
}
// JSON pointers retain field presence, array order and multiplicity in a failed case.
fn json_differences(path: &str, expected: &Value, actual: &Value, out: &mut Vec<Value>) {
    if expected == actual {
        return;
    }
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            for key in e
                .keys()
                .chain(a.keys())
                .collect::<std::collections::BTreeSet<_>>()
            {
                let pointer = format!("{}/{}", path, key.replace('~', "~0").replace('/', "~1"));
                match (e.get(key), a.get(key)) {
                    (Some(e), Some(a)) => json_differences(&pointer, e, a, out),
                    (e, a) => out.push(json!({"pointer":pointer,"expected_present":e.is_some(),"actual_present":a.is_some(),"expected":e,"actual":a})),
                }
            }
        }
        (Value::Array(e), Value::Array(a)) => {
            for i in 0..e.len().max(a.len()) {
                let pointer = format!("{path}/{i}");
                match (e.get(i), a.get(i)) {
                    (Some(e), Some(a)) => json_differences(&pointer, e, a, out),
                    (e, a) => out.push(json!({"pointer":pointer,"expected_present":e.is_some(),"actual_present":a.is_some(),"expected":e,"actual":a})),
                }
            }
        }
        _ => out.push(json!({"pointer":path,"expected":expected,"actual":actual})),
    }
}
fn observation(
    binary: &Path,
    repo: &Path,
    home: &Path,
    dbs: &[PathBuf],
    case: &Value,
    expected: Value,
    out: &Path,
    stdin: Option<&[u8]>,
) -> ProofResult<()> {
    fs::create_dir_all(out)?;
    let argv = list(case, "argv")?
        .iter()
        .map(|v| v.as_str().ok_or("argv must be strings"))
        .collect::<Result<Vec<_>, _>>()?;
    if (stdin.is_none() && !matches!(argv.first(), Some(&"explain" | &"peek")))
        || (stdin.is_some() && argv != ["record", "--stdin"])
    {
        return Err("unsupported journey command or writer outside reingest step".into());
    }
    let before = custody(dbs)?;
    write_canonical_json(&out.join("custody-before.json"), &before)?;
    let mut command = Command::new(binary);
    command
        .args(&argv)
        .current_dir(repo)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .env("TMPDIR", out)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let execution = (|| -> ProofResult<(std::process::Output, Option<String>)> {
        let mut child = command.spawn()?;
        let write_error = if let Some(bytes) = stdin {
            match child.stdin.take() {
                Some(mut pipe) => pipe.write_all(bytes).err().map(|e| e.to_string()),
                None => Some("stdin pipe absent".into()),
            }
        } else {
            drop(child.stdin.take());
            None
        };
        Ok((child.wait_with_output()?, write_error))
    })();
    // Retain after-custody even for failed launch, exit, parsing or equality.
    let after = custody(dbs);
    write_canonical_json(
        &out.join("custody-after.json"),
        &match &after {
            Ok(v) => v.clone(),
            Err(e) => json!({"error":e.to_string()}),
        },
    )?;
    let (output, write_error) = execution?;
    fs::write(out.join("stdout"), &output.stdout)?;
    fs::write(out.join("stderr"), &output.stderr)?;
    let diagnostics = [
        format!("config: {}", home.join(".engram/config.yml").display()),
        format!("db: {}", dbs[0].display()),
    ];
    let mut stderr_values = Vec::new();
    let mut stderr_errors = Vec::new();
    match std::str::from_utf8(&output.stderr) {
        Ok(text) => {
            for line in text.lines() {
                if diagnostics.iter().any(|d| d == line) {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(v) => stderr_values.push(v),
                    Err(_) => stderr_errors.push(line.to_owned()),
                }
            }
        }
        Err(e) => stderr_errors.push(e.to_string()),
    }
    let parsed: Result<Value, String> = if !stderr_errors.is_empty() {
        Err(format!("unrecognized stderr lines: {stderr_errors:?}"))
    } else if case["channel"] == "stderr_json" {
        if !output.stdout.is_empty() || stderr_values.len() != 1 {
            Err("expected empty stdout and exactly one stderr JSON value".into())
        } else {
            Ok(stderr_values[0].clone())
        }
    } else if !stderr_values.is_empty() {
        Err("unexpected stderr JSON on success".into())
    } else {
        serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())
    };
    let mut differences = Vec::new();
    if let Ok(actual) = &parsed {
        json_differences("", &expected, actual, &mut differences);
    }
    let equal = write_error.is_none() && parsed.as_ref().is_ok_and(|v| v == &expected);
    let unchanged = after.as_ref().is_ok_and(|v| v == &before);
    write_canonical_json(
        &out.join("comparison.json"),
        &json!({"argv":argv,"cwd":repo,"environment_cleared":true,"environment":{"HOME":home,"PATH":"/usr/bin:/bin","LANG":"C","LC_ALL":"C","TZ":"UTC","TMPDIR":out},"config":home.join(".engram/config.yml"),"config_sha256":sha256_file(&home.join(".engram/config.yml")).ok(),"differences":differences,"expected":expected,"actual":parsed.as_ref().ok(),"parse_error":parsed.as_ref().err(),"stdin_error":write_error,"exit":output.status.code(),"expected_exit":case["exit"],"equal":equal,"unchanged":unchanged,"mismatch_rule":"complete JSON equality; object key order is immaterial, arrays and every field preserved; raw streams retained"}),
    )?;
    if !equal
        || output.status.code().map(i64::from) != case["exit"].as_i64()
        || (stdin.is_none() && !unchanged)
    {
        return Err(format!("journey observation failed: {}", case["id"]).into());
    }
    after?;
    Ok(())
}
pub fn run(
    root: &Path,
    manifest_hash: &str,
    binary: &Path,
    p0_db: &Path,
    p0_tapes: &Path,
    out: &Path,
) -> ProofResult<Value> {
    let m = verify_inputs(root, manifest_hash)?;
    execute_manifest(root, manifest_hash, binary, p0_db, p0_tapes, out, &m)
}
fn execute_manifest(
    root: &Path,
    manifest_hash: &str,
    binary: &Path,
    p0_db: &Path,
    p0_tapes: &Path,
    out: &Path,
    m: &Value,
) -> ProofResult<Value> {
    fs::create_dir_all(out)?;
    let mut passed = 0;
    for group in list(&m, "groups")? {
        let id = string(group, "id")?;
        if id.contains('/') || id.contains("..") {
            return Err("unsafe group ID".into());
        }
        let dir = out.join(id);
        fs::create_dir(&dir)?;
        let repo = dir.join("repo");
        fs::create_dir(&repo)?;
        let store = repo.join(".engram");
        fs::create_dir(&store)?;
        let home = dir.join("home");
        fs::create_dir_all(home.join(".engram"))?;
        let (db, tapes) = if id == "P0" {
            (p0_db.to_owned(), p0_tapes.to_owned())
        } else {
            let tapes = store.join("tapes");
            fs::create_dir(&tapes)?;
            let mut ids = Vec::new();
            for t in list(group, "tapes")? {
                let tape = string(t, "id")?;
                if tape.len() != 64 || !tape.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("invalid fixture tape ID".into());
                }
                if t["store"] != "additional" && t["deferred"] != true {
                    let src = input(root, string(t, "compressed")?)?;
                    fs::copy(&src, tapes.join(format!("{tape}.jsonl.zst")))?;
                    ids.push(tape.to_string());
                }
            }
            ids.sort();
            let db = store.join("index.sqlite");
            setup_db(&db, &tapes, &ids)?;
            (db, tapes)
        };
        let mut dbs = vec![db.clone()];
        let mut config = string(m, "config_template")?
            .replace("${DB}", &db.to_string_lossy())
            .replace("${TAPES}", &tapes.to_string_lossy());
        if group["additional_store"] == true {
            let extra = dir.join("additional");
            fs::create_dir(&extra)?;
            let extra_db = extra.join("index.sqlite");
            let extra_tapes = extra.join("tapes");
            fs::create_dir(&extra_tapes)?;
            let mut ids = Vec::new();
            for t in list(group, "tapes")? {
                if t["store"] == "additional" || t["store"] == "both" {
                    let tape = string(t, "id")?;
                    fs::copy(
                        input(root, string(t, "compressed")?)?,
                        extra_tapes.join(format!("{tape}.jsonl.zst")),
                    )?;
                    ids.push(tape.to_string());
                }
            }
            ids.sort();
            setup_db(&extra_db, &extra_tapes, &ids)?;
            config += &string(m, "additional_config_template")?
                .replace("${ADDITIONAL_DB}", &extra_db.to_string_lossy());
            dbs.push(extra_db);
        }
        fs::write(home.join(".engram/config.yml"), &config)?;
        fs::write(store.join("config.yml"), &config)?;
        for f in list(group, "target_files")? {
            let rel = Path::new(string(f, "target")?);
            if rel.is_absolute()
                || rel
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err("unsafe target path".into());
            }
            let dst = repo.join(rel);
            if dst.is_absolute() && !dst.starts_with(&repo) {
                return Err("target escapes repository".into());
            }
            fs::create_dir_all(dst.parent().ok_or("target parent")?)?;
            fs::copy(input(root, string(f, "source")?)?, dst)?;
        }
        if id != "P0" {
            for tape_dir in [tapes.clone(), dir.join("additional/tapes")] {
                if tape_dir.exists() {
                    for e in fs::read_dir(&tape_dir)? {
                        fs::set_permissions(e?.path(), fs::Permissions::from_mode(0o444))?;
                    }
                    fs::set_permissions(&tape_dir, fs::Permissions::from_mode(0o555))?;
                }
            }
        }
        protect(&dbs, true)?;
        let repeats = if group.get("reingest").is_some_and(|v| !v.is_null()) {
            3
        } else {
            1
        };
        for phase in 0..repeats {
            if phase > 0 {
                protect(&dbs, false)?;
                let step = if phase == 1 { "reingest" } else { "restart" };
                let replay = &group[step];
                fs::set_permissions(&tapes, fs::Permissions::from_mode(0o755))?;
                let bytes = fs::read(input(root, string(replay, "raw")?)?)?;
                let mut expected: Value =
                    serde_json::from_slice(&fs::read(input(root, string(replay, "expected")?)?)?)?;
                expected["path"] =
                    json!(tapes.join(format!("{}.jsonl.zst", string(replay, "tape_id")?)));
                let result = observation(
                    binary,
                    &repo,
                    &home,
                    &dbs,
                    &json!({"id":step,"argv":["record","--stdin"],"exit":0}),
                    expected,
                    &dir.join(step),
                    Some(&bytes),
                );
                let protected = protect(&dbs, true);
                for e in fs::read_dir(&tapes)? {
                    fs::set_permissions(e?.path(), fs::Permissions::from_mode(0o444))?;
                }
                fs::set_permissions(&tapes, fs::Permissions::from_mode(0o555))?;
                result?;
                protected?;
                let raw = zstd::stream::decode_all(
                    fs::read(tapes.join(format!("{}.jsonl.zst", string(replay, "tape_id")?)))?
                        .as_slice(),
                )?;
                if raw != bytes {
                    return Err("recorded restart/reingest bytes differ from frozen input".into());
                }
            }
            for case in list(
                group,
                if repeats == 3 && phase < 2 {
                    "before_cases"
                } else {
                    "cases"
                },
            )? {
                let case_id = string(case, "id")?;
                if case_id.contains('/') || case_id.contains("..") {
                    return Err("unsafe case ID".into());
                }
                let expected =
                    serde_json::from_slice(&fs::read(input(root, string(case, "expected")?)?)?)?;
                observation(
                    binary,
                    &repo,
                    &home,
                    &dbs,
                    case,
                    expected,
                    &dir.join(format!("phase-{phase}-{case_id}")),
                    None,
                )?;
                passed += 1;
            }
        }
    }
    let result = json!({"status":"passed","manifest_sha256":manifest_hash,"contract_sha256":CONTRACT,"observations_passed":passed,"complete_case_groups":list(&m,"groups")?.len()});
    write_canonical_json(&out.join("result.json"), &result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires frozen journey package and exact isolated product binary"]
    fn frozen_fixture_package_executes_without_p0() {
        let root = PathBuf::from(std::env::var("T1772_JOURNEY_FIXTURE_PACKAGE").unwrap());
        let binary = PathBuf::from(std::env::var("T1772_JOURNEY_FIXTURE_BINARY").unwrap());
        let hash = sha256_file(&root.join("manifest.json")).unwrap();
        let mut m = verify_inputs(&root, &hash).unwrap();
        m["groups"]
            .as_array_mut()
            .unwrap()
            .retain(|g| g["id"] != "P0");
        assert!(
            m["groups"]
                .as_array()
                .unwrap()
                .iter()
                .all(|g| g["id"] != "P0")
        );
        let out = PathBuf::from(std::env::var("T1772_JOURNEY_FIXTURE_OUTPUT").unwrap());
        execute_manifest(
            &root,
            &hash,
            &binary,
            Path::new("UNUSED_NO_P0_DATABASE"),
            Path::new("UNUSED_NO_P0_TAPES"),
            &out,
            &m,
        )
        .unwrap();
    }
    #[test]
    fn complete_output_and_custody_failures_are_retained() {
        for (name, body, exit, should_pass) in [
            ("ok", "printf '{\"value\":1}\\n'", 0, true),
            ("wrong", "printf '{\"value\":2}\\n'", 0, false),
            ("exit", "printf '{\"value\":1}\\n'; exit 7", 0, false),
            ("parse", "printf 'not-json\\n'", 0, false),
            (
                "stderr",
                "printf '{\"value\":1}\\n'; printf 'hidden warning\\n' >&2",
                0,
                false,
            ),
            (
                "mutation",
                "printf '{\"value\":1}\\n'; printf changed > index.sqlite",
                0,
                false,
            ),
        ] {
            let t = tempfile::tempdir().unwrap();
            let repo = t.path().join("repo");
            fs::create_dir(&repo).unwrap();
            let home = t.path().join("home");
            fs::create_dir(&home).unwrap();
            let db = repo.join("index.sqlite");
            fs::write(&db, b"original").unwrap();
            let binary = t.path().join("fixture-command");
            fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
            let out = t.path().join("evidence");
            let result = observation(
                &binary,
                &repo,
                &home,
                &[db],
                &json!({"id":name,"argv":["explain","fixture"],"exit":exit}),
                json!({"value":1}),
                &out,
                None,
            );
            assert_eq!(result.is_ok(), should_pass, "{name}: {result:?}");
            assert!(out.join("custody-after.json").exists());
            assert!(out.join("comparison.json").exists());
            assert!(out.join("stdout").exists());
        }
    }
    #[test]
    fn exact_diffs_preserve_array_order_and_missing_null() {
        let mut diff = Vec::new();
        json_differences(
            "",
            &json!({"rows":[1,2],"a/b":null}),
            &json!({"rows":[2,1]}),
            &mut diff,
        );
        assert_eq!(
            diff.iter()
                .map(|v| v["pointer"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["/a~1b", "/rows/0", "/rows/1"]
        );
        assert_eq!(diff[0]["actual_present"], false);
    }
    #[test]
    fn unsafe_relative_inputs_are_rejected() {
        let t = tempfile::tempdir().unwrap();
        assert!(input(t.path(), "../outside").is_err());
        assert!(input(t.path(), "/absolute").is_err());
    }
    #[test]
    fn supplied_document_cannot_replace_complete_cases() {
        let t = tempfile::tempdir().unwrap();
        let m = json!({"schema":"t1772-section93-package-v1","contract_sha256":CONTRACT,"complete":true,"files":[],"groups":[]});
        write_canonical_json(&t.path().join("manifest.json"), &m).unwrap();
        let hash = sha256_file(&t.path().join("manifest.json")).unwrap();
        assert!(
            verify_inputs(t.path(), &hash)
                .unwrap_err()
                .to_string()
                .contains("missing required journey group")
        );
    }
}
